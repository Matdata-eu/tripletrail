//! What a mapping does, readably: its logical views and triples maps with the records each
//! read and the quads each made ([`explain`]).

use crate::RmlError;
use crate::mapping::{
    Field, FieldKind, Formulation, LogicalSource, Mapping, ObjectMap, Part, SourceRef, TermMap,
    TermType, Value,
};
use crate::run::{Loader, Plan};
use std::fmt::{self, Write as _};
use std::time::Instant;

/// A mapping's plan with what running it read and made, from [`explain`]. Its `Display` is
/// a text report.
#[derive(Debug, Clone)]
pub struct Explanation {
    pub views: Vec<ViewReport>,
    pub maps: Vec<MapReport>,
    /// Reading the sources, building the views and the join indexes.
    pub prepare_seconds: f64,
}

#[derive(Debug, Clone)]
pub struct ViewReport {
    /// The view, its source and its fields, as text.
    pub description: String,
    pub records: usize,
}

#[derive(Debug, Clone)]
pub struct MapReport {
    /// The triples map and its term maps, as text.
    pub description: String,
    pub records: usize,
    /// Quads made, duplicates included.
    pub quads: usize,
    pub seconds: f64,
}

/// Runs `mapping` without keeping its quads, counting per view and triples map the records
/// read and the quads made.
pub fn explain(mapping: &Mapping, loader: &dyn Loader) -> Result<Explanation, RmlError> {
    let started = Instant::now();
    let plan = Plan::new(mapping, loader)?;
    let prepare_seconds = started.elapsed().as_secs_f64();
    let views = mapping
        .views
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let mut d = format!("logical view {}\n  on {}\n", v.node, source(mapping, &v.on));
            fields(&mut d, &v.fields, "", 2);
            for j in &v.joins {
                let _ = writeln!(
                    d,
                    "  {} join with {} on {}",
                    if j.inner { "inner" } else { "left" },
                    mapping.views[j.parent].node,
                    conditions(&j.conditions)
                );
                fields(&mut d, &j.fields, "", 4);
            }
            ViewReport {
                description: d,
                records: plan.view_records(i),
            }
        })
        .collect();
    let mut maps = Vec::new();
    for (t, tm) in mapping.triples_maps.iter().enumerate() {
        let started = Instant::now();
        let quads = plan.run_map(t, &mut |_, _| {})?;
        let mut d = format!(
            "triples map {}\n  source {}\n  subject {}\n",
            tm.node,
            source(mapping, &tm.source),
            match &tm.quoted_subject {
                Some(q) => format!(
                    "the triples of {}, quoted",
                    mapping.triples_maps[q.map].node
                ),
                None => term_map(&tm.subject),
            }
        );
        for c in &tm.classes {
            let _ = writeln!(d, "  class {c}");
        }
        for g in &tm.graphs {
            let _ = writeln!(d, "  graph {}", term_map(g));
        }
        for pom in &tm.predicate_objects {
            let predicates: Vec<String> = pom.predicates.iter().map(term_map).collect();
            for o in &pom.objects {
                let object = match o {
                    ObjectMap::Term(m) => term_map(m),
                    ObjectMap::Join {
                        parent,
                        conditions: c,
                    } if c.is_empty() => format!(
                        "the subject of {} (same record)",
                        mapping.triples_maps[*parent].node
                    ),
                    ObjectMap::Join {
                        parent,
                        conditions: c,
                    } => format!(
                        "the subjects of {} joined on {} (hash join)",
                        mapping.triples_maps[*parent].node,
                        conditions(c)
                    ),
                    ObjectMap::Quoted(q) if q.conditions.is_empty() => format!(
                        "the triples of {} (same record), quoted",
                        mapping.triples_maps[q.map].node
                    ),
                    ObjectMap::Quoted(q) => format!(
                        "the triples of {} joined on {}, quoted",
                        mapping.triples_maps[q.map].node,
                        conditions(&q.conditions)
                    ),
                };
                let _ = writeln!(d, "  {} -> {object}", predicates.join(", "));
            }
            for g in &pom.graphs {
                let _ = writeln!(d, "    graph {}", term_map(g));
            }
        }
        maps.push(MapReport {
            description: d,
            records: plan.records_of(&tm.source),
            quads,
            seconds: started.elapsed().as_secs_f64(),
        });
    }
    Ok(Explanation {
        views,
        maps,
        prepare_seconds,
    })
}

impl fmt::Display for Explanation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "sources, views and join indexes ready in {:.3} s",
            self.prepare_seconds
        )?;
        for v in &self.views {
            write!(f, "\n{}", v.description)?;
            writeln!(f, "  => {} records", v.records)?;
        }
        for m in &self.maps {
            write!(f, "\n{}", m.description)?;
            writeln!(
                f,
                "  => {} records, {} quads in {:.3} s",
                m.records, m.quads, m.seconds
            )?;
        }
        Ok(())
    }
}

fn source(mapping: &Mapping, s: &LogicalSource) -> String {
    let what = match &s.source {
        SourceRef::Path(p) => format!("file {p}"),
        SourceRef::Table(t) => format!("table {t}"),
        SourceRef::Query(q) => format!("query {q}"),
        SourceRef::View(v) => return format!("logical view {}", mapping.views[*v].node),
        SourceRef::Empty => return "no logical source (one empty record)".to_owned(),
    };
    let formulation = match &s.formulation {
        Formulation::JsonPath => "JSONPath".to_owned(),
        Formulation::Csv => "CSV".to_owned(),
        Formulation::XPath => "XPath".to_owned(),
        Formulation::Sql => "SQL".to_owned(),
        Formulation::View => "view".to_owned(),
        Formulation::Other(o) => format!("<{o}>"),
    };
    match &s.iterator {
        Some(i) => format!("{what} ({formulation}, iterator {i})"),
        None => format!("{what} ({formulation})"),
    }
}

fn fields(out: &mut String, fields: &[Field], prefix: &str, indent: usize) {
    for f in fields {
        let name = crate::mapping::absolute(prefix, &f.name);
        let what = match &f.kind {
            FieldKind::Expression(m) => term_map(m),
            FieldKind::Iterable {
                formulation,
                iterator,
            } => {
                let mut s = String::from("records");
                if let Some(i) = iterator {
                    let _ = write!(s, " of iterator {i}");
                }
                if let Some(f) = formulation {
                    let _ = write!(s, " read as {f:?}");
                }
                s
            }
        };
        let _ = writeln!(out, "{:indent$}field {name} = {what}", "");
        self::fields(out, &f.fields, &name, indent + 2);
    }
}

fn conditions(c: &[(TermMap, TermMap)]) -> String {
    c.iter()
        .map(|(child, parent)| format!("{} = {}", term_map(child), term_map(parent)))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// A term map as text: `template "http://ex.org/{id}"`, `reference "name" (literal)`.
fn term_map(m: &TermMap) -> String {
    let mut s = match &m.value {
        Value::Constant(c) => return c.to_string(),
        Value::Reference(r) => format!("reference {r:?}"),
        Value::Template(parts) => {
            let mut t = String::new();
            for p in parts {
                match p {
                    Part::Text(x) => {
                        for c in x.chars() {
                            if matches!(c, '{' | '}' | '\\') {
                                t.push('\\');
                            }
                            t.push(c);
                        }
                    }
                    Part::Reference(r) => {
                        let _ = write!(t, "{{{r}}}");
                    }
                }
            }
            format!("template {t:?}")
        }
        Value::Fresh => "a new blank node per record".to_owned(),
        Value::Function(call) => {
            let inputs: Vec<String> = call
                .inputs
                .iter()
                .map(|(p, m)| format!("{p}: {}", term_map(m)))
                .collect();
            format!("function {}({})", call.function, inputs.join(", "))
        }
    };
    s.push_str(match m.term_type {
        TermType::Iri => " (IRI)",
        TermType::BlankNode => " (blank node)",
        TermType::Literal => " (literal)",
    });
    if let Some(d) = &m.datatype {
        let _ = write!(s, " ^^{d}");
    }
    if let Some(l) = &m.language {
        let _ = write!(s, " @{l}");
    }
    if let Some(d) = &m.datatype_map {
        let _ = write!(s, " ^^[{}]", term_map(d));
    }
    if let Some(l) = &m.language_map {
        let _ = write!(s, " @[{}]", term_map(l));
    }
    s
}
