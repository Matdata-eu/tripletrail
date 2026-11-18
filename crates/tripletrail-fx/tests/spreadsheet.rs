//! Spreadsheet sources, on the test files of SPARQL Anything's spreadsheet triplifier
//! (Apache-2.0, copied unchanged into `tests/sparql-anything/spreadsheet`, see the
//! `LICENSE` and `README.md` there), with the same expected graphs.

use oxigraph::io::{RdfFormat, RdfParser};
use oxigraph::model::{Dataset, GraphName, Quad, Term};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use std::path::PathBuf;
use tripletrail_fx::{FxConfig, FxService};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/sparql-anything/spreadsheet")
}

/// The quads of `file` with `options`, IRI containers under
/// `http://www.example.org/document`, as SPARQL Anything's tests make them.
fn triplify(file: &str, options: &str) -> Dataset {
    try_triplify(file, options, "").unwrap()
}

/// `triples`: option triples (`fxe:properties fxe:… "…"`) in the SERVICE pattern.
fn try_triplify(file: &str, options: &str, triples: &str) -> Result<Dataset, String> {
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(Dataset::new()));
    let query = format!(
        "PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
        SELECT * WHERE {{ SERVICE <x-sparql-anything:location={file},root=http://www.example.org/document,blank-nodes=false{options}> {{
            {triples}
            {{ ?s ?p ?o }} UNION {{ GRAPH ?g {{ ?s ?p ?o }} }} }} }}"
    );
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .map_err(|e| e.to_string())?
    else {
        panic!("not a SELECT")
    };
    let mut dataset = Dataset::new();
    for s in solutions {
        let s = s.map_err(|e| e.to_string())?;
        let subject = match s.get("s") {
            Some(Term::NamedNode(n)) => n.clone(),
            other => panic!("subject {other:?}"),
        };
        let Some(Term::NamedNode(predicate)) = s.get("p") else {
            panic!("predicate")
        };
        let graph = match s.get("g") {
            Some(Term::NamedNode(g)) => GraphName::NamedNode(g.clone()),
            None => GraphName::DefaultGraph,
            other => panic!("graph {other:?}"),
        };
        dataset.insert(&Quad::new(
            subject,
            predicate.clone(),
            s.get("o").unwrap().clone(),
            graph,
        ));
    }
    Ok(dataset)
}

fn expected(file: &str) -> Dataset {
    let format = if file.ends_with(".trig") {
        RdfFormat::TriG
    } else {
        RdfFormat::NQuads
    };
    let text = std::fs::read(dir().join(file)).unwrap();
    RdfParser::from_format(format)
        .for_slice(&text)
        .collect::<Result<Dataset, _>>()
        .unwrap()
}

fn assert_same(actual: &Dataset, expected: &Dataset) {
    let mut missing: Vec<String> = expected
        .iter()
        .filter(|q| !actual.contains(*q))
        .map(|q| q.to_string())
        .collect();
    let mut extra: Vec<String> = actual
        .iter()
        .filter(|q| !expected.contains(*q))
        .map(|q| q.to_string())
        .collect();
    missing.sort();
    extra.sort();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "missing:\n{}\nextra:\n{}",
        missing.join("\n"),
        extra.join("\n")
    );
}

#[test]
fn two_sheets_with_slots() {
    assert_same(&triplify("1.xls", ""), &expected("1.nq"));
}

#[test]
fn headers_name_the_columns() {
    assert_same(
        &triplify("2.xls", ",spreadsheet.headers=true"),
        &expected("2.nq"),
    );
}

#[test]
fn empty_cells_are_empty_strings() {
    assert_same(
        &triplify("NoNullString.xls", ""),
        &expected("NoNullString.nq"),
    );
}

#[test]
fn null_string_drops_empty_cells() {
    // `null-string=` (empty) cannot be written in a service IRI, so as an option triple.
    let d = try_triplify(
        "NullString.xls",
        "",
        r#"fxe:properties fxe:null-string "" ."#,
    )
    .unwrap();
    assert_same(&d, &expected("NullString.nq"));
}

#[test]
fn formulas_evaluated() {
    assert_same(
        &triplify(
            "FormulaEvaluator.xls",
            ",spreadsheet.evaluate-formulas=true",
        ),
        &expected("FormulaEvaluator.nq"),
    );
}

#[test]
fn formulas_as_text() {
    let d = triplify("FormulaEvaluator.xls", "");
    let texts: Vec<String> = d
        .iter()
        .filter(|q| q.graph_name.is_default_graph())
        .filter_map(|q| match q.object {
            oxigraph::model::TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        })
        .filter(|v| v.contains('+') || v.contains("SUM"))
        .collect();
    assert!(!texts.is_empty(), "no formula text in {d}");
    assert!(texts.iter().all(|t| !t.starts_with('=')), "{texts:?}");
}

#[test]
fn headers_row() {
    assert_same(
        &triplify(
            "HeaderRow.xls",
            ",spreadsheet.headers=true,spreadsheet.headers-row=4",
        ),
        &expected("HeaderRow.trig"),
    );
}

#[test]
fn xlsx() {
    assert_same(&triplify("Book1.xlsx", ""), &expected("Book1.nq"));
}

/// An xlsx with a sheet `S` of text, a number, a boolean, a formula, an error, a hyperlink
/// and a comment, as a spreadsheet program writes them.
fn composite_xlsx() -> Vec<u8> {
    use std::io::Write;
    let sheet = r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetData>
        <row r="1"><c r="A1" t="inlineStr"><is><t>name</t></is></c><c r="B1" t="inlineStr"><is><t>n</t></is></c><c r="C1" t="inlineStr"><is><t>ok</t></is></c><c r="D1" t="inlineStr"><is><t>sum</t></is></c><c r="E1" t="inlineStr"><is><t>bad</t></is></c></row>
        <row r="2"><c r="A2" t="inlineStr"><is><t>TripleTrail</t></is></c><c r="B2"><v>2.5</v></c><c r="C2" t="b"><v>1</v></c><c r="D2"><f>B2*2</f><v>5</v></c><c r="E2" t="e"><v>#DIV/0!</v></c></row>
        <row r="3"><c r="A3" t="inlineStr"><is><t>docs</t></is></c><c r="B3" t="inlineStr"><is><t>here</t></is></c></row>
        </sheetData><hyperlinks><hyperlink ref="A3" r:id="rId1"/><hyperlink ref="B3" location="S!A1" display="Top"/></hyperlinks></worksheet>"#;
    let parts = [
        (
            "[Content_Types].xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/comments1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.comments+xml"/></Types>"#,
        ),
        (
            "_rels/.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#,
        ),
        (
            "xl/workbook.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#,
        ),
        (
            "xl/_rels/workbook.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#,
        ),
        ("xl/worksheets/sheet1.xml", sheet),
        (
            "xl/worksheets/_rels/sheet1.xml.rels",
            r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/hyperlink" Target="https://example.org/docs?a=1&amp;b=2" TargetMode="External"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/comments" Target="../comments1.xml"/></Relationships>"#,
        ),
        (
            "xl/comments1.xml",
            r#"<?xml version="1.0" encoding="UTF-8"?><comments xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><authors><author>Ann</author><author>Bo &amp; Co</author></authors><commentList><comment ref="B2" authorId="1"><text><r><t>Half </t></r><r><t xml:space="preserve">of &lt;5&gt;</t></r></text></comment></commentList></comments>"#,
        ),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, text) in parts {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// The triples of the default graph of an xlsx made of `bytes` as sorted N-Triples lines,
/// IRI containers under `http://e/d`.
fn xlsx_triples(bytes: &[u8], options: &str) -> Vec<String> {
    let dir = tripletrail_testdir::tempdir("fx-sheet-composite");
    let name = format!("c{}.xlsx", options.len());
    std::fs::write(dir.join(&name), bytes).unwrap();
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir.to_path_buf(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(Dataset::new()));
    let query = format!(
        "SELECT * WHERE {{ SERVICE <x-sparql-anything:location={name},root=http://e/d,blank-nodes=false{options}> {{ ?s ?p ?o }} }}"
    );
    let QueryResults::Solutions(solutions) = fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
    else {
        unreachable!()
    };
    let mut lines: Vec<String> = solutions
        .map(|s| {
            let s = s.unwrap();
            format!(
                "{} {} {}",
                s.get("s").unwrap(),
                s.get("p").unwrap(),
                s.get("o").unwrap()
            )
            .replace("http://sparql.xyz/facade-x/data/", "xyz:")
            .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
            .replace("http://www.w3.org/2001/XMLSchema#", "xsd:")
        })
        .collect();
    lines.sort();
    std::fs::remove_file(dir.join(&name)).unwrap();
    lines
}

#[test]
fn composite_values_make_cells_containers() {
    let lines = xlsx_triples(
        &composite_xlsx(),
        ",spreadsheet.headers=true,spreadsheet.composite-values=true",
    );
    let expected = [
        // Row 1 (after the header row) and its cells, named by row and column (from 0).
        "<http://e/dS_Row_1> <xyz:name> <http://e/dS_Row_1_0>",
        "<http://e/dS_Row_1_0> <rdf:type> <xyz:STRING>",
        "<http://e/dS_Row_1_0> <rdf:_1> \"TripleTrail\"",
        "<http://e/dS_Row_1_1> <rdf:type> <xyz:NUMERIC>",
        "<http://e/dS_Row_1_1> <rdf:_1> \"2.5\"^^<xsd:double>",
        "<http://e/dS_Row_1_1> <xyz:author> \"Bo & Co\"",
        "<http://e/dS_Row_1_1> <xyz:threadedComment> \"Half of <5>\"",
        "<http://e/dS_Row_1_2> <rdf:type> <xyz:BOOLEAN>",
        "<http://e/dS_Row_1_2> <rdf:_1> \"true\"^^<xsd:boolean>",
        "<http://e/dS_Row_1_3> <rdf:type> <xyz:FORMULA>",
        "<http://e/dS_Row_1_3> <rdf:_1> \"B2*2\"",
        "<http://e/dS_Row_1_4> <rdf:type> <xyz:ERROR>",
        "<http://e/dS_Row_2_0> <xyz:address> \"https://example.org/docs?a=1&b=2\"",
        "<http://e/dS_Row_2_0> <xyz:label> \"docs\"",
        "<http://e/dS_Row_2_1> <xyz:address> \"S!A1\"",
        "<http://e/dS_Row_2_1> <xyz:label> \"Top\"",
    ];
    for e in expected {
        assert!(
            lines.iter().any(|l| l == e),
            "no {e} in\n{}",
            lines.join("\n")
        );
    }
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("<http://e/dS_Row_1_4> <rdf:_1>")),
        "{lines:?}"
    );

    // With formulas evaluated, a formula cell is still FORMULA, with its value.
    let lines = xlsx_triples(
        &composite_xlsx(),
        ",spreadsheet.composite-values=true,spreadsheet.evaluate-formulas=true",
    );
    for e in [
        "<http://e/dS_Row_2_3> <rdf:type> <xyz:FORMULA>",
        "<http://e/dS_Row_2_3> <rdf:_1> \"5.0\"^^<xsd:double>",
        // Without headers, the row holds its cells in slots.
        "<http://e/dS_Row_2> <rdf:_1> <http://e/dS_Row_2_0>",
    ] {
        assert!(
            lines.iter().any(|l| l == e),
            "no {e} in\n{}",
            lines.join("\n")
        );
    }

    // SPARQL Anything's own workbook, with the option.
    let d = triplify("Book1.xlsx", ",spreadsheet.composite-values=true");
    assert!(!d.is_empty());
}

#[test]
fn hyperlink_ranges_are_not_expanded() {
    let composite = ",spreadsheet.composite-values=true";
    // 1,000 ranges of 100,000 cells each over a one-cell sheet: nothing to link.
    let links: String = (0..1000)
        .map(|i| {
            format!(
                r#"<hyperlink ref="A{}:CV{}" location="S!A1"/>"#,
                1 + 1000 * i,
                1000 + 1000 * i
            )
        })
        .collect();
    let bytes = xlsx_sheet_then(
        r#"<row r="1"><c r="A1"><v>1</v></c></row>"#,
        &format!("<hyperlinks>{links}</hyperlinks>"),
    );
    let start = std::time::Instant::now();
    assert!(lift_xlsx(&bytes, composite).unwrap() > 0);
    assert!(start.elapsed().as_secs() < 5, "{:?}", start.elapsed());

    // Overlapping ranges over the cells the sheet has count against spreadsheet.max-cells.
    let rows: String = (1..=100)
        .map(|r| {
            format!(r#"<row r="{r}"><c r="A{r}"><v>{r}</v></c><c r="B{r}"><v>{r}</v></c></row>"#)
        })
        .collect();
    let links = r#"<hyperlink ref="A1:B100" location="S!A1"/>"#.repeat(100);
    let bytes = xlsx_sheet_then(&rows, &format!("<hyperlinks>{links}</hyperlinks>"));
    let e = lift_xlsx(&bytes, &format!("{composite},spreadsheet.max-cells=10000")).unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    assert!(lift_xlsx(&bytes, &format!("{composite},spreadsheet.max-cells=100000")).unwrap() > 0);
}

/// A one-sheet xlsx whose sheet holds `cells` (`(row from 1, "A1"-style reference, number)`).
fn xlsx_with(cells: &[(u32, &str, u32)]) -> Vec<u8> {
    let rows: String = cells
        .iter()
        .map(|(r, at, v)| format!(r#"<row r="{r}"><c r="{at}"><v>{v}</v></c></row>"#))
        .collect();
    xlsx_sheet(&rows)
}

/// A one-sheet xlsx whose sheetData holds `rows`.
fn xlsx_sheet(rows: &str) -> Vec<u8> {
    xlsx_sheet_then(rows, "")
}

/// A one-sheet xlsx whose sheetData holds `rows`, followed by `after` (hyperlinks).
fn xlsx_sheet_then(rows: &str, after: &str) -> Vec<u8> {
    use std::io::Write;
    let parts = [
        ("[Content_Types].xml", r#"<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/></Types>"#.to_owned()),
        ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_owned()),
        ("xl/workbook.xml", r#"<?xml version="1.0" encoding="UTF-8"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#.to_owned()),
        ("xl/_rels/workbook.xml.rels", r#"<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#.to_owned()),
        ("xl/worksheets/sheet1.xml", format!(r#"<?xml version="1.0" encoding="UTF-8"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>{rows}</sheetData>{after}</worksheet>"#)),
    ];
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, text) in parts {
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// The quads of an xlsx file made of `bytes`, or the error.
fn lift_xlsx(bytes: &[u8], options: &str) -> Result<usize, String> {
    lift_bytes("w.xlsx", bytes, options)
}

/// The quads of a file `name` made of `bytes`, or the error.
fn lift_bytes(name: &str, bytes: &[u8], options: &str) -> Result<usize, String> {
    let dir = tripletrail_testdir::tempdir("fx-sheet-limits");
    std::fs::write(dir.join(name), bytes).unwrap();
    let fx = FxService::new(
        FxConfig {
            fixed_limits: false,
            databases: None,
            base: dir.to_path_buf(),
            allowed: None,
            remote: Default::default(),
        },
        SparqlEvaluator::new,
    );
    let empty = Box::leak(Box::new(Dataset::new()));
    let query = format!(
        "SELECT * WHERE {{ SERVICE <x-sparql-anything:location={name}{options}> {{
            GRAPH ?g {{ ?s ?p ?o }} }} }}"
    );
    let result = match fx
        .register(SparqlEvaluator::new())
        .parse_query(&query)
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
    {
        Ok(QueryResults::Solutions(s)) => s
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.len())
            .map_err(|e| e.to_string()),
        Ok(_) => unreachable!(),
        Err(e) => Err(e.to_string()),
    };
    std::fs::remove_dir_all(&dir).unwrap();
    result
}

/// calamine's ranges hold a sheet's whole bounding box: a 1.5 KB file with cells in A1 and
/// XFD1048576 asked for 17 billion cells and aborted the process. Read cell by cell, it is
/// two cells and a million empty row containers, which a lower limit refuses.
#[test]
fn cells_far_apart_are_refused_not_allocated() {
    let started = std::time::Instant::now();
    let e = lift_xlsx(
        &xlsx_with(&[(1, "A1", 1), (1_048_576, "XFD1048576", 2)]),
        ",spreadsheet.max-cells=100000",
    )
    .unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    assert!(started.elapsed().as_secs() < 10);
    // Far apart in one row: the empty cells between count.
    let e = lift_xlsx(
        &xlsx_with(&[(1, "A1", 1), (1, "XFD1", 2)]),
        ",spreadsheet.max-cells=1000",
    )
    .unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    // Close together they are fine: root, row containers and cells.
    assert!(lift_xlsx(&xlsx_with(&[(1, "A1", 1), (2, "B2", 2)]), "").unwrap() > 4);
    // ... unless the limit is lower.
    let e = lift_xlsx(
        &xlsx_with(&[(1, "A1", 1), (2, "B2", 2)]),
        ",spreadsheet.max-cells=1",
    )
    .unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
}

#[test]
fn workbooks_that_unpack_too_large_are_refused() {
    let bytes = xlsx_with(&[(1, "A1", 1)]);
    let e = lift_xlsx(&bytes, ",spreadsheet.max-bytes=500").unwrap_err();
    assert!(e.contains("spreadsheet.max-bytes"), "{e}");
    assert!(lift_xlsx(&bytes, "").is_ok());
    let e = lift_xlsx(&bytes, ",spreadsheet.max-cells=none").unwrap_err();
    assert!(e.contains("positive number"), "{e}");
}

/// 600 rows with a cell in A and one in XFD: 6 KB, 9.8 million cells with the empty ones
/// between, which took 3 GB in quads. The default limit refuses it before any is made.
#[test]
fn wide_rows_are_refused_by_default() {
    let cells: Vec<(u32, String, u32)> = (1..=600)
        .flat_map(|r| [(r, format!("A{r}"), 1), (r, format!("XFD{r}"), 2)])
        .collect();
    let cells: Vec<(u32, &str, u32)> = cells.iter().map(|(r, a, v)| (*r, a.as_str(), *v)).collect();
    let started = std::time::Instant::now();
    let e = lift_xlsx(&xlsx_with(&cells), "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    assert!(started.elapsed().as_secs() < 10);
}

/// A one-sheet ods file whose content.xml body (inside `table:table`) is `table`.
fn ods_with(table: &str) -> Vec<u8> {
    use std::io::Write;
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" office:version="1.2"><office:body><office:spreadsheet><table:table table:name="S">{table}</table:table></office:spreadsheet></office:body></office:document-content>"#
    );
    let manifest = r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.2"><manifest:file-entry manifest:full-path="/" manifest:media-type="application/vnd.oasis.opendocument.spreadsheet"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/></manifest:manifest>"#;
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let stored =
        zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for (name, text, options) in [
        (
            "mimetype",
            "application/vnd.oasis.opendocument.spreadsheet",
            stored,
        ),
        ("content.xml", content.as_str(), Default::default()),
        ("META-INF/manifest.xml", manifest, Default::default()),
    ] {
        zip.start_file(name, options).unwrap();
        zip.write_all(text.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// A cell of a number, `columns` times.
fn ods_cell(v: u32, columns: u32) -> String {
    format!(
        r#"<table:table-cell table:number-columns-repeated="{columns}" office:value-type="float" office:value="{v}"><text:p>{v}</text:p></table:table-cell>"#
    )
}

/// calamine writes out an ods file's repeated rows and columns when it opens it: a row
/// repeated 3,000 times with a cell repeated 3,000 times, in 750 bytes, asked for 3.3 GB.
/// The repeats are counted from content.xml before calamine reads it.
#[test]
fn repeated_ods_cells_are_refused_before_they_are_read() {
    let started = std::time::Instant::now();
    let bomb = ods_with(&format!(
        r#"<table:table-row table:number-rows-repeated="3000">{}</table:table-row>"#,
        ods_cell(1, 3000)
    ));
    let e = lift_bytes("w.ods", &bomb, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    // Far apart in rows: the box between counts.
    let tall = ods_with(&format!(
        r#"<table:table-row>{a}</table:table-row><table:table-row table:number-rows-repeated="5000"><table:table-cell table:number-columns-repeated="3"/></table:table-row><table:table-row><table:table-cell table:number-columns-repeated="1000"/>{a}</table:table-row>"#,
        a = ods_cell(1, 1)
    ));
    let e = lift_bytes("w.ods", &tall, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
    assert!(started.elapsed().as_secs() < 10);
    assert!(lift_bytes("w.ods", &tall, ",spreadsheet.max-cells=10000000").is_ok());
}

/// LibreOffice ends a sheet with empty rows and columns repeated to the sheet's edge: they
/// are no cells, and a small sheet that ends so is read.
#[test]
fn trailing_empty_ods_repeats_are_free() {
    let ods = ods_with(&format!(
        r#"<table:table-row>{a}{b}<table:table-cell table:number-columns-repeated="16382"/></table:table-row><table:table-row table:number-rows-repeated="3"><table:table-cell table:number-columns-repeated="2"/>{b}</table:table-row><table:table-row table:number-rows-repeated="1048570"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>"#,
        a = ods_cell(1, 1),
        b = ods_cell(2, 1)
    ));
    // The root's type, its 4 rows, 2 cells in the first row and 1 in each other (a row
    // starts at its first non-empty cell).
    let n = lift_bytes("w.ods", &ods, ",spreadsheet.max-cells=20").unwrap();
    assert_eq!(n, 1 + 4 + 2 + 3);
}

/// calamine writes out an ods run of spaces (`<text:s text:c="N"/>`): one cell with
/// 2,000,000,000 spaces in 700 bytes asked for 2 GB. The runs count against
/// `spreadsheet.max-bytes`.
#[test]
fn ods_runs_of_spaces_are_counted() {
    let cell = |n: u64| {
        format!(
            r#"<table:table-row><table:table-cell office:value-type="string"><text:p>a<text:s text:c="{n}"/>b</text:p></table:table-cell></table:table-row>"#
        )
    };
    let e = lift_bytes("w.ods", &ods_with(&cell(2_000_000_000)), "").unwrap_err();
    assert!(e.contains("spreadsheet.max-bytes"), "{e}");
    let many = ods_with(&cell(2_000_000).repeat(600));
    let e = lift_bytes("w.ods", &many, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-bytes"), "{e}");
    assert!(lift_bytes("w.ods", &ods_with(&cell(3)), "").is_ok());
}

/// calamine copies a cell's text for each repeat: a cell of a million spaces, or of a
/// million letters, repeated 900,000 times (under the cell limit) asked for 900 GB in a
/// file of 2 KB. The text of the cells times their repeats counts against
/// `spreadsheet.max-bytes`.
#[test]
fn ods_repeated_cell_text_is_counted() {
    let spaces = r#"<table:table-cell office:value-type="string"><text:p>a<text:s text:c="1000000"/>b</text:p></table:table-cell>"#;
    let letters = format!(
        r#"<table:table-cell office:value-type="string"><text:p>{}</text:p></table:table-cell>"#,
        "x".repeat(1_000_000)
    );
    for (rows, cell) in [
        (
            1,
            spaces.replace(
                "<table:table-cell ",
                r#"<table:table-cell table:number-columns-repeated="900000" "#,
            ),
        ),
        (900_000, spaces.to_owned()),
        (
            1,
            letters.replace(
                "<table:table-cell ",
                r#"<table:table-cell table:number-columns-repeated="900000" "#,
            ),
        ),
    ] {
        let ods = ods_with(&format!(
            r#"<table:table-row table:number-rows-repeated="{rows}">{cell}</table:table-row>"#
        ));
        let e = lift_bytes("w.ods", &ods, "").unwrap_err();
        assert!(e.contains("spreadsheet.max-bytes"), "{e}");
    }
    // Empty cells repeated to the sheet's edge hold no text.
    let edge = ods_with(&format!(
        r#"<table:table-row>{}<table:table-cell table:number-columns-repeated="16383"></table:table-cell></table:table-row><table:table-row table:number-rows-repeated="1048575"><table:table-cell table:number-columns-repeated="16384"/></table:table-row>"#,
        ods_cell(1, 1)
    ));
    assert!(lift_bytes("w.ods", &edge, "").is_ok());
}

/// SPARQL Anything's 1.xls with `patch` applied to its bytes.
fn patched_xls(patch: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut bytes = std::fs::read(dir().join("1.xls")).unwrap();
    patch(&mut bytes);
    bytes
}

/// The offset of the first record of type `typ` and length `len` in `bytes`.
fn record(bytes: &[u8], typ: u16, len: u16) -> usize {
    let mut head = typ.to_le_bytes().to_vec();
    head.extend(len.to_le_bytes());
    bytes.windows(4).position(|w| w == head).unwrap()
}

/// calamine reads an .xls file whole when it opens it, and trusts it: it reserves the cells
/// a sheet's DIMENSIONS record names, makes a range around the cells, and follows sector
/// chains without a limit. Each of these is refused before calamine reads the file.
#[test]
fn xls_files_are_bounded_before_they_are_read() {
    let started = std::time::Instant::now();
    assert!(lift_bytes("w.xls", &patched_xls(|_| {}), "").is_ok());

    // DIMENSIONS of four billion rows.
    let huge = patched_xls(|b| {
        let at = record(b, 0x0200, 14) + 4;
        b[at + 4..at + 8].copy_from_slice(&0xFFFF_FFF0u32.to_le_bytes());
    });
    let e = lift_bytes("w.xls", &huge, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");

    // A first row past the last, which calamine's count wraps around.
    let wrapped = patched_xls(|b| {
        let at = record(b, 0x0200, 14) + 4;
        b[at..at + 4].copy_from_slice(&5u32.to_le_bytes());
        b[at + 4..at + 8].copy_from_slice(&2u32.to_le_bytes());
    });
    let e = lift_bytes("w.xls", &wrapped, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");

    // A cell in the sheet's far corner: a range of 65,536 x 256 cells.
    let far = patched_xls(|b| {
        let at = record(b, 0x00FD, 10) + 4;
        b[at..at + 2].copy_from_slice(&0xFFFFu16.to_le_bytes());
        b[at + 2..at + 4].copy_from_slice(&0xFFu16.to_le_bytes());
    });
    let e = lift_bytes("w.xls", &far, "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");

    // A directory chain that loops back to itself.
    let looped = patched_xls(|b| {
        let dir = u32::from_le_bytes(b[0x30..0x34].try_into().unwrap()) as usize;
        let fat = u32::from_le_bytes(b[0x4C..0x50].try_into().unwrap()) as usize;
        let at = (fat + 1) * 512 + 4 * dir;
        b[at..at + 4].copy_from_slice(&(dir as u32).to_le_bytes());
    });
    let e = lift_bytes("w.xls", &looped, "").unwrap_err();
    assert!(e.contains("loops"), "{e}");

    // Two sheets naming the same bytes, which calamine would read twice.
    let twice = patched_xls(|b| {
        let at = record(b, 0x0085, 0x0E);
        let sheet = b[at..at + 4 + 0x0E].to_vec();
        // Overwrite the record after it with a copy, keeping the stream's length.
        let next = at + 4 + 0x0E;
        let len = u16::from_le_bytes([b[next + 2], b[next + 3]]) as usize;
        assert!(len + 4 >= sheet.len());
        b[next..next + sheet.len()].copy_from_slice(&sheet);
        let rest = (len + 4 - sheet.len()) as u16;
        if rest >= 4 {
            let pad = next + sheet.len();
            b[pad..pad + 2].copy_from_slice(&0x0000u16.to_le_bytes());
            b[pad + 2..pad + 4].copy_from_slice(&(rest - 4).to_le_bytes());
        }
    });
    let e = lift_bytes("w.xls", &twice, "").unwrap_err();
    assert!(e.contains("overlap"), "{e}");
    assert!(started.elapsed().as_secs() < 10);
}

/// A BIFF record.
fn biff(typ: u16, data: &[u8]) -> Vec<u8> {
    let mut r = typ.to_le_bytes().to_vec();
    r.extend((data.len() as u16).to_le_bytes());
    r.extend(data);
    r
}

/// A one-sheet .xls file, a compound file built from scratch, whose sheet holds `records`.
fn xls_with(records: &[u8]) -> Vec<u8> {
    xls_book(&[], records)
}

/// A one-sheet .xls file with `globals` in its workbook globals and `records` in its sheet.
fn xls_book(globals: &[u8], records: &[u8]) -> Vec<u8> {
    let bof = |kind: u16| {
        let mut d = vec![0x00, 0x06];
        d.extend(kind.to_le_bytes());
        d.extend([0u8; 12]);
        biff(0x0809, &d)
    };
    let sheet = |pos: u32| {
        let mut d = pos.to_le_bytes().to_vec();
        d.extend([0, 0, 1, 0, b'S']);
        biff(0x0085, &d)
    };
    let start = bof(5).len() + globals.len() + sheet(0).len() + 4;
    let mut stream = [
        bof(5),
        globals.to_vec(),
        sheet(start as u32),
        biff(0x000A, &[]),
        bof(0x10),
    ]
    .concat();
    stream.extend(records);
    stream.extend(biff(0x000A, &[]));
    let len = stream.len().max(4096);
    let sectors = len.div_ceil(512);
    stream.resize(sectors * 512, 0);
    // Sectors: the FAT, the directory, the stream.
    let mut fats = 1;
    while fats + 1 + sectors > fats * 128 {
        fats += 1;
    }
    let mut fat: Vec<u32> = vec![0xFFFF_FFFD; fats];
    fat.push(0xFFFF_FFFE);
    fat.extend((0..sectors as u32).map(|i| fats as u32 + 2 + i));
    *fat.last_mut().unwrap() = 0xFFFF_FFFE;
    fat.resize(fats * 128, 0xFFFF_FFFF);
    let entry = |name: &str, kind: u8, start: u32, size: u64, child: u32| {
        let mut e: Vec<u8> = name
            .encode_utf16()
            .chain([0])
            .flat_map(u16::to_le_bytes)
            .collect();
        let n = e.len() as u16;
        e.resize(64, 0);
        e.extend(n.to_le_bytes());
        e.extend([kind, 1]);
        e.extend(u32::MAX.to_le_bytes());
        e.extend(u32::MAX.to_le_bytes());
        e.extend(child.to_le_bytes());
        e.resize(116, 0);
        e.extend(start.to_le_bytes());
        e.extend(size.to_le_bytes());
        e
    };
    let mut dir = entry("Root Entry", 5, 0xFFFF_FFFE, 0, 1);
    dir.extend(entry("Workbook", 2, fats as u32 + 1, len as u64, u32::MAX));
    dir.resize(512, 0);
    let mut header = b"\xD0\xCF\x11\xE0\xA1\xB1\x1A\xE1".to_vec();
    header.resize(24, 0);
    for v in [0x3Eu16, 3, 0xFFFE, 9, 6] {
        header.extend(v.to_le_bytes());
    }
    header.resize(44, 0);
    for v in [
        fats as u32,
        fats as u32,
        0,
        4096,
        0xFFFF_FFFE,
        0,
        0xFFFF_FFFE,
        0,
    ] {
        header.extend(v.to_le_bytes());
    }
    for i in 0..109u32 {
        header.extend(if (i as usize) < fats { i } else { u32::MAX }.to_le_bytes());
    }
    assert_eq!(header.len(), 512);
    let fat: Vec<u8> = fat.iter().flat_map(|v| v.to_le_bytes()).collect();
    [header, fat, dir, stream].concat()
}

/// A Number cell.
fn xls_number(row: u16, col: u16, v: f64) -> Vec<u8> {
    let mut d = row.to_le_bytes().to_vec();
    d.extend(col.to_le_bytes());
    d.extend([0, 0]);
    d.extend(v.to_le_bytes());
    biff(0x0203, &d)
}

/// A MulRk record of `n` cells from column `first`, naming `last` as its last column.
fn xls_mulrk(row: u16, first: u16, last: u16, n: usize) -> Vec<u8> {
    let mut d = row.to_le_bytes().to_vec();
    d.extend(first.to_le_bytes());
    for _ in 0..n {
        d.extend([0, 0, 2, 0, 0, 0]);
    }
    d.extend(last.to_le_bytes());
    biff(0x00BD, &d)
}

/// calamine counts a MulRk run as a u16 `last - first + 1`, which wraps when the last
/// column is below the first, and numbers its cells past column 65535: with cells in rows
/// 0 and 65535, a 65 KB file asked for 22 GB. The run is counted as calamine reads it.
#[test]
fn xls_runs_are_counted_as_calamine_reads_them() {
    let ok = [
        xls_number(0, 0, 1.5),
        xls_number(1, 1, 3.5),
        xls_mulrk(2, 0, 2, 3),
    ]
    .concat();
    assert!(lift_bytes("w.xls", &xls_with(&ok), "").is_ok());
    let wrapped = [
        xls_number(0, 60000, 1.0),
        xls_mulrk(1, 60000, 5000, 10537),
        xls_number(65535, 60000, 2.0),
    ]
    .concat();
    let e = lift_bytes("w.xls", &xls_with(&wrapped), "").unwrap_err();
    assert!(e.contains("spreadsheet.max-cells"), "{e}");
}

/// A shared string table of one string of `n` letters, with its first part in the SST
/// record and the rest in Continue records, as Excel splits long strings.
fn xls_sst(n: usize) -> Vec<u8> {
    let mut first = 1u32.to_le_bytes().to_vec();
    first.extend(1u32.to_le_bytes());
    first.extend((n as u16).to_le_bytes());
    first.push(0);
    let text = vec![b'x'; n];
    let (head, tail) = text.split_at(n.min(8000));
    first.extend(head);
    let mut out = biff(0x00FC, &first);
    for part in tail.chunks(8000) {
        let mut c = vec![0u8];
        c.extend(part);
        out.extend(biff(0x003C, &c));
    }
    out
}

/// A LabelSst cell naming shared string `i`.
fn xls_label_sst(row: u16, col: u16, i: u32) -> Vec<u8> {
    let mut d = row.to_le_bytes().to_vec();
    d.extend(col.to_le_bytes());
    d.extend([0, 0]);
    d.extend(i.to_le_bytes());
    biff(0x00FD, &d)
}

/// calamine copies a shared string into every cell that names it: 400,000 cells naming one
/// string of 60,000 letters asked for 24 GB. The copies count against
/// `spreadsheet.max-bytes`.
#[test]
fn xls_shared_strings_count_per_cell() {
    let cells = |rows: u16, cols: u16| {
        (0..rows)
            .flat_map(|r| (0..cols).map(move |c| xls_label_sst(r, c, 0)))
            .flatten()
            .collect::<Vec<u8>>()
    };
    let small = xls_book(&xls_sst(5), &cells(2, 3));
    assert_eq!(lift_bytes("w.xls", &small, "").unwrap(), 1 + 2 + 6);
    // 60,000 letters in 2,000 cells: 120 MB of copies (counted as up to 3 bytes a
    // letter), over a 100 MB limit.
    let big = xls_book(&xls_sst(60_000), &cells(40, 50));
    let e = lift_bytes("w.xls", &big, ",spreadsheet.max-bytes=100000000").unwrap_err();
    assert!(e.contains("spreadsheet.max-bytes"), "{e}");
    assert!(lift_bytes("w.xls", &big, "").is_ok());
}

/// For RML: a row of only error cells is left out, so it does not become the header.
#[test]
fn sheet_cells_leave_out_rows_of_errors() {
    let book = xlsx_sheet(
        r#"<row r="1"><c r="C1" t="e"><v>#DIV/0!</v></c></row><row r="2"><c r="A2" t="inlineStr"><is><t>id</t></is></c></row><row r="3"><c r="A3"><v>7</v></c></row>"#,
    );
    let cells = tripletrail_fx::sources::sheet_cells(&book, None).unwrap();
    let rows: Vec<_> = cells.into_iter().collect();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].1[&0], "id");
    assert_eq!(rows[1].1[&0], "7");
}
