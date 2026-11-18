//! The hyperlinks and comments of an xlsx or xlsm sheet, which calamine does not read, for
//! `spreadsheet.composite-values`.
//!
//! A sheet's part lists its hyperlinks (`<hyperlink ref="C12" r:id="rId1" location="…"
//! display="…"/>`, the target of an external link in the sheet's relationships), and its
//! relationships name its comments part (`<comment ref="B12" authorId="0"><text>…</text>`).

use crate::FxError;
use quick_xml::events::{BytesStart, Event};
use std::collections::HashMap;
use std::io::{Cursor, Read};

/// What a cell has besides its value.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Extras {
    /// A hyperlink's address: the URL of an external link, else its location in the workbook.
    pub address: Option<String>,
    /// A hyperlink's display text.
    pub label: Option<String>,
    /// Whether the cell has a hyperlink.
    pub link: bool,
    pub author: Option<String>,
    /// A comment's text.
    pub comment: Option<String>,
}

/// A hyperlink over a range of cells (rows and columns from 0, first and last).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub rows: (u32, u32),
    pub cols: (u32, u32),
    pub address: Option<String>,
    pub label: Option<String>,
}

/// A sheet's hyperlinks, as ranges (a range may cover cells the sheet does not have, so it
/// is never expanded here), and its comments by (row, column) from 0.
#[derive(Debug, Default)]
pub(crate) struct SheetExtras {
    pub links: Vec<Link>,
    pub comments: HashMap<(u32, u32), Extras>,
}

/// The hyperlinks and comments of the sheet `name`; none for a file that is not an xlsx or
/// xlsm, or a sheet without any.
pub(crate) fn sheet_extras(
    bytes: &[u8],
    name: &str,
    max_bytes: u64,
) -> Result<SheetExtras, FxError> {
    let mut out = SheetExtras::default();
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(out);
    };
    let mut read = |path: &str| -> Option<Vec<u8>> {
        let entry = zip.by_name(path).ok()?;
        let mut data = Vec::new();
        entry.take(max_bytes).read_to_end(&mut data).ok()?;
        Some(data)
    };
    let (Some(workbook), Some(workbook_rels)) =
        (read("xl/workbook.xml"), read("xl/_rels/workbook.xml.rels"))
    else {
        return Ok(out);
    };
    // The sheet's relationship id, then its part.
    let mut sheet_id = None;
    each_element(&workbook, |e| {
        if e.local_name().as_ref() == b"sheet" && attr(e, b"name").as_deref() == Some(name) {
            sheet_id = attr(e, b"id");
        }
    })?;
    let Some(sheet_id) = sheet_id else {
        return Ok(out);
    };
    let rels = relationships(&workbook_rels)?;
    let Some((_, target)) = rels.get(&sheet_id) else {
        return Ok(out);
    };
    let sheet_path = resolve("xl/", target);
    let Some(sheet) = read(&sheet_path) else {
        return Ok(out);
    };
    let (dir, file) = sheet_path.rsplit_once('/').unwrap_or(("", &sheet_path));
    let sheet_rels = read(&format!("{dir}/_rels/{file}.rels"))
        .map(|r| relationships(&r))
        .transpose()?
        .unwrap_or_default();

    each_element(&sheet, |e| {
        if e.local_name().as_ref() != b"hyperlink" {
            return;
        }
        let Some(range) = attr(e, b"ref") else {
            return;
        };
        let external = attr(e, b"id")
            .and_then(|id| sheet_rels.get(&id))
            .map(|(_, target)| target.clone());
        let location = attr(e, b"location");
        let address = match (external, location) {
            (Some(url), Some(loc)) => Some(format!("{url}#{loc}")),
            (Some(url), None) => Some(url),
            (None, loc) => loc,
        };
        if let Some((rows, cols)) = range_of(&range) {
            out.links.push(Link {
                rows,
                cols,
                address,
                label: attr(e, b"display"),
            });
        }
    })?;

    // Comments: the part the sheet's relationships name.
    if let Some((_, target)) = sheet_rels
        .values()
        .find(|(kind, _)| kind.ends_with("/comments"))
        && let Some(comments) = read(&resolve(&format!("{dir}/"), target))
    {
        read_comments(&comments, &mut out.comments)?;
    }
    Ok(out)
}

/// The comments of a comments part, with their authors.
fn read_comments(xml: &[u8], out: &mut HashMap<(u32, u32), Extras>) -> Result<(), FxError> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut authors: Vec<String> = Vec::new();
    let mut in_author = false;
    let mut author = String::new();
    // The comment being read: its cell, author and text.
    let mut current: Option<((u32, u32), Option<usize>, String)> = None;
    let mut in_t = false;
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| parse(&e))? {
            Event::Eof => break,
            Event::Start(e) => match e.local_name().as_ref() {
                b"author" => {
                    in_author = true;
                    author.clear();
                }
                b"comment" => {
                    current = attr(&e, b"ref").and_then(|r| cell_ref(&r)).map(|cell| {
                        let id = attr(&e, b"authorId").and_then(|a| a.parse().ok());
                        (cell, id, String::new())
                    });
                }
                b"t" => in_t = true,
                _ => {}
            },
            Event::Text(t) => {
                let text = t
                    .xml_content(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| parse(&e))?;
                if in_author {
                    author.push_str(&text);
                } else if in_t && let Some((_, _, body)) = current.as_mut() {
                    body.push_str(&text);
                }
            }
            Event::GeneralRef(r) => {
                let text = match r.resolve_char_ref().ok().flatten() {
                    Some(c) => c.to_string(),
                    None => r
                        .decode()
                        .ok()
                        .and_then(|name| quick_xml::escape::resolve_predefined_entity(&name))
                        .unwrap_or_default()
                        .to_owned(),
                };
                if in_author {
                    author.push_str(&text);
                } else if in_t && let Some((_, _, body)) = current.as_mut() {
                    body.push_str(&text);
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"author" => {
                    in_author = false;
                    authors.push(std::mem::take(&mut author));
                }
                b"t" => in_t = false,
                b"comment" => {
                    if let Some((cell, id, body)) = current.take() {
                        let x = out.entry(cell).or_default();
                        x.author = id.and_then(|i| authors.get(i).cloned());
                        x.comment = Some(body);
                    }
                }
                _ => {}
            },
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

/// Calls `f` on every start or empty element of `xml`.
fn each_element(xml: &[u8], mut f: impl FnMut(&BytesStart<'_>)) -> Result<(), FxError> {
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf).map_err(|e| parse(&e))? {
            Event::Eof => return Ok(()),
            Event::Start(e) | Event::Empty(e) => f(&e),
            _ => {}
        }
        buf.clear();
    }
}

/// A part's relationships: id → (type, target).
fn relationships(xml: &[u8]) -> Result<HashMap<String, (String, String)>, FxError> {
    let mut rels = HashMap::new();
    each_element(xml, |e| {
        if e.local_name().as_ref() == b"Relationship"
            && let (Some(id), Some(target)) = (attr(e, b"Id"), attr(e, b"Target"))
        {
            let kind = attr(e, b"Type").unwrap_or_default();
            rels.insert(id, (kind, target));
        }
    })?;
    Ok(rels)
}

/// A relationship target relative to the folder `dir` (with its `/`), or from the package
/// root when it starts with `/`; `..` steps up.
fn resolve(dir: &str, target: &str) -> String {
    if let Some(absolute) = target.strip_prefix('/') {
        return absolute.to_owned();
    }
    let mut parts: Vec<&str> = dir.split('/').filter(|p| !p.is_empty()).collect();
    for part in target.split('/') {
        match part {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            p => parts.push(p),
        }
    }
    parts.join("/")
}

/// An attribute's value by its local name (`r:id` is `id`), unescaped.
fn attr(e: &BytesStart<'_>, name: &[u8]) -> Option<String> {
    e.attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| {
            a.normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .ok()
                .map(|v| v.into_owned())
        })
}

/// A cell reference (`C12`, `$C$12`) as (row, column) from 0.
fn cell_ref(r: &str) -> Option<(u32, u32)> {
    let r = r.replace('$', "");
    let split = r.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = r.split_at(split);
    if letters.is_empty() || letters.len() > 3 || !letters.bytes().all(|b| b.is_ascii_alphabetic())
    {
        return None;
    }
    let col = letters.bytes().fold(0u32, |n, b| {
        n * 26 + u32::from(b.to_ascii_uppercase() - b'A' + 1)
    });
    let row: u32 = digits.parse().ok()?;
    (row >= 1).then(|| (row - 1, col - 1))
}

/// The rows and columns (first, last) of a reference or a range (`A1:B2`).
fn range_of(range: &str) -> Option<((u32, u32), (u32, u32))> {
    let (a, b) = range.split_once(':').unwrap_or((range, range));
    let ((r0, c0), (r1, c1)) = (cell_ref(a)?, cell_ref(b)?);
    Some(((r0.min(r1), r0.max(r1)), (c0.min(c1), c0.max(c1))))
}

fn parse(e: &dyn std::fmt::Display) -> FxError {
    FxError::Parse(format!("spreadsheet: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references() {
        assert_eq!(cell_ref("A1"), Some((0, 0)));
        assert_eq!(cell_ref("$C$12"), Some((11, 2)));
        assert_eq!(cell_ref("AA3"), Some((2, 26)));
        assert_eq!(cell_ref("A0"), None);
        assert_eq!(range_of("B2:A1"), Some(((0, 1), (0, 1))));
        assert_eq!(
            resolve("xl/worksheets/", "../comments1.xml"),
            "xl/comments1.xml"
        );
        assert_eq!(
            resolve("xl/", "/xl/worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
        assert_eq!(
            resolve("xl/", "worksheets/sheet1.xml"),
            "xl/worksheets/sheet1.xml"
        );
    }
}
