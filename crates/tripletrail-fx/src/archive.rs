//! Archives and folders, as SPARQL Anything sees them: a list of names. A zip or tar archive
//! (also compressed: `.tar.gz`, `.tgz`, `.tar.bz2`, `.tbz2`) gives the root one slot per
//! entry, directories included, in archive order; a folder gives one per file and directory
//! below it (itself first) as `file:` URLs. `archive.matches` keeps the names a regular
//! expression matches in full (Java's `String.matches`; look-around is not supported).
//!
//! `from-archive=<archive>` reads `location` from inside that archive instead of from the
//! file system, so the names a listing gives can be triplified in a nested SERVICE.

use crate::FxError;
use crate::options::Options;
use crate::tree::Sink;
use std::io::Read;
use std::path::Path;

/// The kinds of archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Zip,
    Tar,
    TarGz,
    TarBz2,
}

impl Kind {
    /// The kind a file name or `archive-format` names: `zip`, `tar`, `tar.gz`, `tgz`,
    /// `tar.bz2`, `tbz2`.
    pub(crate) fn from_name(name: &str) -> Result<Option<Self>, FxError> {
        let name = name.to_ascii_lowercase();
        Ok(Some(if name.ends_with("zip") {
            Kind::Zip
        } else if name.ends_with("tar.gz") || name.ends_with("tgz") {
            Kind::TarGz
        } else if name.ends_with("tar") {
            Kind::Tar
        } else if name.ends_with("tar.bz2") || name.ends_with("tbz2") {
            Kind::TarBz2
        } else if name.ends_with("bz2") {
            // SPARQL Anything reads a bzip2 file only as a tar archive, as for gzip.
            return Err(FxError::Unsupported(
                "a bzip2-compressed file that is not a tar archive (.tar.bz2)".into(),
            ));
        } else {
            return Ok(None);
        }))
    }
}

/// The most bytes `from-archive` decompresses for one entry, so that a small archive that
/// expands to far more (a "zip bomb") is refused instead of filling memory.
const MAX_ENTRY_BYTES: u64 = 4 << 30;

/// The most names a listing of an archive or folder gives.
const MAX_NAMES: usize = 1_000_000;

/// The most bytes a `.tar.gz` or `.tar.bz2` listing decompresses: a listing reads through every entry.
const MAX_LISTED_BYTES: u64 = 16 << 30;

/// A reader that fails once it has given `left` bytes.
struct Limited<R> {
    inner: R,
    left: u64,
}

impl<R: Read> Read for Limited<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.left = self.left.checked_sub(n as u64).ok_or_else(|| {
            std::io::Error::other(format!(
                "the archive decompresses to more than {MAX_LISTED_BYTES} bytes"
            ))
        })?;
        Ok(n)
    }
}

fn too_many(what: &str) -> FxError {
    FxError::Parse(format!("{what} holds more than {MAX_NAMES} entries"))
}

/// Reads all of `from` into `out`, refusing more than [`MAX_ENTRY_BYTES`].
fn read_capped(from: impl Read, out: &mut Vec<u8>, kind: Kind, name: &str) -> Result<(), FxError> {
    read_at_most(from, out, MAX_ENTRY_BYTES, kind, name)
}

fn read_at_most(
    from: impl Read,
    out: &mut Vec<u8>,
    limit: u64,
    kind: Kind,
    name: &str,
) -> Result<(), FxError> {
    let read = from
        .take(limit + 1)
        .read_to_end(out)
        .map_err(|e| error(kind, e))?;
    if read as u64 > limit {
        return Err(error(
            kind,
            format!("{name} decompresses to more than {limit} bytes"),
        ));
    }
    Ok(())
}

fn error(kind: Kind, e: impl std::fmt::Display) -> FxError {
    FxError::Parse(format!("{kind:?} archive: {e}"))
}

/// The names of the entries of an archive, in archive order.
fn names(bytes: &[u8], kind: Kind) -> Result<Vec<String>, FxError> {
    match kind {
        Kind::Zip => {
            let mut archive =
                zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| error(kind, e))?;
            if archive.len() > MAX_NAMES {
                return Err(too_many("the zip archive"));
            }
            (0..archive.len())
                .map(|i| {
                    archive
                        .by_index_raw(i)
                        .and_then(|f| f.name().map(|n| n.into_owned()))
                        .map_err(|e| error(kind, e))
                })
                .collect()
        }
        Kind::Tar => tar_names(bytes, kind),
        Kind::TarGz | Kind::TarBz2 => tar_names(
            Limited {
                inner: decompressed(bytes, kind),
                left: MAX_LISTED_BYTES,
            },
            kind,
        ),
    }
}

/// The tar stream of an archive: its bytes, or them decompressed.
fn decompressed(bytes: &[u8], kind: Kind) -> Box<dyn Read + '_> {
    match kind {
        Kind::TarGz => Box::new(flate2::read::GzDecoder::new(bytes)),
        Kind::TarBz2 => Box::new(crabz2::reader(bytes)),
        Kind::Zip | Kind::Tar => Box::new(bytes),
    }
}

fn tar_names(tar: impl Read, kind: Kind) -> Result<Vec<String>, FxError> {
    let mut archive = tar::Archive::new(tar);
    let mut out = Vec::new();
    for entry in archive.entries().map_err(|e| error(kind, e))? {
        let entry = entry.map_err(|e| error(kind, e))?;
        if out.len() == MAX_NAMES {
            return Err(too_many("the tar archive"));
        }
        let path = entry.path_bytes();
        out.push(String::from_utf8_lossy(&path).into_owned());
    }
    Ok(out)
}

/// The bytes of the entry `name` of an archive.
pub(crate) fn entry(bytes: &[u8], kind: Kind, name: &str) -> Result<Vec<u8>, FxError> {
    let missing = || FxError::Option(format!("the archive holds no file {name:?}"));
    let mut out = Vec::new();
    match kind {
        Kind::Zip => {
            let mut archive =
                zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| error(kind, e))?;
            let file = archive.by_name(name).map_err(|_| missing())?;
            read_capped(file, &mut out, kind, name)?;
        }
        Kind::Tar | Kind::TarGz | Kind::TarBz2 => {
            let mut archive = tar::Archive::new(decompressed(bytes, kind));
            let mut found = false;
            for e in archive.entries().map_err(|e| error(kind, e))? {
                let e = e.map_err(|e| error(kind, e))?;
                if *e.path_bytes() == *name.as_bytes() {
                    read_capped(e, &mut out, kind, name)?;
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(missing());
            }
        }
    }
    Ok(out)
}

/// `archive.matches` as a regular expression matching whole names.
fn matcher(options: &Options) -> Result<Option<regex::Regex>, FxError> {
    let Some(pattern) = options.get("archive.matches") else {
        return Ok(None);
    };
    regex::Regex::new(&format!("^(?:{pattern})$"))
        .map(Some)
        .map_err(|e| {
            FxError::Option(format!(
                "archive.matches {pattern:?} is not a supported regular expression \
                 (look-around and back-references are not): {e}"
            ))
        })
}

/// The root's slots: the names of the archive's entries.
pub(crate) fn triplify(
    bytes: &[u8],
    kind: Kind,
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let matches = matcher(options)?;
    fill(sink, names(bytes, kind)?, matches.as_ref());
    Ok(())
}

/// The root's slots: the `file:` URLs of `dir` and everything below it.
pub(crate) fn triplify_folder(
    dir: &Path,
    options: &Options,
    sink: &mut Sink<'_>,
) -> Result<(), FxError> {
    let matches = matcher(options)?;
    let mut paths = Vec::new();
    walk(dir, &mut paths)?;
    // The folder is resolved once; the entries below it are not, so that a symbolic link is
    // listed as itself and never as the file it points to, which may be outside the
    // directories sources may be read from.
    let real = dir.canonicalize().map_err(|source| FxError::Read {
        path: dir.display().to_string(),
        source,
    })?;
    let names = paths
        .iter()
        .map(|p| {
            let relative = p.strip_prefix(dir).unwrap_or(p);
            let is_dir = std::fs::symlink_metadata(p).is_ok_and(|m| m.is_dir());
            file_url(&real.join(relative), is_dir)
        })
        .collect();
    fill(sink, names, matches.as_ref());
    Ok(())
}

fn fill(sink: &mut Sink<'_>, names: Vec<String>, matches: Option<&regex::Regex>) {
    let root = sink.root();
    let mut n = 0;
    for name in names {
        if matches.is_some_and(|m| !m.is_match(&name)) {
            continue;
        }
        if let Some(o) = sink.string(&name) {
            n += 1;
            sink.slot(&root, n, o);
        }
    }
}

/// `dir` and the files and directories below it, each directory's entries sorted by name.
fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<(), FxError> {
    if out.len() >= MAX_NAMES {
        return Err(too_many("the folder"));
    }
    out.push(dir.to_path_buf());
    let read = |e: std::io::Error| FxError::Read {
        path: dir.display().to_string(),
        source: e,
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(read)?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(read)?;
    entries.sort();
    for p in entries {
        // Symbolic links are listed, not followed.
        if p.is_dir() && !p.is_symlink() {
            walk(&p, out)?;
        } else {
            if out.len() >= MAX_NAMES {
                return Err(too_many("the folder"));
            }
            out.push(p);
        }
    }
    Ok(())
}

/// A path as a `file:` URL (a directory's ends with `/`, as Java's `Path.toUri`).
fn file_url(path: &Path, dir: bool) -> String {
    let mut url = String::from("file://");
    let text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        url.push('/');
    }
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~:".contains(&b) {
            url.push(b as char);
        } else {
            url.push_str(&format!("%{b:02X}"));
        }
    }
    if dir && !url.ends_with('/') {
        url.push('/');
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn entries_that_decompress_past_the_limit_are_refused() {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        gz.write_all(&vec![0; 1 << 20]).unwrap();
        let small = gz.finish().unwrap();
        let mut out = Vec::new();
        let e = read_at_most(
            flate2::read::GzDecoder::new(&small[..]),
            &mut out,
            1 << 16,
            Kind::TarGz,
            "zeros",
        )
        .unwrap_err();
        assert!(e.to_string().contains("more than 65536 bytes"), "{e}");
        let mut out = Vec::new();
        read_at_most(
            flate2::read::GzDecoder::new(&small[..]),
            &mut out,
            1 << 20,
            Kind::TarGz,
            "zeros",
        )
        .unwrap();
        assert_eq!(out.len(), 1 << 20);
    }

    #[test]
    fn listings_stop_past_the_limit() {
        let mut limited = Limited {
            inner: &[0u8; 100][..],
            left: 10,
        };
        let mut out = Vec::new();
        let e = limited.read_to_end(&mut out).unwrap_err();
        assert!(e.to_string().contains("decompresses to more than"), "{e}");
    }
}
