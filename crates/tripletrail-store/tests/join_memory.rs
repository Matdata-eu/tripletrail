//! The memory budget of one query's id joins is a setting (#479): a join past it gives up,
//! so the caller evaluates the pattern row by row, and a raised budget lets it through again.
//! One query's own limit (#189) applies on its thread only, below the process's. A test file
//! of its own: the budget is for the whole process.

use oxigraph::io::RdfFormat;
use oxigraph::model::{NamedNode, TermRef};
use tripletrail_store::{
    DEFAULT_JOIN_MEMORY, GraphSelector, IdSlot, IdTriple, Store, join_memory, memory_limit,
    set_join_memory, with_join_memory,
};

#[test]
fn a_join_gives_up_past_the_budget() {
    assert_eq!(join_memory(), DEFAULT_JOIN_MEMORY);
    assert_eq!(memory_limit(), None);
    let store = Store::new();
    let data: String = (0..200)
        .map(|i| format!("<urn:s{i}> <urn:p> {i} .\n"))
        .collect();
    store.bulk_load(RdfFormat::Turtle, data.as_bytes()).unwrap();
    let snap = store.snapshot();
    let p = NamedNode::new("urn:p").unwrap();
    let p = snap.term_id(TermRef::NamedNode(p.as_ref())).unwrap();
    // A cross join: 200 × 200 rows of 4 ids, 1.28 MB.
    let triples = [
        IdTriple {
            s: IdSlot::Var(0),
            p: IdSlot::Const(p),
            o: IdSlot::Var(1),
        },
        IdTriple {
            s: IdSlot::Var(2),
            p: IdSlot::Const(p),
            o: IdSlot::Var(3),
        },
    ];
    let join = || snap.join_ids(&triples, 4, GraphSelector::Default, &|| false);
    assert_eq!(join().map(|r| r.len()), Some(40_000));
    set_join_memory(64 << 10);
    assert_eq!(join_memory(), 64 << 10);
    assert!(join().is_none(), "past a 64 KiB budget");
    set_join_memory(4 << 20);
    assert_eq!(join().map(|r| r.len()), Some(40_000));
    // A query's own limit, on this thread only, and then the one before again.
    with_join_memory(64 << 10, || {
        assert_eq!(join_memory(), 64 << 10);
        assert!(join().is_none(), "past a query's 64 KiB");
        let other = std::thread::scope(|s| s.spawn(|| join().map(|r| r.len())).join().unwrap());
        assert_eq!(
            other,
            Some(40_000),
            "another thread keeps the process's budget"
        );
    });
    assert_eq!(join().map(|r| r.len()), Some(40_000));
    // A larger query limit than the process's changes nothing; 0 sets none.
    with_join_memory(1 << 30, || assert_eq!(join_memory(), 4 << 20));
    with_join_memory(0, || assert_eq!(join_memory(), 4 << 20));
    // A limit set to the default is still a limit (#874); 0 sets none.
    set_join_memory(DEFAULT_JOIN_MEMORY);
    assert_eq!(memory_limit(), Some(DEFAULT_JOIN_MEMORY));
    set_join_memory(0);
    assert_eq!(join_memory(), DEFAULT_JOIN_MEMORY);
    assert_eq!(memory_limit(), None);
    with_join_memory(64 << 10, || assert_eq!(memory_limit(), Some(64 << 10)));
}
