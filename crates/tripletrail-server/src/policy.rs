//! Dataset access (#200) as [sparql-guard](https://matdata-eu.github.io/sparql-guard/reference/policy-language)
//! policies: `sg:Policy` nodes that grant `sg:readAllGraphs` (and the proposed
//! `sg:writeAllGraphs`), or `sg:readGraph` and the proposed `sg:writeGraph` (some named
//! graphs, #201), to `sg:agent`s or `sg:group`s.
//!
//! Row rules (`sg:visibleIf`, `sg:hideProperties`, #202) are compiled by the engine
//! ([`tripletrail::RowRule`]); their shapes come from the same files. Shapes outside the spec's
//! SHACL subset make the files fail to load: a policy that silently ignored part of its rules
//! would show more than intended.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use tripletrail::oxigraph::io::{RdfFormat, RdfParser};
use tripletrail::oxigraph::model::Graph;
use tripletrail::oxigraph::model::{Literal, Term};
use tripletrail::{RowRule, RowShapes};

pub const SG: &str = "https://matdata.eu/ns/sparql-guard#";
const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const XSD_BOOLEAN: &str = "http://www.w3.org/2001/XMLSchema#boolean";
/// The name Jena (Fuseki) gives the store's default graph, which sparql-guard grants by IRI.
pub const DEFAULT_GRAPH: &str = "urn:x-arq:DefaultGraph";

/// Who a policy is for.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Agent {
    /// `sg:Anyone`: every caller, anonymous ones included.
    Anyone,
    /// `sg:AuthenticatedAgent`: every caller with an agent.
    Authenticated,
    Iri(String),
}

/// One `sg:Policy`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Policy {
    pub id: String,
    agents: Vec<Agent>,
    groups: Vec<String>,
    pub read: bool,
    pub write: bool,
    /// `sg:readGraph`: graphs readable without `read` ([`DEFAULT_GRAPH`] for the default graph),
    /// those of `write_graphs` included.
    pub read_graphs: Vec<String>,
    /// `sg:writeGraph`: graphs writable without `write`.
    pub write_graphs: Vec<String>,
    /// `sg:visibleIf` and `sg:hideProperties` (#202).
    pub rows: Vec<RowRule>,
    /// The triples of the policy files, where the shapes of `rows` are: set when `rows` is not
    /// empty, shared by the policies of the same files.
    pub shapes: Option<Arc<RowShapes>>,
}

impl Policy {
    /// The policy as the access pages show it (#205); `None` for one without an IRI, which
    /// the pages cannot address.
    pub fn spec(&self) -> Option<crate::policy_edit::PolicySpec> {
        let id = self.id.strip_prefix('<')?.strip_suffix('>')?.to_owned();
        Some(crate::policy_edit::PolicySpec {
            id,
            agents: self
                .agents
                .iter()
                .map(|a| match a {
                    Agent::Anyone => format!("{SG}Anyone"),
                    Agent::Authenticated => format!("{SG}AuthenticatedAgent"),
                    Agent::Iri(i) => i.clone(),
                })
                .collect(),
            groups: self.groups.clone(),
            read_all: self.read,
            write_all: self.write,
            read_graphs: self
                .read_graphs
                .iter()
                .filter(|g| !self.write_graphs.contains(g))
                .cloned()
                .collect(),
            write_graphs: self.write_graphs.clone(),
        })
    }

    /// Whether the policy applies to a caller with this agent (`None` for anonymous) and these
    /// groups.
    pub fn applies_to(&self, agent: Option<&str>, groups: &[String]) -> bool {
        self.agents.iter().any(|a| match a {
            Agent::Anyone => true,
            Agent::Authenticated => agent.is_some(),
            Agent::Iri(iri) => agent == Some(iri.as_str()),
        }) || self.groups.iter().any(|g| groups.contains(g))
    }
}

/// Reads the policies of Turtle, TriG, N-Triples or N-Quads files.
pub fn read(files: &[impl AsRef<Path>]) -> Result<Vec<Policy>, String> {
    let mut quads = Vec::new();
    for file in files {
        let file = file.as_ref();
        let reader = std::fs::File::open(file).map_err(|e| format!("{}: {e}", file.display()))?;
        quads.extend(
            parse(format_of(file)?, std::io::BufReader::new(reader))
                .map_err(|e| format!("{}: {e}", file.display()))?,
        );
    }
    policies(quads)
}

/// The RDF format of a policy file, from its extension.
pub fn format_of(file: &Path) -> Result<RdfFormat, String> {
    file.extension()
        .and_then(|e| e.to_str())
        .and_then(RdfFormat::from_extension)
        .ok_or_else(|| format!("{}: unknown RDF format", file.display()))
}

/// Reads the policies of one document.
#[cfg(test)]
pub fn read_text(text: &str, format: RdfFormat) -> Result<Vec<Policy>, String> {
    policies(parse(format, text.as_bytes())?)
}

fn parse(
    format: RdfFormat,
    reader: impl std::io::Read,
) -> Result<Vec<tripletrail::oxigraph::model::Quad>, String> {
    RdfParser::from_format(format)
        .for_reader(reader)
        .map(|q| q.map_err(|e| e.to_string()))
        .collect()
}

fn policies(quads: Vec<tripletrail::oxigraph::model::Quad>) -> Result<Vec<Policy>, String> {
    let graph: Graph = quads
        .iter()
        .map(|q| tripletrail::oxigraph::model::TripleRef::from(q.as_ref()))
        .collect();
    // Subject -> (predicate, object) pairs, over every file.
    let mut nodes: BTreeMap<String, Vec<(String, Term)>> = BTreeMap::new();
    for quad in quads {
        nodes
            .entry(quad.subject.to_string())
            .or_default()
            .push((quad.predicate.as_str().to_owned(), quad.object));
    }
    let mut policies = Vec::new();
    for (id, properties) in &nodes {
        let is_policy = properties.iter().any(|(p, o)| {
            p == RDF_TYPE && matches!(o, Term::NamedNode(n) if n.as_str() == format!("{SG}Policy"))
        });
        if !is_policy {
            continue;
        }
        policies.push(policy(id, properties)?);
    }
    if policies.iter().any(|p| !p.rows.is_empty()) {
        let graph = Arc::new(RowShapes::new(graph));
        for p in &mut policies {
            if !p.rows.is_empty() {
                tripletrail::check_row_shapes(graph.graph(), &p.rows)
                    .map_err(|e| format!("{}: {e}", p.id))?;
                p.shapes = Some(Arc::clone(&graph));
            }
        }
    }
    Ok(policies)
}

fn policy(id: &str, properties: &[(String, Term)]) -> Result<Policy, String> {
    let mut p = Policy {
        id: id.to_owned(),
        agents: Vec::new(),
        groups: Vec::new(),
        read: false,
        write: false,
        read_graphs: Vec::new(),
        write_graphs: Vec::new(),
        rows: Vec::new(),
        shapes: None,
    };
    let mut references = true;
    let mut visible = Vec::new();
    for (predicate, object) in properties {
        let Some(local) = predicate.strip_prefix(SG) else {
            continue;
        };
        match local {
            "agent" => p.agents.push(match object {
                Term::NamedNode(n) if n.as_str() == format!("{SG}Anyone") => Agent::Anyone,
                Term::NamedNode(n) if n.as_str() == format!("{SG}AuthenticatedAgent") => {
                    Agent::Authenticated
                }
                Term::NamedNode(n) => Agent::Iri(n.as_str().to_owned()),
                _ => return Err(format!("{id}: sg:agent must be an IRI")),
            }),
            "group" => match object {
                Term::Literal(l) => p.groups.push(l.value().to_owned()),
                _ => return Err(format!("{id}: sg:group must be a literal")),
            },
            "readAllGraphs" => p.read |= boolean(id, local, object)?,
            "writeAllGraphs" => p.write |= boolean(id, local, object)?,
            "hideReferences" => references = boolean(id, local, object)?,
            // `*` and `+` paths are always allowed: every node on the way is checked at the scan.
            "allowTransitivePaths" => {
                boolean(id, local, object)?;
            }
            "readGraph" | "writeGraph" => match object {
                Term::NamedNode(n) => {
                    if local == "writeGraph" {
                        p.write_graphs.push(n.as_str().to_owned());
                    }
                    p.read_graphs.push(n.as_str().to_owned());
                }
                _ => return Err(format!("{id}: sg:{local} must be an IRI")),
            },
            "visibleIf" | "hideProperties" | "editableIf" => {
                let shape = match object {
                    Term::NamedNode(_) | Term::BlankNode(_) => object.clone(),
                    _ => return Err(format!("{id}: sg:{local} must name a shape")),
                };
                if local == "visibleIf" {
                    visible.push(shape);
                } else if local == "editableIf" {
                    p.rows.push(RowRule::EditableIf { shape });
                } else {
                    p.rows.push(RowRule::HideProperties { shape });
                }
            }
            other => return Err(format!("{id}: unknown property sg:{other}")),
        }
    }
    p.rows.extend(
        visible
            .into_iter()
            .map(|shape| RowRule::VisibleIf { shape, references }),
    );
    if p.agents.is_empty() && p.groups.is_empty() {
        return Err(format!("{id}: a policy needs an sg:agent or an sg:group"));
    }
    // Writers read what they write.
    p.read |= p.write;
    Ok(p)
}

fn boolean(id: &str, local: &str, object: &Term) -> Result<bool, String> {
    match object {
        Term::Literal(l) if is_boolean(l) => Ok(matches!(l.value(), "true" | "1")),
        _ => Err(format!("{id}: sg:{local} must be true or false")),
    }
}

fn is_boolean(l: &Literal) -> bool {
    l.datatype().as_str() == XSD_BOOLEAN && matches!(l.value(), "true" | "false" | "1" | "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<Policy>, String> {
        let dir = tripletrail_testdir::tempdir("policy");
        let path = dir.join(format!("{:x}.ttl", crate::auth::sha256(text.as_bytes())[0]));
        std::fs::write(&path, text).unwrap();
        read(&[path])
    }

    const PREFIXES: &str = "@prefix sg: <https://matdata.eu/ns/sparql-guard#> .\n@prefix ex: <http://example.org/> .\n";

    #[test]
    fn policies_grant_by_agent_or_group() {
        let p = parse(&format!(
            "{PREFIXES}
            ex:Open a sg:Policy ; sg:agent sg:Anyone ; sg:readAllGraphs true .
            ex:Staff a sg:Policy ; sg:agent sg:AuthenticatedAgent ; sg:readAllGraphs true .
            ex:Writers a sg:Policy ; sg:group \"dim-assets\" ; sg:writeAllGraphs true .
            ex:Jan a sg:Policy ; sg:agent <https://example.org/people/jan> ; sg:readAllGraphs false .
            ex:NotAPolicy sg:readAllGraphs true ."
        ))
        .unwrap();
        assert_eq!(p.len(), 4);
        let by = |id: &str| {
            p.iter()
                .find(|p| p.id.ends_with(&format!("{id}>")))
                .unwrap()
        };
        assert!(by("Open").applies_to(None, &[]));
        assert!(!by("Staff").applies_to(None, &[]));
        assert!(by("Staff").applies_to(Some("urn:x"), &[]));
        let writers = by("Writers");
        assert!(writers.write && writers.read);
        assert!(writers.applies_to(Some("urn:x"), &["dim-assets".to_owned()]));
        assert!(!writers.applies_to(Some("urn:x"), &["other".to_owned()]));
        let jan = by("Jan");
        assert!(jan.applies_to(Some("https://example.org/people/jan"), &[]) && !jan.read);
    }

    #[test]
    fn graph_grants_name_graphs() {
        let p = parse(&format!(
            "{PREFIXES}
            ex:Contractors a sg:Policy ; sg:group \"contractor-x\" ;
              sg:readGraph ex:release, <urn:x-arq:DefaultGraph> ."
        ))
        .unwrap();
        assert!(!p[0].read);
        let mut graphs = p[0].read_graphs.clone();
        graphs.sort();
        assert_eq!(graphs, ["http://example.org/release", DEFAULT_GRAPH]);
        let p = parse(&format!(
            "{PREFIXES} ex:Editors a sg:Policy ; sg:group \"editors\" ; sg:writeGraph ex:wip ."
        ))
        .unwrap();
        assert!(!p[0].write);
        assert_eq!(p[0].write_graphs, ["http://example.org/wip"]);
        // Writers read what they write.
        assert_eq!(p[0].read_graphs, ["http://example.org/wip"]);
    }

    #[test]
    fn rules_it_cannot_enforce_fail_to_load() {
        #[cfg(feature = "enterprise")]
        tripletrail_enterprise::rows::install();
        for (body, expected) in [
            ("sg:agent sg:Anyone ; sg:readGraph \"g\"", "must be an IRI"),
            ("sg:agent sg:Anyone ; sg:visibleIf ex:Shape", "no shape"),
            ("sg:agent sg:Anyone ; sg:editableIf ex:Shape", "no shape"),
            (
                "sg:agent sg:Anyone ; sg:editableIf \"x\"",
                "must name a shape",
            ),
            (
                "sg:agent sg:Anyone ; sg:visibleIf \"x\"",
                "must name a shape",
            ),
            (
                "sg:agent sg:Anyone ; sg:readEverything true",
                "unknown property",
            ),
            ("sg:readAllGraphs true", "needs an sg:agent"),
            (
                "sg:agent sg:Anyone ; sg:readAllGraphs \"yes\"",
                "true or false",
            ),
        ] {
            // A row rule that names a shape is well formed; the open build
            // then refuses it before it looks for the shape, since compiling
            // row rules is an enterprise feature.
            let expected = if expected == "no shape" && !cfg!(feature = "enterprise") {
                tripletrail::NEEDS_ENTERPRISE
            } else {
                expected
            };
            let e = parse(&format!("{PREFIXES} ex:P a sg:Policy ; {body} .")).unwrap_err();
            assert!(e.contains(expected), "{body}: {e}");
        }
    }
}
