// The Records edit form's helpers (#82): `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const edit = require("../records-edit.js");

test("language maps are rows and back, empty texts left out", () => {
  const rows = edit.langRows({ en: "Line", fr: ["Ligne", "Voie"] });
  assert.deepStrictEqual(rows, [{ lang: "en", text: "Line" }, { lang: "fr", text: "Ligne" }, { lang: "fr", text: "Voie" }]);
  assert.deepStrictEqual(edit.langMap([...rows, { lang: "de", text: "" }]), { en: "Line", fr: ["Ligne", "Voie"] });
});

test("a key's body value: one value or null when single, references by IRI", () => {
  const name = edit.formField({ key: "name", kind: "text", single: true });
  assert.strictEqual(edit.bodyValue(name, ["A"]), "A");
  assert.strictEqual(edit.bodyValue(name, [""]), null);
  const line = edit.formField({ key: "line", kind: "reference", target: "Line", single: false });
  assert.deepStrictEqual(edit.bodyValue(line, [{ id: "http://x/l1", label: "L1" }, "http://x/l2", ""]), ["http://x/l1", "http://x/l2"]);
});

test("only the keys that changed are sent", () => {
  assert.deepStrictEqual(edit.changes({ a: "1", b: ["x"], c: null }, { a: "1", b: ["x", "y"], c: null }), { b: ["x", "y"] });
  assert.deepStrictEqual(edit.changes({ a: "1" }, { a: "1" }), {});
});

test("a 422's problems by key", () => {
  const byKey = edit.problemsByKey({ errors: [{ key: "name", message: "Required" }, { message: "Closed shape" }, { key: "name", constraint: "sh:pattern" }] });
  assert.deepStrictEqual(byKey, { name: ["Required", "sh:pattern"], "": ["Closed shape"] });
});

test("a property of /search/classes as a form field", () => {
  const f = edit.formField({ key: "opened", title: "Opened", kind: "date", datatype: "http://www.w3.org/2001/XMLSchema#date", single: true, required: true });
  assert.strictEqual(f.label, "Opened");
  assert.strictEqual(f.datatype, "date");
  assert.ok(f.required && f.single);
  assert.ok(edit.formField({ key: "n", kind: "text", datatype: "http://www.w3.org/1999/02/22-rdf-syntax-ns#langString" }).lang);
});
