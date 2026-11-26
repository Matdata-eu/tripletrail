// The row rule builder's shapes (enterprise#8): `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const rows = require("../row-rules.js");

const EX = "http://example.org/";

test("a rule's conditions become property shapes of the subset sparql-guard reads", () => {
  const t = rows.turtle({
    kind: "visibleIf", id: EX + "OwnArea", target: EX + "Project",
    conditions: [
      { path: [EX + "assignedTo", EX + "area"], op: "agent" },
      { path: [EX + "status"], op: "in", value: "open, planned" },
      { path: [EX + "level"], op: "equals", value: "3" },
      { path: [EX + "owner"], op: "equals", value: EX + "team" },
      { path: [EX + "closed"], op: "absent" },
      { path: [EX + "name"], op: "exists" },
    ],
  });
  assert.match(t, /^@prefix sh: <http:\/\/www\.w3\.org\/ns\/shacl#> \.\n@prefix sg: /);
  assert.match(t, /<http:\/\/example\.org\/OwnArea> a sh:NodeShape ;\n    sh:targetClass <http:\/\/example\.org\/Project> ;/);
  assert.ok(t.includes("sh:path ( <http://example.org/assignedTo> <http://example.org/area> ) ; sh:hasValue sg:requestingAgent"), t);
  assert.ok(t.includes('sh:in ( "open" "planned" )'), t);
  assert.ok(t.includes("sh:hasValue 3 ]"), t);
  assert.ok(t.includes("sh:hasValue <http://example.org/team> ]"), t);
  assert.ok(t.includes("sh:maxCount 0"), t);
  assert.ok(t.includes("sh:minCount 1"), t);
  assert.ok(t.trimEnd().endsWith("] ."), t);
});

test("the data says whether a value is an IRI or text", () => {
  const kinds = (p) => (p[0] === EX + "code" ? "literal" : "iri");
  const t = rows.turtle({ kind: "editableIf", id: EX + "S", target: EX + "C",
    conditions: [{ path: [EX + "code"], op: "equals", value: "http://x" }, { path: [EX + "area"], op: "in", value: "urn:a\nurn:b" }] }, kinds);
  assert.ok(t.includes('sh:hasValue "http://x"'), t);
  assert.ok(t.includes("sh:in ( <urn:a> <urn:b> )"), t);
});

test("hidden properties, on a class or everywhere", () => {
  const t = rows.turtle({ kind: "hideProperties", id: EX + "NoPhones", target: "", properties: [EX + "phone", EX + "email"] });
  assert.ok(!t.includes("targetClass"), t);
  assert.ok(t.includes("sh:property [ sh:path <http://example.org/phone> ] ;\n    sh:property [ sh:path <http://example.org/email> ] ."), t);
});

test("an incomplete or unsafe rule is refused, never written", () => {
  assert.throws(() => rows.turtle({ kind: "visibleIf", id: EX + "S", target: "", conditions: [{ path: [EX + "a"], op: "exists" }] }), /class/);
  assert.throws(() => rows.turtle({ kind: "visibleIf", id: EX + "S", target: EX + "C", conditions: [] }), /condition/);
  assert.throws(() => rows.turtle({ kind: "visibleIf", id: EX + "S", target: EX + "C", conditions: [{ path: [EX + "a"], op: "equals", value: "" }] }), /value/);
  assert.throws(() => rows.turtle({ kind: "hideProperties", id: EX + "S", properties: [] }), /properties/);
  // No IRI can close the brackets and write Turtle of its own.
  assert.throws(() => rows.turtle({ kind: "visibleIf", id: EX + "S> a <x", target: EX + "C", conditions: [{ path: [EX + "a"], op: "exists" }] }), /Not an IRI/);
  assert.throws(() => rows.turtle({ kind: "visibleIf", id: EX + "S", target: EX + "C", conditions: [{ path: [EX + "a> ; sh:x <y"], op: "exists" }] }), /Not an IRI/);
  // A string with quotes stays one string.
  const t = rows.turtle({ kind: "visibleIf", id: EX + "S", target: EX + "C", conditions: [{ path: [EX + "a"], op: "equals", value: 'a" ] . <x> <y> "z' }] });
  assert.ok(t.includes('sh:hasValue "a\\" ] . <x> <y> \\"z" ]'), t);
});

test("shape IRIs are new, and a policy's rules change one at a time", () => {
  const id = rows.shapeId("assets", "visibleIf", EX + "Project", []);
  assert.strictEqual(id, "urn:tripletrail:access:assets:shape:visibleIf-Project");
  assert.strictEqual(rows.shapeId("assets", "visibleIf", EX + "Project", [id]), id + "-2");
  const rule = { rule: "sg:visibleIf", shape: id };
  const rules = rows.withRule(rows.withRule([], rule), rule);
  assert.deepStrictEqual(rules, [rule]);
  assert.deepStrictEqual(rows.withoutRule(rules, rule), []);
});
