//! Every registered geometry function, called with arguments built from the input. The
//! function table turns panics into SPARQL errors, so none reaches a query; this target still
//! fails on a panic raised in tripletrail's own code (a bug to fix), and on a hang or a blow-up in
//! memory. Panics inside third-party geometry crates (`i_overlay` behind `geo`'s boolean
//! operations, the `wkt` writer) are left to the function table: they are upstream bugs that
//! would otherwise drown everything else.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oxigraph::model::vocab::xsd;
use oxigraph::model::{Literal, NamedNode, Term};
use std::sync::LazyLock;
use tripletrail_geo::{WKT_LITERAL, function_table};

static FUNCTIONS: LazyLock<Vec<(NamedNode, tripletrail_geo::Func)>> = LazyLock::new(function_table);

/// Geometries the input picks from, then edits: empty, degenerate, 3-D and CRS-tagged cases.
const GEOMETRIES: &[&str] = &[
    "POINT EMPTY",
    "LINESTRING EMPTY",
    "POLYGON EMPTY",
    "MULTIPOLYGON EMPTY",
    "GEOMETRYCOLLECTION EMPTY",
    "POINT(0 0)",
    "POINT(10.75 59.91)",
    "LINESTRING(0 0, 0 0)",
    "LINESTRING(0 0, 1 1, 2 0)",
    "LINESTRING(10 59, 11 60, 10 59)",
    "POLYGON((0 0, 1 0, 1 1, 0 1, 0 0))",
    "POLYGON((0 0, 1 1, 1 0, 0 1, 0 0))",
    "POLYGON((0 0, 4 0, 4 4, 0 4, 0 0), (1 1, 2 1, 2 2, 1 2, 1 1))",
    "POLYGON((0 0, 1 0, 0 0))",
    "MULTIPOINT((0 0), (1 1), (0 0))",
    "MULTILINESTRING((0 0, 1 1), (1 1, 2 2))",
    "MULTIPOLYGON(((0 0, 1 0, 1 1, 0 0)), ((2 2, 3 2, 3 3, 2 2)))",
    "GEOMETRYCOLLECTION(POINT(0 0), LINESTRING(0 0, 1 1))",
    "POINT Z(1 2 3)",
    "LINESTRING Z(0 0 0, 1 1 1)",
    "LINESTRING M(0 0 0, 1 1 1)",
    "LINESTRING ZM(0 0 0 0, 1 1 1 1)",
    "<http://www.opengis.net/def/crs/EPSG/0/4326> POINT(59.91 10.75)",
    "<http://www.opengis.net/def/crs/EPSG/0/25832> LINESTRING(500000 6600000, 501000 6601000)",
];

const NUMBERS: &[f64] = &[
    0.0,
    1.0,
    -1.0,
    0.5,
    2.0,
    10.0,
    1e-9,
    1e9,
    1e300,
    f64::NAN,
    f64::INFINITY,
    f64::NEG_INFINITY,
];

/// Reads the input a byte at a time; zeros once it runs out.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn byte(&mut self) -> u8 {
        let Some((&b, rest)) = self.0.split_first() else {
            return 0;
        };
        self.0 = rest;
        b
    }

    fn pick<'t, T>(&mut self, items: &'t [T]) -> &'t T {
        let i = usize::from(self.byte()) | usize::from(self.byte()) << 8;
        &items[i % items.len()]
    }

    fn number(&mut self) -> f64 {
        match self.byte() % 4 {
            0 => *self.pick(NUMBERS),
            1 => f64::from(self.byte() as i8),
            _ => f64::from_le_bytes(std::array::from_fn(|_| self.byte())),
        }
    }

    fn geometry(&mut self) -> String {
        match self.byte() % 4 {
            // A geometry built from input numbers.
            0 => {
                let n = 1 + usize::from(self.byte() % 6);
                let coords: Vec<String> = (0..n)
                    .map(|_| format!("{} {}", self.number(), self.number()))
                    .collect();
                match self.byte() % 3 {
                    0 => format!("MULTIPOINT({})", coords.join(", ")),
                    1 => format!("LINESTRING({})", coords.join(", ")),
                    _ => format!("POLYGON(({}, {}))", coords.join(", "), coords[0]),
                }
            }
            // Raw text, so the parser is exercised too.
            1 => {
                let n = usize::from(self.byte() % 64);
                let bytes: Vec<u8> = (0..n).map(|_| self.byte()).collect();
                String::from_utf8_lossy(&bytes).into_owned()
            }
            _ => (*self.pick(GEOMETRIES)).to_owned(),
        }
    }

    fn argument(&mut self) -> Term {
        match self.byte() % 8 {
            0..=3 => Literal::new_typed_literal(self.geometry(), WKT_LITERAL).into(),
            4 | 5 => Literal::new_typed_literal(self.number().to_string(), xsd::DOUBLE).into(),
            6 => Literal::from(i64::from(self.byte() as i8)).into(),
            _ => match self.byte() % 3 {
                0 => {
                    NamedNode::new_unchecked("http://www.opengis.net/def/uom/OGC/1.0/metre").into()
                }
                1 => Literal::new_simple_literal("").into(),
                _ => Literal::from(self.byte() % 2 == 0).into(),
            },
        }
    }
}

fn fail_on_own_panics() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let ours = info
            .location()
            .is_some_and(|l| l.file().contains("crates/tripletrail"));
        if ours {
            previous(info);
            std::process::abort();
        }
    }));
}

static HOOK: std::sync::Once = std::sync::Once::new();

fuzz_target!(|data: &[u8]| {
    HOOK.call_once(fail_on_own_panics);
    let mut input = Input(data);
    let (name, f) = input.pick(&FUNCTIONS);
    let arity = usize::from(input.byte() % 5);
    let args: Vec<Term> = (0..arity).map(|_| input.argument()).collect();
    // Replaying a crash with FUZZ_DEBUG=1 shows the call.
    if std::env::var_os("FUZZ_DEBUG").is_some() {
        eprintln!(
            "{name}({})",
            args.iter()
                .map(Term::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let _ = f(&args);
});
