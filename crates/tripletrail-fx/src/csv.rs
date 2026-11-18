//! CSV: the root holds one container per data row (`rdf:_1`, `rdf:_2`, ...). A cell is a
//! key named by its column header with `csv.headers=true`, else a slot by column number
//! (`rdf:_1` for the first column). Cells are `xsd:string`; empty cells are `""` unless
//! `csv.null-string` (or `null-string`) is set to the empty string. With
//! `blank-nodes=false` the rows are `<root>#row1`, `<root>#row2`, ... (`<root>row1` when the
//! root ends with `/` or `#`). The options are read by [`CsvOptions`]. With `geo.wkt` or
//! `geo.x`/`geo.y`, a row also gets `geo:asWKT` (see [`crate::geo`]).

use crate::FxError;
use crate::RDF;
use crate::geo;
use crate::options::Options;
use crate::sources::{CsvOptions, CsvRows};
use crate::tree::Sink;
use oxigraph::model::{BlankNode, Literal, NamedNode, NamedOrBlankNode};

pub(crate) fn triplify(
    bytes: &[u8],
    options: &Options,
    sink: &mut Sink<'_>,
    tab: bool,
    mut each: Option<crate::EachSlice<'_>>,
) -> Result<(), FxError> {
    let csv = CsvOptions::new(options, tab)?;
    let ignore_unnamed = options.flag("csv.ignore-columns-with-no-header", false)?;
    let mut rows = CsvRows::new(bytes, &csv)?;

    let root = sink.root();
    // With `each` (`slice=true`), the triples of every `slice.rows` rows go to it with these,
    // then are dropped.
    let header = sink.quads.len();
    let slice_rows = slice_rows(options)?;
    let mut in_slice = 0;
    let row_prefix = {
        let r = &sink.common.root;
        if r.ends_with('/') || r.ends_with('#') {
            format!("{r}row")
        } else {
            format!("{r}#row")
        }
    };
    // The predicate of each column: a key when the column has a header, else a slot (unless
    // ignored); `None` when the pattern cannot match it.
    let column = |sink: &Sink<'_>, i: usize, name: Option<&String>| match name {
        Some(name) => sink.predicate(sink.key_iri(name)),
        None if ignore_unnamed => None,
        None => sink.predicate(format!("{RDF}_{}", i + 1)),
    };
    let mut columns: Vec<Option<NamedNode>> = rows
        .headers
        .iter()
        .enumerate()
        .map(|(i, name)| column(sink, i, name.as_ref()))
        .collect();
    // `geo.wkt` or `geo.x`/`geo.y`: the row's geometry.
    let geometry = match geo::Columns::from_options(options)? {
        Some(columns) => sink
            .predicate(geo::AS_WKT.to_owned())
            .map(|p| Ok::<_, FxError>((p, columns.resolve(&rows.headers)?, columns.crs)))
            .transpose()?,
        None => None,
    };
    let wkt_literal = NamedNode::new_unchecked(geo::WKT_LITERAL);
    let mut n = 0;
    while let Some(row) = rows.next_row()? {
        n += 1;
        let c: NamedOrBlankNode = if sink.common.blank_nodes {
            BlankNode::default().into()
        } else {
            NamedNode::new_unchecked(format!("{row_prefix}{n}")).into()
        };
        sink.slot(&root, n, c.clone().into());
        for (i, cell) in row.cells().enumerate() {
            if i == columns.len() {
                columns.push(column(sink, i, None));
            }
            let (Some(p), Some(cell)) = (&columns[i], cell) else {
                continue;
            };
            if let Some(o) = sink.string(cell) {
                sink.emit(&c, p, o);
            }
        }
        if let Some((p, positions, crs)) = &geometry {
            let cells: Vec<Option<&str>> = row.cells().collect();
            if let Some(wkt) = positions.wkt(crs, |i| cells.get(i).copied().flatten()) {
                let o = Literal::new_typed_literal(wkt, wkt_literal.clone());
                sink.emit(&c, p, o.into());
            }
        }
        if let Some(each) = each.as_mut() {
            in_slice += 1;
            if in_slice == slice_rows {
                each(&sink.quads)?;
                sink.quads.truncate(header);
                in_slice = 0;
            }
        }
    }
    if let Some(each) = each.as_mut()
        && in_slice > 0
    {
        each(&sink.quads)?;
        sink.quads.truncate(header);
    }
    Ok(())
}

/// The rows in one slice with `slice=true`: `slice.rows`, by default 10,000.
fn slice_rows(options: &Options) -> Result<usize, FxError> {
    match options.get("slice.rows") {
        None => Ok(10_000),
        Some(n) => {
            n.parse().ok().filter(|&n| n > 0).ok_or_else(|| {
                FxError::Option(format!("slice.rows {n:?} is not a positive number"))
            })
        }
    }
}

/// Whether a source is tab-separated by default: the media type
/// `text/tab-separated-values`, or else a `.tsv` or `.tab` location.
pub(crate) fn is_tab(options: &Options, location: Option<&str>) -> bool {
    if let Some(media) = options.get("media-type") {
        return media
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("text/tab-separated-values");
    }
    let Some(location) = location else {
        return false;
    };
    let path = location.split(['?', '#']).next().unwrap_or_default();
    let name = path.rsplit(['/', '\\']).next().unwrap_or_default();
    name.rsplit_once('.')
        .is_some_and(|(_, e)| e.eq_ignore_ascii_case("tsv") || e.eq_ignore_ascii_case("tab"))
}
