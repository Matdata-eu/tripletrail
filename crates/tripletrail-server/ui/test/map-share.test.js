// The time, scale bar and citation of the Map page (#37): `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const m = require("../map-share.js");

test("the slider's span is the union of the extents", () => {
  assert.deepStrictEqual(m.span([["2026-01-01T00:00:00Z", null], ["2025-06-01", "2026-02-01T12:00:00Z"]], "2026-11-21"),
    { from: "2025-06-01", to: "2026-11-21", days: 538 });
  assert.deepStrictEqual(m.span([[null, "2026-03-01T00:00:00Z"]], "2026-11-21"), { from: "2025-03-01", to: "2026-03-01", days: 365 });
  assert.strictEqual(m.span([], "2026-11-21"), null);
});

test("days become a datetime and back", () => {
  assert.strictEqual(m.datetime("", ""), "");
  assert.strictEqual(m.datetime("2026-02-01", "2026-02-01"), "2026-02-01");
  assert.strictEqual(m.datetime("2026-02-01", ""), "2026-02-01/..");
  assert.strictEqual(m.datetime("", "2026-03-31"), "../2026-03-31");
  assert.deepStrictEqual(m.parse("2026-02-01/.."), { from: "2026-02-01", to: "" });
  assert.deepStrictEqual(m.parse("2026-02-01"), { from: "2026-02-01", to: "2026-02-01" });
  assert.deepStrictEqual(m.parse("2026-02-01T10:00:00Z/2026-03-01T00:00:00Z"), { from: "2026-02-01", to: "2026-03-01" });
  assert.deepStrictEqual(m.parse(""), { from: "", to: "" });
});

test("a scale bar is a round length", () => {
  assert.deepStrictEqual(m.scaleBar(10, 120), { metres: 1000, pixels: 100, label: "1 km" });
  assert.deepStrictEqual(m.scaleBar(1, 120), { metres: 100, pixels: 100, label: "100 m" });
  assert.strictEqual(m.scaleBar(30, 120).label, "2 km");
  assert.strictEqual(m.scaleBar(400, 120).label, "20 km");
  // At the equator and zoom 0, a 512-pixel tile spans the Earth.
  assert.ok(Math.abs(m.metresPerPixel(0, 0) * 512 - 40075016.686) < 1e-6);
});

test("a citation names the data, the link and each layer's query", () => {
  const text = m.citation({ title: "rail", dataset: "rail", version: "42-7", link: "https://example.org/$/ui/map/rail?hide=X#9/50.8/4.4",
    when: "2026-11-21", time: "2026-02-01/..", layers: [{ title: "Station", sparql: " SELECT ?f WHERE { ?f a <https://example.org/Station> }\n", items: "https://example.org/rail/ogc/collections/Station/items?f=json" }] });
  assert.strictEqual(text, [
    'rail. Map of the dataset "rail" in TripleTrail, data version 42-7. Retrieved 2026-11-21.',
    "https://example.org/$/ui/map/rail?hide=X#9/50.8/4.4",
    "Time: 2026-02-01/..",
    "",
    'Layer "Station"',
    "OGC API items: https://example.org/rail/ogc/collections/Station/items?f=json",
    "SPARQL:",
    "SELECT ?f WHERE { ?f a <https://example.org/Station> }",
  ].join("\n"));
});
