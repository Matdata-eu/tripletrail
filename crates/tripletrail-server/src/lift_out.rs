//! The triples one `tripletrail lift` query writes, each once, in bounded memory (#389).
//!
//! A query's lines are kept in memory up to [`Limits::spool_bytes`], then in a temporary file.
//! Duplicates are filtered by a set of 128-bit hashes of the lines, up to
//! [`Limits::seen`] of them. Past that the set stops growing: a later line whose hash is in it
//! is dropped as before, any other is written to a second part and its (hash, line number)
//! goes to sorted runs on disk. When the query is done the runs are merged, and the second
//! part is copied without the lines whose hash came earlier. The output keeps the first of
//! each triple, in the order the query made them, as with the set alone.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet};
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// How much one query's output may hold in memory.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    /// Bytes of lines held in memory before they go to a temporary file.
    pub spool_bytes: usize,
    /// Hashes in the set. 7/8 of 2^24 fills a table of 2^24 buckets of 17 bytes (about
    /// 290 MB); one more would double it.
    pub seen: usize,
    /// (hash, line number) pairs sorted in memory before they go to disk as a run.
    pub run: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            spool_bytes: 64 << 20,
            seen: (1 << 24) / 8 * 7,
            run: 1 << 22,
        }
    }
}

/// A temporary file, removed when dropped.
struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn create(dir: &Path) -> io::Result<(Self, File)> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        loop {
            let path = dir.join(format!(
                ".tripletrail-lift-{}-{}.tmp",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => return Ok((Self { path }, file)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bytes held in memory up to a limit, then in a temporary file.
struct Spool {
    memory: Vec<u8>,
    file: Option<(TempFile, BufWriter<File>)>,
    limit: usize,
    dir: PathBuf,
}

impl Spool {
    fn new(dir: &Path, limit: usize) -> Self {
        Self {
            memory: Vec::new(),
            file: None,
            limit,
            dir: dir.to_path_buf(),
        }
    }

    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if let Some((_, w)) = &mut self.file {
            return w.write_all(bytes);
        }
        self.memory.extend_from_slice(bytes);
        if self.memory.len() > self.limit {
            let (temp, file) = TempFile::create(&self.dir)?;
            let mut w = BufWriter::with_capacity(1 << 20, file);
            w.write_all(&self.memory)?;
            self.memory = Vec::new();
            self.file = Some((temp, w));
        }
        Ok(())
    }

    /// What was written, from the start, after the temporary file that holds it (if any):
    /// bound as `let (_temp, reader)`, the reader is dropped first and closes the file before
    /// it is removed, which Windows needs.
    fn reader(self) -> io::Result<(Option<TempFile>, Box<dyn BufRead>)> {
        match self.file {
            None => Ok((None, Box::new(io::Cursor::new(self.memory)))),
            Some((temp, w)) => {
                let mut file = w.into_inner().map_err(io::IntoInnerError::into_error)?;
                io::Seek::rewind(&mut file)?;
                Ok((
                    Some(temp),
                    Box::new(BufReader::with_capacity(1 << 20, file)),
                ))
            }
        }
    }
}

/// The lines of the second part: hashes not in the set, with their line numbers, sorted a run
/// at a time.
struct Late {
    lines: Spool,
    pairs: Vec<(u128, u64)>,
    runs: Vec<TempFile>,
    next: u64,
}

/// One query's output: N-Triples lines, each written once.
pub(crate) struct Output {
    limits: Limits,
    dir: PathBuf,
    seen: HashSet<u128>,
    early: Spool,
    late: Option<Late>,
    early_count: usize,
}

impl Output {
    /// Temporary files go to `dir`.
    pub(crate) fn new(dir: &Path, limits: Limits) -> Self {
        Self {
            limits,
            dir: dir.to_path_buf(),
            seen: HashSet::new(),
            early: Spool::new(dir, limits.spool_bytes),
            late: None,
            early_count: 0,
        }
    }

    /// Adds one line (a triple and its newline), unless it was added before.
    pub(crate) fn line(&mut self, line: &[u8]) -> io::Result<()> {
        let hash = hash(line);
        if self.seen.contains(&hash) {
            return Ok(());
        }
        if self.late.is_none() && self.seen.len() < self.limits.seen {
            self.seen.insert(hash);
            self.early_count += 1;
            return self.early.write(line);
        }
        let late = self.late.get_or_insert_with(|| Late {
            lines: Spool::new(&self.dir, self.limits.spool_bytes),
            pairs: Vec::new(),
            runs: Vec::new(),
            next: 0,
        });
        late.lines.write(line)?;
        late.pairs.push((hash, late.next));
        late.next += 1;
        if late.pairs.len() >= self.limits.run {
            late.runs.push(write_run(&self.dir, &mut late.pairs)?);
        }
        Ok(())
    }

    /// Writes the lines to `out` and returns how many there were.
    pub(crate) fn finish(self, out: &mut dyn Write) -> io::Result<usize> {
        let Self {
            early,
            late,
            early_count,
            seen,
            ..
        } = self;
        drop(seen);
        let (_temp, mut reader) = early.reader()?;
        io::copy(&mut reader, out)?;
        let Some(mut late) = late else {
            return Ok(early_count);
        };
        // Line numbers of the second part whose hash came earlier in it.
        let mut repeated = vec![0u64; usize::try_from(late.next.div_ceil(64)).unwrap_or(0)];
        late.pairs.sort_unstable();
        let mut sources: Vec<Pairs> = Vec::new();
        for run in &late.runs {
            sources.push(Box::new(read_run(&run.path)?));
        }
        sources.push(Box::new(
            std::mem::take(&mut late.pairs).into_iter().map(Ok),
        ));
        let mut heap = BinaryHeap::new();
        for (i, s) in sources.iter_mut().enumerate() {
            if let Some(p) = s.next() {
                heap.push(Reverse((p?, i)));
            }
        }
        let mut last = None;
        while let Some(Reverse(((hash, line), i))) = heap.pop() {
            if last == Some(hash) {
                repeated[(line / 64) as usize] |= 1 << (line % 64);
            }
            last = Some(hash);
            if let Some(p) = sources[i].next() {
                heap.push(Reverse((p?, i)));
            }
        }
        drop(sources);
        let (_temp, mut reader) = late.lines.reader()?;
        let mut count = early_count;
        let mut buf = Vec::new();
        for line in 0..late.next {
            buf.clear();
            reader.read_until(b'\n', &mut buf)?;
            if repeated[(line / 64) as usize] & (1 << (line % 64)) == 0 {
                out.write_all(&buf)?;
                count += 1;
            }
        }
        Ok(count)
    }
}

/// A 128-bit hash of `line` (a collision, which would drop a triple, has odds of about 1e-26
/// for a million lines).
fn hash(line: &[u8]) -> u128 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let half = |seed: u8| {
        let mut h = DefaultHasher::new();
        seed.hash(&mut h);
        line.hash(&mut h);
        h.finish()
    };
    (u128::from(half(0)) << 64) | u128::from(half(1))
}

const PAIR: usize = 24;

/// (hash, line number) pairs in order, from a run or from memory.
type Pairs = Box<dyn Iterator<Item = io::Result<(u128, u64)>>>;

fn write_run(dir: &Path, pairs: &mut Vec<(u128, u64)>) -> io::Result<TempFile> {
    pairs.sort_unstable();
    let (temp, file) = TempFile::create(dir)?;
    let mut w = BufWriter::with_capacity(1 << 20, file);
    for (hash, line) in pairs.drain(..) {
        w.write_all(&hash.to_le_bytes())?;
        w.write_all(&line.to_le_bytes())?;
    }
    w.flush()?;
    Ok(temp)
}

fn read_run(path: &Path) -> io::Result<impl Iterator<Item = io::Result<(u128, u64)>> + use<>> {
    let mut r = BufReader::with_capacity(1 << 16, File::open(path)?);
    Ok(std::iter::from_fn(move || {
        let mut b = [0u8; PAIR];
        match r.read_exact(&mut b) {
            Ok(()) => {
                let hash = u128::from_le_bytes(b[..16].try_into().unwrap_or_default());
                let line = u64::from_le_bytes(b[16..].try_into().unwrap_or_default());
                Some(Ok((hash, line)))
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => None,
            Err(e) => Some(Err(e)),
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[String], limits: Limits) -> (Vec<u8>, usize) {
        let dir = tripletrail_testdir::tempdir("lift-out");
        let mut out = Output::new(&dir, limits);
        for l in lines {
            out.line(l.as_bytes()).unwrap();
        }
        let mut bytes = Vec::new();
        let n = out.finish(&mut bytes).unwrap();
        (bytes, n)
    }

    #[test]
    fn small_limits_give_what_the_set_alone_gives() {
        // Repeats within the first part, across parts and within the second part, over
        // several runs and spooled to files.
        let lines: Vec<String> = (0..500)
            .map(|i| {
                format!(
                    "<http://e/s{}> <http://e/p> <http://e/o> .\n",
                    (i * 7) % 113
                )
            })
            .collect();
        let unbounded = run(&lines, Limits::default());
        assert_eq!(unbounded.1, 113);
        for limits in [
            Limits {
                spool_bytes: 64,
                seen: 4,
                run: 3,
            },
            Limits {
                spool_bytes: 1,
                seen: 0,
                run: 1,
            },
            Limits {
                spool_bytes: 1 << 20,
                seen: 50,
                run: 1000,
            },
        ] {
            assert_eq!(run(&lines, limits), unbounded, "{limits:?}");
        }
    }

    #[test]
    fn temporary_files_are_removed() {
        let dir = tripletrail_testdir::tempdir("lift-out-removed");
        let mut out = Output::new(
            &dir,
            Limits {
                spool_bytes: 8,
                seen: 2,
                run: 2,
            },
        );
        for i in 0..40 {
            out.line(format!("<http://e/{}> <http://e/p> 1 .\n", i % 9).as_bytes())
                .unwrap();
        }
        assert!(std::fs::read_dir(&dir).unwrap().count() > 0);
        let mut bytes = Vec::new();
        assert_eq!(out.finish(&mut bytes).unwrap(), 9);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
