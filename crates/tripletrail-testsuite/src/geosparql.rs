//! The [GeoSPARQL Compliance Benchmark](https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark)
//! (Jovanovik, Homburg and Spasić, 2021): 206 queries on one dataset, covering the 30
//! requirements of GeoSPARQL 1.0, with one or more accepted answers each. It is the test set
//! GeoSPARQL compliance of RDF stores is usually reported against.
//!
//! The benchmark is GPL-licensed: the runner reads it from a local checkout and copies
//! nothing of it.

use crate::manifest::{Files, Kind, Test};
use std::collections::BTreeMap;
use std::path::Path;

/// The IRI the benchmark's files are read under: their location on GitHub.
pub const GSB_BASE: &str =
    "https://github.com/OpenLinkSoftware/GeoSPARQLBenchmark/blob/master/src/main/resources/";

/// The benchmark's resources in a checkout.
pub const GSB_DIR: &str = "src/main/resources";

/// The benchmark's constants, with the weight of each query in the compliance percentage.
const CONSTANTS: &str = "src/main/java/org/hobbit/geosparql/util/GSBConstants.java";

/// The GeoSPARQL 1.0 requirements, by number, under
/// `http://www.opengis.net/spec/geosparql/1.0/req/`.
const REQUIREMENTS: [&str; 30] = [
    "core/sparql-protocol",
    "core/spatial-object-class",
    "core/feature-class",
    "topology-vocab-extension/sf-spatial-relations",
    "topology-vocab-extension/eh-spatial-relations",
    "topology-vocab-extension/rcc8-spatial-relations",
    "geometry-extension/geometry-class",
    "geometry-extension/feature-properties",
    "geometry-extension/geometry-properties",
    "geometry-extension/wkt-literal",
    "geometry-extension/wkt-literal-default-srs",
    "geometry-extension/wkt-axis-order",
    "geometry-extension/wkt-literal-empty",
    "geometry-extension/geometry-as-wkt-literal",
    "geometry-extension/gml-literal",
    "geometry-extension/gml-literal-empty",
    "geometry-extension/gml-profile",
    "geometry-extension/geometry-as-gml-literal",
    "geometry-extension/query-functions",
    "geometry-extension/srid-function",
    "geometry-topology-extension/relate-query-function",
    "geometry-topology-extension/sf-query-functions",
    "geometry-topology-extension/eh-query-functions",
    "geometry-topology-extension/rcc8-query-functions",
    "rdfs-entailment-extension/bgp-rdfs-ent",
    "rdfs-entailment-extension/wkt-geometry-types",
    "rdfs-entailment-extension/gml-geometry-types",
    "query-rewrite-extension/sf-query-rewrite",
    "query-rewrite-extension/eh-query-rewrite",
    "query-rewrite-extension/rcc8-query-rewrite",
];

/// The benchmark's queries as tests, by requirement and in the benchmark's order. The
/// dataset loads into the default graph; any of a query's answer files is a pass.
pub fn tests(files: &Files) -> Result<Vec<Test>, String> {
    let dir = files.path(&format!("{GSB_BASE}gsb_queries/"))?;
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut names: Vec<(Vec<u32>, String)> = Vec::new();
    for entry in entries {
        let name = entry
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .file_name()
            .to_string_lossy()
            .into_owned();
        // query-r19-2-1.rq; the benchmark's other queries (query01.rq, …) are for its
        // performance runs.
        if let Some(numbers) = query_numbers(&name) {
            names.push((numbers, name));
        }
    }
    names.sort();
    let answers = files.path(&format!("{GSB_BASE}gsb_answers/"))?;
    let mut tests = Vec::new();
    for (numbers, name) in names {
        let stem = name.trim_end_matches(".rq");
        let requirement = numbers[0] as usize;
        let mut alternatives = Vec::new();
        for k in 1.. {
            let file = format!("{stem}-alternative-{k}.srx");
            if !answers.join(&file).exists() {
                break;
            }
            alternatives.push(format!("{GSB_BASE}gsb_answers/{file}"));
        }
        tests.push(Test {
            id: format!("{GSB_BASE}gsb_queries/{name}"),
            kind: Kind::QueryEvaluation,
            name: String::new(),
            group: format!(
                "R{requirement} {}",
                REQUIREMENTS.get(requirement - 1).copied().unwrap_or("?")
            ),
            not_approved: None,
            action: Some(format!("{GSB_BASE}gsb_queries/{name}")),
            data: vec![format!("{GSB_BASE}gsb_dataset/dataset.rdf")],
            graph_data: Vec::new(),
            unsupported: None,
            service_data: Vec::new(),
            regime: None,
            recognized: Vec::new(),
            result: Some(format!("{GSB_BASE}gsb_answers/{stem}.srx")),
            alternatives,
            result_data: Vec::new(),
            result_graph_data: Vec::new(),
            lax: false,
            requires: Vec::new(),
            requests: Vec::new(),
            report: None,
        });
    }
    if tests.is_empty() {
        return Err(format!("no benchmark queries in {}", dir.display()));
    }
    Ok(tests)
}

/// The numbers of `query-r19-2-1.rq`: `[19, 2, 1]`.
fn query_numbers(name: &str) -> Option<Vec<u32>> {
    let numbers = name.strip_prefix("query-r")?.strip_suffix(".rq")?;
    numbers.split('-').map(|n| n.parse().ok()).collect()
}

/// Each query's weight in the benchmark's compliance percentage, by file name
/// (`query-r19-2-1.rq`), read from the benchmark's constants: each requirement weighs 1/30,
/// shared among its queries.
pub fn weights(checkout: &Path) -> Result<BTreeMap<String, f64>, String> {
    let path = checkout.join(CONSTANTS);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut weights = BTreeMap::new();
    // public static final double GSB_ANSWER_WEIGHT_19_2_1 = 1.0/30.0 * 1.0/9.0 * 1.0/2.0;
    for line in text.lines() {
        let Some(rest) = line
            .trim()
            .strip_prefix("public static final double GSB_ANSWER_WEIGHT_")
        else {
            continue;
        };
        let Some((key, expression)) = rest.split_once('=') else {
            continue;
        };
        let mut weight = 1.0;
        for factor in expression.trim().trim_end_matches(';').split('*') {
            let value = match factor.trim().split_once('/') {
                Some((a, b)) => number(a)? / number(b)?,
                None => number(factor)?,
            };
            weight *= value;
        }
        let name = format!("query-r{}.rq", key.trim().replace('_', "-"));
        // The constants write requirements under 10 with one digit (WEIGHT_4_1), the files
        // with two (query-r04-1.rq).
        let name = match name
            .strip_prefix("query-r")
            .and_then(|r| r.split_once(['-', '.']))
        {
            Some((n, _)) if n.len() == 1 => name.replacen("query-r", "query-r0", 1),
            _ => name,
        };
        weights.insert(name, weight);
    }
    if weights.is_empty() {
        return Err(format!(
            "{}: no GSB_ANSWER_WEIGHT_ constants",
            path.display()
        ));
    }
    Ok(weights)
}

fn number(text: &str) -> Result<f64, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("{CONSTANTS}: {text:?} is not a number"))
}

/// The benchmark's compliance percentage: the weights of the queries that pass, plus
/// requirement 17 (the GML profile), which has no query and counts as met when any query
/// passes.
pub fn compliance(weights: &BTreeMap<String, f64>, passed: &[&str]) -> f64 {
    let mut score: f64 = passed.iter().filter_map(|name| weights.get(*name)).sum();
    if !passed.is_empty() {
        score += 1.0 / REQUIREMENTS.len() as f64;
    }
    100.0 * score
}

/// A failure reason without the benchmark's expected values (`expected 1 rows, got 2`): the
/// report and the known failures are committed, the benchmark's GPL data is not.
pub fn without_values(reason: &str) -> String {
    match reason
        .split_once("; missing")
        .or_else(|| reason.split_once("; unexpected"))
    {
        Some((counts, _)) => counts.to_owned(),
        None => reason.to_owned(),
    }
}

/// Whether a test is one of the benchmark's.
pub fn is_benchmark(test: &Test) -> bool {
    test.id.starts_with(GSB_BASE)
}

/// The query's file name (`query-r19-2-1.rq`).
pub fn file_name(test: &Test) -> &str {
    test.id.rsplit('/').next().unwrap_or(&test.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_of_query_files() {
        assert_eq!(query_numbers("query-r01.rq"), Some(vec![1]));
        assert_eq!(query_numbers("query-r19-2-1.rq"), Some(vec![19, 2, 1]));
        assert_eq!(query_numbers("query05_detail.rq"), None);
        assert_eq!(query_numbers("query-r04-1.srx"), None);
    }

    #[test]
    fn weights_by_file_name() {
        let dir = tripletrail_testdir::tempdir("gsb-weights");
        let file = dir.join(CONSTANTS);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(
            &file,
            "    public static final double GSB_ANSWER_WEIGHT_1 = 1.0/30.0;\n\
             public static final double GSB_ANSWER_WEIGHT_4_2 = 1.0/30.0 * 1.0/8.0;\n\
             public static final double GSB_ANSWER_WEIGHT_19_2_1 = 1.0/30.0 * 1.0/9.0 * 1.0/2.0;\n\
             public static final double [] GSB_ANSWERS_WEIGHTS = {\n",
        )
        .unwrap();
        let weights = weights(&dir).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let keys: Vec<&str> = weights.keys().map(String::as_str).collect();
        assert_eq!(keys, ["query-r01.rq", "query-r04-2.rq", "query-r19-2-1.rq"]);
        assert!((weights["query-r19-2-1.rq"] - 1.0 / 540.0).abs() < 1e-12);
        let score = compliance(&weights, &["query-r01.rq"]);
        assert!((score - 200.0 / 30.0).abs() < 1e-9);
        assert_eq!(compliance(&weights, &[]), 0.0);
    }

    #[test]
    fn reasons_without_values() {
        assert_eq!(
            without_values("expected 1 rows, got 2; unexpected `?f=<http://example.org/A>`"),
            "expected 1 rows, got 2"
        );
        assert_eq!(
            without_values("expected 3 rows, got 3; missing `a`; unexpected `b`"),
            "expected 3 rows, got 3"
        );
        assert_eq!(without_values("query failed: x"), "query failed: x");
    }
}
