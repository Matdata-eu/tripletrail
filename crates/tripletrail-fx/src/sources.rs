//! Reading sources: where a location may be read from, the bytes of a source, and the
//! parsers of the formats as plain values (JSON values, CSV rows). The Facade-X triplifiers
//! are built on these, and so can other mappings of the same sources (RML).

pub use crate::spreadsheet::sheet_cells;
use crate::{FxConfig, FxError, Options};
use std::path::PathBuf;
use std::sync::Arc;

/// The file a location names (a path, or a `file:` URL), resolved against
/// [`FxConfig::base`], if [`FxConfig::allowed`] lets it be read.
pub fn resolve(config: &FxConfig, location: &str) -> Result<PathBuf, FxError> {
    let path = if let Some(rest) = location.strip_prefix("file://") {
        PathBuf::from(percent_decode(rest))
    } else if location.contains("://") {
        return Err(FxError::Unsupported(format!(
            "only local files can be read yet, not {location}"
        )));
    } else {
        PathBuf::from(location)
    };
    let path = config.base.join(path);
    if let Some(allowed) = &config.allowed {
        let dirs: Vec<PathBuf> = allowed
            .iter()
            .filter_map(|dir| dir.canonicalize().ok())
            .collect();
        let denied = || {
            FxError::Denied(format!(
                "{location} is outside the directories sources may be read from"
            ))
        };
        let real = match path.canonicalize() {
            Ok(real) => real,
            // A path that does not exist: denied all the same when it is outside, so that
            // the error does not tell whether a file outside exists.
            Err(source) => {
                if !dirs.iter().any(|dir| lexical(&path).starts_with(dir)) {
                    return Err(denied());
                }
                return Err(FxError::Read {
                    path: path.display().to_string(),
                    source,
                });
            }
        };
        if !dirs.iter().any(|dir| real.starts_with(dir)) {
            return Err(denied());
        }
    }
    Ok(path)
}

/// `path` made absolute, with `.` and `..` taken out without reading the file system.
fn lexical(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            c => out.push(c),
        }
    }
    out
}

/// The bytes of a source, from the `content` option or the file of the `location` option.
pub struct Source {
    bytes: Vec<u8>,
    /// The location and the file it names, when the source is a file. For a URL, the path
    /// is the URL itself, and no file.
    pub location: Option<(String, PathBuf)>,
    /// The media type a URL was served with.
    pub media_type: Option<String>,
    /// Whether the source was read over HTTP.
    pub remote: bool,
}

impl Source {
    /// The bytes, without a UTF-8 byte order mark.
    pub fn bytes(&self) -> &[u8] {
        self.bytes
            .strip_prefix(b"\xEF\xBB\xBF")
            .unwrap_or(&self.bytes)
    }
}

/// Reads the source the options name: `content`, else `location`. The `charset` option may
/// only be UTF-8 for now.
/// The scheme of a file held in memory: `location=upload:NAME`.
pub const UPLOAD: &str = "upload:";

/// Files held in memory, by `upload:NAME` ([`crate::FxService::with_upload`]).
#[derive(Clone, Default)]
pub struct Uploads(pub(crate) Arc<std::collections::HashMap<String, Arc<[u8]>>>);

impl Uploads {
    fn get(&self, location: &str) -> Result<Option<&Arc<[u8]>>, FxError> {
        if !location.starts_with(UPLOAD) {
            return Ok(None);
        }
        self.0
            .get(location)
            .map(Some)
            .ok_or_else(|| FxError::Option(format!("{location}: no such upload")))
    }
}

pub fn read(config: &FxConfig, uploads: &Uploads, options: &Options) -> Result<Source, FxError> {
    if let Some(charset) = options.get("charset")
        && !charset.eq_ignore_ascii_case("utf-8")
        && !charset.eq_ignore_ascii_case("utf8")
    {
        return Err(FxError::Unsupported(format!(
            "charset {charset} (only UTF-8)"
        )));
    }
    if let (Some(archive), Some(location)) = (options.get("from-archive"), options.get("location"))
    {
        let bytes = if let Some(bytes) = uploads.get(archive)? {
            bytes.to_vec()
        } else if crate::remote::is_remote(archive) {
            crate::remote::fetch(&config.remote, archive, options)?.bytes
        } else {
            let path = resolve(config, archive)?;
            std::fs::read(&path).map_err(|source| FxError::Read {
                path: path.display().to_string(),
                source,
            })?
        };
        let kind = match options.get("archive-format") {
            Some(f) => crate::archive::Kind::from_name(f)?,
            None => crate::archive::Kind::from_name(archive)?,
        }
        .ok_or_else(|| {
            FxError::Option(format!(
                "from-archive {archive}: not a zip or tar archive (set archive-format)"
            ))
        })?;
        return Ok(Source {
            bytes: crate::archive::entry(&bytes, kind, location)?,
            location: Some((location.to_owned(), config.base.join(location))),
            media_type: None,
            remote: false,
        });
    }
    match (options.get("content"), options.get("location")) {
        (Some(content), _) => Ok(Source {
            bytes: content.as_bytes().to_vec(),
            location: None,
            media_type: None,
            remote: false,
        }),
        (None, Some(location)) if location.starts_with(UPLOAD) => Ok(Source {
            bytes: uploads
                .get(location)?
                .map(|b| b.to_vec())
                .unwrap_or_default(),
            location: Some((location.to_owned(), PathBuf::from(location))),
            media_type: None,
            remote: false,
        }),
        (None, Some(location)) if crate::remote::is_remote(location) => {
            let fetched = crate::remote::fetch(&config.remote, location, options)?;
            Ok(Source {
                bytes: fetched.bytes,
                location: Some((location.to_owned(), PathBuf::from(location))),
                media_type: fetched.media_type,
                remote: true,
            })
        }
        (None, Some(location)) => {
            let path = resolve(config, location)?;
            let bytes = std::fs::read(&path).map_err(|source| FxError::Read {
                path: path.display().to_string(),
                source,
            })?;
            Ok(Source {
                bytes,
                location: Some((location.to_owned(), path)),
                media_type: None,
                remote: false,
            })
        }
        (None, None) => Err(FxError::Option("no location or content".into())),
    }
}

/// A JSON document.
pub fn json(bytes: &[u8]) -> Result<serde_json::Value, FxError> {
    serde_json::from_slice(bytes).map_err(|e| FxError::Parse(format!("JSON: {e}")))
}

/// The records an XPath `iterator` selects in an XML document, and per record the string
/// values each of `references` selects (XPaths relative to the record, or absolute): the
/// text of an element and its descendants, an attribute's value, a text node's text.
/// The XPath subset of `xml.path` (see `docs/facade-x.md`).
pub fn xml_records(
    bytes: &[u8],
    iterator: &str,
    references: &[&str],
) -> Result<Vec<Vec<Vec<String>>>, FxError> {
    crate::xml::records(bytes, iterator, references)
}

/// How to read a CSV source, from the `csv.*` options.
#[derive(Debug, Clone)]
pub struct CsvOptions {
    /// `csv.headers`: name the columns from a header row.
    pub headers: bool,
    /// `csv.headers-row`: the row (from 1) of the headers. The rows before it are data.
    pub header_row: usize,
    /// `csv.headers.sanitize`: replace each run of characters other than letters and digits
    /// in header names with this, and drop it at both ends.
    pub sanitize: Option<String>,
    pub delimiter: u8,
    /// `None`: no quoting.
    pub quote: Option<u8>,
    /// `csv.null-string`: cells equal to it are null.
    pub null: Option<String>,
    /// Trim the spaces around cells (formats `TDF` and `Oracle`).
    pub trim: bool,
    /// The escape character of the formats that have one (`MySQL`, `PostgreSQLText`,
    /// `Oracle`, `InformixUnload`): a backslash.
    pub escape: Option<u8>,
}

impl CsvOptions {
    /// The options of a source. `tab`: tab-separated unless the options say otherwise (a
    /// `.tsv` or `.tab` file, or the media type `text/tab-separated-values`).
    ///
    /// The delimiter is `csv.delimiter`, else that of `csv.format`, else tab when `tab`,
    /// else comma. Formats, as Apache Commons CSV: `Default`, `RFC4180`, `Excel`, `TDF` (tab,
    /// trimmed), `PostgreSQLCsv` (empty cells are null), `InformixUnloadCsv`, and those with a
    /// backslash escape: `MySQL` (tab, no quoting, `\N` is null), `PostgreSQLText` (tab, `\N`
    /// is null), `Oracle` (comma, trimmed, `\N` is null) and `InformixUnload` (`|`).
    pub fn new(options: &Options, tab: bool) -> Result<Self, FxError> {
        let format = options.get("csv.format");
        let null_n = || Some("\\N".to_owned());
        let mut quote_default = Some(b'"');
        let (mut delimiter, mut null, trim, escape) = match format
            .map(|f| f.to_ascii_uppercase().replace('_', ""))
        {
            None => (if tab { b'\t' } else { b',' }, None, false, None),
            Some(f) => match f.as_str() {
                "DEFAULT" | "RFC4180" | "EXCEL" | "INFORMIXUNLOADCSV" => (b',', None, false, None),
                "TDF" => (b'\t', None, true, None),
                "POSTGRESQLCSV" => (b',', Some(String::new()), false, None),
                "MYSQL" => {
                    quote_default = None;
                    (b'\t', null_n(), false, Some(b'\\'))
                }
                "POSTGRESQLTEXT" => (b'\t', null_n(), false, Some(b'\\')),
                "ORACLE" => (b',', null_n(), true, Some(b'\\')),
                "INFORMIXUNLOAD" => (b'|', None, false, Some(b'\\')),
                _ => {
                    return Err(FxError::Unsupported(format!(
                        "csv.format {} (Default, RFC4180, Excel, TDF, PostgreSQLCsv, \
                         InformixUnloadCsv, MySQL, PostgreSQLText, Oracle or InformixUnload)",
                        format.unwrap_or_default()
                    )));
                }
            },
        };
        if let Some(d) = options.get("csv.delimiter") {
            delimiter = one_byte("csv.delimiter", d)?;
        }
        let quote = match options.get("csv.quote-char") {
            None => quote_default,
            Some(q) if q.eq_ignore_ascii_case("true") => Some(b'"'),
            Some(q) if q.is_empty() || q.eq_ignore_ascii_case("false") => None,
            Some(q) => Some(one_byte("csv.quote-char", q)?),
        };
        if let Some(n) = options.get("csv.null-string") {
            null = Some(n.to_owned());
        }
        let header_row = match options.get("csv.headers-row") {
            None => 1,
            Some(n) => n
                .trim()
                .parse::<usize>()
                .ok()
                .filter(|&n| n >= 1)
                .ok_or_else(|| {
                    FxError::Option(format!("csv.headers-row must be 1 or more, not {n:?}"))
                })?,
        };
        let sanitize = match options.get("csv.headers.sanitize") {
            None => None,
            Some(s) if s.eq_ignore_ascii_case("false") => None,
            Some(s) if s.eq_ignore_ascii_case("true") => Some("_".to_owned()),
            Some(s) => Some(s.to_owned()),
        };
        Ok(Self {
            headers: options.flag("csv.headers", false)?,
            header_row,
            sanitize,
            delimiter,
            quote,
            null,
            trim,
            escape,
        })
    }

    fn reader<'a>(&self, bytes: &'a [u8]) -> Records<'a> {
        if let Some(escape) = self.escape {
            return Records::Escaped(Escaped {
                bytes,
                at: 0,
                line: 1,
                delimiter: self.delimiter,
                quote: self.quote,
                escape,
                trim: self.trim,
            });
        }
        Records::Csv(
            csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(self.delimiter)
                .quote(self.quote.unwrap_or(b'"'))
                .quoting(self.quote.is_some())
                .trim(if self.trim {
                    csv::Trim::All
                } else {
                    csv::Trim::None
                })
                .from_reader(bytes),
        )
    }
}

/// The records of a CSV source: through the `csv` crate, or, for the formats with an escape
/// character, which it reads only inside quotes, [`Escaped`].
enum Records<'a> {
    Csv(csv::Reader<&'a [u8]>),
    Escaped(Escaped<'a>),
}

/// A reader of CSV with a backslash escape, as Apache Commons CSV reads it: `\r`, `\n`, `\t`,
/// `\b` and `\f` are those characters, an escaped delimiter, quote, escape or line break is
/// itself, and any other escaped character keeps its backslash (so `\N` stays `\N`). Blank
/// lines are skipped.
struct Escaped<'a> {
    bytes: &'a [u8],
    at: usize,
    line: usize,
    delimiter: u8,
    quote: Option<u8>,
    escape: u8,
    trim: bool,
}

impl Escaped<'_> {
    fn read_record(&mut self, record: &mut csv::StringRecord) -> Result<bool, FxError> {
        record.clear();
        let b = self.bytes;
        // Blank lines.
        while self.at < b.len() && matches!(b[self.at], b'\n' | b'\r') {
            if b[self.at] == b'\n' {
                self.line += 1;
            }
            self.at += 1;
        }
        if self.at >= b.len() {
            return Ok(false);
        }
        let mut field: Vec<u8> = Vec::new();
        loop {
            // A field: quoted, or up to the delimiter or the line's end.
            field.clear();
            let mut quoted = false;
            let mut start = self.at;
            if self.trim {
                while start < b.len() && b[start] == b' ' {
                    start += 1;
                }
            }
            if self.quote.is_some_and(|q| b.get(start) == Some(&q)) {
                let q = self.quote.unwrap_or(b'"');
                quoted = true;
                self.at = start + 1;
                loop {
                    let Some(&c) = b.get(self.at) else {
                        return Err(FxError::Parse(format!(
                            "CSV line {}: a quoted cell is not closed",
                            self.line
                        )));
                    };
                    self.at += 1;
                    if c == q {
                        if b.get(self.at) == Some(&q) {
                            field.push(q);
                            self.at += 1;
                        } else {
                            break;
                        }
                    } else if c == self.escape {
                        self.escaped(&mut field)?;
                    } else {
                        if c == b'\n' {
                            self.line += 1;
                        }
                        field.push(c);
                    }
                }
                // Up to the delimiter: text after the closing quote is kept.
                while let Some(&c) = b.get(self.at) {
                    if c == self.delimiter || c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.at += 1;
                    if !(self.trim && c == b' ') {
                        field.push(c);
                    }
                }
            } else {
                while let Some(&c) = b.get(self.at) {
                    if c == self.delimiter || c == b'\n' || c == b'\r' {
                        break;
                    }
                    self.at += 1;
                    if c == self.escape {
                        self.escaped(&mut field)?;
                    } else {
                        field.push(c);
                    }
                }
            }
            let text = String::from_utf8(std::mem::take(&mut field))
                .map_err(|_| FxError::Parse(format!("CSV line {}: not UTF-8", self.line)))?;
            record.push_field(if self.trim && !quoted {
                text.trim()
            } else {
                &text
            });
            match b.get(self.at) {
                Some(&c) if c == self.delimiter => self.at += 1,
                Some(b'\r') => {
                    self.at += 1;
                    if b.get(self.at) == Some(&b'\n') {
                        self.at += 1;
                    }
                    self.line += 1;
                    return Ok(true);
                }
                Some(b'\n') => {
                    self.at += 1;
                    self.line += 1;
                    return Ok(true);
                }
                _ => return Ok(true),
            }
        }
    }

    /// The character after an escape, read into `field`.
    fn escaped(&mut self, field: &mut Vec<u8>) -> Result<(), FxError> {
        let Some(&c) = self.bytes.get(self.at) else {
            return Err(FxError::Parse(format!(
                "CSV line {}: the file ends in an escape",
                self.line
            )));
        };
        self.at += 1;
        match c {
            b'r' => field.push(b'\r'),
            b'n' => field.push(b'\n'),
            b't' => field.push(b'\t'),
            b'b' => field.push(0x08),
            b'f' => field.push(0x0C),
            b'\n' | b'\r' | 0x0C | b'\t' | 0x08 => {
                if c == b'\n' {
                    self.line += 1;
                }
                field.push(c);
            }
            c if c == self.delimiter || c == self.escape || Some(c) == self.quote => field.push(c),
            c => {
                field.push(self.escape);
                field.push(c);
            }
        }
        Ok(())
    }
}

/// The rows of a CSV source, read one at a time with [`CsvRows::next_row`].
pub struct CsvRows<'a> {
    reader: Records<'a>,
    /// The name of each column, by position; `None` for a column without a header (or all
    /// of them without `csv.headers`). Names are trimmed, sanitized when asked, and made
    /// unique by appending `_1`, `_2`, ... as SPARQL Anything does.
    pub headers: Vec<Option<String>>,
    null: Option<String>,
    /// The record number of the header row, skipped when it comes.
    skip: Option<usize>,
    record: csv::StringRecord,
    read: usize,
}

impl<'a> CsvRows<'a> {
    pub fn new(bytes: &'a [u8], options: &CsvOptions) -> Result<Self, FxError> {
        let mut headers = Vec::new();
        let mut skip = None;
        if options.headers {
            let mut reader = options.reader(bytes);
            let mut record = csv::StringRecord::new();
            let mut n = 0;
            while n < options.header_row && read_record(&mut reader, &mut record)? {
                n += 1;
            }
            if n == options.header_row {
                headers = header_names(&record, options.sanitize.as_deref());
                if headers.iter().any(Option::is_some) {
                    skip = Some(n);
                }
            }
        }
        Ok(Self {
            reader: options.reader(bytes),
            headers,
            null: options.null.clone(),
            skip,
            record: csv::StringRecord::new(),
            read: 0,
        })
    }

    /// The next data row, or `None` at the end.
    pub fn next_row(&mut self) -> Result<Option<Row<'_>>, FxError> {
        loop {
            if !read_record(&mut self.reader, &mut self.record)? {
                return Ok(None);
            }
            self.read += 1;
            if Some(self.read) != self.skip {
                return Ok(Some(Row {
                    record: &self.record,
                    null: self.null.as_deref(),
                }));
            }
        }
    }
}

/// A row of a CSV source.
pub struct Row<'a> {
    record: &'a csv::StringRecord,
    null: Option<&'a str>,
}

impl<'a> Row<'a> {
    /// The cells by column position (from 0); `None` for a null cell.
    pub fn cells(&self) -> impl Iterator<Item = Option<&'a str>> + 'a {
        let null = self.null;
        self.record
            .iter()
            .map(move |cell| if null == Some(cell) { None } else { Some(cell) })
    }
}

fn read_record(reader: &mut Records<'_>, record: &mut csv::StringRecord) -> Result<bool, FxError> {
    match reader {
        Records::Csv(reader) => reader.read_record(record).map_err(|e| {
            let line = e.position().map(|p| p.line()).unwrap_or_default();
            FxError::Parse(format!("CSV line {line}: {e}"))
        }),
        Records::Escaped(reader) => reader.read_record(record),
    }
}

fn header_names(record: &csv::StringRecord, sanitize: Option<&str>) -> Vec<Option<String>> {
    let mut names: Vec<Option<String>> = Vec::new();
    for cell in record {
        let mut name = cell.trim().to_owned();
        if let Some(with) = sanitize {
            name = sanitized(&name, with);
        }
        if name.is_empty() {
            names.push(None);
            continue;
        }
        let mut c = 0;
        while names.iter().any(|n| n.as_deref() == Some(name.as_str())) {
            c += 1;
            name.push('_');
            name.push_str(&c.to_string());
        }
        names.push(Some(name));
    }
    names
}

/// `name` with each run of characters other than letters and digits replaced by `with`, and
/// without `with` at either end.
fn sanitized(name: &str, with: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in name.chars() {
        if c.is_alphanumeric() {
            if gap && !out.is_empty() {
                out.push_str(with);
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

/// A one-character option (`;`, `|`, a tab, or `\t` for a tab).
fn one_byte(name: &str, value: &str) -> Result<u8, FxError> {
    match value {
        "\\t" => Ok(b'\t'),
        v if v.len() == 1 => Ok(v.as_bytes()[0]),
        v => Err(FxError::Option(format!(
            "{name} must be one ASCII character, not {v:?}"
        ))),
    }
}

pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(b) = s
                .get(i + 1..i + 3)
                .and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_like_sparql_anything() {
        assert_eq!(sanitized("Production Year", "-"), "Production-Year");
        assert_eq!(sanitized("Model:", "-"), "Model");
        assert_eq!(sanitized("Purchase $ Price", "-"), "Purchase-Price");
        assert_eq!(sanitized("meta:kritikalität", "_"), "meta_kritikalität");
        assert_eq!(sanitized("  a  ", "_"), "a");
    }

    #[test]
    fn duplicate_headers_get_suffixes() {
        let record = csv::StringRecord::from(vec!["a", "a", "", "a", " b "]);
        assert_eq!(
            header_names(&record, None),
            vec![
                Some("a".into()),
                Some("a_1".into()),
                None,
                Some("a_1_2".into()),
                Some("b".into())
            ]
        );
    }
}
