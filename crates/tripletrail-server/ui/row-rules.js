// Row rule shapes from a form (enterprise#8): the Access page's builder turns a class and a
// list of conditions into the Turtle of a SHACL node shape, using only the subset of SHACL that
// sparql-guard's row rules accept, so a saved rule always loads. Plain functions with no page
// state: `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
//
// A rule: { kind: "visibleIf" | "editableIf" | "hideProperties", id (the shape's IRI), target
// (a class IRI, or "" for every resource, hideProperties only), conditions: [{ path: [IRI…]
// (a sequence path when longer than one), op, value }], properties: [IRI…] (hideProperties) }.
//
// Operators: "agent" (the value is the requesting user: sg:requestingAgent), "equals" (one
// value), "in" (values separated by commas or new lines), "exists", "absent".
"use strict";

(function (root) {
  const SG = "https://matdata.eu/ns/sparql-guard#";

  const KINDS = [
    { kind: "visibleIf", label: "Show only when", help: "Callers under the policy see a resource of the class only when it meets every condition. The others are hidden, as subject and as object." },
    { kind: "editableIf", label: "Edit only when", help: "Callers under the policy may change a resource of the class only when it meets every condition, before and after the change. Nothing is hidden." },
    { kind: "hideProperties", label: "Hide these properties", help: "Callers under the policy do not see these properties: on the class's resources, or on every resource when no class is chosen." },
  ];
  const OPS = [
    { op: "agent", label: "is the requesting user", value: false },
    { op: "equals", label: "is", value: true },
    { op: "in", label: "is one of", value: true },
    { op: "exists", label: "has a value", value: false },
    { op: "absent", label: "has no value", value: false },
  ];

  const isIri = (v) => /^[a-z][a-z0-9+.-]*:[^\s<>"{}|\\^`]*$/i.test(v);
  const iri = (v) => {
    if (!isIri(v)) throw new Error(`Not an IRI: ${v}`);
    return `<${v}>`;
  };
  /** A literal in Turtle: a number or boolean as written, else a string. */
  const literal = (v) => {
    if (/^(true|false)$/.test(v) || /^[+-]?\d+(\.\d+)?$/.test(v)) return v;
    return JSON.stringify(v);
  };
  /** A value: an IRI when `kind` says so or when it looks like one, else a literal. */
  const value = (v, kind) => {
    v = String(v).trim();
    if (kind === "literal") return literal(v);
    if (kind === "iri") return iri(v);
    // Without the data's word: web and urn IRIs are IRIs, the rest text.
    return isIri(v) && /^(https?|urn):/i.test(v) ? iri(v) : literal(v);
  };
  const values = (text) => String(text).split(/[\n,]/).map((v) => v.trim()).filter(Boolean);

  function path(p) {
    const steps = (Array.isArray(p) ? p : [p]).map((s) => String(s).trim()).filter(Boolean);
    if (!steps.length) throw new Error("A condition needs a property");
    return steps.length === 1 ? iri(steps[0]) : `( ${steps.map(iri).join(" ")} )`;
  }

  function condition(c, kinds) {
    const kind = kinds ? kinds(c.path) : undefined;
    const p = `sh:path ${path(c.path)}`;
    switch (c.op) {
      case "agent": return `[ ${p} ; sh:hasValue sg:requestingAgent ]`;
      case "equals": {
        if (String(c.value ?? "").trim() === "") throw new Error("A condition needs a value");
        return `[ ${p} ; sh:hasValue ${value(c.value, kind)} ]`;
      }
      case "in": {
        const vs = values(c.value ?? "");
        if (!vs.length) throw new Error("A condition needs one value or more");
        return `[ ${p} ; sh:in ( ${vs.map((v) => value(v, kind)).join(" ")} ) ]`;
      }
      case "exists": return `[ ${p} ; sh:minCount 1 ]`;
      case "absent": return `[ ${p} ; sh:maxCount 0 ]`;
      default: throw new Error(`Unknown condition ${c.op}`);
    }
  }

  /**
   * The Turtle of a rule's shape, with the prefixes it uses. `kinds(path)` may say whether a
   * property's values are "iri" or "literal" (from the data), to write a value right.
   */
  function turtle(rule, kinds) {
    const lines = [];
    if (rule.target) lines.push(`sh:targetClass ${iri(rule.target)}`);
    else if (rule.kind !== "hideProperties") throw new Error("Choose the class the rule is about");
    if (rule.kind === "hideProperties") {
      const props = (rule.properties || []).filter(Boolean);
      if (!props.length) throw new Error("Choose the properties to hide");
      for (const p of props) lines.push(`sh:property [ sh:path ${iri(p)} ]`);
    } else {
      const conditions = rule.conditions || [];
      if (!conditions.length) throw new Error("Add a condition");
      for (const c of conditions) lines.push(`sh:property ${condition(c, kinds)}`);
    }
    const head = `@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix sg: <${SG}> .\n\n${iri(rule.id)} a sh:NodeShape`;
    return `${head} ;\n    ${lines.join(" ;\n    ")} .\n`;
  }

  /** A shape IRI for a new rule of `dataset` about `target`. */
  function shapeId(dataset, kind, target, taken) {
    const slug = (s) => String(s || "all").replace(/^.*[/#:]/, "").replace(/[^A-Za-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "all";
    const base = `urn:tripletrail:access:${encodeURIComponent(dataset)}:shape:${kind}-${slug(target)}`;
    let id = base;
    for (let n = 2; taken && taken.includes(id); n++) id = `${base}-${n}`;
    return id;
  }

  /** A policy's rules with `rule` (`{rule: "sg:…", shape}`) added, once. */
  function withRule(rules, rule) {
    const list = (rules || []).filter((r) => !(r.rule === rule.rule && r.shape === rule.shape));
    return [...list, rule];
  }
  /** A policy's rules without `rule`. */
  const withoutRule = (rules, rule) => (rules || []).filter((r) => !(r.rule === rule.rule && r.shape === rule.shape));

  const api = { KINDS, OPS, turtle, shapeId, withRule, withoutRule, isIri };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.TripleTrailRowRules = api;
})(typeof window !== "undefined" ? window : globalThis);
