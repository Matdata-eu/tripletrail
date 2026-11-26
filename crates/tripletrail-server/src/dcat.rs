//! A [DCAT-AP](https://semiceu.github.io/DCAT-AP/) catalog of the served datasets at
//! `/.well-known/dcat` (#141), for data portals such as data.europa.eu that harvest DCAT.
//!
//! The server writes what it knows: the catalog, a `dcat:Dataset` per dataset (the node of
//! its VoID description), a distribution for its SPARQL endpoint and one for its dump, and
//! the endpoint as a `dcat:DataService`. What it cannot know (titles, descriptions,
//! publisher, contact point, licence, keywords, themes) comes from the Turtle file of
//! `--catalog`, whose relative IRIs resolve against the server's URL: `<#catalog>` is the
//! catalog and `<kg/void#dataset>` the dataset `kg` (or an alias: `<live/void#dataset>` is
//! the dataset `live` names). A title or description the file gives replaces the generated
//! one.
//!
//! Only what the catalog and the datasets the caller may list reach is served: the record of
//! a dataset the caller may not see is left out, with all that only it leads to.

use crate::HttpError;
use std::collections::{HashMap, HashSet, VecDeque};
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::vocab::rdf;
use tripletrail::oxigraph::model::{
    Literal, NamedNode, NamedOrBlankNode, NamedOrBlankNodeRef, Term, Triple,
};

pub(crate) const DCAT: &str = "http://www.w3.org/ns/dcat#";
const DCTERMS: &str = "http://purl.org/dc/terms/";
const FOAF: &str = "http://xmlns.com/foaf/0.1/";
const SPARQL_PROTOCOL: &str = "https://www.w3.org/TR/sparql11-protocol/";
const NQUADS: &str = "http://www.iana.org/assignments/media-types/application/n-quads";

fn dcat(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{DCAT}{name}"))
}

fn dct(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{DCTERMS}{name}"))
}

fn iri(i: impl Into<String>) -> NamedNode {
    NamedNode::new_unchecked(i)
}

/// The catalog's IRI, `<#catalog>` in the `--catalog` file.
pub(crate) fn catalog_iri(origin: &str) -> String {
    format!("{origin}/#catalog")
}

/// Parses the `--catalog` file `text` with `base` as base IRI: an error says where it is
/// wrong. Run at start to refuse a bad file, and for each request with the request's origin.
pub(crate) fn parse(text: &str, base: &str) -> Result<Vec<Triple>, String> {
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri(base)
        .map_err(|e| e.to_string())?;
    parser
        .for_slice(text.as_bytes())
        .map(|q| q.map(Triple::from).map_err(|e| e.to_string()))
        .collect()
}

/// The catalog for a caller who may list the datasets `listed` (of the server's `all`), at
/// `origin`, with the `--catalog` file `file`. In the file, an alias's IRIs
/// (`<live/void#dataset>`, `<live/void#dump>`) stand for those of the dataset it names now,
/// from `aliases` (alias, dataset), so that the record follows a swap.
pub(crate) fn describe(
    origin: &str,
    listed: &[&str],
    all: &[&str],
    aliases: &[(&str, &str)],
    file: Option<&str>,
    site_name: Option<&str>,
) -> Result<Vec<Triple>, HttpError> {
    // The origin comes from the request's Host (or X-Forwarded-Host): not an IRI, the
    // request is wrong.
    NamedNode::new(format!("{origin}/"))
        .map_err(|e| HttpError(400, format!("the request's host gives no IRI: {e}")))?;
    let mut given = match file {
        Some(text) => parse(text, &format!("{origin}/"))
            .map_err(|e| HttpError(500, format!("the --catalog file: {e}")))?,
        None => Vec::new(),
    };
    // An alias's IRIs (`<live/...>`) are those of the dataset it names now.
    if !aliases.is_empty() {
        let moved: Vec<(String, String)> = aliases
            .iter()
            .map(|(alias, target)| (format!("{origin}/{alias}/"), format!("{origin}/{target}/")))
            .collect();
        let to = |n: &NamedNode| {
            moved.iter().find_map(|(from, to)| {
                n.as_str()
                    .strip_prefix(from.as_str())
                    .map(|rest| iri(format!("{to}{rest}")))
            })
        };
        for t in &mut given {
            if let NamedOrBlankNode::NamedNode(n) = &t.subject
                && let Some(n) = to(n)
            {
                t.subject = n.into();
            }
            if let Term::NamedNode(n) = &t.object
                && let Some(n) = to(n)
            {
                t.object = n.into();
            }
        }
    }
    let catalog: NamedOrBlankNode = iri(catalog_iri(origin)).into();
    let datasets: Vec<NamedOrBlankNode> = listed
        .iter()
        .map(|name| iri(crate::void::dataset_iri(origin, name)).into())
        .collect();
    // Every IRI under a dataset the caller may not list: as subject or object, it is left out.
    let mut hidden: Vec<String> = all
        .iter()
        .filter(|name| !listed.contains(name))
        .map(|name| format!("{origin}/{name}/"))
        .collect();
    hidden.extend(
        aliases
            .iter()
            .filter(|(_, target)| !listed.contains(target))
            .map(|(alias, _)| format!("{origin}/{alias}/")),
    );
    // The catalog, and each dataset with the nodes the server links it to: what the file says
    // about a distribution (its licence) or the endpoint is served too.
    let mut roots = vec![catalog.clone()];
    for (name, ds) in listed.iter().zip(&datasets) {
        roots.push(ds.clone());
        for node in ["void#sparql", "void#dump", "sparql"] {
            roots.push(iri(format!("{origin}/{name}/{node}")).into());
        }
    }
    let mut out = reachable(&given, &roots, &hidden);
    let has = |out: &[Triple], s: &NamedOrBlankNode, p: &NamedNode| {
        out.iter().any(|t| &t.subject == s && &t.predicate == p)
    };
    let mut add = Vec::new();
    let t = |s: &NamedOrBlankNode, p: NamedNode, o: Term| Triple::new(s.clone(), p, o);

    let host = origin.split("://").nth(1).unwrap_or(origin);
    add.push(t(&catalog, rdf::TYPE.into(), dcat("Catalog").into()));
    if !has(&out, &catalog, &dct("title")) {
        add.push(t(
            &catalog,
            dct("title"),
            Literal::new_simple_literal(match site_name {
                Some(name) => name.to_owned(),
                None => format!("TripleTrail at {host}"),
            })
            .into(),
        ));
    }
    if !has(&out, &catalog, &dct("description")) {
        add.push(t(
            &catalog,
            dct("description"),
            Literal::new_simple_literal(format!(
                "The datasets served by the TripleTrail server at {host}, each with a SPARQL \
                 endpoint and a dump."
            ))
            .into(),
        ));
    }
    add.push(t(
        &catalog,
        iri(format!("{FOAF}homepage")),
        iri(format!("{origin}/")).into(),
    ));
    for (name, ds) in listed.iter().zip(&datasets) {
        let endpoint = iri(format!("{origin}/{name}/sparql"));
        let service: NamedOrBlankNode = endpoint.clone().into();
        add.push(t(&catalog, dcat("dataset"), ds.clone().into()));
        add.push(t(&catalog, dcat("service"), endpoint.clone().into()));
        add.push(t(ds, rdf::TYPE.into(), dcat("Dataset").into()));
        if !has(&out, ds, &dct("title")) {
            add.push(t(
                ds,
                dct("title"),
                Literal::new_simple_literal(*name).into(),
            ));
        }
        if !has(&out, ds, &dct("description")) {
            add.push(t(
                ds,
                dct("description"),
                Literal::new_simple_literal(format!(
                    "The dataset {name}, with a SPARQL endpoint and a dump."
                ))
                .into(),
            ));
        }
        // The SPARQL endpoint, as a distribution and as a data service.
        let query: NamedOrBlankNode = iri(format!("{origin}/{name}/void#sparql")).into();
        add.push(t(ds, dcat("distribution"), query.clone().into()));
        add.push(t(&query, rdf::TYPE.into(), dcat("Distribution").into()));
        add.push(t(
            &query,
            dct("title"),
            Literal::new_simple_literal("SPARQL endpoint").into(),
        ));
        add.push(t(&query, dcat("accessURL"), endpoint.clone().into()));
        add.push(t(&query, dcat("accessService"), endpoint.clone().into()));
        add.push(t(&service, rdf::TYPE.into(), dcat("DataService").into()));
        add.push(t(
            &service,
            dct("title"),
            Literal::new_simple_literal(format!("SPARQL endpoint of {name}")).into(),
        ));
        add.push(t(&service, dcat("endpointURL"), endpoint.clone().into()));
        // A GET of the endpoint answers its SPARQL Service Description.
        add.push(t(
            &service,
            dcat("endpointDescription"),
            endpoint.clone().into(),
        ));
        add.push(t(&service, dct("conformsTo"), iri(SPARQL_PROTOCOL).into()));
        add.push(t(&service, dcat("servesDataset"), ds.clone().into()));
        // The dump: every graph, as N-Quads.
        let dump: NamedOrBlankNode = iri(format!("{origin}/{name}/void#dump")).into();
        let url = iri(format!("{origin}/{name}/get?format=nquads"));
        add.push(t(ds, dcat("distribution"), dump.clone().into()));
        add.push(t(&dump, rdf::TYPE.into(), dcat("Distribution").into()));
        add.push(t(
            &dump,
            dct("title"),
            Literal::new_simple_literal("Dump (N-Quads)").into(),
        ));
        add.push(t(&dump, dcat("accessURL"), url.clone().into()));
        add.push(t(&dump, dcat("downloadURL"), url.into()));
        add.push(t(&dump, dcat("mediaType"), iri(NQUADS).into()));
    }
    out.extend(add);
    // One block per node in Turtle. The distributions have IRIs, not blank nodes, so the
    // order is the same every time.
    out.sort_by_cached_key(|t| t.subject.to_string());
    Ok(out)
}

/// The triples of `given` whose subject `roots` reach through the objects of other such
/// triples, leaving out the nodes `hidden` and what only they lead to.
fn reachable(given: &[Triple], roots: &[NamedOrBlankNode], hidden: &[String]) -> Vec<Triple> {
    let is_hidden = |n: NamedOrBlankNodeRef<'_>| match n {
        NamedOrBlankNodeRef::NamedNode(n) => {
            hidden.iter().any(|h| n.as_str().starts_with(h.as_str()))
        }
        NamedOrBlankNodeRef::BlankNode(_) => false,
    };
    let mut by_subject: HashMap<NamedOrBlankNodeRef<'_>, Vec<&Triple>> = HashMap::new();
    for t in given {
        by_subject.entry(t.subject.as_ref()).or_default().push(t);
    }
    let mut seen: HashSet<NamedOrBlankNodeRef<'_>> = HashSet::new();
    let mut todo: VecDeque<NamedOrBlankNodeRef<'_>> = roots.iter().map(|r| r.as_ref()).collect();
    let mut out = Vec::new();
    while let Some(node) = todo.pop_front() {
        if !seen.insert(node) || is_hidden(node) {
            continue;
        }
        for t in by_subject.get(&node).into_iter().flatten() {
            let next = match &t.object {
                Term::NamedNode(n) => Some(NamedOrBlankNodeRef::from(n.as_ref())),
                Term::BlankNode(b) => Some(NamedOrBlankNodeRef::from(b.as_ref())),
                _ => None,
            };
            if next.is_some_and(is_hidden) {
                continue;
            }
            out.push((*t).clone());
            todo.extend(next);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_fills_in_and_hidden_datasets_stay_out() {
        let file = r#"
            @prefix dct: <http://purl.org/dc/terms/> .
            @prefix dcat: <http://www.w3.org/ns/dcat#> .
            <#catalog> dct:title "Rail data" ; dct:publisher <https://example.org/infra> .
            <https://example.org/infra> dct:title "Infra" .
            <live/void#dataset> dct:title "Network" ; dcat:keyword "rail" ;
                dct:license <http://creativecommons.org/licenses/by/4.0/> .
            <secret/void#dataset> dct:title "Secret" ; dcat:contactPoint [ dct:title "x" ] .
            <live/void#dump> dct:license <http://creativecommons.org/publicdomain/zero/1.0/> .
            <kg/sparql> dct:description "The public endpoint" .
            <#catalog> dct:relation <secret/void#dump> .
            <kg/void#dataset> dct:relation <secret/get?format=nquads> , <secalias/void#sparql> .
            <secret/void#dump> dct:title "hidden dump note" .
        "#;
        let triples = describe(
            "http://h",
            &["kg"],
            &["kg", "secret"],
            &[("live", "kg"), ("secalias", "secret")],
            Some(file),
            None,
        )
        .unwrap();
        let text: Vec<String> = triples.iter().map(ToString::to_string).collect();
        let has = |s: &str| text.iter().any(|t| t.contains(s));
        assert!(has("\"Rail data\""));
        assert!(
            !has("TripleTrail at h"),
            "the file's title replaces the default"
        );
        assert!(has("\"Infra\""), "reached through the publisher");
        assert!(has(
            "<http://h/kg/void#dataset> <http://purl.org/dc/terms/title> \"Network\""
        ));
        assert!(has(
            "<http://h/kg/sparql> <http://www.w3.org/ns/dcat#endpointURL>"
        ));
        assert!(!has("Secret"));
        assert!(!has("secret"));
        assert!(!has("\"x\""));
        assert!(!has("hidden"));
        assert!(has(
            "<http://h/kg/void#dump> <http://purl.org/dc/terms/license>"
        ));
        assert!(has("\"The public endpoint\""));
        assert!(
            !has("relation"),
            "links to a hidden dataset's IRIs are left out: {:?}",
            text.iter()
                .filter(|t| t.contains("relation"))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_host_that_is_no_iri_is_the_requests_fault() {
        let e = describe("http://a b<>", &["kg"], &["kg"], &[], None, None).unwrap_err();
        assert_eq!(e.0, 400);
    }

    #[test]
    fn without_a_file_the_mandatory_properties_are_there() {
        let triples = describe("http://h", &["kg"], &["kg"], &[], None, None).unwrap();
        let text: Vec<String> = triples.iter().map(ToString::to_string).collect();
        // --site-name titles the catalog when the file does not (#262).
        let named = describe("http://h", &["kg"], &["kg"], &[], None, Some("Rail KG")).unwrap();
        assert!(named.iter().any(|t| t.to_string().ends_with("<http://purl.org/dc/terms/title> \"Rail KG\"")));
        for want in [
            "<http://h/#catalog> <http://purl.org/dc/terms/title> \"TripleTrail at h\"",
            "<http://h/kg/void#dataset> <http://purl.org/dc/terms/title> \"kg\"",
            "<http://h/kg/void#dataset> <http://purl.org/dc/terms/description>",
            "<http://h/kg/get?format=nquads>",
        ] {
            assert!(text.iter().any(|t| t.contains(want)), "{want}: {text:#?}");
        }
    }
}
