//! A configuration file for `tripletrail serve` (#826): `--config tripletrail.yaml` (or
//! `TRIPLETRAIL_CONFIG`). Its keys are `serve`'s long options, and `datasets` describes the
//! datasets one by one:
//!
//! ```yaml
//! bind: 0.0.0.0:7878
//! location: /data
//! timeout: 30
//! datasets:
//!   - name: kg
//!     union-default-graph: true
//!     rdfs: true
//!   - name: rinf
//!     path: /snapshots/rinf
//!     read-only: true
//! ```
//!
//! The file is read as if its options had been given on the command line, after them: an
//! option given as a flag, or by its environment variable, wins over the file.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use yaml_rust2::{Yaml, YamlLoader};

/// One long option of `serve`: its name, whether it takes a value, its environment variable.
#[derive(Clone, Debug)]
pub struct Opt {
    pub long: String,
    /// Its short form (`-b`), if it has one.
    pub short: Option<char>,
    pub takes_value: bool,
    pub env: Option<String>,
}

/// The configuration file `argv` (`tripletrail serve …`) names with `--config`, or else
/// `TRIPLETRAIL_CONFIG` names; `None` for another command, or none named.
pub fn file_of(argv: &[OsString]) -> Option<PathBuf> {
    if argv.get(1).map(OsString::as_os_str) != Some(OsStr::new("serve")) {
        return None;
    }
    let mut args = argv.iter().skip(2);
    while let Some(a) = args.next() {
        let a = a.to_string_lossy();
        if a == "--" {
            break;
        }
        if a == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(path) = a.strip_prefix("--config=") {
            return Some(PathBuf::from(path));
        }
    }
    std::env::var_os("TRIPLETRAIL_CONFIG")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `argv` with the options of the configuration file `text` added after the command line's,
/// leaving out those the command line or the environment already sets. `options` are the
/// long options `serve` takes.
pub fn expand(argv: &[OsString], text: &str, options: &[Opt]) -> Result<Vec<OsString>, String> {
    let docs = YamlLoader::load_from_str(text).map_err(|e| e.to_string())?;
    let mut out: Vec<OsString> = argv.to_vec();
    let Some(doc) = docs.into_iter().next() else {
        return Ok(out);
    };
    let Yaml::Hash(map) = doc else {
        if matches!(doc, Yaml::Null) {
            return Ok(out);
        }
        return Err("the file must be a map of options, such as \"bind: 0.0.0.0:7878\"".into());
    };
    // Whether the command line gives `o`: `--long`, `--long=…`, or its short form, alone
    // (`-b`, `-bVALUE`) or in a group of flags (`-vb`).
    let given = |o: &Opt| {
        let mut args = argv.iter().skip(2);
        while let Some(a) = args.next() {
            let a = a.to_string_lossy();
            if a == "--" {
                break;
            }
            if let Some(long) = a.strip_prefix("--") {
                if long == o.long || long.starts_with(&format!("{}=", o.long)) {
                    return true;
                }
                // The value of a long option that takes one is not an option.
                if !long.contains('=') && options.iter().any(|x| x.long == long && x.takes_value) {
                    args.next();
                }
                continue;
            }
            let Some(shorts) = a.strip_prefix('-').filter(|s| !s.is_empty()) else {
                continue;
            };
            for (i, c) in shorts.char_indices() {
                if Some(c) == o.short {
                    return true;
                }
                if options.iter().any(|x| x.short == Some(c) && x.takes_value) {
                    // The rest is its value, or the next argument is.
                    if i + c.len_utf8() == shorts.len() {
                        args.next();
                    }
                    break;
                }
            }
        }
        false
    };
    let set = |o: &Opt| {
        given(o)
            || o.env
                .as_ref()
                .is_some_and(|e| std::env::var_os(e).is_some_and(|v| !v.is_empty()))
    };
    let find = |key: &str| -> Result<&Opt, String> {
        let key = key.replace('_', "-");
        options.iter().find(|o| o.long == key).ok_or_else(|| {
            format!("unknown option \"{key}\": the keys are serve's long options (see tripletrail serve --help) and datasets")
        })
    };
    let mut add = |o: &Opt, value: Option<String>| {
        out.push(format!("--{}", o.long).into());
        if let Some(v) = value {
            out.push(v.into());
        }
    };
    for (key, value) in &map {
        let Yaml::String(key) = key else {
            return Err(format!("a key must be a name, not {key:?}"));
        };
        if key == "datasets" {
            continue;
        }
        let o = find(key)?;
        if o.long == "config" {
            return Err("\"config\" can't name another file".into());
        }
        if set(o) {
            continue;
        }
        let values: Vec<&Yaml> = match value {
            Yaml::Array(items) => items.iter().collect(),
            v => vec![v],
        };
        for v in values {
            match (v, o.takes_value) {
                (Yaml::Boolean(true), false) => add(o, None),
                (Yaml::Boolean(false) | Yaml::Null, false) => {}
                (_, false) => {
                    return Err(format!("{key}: is true or false"));
                }
                (Yaml::Null, true) => {}
                (v, true) => add(o, Some(scalar(key, v)?)),
            }
        }
    }
    if let Some(datasets) = map.get(&Yaml::String("datasets".into())) {
        // Datasets named on the command line replace the file's.
        if !["dataset", "attach"]
            .iter()
            .any(|l| options.iter().find(|o| o.long == *l).is_some_and(&set))
        {
            let Yaml::Array(list) = datasets else {
                return Err("datasets: is a list, one map per dataset".into());
            };
            for d in list {
                for (long, value) in dataset(d)? {
                    let o = find(long)?;
                    add(o, Some(value));
                }
            }
        }
    }
    Ok(out)
}

/// A scalar value as the command line would give it.
fn scalar(key: &str, v: &Yaml) -> Result<String, String> {
    Ok(match v {
        Yaml::String(s) | Yaml::Real(s) => s.clone(),
        Yaml::Integer(i) => i.to_string(),
        Yaml::Boolean(b) => b.to_string(),
        _ => return Err(format!("{key}: a value or a list of values")),
    })
}

/// The options that make dataset `d` of the file.
fn dataset(d: &Yaml) -> Result<Vec<(&'static str, String)>, String> {
    let Yaml::Hash(m) = d else {
        return Err("datasets: each is a map with a name, such as \"- name: kg\"".into());
    };
    let get = |k: &str| {
        m.get(&Yaml::String(k.into()))
            .or_else(|| m.get(&Yaml::String(k.replace('-', "_"))))
    };
    for (k, _) in m {
        let ok = matches!(k, Yaml::String(k) if [
            "name", "path", "read-only", "read_only", "union-default-graph", "union_default_graph",
            "rdfs", "owl-rl", "owl_rl", "aliases", "alias", "text-properties", "text_properties",
        ].contains(&k.as_str()));
        if !ok {
            return Err(format!(
                "datasets: unknown key {k:?}; a dataset has name, path, read-only, union-default-graph, rdfs, owl-rl, aliases and text-properties"
            ));
        }
    }
    let Some(Yaml::String(name)) = get("name") else {
        return Err("datasets: each needs a name".into());
    };
    let flag = |k: &str| -> Result<bool, String> {
        match get(k) {
            None | Some(Yaml::Null) | Some(Yaml::Boolean(false)) => Ok(false),
            Some(Yaml::Boolean(true)) => Ok(true),
            Some(_) => Err(format!("datasets: {name}: {k} is true or false")),
        }
    };
    let mut out = Vec::new();
    let read_only = flag("read-only")?;
    match get("path") {
        Some(Yaml::String(path)) => out.push((
            "attach",
            format!("{name}={path}{}", if read_only { ":ro" } else { "" }),
        )),
        None => {
            if read_only {
                return Err(format!(
                    "datasets: {name}: read-only needs a path; the datasets of location are read-only with \"read-only: true\" at the top"
                ));
            }
            out.push(("dataset", name.clone()));
        }
        Some(_) => return Err(format!("datasets: {name}: path is a directory")),
    }
    if flag("union-default-graph")? {
        out.push(("union-default-graph", name.clone()));
    }
    for regime in ["rdfs", "owl-rl"] {
        match get(regime) {
            None | Some(Yaml::Null) | Some(Yaml::Boolean(false)) => {}
            Some(Yaml::Boolean(true)) => out.push((regime, name.clone())),
            Some(Yaml::String(graph)) => out.push((regime, format!("{name}={graph}"))),
            Some(_) => {
                return Err(format!(
                    "datasets: {name}: {regime} is true or the IRI of the graph to keep it in"
                ));
            }
        }
    }
    match get("text-properties") {
        None | Some(Yaml::Null) => {}
        Some(Yaml::String(p)) => out.push(("text-properties", format!("{name}={p}"))),
        Some(Yaml::Array(list)) => {
            for p in list {
                let Yaml::String(p) = p else {
                    return Err(format!("datasets: {name}: text-properties are IRIs"));
                };
                out.push(("text-properties", format!("{name}={p}")));
            }
        }
        Some(_) => return Err(format!("datasets: {name}: text-properties are IRIs")),
    }
    let aliases = match get("aliases").or_else(|| get("alias")) {
        None | Some(Yaml::Null) => Vec::new(),
        Some(Yaml::String(a)) => vec![a.clone()],
        Some(Yaml::Array(list)) => list
            .iter()
            .map(|a| match a {
                Yaml::String(a) => Ok(a.clone()),
                _ => Err(format!("datasets: {name}: aliases are names")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err(format!("datasets: {name}: aliases are names")),
    };
    for a in aliases {
        out.push(("alias", format!("{a}={name}")));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> Vec<Opt> {
        let o = |long: &str, takes_value: bool, env: Option<&str>| Opt {
            long: long.into(),
            short: match long {
                "bind" => Some('b'),
                "cors" => Some('c'),
                _ => None,
            },
            takes_value,
            env: env.map(Into::into),
        };
        vec![
            o("bind", true, Some("TRIPLETRAIL_TEST_BIND_UNSET")),
            o("timeout", true, None),
            o("cors", false, None),
            o("dataset", true, None),
            o("attach", true, None),
            o("alias", true, None),
            o("union-default-graph", true, None),
            o("rdfs", true, None),
            o("owl-rl", true, None),
            o("config", true, None),
        ]
    }

    fn args(a: &[&str]) -> Vec<OsString> {
        a.iter().map(OsString::from).collect()
    }

    #[test]
    fn options_and_datasets_become_flags() {
        let text = "bind: 0.0.0.0:9000\ntimeout: 30\ncors: true\ndatasets:\n  - name: kg\n    union-default-graph: true\n    rdfs: true\n    aliases: [live]\n  - name: rinf\n    path: /snap/rinf\n    read-only: true\n    owl_rl: urn:inf\n";
        let out = expand(
            &args(&["tripletrail", "serve", "--timeout", "5"]),
            text,
            &opts(),
        )
        .unwrap();
        let got: Vec<String> = out
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            got,
            [
                "tripletrail",
                "serve",
                "--timeout",
                "5",
                "--bind",
                "0.0.0.0:9000",
                "--cors",
                "--dataset",
                "kg",
                "--union-default-graph",
                "kg",
                "--rdfs",
                "kg",
                "--alias",
                "live=kg",
                "--attach",
                "rinf=/snap/rinf:ro",
                "--owl-rl",
                "rinf=urn:inf",
            ]
        );
        // Datasets on the command line replace the file's.
        let out = expand(
            &args(&["tripletrail", "serve", "--dataset", "x"]),
            text,
            &opts(),
        )
        .unwrap();
        assert!(!out.iter().any(|a| a == "kg"));
    }

    #[test]
    fn a_short_flag_wins_over_the_file() {
        let text = "bind: 0.0.0.0:9000\ncors: true\n";
        let got = |a: &[&str]| -> Vec<String> {
            expand(&args(a), text, &opts())
                .unwrap()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            got(&["tripletrail", "serve", "-b", "1.2.3.4:1"]),
            ["tripletrail", "serve", "-b", "1.2.3.4:1", "--cors"]
        );
        assert_eq!(
            got(&["tripletrail", "serve", "-b1.2.3.4:1"]),
            ["tripletrail", "serve", "-b1.2.3.4:1", "--cors"]
        );
        assert_eq!(
            got(&["tripletrail", "serve", "-cb", "x"]),
            ["tripletrail", "serve", "-cb", "x"]
        );
        // A value that looks like a flag is a value.
        assert_eq!(
            got(&["tripletrail", "serve", "--timeout", "-c"]),
            [
                "tripletrail",
                "serve",
                "--timeout",
                "-c",
                "--bind",
                "0.0.0.0:9000",
                "--cors"
            ]
        );
    }

    #[test]
    fn mistakes_are_named() {
        let e = |text: &str| expand(&args(&["tripletrail", "serve"]), text, &opts()).unwrap_err();
        assert!(e("bnd: x").contains("unknown option \"bnd\""));
        assert!(e("cors: 3").contains("cors: is true or false"));
        assert!(e("- a").contains("map of options"));
        assert!(e("datasets:\n  - path: /x").contains("needs a name"));
        assert!(e("datasets:\n  - name: a\n    readonly: true").contains("unknown key"));
        assert!(e("datasets:\n  - name: a\n    read-only: true").contains("needs a path"));
        assert!(e("config: other.yaml").contains("another file"));
    }

    #[test]
    fn the_file_is_found() {
        assert_eq!(
            file_of(&args(&["tripletrail", "serve", "--config", "a.yaml"])),
            Some(PathBuf::from("a.yaml"))
        );
        assert_eq!(
            file_of(&args(&["tripletrail", "serve", "--config=b.yaml"])),
            Some(PathBuf::from("b.yaml"))
        );
        assert_eq!(
            file_of(&args(&["tripletrail", "load", "--config", "a.yaml"])),
            None
        );
    }
}
