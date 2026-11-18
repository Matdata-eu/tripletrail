//! A `pipeline.yaml` as code to run elsewhere (#405): an Airflow DAG that runs its steps with
//! `tripletrail pipeline run`, or a Python script that runs them in process with `tripletrail.Engine`.

use crate::pipeline::{Kind, Pipeline, Severity, Step};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

/// Where the generated code finds things.
pub struct Target<'a> {
    /// The `pipeline.yaml`, as the generated code should name it.
    pub file: &'a str,
    /// The dataset directory (`--location`), or `None` for a store in memory.
    pub location: Option<&'a str>,
    pub dataset: &'a str,
    /// Parameter values over the pipeline's defaults (the script binds them when made).
    pub values: &'a HashMap<String, String>,
}

/// A Python string literal of `s`.
fn py(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `s` on one line, for a Python comment: line breaks and other control characters become spaces.
fn comment(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || c == '\u{2028}' || c == '\u{2029}' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// `s` inside a `"""` docstring: one line, no backslashes or double quotes.
fn doc(s: &str) -> String {
    comment(s).replace('\\', "/").replace('"', "'")
}

/// A triple-quoted Python raw-ish literal for SPARQL text: a plain literal when it cannot be one.
fn py_text(s: &str) -> String {
    if !s.contains("\"\"\"") && !s.contains('\\') && !s.ends_with('"') {
        format!("\"\"\"{s}\"\"\"")
    } else {
        py(s)
    }
}

/// An Airflow task id for a step: letters, digits, `_`, `-` and `.`.
fn task_id(step: &Step, number: usize, seen: &mut Vec<String>) -> String {
    let mut id: String = step
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_-.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if step.name == format!("{number} {}", step.kind.name()) {
        id = format!("{}_{number}", step.kind.name());
    } else if id.is_empty() || id.starts_with(|c: char| !c.is_ascii_alphabetic()) {
        id = format!("step_{number}_{id}");
    }
    while seen.contains(&id) {
        id.push('_');
    }
    seen.push(id.clone());
    id
}

/// An Airflow DAG running the pipeline with `tripletrail pipeline run` on the dataset at
/// `target.location`: one task per step (each `--from N --to N`), or one task for all. The
/// pipeline's parameters are DAG params, passed with `-v`.
pub fn airflow(pipeline: &Pipeline, target: &Target<'_>, per_step: bool) -> String {
    let mut out = String::new();
    let dag_id: String = pipeline
        .name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "_-.".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let dag_id = if dag_id.is_empty() {
        "pipeline".to_owned()
    } else {
        dag_id
    };
    let _ = writeln!(
        out,
        "\"\"\"Runs the pipeline {} with `tripletrail pipeline run` (made by `tripletrail pipeline export`).

Each task runs on the same dataset directory, one after the other; a task fails when its step
fails or its validation gate stops it, and its log holds the step's JSON report.
\"\"\"

import shlex

import pendulum
from airflow import DAG

try:
    from airflow.providers.standard.operators.bash import BashOperator
except ImportError:  # Airflow 2
    from airflow.operators.bash import BashOperator

TRIPLETRAIL = \"tripletrail\"
PIPELINE = {}
LOCATION = {}
DATASET = {}
",
        doc(&pipeline.name),
        py(target.file),
        target.location.map_or("None".to_owned(), py),
        py(target.dataset),
    );
    let mut parameters: Vec<(&String, &String)> = pipeline.parameters.iter().collect();
    parameters.sort();
    out.push_str("PARAMS = {\n");
    for (k, v) in &parameters {
        let v = target.values.get(*k).unwrap_or(v);
        let _ = writeln!(out, "    {}: {},", py(k), py(v));
    }
    out.push_str("}\n\n");
    out.push_str(
        "
def command(first=None, last=None):
    words = [TRIPLETRAIL, \"pipeline\", \"run\", PIPELINE, \"--dataset\", DATASET]
    if LOCATION:
        words += [\"--location\", LOCATION]
    if first:
        words += [\"--from\", first]
    if last:
        words += [\"--to\", last]
    line = \" \".join(shlex.quote(w) for w in words)
    # The DAG's params, as given when it is triggered, through the environment (ENV).
    for i, name in enumerate(PARAMS):
        line += \" -v \" + shlex.quote(name + \"=\") + f'\"$TRIPLETRAIL_PARAM_{i}\"'
    return line


ENV = {f\"TRIPLETRAIL_PARAM_{i}\": \"{{ params[%r] }}\" % name for i, name in enumerate(PARAMS)}


",
    );
    let _ = writeln!(
        out,
        "with DAG(
    dag_id={},
    schedule=None,
    start_date=pendulum.datetime(2026, 1, 1, tz=\"UTC\"),
    catchup=False,
    params=PARAMS,
    max_active_runs=1,
    tags=[\"tripletrail\"],
) as dag:",
        py(&dag_id)
    );
    if per_step {
        let mut seen = Vec::new();
        let mut ids = Vec::new();
        for (i, step) in pipeline.steps.iter().enumerate() {
            let id = task_id(step, i + 1, &mut seen);
            let number = (i + 1).to_string();
            let _ = writeln!(
                out,
                "    t{number} = BashOperator(task_id={id}, bash_command=command({n}, {n}), env=ENV, append_env=True)  # {kind}",
                id = py(&id),
                n = py(&number),
                kind = step.kind.name(),
            );
            ids.push(format!("t{number}"));
        }
        if ids.len() > 1 {
            let _ = writeln!(out, "    {}", ids.join(" >> "));
        }
    } else {
        let _ = writeln!(
            out,
            "    run = BashOperator(task_id=\"pipeline\", bash_command=command(), env=ENV, append_env=True)"
        );
    }
    out
}

/// A Python script that runs the pipeline in process with `tripletrail.Engine`. Files are read
/// when it runs, except updates and queries that use parameters: those are bound now (with the
/// pipeline's defaults over `target.values`) and written into the script. Lift steps run
/// `tripletrail lift`; python steps need a served dataset, so the script stops at them.
pub fn python(pipeline: &Pipeline, target: &Target<'_>) -> String {
    let mut values = pipeline.parameters.clone();
    values.extend(target.values.clone());
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\"\"\"Runs the pipeline {} in process with tripletrail.Engine (made by `tripletrail pipeline export`).\"\"\"

import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import tripletrail
from tripletrail import NamedNode

HERE = Path({}).resolve().parent  # paths of the pipeline are relative to its folder
SHAPES = NamedNode(\"urn:x-tripletrail:pipeline:shapes\")
SEVERITY = {{\"Info\": 1, \"Warning\": 2, \"Violation\": 3}}


def path(p):
    return str(HERE / p)


def files(p, extensions):
    \"\"\"A file, or the files of a folder with these extensions, in name order.\"\"\"
    p = HERE / p
    if p.is_dir():
        return sorted(str(f) for f in p.rglob(\"*\") if f.is_file() and f.suffix.lstrip(\".\") in extensions)
    return [str(p)]


RDF = {{\"ttl\", \"nt\", \"trig\", \"nq\", \"rdf\", \"owl\", \"jsonld\", \"n3\", \"xml\", \"gz\"}}


def load_shapes(e, paths):
    if e.contains_named_graph(SHAPES):
        e.remove_graph(SHAPES)
    for p in paths:
        for f in files(p, RDF):
            e.load(path=f, to_graph=SHAPES)


def main():
    e = tripletrail.Engine({})",
        doc(&pipeline.name),
        py(target.file),
        target.location.map_or(String::new(), py),
    );
    if pipeline.union_default_graph {
        out.push_str("    e.union_default_graph = True\n");
    }
    for (i, step) in pipeline.steps.iter().enumerate() {
        let _ = writeln!(
            out,
            "\n    # Step {} ({}): {}",
            i + 1,
            step.kind.name(),
            comment(&step.name)
        );
        let graph = step
            .graph
            .as_ref()
            .map(|g| format!("NamedNode({})", py(g.as_str())));
        if step.clear
            && let Some(g) = &graph
        {
            let _ = writeln!(
                out,
                "    if e.contains_named_graph({g}):\n        e.remove_graph({g})"
            );
        }
        let to_graph = graph
            .as_ref()
            .map_or(String::new(), |g| format!(", to_graph={g}"));
        match &step.kind {
            Kind::Load { files } => {
                for f in files {
                    let _ = writeln!(
                        out,
                        "    for f in files({}, RDF):\n        e.load(path=f{to_graph})",
                        py(f)
                    );
                }
            }
            Kind::Update { files } => {
                for f in files {
                    let bound = bound_files(&pipeline.dir, f, &["ru"], &values);
                    match bound {
                        Some(texts) => {
                            for (name, text) in texts {
                                let _ = writeln!(
                                    out,
                                    "    e.update({})  # {}, with its parameters bound",
                                    py_text(&text),
                                    comment(&name)
                                );
                            }
                        }
                        None => {
                            let _ = writeln!(
                                out,
                                "    for f in files({}, {{\"ru\"}}):\n        e.update(Path(f).read_text())",
                                py(f)
                            );
                        }
                    }
                }
            }
            Kind::Lift { for_each, then, .. } if for_each.is_some() || !then.is_empty() => {
                let _ = writeln!(
                    out,
                    "    # A lift with for_each or then: run it as `tripletrail pipeline run {} --from {n} --to {n}`.
    raise SystemExit({})",
                    comment(target.file),
                    py(&format!("step {} ({}) lifts with for_each or then: see the comment above", i + 1, step.name)),
                    n = i + 1,
                );
            }
            Kind::Publish {
                location,
                graphs,
                merge,
            } if graphs.is_empty() && !*merge => {
                let _ = writeln!(
                    out,
                    "    new, old = path({l}) + \".new\", path({l}) + \".old\"
    there = os.listdir(path({l})) if os.path.isdir(path({l})) else []
    if there and not (\"CURRENT\" in there and any(n.startswith(\"checkpoint-\") for n in there)):
        raise SystemExit({l} + \" holds files and is not a dataset: not replacing it\")
    shutil.rmtree(new, ignore_errors=True)
    e.save(new)
    if os.path.exists(path({l})):
        os.replace(path({l}), old)
    os.replace(new, path({l}))
    shutil.rmtree(old, ignore_errors=True)",
                    l = py(location)
                );
            }
            Kind::Reason { .. } => {
                let _ =
                    writeln!(
                    out,
                    "    # A reason step: run it as `tripletrail pipeline run {} --from {n} --to {n}`.
    raise SystemExit({})",
                    comment(target.file),
                    py(&format!("step {} ({}) reasons: see the comment above", i + 1, step.name)),
                    n = i + 1,
                );
            }
            Kind::Publish { .. } => {
                let _ = writeln!(
                    out,
                    "    # A publish of some graphs: run it as `tripletrail pipeline run {} --from {n} --to {n}`.
    raise SystemExit({})",
                    comment(target.file),
                    py(&format!("step {} ({}) publishes some graphs: see the comment above", i + 1, step.name)),
                    n = i + 1,
                );
            }
            Kind::Lift {
                files, base, jobs, ..
            } => {
                let mut words = vec!["\"tripletrail\"".to_owned(), "\"lift\"".to_owned()];
                words.extend(files.iter().map(|f| format!("path({})", py(f))));
                words.push("\"--base\"".to_owned());
                words.push(match base {
                    Some(b) => format!("path({})", py(b)),
                    None => "str(HERE)".to_owned(),
                });
                if let Some(j) = jobs {
                    words.push(format!("\"--jobs\", \"{j}\""));
                }
                let mut names: Vec<&String> = values.keys().collect();
                names.sort();
                for k in names {
                    words.push(format!("\"-v\", {}", py(&format!("{k}={}", values[k]))));
                }
                let _ = writeln!(
                    out,
                    "    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, \"lift.nt\")
        subprocess.run([{}, \"--output\", out], check=True)
        e.load(path=out{to_graph})",
                    words.join(", ")
                );
            }
            Kind::Infer {
                files,
                data,
                max_passes,
                all_constructed,
                ordered,
            } => {
                let shapes: Vec<String> = files.iter().map(|f| py(f)).collect();
                let data = data
                    .as_ref()
                    .map(|g| format!("NamedNode({})", py(g.as_str())));
                let into = graph.clone().or_else(|| data.clone());
                let _ = writeln!(out, "    load_shapes(e, [{}])", shapes.join(", "));
                let mut args = vec!["SHAPES".to_owned()];
                if let Some(d) = &data {
                    args.push(format!("data_graph={d}"));
                }
                if *ordered {
                    args.push("ordered=True".to_owned());
                } else if *max_passes != 64 {
                    args.push(format!("max_iterations={max_passes}"));
                }
                if *all_constructed {
                    args.push("include_existing=True".to_owned());
                }
                match into {
                    Some(g) => {
                        args.push(format!("into={g}"));
                        let _ = writeln!(out, "    e.infer({})", args.join(", "));
                    }
                    None => {
                        let _ = writeln!(
                            out,
                            "    e.extend(tripletrail.Quad(t.subject, t.predicate, t.object) for t in e.infer({}))",
                            args.join(", ")
                        );
                    }
                }
                out.push_str("    e.remove_graph(SHAPES)\n");
            }
            Kind::Validate {
                files,
                data,
                fail_on,
                report,
            } => {
                let shapes: Vec<String> = files.iter().map(|f| py(f)).collect();
                let _ = writeln!(out, "    load_shapes(e, [{}])", shapes.join(", "));
                let data = data.as_ref().map_or(String::new(), |g| {
                    format!(", data_graph=NamedNode({})", py(g.as_str()))
                });
                let _ = writeln!(out, "    report = e.validate(SHAPES{data})");
                out.push_str("    e.remove_graph(SHAPES)\n");
                if let Some(r) = report {
                    let _ = writeln!(
                        out,
                        "    Path(path({r})).parent.mkdir(parents=True, exist_ok=True)\n    report.serialize(path({r}), {e})",
                        r = py(r),
                        e = py(&extension_of(r)),
                    );
                }
                if *fail_on != Severity::Never {
                    let level = match fail_on {
                        Severity::Info => 1,
                        Severity::Warning => 2,
                        _ => 3,
                    };
                    let _ = writeln!(
                        out,
                        "    stop = [r for r in report.results if SEVERITY.get(r[\"severity\"].value.rsplit(\"#\", 1)[-1], 3) >= {level}]
    if stop:
        raise SystemExit({} + f\": validation found {{len(stop)}} results\")",
                        py(&step.name)
                    );
                }
            }
            Kind::Export { file, query } => {
                let _ = writeln!(
                    out,
                    "    Path(path({f})).parent.mkdir(parents=True, exist_ok=True)",
                    f = py(file)
                );
                let extension = extension_of(file);
                let triples = !matches!(extension.as_str(), "nq" | "trig");
                let extension = py(&extension);
                match query {
                    Some(q) => {
                        let text = match bound_files(&pipeline.dir, q, &["rq"], &values) {
                            _ if crate::pipeline::is_inline_query(q) => py_text(q),
                            Some(mut t) if t.len() == 1 => py_text(&t.remove(0).1),
                            _ => format!("Path(path({})).read_text()", py(q)),
                        };
                        let _ = writeln!(
                            out,
                            "    e.query({text}).serialize(path({}), {extension})",
                            py(file)
                        );
                    }
                    None => {
                        let from = match &graph {
                            Some(g) => format!(", from_graph={g}"),
                            None if triples => ", from_graph=tripletrail.DefaultGraph()".to_owned(),
                            None => String::new(),
                        };
                        let _ = writeln!(out, "    e.dump(path({}), {extension}{from})", py(file));
                    }
                }
            }
            Kind::Python { target: t, .. } => {
                let _ = writeln!(
                    out,
                    "    # {} works on a dataset served over HTTP, which an Engine in process is not:
    # run this step with `tripletrail pipeline run {} --from {n} --to {n}` on the saved store.
    raise SystemExit({})",
                    comment(t),
                    comment(target.file),
                    py(&format!("step {} ({}) is a python step: see the comment above", i + 1, step.name)),
                    n = i + 1,
                );
            }
        }
    }
    out.push_str("\n\nif __name__ == \"__main__\":\n    main()\n");
    out
}

/// The extension of a file name, `nt` when it has none.
fn extension_of(file: &str) -> String {
    Path::new(file)
        .extension()
        .map_or_else(|| "nt".to_owned(), |e| e.to_string_lossy().into_owned())
}

/// The texts of the files (or the files of the folder) when parameters change them, bound;
/// `None` when they use none or cannot be read (then the script reads them when it runs).
fn bound_files(
    dir: &Path,
    path: &str,
    extensions: &[&str],
    values: &HashMap<String, String>,
) -> Option<Vec<(String, String)>> {
    if values.is_empty() {
        return None;
    }
    let full = dir.join(path);
    let files = crate::testing::files_of(&full, extensions).ok()?;
    let mut out = Vec::new();
    let mut changed = false;
    for f in files {
        let text = std::fs::read_to_string(&f).ok()?;
        let bound = crate::lift::bind_parameters(&text, values).ok()?;
        changed |= bound != text;
        let name = f.strip_prefix(dir).unwrap_or(&f).display().to_string();
        out.push((name, bound));
    }
    changed.then_some(out)
}
