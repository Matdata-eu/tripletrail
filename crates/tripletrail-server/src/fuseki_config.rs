//! `tripletrail config import-fuseki config.ttl` (#826): a Fuseki assembler file translated to a
//! [configuration file](crate::config), with what could not be mapped listed.

use std::fmt::Write as _;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::{Graph, NamedNodeRef, NamedOrBlankNodeRef, Term, TermRef};

const FUSEKI: &str = "http://jena.apache.org/fuseki#";
const JA: &str = "http://jena.hpl.hp.com/2005/11/Assembler#";
const TDB2: &str = "http://jena.apache.org/2016/tdb#";
const TDB1: &str = "http://jena.hpl.hp.com/2008/tdb#";
const TEXT: &str = "http://jena.apache.org/text#";
const GEOSPARQL: &str = "http://jena.apache.org/geosparql#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";

/// The endpoint names TripleTrail answers for every dataset, and the operations it serves.
const NAMES: [&str; 8] = [
    "", "query", "sparql", "update", "data", "get", "shacl", "upload",
];
const OPERATIONS: [&str; 9] = [
    "query", "update", "gsp-r", "gsp-rw", "shacl", "upload", "patch", "no-op", "gsp",
];

/// The translation of a Fuseki configuration.
#[derive(Debug, Default)]
pub struct Imported {
    /// The configuration file, YAML.
    pub yaml: String,
    /// What it does not carry over, one line each.
    pub unmapped: Vec<String>,
}

#[derive(Default)]
struct Dataset {
    name: String,
    union: bool,
    rdfs: bool,
    owl: bool,
    read_only: bool,
    memory_files: Vec<String>,
    from: Option<String>,
    /// The text index's fields on properties TripleTrail does not index by default.
    text_properties: Vec<String>,
}

/// Translates the Fuseki configuration `text` (Turtle, as `fuseki-server --config` reads it).
pub fn import(text: &str) -> Result<Imported, String> {
    let mut graph = Graph::new();
    // Fuseki's files name their resources relative to the file (`@prefix : <#>`).
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("file:///config.ttl")
        .map_err(|e| e.to_string())?;
    for t in parser.for_slice(text.as_bytes()) {
        let q = t.map_err(|e| e.to_string())?;
        graph.insert(&tripletrail::oxigraph::model::Triple::from(q));
    }
    let iri = |local: &str, ns: &str| format!("{ns}{local}");
    let objects = |s: NamedOrBlankNodeRef<'_>, pred: &str| -> Vec<Term> {
        graph
            .objects_for_subject_predicate(s, NamedNodeRef::new_unchecked(pred))
            .map(TermRef::into_owned)
            .collect()
    };
    let subject = |t: &Term| -> Option<tripletrail::oxigraph::model::NamedOrBlankNode> {
        match t {
            Term::NamedNode(n) => Some(n.clone().into()),
            Term::BlankNode(b) => Some(b.clone().into()),
            _ => None,
        }
    };
    let value = |t: &Term| -> String {
        match t {
            Term::Literal(l) => l.value().to_owned(),
            Term::NamedNode(n) => n.as_str().to_owned(),
            Term::BlankNode(b) => format!("_:{}", b.as_str()),
            #[allow(unreachable_patterns)]
            _ => t.to_string(),
        }
    };
    let types = |s: NamedOrBlankNodeRef<'_>| -> Vec<String> {
        objects(s, RDF_TYPE).iter().map(value).collect()
    };
    let mut out = Imported::default();
    let mut timeout: Option<f64> = None;
    // ja:context [ ja:cxtName "arq:queryTimeout" ; ja:cxtValue "10000[,60000]" ] (ms).
    let mut contexts = |s: NamedOrBlankNodeRef<'_>, owner: &str, unmapped: &mut Vec<String>| {
        for c in objects(s, &iri("context", JA)) {
            let Some(c) = subject(&c) else { continue };
            let name = objects(c.as_ref(), &iri("cxtName", JA))
                .first()
                .map(value)
                .unwrap_or_default();
            let v = objects(c.as_ref(), &iri("cxtValue", JA))
                .first()
                .map(value)
                .unwrap_or_default();
            if name == "arq:queryTimeout" || name.ends_with("#queryTimeout") {
                // The overall limit, the second number when there are two.
                let ms = v
                    .split(',')
                    .rev()
                    .find_map(|n| n.trim().parse::<f64>().ok());
                match ms {
                    Some(ms) => {
                        timeout = Some(timeout.map_or(ms / 1000.0, |t: f64| t.max(ms / 1000.0)))
                    }
                    None => unmapped.push(format!(
                        "{owner}: query timeout \"{v}\" is not a number of milliseconds"
                    )),
                }
            } else {
                unmapped.push(format!("{owner}: context setting {name} = {v}"));
            }
        }
    };
    // The server's own settings.
    let servers: Vec<_> = graph
        .subjects_for_predicate_object(
            NamedNodeRef::new_unchecked(RDF_TYPE),
            NamedNodeRef::new_unchecked(&iri("Server", FUSEKI)),
        )
        .map(|s| s.into_owned())
        .collect();
    for s in &servers {
        contexts(s.as_ref(), "the server", &mut out.unmapped);
    }
    // Every service: fuseki:Service, or a subject of fuseki:name with a fuseki:dataset.
    let mut services: Vec<tripletrail::oxigraph::model::NamedOrBlankNode> = graph
        .subjects_for_predicate_object(
            NamedNodeRef::new_unchecked(RDF_TYPE),
            NamedNodeRef::new_unchecked(&iri("Service", FUSEKI)),
        )
        .map(|s| s.into_owned())
        .collect();
    for t in graph.triples_for_predicate(NamedNodeRef::new_unchecked(&iri("dataset", FUSEKI))) {
        let s = t.subject.into_owned();
        if !services.contains(&s) {
            services.push(s);
        }
    }
    let mut datasets: Vec<Dataset> = Vec::new();
    for service in &services {
        let s = service.as_ref();
        let Some(name) = objects(s, &iri("name", FUSEKI)).first().map(value) else {
            out.unmapped
                .push(format!("a service without fuseki:name ({service})"));
            continue;
        };
        let name = name.trim_matches('/').to_owned();
        let mut d = Dataset {
            name: name.clone(),
            ..Dataset::default()
        };
        contexts(s, &name, &mut out.unmapped);
        // Endpoints: fuseki:endpoint [ fuseki:operation …; fuseki:name … ], or the older
        // fuseki:serviceQuery "sparql" and the like.
        let mut writes = false;
        for e in objects(s, &iri("endpoint", FUSEKI)) {
            let Some(e) = subject(&e) else { continue };
            let op = objects(e.as_ref(), &iri("operation", FUSEKI))
                .first()
                .map(value)
                .unwrap_or_default();
            let op = op.strip_prefix(FUSEKI).unwrap_or(&op).to_owned();
            let ep = objects(e.as_ref(), &iri("name", FUSEKI))
                .first()
                .map(value)
                .unwrap_or_default();
            if matches!(
                op.as_str(),
                "update" | "gsp-rw" | "upload" | "patch" | "gsp"
            ) {
                writes = true;
            }
            if !OPERATIONS.contains(&op.as_str()) {
                out.unmapped.push(format!("{name}: operation {op}"));
            } else if !NAMES.contains(&ep.as_str()) {
                out.unmapped.push(format!(
                    "{name}: the endpoint name \"{ep}\" ({op}); TripleTrail answers {op} at its standard names (/{name}/query, /sparql, /update, /data, /get, /shacl)"
                ));
            }
        }
        for (legacy, write) in [
            ("serviceQuery", false),
            ("serviceReadGraphStore", false),
            ("serviceUpdate", true),
            ("serviceUpload", true),
            ("serviceReadWriteGraphStore", true),
        ] {
            for ep in objects(s, &iri(legacy, FUSEKI)).iter().map(value) {
                writes |= write;
                if !NAMES.contains(&ep.as_str()) {
                    out.unmapped.push(format!(
                        "{name}: the endpoint name \"{ep}\" (fuseki:{legacy}); TripleTrail answers at its standard names"
                    ));
                }
            }
        }
        d.read_only = !writes;
        // The dataset, through its wrappers: text index, GeoSPARQL, inference.
        let mut node = objects(s, &iri("dataset", FUSEKI))
            .first()
            .and_then(subject);
        let mut seen = 0;
        while let Some(n) = node.take() {
            seen += 1;
            if seen > 16 {
                break;
            }
            let n = n.as_ref();
            let ts = types(n);
            let is = |local: &str, ns: &str| ts.iter().any(|t| *t == iri(local, ns));
            if is("TextDataset", TEXT) {
                text_index(&graph, n, &mut d.text_properties);
                node = objects(n, &iri("dataset", TEXT)).first().and_then(subject);
            } else if is("GeosparqlDataset", GEOSPARQL) {
                out.unmapped.push(format!(
                    "{name}: the GeoSPARQL dataset's settings (TripleTrail's GeoSPARQL and spatial index are built in)"
                ));
                node = objects(n, &iri("dataset", GEOSPARQL))
                    .first()
                    .and_then(subject);
            } else if is("DatasetTDB2", TDB2) || is("DatasetTDB", TDB1) {
                let ns = if is("DatasetTDB2", TDB2) { TDB2 } else { TDB1 };
                d.union |= objects(n, &iri("unionDefaultGraph", ns))
                    .first()
                    .is_some_and(|v| value(v) == "true");
                if let Some(loc) = objects(n, &iri("location", ns)).first().map(value) {
                    d.from = Some(loc);
                }
                contexts(n, &name, &mut out.unmapped);
            } else if is("RDFDataset", JA) || is("MemoryDataset", JA) || ts.is_empty() {
                for f in objects(n, &iri("data", JA)) {
                    d.memory_files.push(value(&f));
                }
                // A default graph that is an inference model.
                if let Some(g) = objects(n, &iri("defaultGraph", JA))
                    .first()
                    .and_then(subject)
                {
                    let g = g.as_ref();
                    if types(g).iter().any(|t| *t == iri("InfModel", JA)) {
                        for r in objects(g, &iri("reasoner", JA)) {
                            let Some(r) = subject(&r) else { continue };
                            let url = objects(r.as_ref(), &iri("reasonerURL", JA))
                                .first()
                                .map(value)
                                .unwrap_or_default();
                            if url.contains("RDFS") {
                                d.rdfs = true;
                            } else if url.contains("OWL") {
                                d.owl = true;
                                out.unmapped.push(format!(
                                    "{name}: the reasoner {url} is approximated by OWL 2 RL (owl-rl)"
                                ));
                            } else {
                                out.unmapped.push(format!("{name}: the reasoner {url}"));
                            }
                        }
                        // The base model's dataset.
                        if let Some(base) =
                            objects(g, &iri("baseModel", JA)).first().and_then(subject)
                        {
                            let base = base.as_ref();
                            if let Some(ds) = objects(base, &iri("dataset", TDB2))
                                .first()
                                .or(objects(base, &iri("dataset", TDB1)).first())
                                .and_then(subject)
                            {
                                node = Some(ds);
                                continue;
                            }
                            if let Some(loc) =
                                objects(base, &iri("location", TDB2)).first().map(value)
                            {
                                d.from = Some(loc);
                            }
                            for f in objects(base, &iri("content", JA))
                                .iter()
                                .filter_map(subject)
                            {
                                for e in objects(f.as_ref(), &iri("externalContent", JA)) {
                                    d.memory_files.push(value(&e));
                                }
                            }
                        }
                    }
                }
                for g in objects(n, &iri("namedGraph", JA)) {
                    let _ = g;
                    out.unmapped.push(format!(
                        "{name}: ja:namedGraph (load the graph's data into the dataset instead)"
                    ));
                }
            } else {
                out.unmapped
                    .push(format!("{name}: the dataset type {}", ts.join(", ")));
            }
        }
        datasets.push(d);
    }
    if services.is_empty() {
        return Err(
            "no Fuseki service (fuseki:Service with fuseki:name and fuseki:dataset) in the file"
                .into(),
        );
    }
    // The file.
    let y = &mut out.yaml;
    let _ = writeln!(
        y,
        "# From a Fuseki configuration by tripletrail config import-fuseki."
    );
    if let Some(t) = timeout {
        let _ = writeln!(y, "timeout: {}", trim(t));
    }
    let only_memory: Vec<&Dataset> = datasets
        .iter()
        .filter(|d| !d.memory_files.is_empty())
        .collect();
    if let [d] = only_memory.as_slice() {
        if datasets.first().map(|f| f.name.as_str()) == Some(d.name.as_str()) {
            let _ = writeln!(y, "load:");
            for f in &d.memory_files {
                let _ = writeln!(y, "  - {}", quote(f.strip_prefix("file:").unwrap_or(f)));
            }
        } else {
            out.unmapped.push(format!("{}: its data files (load: loads into the first dataset only; load the others with tripletrail load --dataset)", d.name));
        }
    } else {
        for d in &only_memory {
            out.unmapped.push(format!("{}: its data files (load: loads into the first dataset only; load the others with tripletrail load --dataset)", d.name));
        }
    }
    let _ = writeln!(y, "datasets:");
    for d in &datasets {
        let _ = writeln!(y, "  - name: {}", quote(&d.name));
        if let Some(from) = &d.from {
            let _ = writeln!(
                y,
                "    # Fuseki's data in {from}: tdb2.tdbdump --loc {from} > {n}.nq, then tripletrail load --location DIR --dataset {n} {n}.nq",
                n = d.name
            );
        }
        if d.union {
            let _ = writeln!(y, "    union-default-graph: true");
        }
        if d.rdfs {
            let _ = writeln!(y, "    rdfs: true");
        }
        if d.owl {
            let _ = writeln!(y, "    owl-rl: true");
        }
        if !d.text_properties.is_empty() {
            let _ = writeln!(y, "    text-properties:");
            for p in &d.text_properties {
                let _ = writeln!(y, "      - \"{p}\"");
            }
        }
        if d.read_only {
            out.unmapped.push(format!(
                "{}: read-only (no update endpoint); TripleTrail makes a dataset read-only with path and read-only: true, or with read-only: true for all datasets of location",
                d.name
            ));
        }
    }
    if !out.unmapped.is_empty() {
        let _ = writeln!(y, "# Not carried over:");
        for u in &out.unmapped {
            let _ = writeln!(y, "#   - {u}");
        }
    }
    Ok(out)
}

/// The properties of a text index's fields that TripleTrail's full-text index does not cover by
/// default: the dataset's `text-properties`.
fn text_index(graph: &Graph, dataset: NamedOrBlankNodeRef<'_>, properties: &mut Vec<String>) {
    let o = |s: NamedOrBlankNodeRef<'_>, p: &str| -> Vec<Term> {
        graph
            .objects_for_subject_predicate(s, NamedNodeRef::new_unchecked(p))
            .map(TermRef::into_owned)
            .collect()
    };
    let node = |t: &Term| match t {
        Term::NamedNode(n) => Some(tripletrail::oxigraph::model::NamedOrBlankNode::from(
            n.clone(),
        )),
        Term::BlankNode(b) => Some(b.clone().into()),
        _ => None,
    };
    for index in o(dataset, &format!("{TEXT}index")).iter().filter_map(node) {
        for map in o(index.as_ref(), &format!("{TEXT}entityMap"))
            .iter()
            .filter_map(node)
        {
            // text:map is an RDF list of [ text:field … ; text:predicate … ].
            let mut list = o(map.as_ref(), &format!("{TEXT}map"))
                .first()
                .and_then(node);
            let mut seen = 0;
            while let Some(cell) = list.take() {
                seen += 1;
                if seen > 1000 {
                    break;
                }
                let first = o(
                    cell.as_ref(),
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#first",
                );
                for f in first.iter().filter_map(node) {
                    for p in o(f.as_ref(), &format!("{TEXT}predicate")) {
                        if let Term::NamedNode(p) = p
                            && !tripletrail::text::INDEXED.contains(&p.as_str())
                            && !properties.iter().any(|q| q == p.as_str())
                        {
                            properties.push(p.into_string());
                        }
                    }
                }
                list = o(
                    cell.as_ref(),
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#rest",
                )
                .first()
                .and_then(node);
            }
        }
    }
}

fn trim(x: f64) -> String {
    let s = format!("{x:.3}");
    s.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// A YAML scalar, quoted when it would read otherwise.
fn quote(s: &str) -> String {
    let plain = !s.is_empty()
        && s.chars().all(|c| c.is_alphanumeric() || "-_./".contains(c))
        && !matches!(s, "true" | "false" | "null" | "yes" | "no" | "on" | "off")
        && s.parse::<f64>().is_err();
    if plain {
        s.to_owned()
    } else {
        format!("{s:?}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"
@prefix :       <#> .
@prefix fuseki: <http://jena.apache.org/fuseki#> .
@prefix rdf:    <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs:   <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ja:     <http://jena.hpl.hp.com/2005/11/Assembler#> .
@prefix tdb2:   <http://jena.apache.org/2016/tdb#> .
@prefix text:   <http://jena.apache.org/text#> .
@prefix skos:   <http://www.w3.org/2004/02/skos/core#> .

[] rdf:type fuseki:Server ;
   ja:context [ ja:cxtName "arq:queryTimeout" ; ja:cxtValue "10000,60000" ] .

:kg rdf:type fuseki:Service ;
    fuseki:name "kg" ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "sparql" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:query ; fuseki:name "sparql-ro" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:update ; fuseki:name "update" ] ;
    fuseki:endpoint [ fuseki:operation fuseki:gsp-rw ; fuseki:name "data" ] ;
    fuseki:dataset :text .

:text rdf:type text:TextDataset ;
    text:dataset :tdb ;
    text:index [ a text:TextIndexLucene ;
        text:entityMap [ text:defaultField "label" ; text:entityField "uri" ;
            text:map ( [ text:field "label" ; text:predicate rdfs:label ]
                       [ text:field "note" ; text:predicate skos:note ] ) ] ] .

:tdb rdf:type tdb2:DatasetTDB2 ;
    tdb2:location "/fuseki/databases/kg" ;
    tdb2:unionDefaultGraph true .

:inf rdf:type fuseki:Service ;
    fuseki:name "reasoned" ;
    fuseki:serviceQuery "query" ;
    fuseki:dataset [ rdf:type ja:RDFDataset ;
        ja:defaultGraph [ rdf:type ja:InfModel ;
            ja:reasoner [ ja:reasonerURL <http://jena.hpl.hp.com/2003/RDFSExptRuleReasoner> ] ;
            ja:baseModel [ rdf:type ja:MemoryModel ;
                ja:content [ ja:externalContent <file:data.ttl> ] ] ] ] .
"#;

    #[test]
    fn a_fuseki_configuration_is_translated() {
        let got = import(CONFIG).unwrap();
        let y = &got.yaml;
        assert!(y.contains("timeout: 60\n"), "{y}");
        assert!(y.contains("  - name: kg\n"), "{y}");
        assert!(y.contains("    union-default-graph: true\n"), "{y}");
        assert!(y.contains("tdb2.tdbdump --loc /fuseki/databases/kg"), "{y}");
        assert!(y.contains("  - name: reasoned\n    rdfs: true\n"), "{y}");
        let un = got.unmapped.join("\n");
        assert!(un.contains("\"sparql-ro\""), "{un}");
        // The field on skos:note is indexed with text-properties; rdfs:label always is.
        assert!(!un.contains("skos/core#note"), "{un}");
        assert!(
            y.contains(
                "    text-properties:\n      - \"http://www.w3.org/2004/02/skos/core#note\"\n"
            ),
            "{y}"
        );
        assert!(!y.contains("rdf-schema#label"), "{y}");
        assert!(un.contains("reasoned: read-only"), "{un}");
        assert!(!un.contains("kg: read-only"), "{un}");
        // The file it writes is one `serve --config` reads.
        let opts: Vec<crate::config::Opt> = [
            "timeout",
            "load",
            "dataset",
            "attach",
            "alias",
            "union-default-graph",
            "rdfs",
            "owl-rl",
            "text-properties",
        ]
        .iter()
        .map(|l| crate::config::Opt {
            long: (*l).into(),
            short: None,
            takes_value: true,
            env: None,
        })
        .collect();
        let argv: Vec<std::ffi::OsString> =
            ["tripletrail", "serve"].iter().map(Into::into).collect();
        let args = crate::config::expand(&argv, y, &opts).unwrap();
        let args: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.windows(2)
                .any(|w| w == ["--union-default-graph", "kg"]),
            "{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w == ["--rdfs", "reasoned"]),
            "{args:?}"
        );
        assert!(
            args.windows(2).any(|w| w
                == [
                    "--text-properties",
                    "kg=http://www.w3.org/2004/02/skos/core#note"
                ]),
            "{args:?}"
        );
    }

    #[test]
    fn a_file_without_services_is_refused() {
        assert!(
            import("<a> <b> <c> .")
                .unwrap_err()
                .contains("no Fuseki service")
        );
        assert!(import("not turtle").is_err());
    }
}
