//! The same query answers on an on-disk store in each of its states (#154), from fuzzer input:
//! the check of `crates/tripletrail/tests/store_states.rs`.
#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::PathBuf;

mod states {
    include!("../../crates/tripletrail/tests/support/store_states.rs");
}

fuzz_target!(|data: &[u8]| {
    // In the fuzz build folder, as `store_files` does: libFuzzer ends the process with `exit`.
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join(format!("store-states-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    states::check(&dir, &states::Plan::new(data));
    let _ = std::fs::remove_dir_all(&dir);
});
