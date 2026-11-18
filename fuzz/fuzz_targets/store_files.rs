//! A store directory with one of its files replaced by the input: opening the store and
//! reading every quad gives an error or the quads, never a panic or an abort.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxigraph::io::RdfFormat;
use std::path::PathBuf;
use std::sync::LazyLock;
use tripletrail_store::Store;

const DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix geo: <http://www.opengis.net/ont/geosparql#> .
ex:a ex:name "Alpha" ; ex:label "alfa"@nb ; ex:n 1 ; ex:d 2.5 ; ex:knows ex:b , ex:c .
ex:b ex:name "Beta" ; ex:n 2 ; ex:knows ex:c .
_:x ex:knows ex:a .
ex:s geo:asWKT "POINT(10.75 59.91)"^^geo:wktLiteral .
"#;

/// A store with one checkpoint and one logged transaction, and its files' contents.
///
/// In the fuzz build folder rather than the system's temporary one: a static is never dropped
/// and libFuzzer ends the process with `exit`, so the folder outlives the run. In `fuzz/target`
/// it goes with `cargo clean` or a fresh checkout instead of piling up in `/tmp`.
static TEMPLATE: LazyLock<(PathBuf, Vec<(PathBuf, Vec<u8>)>)> = LazyLock::new(|| {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("store-files-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let store = Store::open(&dir).unwrap();
    store.bulk_load(RdfFormat::Turtle, DATA.as_bytes()).unwrap();
    store.checkpoint().unwrap();
    store
        .bulk_load(
            RdfFormat::NTriples,
            "<http://example.com/z> <http://example.com/n> \"3\" .\n".as_bytes(),
        )
        .unwrap();
    drop(store);
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else {
                files.push((p.clone(), std::fs::read(&p).unwrap()));
            }
        }
    }
    files.sort();
    (dir, files)
});

fuzz_target!(|data: &[u8]| {
    let Some((&which, content)) = data.split_first() else {
        return;
    };
    let (dir, files) = &*TEMPLATE;
    let (path, original) = &files[usize::from(which) % files.len()];
    // Half the time, the input patches the real file instead of replacing it, which keeps
    // the magic numbers and lengths that a random file would fail at once.
    let damaged = if which & 0x80 != 0 && !original.is_empty() {
        let mut d = original.clone();
        for chunk in content.chunks(3) {
            if let [hi, lo, value] = chunk {
                let at = (usize::from(*hi) << 8 | usize::from(*lo)) % d.len();
                d[at] = *value;
            }
        }
        d
    } else {
        content.to_vec()
    };
    std::fs::write(path, &damaged).unwrap();
    if let Ok(store) = Store::open(dir) {
        let _ = store.iter().count();
    }
    // Opening may have rewritten files (the log is truncated to its last commit).
    for (p, c) in files {
        std::fs::write(p, c).unwrap();
    }
});
