// The geometry of the Map page's tools (#33): `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const { geometry: g, featureNumber } = require("../map-tools.js");

const close = (a, b, tolerance, what) => assert.ok(Math.abs(a - b) <= tolerance, `${what}: ${a} is not ${b} ± ${tolerance}`);

test("distances and bearings on the sphere", () => {
  // Brussels-Central to Antwerp-Central, about 41.6 km north-north-east.
  const bxl = [4.3571, 50.8455];
  const ant = [4.4212, 51.2172];
  close(g.distance(bxl, ant), 41600, 300, "Brussels to Antwerp");
  close(g.bearing(bxl, ant), 6.3, 1, "the bearing");
  // A degree of latitude, and of longitude on the equator.
  close(g.distance([0, 0], [0, 1]), 111195, 1, "a degree of latitude");
  close(g.distance([0, 0], [1, 0]), 111195, 1, "a degree on the equator");
  assert.strictEqual(g.bearing([0, 0], [1, 0]), 90);
  assert.strictEqual(g.fmtBearing(g.bearing([0, 0], [0, -1])), "180° S");
  assert.strictEqual(g.fmtBearing(359.6), "0° N");
  close(g.length([[0, 0], [0, 1], [1, 1]]), 111195 + 111178, 20, "two legs");
});

test("areas and perimeters", () => {
  // A degree square on the equator: about 12,364 km².
  const square = [[0, 0], [1, 0], [1, 1], [0, 1]];
  close(g.ringArea(square) / 1e6, 12364, 15, "a degree square");
  assert.strictEqual(g.ringArea(square), g.ringArea([...square, [0, 0]]), "open and closed rings alike");
  assert.strictEqual(g.ringArea([...square].reverse()), g.ringArea(square), "either way round");
  close(g.perimeter(square), 4 * 111195, 100, "its perimeter");
  // A circle of 1 km: π km² within a percent, its points 1 km from the centre.
  const c = g.circle([4.35, 50.85], 1000);
  close(g.ringArea(c) / 1e6, Math.PI, 0.01 * Math.PI, "a circle of 1 km");
  for (const p of c) close(g.distance([4.35, 50.85], p), 1000, 0.01, "a point of the circle");
  assert.deepStrictEqual(c[0], c[c.length - 1], "a closed ring");
});

test("WKT both ways", () => {
  assert.strictEqual(g.toWkt({ type: "Point", coordinates: [4.123456789, 50.1] }), "POINT(4.123457 50.1)");
  assert.strictEqual(g.toWkt({ type: "LineString", coordinates: [[0, 0], [1, 1]] }), "LINESTRING(0 0, 1 1)");
  assert.strictEqual(g.toWkt({ type: "Polygon", coordinates: [[[0, 0], [1, 0], [1, 1]]] }), "POLYGON((0 0, 1 0, 1 1, 0 0))");
  assert.deepStrictEqual(g.fromWkt("POINT(4.3 50.8)"), { type: "Point", coordinates: [4.3, 50.8] });
  assert.deepStrictEqual(g.fromWkt(" linestring (0 0, 1 1) "), { type: "LineString", coordinates: [[0, 0], [1, 1]] });
  assert.deepStrictEqual(g.fromWkt("POLYGON((0 0, 1 0, 1 1, 0 0), (0.2 0.2, 0.3 0.2, 0.3 0.3, 0.2 0.2))"),
    { type: "Polygon", coordinates: [[[0, 0], [1, 0], [1, 1], [0, 0]]] }, "the outer ring");
  assert.deepStrictEqual(g.fromWkt("<http://www.opengis.net/def/crs/OGC/1.3/CRS84> POINT(1 2)"), { type: "Point", coordinates: [1, 2] });
  // What the map does not draw: other CRSs, other types, broken WKT.
  for (const wkt of ["<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)", "MULTIPOINT((1 2))", "POINT(1)", "POLYGON((0 0, 1 1))", "", "https://example.org/a"]) {
    assert.strictEqual(g.fromWkt(wkt), null, wkt);
  }
});

test("lengths and areas as people read them", () => {
  assert.deepStrictEqual([3.24, 850.4, 1250, 12345, 125400].map(g.fmtLength), ["3.2 m", "850 m", "1.25 km", "12.3 km", "125 km"]);
  assert.deepStrictEqual([450.2, 12500, 250000, 3.21e6, 12.34e6, 512e6].map(g.fmtArea), ["450 m²", "1.25 ha", "25.0 ha", "3.21 km²", "12.3 km²", "512 km²"]);
  assert.strictEqual(g.fmtPoint([4.351234, 50.850123]), "50.85012, 4.35123");
});

test("a band's width on the map", () => {
  // 500 m each side on the equator at zoom 0: 1,000 m over 78,271.5 m a pixel.
  close(g.bandWidth(500, 0), 1000 / (40075016.686 / 512), 1e-9, "on the equator");
  close(g.bandWidth(500, 60), 2 * g.bandWidth(500, 0), 1e-9, "twice as wide at 60°");
});

test("EPSG codes and projected points", () => {
  assert.strictEqual(g.epsgIri("31370"), "http://www.opengis.net/def/crs/EPSG/0/31370");
  assert.strictEqual(g.epsgIri(" epsg:3857 "), "http://www.opengis.net/def/crs/EPSG/0/3857");
  for (const bad of ["", "CRS84", "1234567", "31370; DROP", "EPSG:"]) assert.strictEqual(g.epsgIri(bad), null, bad);
  assert.strictEqual(g.fmtProjected("<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(149179.3617754487 170566.12911798153)"), "149179.36 170566.13");
  assert.strictEqual(g.fmtProjected("POINT(4.1234567 50.5)"), "4.123457 50.5");
  assert.strictEqual(g.fmtProjected("LINESTRING(0 0, 1 1)"), null);
});

test("a drawn area as CQL2 for the OGC API's items", () => {
  const poly = "POLYGON((4.3 50.95, 4.6 50.95, 4.6 51.3, 4.3 51.3, 4.3 50.95))";
  assert.strictEqual(g.cqlArea({ area: poly }), "S_INTERSECTS(geometry, " + poly + ")");
  assert.strictEqual(g.cqlArea({ area: poly, rel: "within" }), "S_WITHIN(geometry, " + poly + ")");
  // No CQL2 form: outside (no geometry meets the area), a distance, a feature, another
  // layer, a CRS named first.
  assert.strictEqual(g.cqlArea({ area: poly, rel: "disjoint" }), "");
  assert.strictEqual(g.cqlArea({ area: "POINT(4.3571 50.8455)", rel: "dwithin", dist: 2000 }), "");
  assert.strictEqual(g.cqlArea({ area: "https://example.org/rail#BXC", rel: "within" }), "");
  assert.strictEqual(g.cqlArea({ area: "layer:Station" }), "");
  assert.strictEqual(g.cqlArea({ area: "<http://www.opengis.net/def/crs/EPSG/0/31370> POINT(150000 170000)" }), "");
  assert.strictEqual(g.cqlArea(null), "");
});

test("a feature's number is the server's", () => {
  // The values of tiles.rs's test: FNV-1a 64 cut to 40 bits.
  assert.strictEqual(featureNumber(""), Number(0xcbf29ce484222325n & 0xffffffffffn));
  assert.strictEqual(featureNumber("a"), Number(0xaf63dc4c8601ec8cn & 0xffffffffffn));
  assert.strictEqual(featureNumber("http://data.europa.eu/949/tunnel/0fc55bc6a3"), 154522281420);
});
