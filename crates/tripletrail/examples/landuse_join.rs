//! Times a spatial join of many short lines against many more polygons, shaped like the
//! railway tracks and landuse areas of OSM Europe at a smaller scale (#2):
//! `landuse_join <dir> [polygons] [runs]`.
//!
//! The first run with a new `<dir>` writes the data there (about 2 KB of store per polygon):
//! polygons of 8 to 40 vertices at OSM Europe's density (about 3.3 per km²), a few large ones
//! of thousands of vertices, and one line of 30 segments of about 200 m for every 26 polygons.
//! The store is then opened again, so the geometries are read from its files.
use std::fmt::Write as _;
use std::time::Instant;

fn rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("RssAnon:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
        })
        .map_or(0, |kb| kb / 1024)
}

/// A small deterministic generator (xorshift), so every run writes the same data.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

const PREFIX: &str = "http://example.org/bench#";
const GEO: &str = "http://www.opengis.net/ont/geosparql#";

fn polygon(out: &mut String, cx: f64, cy: f64, r: f64, n: usize, rng: &mut Rng) {
    out.push_str("POLYGON((");
    let mut first = None;
    for k in 0..n {
        let a = k as f64 / n as f64 * std::f64::consts::TAU;
        let d = r * (0.6 + 0.4 * rng.next());
        let (x, y) = (cx + d * a.cos(), cy + d * a.sin() * 0.65);
        first.get_or_insert((x, y));
        let _ = write!(out, "{x:.7} {y:.7}, ");
    }
    let (x, y) = first.unwrap();
    let _ = write!(out, "{x:.7} {y:.7}))");
}

fn write_data(dir: &str, polygons: usize) {
    let store = tripletrail_store::Store::open(dir).unwrap();
    // OSM Europe: 33.2 M landuse areas, 1.25 M railway tracks, on about 10 M km².
    let km2 = polygons as f64 / 3.3;
    let side_km = km2.sqrt();
    let (deg_x, deg_y) = (side_km / 75.0, side_km / 111.0);
    let cell_x = deg_x / (polygons as f64).sqrt();
    let cell_y = deg_y / (polygons as f64).sqrt();
    let per_row = (polygons as f64).sqrt().ceil() as usize;
    let tracks = polygons / 26;
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let path = format!("{dir}.nt");
    let mut file = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
    let mut nt = String::new();
    let mut flush = |nt: &mut String| {
        std::io::Write::write_all(&mut file, nt.as_bytes()).unwrap();
        nt.clear();
    };
    let mut wkt = String::new();
    for i in 0..polygons {
        let (cx, cy) = (
            5.0 + (i % per_row) as f64 * cell_x + cell_x / 2.0,
            50.0 + (i / per_row) as f64 * cell_y + cell_y / 2.0,
        );
        wkt.clear();
        // One in 20,000 is a large forest of thousands of vertices.
        if i % 20_000 == 7 {
            polygon(&mut wkt, cx, cy, cell_x * 15.0, 4000, &mut rng);
        } else {
            let n = 8 + (rng.next() * 32.0) as usize;
            polygon(&mut wkt, cx, cy, cell_x * 0.7, n, &mut rng);
        }
        let _ = writeln!(
            nt,
            "<{PREFIX}l{i}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{PREFIX}OsmLanduse> .\n\
             <{PREFIX}l{i}> <{GEO}hasGeometry> <{PREFIX}l{i}g> .\n\
             <{PREFIX}l{i}g> <{GEO}asWKT> \"{wkt}\"^^<{GEO}wktLiteral> ."
        );
        if nt.len() > 64 << 20 {
            flush(&mut nt);
        }
    }
    for t in 0..tracks {
        wkt.clear();
        wkt.push_str("LINESTRING(");
        let (mut x, mut y) = (5.0 + rng.next() * deg_x, 50.0 + rng.next() * deg_y);
        let mut heading = rng.next() * std::f64::consts::TAU;
        for k in 0..=30 {
            if k > 0 {
                wkt.push_str(", ");
                heading += (rng.next() - 0.5) * 0.3;
                x += heading.cos() * 0.2 / 75.0;
                y += heading.sin() * 0.2 / 111.0;
            }
            let _ = write!(wkt, "{x:.7} {y:.7}");
        }
        wkt.push(')');
        let _ = writeln!(
            nt,
            "<{PREFIX}t{t}> <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> <{PREFIX}OsmTrack> .\n\
             <{PREFIX}t{t}> <{GEO}hasGeometry> <{PREFIX}t{t}g> .\n\
             <{PREFIX}t{t}g> <{GEO}asWKT> \"{wkt}\"^^<{GEO}wktLiteral> ."
        );
    }
    flush(&mut nt);
    drop(file);
    store
        .bulk_load(
            oxigraph::io::RdfFormat::NTriples,
            std::io::BufReader::new(std::fs::File::open(&path).unwrap()),
        )
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    store.checkpoint().unwrap();
    println!("wrote {polygons} polygons and {tracks} lines");
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("a dataset directory");
    let polygons: usize = args.next().map_or(1_000_000, |r| r.parse().unwrap());
    let runs: usize = args.next().map_or(3, |r| r.parse().unwrap());
    if !std::path::Path::new(&dir).exists() {
        let t = Instant::now();
        write_data(&dir, polygons);
        println!("written in {:?}", t.elapsed());
    }
    let engine = tripletrail::Engine::open_tripletrail(&dir).unwrap();
    let query = format!(
        "PREFIX geo: <{GEO}> PREFIX geof: <http://www.opengis.net/def/function/geosparql/> \
         PREFIX bench: <{PREFIX}> SELECT (COUNT(*) AS ?n) WHERE {{ \
         ?t a bench:OsmTrack ; geo:hasGeometry/geo:asWKT ?w1 . \
         ?l a bench:OsmLanduse ; geo:hasGeometry/geo:asWKT ?w2 . \
         FILTER(geof:sfIntersects(?w1, ?w2)) }}"
    );
    for run in 0..runs {
        let t = Instant::now();
        let n = match engine.query(&query).unwrap() {
            oxigraph::sparql::QueryResults::Solutions(s) => s
                .map(|s| s.unwrap().get("n").map(ToString::to_string))
                .next()
                .flatten(),
            _ => None,
        };
        let stats = engine.last_spatial_stats();
        println!(
            "run {run}: {:?}, n = {}, pairing {:?}, rss {} MB, {:?}",
            t.elapsed(),
            n.unwrap_or_default(),
            stats.pairing,
            rss_mb(),
            engine.memory_use().map(|m| m.geometries)
        );
        for d in &stats.decisions {
            println!("  {d}");
        }
    }
}
