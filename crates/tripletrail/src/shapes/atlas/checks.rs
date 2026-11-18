//! The requirement checks of RINF Atlas's views as facets computed from the data
//! (`tts:select`), which the map counts, filters on and colours by.
//!
//! A check compares the values of some facets of a feature (`atlas:compareTo`, with the
//! values `atlas:impliedValue` adds) with a constraint, and gives `atlas:Meets`,
//! `atlas:DoesNotMeet`, or `atlas:whenMissing` (default `atlas:NoData`) for a feature without
//! a value; a feature it does not apply to (`atlas:appliesWhen`) gets none. The checked
//! classes (`atlas:checkClass`) get each check, and the summary (the worst result of the
//! checks that apply) when there are several; the section class (`atlas:checkSection`) gets
//! each of them as the best result of its parts, the features of the checked classes that
//! take their geometry from it (`atlas:geometryFrom`). RINF Atlas works them out after each
//! index update; TripleTrail when the map asks, in SPARQL.

use super::{ATLAS, atlas, list_items, properties};
use crate::shapes::{Reader, sh, tts};
use oxigraph::model::vocab::{rdf, rdfs};
use oxigraph::model::{
    BlankNode, Graph, Literal, NamedNodeRef, NamedOrBlankNode, NamedOrBlankNodeRef, Term, TermRef,
    TripleRef,
};

/// Where the computed facets go in the map's lists: after the facets of the data.
const ORDER: i64 = 100_000;

fn result(local: &str) -> String {
    format!("<{ATLAS}{local}>")
}

/// The keys of every view's checks and summaries.
pub(super) fn keys(shapes: &Graph) -> Vec<Term> {
    let mut keys: Vec<Term> = shapes
        .triples_for_predicate(&atlas("check"))
        .map(|t| t.object.into_owned())
        .collect();
    let views: Vec<NamedOrBlankNode> = shapes
        .triples_for_predicate(&atlas("view"))
        .map(|t| t.subject.into_owned())
        .collect();
    for view in views {
        if let Some((key, _)) = summary(shapes, view.as_ref()) {
            keys.push(key);
        }
    }
    keys
}

/// The checks of `view`: its property shapes with `atlas:check`.
/// By `sh:order`, then key.
fn checks_of(shapes: &Graph, view: NamedOrBlankNodeRef<'_>) -> Vec<NamedOrBlankNode> {
    let mut checks: Vec<(i64, String, NamedOrBlankNode)> = properties(shapes, view)
        .into_iter()
        .filter_map(|p| {
            let key = shapes
                .object_for_subject_predicate(&p, &atlas("check"))?
                .to_string();
            let order = shapes
                .object_for_subject_predicate(&p, &sh("order"))
                .and_then(number)
                .map_or(i64::MAX, |n| n as i64);
            Some((order, key, p))
        })
        .collect();
    checks.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    checks.into_iter().map(|(_, _, p)| p).collect()
}

/// The key and label of `view`'s summary: `atlas:summaryCheck`, by default `{view}.all`,
/// when it has two checks or more.
fn summary(shapes: &Graph, view: NamedOrBlankNodeRef<'_>) -> Option<(Term, String)> {
    if checks_of(shapes, view).len() < 2 {
        return None;
    }
    let key = match shapes.object_for_subject_predicate(view, &atlas("summaryCheck")) {
        Some(k) => k.into_owned(),
        None => {
            let TermRef::Literal(name) =
                shapes.object_for_subject_predicate(view, &atlas("view"))?
            else {
                return None;
            };
            Literal::new_simple_literal(format!("{}.all", name.value())).into()
        }
    };
    let label = match shapes.object_for_subject_predicate(view, &atlas("summaryLabel")) {
        Some(TermRef::Literal(l)) => l.value().to_owned(),
        _ => "Meets every requirement".to_owned(),
    };
    Some((key, label))
}

/// A SHACL path in SPARQL.
fn sparql_path(shapes: &Graph, path: TermRef<'_>) -> Option<String> {
    match path {
        TermRef::NamedNode(n) => Some(format!("<{}>", n.as_str())),
        TermRef::BlankNode(b) => {
            let node: NamedOrBlankNodeRef<'_> = b.into();
            let one = |p: &str| shapes.object_for_subject_predicate(node, &sh(p));
            if let Some(inner) = one("inversePath") {
                return Some(format!("^({})", sparql_path(shapes, inner)?));
            }
            if let Some(list) = one("alternativePath") {
                let parts: Option<Vec<String>> = list_items(shapes, list)
                    .into_iter()
                    .map(|t| sparql_path(shapes, t))
                    .collect();
                return Some(format!("({})", parts?.join("|")));
            }
            for (p, suffix) in [
                ("zeroOrMorePath", "*"),
                ("oneOrMorePath", "+"),
                ("zeroOrOnePath", "?"),
            ] {
                if let Some(inner) = one(p) {
                    return Some(format!("({}){suffix}", sparql_path(shapes, inner)?));
                }
            }
            let parts: Option<Vec<String>> = list_items(shapes, path)
                .into_iter()
                .map(|t| sparql_path(shapes, t))
                .collect();
            let parts = parts?;
            (!parts.is_empty()).then(|| parts.join("/"))
        }
        _ => None,
    }
}

/// A class of the set: its node shape, target class, and the SPARQL path of each facet key
/// (its own facets, then the shared ones).
struct Class {
    node: NamedOrBlankNode,
    target: String,
    facets: Vec<(Term, String)>,
}

impl Class {
    fn of(shapes: &Graph, nodes: &[NamedOrBlankNode], key: TermRef<'_>) -> Option<Class> {
        let node = nodes
            .iter()
            .find(|n| shapes.object_for_subject_predicate(*n, &atlas("key")) == Some(key))?;
        let target = match shapes.object_for_subject_predicate(node, &sh("targetClass"))? {
            TermRef::NamedNode(c) => format!("<{}>", c.as_str()),
            _ => return None,
        };
        let shared = nodes.iter().filter(|n| {
            matches!(shapes.object_for_subject_predicate(*n, &atlas("shared")),
                Some(TermRef::Literal(l)) if l.value() == "true")
        });
        let mut facets = Vec::new();
        for p in std::iter::once(node)
            .chain(shared)
            .flat_map(|n| properties(shapes, n.as_ref()))
        {
            if let (Some(k), Some(path)) = (
                shapes.object_for_subject_predicate(&p, &atlas("facet")),
                shapes
                    .object_for_subject_predicate(&p, &sh("path"))
                    .and_then(|path| sparql_path(shapes, path)),
            ) && !facets.iter().any(|(o, _): &(Term, String)| o.as_ref() == k)
            {
                facets.push((k.into_owned(), path));
            }
        }
        Some(Class {
            node: node.clone(),
            target,
            facets,
        })
    }

    fn path(&self, key: TermRef<'_>) -> Option<&str> {
        self.facets
            .iter()
            .find(|(k, _)| k.as_ref() == key)
            .map(|(_, p)| p.as_str())
    }
}

/// The SPARQL terms of a list (`sh:in`, `atlas:notIn`) or of every object of `p`
/// (`sh:hasValue`).
fn values(shapes: &Graph, node: NamedOrBlankNodeRef<'_>, p: &str) -> Vec<String> {
    let predicate = if p.starts_with("notIn") {
        atlas(p)
    } else {
        sh(p)
    };
    if p == "hasValue" {
        let mut all: Vec<String> = shapes
            .objects_for_subject_predicate(node, &predicate)
            .map(|v| v.to_string())
            .collect();
        all.sort();
        return all;
    }
    shapes
        .object_for_subject_predicate(node, &predicate)
        .map(|list| {
            list_items(shapes, list)
                .into_iter()
                .map(|v| v.to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// A literal's number, if it is one.
fn number(term: TermRef<'_>) -> Option<f64> {
    match term {
        TermRef::Literal(l) => l.value().trim().parse().ok(),
        _ => None,
    }
}

/// Check `check`'s result for the feature `this` of `class`, as a SPARQL expression: a
/// result IRI, or an error (no value) when it does not apply. `None` when the class has none
/// of the compared facets.
fn expression(
    shapes: &Graph,
    check: NamedOrBlankNodeRef<'_>,
    class: &Class,
    this: &str,
    note: &mut impl FnMut(String),
) -> Option<Check> {
    // The compared values, pooled as ?x.
    let mut pool: Vec<String> = shapes
        .objects_for_subject_predicate(check, &atlas("compareTo"))
        .filter_map(|k| class.path(k))
        .map(|path| format!("{{ {this} {path} ?x }}"))
        .collect();
    if pool.is_empty() {
        return None;
    }
    // Sorted, as all parts below: blank nodes come in no fixed order, and a pack built twice
    // from the same shapes must be the same.
    pool.sort();
    let compared = pool.len();
    for implied in shapes.objects_for_subject_predicate(check, &atlas("impliedValue")) {
        let Some(implied) = super::as_subject(implied) else {
            continue;
        };
        let one = |p: &str| shapes.object_for_subject_predicate(implied, &atlas(p));
        match (
            one("facet").and_then(|k| class.path(k)),
            one("when"),
            one("value"),
        ) {
            (Some(path), Some(when), Some(value)) => {
                pool.push(format!("{{ {this} {path} {when} BIND({value} AS ?x) }}"));
            }
            _ => note("atlas:impliedValue on a facet the checked class lacks".into()),
        }
    }
    pool[compared..].sort();
    let pool = pool.join(" UNION ");
    let has = |filter: &str| format!("EXISTS {{ {pool} FILTER({filter}) }}");
    let missing = match shapes.object_for_subject_predicate(check, &atlas("whenMissing")) {
        Some(TermRef::NamedNode(n)) if n.as_str() == format!("{ATLAS}DoesNotMeet") => {
            result("DoesNotMeet")
        }
        _ => result("NoData"),
    };
    let (meets, fails, no_data) = (result("Meets"), result("DoesNotMeet"), result("NoData"));
    let bounds: Vec<(&str, f64)> = [
        ("minInclusive", "<"),
        ("minExclusive", "<="),
        ("maxInclusive", ">"),
        ("maxExclusive", ">="),
    ]
    .into_iter()
    .filter_map(|(p, breaks)| {
        number(shapes.object_for_subject_predicate(check, &sh(p))?).map(|b| (breaks, b))
    })
    .collect();
    let mut terms: Vec<(String, f64)> = shapes
        .objects_for_subject_predicate(check, &atlas("termValue"))
        .filter_map(super::as_subject)
        .filter_map(|t| {
            let term = shapes.object_for_subject_predicate(t, &atlas("term"))?;
            let n = number(shapes.object_for_subject_predicate(t, &atlas("value"))?)?;
            Some((term.to_string(), n))
        })
        .collect();
    terms.sort_by(|a, b| a.0.cmp(&b.0));
    let list = |items: &[String]| items.join(", ");
    let value = if !bounds.is_empty() && !terms.is_empty() {
        // Concepts standing for a number: meets when one of them satisfies the bounds.
        let breaks = |n: f64| {
            bounds.iter().any(|(op, b)| match *op {
                "<" => n < *b,
                "<=" => n <= *b,
                ">" => n > *b,
                _ => n >= *b,
            })
        };
        let ok: Vec<String> = terms
            .iter()
            .filter(|(_, n)| !breaks(*n))
            .map(|(t, _)| t.clone())
            .collect();
        let all: Vec<String> = terms.iter().map(|(t, _)| t.clone()).collect();
        let meets_test = if ok.is_empty() {
            "false".to_owned()
        } else {
            has(&format!("?x IN ({})", list(&ok)))
        };
        format!(
            "IF(!EXISTS {{ {pool} }}, {missing}, IF({meets_test}, {meets}, IF({}, {fails}, {no_data})))",
            has(&format!("?x IN ({})", list(&all)))
        )
    } else if !bounds.is_empty() {
        // Numbers: meets when none breaks a bound.
        let broken: Vec<String> = bounds
            .iter()
            .map(|(op, b)| format!("?x {op} {b:?}"))
            .collect();
        format!(
            "IF(!{}, {missing}, IF({}, {fails}, {meets}))",
            has("isNumeric(?x)"),
            has(&format!("isNumeric(?x) && ({})", broken.join(" || ")))
        )
    } else {
        let any_of = values(shapes, check, "hasValue");
        let every_in = values(shapes, check, "in");
        let none_of = values(shapes, check, "notIn");
        let meets_test = if !every_in.is_empty() {
            format!("!{}", has(&format!("?x NOT IN ({})", list(&every_in))))
        } else if !none_of.is_empty() {
            has(&format!("?x NOT IN ({})", list(&none_of)))
        } else if !any_of.is_empty() {
            has(&format!("?x IN ({})", list(&any_of)))
        } else {
            note("requirement checks without a constraint".into());
            return None;
        };
        format!("IF(!EXISTS {{ {pool} }}, {missing}, IF({meets_test}, {meets}, {fails}))")
    };
    // Where it applies: one of the values of each `atlas:appliesWhen`.
    let mut applies = Vec::new();
    for when in shapes.objects_for_subject_predicate(check, &atlas("appliesWhen")) {
        let Some(when) = super::as_subject(when) else {
            continue;
        };
        let Some(path) = shapes
            .object_for_subject_predicate(when, &atlas("facet"))
            .and_then(|k| class.path(k))
        else {
            note("atlas:appliesWhen on a facet the checked class lacks".into());
            return None;
        };
        let mut allowed = values(shapes, when, "in");
        allowed.extend(values(shapes, when, "hasValue"));
        applies.push(format!(
            "EXISTS {{ {this} {path} ?a FILTER(?a IN ({})) }}",
            list(&allowed)
        ));
    }
    applies.sort();
    Some(if applies.is_empty() {
        Check {
            expression: value,
            applies: None,
        }
    } else {
        let applies = applies.join(" && ");
        Check {
            expression: format!("IF({applies}, {value}, ?none)"),
            applies: Some(applies),
        }
    })
}

/// A check's result for a feature: `expression`, unbound where it does not apply, which is
/// where `applies` (when it has one) is false.
#[derive(Clone)]
struct Check {
    expression: String,
    applies: Option<String>,
}

/// The worst (`worst`) or best result of `results` (expressions that may be errors).
fn pick(results: &[String], worst: bool) -> String {
    let order = if worst {
        ["DoesNotMeet", "NoData", "Meets"]
    } else {
        ["Meets", "NoData", "DoesNotMeet"]
    };
    let mut out = "?none".to_owned();
    for r in order.iter().rev() {
        let r = result(r);
        let any: Vec<String> = results
            .iter()
            .map(|e| format!("COALESCE({e} = {r}, false)"))
            .collect();
        out = format!("IF({}, {r}, {out})", any.join(" || "));
    }
    out
}

/// A computed facet on `node`: `key`, its label and comment, and `select`, the pattern that
/// binds its value for `$this` as `?value` (see [`value_of`] and [`best_of_parts`]).
#[allow(clippy::too_many_arguments)]
fn add_facet(
    out: &mut Graph,
    node: &NamedOrBlankNode,
    key: TermRef<'_>,
    label: &str,
    comment: Option<String>,
    order: i64,
    select: &str,
) {
    let p = BlankNode::default();
    out.insert(TripleRef::new(node, &sh("property"), &p));
    out.insert(TripleRef::new(&p, &sh("path"), rdf::TYPE));
    out.insert(TripleRef::new(&p, &tts("key"), key));
    out.insert(TripleRef::new(&p, &sh("name"), &Literal::from(label)));
    if let Some(comment) = comment {
        out.insert(TripleRef::new(
            &p,
            &sh("description"),
            &Literal::from(comment),
        ));
    }
    out.insert(TripleRef::new(&p, &tts("facet"), &tts("Facet-Values")));
    out.insert(TripleRef::new(&p, &tts("onlyFor"), &Reader::Map.iri()));
    out.insert(TripleRef::new(
        &p,
        &sh("order"),
        &Literal::from(ORDER + order),
    ));
    out.insert(TripleRef::new(&p, &tts("select"), &Literal::from(select)));
}

/// The pattern binding `expression` as the facet's value.
fn value_of(expression: &str) -> String {
    format!("BIND({expression} AS ?value) FILTER(BOUND(?value))")
}

/// The worst result of the checks `results` of one feature: each check is bound once, as
/// `?{prefix}{i}`, then compared (the checks repeated in each comparison made a summary of
/// eight checks too slow for all of RINF, #945). Answers the binds and the expression.
/// When every check has a condition, the features none of them applies to are left out first,
/// so most of RINF's tracks (not on the TEN-T network) are not checked eight times for nothing.
fn worst_of(results: &[Check], prefix: &str) -> (String, String) {
    let mut binds: Vec<String> = Vec::new();
    let mut conditions: Vec<String> = Vec::new();
    for r in results {
        match &r.applies {
            Some(a) => conditions.push(format!("({a})")),
            None => {
                conditions.clear();
                break;
            }
        }
    }
    if !conditions.is_empty() {
        conditions.sort();
        conditions.dedup();
        binds.push(format!("FILTER({})", conditions.join(" || ")));
    }
    binds.extend(
        results
            .iter()
            .enumerate()
            .map(|(i, e)| format!("BIND({} AS ?{prefix}{i})", e.expression)),
    );
    let vars: Vec<String> = (0..results.len())
        .map(|i| format!("?{prefix}{i}"))
        .collect();
    (binds.join(" "), pick(&vars, true))
}

/// The best result over the parts of a section: per part class, the binds and the result
/// expression of a `?part` that takes its geometry from `$this` through `from`. Looked for per
/// section, best result first, so the map's tiles (a few sections each) check only their own
/// sections' parts; each part's checks are bound once per look (#945).
fn best_of_parts(parts: &[(&str, &str, String, String)]) -> String {
    let mut out = "?none".to_owned();
    for r in ["DoesNotMeet", "NoData", "Meets"] {
        let r = result(r);
        let branches: Vec<String> = parts
            .iter()
            .map(|(from, class, binds, e)| {
                format!("{{ ?part {from} $this . ?part a {class} . {binds} FILTER(COALESCE({e} = {r}, false)) }}")
            })
            .collect();
        out = format!("IF(EXISTS {{ {} }}, {r}, {out})", branches.join(" UNION "));
    }
    value_of(&out)
}

/// Writes the checks of `view` as computed facets of its checked classes and section class.
pub(super) fn write(
    shapes: &Graph,
    out: &mut Graph,
    view: NamedOrBlankNodeRef<'_>,
    nodes: &[NamedOrBlankNode],
    note: &mut impl FnMut(String),
) {
    let checks = checks_of(shapes, view);
    if checks.is_empty() {
        return;
    }
    let mut classes: Vec<Class> = shapes
        .objects_for_subject_predicate(view, &atlas("checkClass"))
        .filter_map(|k| {
            let class = Class::of(shapes, nodes, k);
            if class.is_none() {
                note("atlas:checkClass naming no class of the set".into());
            }
            class
        })
        .collect();
    classes.sort_by(|a, b| a.target.cmp(&b.target));
    let summary = summary(shapes, view);
    let text = |p: NamedOrBlankNodeRef<'_>, predicate: NamedNodeRef<'_>| match shapes
        .object_for_subject_predicate(p, predicate)
    {
        Some(TermRef::Literal(l)) => Some(l.value().to_owned()),
        _ => None,
    };
    let order = |p: NamedOrBlankNodeRef<'_>| {
        shapes
            .object_for_subject_predicate(p, &sh("order"))
            .and_then(number)
            .map_or(0, |n| n as i64)
    };
    let deadline = atlas("deadline");
    let comment = |p: NamedOrBlankNodeRef<'_>| {
        let comment = text(p, rdfs::COMMENT);
        let deadline = text(p, deadline.as_ref());
        match (comment, deadline) {
            (Some(c), Some(d)) => Some(format!("{c} Deadline: {d}.")),
            (c, None) => c,
            (None, Some(d)) => Some(format!("Deadline: {d}.")),
        }
    };
    // Each check's expression for a feature `this` of each checked class.
    let of_class =
        |class: &Class, this: &str, note: &mut dyn FnMut(String)| -> Vec<(usize, Check)> {
            let mut note = |s: String| note(s);
            checks
                .iter()
                .enumerate()
                .filter_map(|(i, c)| {
                    expression(shapes, c.as_ref(), class, this, &mut note).map(|e| (i, e))
                })
                .collect()
        };
    for class in &classes {
        let found = of_class(class, "$this", &mut *note);
        for (i, e) in &found {
            let c = checks[*i].as_ref();
            let key = shapes
                .object_for_subject_predicate(c, &atlas("check"))
                .expect("a check");
            let label = text(c, rdfs::LABEL).unwrap_or_else(|| key.to_string());
            add_facet(
                out,
                &class.node,
                key,
                &label,
                comment(c),
                order(c),
                &value_of(&e.expression),
            );
        }
        if let Some((key, label)) = &summary
            && found.len() > 1
        {
            let all: Vec<Check> = found.iter().map(|(_, e)| e.clone()).collect();
            let (binds, worst) = worst_of(&all, "check");
            add_facet(
                out,
                &class.node,
                key.as_ref(),
                label,
                None,
                -1,
                &format!("{binds} {}", value_of(&worst)),
            );
        }
    }
    // The section class: the best result of its parts.
    let Some(section_key) = shapes.object_for_subject_predicate(view, &atlas("checkSection"))
    else {
        return;
    };
    let Some(section) = Class::of(shapes, nodes, section_key) else {
        note("atlas:checkSection naming no class of the set".into());
        return;
    };
    // The parts of `$this`: each checked class's features that take their geometry from it.
    let parts: Vec<(&Class, String)> = classes
        .iter()
        .filter_map(|class| {
            let from = shapes.object_for_subject_predicate(&class.node, &atlas("geometryFrom"))?;
            Some((class, sparql_path(shapes, from)?))
        })
        .collect();
    if parts.is_empty() {
        note("atlas:checkSection without parts (atlas:geometryFrom)".into());
        return;
    }
    // Per part class with a result: the section's pattern over them.
    let best = |per_part: &dyn Fn(&Class) -> Option<(String, String)>| -> Option<String> {
        let found: Vec<(&str, &str, String, String)> = parts
            .iter()
            .filter_map(|(class, from)| {
                let (binds, e) = per_part(class)?;
                Some((from.as_str(), class.target.as_str(), binds, e))
            })
            .collect();
        (!found.is_empty()).then(|| best_of_parts(&found))
    };
    let mut any = 0;
    for (i, c) in checks.iter().enumerate() {
        let c = c.as_ref();
        let per_part = |class: &Class| -> Option<(String, String)> {
            let mut ignore = |_: String| {};
            expression(shapes, checks[i].as_ref(), class, "?part", &mut ignore).map(|e| {
                let filter = e
                    .applies
                    .map(|a| format!("FILTER({a})"))
                    .unwrap_or_default();
                (filter, e.expression)
            })
        };
        let Some(e) = best(&per_part) else {
            continue;
        };
        any += 1;
        let key = shapes
            .object_for_subject_predicate(c, &atlas("check"))
            .expect("a check");
        let label = text(c, rdfs::LABEL).unwrap_or_else(|| key.to_string());
        add_facet(out, &section.node, key, &label, comment(c), order(c), &e);
    }
    if let Some((key, label)) = &summary
        && any > 1
    {
        let per_part = |class: &Class| -> Option<(String, String)> {
            let all: Vec<Check> = of_class(class, "?part", &mut |_| {})
                .into_iter()
                .map(|(_, e)| e)
                .collect();
            (all.len() > 1).then(|| worst_of(&all, "check"))
        };
        if let Some(e) = best(&per_part) {
            add_facet(out, &section.node, key.as_ref(), label, None, -1, &e);
        }
    }
}
