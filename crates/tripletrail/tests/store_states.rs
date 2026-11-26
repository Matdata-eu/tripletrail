//! The same query answers on an on-disk store in each of its states (#154): writes in memory,
//! compacted into runs, merged into the segments, and opened again, all equal to Oxigraph's
//! store. The `store_states` fuzz target runs the same check on fuzzer input.

#[path = "support/store_states.rs"]
#[allow(dead_code)]
mod states;

/// A fixed number of inputs from a seeded generator, so a failure reproduces;
/// `STORE_STATES_INPUTS` sets how many (40 by default).
#[test]
fn query_answers_stay_the_same_in_every_state_of_a_store() {
    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let inputs: usize = std::env::var("STORE_STATES_INPUTS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(40);
    let mut runs = 0;
    for i in 0..inputs {
        let data: Vec<u8> = (0..900).map(|_| next() as u8).collect();
        let dir = tripletrail_testdir::tempdir(&format!("store-states-{i}"));
        runs += usize::from(states::check(&dir, &states::Plan::new(&data)));
    }
    // Most inputs reach the state with runs.
    assert!(
        runs * 10 >= inputs * 9,
        "{runs} of {inputs} inputs had runs"
    );
}
