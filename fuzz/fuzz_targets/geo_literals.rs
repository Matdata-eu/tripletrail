//! Geometry literals of every supported datatype: parsing must never panic, and whatever
//! parses must serialise back to WKT that parses again.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxigraph::model::{Literal, NamedNodeRef};
use tripletrail_geo::{
    GEOJSON_LITERAL, GML_LITERAL, H3_LITERAL, KML_LITERAL, WKT_LITERAL, geom_literal,
    parse_wkt_literal_uncached,
};

const DATATYPES: [NamedNodeRef<'static>; 5] = [
    WKT_LITERAL,
    GEOJSON_LITERAL,
    GML_LITERAL,
    KML_LITERAL,
    H3_LITERAL,
];

fuzz_target!(|data: &[u8]| {
    let Some((&kind, rest)) = data.split_first() else {
        return;
    };
    let Ok(text) = std::str::from_utf8(rest) else {
        return;
    };
    let datatype = DATATYPES[usize::from(kind) % DATATYPES.len()];
    let term = Literal::new_typed_literal(text, datatype).into();
    if let Some(g) = parse_wkt_literal_uncached(&term) {
        let wkt = geom_literal(&g);
        assert!(
            parse_wkt_literal_uncached(&wkt).is_some(),
            "{text:?} parsed but its WKT {wkt} does not"
        );
    }
});
