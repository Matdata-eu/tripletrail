//! Named graph grants, checked differentially (#208): random datasets, grants and queries;
//! a caller limited to some graphs must get the answers of a store holding only those graphs.
//! The `graph_grants` fuzz target runs the same check on fuzzer input.

#[path = "support/grants.rs"]
#[allow(dead_code)]
mod grants;

/// A fixed number of inputs from a seeded generator, so a failure reproduces;
/// `GRANTS_INPUTS` sets how many (400 by default).
#[test]
fn grants_answer_as_a_store_of_the_granted_graphs() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let inputs: usize = std::env::var("GRANTS_INPUTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(400);
    for _ in 0..inputs {
        let data: Vec<u8> = (0..160).map(|_| next() as u8).collect();
        grants::check(&data);
    }
}

/// Updates under read and write grants, from the same seeded generator.
#[test]
fn updates_under_grants_change_only_what_a_store_of_those_graphs_would() {
    let mut state: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let inputs: usize = std::env::var("GRANTS_INPUTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(400);
    for _ in 0..inputs {
        let data: Vec<u8> = (0..200).map(|_| next() as u8).collect();
        grants::check_update(&data);
    }
}
