//! CSV sources, on the examples of the CSV format documentation and test files of SPARQL
//! Anything (Apache-2.0), with the same expected answers.

use oxigraph::model::{Dataset, Graph};
use oxigraph::sparql::{QueryResults, SparqlEvaluator};
use tripletrail_fx::{FxConfig, FxService};

const PREFIXES: &str = "PREFIX xyz: <http://sparql.xyz/facade-x/data/>
PREFIX fx: <http://sparql.xyz/facade-x/ns/>
PREFIX fxe: <http://sparql.xyz/facade-x/engine/>
PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#>
PREFIX xsd: <http://www.w3.org/2001/XMLSchema#>
PREFIX eg: <http://www.example.org/>
";

const SIMPLE: &str = "email,name,surname\\nlaura@example.com,Laura,Grey\\ncraig@example.com,Craig,Johnson\\nmary@example.com,Mary,Jenkins\\njamie@example.com,Jamie,Smith\\n";

const IRIS: &str = "Sepal_length\\tSepal_width\\tPetal_length\\tPetal_width\\tSpecies\\n5.1\\t3.5\\t1.4\\t0.2\\tI. setosa\\n4.9\\t3.0\\t1.4\\t0.2\\tI. setosa\\n4.7\\t3.2\\t1.3\\t0.2\\tI. setosa\\n4.6\\t3.1\\t1.5\\t0.2\\tI. setosa\\n5.0\\t3.6\\t1.4\\t0.2\\tI. setosa\\n";

fn fx(config: FxConfig) -> FxService {
    FxService::new(config, SparqlEvaluator::new)
}

fn run(fx: &FxService, query: &str) -> QueryResults<'static> {
    let empty = Box::leak(Box::new(Dataset::new()));
    fx.register(SparqlEvaluator::new())
        .parse_query(&format!("{PREFIXES}{query}"))
        .unwrap()
        .on_queryable_dataset(&*empty)
        .execute()
        .unwrap()
}

fn rows(query: &str) -> Vec<String> {
    let QueryResults::Solutions(solutions) = run(&fx(FxConfig::default()), query) else {
        panic!("not a SELECT")
    };
    solutions
        .map(|s| {
            let s = s.unwrap();
            s.iter()
                .map(|(v, t)| format!("?{}={t}", v.as_str()))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

fn graph_with(fx: &FxService, query: &str) -> Vec<String> {
    let QueryResults::Graph(triples) = run(fx, query) else {
        panic!("not a CONSTRUCT")
    };
    let g: Graph = triples.map(Result::unwrap).collect();
    let mut lines: Vec<String> = g.iter().map(|t| t.to_string()).collect();
    lines.sort();
    lines
}

fn graph(query: &str) -> Vec<String> {
    graph_with(&fx(FxConfig::default()), query)
}

/// The triples of `content` read with the option triples `options`, with IRI containers
/// under `http://www.example.org/document`, as N-Triples without the namespaces.
fn triplify(content: &str, options: &str) -> Vec<String> {
    graph(&format!(
        r#"CONSTRUCT {{ ?s ?p ?o }} WHERE {{ SERVICE <x-sparql-anything:> {{
            fxe:properties fxe:content "{content}" ; fxe:media-type "text/csv" ;
              fxe:root "http://www.example.org/document" ; fxe:blank-nodes "false" {options} .
            ?s ?p ?o }} }}"#
    ))
    .into_iter()
    .map(|t| {
        t.replace("http://www.example.org/", "eg:")
            .replace("http://sparql.xyz/facade-x/data/", "xyz:")
            .replace("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "rdf:")
            .replace("http://sparql.xyz/facade-x/ns/", "fx:")
    })
    .collect()
}

#[test]
fn default_transformation_has_slots_per_row_and_column() {
    let g = triplify(SIMPLE, "");
    assert_eq!(g.len(), 1 + 5 + 15, "{g:#?}");
    for t in [
        "<eg:document> <rdf:type> <fx:Root>",
        "<eg:document> <rdf:_1> <eg:document#row1>",
        "<eg:document> <rdf:_5> <eg:document#row5>",
        "<eg:document#row1> <rdf:_1> \"email\"",
        "<eg:document#row5> <rdf:_3> \"Smith\"",
    ] {
        assert!(g.contains(&t.to_owned()), "{t} not in {g:#?}");
    }
}

#[test]
fn headers_name_the_keys() {
    let g = triplify(SIMPLE, "; fxe:csv.headers true");
    assert_eq!(g.len(), 1 + 4 + 12, "{g:#?}");
    assert!(g.contains(&"<eg:document#row1> <xyz:email> \"laura@example.com\"".to_owned()));
    assert!(g.contains(&"<eg:document#row4> <xyz:surname> \"Smith\"".to_owned()));
}

#[test]
fn tdf_format_average() {
    let r = rows(&format!(
        r#"SELECT (AVG(xsd:float(?petalLength)) AS ?avgPetalLength) WHERE {{
          SERVICE <x-sparql-anything:csv.headers=true,csv.format=TDF,media-type=text/csv> {{
            fxe:properties fxe:content "{IRIS}" .
            ?s xyz:Sepal_length ?length ; xyz:Petal_length ?petalLength
            FILTER(xsd:float(?length) > 4.9) }} }}"#
    ));
    assert_eq!(
        r,
        vec!["?avgPetalLength=\"1.4\"^^<http://www.w3.org/2001/XMLSchema#float>"]
    );
}

#[test]
fn headers_row_three_names_no_sepal_length() {
    let r = rows(&format!(
        r#"SELECT (AVG(xsd:float(?petalLength)) AS ?avgPetalLength) WHERE {{
          SERVICE <x-sparql-anything:csv.headers=true,csv.format=TDF,csv.headers-row=3,media-type=text/csv> {{
            fxe:properties fxe:content "{IRIS}" .
            ?s xyz:Sepal_length ?length ; xyz:Petal_length ?petalLength
            FILTER(xsd:float(?length) > 4.9) }} }}"#
    ));
    assert_eq!(
        r,
        vec!["?avgPetalLength=\"0\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
}

#[test]
fn delimiter_option() {
    let r = rows(&format!(
        r#"SELECT (MAX(xsd:float(?petalLength)) AS ?maxPetalLength) WHERE {{
          SERVICE <x-sparql-anything:csv.headers=true> {{
            fxe:properties fxe:content "{IRIS}" ; fxe:media-type "text/csv" ; fxe:csv.delimiter "\t" .
            ?s xyz:Sepal_length ?length ; xyz:Petal_length ?petalLength
            FILTER(xsd:float(?length) < 4.9) }} }}"#
    ));
    assert_eq!(
        r,
        vec!["?maxPetalLength=\"1.5\"^^<http://www.w3.org/2001/XMLSchema#float>"]
    );
}

#[test]
fn tab_separated_media_type_defaults_to_tab() {
    let g = triplify(
        IRIS,
        "; fxe:csv.headers true ; fxe:media-type \"text/tab-separated-values\"",
    );
    assert!(
        g.contains(&"<eg:document#row5> <xyz:Species> \"I. setosa\"".to_owned()),
        "{g:#?}"
    );
}

#[test]
fn quote_char() {
    let g = triplify(
        "email,name,surname\\nlaura@example.com,'Laura, Nancy',Grey\\ncraig@example.com,Craig,Johnson\\n",
        "; fxe:csv.headers true ; fxe:csv.quote-char \"'\"",
    );
    assert!(
        g.contains(&"<eg:document#row1> <xyz:name> \"Laura, Nancy\"".to_owned()),
        "{g:#?}"
    );
    assert!(g.contains(&"<eg:document#row1> <xyz:surname> \"Grey\"".to_owned()));
}

#[test]
fn no_quoting() {
    // NoQuote.csv of the SPARQL Anything tests, with quote-char false and with "".
    for quote in ["false", ""] {
        let g = triplify(
            "id\\tname\\n1\\t\\\"Kostas Tsianos\\\" Theatre\\n2\\tPlain Name",
            &format!(
                "; fxe:csv.headers true ; fxe:csv.delimiter \"\\t\" ; fxe:csv.quote-char \"{quote}\""
            ),
        );
        assert!(
            g.contains(&r#"<eg:document#row1> <xyz:name> "\"Kostas Tsianos\" Theatre""#.to_owned()),
            "{g:#?}"
        );
    }
}

#[test]
fn null_string() {
    let r = rows(
        r#"SELECT ?name ?surname WHERE {
          SERVICE <x-sparql-anything:csv.headers=true> {
            fxe:properties fxe:content "email,name,surname\nlaura@example.com,Laura,Grey\n,Mary,Jenkins\n" ;
              fxe:media-type "text/csv" ; fxe:csv.null-string "" .
            ?c xyz:name ?name ; xyz:surname ?surname
            FILTER NOT EXISTS { ?c xyz:email ?email } } }"#,
    );
    assert_eq!(r, vec!["?name=\"Mary\" ?surname=\"Jenkins\""]);
    // Without it, empty cells are empty strings.
    let g = triplify(",Mary,Jenkins", "");
    assert!(
        g.contains(&"<eg:document#row1> <rdf:_1> \"\"".to_owned()),
        "{g:#?}"
    );
    // test3.csv: no triple has the null string as its object.
    for null in ["", "N/A", " "] {
        let g = triplify(
            "name,friend,age\\n\\\"fred\\\",\\\"joe\\\",10\\n\\\"sally\\\",\\\"jim\\\",\\n\\\"will\\\",\\\"bill\\\",\\\"\\\"\\n\\\"joe\\\",\\\"N/A\\\",\\\"\\\"\\n",
            &format!("; fxe:csv.null-string \"{null}\""),
        );
        assert!(
            !g.iter().any(|t| t.ends_with(&format!(" \"{null}\" ."))),
            "{null:?}: {g:#?}"
        );
    }
}

#[test]
fn ignore_columns_with_no_header() {
    let r = rows(
        r#"SELECT DISTINCT ?fred ?sally WHERE { SERVICE <x-sparql-anything:> {
            fxe:properties fxe:csv.headers true ; fxe:content ",state\nfred,CO\nsally,FL" ;
              fxe:media-type "text/csv" ; fxe:csv.ignore-columns-with-no-header true .
            ?root a fx:Root ; rdf:_1 [ rdf:_1 ?fred ] ; rdf:_2 [ rdf:_1 ?sally ] } }"#,
    );
    assert!(r.is_empty(), "{r:?}");
    // Without it, the column without a header is a slot.
    let g = triplify(",state\\nfred,CO", "; fxe:csv.headers true");
    assert!(
        g.contains(&"<eg:document#row1> <rdf:_1> \"fred\"".to_owned()),
        "{g:#?}"
    );
    assert!(g.contains(&"<eg:document#row1> <xyz:state> \"CO\"".to_owned()));
}

#[test]
fn headers_row_in_the_middle() {
    // HeadersRow.csv: the rows before the header row are data too.
    let g = triplify(
        r#"1997,Ford,E350,\"ac, abs, moon\",3000.00\n1999,Chevy,\"Venture \"\"Extended Edition\"\"\",\"\",4900.00\n1999,Chevy,\"Venture \"\"Extended Edition, Very Large\"\"\",,5000.00\nYear,Make,Model,Description,Price\n1996,Jeep,Grand Cherokee,\"MUST SELL!\nair, moon roof, loaded\",4799.00"#,
        "; fxe:csv.headers true ; fxe:csv.headers-row 4",
    );
    assert_eq!(g.len(), 1 + 4 + 20, "{g:#?}");
    for t in [
        r#"<eg:document#row1> <xyz:Description> "ac, abs, moon""#,
        r#"<eg:document#row2> <xyz:Model> "Venture \"Extended Edition\"""#,
        r#"<eg:document#row3> <xyz:Description> """#,
        r#"<eg:document#row4> <xyz:Description> "MUST SELL!\nair, moon roof, loaded""#,
        r#"<eg:document#row4> <xyz:Year> "1996""#,
    ] {
        assert!(g.contains(&t.to_owned()), "{t} not in {g:#?}");
    }
}

#[test]
fn sanitized_headers() {
    // HeadersSanitize.csv with "-".
    let g = triplify(
        r#"Production Year,Make@en,Model:,Description,Purchase $ Price,meta:kritikalität\n1997,Ford,E350,\"ac, abs, moon\",3000.00,y"#,
        "; fxe:csv.headers true ; <http://sparql.xyz/facade-x/engine/csv.headers.sanitize> \"-\"",
    );
    for key in [
        "Production-Year",
        "Make-en",
        "Model",
        "Description",
        "Purchase-Price",
        "meta-kritikalität",
    ] {
        assert!(
            g.iter().any(|t| t.contains(&format!("<xyz:{key}>"))),
            "{key} not in {g:#?}"
        );
    }
    let r = rows(&format!(
        r#"SELECT (AVG(xsd:float(?petalLength)) AS ?avgPetalLength) WHERE {{
          SERVICE <x-sparql-anything:csv.headers=true,csv.headers.sanitize=true,csv.format=TDF,media-type=text/csv> {{
            fxe:properties fxe:content "{IRIS}" .
            ?s xyz:Sepal_length ?length ; xyz:Petal_length ?petalLength
            FILTER(xsd:float(?length) > 4.9) }} }}"#
    ));
    assert_eq!(
        r,
        vec!["?avgPetalLength=\"1.4\"^^<http://www.w3.org/2001/XMLSchema#float>"]
    );
}

#[test]
fn repeated_headers_are_numbered() {
    let g = triplify("a,a,b\\n1,2,3", "; fxe:csv.headers true");
    for t in [
        "<eg:document#row1> <xyz:a> \"1\"",
        "<eg:document#row1> <xyz:a_1> \"2\"",
        "<eg:document#row1> <xyz:b> \"3\"",
    ] {
        assert!(g.contains(&t.to_owned()), "{t} not in {g:#?}");
    }
}

#[test]
fn files_by_extension() {
    let dir = scratch("csv-files");
    std::fs::write(dir.join("a.csv"), "id,name\n1,x\n").unwrap();
    std::fs::write(dir.join("b.tsv"), "id\tname\n2\ty\n").unwrap();
    let fx = fx(FxConfig {
        fixed_limits: false,
        databases: None,
        base: dir.to_path_buf(),
        allowed: Some(vec![dir.to_path_buf()]),
        remote: Default::default(),
    });
    let QueryResults::Solutions(s) = run(
        &fx,
        r#"SELECT ?id ?name WHERE {
            { SERVICE <x-sparql-anything:location=a.csv,csv.headers=true> { ?r xyz:id ?id ; xyz:name ?name } }
            UNION
            { SERVICE <x-sparql-anything:location=b.tsv,csv.headers=true> { ?r xyz:id ?id ; xyz:name ?name } }
        } ORDER BY ?id"#,
    ) else {
        panic!()
    };
    let r: Vec<String> = s
        .map(|s| {
            let s = s.unwrap();
            format!("{} {}", s.get("id").unwrap(), s.get("name").unwrap())
        })
        .collect();
    assert_eq!(r, vec!["\"1\" \"x\"", "\"2\" \"y\""]);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn bad_options_fail() {
    let fx = fx(FxConfig::default());
    for options in [
        "fxe:csv.delimiter \";;\"",
        "fxe:csv.headers-row \"0\"",
        "fxe:csv.format \"Bogus\"",
    ] {
        let result = fx
            .register(SparqlEvaluator::new())
            .parse_query(&format!(
                "{PREFIXES}SELECT * WHERE {{ SERVICE <x-sparql-anything:> {{
                    fxe:properties fxe:content \"a\" ; fxe:media-type \"text/csv\" ; {options} .
                    ?s ?p ?o }} }}"
            ))
            .unwrap()
            .on_queryable_dataset(&Dataset::new())
            .execute()
            .and_then(|r| match r {
                QueryResults::Solutions(s) => s.collect::<Result<Vec<_>, _>>().map(|_| ()),
                _ => Ok(()),
            });
        assert!(result.is_err(), "{options} was accepted");
    }
}

fn scratch(name: &str) -> tripletrail_testdir::TestDir {
    tripletrail_testdir::tempdir(&format!("fx-{name}"))
}

#[test]
fn blank_lines_are_skipped() {
    let g = triplify("a\\n\\nb\\r\\n\\r\\nc", "");
    assert_eq!(g.len(), 1 + 3 + 3, "{g:#?}");
    assert!(g.contains(&"<eg:document#row3> <rdf:_1> \"c\"".to_owned()));
}

#[test]
fn slices_give_the_answers_of_the_whole_file() {
    let service = |content: &str, slice: bool, pattern: &str| {
        format!(
            r#"SERVICE <x-sparql-anything:> {{
                fxe:properties fxe:content "{content}" ; fxe:media-type "text/csv" ;
                  fxe:csv.headers true ; fxe:slice {slice} ; fxe:slice.rows 3 .
                {pattern} }}"#
        )
    };
    let names = |slice: bool| {
        let mut r = rows(&format!(
            "SELECT ?name ?surname WHERE {{ {} }}",
            service(SIMPLE, slice, "?row xyz:name ?name ; xyz:surname ?surname")
        ));
        r.sort();
        r
    };
    let whole = names(false);
    assert_eq!(whole.len(), 4);
    assert_eq!(names(true), whole);
    // A slice holds the root and at most `slice.rows` rows: 3 then 1 here, so only the
    // first three rows meet (6 ordered pairs).
    let pairs = rows(&format!(
        "SELECT (COUNT(*) AS ?n) WHERE {{ {} }}",
        service(
            SIMPLE,
            true,
            "?root a fx:Root ; ?a ?r1 ; ?b ?r2 . ?r1 xyz:name ?x . ?r2 xyz:name ?y
             FILTER(?r1 != ?r2)"
        )
    ));
    assert_eq!(
        pairs,
        ["?n=\"6\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
    );
    // No data rows: the pattern still runs, on the root alone.
    let empty = rows(&format!(
        "SELECT ?root WHERE {{ {} }}",
        service("name\\n", true, "?root a fx:Root")
    ));
    assert_eq!(empty.len(), 1);
}

/// The cells of `content` read as `format`, rows joined with `|` and cells with `,` (null
/// cells as `-`).
fn cells(content: &str, format: &str) -> Vec<String> {
    let literal = content
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    let g = triplify(&literal, &format!("; fxe:csv.format \"{format}\""));
    let mut rows: Vec<Vec<(usize, String)>> = Vec::new();
    for t in g {
        // <eg:document#rowN> <rdf:_M> "value"
        let Some(rest) = t.strip_prefix("<eg:document#row") else {
            continue;
        };
        let (row, rest) = rest.split_once("> <rdf:_").unwrap();
        let (col, value) = rest.split_once("> ").unwrap();
        let (row, col): (usize, usize) = (row.parse().unwrap(), col.parse().unwrap());
        if rows.len() < row {
            rows.resize(row, Vec::new());
        }
        let value = value.trim_end_matches(" .");
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        rows[row - 1].push((col, value.to_owned()));
    }
    rows.into_iter()
        .map(|mut r| {
            r.sort();
            let n = r.last().map_or(0, |c| c.0);
            (1..=n)
                .map(|c| {
                    r.iter()
                        .find(|x| x.0 == c)
                        .map_or("-".to_owned(), |x| x.1.clone())
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

#[test]
fn formats_with_an_escape_character() {
    // MySQL: tabs, no quoting, \N is null, escaped tabs and line breaks stay in the cell.
    assert_eq!(
        cells("a\tb\\tc\t\\N\td\nx\\\ny\t\"q\"\t\\\\\tz\n", "MySQL"),
        ["a,b\\tc,-,d", "x\\ny,\\\"q\\\",\\\\,z"]
    );
    // PostgreSQLText quotes; an escaped quote inside quotes.
    assert_eq!(
        cells("\"a\\\"b\"\t\\N\t\\x\n", "PostgreSQLText"),
        ["a\\\"b,-,\\\\x"]
    );
    // Oracle: commas, trimmed.
    assert_eq!(cells(" a ,\\N, b\\,c \n", "Oracle"), ["a,-,b,c"]);
    // InformixUnload: pipes.
    assert_eq!(
        cells("a|b\\|c|\n\nd|e\n", "InformixUnload"),
        ["a,b|c,", "d,e"]
    );
}
