//! Spreadsheets (xlsx, xlsm, xlsb, xls, ods): one Facade-X graph per sheet, named
//! `<root><sheet name>`, each with its own `fx:Root` holding one container per row
//! (`rdf:_1`, `rdf:_2`, ...). Rows are numbered from the sheet's first used row; with
//! `blank-nodes=false` they are `<root><sheet name>_Row_<n>`.
//!
//! A row's cells run from its first to its last non-empty cell, so `rdf:_1` is the first
//! non-empty cell of that row (not necessarily column A), and empty cells in between give
//! `""` (dropped with `null-string=""`). With `spreadsheet.headers=true` the row
//! `spreadsheet.headers-row` (default 1) names the columns, counted from column A, and is
//! not a data row; the rows before it are data. These match SPARQL Anything, including that
//! header columns are counted from column A while data cells are counted from the row's
//! first cell.
//!
//! Values: text as `xsd:string`, numbers as `xsd:double` (written as Java writes doubles,
//! `1.0`, `1.5E10`), booleans as `xsd:boolean`, dates as UTC `xsd:dateTime` text in a plain
//! string (`2020-01-31T00:00:00Z`). Formula cells give the formula text (`A2+B2`), or with
//! `spreadsheet.evaluate-formulas=true` the value the file holds for it.
//! `spreadsheet.ignore-columns-with-no-header` drops cells of columns without a header.

use crate::FxError;
use crate::options::{Needs, Options};
use crate::tree::{Common, Sink, escape_iri, java_double, xsd_double};
use crate::xlsx_extras::{Extras, SheetExtras};
use calamine::{Data, Reader, Sheets};
use oxigraph::model::vocab::xsd;
use oxigraph::model::{BlankNode, NamedNode, NamedOrBlankNode, Quad, Term};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Cursor;

/// How many cells a workbook may hold, by default (`spreadsheet.max-cells`). A cell costs
/// about 300 to 400 bytes while it is lifted, so the default stays under half a gigabyte.
pub(crate) const MAX_CELLS: usize = 1_000_000;

/// How many bytes the parts of an xlsx, xlsm or ods file may unpack to, by default
/// (`spreadsheet.max-bytes`), going by the sizes the zip file declares.
pub(crate) const MAX_BYTES: u64 = 1 << 30;

/// A limit from an option, else its default.
fn limit(options: &Options, name: &str, default: u64) -> Result<u64, FxError> {
    match options.get(name) {
        None => Ok(default),
        Some(n) => n
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| FxError::Option(format!("{name} must be a positive number, not {n:?}"))),
    }
}

/// Refuses a zip-based workbook (xlsx, xlsm, ods) whose parts declare more than `max` bytes
/// unpacked: the shared strings and styles are read whole.
fn check_unpacked(bytes: &[u8], max: u64) -> Result<(), FxError> {
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        // Not a zip (xls): calamine says what it is.
        return Ok(());
    };
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        if let Ok(entry) = zip.by_index_raw(i) {
            total = total.saturating_add(entry.size());
        }
    }
    if total > max {
        return Err(FxError::Unsupported(format!(
            "spreadsheet: the workbook unpacks to {total} bytes, more than {max} \
             (spreadsheet.max-bytes)"
        )));
    }
    Ok(())
}

/// Refuses an ods workbook that calamine would expand into more than `max` cells.
///
/// calamine reads every sheet of an ods file when it opens it, into a range as large as the
/// box around the sheet's non-empty cells, with repeated rows and columns
/// (`table:number-rows-repeated`, `table:number-columns-repeated`) written out. A file of a
/// few hundred bytes can ask for billions of cells, so content.xml is scanned first and
/// those boxes, and the cells of the rows as written, are counted. calamine also copies a
/// cell's text for each of its repeats and writes out runs of spaces
/// (`<text:s text:c="N"/>`), so the text of the cells, spaces written out and repeats
/// counted, is added up against `max_bytes`.
fn check_ods(bytes: &[u8], max: u64, max_bytes: u64) -> Result<(), FxError> {
    use quick_xml::events::Event;
    let Ok(mut zip) = zip::ZipArchive::new(Cursor::new(bytes)) else {
        return Ok(());
    };
    let Ok(content) = zip.by_name("content.xml") else {
        // Not an ods file (xlsx, xlsm).
        return Ok(());
    };
    let refuse = |n: u64| {
        FxError::Unsupported(format!(
            "spreadsheet: the workbook spans more than {max} cells (with its repeated rows \
             and columns written out, {n} or more; spreadsheet.max-cells)"
        ))
    };
    let parse =
        |e: &dyn std::fmt::Display| FxError::Parse(format!("spreadsheet: content.xml: {e}"));
    let repeats = |e: &quick_xml::events::BytesStart<'_>, name: &[u8]| -> Result<u64, FxError> {
        match e.try_get_attribute(name).map_err(|e| parse(&e))? {
            None => Ok(1),
            Some(a) => std::str::from_utf8(&a.value)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .ok_or_else(|| parse(&"a repeat count is not a number")),
        }
    };
    let mut reader = quick_xml::Reader::from_reader(std::io::BufReader::new(content));
    let mut buf = Vec::new();
    // Cells calamine holds before it makes the range, over all sheets.
    let mut written: u64 = 0;
    // Box of the current sheet: first and last non-empty row and column.
    let mut rows: Option<(u64, u64)> = None;
    let mut cols: Option<(u64, u64)> = None;
    let (mut row, mut col, mut row_repeats, mut extent) = (0u64, 0u64, 1u64, 0u64);
    let mut boxes: u64 = 0;
    // Bytes of cell text, as calamine copies it.
    let mut text: u64 = 0;
    // In a cell: how many times calamine copies it, its bytes so far, and the bytes it
    // has when it holds nothing (0 for a cell with a value type or formula).
    let mut cell: Option<(u64, u64, u64)> = None;
    let too_much_text = |n: u64| {
        FxError::Unsupported(format!(
            "spreadsheet: the workbook's cell text, with repeated cells and runs of spaces \
             (text:s) written out, comes to more than {max_bytes} bytes ({n} or more; \
             spreadsheet.max-bytes)"
        ))
    };
    loop {
        let event = reader.read_event_into(&mut buf).map_err(|e| parse(&e))?;
        let length = match &event {
            Event::Text(t) => Some(t.len()),
            Event::CData(t) => Some(t.len()),
            Event::GeneralRef(t) => Some(t.len() + 2),
            _ => None,
        };
        if let Some(n) = length {
            if let Some((_, bytes, _)) = &mut cell {
                *bytes = bytes.saturating_add(n as u64);
            }
            buf.clear();
            continue;
        }
        let (start, e) = match event {
            Event::Eof => break,
            Event::Start(e) => (true, e),
            Event::Empty(e) => (false, e),
            Event::End(e) => {
                match e.name().as_ref() {
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        // A cell without a value or any text is empty.
                        if let Some((times, bytes, _)) = cell.take().filter(|c| c.1 > c.2) {
                            text = text.saturating_add(times.saturating_mul(bytes));
                            if text > max_bytes {
                                return Err(too_much_text(text));
                            }
                        }
                    }
                    b"table:table-row" => {
                        row = row.saturating_add(row_repeats);
                        // The row as written holds its cells up to its last filled one.
                        written = written.saturating_add(extent);
                        if written > max {
                            return Err(refuse(written));
                        }
                    }
                    b"table:table" => {
                        if let (Some(r), Some(c)) = (rows, cols) {
                            let area = (r.1 - r.0 + 1).saturating_mul(c.1 - c.0 + 1);
                            boxes = boxes.saturating_add(area);
                        }
                        if boxes > max {
                            return Err(refuse(boxes));
                        }
                    }
                    _ => {}
                }
                buf.clear();
                continue;
            }
            _ => {
                buf.clear();
                continue;
            }
        };
        match e.name().as_ref() {
            b"table:table" => {
                (rows, cols, row) = (None, None, 0);
            }
            b"table:table-row" => {
                row_repeats = repeats(&e, b"table:number-rows-repeated")?;
                (col, extent) = (0, 0);
                if !start {
                    row = row.saturating_add(row_repeats);
                }
            }
            b"text:s" => {
                let n = repeats(&e, b"text:c")?;
                match &mut cell {
                    Some((_, bytes, _)) => *bytes = bytes.saturating_add(n),
                    None => text = text.saturating_add(n),
                }
                if text > max_bytes {
                    return Err(too_much_text(text));
                }
            }
            b"table:table-cell" | b"table:covered-table-cell" => {
                let n = repeats(&e, b"table:number-columns-repeated")?.max(1);
                // Its attributes (a string value, a formula) are copied too.
                let times = n.saturating_mul(row_repeats.max(1));
                let own = e.len() as u64;

                let filled = start
                    || e.try_get_attribute(b"office:value-type")
                        .map_err(|e| parse(&e))?
                        .is_some()
                    || e.try_get_attribute(b"table:formula")
                        .map_err(|e| parse(&e))?
                        .is_some();
                // An empty cell is no text: calamine writes out empty cells only before a
                // filled one, as the cell box counts.
                if start {
                    let has_value = e
                        .try_get_attribute(b"office:value-type")
                        .map_err(|e| parse(&e))?
                        .is_some()
                        || e.try_get_attribute(b"table:formula")
                            .map_err(|e| parse(&e))?
                            .is_some();
                    cell = Some((times, own, if has_value { 0 } else { own }));
                } else if filled {
                    text = text.saturating_add(times.saturating_mul(own));
                    if text > max_bytes {
                        return Err(too_much_text(text));
                    }
                }
                if filled {
                    let last = col.saturating_add(n - 1);
                    cols = Some(cols.map_or((col, last), |(a, b)| (a.min(col), b.max(last))));
                    let last_row = row.saturating_add(row_repeats.max(1) - 1);
                    rows =
                        Some(rows.map_or((row, last_row), |(a, b)| (a.min(row), b.max(last_row))));
                    extent = last.saturating_add(1);
                }
                col = col.saturating_add(n);
            }
            _ => {}
        }
        buf.clear();
    }
    Ok(())
}

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    common: &Common,
    needs: &Needs,
) -> Result<Vec<Quad>, FxError> {
    let composite = options.flag("spreadsheet.composite-values", false)?;
    let headers = options.flag("spreadsheet.headers", false)?;
    let header_row = match options.get("spreadsheet.headers-row") {
        None => 1,
        Some(n) => n
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|&n| n >= 1)
            .ok_or_else(|| {
                FxError::Option(format!(
                    "spreadsheet.headers-row must be 1 or more, not {n:?}"
                ))
            })?,
    };
    let evaluate = options.flag("spreadsheet.evaluate-formulas", false)?;
    let ignore_unnamed = options.flag("spreadsheet.ignore-columns-with-no-header", false)?;

    let max_cells = limit(options, "spreadsheet.max-cells", MAX_CELLS as u64)?;
    let max_bytes = limit(options, "spreadsheet.max-bytes", MAX_BYTES)?;
    let mut workbook = open(bytes, max_cells, max_bytes)?;
    let mut quads = Vec::new();
    let mut cells = Cells {
        count: 0,
        max: max_cells,
    };
    for name in workbook.sheet_names() {
        let sheet = cells.read(&mut workbook, &name, !evaluate, composite && evaluate)?;
        let extras = if composite {
            let found = crate::xlsx_extras::sheet_extras(bytes, &name, max_bytes)?;
            Some(cells.extras(&sheet, found)?)
        } else {
            None
        };
        let mut root = common.root.clone();
        escape_iri(&name, &mut root);
        let common = Common {
            root,
            ..common.clone()
        };
        let mut sink = Sink::new(&common, needs);
        cells.spend(sheet.gaps())?;
        sheet.triplify(
            &mut sink,
            headers.then_some(header_row - 1),
            ignore_unnamed,
            extras.as_ref(),
        )?;
        quads.append(&mut sink.quads);
    }
    Ok(quads)
}

/// Opens a workbook, after the checks that keep a small file from asking for more than
/// `max_cells` cells or `max_bytes` bytes.
fn open(bytes: &[u8], max_cells: u64, max_bytes: u64) -> Result<Sheets<Cursor<&[u8]>>, FxError> {
    check_unpacked(bytes, max_bytes)?;
    check_ods(bytes, max_cells, max_bytes)?;
    if crate::xls::is_compound(bytes) {
        crate::xls::check(bytes, max_cells, max_bytes)?;
    }
    calamine::open_workbook_auto_from_rs(Cursor::new(bytes))
        .map_err(|e| FxError::Parse(format!("spreadsheet: {e}")))
}

/// The non-empty cells of one sheet of a workbook (xlsx, xlsm, xlsb, xls, ods) as text, by
/// row and column (both from 0), for mappings other than Facade-X (RML). `sheet` names the
/// sheet; without it, the first one. Formula cells give the value the file holds for them.
///
/// Text is as written; numbers as Excel shows them by default (`10`, not `10.0`; `0.5`);
/// booleans `true` and `false`; dates `xsd:dateTime` text without a time zone
/// (`2020-01-31T00:00:00`), and times without a date `xsd:time` text (`13:05:00`). Error
/// cells are empty, and a row of only empty and error cells is left out. The workbook may hold at most 1,000,000
/// non-empty cells and unpack to at most 1 GiB, as Facade-X's default limits.
pub fn sheet_cells(
    bytes: &[u8],
    sheet: Option<&str>,
) -> Result<BTreeMap<u32, BTreeMap<u32, String>>, FxError> {
    let mut workbook = open(bytes, MAX_CELLS as u64, MAX_BYTES)?;
    let names = workbook.sheet_names();
    let name = match sheet {
        Some(s) => names
            .iter()
            .find(|n| n.as_str() == s)
            .ok_or_else(|| {
                FxError::Parse(format!(
                    "spreadsheet: no sheet {s:?}; the sheets are {names:?}"
                ))
            })?
            .clone(),
        None => names
            .first()
            .ok_or_else(|| FxError::Parse("spreadsheet: the workbook has no sheets".into()))?
            .clone(),
    };
    let mut cells = Cells {
        count: 0,
        max: MAX_CELLS as u64,
    };
    let sheet = cells.read(&mut workbook, &name, false, false)?;
    Ok(sheet
        .rows
        .into_iter()
        .map(|(r, row)| {
            let row = row
                .into_iter()
                .filter_map(|(c, cell)| match cell {
                    Owned::Value(v) => value_text(&v).map(|t| (c, t)),
                    Owned::Formula(f) => Some((c, f)),
                })
                .collect::<BTreeMap<_, _>>();
            (r, row)
        })
        // A row of only empty or error cells is no row: it must not become the header.
        .filter(|(_, row)| !row.is_empty())
        .collect())
}

/// A cell's value as [`sheet_cells`] gives it; `None` for an empty or error cell.
fn value_text(v: &Data) -> Option<String> {
    Some(match v {
        Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => number_text(*f),
        Data::Bool(b) => b.to_string(),
        // A time without a date (a value below 1) as `xsd:time` text.
        Data::DateTime(d) if d.is_datetime() && (0.0..1.0).contains(&d.as_f64()) => {
            let t = utc_date_time(d);
            t[t.find('T').map_or(0, |i| i + 1)..t.len() - 1].to_owned()
        }
        Data::DateTime(d) if d.is_datetime() => {
            let mut t = utc_date_time(d);
            t.pop();
            t
        }
        Data::DateTime(d) => number_text(d.as_f64()),
        Data::Error(_) | Data::Empty => return None,
    })
}

/// A number without a trailing `.0` when it is whole, else in Rust's shortest form.
fn number_text(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 {
        format!("{}", f as i64)
    } else {
        f.to_string()
    }
}

/// Reads sheets cell by cell, counting the non-empty cells of the workbook.
///
/// calamine's `Range` holds every cell of a sheet's bounding box, so a sheet with one cell
/// in A1 and one in XFD1048576 asks for 17 billion cells and aborts the process. xlsx and
/// xlsb sheets are therefore read through their cell readers into rows of their non-empty
/// cells. ods sheets come from calamine's range, after [`check_ods`] has bounded it; xls
/// sheets are read whole by calamine when the file is opened, after `xls::check` has
/// bounded them.
struct Cells {
    count: u64,
    max: u64,
}

impl Cells {
    /// Counts `n` more cells.
    fn spend(&mut self, n: u64) -> Result<(), FxError> {
        self.count = self.count.saturating_add(n);
        if self.count > self.max {
            return Err(FxError::Unsupported(format!(
                "spreadsheet: the workbook has more than {} cells, the empty ones between \
                 a row's cells included (spreadsheet.max-cells)",
                self.max
            )));
        }
        Ok(())
    }

    /// The hyperlinks and comments of the cells `sheet` has. A hyperlink's range is matched
    /// against the sheet's rows and cells, each row and cell it reaches counted as a cell, so
    /// many large or overlapping ranges cannot ask for more than `spreadsheet.max-cells`.
    fn extras(
        &mut self,
        sheet: &Sheet,
        found: SheetExtras,
    ) -> Result<HashMap<(u32, u32), Extras>, FxError> {
        let mut out: HashMap<(u32, u32), Extras> = found
            .comments
            .into_iter()
            .filter(|((r, c), _)| sheet.rows.get(r).is_some_and(|row| row.contains_key(c)))
            .collect();
        for link in found.links {
            for (&r, row) in sheet.rows.range(link.rows.0..=link.rows.1) {
                self.spend(1)?;
                for (&c, _) in row.range(link.cols.0..=link.cols.1) {
                    self.spend(1)?;
                    let x = out.entry((r, c)).or_default();
                    x.link = true;
                    x.address.clone_from(&link.address);
                    x.label.clone_from(&link.label);
                }
            }
        }
        Ok(out)
    }

    fn add(&mut self, sheet: &mut Sheet, (r, c): (u32, u32), cell: Owned) -> Result<(), FxError> {
        let row = sheet.rows.entry(r).or_default();
        if let Some(old) = row.get_mut(&c) {
            // A formula's text wins over the value the file holds for it.
            *old = cell;
            return Ok(());
        }
        self.spend(1)?;
        row.insert(c, cell);
        Ok(())
    }

    fn read(
        &mut self,
        workbook: &mut Sheets<Cursor<&[u8]>>,
        name: &str,
        formulas: bool,
        mark_formulas: bool,
    ) -> Result<Sheet, FxError> {
        let error =
            |e: &dyn std::fmt::Display| FxError::Parse(format!("spreadsheet sheet {name:?}: {e}"));
        let mut sheet = Sheet::default();
        macro_rules! stream {
            ($book:expr) => {{
                let mut reader = $book.worksheet_cells_reader(name).map_err(|e| error(&e))?;
                while let Some(cell) = reader.next_cell().map_err(|e| error(&e))? {
                    let value = Data::from(cell.get_value().clone());
                    if value != Data::Empty {
                        self.add(&mut sheet, cell.get_position(), Owned::Value(value))?;
                    }
                }
                drop(reader);
                if formulas || mark_formulas {
                    let mut reader = $book.worksheet_cells_reader(name).map_err(|e| error(&e))?;
                    while let Some(cell) = reader.next_formula().map_err(|e| error(&e))? {
                        if cell.get_value().is_empty() {
                            continue;
                        }
                        if formulas {
                            let f = Owned::Formula(cell.get_value().clone());
                            self.add(&mut sheet, cell.get_position(), f)?;
                        } else {
                            sheet.mark(cell.get_position());
                        }
                    }
                }
            }};
        }
        match workbook {
            Sheets::Xlsx(book) => stream!(book),
            Sheets::Xlsb(book) => stream!(book),
            _ => {
                let values = workbook.worksheet_range(name).map_err(|e| error(&e))?;
                for (r, c, v) in values.used_cells() {
                    let (r, c) = (
                        r as u32 + values.start().map_or(0, |s| s.0),
                        c as u32 + values.start().map_or(0, |s| s.1),
                    );
                    self.add(&mut sheet, (r, c), Owned::Value(v.clone()))?;
                }
                if formulas || mark_formulas {
                    // A reader that cannot give formulas gives the values.
                    if let Ok(f) = workbook.worksheet_formula(name) {
                        for (r, c, v) in f.used_cells() {
                            if v.is_empty() {
                                continue;
                            }
                            let (r, c) = (
                                r as u32 + f.start().map_or(0, |s| s.0),
                                c as u32 + f.start().map_or(0, |s| s.1),
                            );
                            if formulas {
                                self.add(&mut sheet, (r, c), Owned::Formula(v.clone()))?;
                            } else {
                                sheet.mark((r, c));
                            }
                        }
                    }
                }
            }
        }
        Ok(sheet)
    }
}

/// A row's cells with their columns; `None` for an empty one.
type RowCells<'a> = Vec<(u32, Option<Cell<'a>>)>;

/// A cell's value, or its formula's text.
enum Owned {
    Value(Data),
    Formula(String),
}

/// A sheet's non-empty cells, by row and column (from 0).
#[derive(Default)]
struct Sheet {
    rows: BTreeMap<u32, BTreeMap<u32, Owned>>,
    /// The cells holding a formula whose value was read instead (with
    /// `spreadsheet.composite-values` and `spreadsheet.evaluate-formulas`).
    formula_cells: HashSet<(u32, u32)>,
}

impl Sheet {
    /// Marks a cell with a value as holding a formula.
    fn mark(&mut self, (r, c): (u32, u32)) {
        if self.rows.get(&r).is_some_and(|row| row.contains_key(&c)) {
            self.formula_cells.insert((r, c));
        }
    }

    /// The empty cells the sheet gives beyond its non-empty ones: those between a row's
    /// first and last cell, and a row container for each empty row between used ones. A row
    /// of A1 and XFD1 gives 16,384 cells; they are counted before any is made.
    fn gaps(&self) -> u64 {
        let (Some(&first), Some(&last)) = (self.rows.keys().next(), self.rows.keys().next_back())
        else {
            return 0;
        };
        let mut gaps = u64::from(last - first) + 1 - self.rows.len() as u64;
        for row in self.rows.values() {
            if let (Some((&a, _)), Some((&b, _))) = (row.first_key_value(), row.last_key_value()) {
                gaps += u64::from(b - a) + 1 - row.len() as u64;
            }
        }
        gaps
    }

    /// `header_row`: the row (from 0) naming the columns. With `composite`, the hyperlinks
    /// and comments of the cells, each cell is a container (`spreadsheet.composite-values`).
    fn triplify(
        &self,
        sink: &mut Sink<'_>,
        header_row: Option<u32>,
        ignore_unnamed: bool,
        composite: Option<&HashMap<(u32, u32), Extras>>,
    ) -> Result<(), FxError> {
        let root = sink.root();
        let (Some(&first_row), Some(&last_row)) =
            (self.rows.keys().next(), self.rows.keys().next_back())
        else {
            return Ok(());
        };
        // The key of each column (from 1, counted from column A).
        // `None` for a key the pattern cannot match.
        let mut keys: HashMap<u32, Option<NamedNode>> = HashMap::new();
        if let Some(h) = header_row {
            let mut names: Vec<String> = Vec::new();
            for (col, cell) in self.row_cells(h).into_iter().flatten() {
                let mut name = cell.map(|c| c.text()).unwrap_or_default().trim().to_owned();
                if name.is_empty() {
                    continue;
                }
                let mut c = 0;
                while names.contains(&name) {
                    c += 1;
                    name.push('_');
                    name.push_str(&c.to_string());
                }
                keys.insert(col + 1, sink.predicate(sink.key_iri(&name)));
                names.push(name);
            }
        }
        let rows = format!("{}_Row_", sink.common.root);
        let mut n = 0;
        for r in first_row..=last_row {
            if header_row == Some(r) {
                continue;
            }
            n += 1;
            let row: NamedOrBlankNode = if sink.common.blank_nodes {
                BlankNode::default().into()
            } else {
                NamedNode::new_unchecked(format!("{rows}{n}")).into()
            };
            sink.slot(&root, n, row.clone().into());
            let Some(cells) = self.row_cells(r) else {
                // An empty row between used ones is still a row container.
                continue;
            };
            let first = cells.first().map_or(0, |(col, _)| *col);
            for (col, cell) in cells {
                let id = col - first + 1;
                let value = match (composite, cell) {
                    (Some(extras), cell) => {
                        let container = sink.container(String::new);
                        let container: NamedOrBlankNode = if sink.common.blank_nodes {
                            container
                        } else {
                            NamedNode::new_unchecked(format!("{rows}{n}_{col}")).into()
                        };
                        if let Some(cell) = cell {
                            let formula = self.formula_cells.contains(&(r, col));
                            cell.composite(sink, &container, formula, extras.get(&(r, col)));
                        }
                        Some(Term::from(container))
                    }
                    (None, Some(cell)) => cell.term(sink),
                    (None, None) => sink.string(""),
                };
                let Some(value) = value else { continue };
                match keys.get(&id) {
                    Some(Some(p)) => sink.emit(&row, p, value),
                    Some(None) => {}
                    None if !ignore_unnamed => sink.slot(&row, id as usize, value),
                    None => {}
                }
            }
        }
        Ok(())
    }

    /// The cells of row `r` (absolute, from 0) from its first to its last non-empty cell,
    /// with their columns (from 0); `None` for an empty cell. `None` for an empty row.
    fn row_cells(&self, r: u32) -> Option<RowCells<'_>> {
        let row = self.rows.get(&r)?;
        let (Some((&first, _)), Some((&last, _))) = (row.first_key_value(), row.last_key_value())
        else {
            return None;
        };
        let mut cells = Vec::with_capacity((last - first) as usize + 1);
        let mut next = first;
        for (&c, cell) in row {
            while next < c && next <= last {
                cells.push((next, None));
                next += 1;
            }
            cells.push((c, Some(cell.as_cell())));
            next = c + 1;
        }
        Some(cells)
    }
}

impl Owned {
    fn as_cell(&self) -> Cell<'_> {
        match self {
            Owned::Value(v) => Cell::Value(v),
            Owned::Formula(f) => Cell::Formula(f),
        }
    }
}

enum Cell<'a> {
    Value(&'a Data),
    Formula(&'a str),
}

impl Cell<'_> {
    fn term(&self, sink: &Sink<'_>) -> Option<Term> {
        match self {
            Cell::Formula(f) => sink.string(f),
            Cell::Value(v) => match v {
                Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => sink.string(s),
                Data::Float(f) => sink.typed(&xsd_double(*f), xsd::DOUBLE),
                Data::Int(i) => sink.typed(&java_double(*i as f64), xsd::DOUBLE),
                Data::Bool(b) => sink.typed(if *b { "true" } else { "false" }, xsd::BOOLEAN),
                Data::DateTime(d) if d.is_datetime() => sink.string(&utc_date_time(d)),
                Data::DateTime(d) => sink.typed(&xsd_double(d.as_f64()), xsd::DOUBLE),
                Data::Error(_) | Data::Empty => sink.string(""),
            },
        }
    }

    /// The cell as a container (`spreadsheet.composite-values`), as SPARQL Anything makes
    /// it: typed by its kind (`xyz:STRING`, `xyz:NUMERIC`, `xyz:BOOLEAN`, `xyz:FORMULA`,
    /// `xyz:ERROR`, `xyz:BLANK`), its value as `rdf:_1`, and its hyperlink and comment.
    /// `formula`: the cell holds a formula whose value was read.
    fn composite(
        &self,
        sink: &mut Sink<'_>,
        container: &NamedOrBlankNode,
        formula: bool,
        extras: Option<&Extras>,
    ) {
        let (kind, value) = match self {
            Cell::Formula(f) => ("FORMULA", sink.string(f)),
            Cell::Value(_) if formula => ("FORMULA", self.term(sink)),
            Cell::Value(v) => match v {
                Data::String(s) => ("STRING", sink.string(s)),
                // ods dates and durations, as their text.
                Data::DateTimeIso(s) | Data::DurationIso(s) => ("NUMERIC", sink.string(s)),
                Data::Float(f) => ("NUMERIC", sink.typed(&xsd_double(*f), xsd::DOUBLE)),
                Data::Int(i) => ("NUMERIC", sink.typed(&java_double(*i as f64), xsd::DOUBLE)),
                // A date is a number of days, as Excel holds it.
                Data::DateTime(d) => ("NUMERIC", sink.typed(&xsd_double(d.as_f64()), xsd::DOUBLE)),
                Data::Bool(b) => (
                    "BOOLEAN",
                    sink.typed(if *b { "true" } else { "false" }, xsd::BOOLEAN),
                ),
                Data::Error(_) => ("ERROR", None),
                Data::Empty => ("BLANK", None),
            },
        };
        sink.class(container, sink.key_iri(kind));
        if let Some(value) = value {
            sink.slot(container, 1, value);
        }
        let Some(extras) = extras else { return };
        if extras.link {
            if let Some(address) = extras.address.as_deref().and_then(|a| sink.string(a)) {
                sink.key(container, "address", address);
            }
            let label = extras.label.clone().unwrap_or_else(|| self.text());
            if let Some(label) = sink.string(&label) {
                sink.key(container, "label", label);
            }
        }
        if let Some(comment) = &extras.comment {
            if let Some(author) = extras.author.as_deref().and_then(|a| sink.string(a)) {
                sink.key(container, "author", author);
            }
            if let Some(text) = sink.string(comment) {
                sink.key(container, "threadedComment", text);
            }
        }
    }

    /// The text of a header cell.
    fn text(&self) -> String {
        match self {
            Cell::Formula(f) => (*f).to_owned(),
            Cell::Value(v) => match v {
                Data::String(s) | Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
                Data::Float(f) => java_double(*f),
                Data::Int(i) => java_double(*i as f64),
                Data::Bool(b) => b.to_string(),
                Data::DateTime(d) if d.is_datetime() => utc_date_time(d),
                Data::DateTime(d) => java_double(d.as_f64()),
                Data::Error(_) | Data::Empty => String::new(),
            },
        }
    }
}

/// An Excel date as `xsd:dateTime` text in UTC, with milliseconds only when not zero.
fn utc_date_time(d: &calamine::ExcelDateTime) -> String {
    let (y, mo, day, h, mi, s, ms) = d.to_ymd_hms_milli();
    if ms == 0 {
        format!("{y:04}-{mo:02}-{day:02}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!("{y:04}-{mo:02}-{day:02}T{h:02}:{mi:02}:{s:02}.{ms:03}Z")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_as_excel_shows_them() {
        assert_eq!(number_text(10.0), "10");
        assert_eq!(number_text(-3.0), "-3");
        assert_eq!(number_text(0.5), "0.5");
        assert_eq!(number_text(1e20), "100000000000000000000");
        assert_eq!(value_text(&Data::Int(7)).as_deref(), Some("7"));
        assert_eq!(value_text(&Data::Bool(true)).as_deref(), Some("true"));
        assert_eq!(value_text(&Data::Empty), None);
        let date = Data::DateTime(calamine::ExcelDateTime::new(
            43861.5,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        assert_eq!(value_text(&date).as_deref(), Some("2020-01-31T12:00:00"));
        let time = Data::DateTime(calamine::ExcelDateTime::new(
            0.545_138_888_888_889,
            calamine::ExcelDateTimeType::DateTime,
            false,
        ));
        assert_eq!(value_text(&time).as_deref(), Some("13:05:00"));
    }
}
