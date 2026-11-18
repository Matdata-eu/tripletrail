// The Workbench (#402): develop SHACL shapes, SHACL-AF rules and queries on a workspace's scratch
// store (/$/workspaces, #400), validate and infer with provenance (#401), and see why a triple
// is there. Plain JavaScript like app.js; CodeMirror 6 and Tabulator are vendored (MIT) and
// loaded only when the Workbench opens. app.js calls TripleTrailWorkbench.render(ui, args).
"use strict";

window.TripleTrailWorkbench = (function () {
  const VENDOR = "/$/ui/vendor/";
  const API = "/$/workspaces";
  let ui = null; // helpers from app.js: api, el, $, toast, fail, go, ...
  const $ = (sel, root = document) => root.querySelector(sel);
  const el = (...a) => ui.el(...a);
  /** replaceChildren without the null and false children (optional parts). */
  const fill = (node, ...children) => node.replaceChildren(...children.flat().filter((c) => c != null && c !== false));

  // ---------- vendored libraries, loaded once ----------
  const loaded = {};
  function script(src) {
    return (loaded[src] ||= new Promise((resolve, reject) => {
      const s = document.createElement("script");
      s.src = src;
      s.onload = resolve;
      s.onerror = () => reject(new Error("Could not load " + src));
      document.head.append(s);
    }));
  }
  function css(href) {
    return (loaded[href] ||= new Promise((resolve) => {
      const l = document.createElement("link");
      l.rel = "stylesheet";
      l.href = href;
      l.onload = l.onerror = resolve;
      document.head.append(l);
    }));
  }
  const libraries = () => Promise.all([
    script(VENDOR + "codemirror/codemirror.min.js"),
    script(VENDOR + "tabulator/tabulator.min.js"),
    css(VENDOR + "tabulator/tabulator.min.css"),
  ]);

  // ---------- RDF terms ----------
  const COMMON_PREFIXES = {
    rdf: "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    rdfs: "http://www.w3.org/2000/01/rdf-schema#",
    xsd: "http://www.w3.org/2001/XMLSchema#",
    owl: "http://www.w3.org/2002/07/owl#",
    sh: "http://www.w3.org/ns/shacl#",
    skos: "http://www.w3.org/2004/02/skos/core#",
    dcterms: "http://purl.org/dc/terms/",
    geo: "http://www.opengis.net/ont/geosparql#",
    schema: "https://schema.org/",
    foaf: "http://xmlns.com/foaf/0.1/",
    prov: "http://www.w3.org/ns/prov#",
    era: "http://data.europa.eu/949/",
    ex: "http://example.com/",
  };
  const RDF_TYPE = COMMON_PREFIXES.rdf + "type";
  const XSD_STRING = COMMON_PREFIXES.xsd + "string";
  let prefixes = { ...COMMON_PREFIXES }; // grows with the workspace's own @prefix lines

  function shortIri(iri) {
    let best = null;
    for (const [p, ns] of Object.entries(prefixes)) {
      if (iri.startsWith(ns) && /^[A-Za-z0-9_.-]*$/.test(iri.slice(ns.length)) && (!best || ns.length > best[1].length)) best = [p, ns];
    }
    return best ? best[0] + ":" + iri.slice(best[1].length) : "<" + iri + ">";
  }
  function termText(t) {
    if (!t) return "";
    if (t.type === "uri") return shortIri(t.value);
    if (t.type === "bnode") return "_:" + t.value;
    if (t.type === "literal" || t.type === "typed-literal") {
      const q = JSON.stringify(t.value);
      if (t["xml:lang"]) return q + "@" + t["xml:lang"];
      if (t.datatype && t.datatype !== XSD_STRING) return q + "^^" + shortIri(t.datatype);
      return q;
    }
    return String(t.value);
  }
  const termKey = (t) => (!t ? "" : t.type === "uri" ? "<" + t.value + ">" : t.type === "bnode" ? "_:" + t.value
    : JSON.stringify([t.value, t["xml:lang"] || "", t.datatype || XSD_STRING]));
  const tripleKey = (s, p, o) => termKey(s) + " " + termKey(p) + " " + termKey(o);
  /** A term as an element: IRIs and blank nodes open their describe view. */
  function termNode(t, mark) {
    if (!t) return el("span", { class: "muted" }, "–");
    const text = termText(t);
    if (t.type === "uri" || t.type === "bnode") {
      return el("button", { class: "link term", type: "button", title: t.type === "uri" ? t.value : "blank node",
        onclick: (e) => { e.stopPropagation(); describe(t, mark); } }, text);
    }
    return el("span", { class: "term lit", title: t.datatype || "" }, text);
  }
  /** The terms of one N-Triples line (what CONSTRUCT and DESCRIBE answer). */
  function parseNTriples(text) {
    const term = /\s*(<[^>]*>|_:\S+|"(?:[^"\\]|\\.)*"(?:@[A-Za-z0-9-]+|\^\^<[^>]*>)?)/y;
    const unescape = (s) => JSON.parse('"' + s.replace(/\\U([0-9A-Fa-f]{8})/g, (m, h) => String.fromCodePoint(parseInt(h, 16)).replace(/"/g, '\\"')) + '"');
    const toTerm = (s) => {
      if (s[0] === "<") return { type: "uri", value: s.slice(1, -1) };
      if (s[0] === "_") return { type: "bnode", value: s.slice(2) };
      const m = s.match(/^"((?:[^"\\]|\\.)*)"(?:@([A-Za-z0-9-]+)|\^\^<([^>]*)>)?$/);
      const t = { type: "literal", value: unescape(m[1]) };
      if (m[2]) t["xml:lang"] = m[2]; else t.datatype = m[3] || XSD_STRING;
      return t;
    };
    const out = [];
    for (const line of text.split("\n")) {
      if (!line.trim() || line.trim().startsWith("#")) continue;
      term.lastIndex = 0;
      const parts = [];
      for (let i = 0; i < 3; i++) {
        const m = term.exec(line);
        if (!m) break;
        parts.push(toTerm(m[1]));
      }
      if (parts.length === 3) out.push({ subject: parts[0], predicate: parts[1], object: parts[2] });
    }
    return out;
  }
  const ntTerm = (t) => (t.type === "uri" ? `<${t.value}>` : t.type === "bnode" ? "_:" + t.value
    : JSON.stringify(t.value) + (t["xml:lang"] ? "@" + t["xml:lang"] : t.datatype && t.datatype !== XSD_STRING ? `^^<${t.datatype}>` : ""));

  // ---------- examples ----------
  const PREFIXES = "@prefix sh: <http://www.w3.org/ns/shacl#> .\n@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .\n@prefix xsd: <http://www.w3.org/2001/XMLSchema#> .\n@prefix ex: <http://example.com/> .\n";
  const EXAMPLES = [
    { id: "validate", title: "Validate people and addresses",
      text: "Node and property shapes with counts, datatypes, patterns and classes; the data breaks a few of them.",
      files: {
        "data/people.ttl": PREFIXES + `
ex:alice a ex:Person ; ex:name "Alice" ; ex:age 34 ; ex:email "alice@example.com" ; ex:address ex:addr1 .
ex:bob a ex:Person ; ex:name "Bob" ; ex:age "forty" ; ex:email "bob-at-example" .
ex:carol a ex:Person ; ex:age 29 ; ex:address ex:bob .
ex:addr1 a ex:Address ; ex:city "Brussels" ; ex:postcode "1000" .
ex:acme a ex:Company ; ex:name "ACME" .
`,
        "shapes/person.ttl": PREFIXES + `
ex:PersonShape a sh:NodeShape ;
  sh:targetClass ex:Person ;
  sh:property [ sh:path ex:name ; sh:minCount 1 ; sh:maxCount 1 ; sh:datatype xsd:string ;
                sh:message "A person has exactly one name" ] ;
  sh:property [ sh:path ex:age ; sh:datatype xsd:integer ; sh:minInclusive 0 ] ;
  sh:property [ sh:path ex:email ; sh:pattern "^[^@ ]+@[^@ ]+$" ; sh:severity sh:Warning ] ;
  sh:property [ sh:path ex:address ; sh:class ex:Address ] .

ex:AddressShape a sh:NodeShape ;
  sh:targetClass ex:Address ;
  sh:property [ sh:path ex:postcode ; sh:minCount 1 ; sh:pattern "^[0-9]{4}$" ] .
`,
        "queries/people.rq": "PREFIX ex: <http://example.com/>\nSELECT ?person ?name ?age WHERE {\n  ?person a ex:Person .\n  OPTIONAL { ?person ex:name ?name }\n  OPTIONAL { ?person ex:age ?age }\n}\n",
        "README.md": "# Validate people and addresses\n\nOpen Validate and run (Ctrl+Enter). Fix the data in data/people.ttl, run again and watch results move to \"fixed\".\n",
      } },
    { id: "triple-rule", title: "Triple rules: types from properties",
      text: "sh:TripleRule infers a type for everything that has a property, and a second rule builds on the first.",
      files: {
        "data/vehicles.ttl": PREFIXES + `
ex:car1 ex:wheels 4 ; ex:seats 5 .
ex:bike1 ex:wheels 2 .
ex:truck1 ex:wheels 6 ; ex:payload 12000 .
`,
        "rules/types.ttl": PREFIXES + `
ex:WheeledShape a sh:NodeShape ;
  sh:targetSubjectsOf ex:wheels ;
  sh:rule [ a sh:TripleRule ; sh:order 1 ;
            sh:subject sh:this ; sh:predicate <http://www.w3.org/1999/02/22-rdf-syntax-ns#type> ; sh:object ex:Vehicle ] .

ex:VehicleShape a sh:NodeShape ;
  sh:targetClass ex:Vehicle ;
  sh:rule [ a sh:TripleRule ; sh:order 2 ;
            sh:subject sh:this ; sh:predicate ex:registered ; sh:object false ] .
`,
        "README.md": "# Triple rules\n\nOpen Rules and run. Click an inferred triple to see which rule and focus node made it. The second rule only fires on the types the first one inferred.\n",
      } },
    { id: "sparql-rule", title: "SPARQL rules: grandparents",
      text: "sh:SPARQLRule with a CONSTRUCT; the why? panel shows the bindings that made each triple.",
      files: {
        "data/family.ttl": PREFIXES + `
ex:ann ex:parentOf ex:ben , ex:cleo .
ex:ben ex:parentOf ex:dana .
ex:cleo ex:parentOf ex:eli , ex:fay .
`,
        "rules/family.ttl": PREFIXES + `
ex:FamilyShape a sh:NodeShape ;
  sh:targetSubjectsOf ex:parentOf ;
  sh:rule [ a sh:SPARQLRule ;
    rdfs:label "grandparent" ;
    sh:construct """
      PREFIX ex: <http://example.com/>
      CONSTRUCT { $this ex:grandparentOf ?grandchild }
      WHERE { $this ex:parentOf/ex:parentOf ?grandchild }
    """ ] .
`,
        "README.md": "# SPARQL rules\n\nRun Rules, then click a triple: the why? panel shows the rule's CONSTRUCT and the solution ($this, ?grandchild) that made it. \"Try in Query\" opens the rule as a query.\n",
      } },
    { id: "conditions", title: "Conditions and rule order",
      text: "sh:condition limits a rule to the focus nodes that conform to a shape; sh:order runs rules in turn.",
      files: {
        "data/sensors.ttl": PREFIXES + `
ex:s1 a ex:Sensor ; ex:reading 12.5 ; ex:max 20.0 .
ex:s2 a ex:Sensor ; ex:reading 31.0 ; ex:max 25.0 .
ex:s3 a ex:Sensor ; ex:max 25.0 .
`,
        "rules/alarms.ttl": PREFIXES + `
ex:HasReading a sh:NodeShape ;
  sh:property [ sh:path ex:reading ; sh:minCount 1 ] .

ex:SensorRules a sh:NodeShape ;
  sh:targetClass ex:Sensor ;
  sh:rule [ a sh:SPARQLRule ; sh:order 1 ;
    sh:condition ex:HasReading ;
    sh:construct """
      PREFIX ex: <http://example.com/>
      CONSTRUCT { $this a ex:Alarm }
      WHERE { $this ex:reading ?r ; ex:max ?m . FILTER (?r > ?m) }
    """ ] ;
  sh:rule [ a sh:TripleRule ; sh:order 2 ;
    sh:condition [ sh:property [ sh:path ex:reading ; sh:maxCount 0 ] ] ;
    sh:subject sh:this ; sh:predicate ex:status ; sh:object "no reading" ] .
`,
        "README.md": "# Conditions and order\n\nThe alarm rule only looks at sensors with a reading; the status rule only at those without one. The Rules table shows how many focus nodes each rule saw.\n",
      } },
    { id: "closure", title: "Fixpoint: transitive closure",
      text: "A rule that feeds itself: passes repeat until nothing new comes out. Compare with a single pass.",
      files: {
        "data/network.ttl": PREFIXES + `
ex:a ex:linkedTo ex:b . ex:b ex:linkedTo ex:c . ex:c ex:linkedTo ex:d . ex:d ex:linkedTo ex:e .
`,
        "rules/reach.ttl": PREFIXES + `
ex:ReachShape a sh:NodeShape ;
  sh:targetSubjectsOf ex:linkedTo , ex:reaches ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.com/>
      CONSTRUCT { $this ex:reaches ?y } WHERE { { $this ex:linkedTo ?y } UNION { $this ex:reaches/ex:linkedTo ?y } }
    """ ] .
`,
        "README.md": "# Fixpoint\n\nRun Rules with \"Fixpoint\": every node reaches all the nodes after it, found over several passes (see the pass column). With \"Single pass\" only the first step is there.\n",
      } },
    { id: "rules-then-shapes", title: "Rules, then validation",
      text: "Validation checks the data plus what the rules infer: a rule fills in a value a shape requires.",
      files: {
        "data/orders.ttl": PREFIXES + `
ex:o1 a ex:Order ; ex:net 100.0 ; ex:vatRate 0.21 .
ex:o2 a ex:Order ; ex:net 50.0 .
`,
        "rules/totals.ttl": PREFIXES + `
ex:OrderRules a sh:NodeShape ;
  sh:targetClass ex:Order ;
  sh:rule [ a sh:SPARQLRule ; sh:construct """
      PREFIX ex: <http://example.com/>
      CONSTRUCT { $this ex:total ?t } WHERE { $this ex:net ?n ; ex:vatRate ?v . BIND (?n * (1 + ?v) AS ?t) }
    """ ] .
`,
        "shapes/order.ttl": PREFIXES + `
ex:OrderShape a sh:NodeShape ;
  sh:targetClass ex:Order ;
  sh:property [ sh:path ex:total ; sh:minCount 1 ; sh:message "An order needs a total (from its VAT rate)" ] .
`,
        "README.md": "# Rules, then validation\n\nValidate with \"after rules\" on: only ex:o2 fails, because it has no VAT rate. Turn \"after rules\" off and both fail.\n",
      } },
  ];

  // ---------- file templates and snippets ----------
  const TEMPLATES = {
    data: PREFIXES + "\nex:thing1 a ex:Thing ; ex:name \"Thing 1\" .\n",
    shapes: PREFIXES + "\nex:ThingShape a sh:NodeShape ;\n  sh:targetClass ex:Thing ;\n  sh:property [ sh:path ex:name ; sh:minCount 1 ] .\n",
    rules: PREFIXES + "\nex:ThingRules a sh:NodeShape ;\n  sh:targetClass ex:Thing ;\n  sh:rule [ a sh:TripleRule ;\n    sh:subject sh:this ; sh:predicate ex:checked ; sh:object true ] .\n",
    queries: "PREFIX ex: <http://example.com/>\nSELECT * WHERE {\n  ?s ?p ?o\n} LIMIT 100\n",
    updates: "PREFIX ex: <http://example.com/>\nINSERT { ?s ex:seen true } WHERE { ?s a ex:Thing }\n",
    tests: "",
    lift: "",
  };
  const SNIPPETS = [
    ["node shape", "${Name}Shape a sh:NodeShape ;\n  sh:targetClass ${class} ;\n  sh:property [ sh:path ${path} ; sh:minCount 1 ] .\n"],
    ["property shape", "sh:property [\n  sh:path ${path} ;\n  ${}\n] ;"],
    ["minCount / maxCount", "sh:minCount ${1} ; sh:maxCount ${1} ;"],
    ["datatype", "sh:datatype xsd:${string} ;"],
    ["class", "sh:class ${class} ;"],
    ["pattern", "sh:pattern \"${^...$}\" ;"],
    ["in (list of values)", "sh:in ( ${values} ) ;"],
    ["node kind IRI", "sh:nodeKind sh:IRI ;"],
    ["value range", "sh:minInclusive ${0} ; sh:maxInclusive ${100} ;"],
    ["severity warning", "sh:severity sh:Warning ;"],
    ["message", "sh:message \"${message}\" ;"],
    ["SPARQL constraint", "sh:sparql [\n  sh:message \"${message}\" ;\n  sh:select \"\"\"\n    SELECT $this ?value WHERE {\n      $this ${path} ?value .\n      FILTER (${condition})\n    }\n  \"\"\"\n] ;"],
    ["triple rule", "sh:rule [ a sh:TripleRule ;\n  sh:subject sh:this ; sh:predicate ${predicate} ; sh:object ${object} ] ;"],
    ["SPARQL rule", "sh:rule [ a sh:SPARQLRule ;\n  sh:construct \"\"\"\n    CONSTRUCT { $this ${predicate} ?value }\n    WHERE { $this ${path} ?value }\n  \"\"\" ] ;"],
    ["rule condition", "sh:condition ${Shape} ;"],
    ["rule order", "sh:order ${1} ;"],
    ["prefix", "@prefix ${ex}: <${http://example.com/}> ."],
  ];
  const SHACL_TERMS = ("NodeShape PropertyShape targetClass targetNode targetSubjectsOf targetObjectsOf property path minCount maxCount datatype class nodeKind IRI Literal BlankNode BlankNodeOrIRI IRIOrLiteral "
    + "pattern flags minLength maxLength minInclusive maxInclusive minExclusive maxExclusive in hasValue languageIn uniqueLang equals disjoint lessThan lessThanOrEquals node qualifiedValueShape qualifiedMinCount qualifiedMaxCount "
    + "closed ignoredProperties not and or xone message severity Violation Warning Info deactivated name description order group sparql select ask prefixes declare prefix namespace "
    + "rule TripleRule SPARQLRule subject predicate object this construct condition inversePath alternativePath zeroOrMorePath oneOrMorePath zeroOrOnePath").split(" ");

  // ---------- state ----------
  let wsId = null, ws = null;
  /** path -> { state (EditorState), text (saved), dirty, error, timer } */
  let docs = new Map();
  let openPath = null;
  let view = null; // the one EditorView
  let lastQueryPath = null;
  let pane = "validate";
  let validation = null, previousValidation = null, inference = null, lastTests = null;
  let lastPipeline = null; // GET …/pipeline: the steps and what their last run did
  let historyRuns = [], historyLast = 0, historySeen = 0, historyTimer = null; // other clients' runs (#405)
  let queryStore = ""; // "": the workspace's scratch store; "N": the pipeline's store after step N
  let inferredByKey = new Map(); // tripleKey -> index into inference.triples
  let storeTerms = null; // classes and properties of the scratch store, for completion
  const tables = {}; // Tabulator instances by name
  let busy = {};

  const base = () => API + "/" + encodeURIComponent(wsId);
  const folderOf = (path) => (path.includes("/") ? path.split("/")[0] : "");
  const extOf = (path) => (path.match(/\.([a-z0-9]+)(\.gz)?$/i) || [])[1]?.toLowerCase() || "";
  const isRdf = (path) => ["data", "shapes", "rules"].includes(folderOf(path));
  function mediaType(path) {
    return { ttl: "text/turtle", nt: "application/n-triples", nq: "application/n-quads", trig: "application/trig", rdf: "application/rdf+xml",
      owl: "application/rdf+xml", jsonld: "application/ld+json", rq: "application/sparql-query", ru: "application/sparql-update",
      yaml: "application/yaml", yml: "application/yaml", md: "text/markdown", json: "application/json", csv: "text/csv" }[extOf(path)] || "text/plain";
  }

  // ---------- requests ----------
  async function request(method, path, { params, body, type, accept } = {}) {
    const url = new URL(path, location.origin);
    if (params) for (const [k, v] of Object.entries(params)) if (v != null && v !== "") url.searchParams.set(k, v);
    // The workbench's own requests: its history lists only other clients' runs.
    const headers = { "X-TripleTrail-Client": "workbench" };
    const token = ui.store.get(ui.TOKEN_KEY);
    if (token) headers.Authorization = "Bearer " + token;
    if (type) headers["Content-Type"] = type;
    if (accept) headers.Accept = accept;
    const r = await fetch(url, { method, headers, body });
    const text = await r.text();
    if (!r.ok) {
      if (r.status === 401) ui.openToken("The workspace needs the admin token.");
      throw new ui.ApiError(r.status, text.trim() || r.statusText);
    }
    return { text, type: (r.headers.get("Content-Type") || "").split(";")[0] };
  }
  const json = async (...a) => JSON.parse((await request(...a)).text || "null");

  // ---------- the list of workspaces ----------
  async function renderList(root) {
    teardown();
    if (location.hash.startsWith("#share=")) return openShared(root);
    const configure = new URLSearchParams(location.search).get("configure");
    if (configure) return startConfigure(root, configure);
    fill(root, el("div", { class: "toolbar" }, el("h1", {}, "Workbench"), el("span", { class: "grow" }),
      examplesMenu(), el("button", { class: "btn primary", type: "button", onclick: () => create({ name: "Untitled" }) }, "New workspace")));
    const intro = el("p", { class: "muted small wb-intro" },
      "A workspace is a folder of data, shapes, rules and queries with a scratch store of its own: try shapes and rules on a sample "
      + "without touching a dataset. Scratch workspaces are removed after a day unused; keep one to save it.");
    root.append(intro);
    let list;
    try { list = (await json("GET", API)).workspaces; } catch (e) {
      root.append(el("div", { class: "card" }, el("p", { class: "empty error" }, e.status === 404 ? "Workspaces are turned off on this server (--no-workspaces)." : e.message)));
      return;
    }
    list.sort((a, b) => (b.used || "").localeCompare(a.used || ""));
    const rows = list.map((w) => el("tr", { class: "clickable", onclick: (e) => { if (!e.target.closest("button")) ui.go("workbench/" + w.id); } },
      el("td", {}, el("span", { class: "name" }, w.name)),
      el("td", {}, el("span", { class: "chip" + (w.lifetime === "saved" ? " ro" : "") }, w.lifetime)),
      el("td", { class: "num" }, ui.fmtNum((w.files || []).length)),
      el("td", { class: "num" }, w.triples == null ? "–" : ui.fmtNum(w.triples)),
      el("td", {}, ui.fmtTime(w.used)),
      el("td", {}, w.expires ? ui.fmtTime(w.expires) : el("span", { class: "muted" }, "kept")),
      el("td", { class: "row-actions" }, el("button", { class: "btn small danger", type: "button", onclick: () => remove(w) }, "Delete"))));
    root.append(el("div", { class: "card table-wrap" },
      el("table", { class: "grid" }, el("thead", {}, el("tr", {}, ...["Name", "Kept", "Files", "Triples", "Last used", "Expires", ""].map((h, i) => el("th", { class: i === 2 || i === 3 ? "num" : null }, h)))),
        el("tbody", {}, rows)),
      list.length ? null : el("p", { class: "empty muted" }, "No workspaces yet. Start a new one, or open an example.")));
  }
  function examplesMenu() {
    const sel = el("select", { class: "btn", "aria-label": "Examples", title: "Start a workspace from an example",
      onchange: (e) => { const ex = EXAMPLES.find((x) => x.id === e.target.value); e.target.value = ""; if (ex) create({ name: ex.title, files: ex.files }); } },
    el("option", { value: "" }, "Examples…"), ...EXAMPLES.map((x) => el("option", { value: x.id, title: x.text }, x.title)));
    return sel;
  }
  async function create(body) {
    try {
      const w = await json("POST", API, { body: JSON.stringify(body), type: "application/json" });
      ui.go("workbench/" + w.id);
    } catch (e) { ui.fail(e); }
  }
  async function remove(w) {
    const r = await ui.confirmDialog("Delete " + w.name, "The workspace and its files are removed from the server.");
    if (!r.ok) return;
    try {
      await request("DELETE", API + "/" + encodeURIComponent(w.id));
      ui.toast("Deleted " + w.name, "ok");
      if (wsId === w.id) ui.go("workbench"); else render(ui, []);
    } catch (e) { ui.fail(e); }
  }

  // ---------- one workspace ----------
  function teardown() {
    for (const d of docs.values()) clearTimeout(d.timer);
    for (const t of Object.values(tables)) try { t.destroy(); } catch (e) { /* gone */ }
    for (const k of Object.keys(tables)) delete tables[k];
    view && view.destroy();
    view = null; wsId = null; ws = null; docs = new Map(); openPath = null; lastQueryPath = null;
    validation = previousValidation = inference = null; lastTests = null; lastPipeline = null; queryStore = ""; inferredByKey = new Map(); storeTerms = null;
    clearInterval(historyTimer); historyTimer = null; historyRuns = []; historyLast = historySeen = 0;
    prefixes = { ...COMMON_PREFIXES };
    if (aiAbort) aiAbort.abort();
    aiMessages = [];
  }

  async function openWorkspace(root, id, path) {
    if (wsId !== id) {
      teardown();
      fill(root, el("p", { class: "muted loading" }, "Opening the workspace…"));
      try {
        await libraries();
        wsId = id;
        ws = await json("GET", base());
      } catch (e) {
        wsId = null;
        fill(root, el("div", { class: "card" }, el("p", { class: "empty error" }, e.status === 404 ? "No workspace " + id + " (it may have expired)." : e.message)),
          el("p", {}, el("a", { href: "/$/ui/workbench", "data-route": "" }, "All workspaces")));
        return;
      }
      layout(root);
      await Promise.all(ws.files.filter((f) => !f.path.endsWith(".gz") && f.bytes < 4 << 20).map((f) => load(f.path)));
      learnPrefixes();
      renderTree();
      const first = path || ["README.md"].concat(ws.files.map((f) => f.path)).find((p) => docs.has(p) && (p === "README.md" || /^(shapes|rules|queries)\//.test(p)))
        || ws.files.map((f) => f.path).find((p) => docs.has(p));
      const ai = new URLSearchParams(location.search).get("ai");
      if (first) openFile(first);
      else showEmptyEditor();
      if (ai) { aiPreferred = ai; aiDatasets(); showPane("ai"); }
      else showPane(ws.files.some((f) => f.path.startsWith("shapes/")) ? "validate" : ws.files.some((f) => f.path.startsWith("rules/")) ? "rules" : "query");
    } else if (path && path !== openPath && docs.has(path)) {
      openFile(path);
    }
  }

  function layout(root) {
    const bar = el("div", { class: "wb-bar" },
      el("a", { class: "btn small", href: "/$/ui/workbench", "data-route": "", title: "All workspaces" }, "←"),
      el("input", { class: "wb-name", value: ws.name, "aria-label": "Workspace name",
        onchange: (e) => patch({ name: e.target.value.trim() || "Untitled" }) }),
      el("span", { id: "wb-life" }),
      el("span", { class: "grow" }),
      el("label", { class: "check small", title: "Save and run the open tab while you type (small workspaces)" }, el("input", { type: "checkbox", id: "wb-auto" }), "Run while typing"),
      examplesMenu(),
      el("button", { class: "btn", type: "button", onclick: sampleDialog, title: "Copy a sample of a dataset into data/" }, "Sample…"),
      el("button", { class: "btn", type: "button", onclick: overlayDialog, title: "Work on a whole dataset: its quads as they are now, read only, with this workspace's changes on top" }, "Overlay…"),
      el("button", { class: "btn", type: "button", onclick: promoteDialog, title: "Make the shapes of shapes/ a dataset's own shapes (admin)" }, "Promote shapes…"),
      el("button", { class: "btn", type: "button", onclick: pullDialog, title: "Write the files of a folder of a git repository over the workspace's (admin)" }, "Pull…"),
      el("button", { class: "btn", type: "button", onclick: shareDialog, title: "A link that holds the workspace's files: opening it makes a copy" }, "Share link…"),
      el("button", { class: "btn", type: "button", onclick: () => remove({ id: wsId, name: ws.name }) }, "Delete"));
    const tree = el("nav", { class: "wb-tree card", id: "wb-tree", "aria-label": "Files" });
    const head = el("div", { class: "wb-ed-head" }, el("span", { class: "mono", id: "wb-path" }), el("span", { class: "small", id: "wb-saved" }),
      el("span", { class: "muted small" }, "Ctrl+Enter runs · Ctrl+S saves"));
    const editor = el("div", { class: "wb-editor", id: "wb-editor" });
    const panes = el("div", { class: "wb-panel card" },
      el("div", { class: "wb-tabs", role: "tablist" },
        ...[["query", "Query"], ["validate", "Validate"], ["rules", "Rules"], ["reason", "Reason"], ["tests", "Tests"], ["pipeline", "Pipeline"], ["history", "History"], ["ai", "AI"]].map(([k, label]) =>
          el("button", { type: "button", role: "tab", "data-pane": k, onclick: () => showPane(k) }, label,
            k === "history" ? el("span", { class: "wb-badge", id: "wb-h-badge", hidden: true }) : null)),
        el("span", { class: "grow" }),
        el("button", { class: "btn small primary", type: "button", id: "wb-run", onclick: () => run() }, "Run")),
      el("div", { class: "wb-pane", id: "wb-pane-query", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-validate", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-rules", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-reason", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-tests", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-pipeline", hidden: true }),
      el("div", { class: "wb-pane", id: "wb-pane-history", hidden: true }),
      el("div", { class: "wb-pane wb-pane-ai", id: "wb-pane-ai", hidden: true }));
    const drawer = el("aside", { class: "wb-drawer card", id: "wb-drawer", hidden: true },
      el("div", { class: "dlg-head" }, el("h2", { id: "wb-drawer-title" }), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: closeDrawer }, "✕")),
      el("div", { class: "wb-drawer-body", id: "wb-drawer-body" }));
    fill(root, el("div", { class: "wb" }, bar,
      el("div", { class: "wb-body" }, tree, el("div", { class: "wb-main" }, el("div", { class: "wb-ed card" }, head, editor), panes), drawer)));
    renderLife();
    buildQueryPane(); buildValidatePane(); buildRulesPane(); buildReasonPane(); buildTestsPane(); buildPipelinePane(); buildHistoryPane(); buildAiPane();
    loadHistory();
    historyTimer = setInterval(() => { if (!document.hidden) loadHistory(); }, 5000);
    const CM = window.CM;
    view = new CM.EditorView({ parent: editor, state: CM.EditorState.create({ doc: "" }) });
  }
  function renderLife() {
    const span = $("#wb-life");
    if (!span) return;
    const kept = ws.lifetime === "saved";
    fill(span, el("span", { class: "chip" + (kept ? " ro" : ""), title: kept ? "Kept until deleted" : "Removed when unused until " + ui.fmtTime(ws.expires) }, kept ? "kept" : "scratch"),
      kept ? null : el("button", { class: "link small", type: "button", onclick: () => patch({ lifetime: "saved" }) }, "Keep"),
      overlayStatus());
  }

  // ---------- overlays (#406) ----------
  // An overlay workspace reads a whole dataset, as it was when taken, and keeps its own
  // changes (updates, pipeline steps) apart: the dataset is never written. The changes go as
  // a patch, for an explicit apply.
  function overlayStatus() {
    const o = ws.overlay;
    if (!o) return null;
    const n = o.changes;
    const label = o.lost ? `overlay of ${o.dataset}: lost` : `overlay of ${o.dataset}` + (n == null ? "" : n ? ` · ~${ui.fmtNum(n)} change${n === 1 ? "" : "s"}` : " · unchanged");
    return el("button", { class: "chip wb-overlay" + (o.lost ? " bad" : ""), type: "button", onclick: overlayDialog,
      title: o.lost ? "The server restarted since the overlay was taken: its changes are gone" : `${ui.fmtNum(o.quads)} quads of ${o.dataset} as taken ${ui.fmtTime(o.at)}, read only; the changes stay here` }, label);
  }
  async function refreshOverlay() {
    if (!ws.overlay) return;
    try { ws.overlay = await json("GET", base() + "/overlay"); renderLife(); } catch (e) { /* keep the old one */ }
  }
  async function takeOverlay(dataset) {
    try {
      ws.overlay = await json("POST", base() + "/overlay", { body: JSON.stringify({ dataset }), type: "application/json" });
      storeTerms = null;
      renderLife();
      ui.toast(`Overlay of ${dataset}: ${ui.fmtNum(ws.overlay.quads)} quads, read only. Updates and pipeline steps change this workspace only.`, "ok");
      return true;
    } catch (e) { ui.fail(e); return false; }
  }
  async function dropOverlayChanges() {
    if (!confirm("Drop the overlay's changes? Export them as a patch first to keep them.")) return;
    try { await request("DELETE", base() + "/overlay/changes"); storeTerms = null; await refreshOverlay(); ui.toast("Back to the dataset as taken.", "ok"); } catch (e) { ui.fail(e); }
  }
  async function detachOverlay() {
    if (!confirm("Make this a plain workspace again? The overlay's changes go.")) return;
    try { await request("DELETE", base() + "/overlay"); storeTerms = null; await refreshWorkspace(); renderLife(); } catch (e) { ui.fail(e); }
  }
  async function downloadPatch() {
    try {
      const r = await request("GET", base() + "/overlay/patch");
      save_(r.text, (ws.name || "overlay").replace(/[^\w.-]+/g, "_") + ".rdfp", "application/rdf-patch");
    } catch (e) { ui.fail(e); }
  }
  async function overlayDialog() {
    let datasets = [];
    try { datasets = (await ui.api("GET", "/$/datasets")).datasets; } catch (e) { ui.fail(e); return; }
    if (!datasets.length) { ui.toast("No dataset to work on."); return; }
    const d = el("dialog", { class: "dlg" });
    const ds = el("select", {}, datasets.map((x) => el("option", { value: x.name, selected: ws.overlay && ws.overlay.dataset === x.name }, x.name)));
    const o = ws.overlay;
    const act = (f) => async () => { d.close(); await f(); };
    const current = o && el("div", { class: "small" },
      el("p", {}, o.lost
        ? `This workspace was an overlay of ${o.dataset}; the server restarted since, and its changes are gone.`
        : `Now an overlay of ${o.dataset}: ${ui.fmtNum(o.quads)} quads as taken ${ui.fmtTime(o.at)}` + (o.changes == null ? "." : `, about ${ui.fmtNum(o.changes)} changed.`)),
      el("div", { class: "row" },
        o.lost ? null : el("button", { class: "btn small", type: "button", onclick: act(downloadPatch), title: "The changes as an RDF Patch, to apply to the dataset after review" }, "Download patch"),
        o.lost ? null : el("button", { class: "btn small", type: "button", onclick: act(dropOverlayChanges), title: "Back to the dataset as taken" }, "Drop changes"),
        el("button", { class: "btn small", type: "button", onclick: act(detachOverlay), title: "A plain workspace again" }, "Detach")));
    d.append(el("form", { method: "dialog", onsubmit: async (e) => { e.preventDefault(); if (await takeOverlay(ds.value)) d.close(); } },
      el("div", { class: "dlg-head" }, el("h2", {}, "Overlay a dataset"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, "The workspace's store becomes the whole dataset as it is now, read only, with the files of data/, shapes/ and rules/ on top. Updates and pipeline steps change the workspace only: the dataset is never written, and its writers go on. Nothing is copied; the changes take memory in proportion to their size."),
        el("p", { class: "muted small" }, "Export the changes as an RDF Patch to apply them after review. They are kept in memory: a restart of the server loses them."),
        current || null,
        ws.overlay ? el("p", { class: "small" }, "Taking a dataset again drops the current changes.") : null,
        el("label", { class: "field" }, "Dataset", ds)),
      el("div", { class: "dlg-foot" }, el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Cancel"),
        el("button", { class: "btn primary", type: "submit" }, ws.overlay ? "Take again" : "Take"))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }
  async function patch(body) {
    try { ws = await json("PATCH", base(), { body: JSON.stringify(body), type: "application/json" }); renderLife(); } catch (e) { ui.fail(e); }
  }
  async function refreshWorkspace() {
    try { ws = await json("GET", base()); renderTree(); } catch (e) { /* keep the old one */ }
  }

  // ---------- files ----------
  async function load(path) {
    const r = await request("GET", base() + "/files/" + path);
    docs.set(path, { text: r.text, state: null, dirty: false, error: null, timer: null });
  }
  function learnPrefixes() {
    for (const d of docs.values()) {
      const text = d.state ? d.state.doc.toString() : d.text;
      for (const m of text.matchAll(/(?:@prefix|PREFIX)\s+([A-Za-z][\w.-]*)?:\s*<([^>]*)>/gi)) if (m[1] && !prefixes[m[1]]) prefixes[m[1]] = m[2];
    }
  }
  const FOLDERS = ["data", "shapes", "rules", "queries", "updates", "tests"];
  function renderTree() {
    const tree = $("#wb-tree");
    if (!tree || !ws) return;
    const counts = new Map(ws.files.map((f) => [f.path, f]));
    const paths = new Set([...ws.files.map((f) => f.path), ...docs.keys()]);
    const item = (path) => {
      const d = docs.get(path), f = counts.get(path);
      return el("li", { class: path === openPath ? "on" : null },
        el("button", { class: "wb-file", type: "button", title: path + (f ? ` · ${ui.fmtBytes(f.bytes)}` + (f.triples != null ? ` · ${ui.fmtNum(f.triples)} triples` : "") : ""),
          onclick: () => (d ? openFile(path) : ui.toast(path + " is too large to edit here; it is still in the store."))
        }, el("span", { class: "dot " + (d && d.error ? "bad" : d && d.dirty ? "warn" : "") + (d ? "" : " off") }), el("span", { class: "wb-fname" }, path.split("/").slice(1).join("/") || path),
        f && f.triples != null ? el("span", { class: "muted small wb-count" }, ui.fmtNum(f.triples)) : null),
        el("button", { class: "icon-btn wb-del", type: "button", title: "Delete " + path, "aria-label": "Delete " + path, onclick: () => deleteFile(path) }, "✕"));
    };
    const top = [...paths].filter((p) => !p.includes("/")).sort();
    fill(tree,
      ...FOLDERS.map((folder) => {
        const inside = [...paths].filter((p) => folderOf(p) === folder).sort();
        return el("div", { class: "wb-folder" },
          el("div", { class: "wb-folder-head" }, el("b", {}, folder + "/"),
            el("span", { class: "grow" }),
            el("button", { class: "icon-btn", type: "button", title: "Upload into " + folder + "/", "aria-label": "Upload into " + folder, onclick: () => upload(folder) }, "⇪"),
            el("button", { class: "icon-btn", type: "button", title: "New file in " + folder + "/", "aria-label": "New file in " + folder, onclick: () => newFile(folder) }, "+")),
          inside.length ? el("ul", {}, inside.map(item)) : null);
      }),
      el("div", { class: "wb-folder" }, el("ul", {}, top.map(item)),
        top.includes("README.md") ? null : el("button", { class: "link small", type: "button", onclick: () => newFile("", "README.md") }, "+ README.md")),
      el("p", { class: "muted small wb-total" }, ws.overlay && !ws.overlay.lost
        ? `${ui.fmtNum(ws.overlay.quads)} quads of ${ws.overlay.dataset}` + (ws.triples ? `, ${ui.fmtNum(ws.triples)} triples of files` : "") + " in the store"
        : ws.triples != null ? `${ui.fmtNum(ws.triples)} triples in the store` : ""));
  }
  async function newFile(folder, fixed) {
    const ext = { data: "ttl", shapes: "ttl", rules: "ttl", queries: "rq", updates: "ru", tests: "yaml", lift: "rq" }[folder] || "md";
    const name = fixed || prompt(`New file in ${folder}/`, `${folder === "queries" ? "query" : folder === "shapes" ? "shape" : folder === "rules" ? "rule" : "file"}-${docs.size + 1}.${ext}`);
    if (!name) return;
    const path = fixed ? name : folder + "/" + name.replace(/^\/+/, "");
    if (docs.has(path)) { openFile(path); return; }
    const text = fixed ? "# " + ws.name + "\n" : TEMPLATES[folder] || "";
    try {
      await request("PUT", base() + "/files/" + path, { body: text, type: mediaType(path) });
      docs.set(path, { text, state: null, dirty: false, error: null, timer: null });
      await refreshWorkspace();
      openFile(path);
    } catch (e) { ui.fail(e); }
  }
  function upload(folder) {
    const input = el("input", { type: "file", multiple: true });
    input.addEventListener("change", async () => {
      for (const file of input.files) {
        const path = folder + "/" + file.name.replace(/[^A-Za-z0-9._-]/g, "_");
        try {
          await request("PUT", base() + "/files/" + path, { body: file, type: mediaType(path) });
          if (!path.endsWith(".gz") && file.size < 4 << 20) await load(path);
          ui.toast("Uploaded " + path, "ok");
        } catch (e) { ui.fail(e); }
      }
      learnPrefixes();
      storeTerms = null;
      await refreshWorkspace();
    });
    input.click();
  }
  async function deleteFile(path) {
    const r = await ui.confirmDialog("Delete " + path, "The file is removed from the workspace and its triples from the store.");
    if (!r.ok) return;
    try {
      await request("DELETE", base() + "/files/" + path);
      const d = docs.get(path);
      if (d) clearTimeout(d.timer);
      docs.delete(path);
      if (openPath === path) { openPath = null; showEmptyEditor(); }
      storeTerms = null;
      await refreshWorkspace();
    } catch (e) { ui.fail(e); }
  }

  // ---------- the editor ----------
  function language(path) {
    const CM = window.CM;
    const ext = extOf(path);
    if (["ttl", "nt", "nq", "trig"].includes(ext)) return CM.StreamLanguage.define(CM.turtle);
    if (["rq", "ru"].includes(ext)) return CM.StreamLanguage.define(CM.sparql);
    if (["yaml", "yml"].includes(ext)) return CM.StreamLanguage.define(CM.yaml);
    return null;
  }
  function highlight() {
    const CM = window.CM, t = CM.tags;
    return CM.HighlightStyle.define([
      { tag: [t.keyword, t.modifier], color: "var(--cm-keyword)", fontWeight: "600" },
      { tag: [t.atom, t.bool, t.special(t.variableName)], color: "var(--cm-atom)" },
      { tag: [t.string, t.special(t.string)], color: "var(--cm-string)" },
      { tag: [t.number], color: "var(--cm-number)" },
      { tag: [t.comment, t.lineComment, t.blockComment], color: "var(--cm-comment)", fontStyle: "italic" },
      { tag: [t.variableName, t.definition(t.variableName)], color: "var(--cm-variable)" },
      { tag: [t.typeName, t.meta, t.standard(t.variableName)], color: "var(--cm-type)" },
      { tag: [t.operator, t.punctuation, t.bracket], color: "var(--cm-punct)" },
      { tag: [t.propertyName, t.tagName], color: "var(--cm-property)" },
    ]);
  }
  function extensions(path) {
    const CM = window.CM;
    const lang = language(path);
    return [
      CM.lineNumbers(), CM.highlightActiveLineGutter(), CM.highlightActiveLine(), CM.drawSelection(), CM.history(), CM.foldGutter(),
      CM.indentOnInput(), CM.bracketMatching(), CM.closeBrackets(), CM.highlightSelectionMatches(), CM.lintGutter(),
      CM.syntaxHighlighting(highlight()),
      lang ? lang : [],
      CM.autocompletion({ override: [complete], activateOnTyping: true }),
      CM.hoverTooltip(hover, { hoverTime: 400 }),
      CM.keymap.of([
        { key: "Mod-Enter", run: () => { run(); return true; } },
        { key: "Mod-s", run: () => { save(openPath, true); return true; }, preventDefault: true },
        ...CM.closeBracketsKeymap, ...CM.defaultKeymap, ...CM.searchKeymap, ...CM.historyKeymap, ...CM.foldKeymap,
        ...CM.completionKeymap, ...CM.lintKeymap, CM.indentWithTab,
      ]),
      CM.EditorView.updateListener.of((u) => { if (u.docChanged) edited(path); }),
      CM.EditorView.theme({ "&": { height: "100%" }, ".cm-scroller": { fontFamily: "var(--mono)", fontSize: "13px" } }),
      /\.(md|txt)$/.test(path) ? CM.EditorView.lineWrapping : [],
    ];
  }
  /** Opens `path` in the editor; a query also shows the Query pane, unless `stay`. */
  function openFile(path, { stay = false } = {}) {
    const d = docs.get(path);
    if (!d || !view) return;
    if (openPath && docs.has(openPath)) docs.get(openPath).state = view.state;
    openPath = path;
    if (/^(queries|updates)\//.test(path)) lastQueryPath = path;
    if (!d.state) d.state = window.CM.EditorState.create({ doc: d.text, extensions: extensions(path) });
    view.setState(d.state);
    showDiagnostics(path);
    $("#wb-path").textContent = path;
    savedLabel();
    renderTree();
    // ?ai= stays, so that a reload opens the AI tab on the same dataset again.
    const ai = new URLSearchParams(location.search).get("ai");
    const url = "/$/ui/workbench/" + encodeURIComponent(wsId) + "/" + path + (ai ? "?ai=" + encodeURIComponent(ai) : "");
    if (location.pathname + location.search !== url) history.replaceState(null, "", url);
    if (/^queries\//.test(path) && pane !== "query" && !stay) showPane("query");
    view.focus();
  }
  function showEmptyEditor() {
    view && view.setState(window.CM.EditorState.create({ doc: "", extensions: [window.CM.EditorView.editable.of(false), window.CM.placeholder("Pick a file, or add one with + next to a folder.")] }));
    $("#wb-path").textContent = "";
    $("#wb-saved").textContent = "";
  }
  function savedLabel() {
    const d = docs.get(openPath);
    const s = $("#wb-saved");
    if (!d || !s) return;
    s.className = "small " + (d.error ? "error" : "muted");
    s.textContent = d.error ? "not saved: " + d.error.message : d.dirty ? "editing…" : "saved";
    s.title = d.error ? d.error.message : "";
  }
  function edited(path) {
    const d = docs.get(path);
    if (!d) return;
    d.dirty = true;
    clearTimeout(d.timer);
    d.timer = setTimeout(() => save(path, false), 700);
    savedLabel();
  }
  /** Saves a file (the server parses RDF files: a syntax error comes back with its line). */
  async function save(path, now) {
    const d = docs.get(path);
    if (!d) return true;
    clearTimeout(d.timer);
    const text = path === openPath ? view.state.doc.toString() : d.state ? d.state.doc.toString() : d.text;
    if (text === d.text && !d.error) { d.dirty = false; savedLabel(); renderTree(); return true; }
    try {
      await request("PUT", base() + "/files/" + path, { body: text, type: mediaType(path) });
      d.text = text; d.dirty = false; d.error = null;
      if (isRdf(path)) { storeTerms = null; learnPrefixes(); refreshWorkspace(); }
      showDiagnostics(path);
      savedLabel();
      renderTree();
      if (!now && $("#wb-auto") && $("#wb-auto").checked && (ws.triples || 0) <= 50000) run();
      return true;
    } catch (e) {
      d.error = { message: e.message.replace(/^[^:]*: /, ""), ...position(e.message, text) };
      showDiagnostics(path);
      savedLabel();
      renderTree();
      if (now) ui.toast(e.message, "bad");
      return false;
    }
  }
  async function saveAll() {
    let ok = true;
    for (const [path, d] of docs) if (d.dirty || d.error) ok = (await save(path, true)) && ok;
    return ok;
  }
  /** The line and column of a parser message: Turtle's "at line 3 between columns 1 and 5" or
   *  SPARQL's "error at 3:11". */
  function position(message, text) {
    let m = message.match(/line (\d+)(?:,? (?:between )?columns? (\d+)(?: and (\d+))?)?/);
    let line, from, to;
    if (m) { line = +m[1]; from = m[2] ? +m[2] : 1; to = m[3] ? +m[3] : null; }
    else if ((m = message.match(/error at (\d+):(\d+)/))) { line = +m[1]; from = +m[2]; }
    else return {};
    const lines = text.split("\n");
    line = Math.min(Math.max(line, 1), lines.length);
    let offset = 0;
    for (let i = 0; i < line - 1; i++) offset += lines[i].length + 1;
    const len = lines[line - 1].length;
    const a = offset + Math.min(Math.max(from - 1, 0), len);
    const b = to ? offset + Math.min(Math.max(to - 1, from), len) : Math.max(a + 1, offset + len);
    return { from: a, to: Math.min(b, text.length) };
  }
  function showDiagnostics(path) {
    if (path !== openPath || !view) return;
    const d = docs.get(path);
    const diags = d && d.error && d.error.from != null
      ? [{ from: d.error.from, to: Math.max(d.error.to, d.error.from), severity: "error", message: d.error.message }]
      : d && d.error ? [{ from: 0, to: 0, severity: "error", message: d.error.message }] : [];
    view.dispatch(window.CM.setDiagnostics(view.state, diags));
  }

  // ---------- completion and hover ----------
  async function loadStoreTerms() {
    if (storeTerms) return storeTerms;
    storeTerms = { classes: [], properties: [] };
    try {
      const q = (query) => json("POST", base() + "/query", { body: new URLSearchParams({ query }), type: "application/x-www-form-urlencoded", accept: "application/sparql-results+json" });
      const [c, p] = await Promise.all([
        q("SELECT DISTINCT ?x WHERE { { ?s a ?x } UNION { GRAPH ?g { ?s a ?x } } } LIMIT 2000"),
        q("SELECT DISTINCT ?x WHERE { { ?s ?x ?o } UNION { GRAPH ?g { ?s ?x ?o } } } LIMIT 2000"),
      ]);
      storeTerms = { classes: c.results.bindings.map((b) => b.x.value), properties: p.results.bindings.map((b) => b.x.value) };
    } catch (e) { /* completion without the store */ }
    return storeTerms;
  }
  function docPrefixes(text) {
    const out = {};
    for (const m of text.matchAll(/(?:@prefix|PREFIX)\s+([A-Za-z][\w.-]*)?:\s*<([^>]*)>/gi)) out[m[1] || ""] = m[2];
    return out;
  }
  async function complete(context) {
    const CM = window.CM;
    const path = openPath || "";
    const line = context.state.doc.lineAt(context.pos);
    const before = line.text.slice(0, context.pos - line.from);
    // A prefix declaration: the well-known namespaces.
    const decl = before.match(/(@prefix|PREFIX)\s+([\w.-]*)$/i);
    if (decl) {
      const turtle = decl[1] === "@prefix";
      return { from: context.pos - decl[2].length, options: Object.entries({ ...COMMON_PREFIXES, ...prefixes }).map(([p, ns]) =>
        ({ label: p + ":", detail: ns, apply: `${p}: <${ns}>${turtle ? " ." : ""}`, type: "namespace" })) };
    }
    const word = context.matchBefore(/[A-Za-z][\w.-]*:[\w.-]*|[A-Za-z][\w-]*/);
    if (!word || (word.from === word.to && !context.explicit)) return null;
    const text = context.state.doc.toString();
    const local = docPrefixes(text);
    const nsToPrefix = Object.entries(local).map(([p, ns]) => [ns, p]).sort((a, b) => b[0].length - a[0].length);
    const name = (iri) => { const hit = nsToPrefix.find(([ns]) => iri.startsWith(ns) && /^[\w.-]*$/.test(iri.slice(ns.length))); return hit ? hit[1] + ":" + iri.slice(hit[0].length) : null; };
    const options = new Map();
    const add = (label, type, detail, boost = 0) => { if (label && !options.has(label)) options.set(label, { label, type, detail, boost }); };
    if (word.text.includes(":")) {
      const terms = await loadStoreTerms();
      for (const c of terms.classes) add(name(c), "class", "class in the store", 2);
      for (const p of terms.properties) add(name(p), "property", "property in the store", 1);
      if (local.sh) for (const t of SHACL_TERMS) add(local.sh + "" === COMMON_PREFIXES.sh ? "sh:" + t : null, "keyword", "SHACL");
      // Names used in any file of the workspace.
      for (const d of docs.values()) {
        const t = d.state ? d.state.doc.toString() : d.text;
        if (t.length > 2e6) continue;
        for (const m of t.matchAll(/(?<![\w<"])([A-Za-z][\w-]*):([A-Za-z_][\w-]*(?:\.[\w-]+)*)/g)) if (local[m[1]] != null) add(m[1] + ":" + m[2], "variable", "in the workspace");
      }
      // Undeclared prefixes: offer the namespace too.
      const p = word.text.split(":")[0];
      if (local[p] == null && (prefixes[p] || COMMON_PREFIXES[p])) add(p + ":", "namespace", "add @prefix " + p + ": first");
    }
    if (/\.(ttl|trig)$/.test(path) || /^(shapes|rules)\//.test(path)) {
      for (const [label, body] of SNIPPETS) options.set("snip:" + label, CM.snippetCompletion(body, { label, type: "text", detail: "snippet", boost: -1 }));
    }
    return { from: word.from, options: [...options.values()], validFor: /^[\w.:-]*$/ };
  }
  const labelCache = new Map();
  function hover(view, pos) {
    const line = view.state.doc.lineAt(pos);
    const col = pos - line.from;
    const re = /<([^>\s]+)>|(?<![\w<"])([A-Za-z][\w-]*)?:([A-Za-z_][\w-]*(?:\.[\w-]+)*)?/g;
    let m, hit = null;
    while ((m = re.exec(line.text))) if (m.index <= col && col <= m.index + m[0].length) { hit = m; break; }
    if (!hit) return null;
    let iri = hit[1];
    if (!iri) {
      const ns = docPrefixes(view.state.doc.toString())[hit[2] || ""];
      if (ns == null) return null;
      iri = ns + (hit[3] || "");
    }
    return {
      pos: line.from + hit.index, end: line.from + hit.index + hit[0].length, above: true,
      create: () => {
        const dom = el("div", { class: "wb-hover" }, el("div", { class: "mono small" }, iri), el("div", { class: "muted small" }, "…"));
        describeSummary(iri).then((s) => dom.lastChild.replaceWith(el("div", {},
          s.label ? el("div", {}, el("b", {}, s.label)) : null,
          el("div", { class: "muted small" }, `${ui.fmtNum(s.out)} triples as subject, ${ui.fmtNum(s.in)} as object` + (s.types.length ? " · " + s.types.map(shortIri).join(", ") : "")),
          el("button", { class: "link small", type: "button", onclick: () => describe({ type: "uri", value: iri }) }, "Describe"))));
        return { dom };
      },
    };
  }
  async function describeSummary(iri) {
    if (labelCache.has(iri)) return labelCache.get(iri);
    const q = `SELECT (SAMPLE(?l) AS ?label) (GROUP_CONCAT(DISTINCT STR(?t); separator=" ") AS ?types) (COUNT(DISTINCT ?o) AS ?out)
      (SUM(IF(BOUND(?in), 1, 0)) AS ?inn) WHERE { { <${iri}> ?p ?o OPTIONAL { <${iri}> <http://www.w3.org/2000/01/rdf-schema#label>|<http://www.w3.org/2004/02/skos/core#prefLabel> ?l }
      OPTIONAL { <${iri}> a ?t } } UNION { ?in ?q <${iri}> } }`;
    let s = { label: null, types: [], out: 0, in: 0 };
    try {
      const r = await json("POST", base() + "/query", { body: new URLSearchParams({ query: q }), type: "application/x-www-form-urlencoded", accept: "application/sparql-results+json" });
      const b = r.results.bindings[0] || {};
      s = { label: b.label && b.label.value, types: b.types && b.types.value ? b.types.value.split(" ") : [], out: +(b.out && b.out.value || 0), in: +(b.inn && b.inn.value || 0) };
    } catch (e) { /* not in the store */ }
    labelCache.set(iri, s);
    setTimeout(() => labelCache.delete(iri), 10000);
    return s;
  }

  // ---------- panes ----------
  function showPane(k) {
    pane = k;
    for (const b of document.querySelectorAll(".wb-tabs [data-pane]")) b.setAttribute("aria-selected", String(b.dataset.pane === k));
    for (const p of ["query", "validate", "rules", "reason", "tests", "pipeline", "history", "ai"]) { const n = $("#wb-pane-" + p); if (n) n.hidden = p !== k; }
    if (k === "pipeline") loadPipeline();
    if (k === "history") { historySeen = historyLast; renderHistoryBadge(); }
    for (const t of Object.values(tables)) try { t.redraw(); } catch (e) { /* hidden */ }
  }
  async function run() {
    if (!wsId) return;
    if (busy[pane]) return;
    busy[pane] = true;
    const btn = $("#wb-run");
    btn.disabled = true;
    try {
      if (!(await saveAll())) { ui.toast("A file has a syntax error: fix it first (its line is marked).", "bad"); return; }
      if (pane === "query") await runQuery();
      else if (pane === "validate") await runValidate();
      else if (pane === "tests") await runTests();
      else if (pane === "pipeline") await runPipeline({});
      else if (pane === "history") await loadHistory();
      else if (pane === "ai") await aiAsk();
      else if (pane === "reason") await runReason();
      else await runRules();
    } finally { busy[pane] = false; btn.disabled = false; }
  }
  function table(name, holder, options) {
    if (tables[name]) { try { tables[name].destroy(); } catch (e) { /* gone */ } }
    tables[name] = new window.Tabulator(holder, { layout: "fitDataStretch", maxHeight: "100%", placeholder: "Nothing", ...options });
    return tables[name];
  }
  // Tabulator puts a string a formatter returns into the page as HTML: text from the data goes in
  // as a text node.
  const textCell = (text) => document.createTextNode(text == null ? "" : String(text));
  const termCol = (field, title, extra = {}) => ({ title, field, headerFilter: "input", headerFilterFunc: (v, cell) => termText(cell).toLowerCase().includes(v.toLowerCase()),
    sorter: (a, b) => termText(a).localeCompare(termText(b)), formatter: (cell) => termNode(cell.getValue()), ...extra });

  // Query
  function buildQueryPane() {
    fill($("#wb-pane-query"),
      el("div", { class: "wb-pane-bar" },
        el("span", { class: "small", id: "wb-q-source" }),
        el("span", { class: "grow" }),
        el("select", { id: "wb-q-store", class: "small", title: "The store the query runs on", "aria-label": "Store", onchange: (e) => { queryStore = e.target.value; } },
          el("option", { value: "" }, "Workspace store")),
        el("button", { class: "btn small", type: "button", onclick: () => newFile("queries") }, "New query"),
        el("button", { class: "btn small", type: "button", onclick: explain, title: "Run the query and show its plan with row counts and timings" }, "Explain"),
        el("button", { class: "btn small", type: "button", onclick: () => ui.go("query/workspace:" + encodeURIComponent(wsId)), title: "Query this workspace's store in MatGUI" }, "Open in MatGUI"),
        copyAsPythonButton("query"), saveAsTestButton("query")),
      el("div", { class: "wb-summary small", id: "wb-q-summary" }),
      el("div", { class: "wb-table", id: "wb-q-table" }));
    queryLabel();
  }
  function queryLabel() {
    const s = $("#wb-q-source");
    if (s) fill(s, lastQueryPath ? el("span", {}, "Runs ", el("code", {}, lastQueryPath)) : el("span", { class: "muted" }, "Open or add a file in queries/ to run it here."));
  }
  async function runQuery() {
    queryLabel();
    if (!lastQueryPath) { await newFile("queries"); return; }
    const d = docs.get(lastQueryPath);
    const text = lastQueryPath === openPath ? view.state.doc.toString() : d.state ? d.state.doc.toString() : d.text;
    const update = lastQueryPath.endsWith(".ru");
    const summary = $("#wb-q-summary");
    summary.textContent = "Running…";
    const started = performance.now();
    try {
      if (update && queryStore) { summary.textContent = "A pipeline step's store is read only: choose the workspace store to run an update."; return; }
      const r = await request("POST", base() + (queryStore ? "/pipeline/" + queryStore : "") + (update ? "/update" : "/query"), {
        body: new URLSearchParams({ [update ? "update" : "query"]: text }), type: "application/x-www-form-urlencoded",
        accept: "application/sparql-results+json, application/n-triples;q=0.9" });
      const ms = Math.round(performance.now() - started);
      d.error = null; showDiagnostics(lastQueryPath); savedLabel();
      if (update) {
        summary.textContent = ws.overlay
          ? `Update done in ${ms} ms. It changed the overlay; ${ws.overlay.dataset} is unchanged.`
          : `Update done in ${ms} ms. It changed the scratch store until a file changes.`;
        table("query", $("#wb-q-table"), { data: [] });
        refreshOverlay();
        return;
      }
      if (r.type === "application/n-triples") {
        const rows = parseNTriples(r.text);
        fill(summary, `${ui.fmtNum(rows.length)} triples in ${ms} ms. `, el("span", { class: "muted" }, "Click a triple for why it is there."));
        tripleTable("query", $("#wb-q-table"), rows);
        return;
      }
      const j = JSON.parse(r.text);
      if (typeof j.boolean === "boolean") { summary.textContent = `${j.boolean ? "true" : "false"} (${ms} ms)`; table("query", $("#wb-q-table"), { data: [] }); return; }
      const vars = j.head.vars;
      const rows = j.results.bindings;
      summary.textContent = `${ui.fmtNum(rows.length)} rows in ${ms} ms`;
      const isTriples = ["s", "p", "o"].every((v) => vars.includes(v)) || ["subject", "predicate", "object"].every((v) => vars.includes(v));
      table("query", $("#wb-q-table"), {
        data: rows, columns: vars.map((v) => termCol(v, "?" + v)),
        rowClick: undefined,
      });
      if (isTriples) {
        const [s, p, o] = vars.includes("s") ? ["s", "p", "o"] : ["subject", "predicate", "object"];
        tables.query.on("rowClick", (e, row) => { const b = row.getData(); if (b[s] && b[p] && b[o]) why({ subject: b[s], predicate: b[p], object: b[o] }); });
      }
    } catch (e) {
      fill(summary, el("span", { class: "error" }, e.message));
      if (e.status === 400) {
        d.error = { message: e.message, ...position(e.message, text) };
        showDiagnostics(lastQueryPath);
        savedLabel();
      }
    }
  }
  async function explain() {
    if (!lastQueryPath) { ui.toast("Open a query first."); return; }
    await saveAll();
    const d = docs.get(lastQueryPath);
    const text = lastQueryPath === openPath ? view.state.doc.toString() : d.state ? d.state.doc.toString() : d.text;
    ui.explain(base() + "/explain", text);
  }

  // A table of triples with why? on each row.
  function tripleTable(name, holder, rows, extraColumns = [], onWhy = why) {
    const t = table(name, holder, {
      data: rows,
      columns: [termCol("subject", "Subject"), termCol("predicate", "Predicate"), termCol("object", "Object"), ...extraColumns,
        { title: "", field: "_why", headerSort: false, formatter: () => "why?", cssClass: "wb-why", width: 60 }],
    });
    t.on("rowClick", (e, row) => { if (!e.target.closest(".term")) onWhy(row.getData()); });
    return t;
  }

  // Validate
  function buildValidatePane() {
    fill($("#wb-pane-validate"),
      el("div", { class: "wb-pane-bar" },
        el("label", { class: "check small", title: "Validate the data plus what the rules of rules/ infer" }, el("input", { type: "checkbox", id: "wb-v-rules", checked: true }), "after rules"),
        el("label", { class: "small" }, "Severity ", el("select", { id: "wb-v-sev", onchange: renderValidation },
          el("option", { value: "" }, "all"), el("option", { value: "Violation" }, "violations"), el("option", { value: "Warning" }, "warnings"), el("option", { value: "Info" }, "info"))),
        el("label", { class: "small" }, "Show ", el("select", { id: "wb-v-view", onchange: renderValidation },
          el("option", { value: "results" }, "results by shape"), el("option", { value: "shapes" }, "shapes checked"), el("option", { value: "coverage" }, "coverage"))),
        el("span", { class: "grow" }),
        el("button", { class: "btn small", type: "button", onclick: () => download("validate", "text/turtle", "report.ttl", { rules: $("#wb-v-rules").checked ? null : "false" }) }, "Report (Turtle)"),
        copyAsPythonButton("validate"), saveAsTestButton("validate")),
      el("div", { class: "wb-summary small", id: "wb-v-summary" }, el("span", { class: "muted" }, "Run to validate the data against shapes/.")),
      el("div", { class: "wb-results", id: "wb-v-results" }));
  }
  /** A shape that keeps its key from run to run: blank node ids change whenever the store is
   *  rebuilt, so a blank property shape is known by its node shape and path. */
  const shapeKey = (shape, path, parents) => (shape.type === "uri" ? termKey(shape)
    : "property of " + termKey(parents && parents.get(termKey(shape))) + " " + (path || ""));
  /** A validation result, the same across runs (for new and fixed). */
  const resultKey = (r, parents) => [termKey(r.focusNode), r.resultPath || "", termKey(r.value), termKey(r.sourceConstraintComponent),
    shapeKey(r.sourceShape, r.resultPath, parents), (r.messages || []).join("|")].join(" ");
  async function runValidate() {
    $("#wb-v-summary").textContent = "Validating…";
    try {
      const v = await json("POST", base() + "/validate", { params: { rules: $("#wb-v-rules").checked ? null : "false", limit: 100000 }, accept: "application/json" });
      v.parents = await parentShapes();
      previousValidation = validation;
      validation = v;
      renderValidation();
    } catch (e) {
      validation = null;
      fill($("#wb-v-summary"), el("span", { class: "error" }, e.status === 422 ? "Lint: " + e.message : e.message));
      fill($("#wb-v-results"));
      lintToFile(e.message);
    }
  }
  /** The node shape of each property shape (most are blank nodes, which say little alone). */
  async function parentShapes() {
    const out = new Map();
    try {
      const r = await json("POST", base() + "/query", { body: new URLSearchParams({ query:
        `SELECT ?node ?prop WHERE { GRAPH <${ws.graphs.shapes}> { ?node <http://www.w3.org/ns/shacl#property> ?prop } }` }),
        type: "application/x-www-form-urlencoded", accept: "application/sparql-results+json" });
      for (const b of r.results.bindings) out.set(termKey(b.prop), b.node);
    } catch (e) { /* labels without parents */ }
    return out;
  }
  /** A shape as a heading: a named shape by its name, a blank property shape as "Parent › path". */
  function shapeHeading(shape, path, parents) {
    if (shape.type === "uri") return termNode(shape);
    const parent = parents && parents.get(termKey(shape));
    return el("span", { class: "wb-shape" }, parent ? termNode(parent) : el("span", { class: "muted" }, "property shape"),
      path ? el("span", { class: "mono small" }, " › " + shortPath(path)) : null);
  }
  /** A SPARQL property path with its IRIs shortened. */
  const shortPath = (path) => path.replace(/<([^>]+)>/g, (m, iri) => shortIri(iri));
  /** A shape or rule that does not compile names its node: mark the file that has it. */
  function lintToFile(message) {
    const m = message.match(/<([^>]+)>/);
    if (!m) return;
    for (const [path, d] of docs) {
      if (!/^(shapes|rules)\//.test(path)) continue;
      const text = d.state ? d.state.doc.toString() : d.text;
      const local = docPrefixes(text);
      const names = [m[1], ...Object.entries(local).filter(([, ns]) => m[1].startsWith(ns)).map(([p, ns]) => p + ":" + m[1].slice(ns.length))];
      const at = names.map((n) => text.indexOf(n)).find((i) => i >= 0);
      if (at != null && at >= 0) {
        d.error = { message, from: at, to: at + names.find((n) => text.indexOf(n) === at).length, lint: true };
        if (path === openPath) showDiagnostics(path); else ui.toast("See " + path, "bad");
        renderTree();
        setTimeout(() => { if (d.error && d.error.lint) { d.error = null; showDiagnostics(path); renderTree(); savedLabel(); } }, 15000);
        return;
      }
    }
  }
  function renderValidation() {
    const v = validation;
    if (!v) return;
    const sev = $("#wb-v-sev").value;
    const results = v.results.filter((r) => !sev || (r.severity && r.severity.value.endsWith("#" + sev)));
    const before = previousValidation ? new Set(previousValidation.results.map((r) => resultKey(r, previousValidation.parents))) : null;
    const now = new Set(v.results.map((r) => resultKey(r, v.parents)));
    const fresh = before ? results.filter((r) => !before.has(resultKey(r, v.parents))).length : 0;
    const fixed = before ? previousValidation.results.filter((r) => !now.has(resultKey(r, previousValidation.parents))) : [];
    const counts = { Violation: 0, Warning: 0, Info: 0 };
    for (const r of v.results) for (const k of Object.keys(counts)) if (r.severity && r.severity.value.endsWith("#" + k)) counts[k]++;
    if (!v.shapes.length) {
      fill($("#wb-v-summary"), el("span", { class: "muted" }, "No shapes to validate with: add a file to shapes/ (the + next to it)."));
      fill($("#wb-v-results"));
      return;
    }
    fill($("#wb-v-summary"),
      el("span", { class: "chip " + (v.conforms ? "ok" : "bad") }, v.conforms ? "conforms" : "does not conform"),
      ` ${ui.fmtNum(v.resultCount)} ${v.resultCount === 1 ? "result" : "results"} (${counts.Violation} violations, ${counts.Warning} warnings, ${counts.Info} info) · ${v.shapes.length} shapes · ${v.ms} ms`,
      before ? el("span", {}, " · ", el("b", { class: "new" }, `${fresh} new`), ", ", el("b", { class: "fixed" }, `${fixed.length} fixed`), " since the last run") : null,
      v.truncated ? el("span", { class: "error" }, ` · only the first ${ui.fmtNum(v.results.length)} are listed`) : null);
    const out = $("#wb-v-results");
    const kind = $("#wb-v-view").value;
    if (kind === "shapes") {
      fill(out, el("div", { class: "wb-table", id: "wb-v-shapes" }));
      table("shapes", $("#wb-v-shapes"), { data: v.shapes, columns: [
        termCol("shape", "Shape"), { title: "Kind", field: "property", formatter: (c) => (c.getValue() ? "property" : "node") },
        { title: "Focus nodes", field: "focusNodes", hozAlign: "right" }, { title: "Value nodes", field: "valueNodes", hozAlign: "right" },
        { title: "Results", field: "shape", hozAlign: "right", formatter: (c) => String(v.results.filter((r) => termKey(r.sourceShape) === termKey(c.getValue())).length) }] });
      return;
    }
    if (kind === "coverage") {
      const cov = v.coverage || {};
      fill(out, el("div", { class: "wb-cov" },
        el("h3", {}, `Classes no shape targets (${(cov.untargetedClasses || []).length})`),
        (cov.untargetedClasses || []).length ? el("ul", {}, cov.untargetedClasses.map((c) => el("li", {}, termNode(c.class), el("span", { class: "muted small" }, ` ${ui.fmtNum(c.instances)} instances`))))
          : el("p", { class: "muted small" }, "Every class with instances is targeted, directly or through a superclass."),
        el("h3", {}, `Node shapes with no focus node (${(cov.emptyShapes || []).length})`),
        (cov.emptyShapes || []).length ? el("ul", {}, cov.emptyShapes.map((s) => el("li", {}, termNode(s)))) : el("p", { class: "muted small" }, "Every node shape matched something.")));
      return;
    }
    // Results by shape, then constraint, then focus node.
    const groups = new Map();
    for (const r of results) {
      const shape = shapeKey(r.sourceShape, r.resultPath, v.parents), comp = termKey(r.sourceConstraintComponent) + " " + (r.resultPath || "");
      if (!groups.has(shape)) groups.set(shape, { shape: r.sourceShape, path: r.resultPath, items: new Map(), n: 0, fresh: 0 });
      const g = groups.get(shape);
      if (!g.items.has(comp)) g.items.set(comp, { comp: r.sourceConstraintComponent, path: r.resultPath, rows: [], fresh: 0 });
      const isNew = before && !before.has(resultKey(r, v.parents));
      g.items.get(comp).rows.push({ r, isNew });
      g.n++; if (isNew) { g.fresh++; g.items.get(comp).fresh++; }
    }
    const badge = (n, isNew) => el("span", { class: "chip" + (isNew ? " new" : "") }, ui.fmtNum(n) + (isNew ? " new" : ""));
    const sevDot = (r) => el("span", { class: "dot " + (!r.severity ? "" : r.severity.value.endsWith("Violation") ? "bad" : r.severity.value.endsWith("Warning") ? "warn" : "ok"), title: r.severity ? shortIri(r.severity.value) : "" });
    const rowsOf = (item, limit) => item.rows.slice(0, limit).map(({ r, isNew }) => el("li", { class: "wb-res" + (isNew ? " is-new" : "") },
      sevDot(r), termNode(r.focusNode, { path: r.resultPath, value: r.value }),
      r.value ? el("span", { class: "muted small" }, "value ", termNode(r.value)) : null,
      el("span", { class: "small" }, (r.messages || []).join(" · ")),
      isNew ? el("span", { class: "chip new" }, "new") : null,
      el("button", { class: "link small wb-ai-explain", type: "button", title: "Ask the AI assistant what this result means and how to fix it",
        onclick: () => aiExplain("Explain this validation result and propose a fix:\n" + JSON.stringify(resultFacts(r, v.parents), null, 1)) }, "explain")));
    const sections = [...groups.values()].sort((a, b) => b.n - a.n).map((g) => el("details", { class: "wb-group", open: groups.size <= 6 },
      el("summary", {}, shapeHeading(g.shape, g.path, v.parents), badge(g.n), g.fresh ? badge(g.fresh, true) : null),
      ...[...g.items.values()].sort((a, b) => b.rows.length - a.rows.length).map((item) => {
        const d = el("details", { class: "wb-group inner", open: g.n <= 20 },
          el("summary", {}, el("span", { class: "mono small" }, shortIri(item.comp.value).replace(/^sh:/, "").replace(/ConstraintComponent$/, "")),
            item.path && g.shape.type === "uri" ? el("span", { class: "mono small muted" }, shortPath(item.path)) : null, badge(item.rows.length), item.fresh ? badge(item.fresh, true) : null));
        const list = el("ul", { class: "wb-res-list" });
        const showRows = (limit) => {
          fill(list, ...rowsOf(item, limit), item.rows.length > limit
            ? el("li", {}, el("button", { class: "link small", type: "button", onclick: () => showRows(limit * 5) }, `Show more (${ui.fmtNum(item.rows.length - limit)} left)`)) : null);
        };
        d.addEventListener("toggle", () => { if (d.open && !list.childNodes.length) showRows(200); }, { once: false });
        if (d.open) showRows(200);
        d.append(list);
        return d;
      })));
    fill(out,
      ...(sections.length ? sections : [el("p", { class: "muted" }, results.length || !v.results.length ? "No results: the data conforms." : "No results with this severity.")]),
      fixed.length ? el("details", { class: "wb-group fixed" }, el("summary", {}, el("b", {}, `Fixed since the last run (${fixed.length})`)),
        el("ul", { class: "wb-res-list" }, fixed.slice(0, 500).map((r) => el("li", { class: "wb-res" }, termNode(r.focusNode), r.resultPath ? el("span", { class: "mono small muted" }, shortPath(r.resultPath)) : null,
          el("span", { class: "small" }, (r.messages || []).join(" · ")))))) : null);
  }

  // Rules
  function buildRulesPane() {
    fill($("#wb-pane-rules"),
      el("div", { class: "wb-pane-bar" },
        el("label", { class: "small" }, "Run ", el("select", { id: "wb-r-mode" }, el("option", { value: "fixpoint" }, "to a fixpoint"), el("option", { value: "single" }, "a single pass"))),
        el("label", { class: "small" }, "Show ", el("select", { id: "wb-r-view", onchange: renderRules }, el("option", { value: "triples" }, "inferred triples"), el("option", { value: "rules" }, "rules"), el("option", { value: "strata" }, "strata (why rules run in this order)"))),
        el("span", { class: "grow" }),
        el("button", { class: "btn small", type: "button", onclick: () => download("infer", "text/turtle", "inferred.ttl", ruleParams()) }, "Inferred (Turtle)"),
        el("button", { class: "btn small", type: "button", onclick: downloadAll }, "Data + inferred (N-Triples)"),
        copyAsPythonButton("rules"), saveAsTestButton("rules")),
      el("div", { class: "wb-summary small", id: "wb-r-summary" }, el("span", { class: "muted" }, "Run to apply the rules of rules/ (or of shapes/ when rules/ is empty).")),
      el("div", { class: "wb-table", id: "wb-r-table" }));
  }
  const ruleParams = () => ({ single_pass: $("#wb-r-mode").value === "single" ? "true" : null });
  async function runRules() {
    $("#wb-r-summary").textContent = "Inferring…";
    try {
      inference = await json("POST", base() + "/infer", { params: { ...ruleParams(), provenance: "true", plan: "true", limit: 1000000 }, accept: "application/json" });
      inferredByKey = new Map(inference.triples.map((t, i) => [tripleKey(t.subject, t.predicate, t.object), i]));
      renderRules();
    } catch (e) {
      inference = null;
      fill($("#wb-r-summary"), el("span", { class: "error" }, e.message));
      lintToFile(e.message);
    }
  }
  /** A rule description value: an IRI in <>, a blank node, or a literal's text. */
  const ruleText = (v) => (v == null ? "" : /^<[^>]*>$/.test(v) ? shortIri(v.slice(1, -1)) : v);
  function ruleName(key) {
    const r = inference && inference.rules && inference.rules[key];
    const label = r && r.label ? r.label : key.startsWith("_:") ? "" : ruleText(key);
    const type = r && r.type ? ruleText(r.type).replace(/^sh:/, "") : "rule";
    const template = r && r.construct && r.construct.replace(/\s+/g, " ").match(/CONSTRUCT\s*(\{[^}]*\})/i);
    return label ? `${type} ${label}` : type + (template ? ": " + template[1] : r && r.predicate ? ": " + ruleText(r.predicate) : "");
  }
  function renderRules() {
    const inf = inference;
    if (!inf) return;
    const passes = Math.max(0, ...inf.runs.map((r) => r.pass));
    fill($("#wb-r-summary"), `${ui.fmtNum(inf.count)} ${inf.count === 1 ? "triple" : "triples"} inferred · ${passes} ${passes === 1 ? "pass" : "passes"} · ${inf.ms} ms`,
      inf.truncated ? el("span", { class: "error" }, ` · only the first ${ui.fmtNum(inf.triples.length)} are listed`) : null,
      " ", el("span", { class: "muted" }, "Click a triple for why it is there."));
    if ($("#wb-r-view").value === "strata") {
      renderStrata(inf);
      return;
    }
    if ($("#wb-r-view").value === "rules") {
      const byRule = new Map();
      for (const r of inf.runs) {
        const a = byRule.get(r.rule) || { rule: r.rule, shape: r.shape, group: r.group, passes: 0, focusNodes: 0, constructed: 0, new: 0, ms: 0 };
        a.passes++; a.focusNodes += r.focusNodes; a.constructed += r.constructed; a.new += r.new; a.ms += r.ms;
        byRule.set(r.rule, a);
      }
      const t = table("rules", $("#wb-r-table"), { data: [...byRule.values()], columns: [
        { title: "Rule", field: "rule", formatter: (c) => textCell(ruleName(c.getValue())), headerFilter: "input", headerFilterFunc: (v, cell) => ruleName(cell || "").toLowerCase().includes(v.toLowerCase()) },
        termCol("shape", "Shape"),
        { title: "Group", field: "group", hozAlign: "right" }, { title: "Passes", field: "passes", hozAlign: "right" },
        { title: "Focus nodes", field: "focusNodes", hozAlign: "right" }, { title: "Constructed", field: "constructed", hozAlign: "right" },
        { title: "New", field: "new", hozAlign: "right" }, { title: "ms", field: "ms", hozAlign: "right", formatter: (c) => c.getValue().toFixed(1) },
        { title: "", field: "rule", headerSort: false, formatter: (c) => (inf.rules[c.getValue()] && inf.rules[c.getValue()].construct ? "Try in Query" : ""), cssClass: "wb-why" },
      ] });
      t.on("cellClick", (e, cell) => { if (cell.getColumn().getDefinition().title === "" && inf.rules[cell.getValue()] && inf.rules[cell.getValue()].construct) tryInQuery(cell.getValue()); });
      return;
    }
    const rows = inf.triples.map((t, i) => ({ ...t, _i: i, rule: inf.provenance[i] && inf.provenance[i].rule, focus: inf.provenance[i] && inf.provenance[i].focus, pass: inf.provenance[i] && inf.provenance[i].pass }));
    tripleTable("rules", $("#wb-r-table"), rows, [
      { title: "Rule", field: "rule", formatter: (c) => textCell(ruleName(c.getValue() || "")), headerFilter: "input", headerFilterFunc: (v, cell) => ruleName(cell || "").toLowerCase().includes(v.toLowerCase()) },
      termCol("focus", "Focus"), { title: "Pass", field: "pass", hozAlign: "right", width: 70 }]);
  }
  /** The strata of the run: the groups of rules in the order they ran, and why. */
  function renderStrata(inf) {
    const plan = inf.plan;
    const box = $("#wb-r-table");
    if (!plan || !plan.rules.length) { fill(box, el("p", { class: "muted small" }, "No rules ran.")); return; }
    const passes = new Map();
    for (const r of inf.runs) passes.set(r.group, Math.max(passes.get(r.group) || 0, r.pass));
    const groupOf = new Map();
    plan.groups.forEach((g) => g.rules.forEach((i) => groupOf.set(i, g.group)));
    const keys = (through) => (through ? through.map((k) => k.startsWith("http://www.w3.org/1999/02/22-rdf-syntax-ns#type ") ? "a " + shortIri(k.slice(48)) : shortIri(k)).join(", ") : "anything (TripleTrail cannot tell)");
    // Rules whose writes TripleTrail cannot tell (a variable predicate): any rule may read them.
    const writeAny = plan.rules.map((r, i) => (r.writes == null ? i : -1)).filter((i) => i >= 0);
    const name = (i) => ruleName(plan.rules[i].rule);
    const why = (i) => {
      const into = plan.edges.filter((e) => e.to === i);
      const any = [];
      if (plan.rules[i].reads == null) any.push(el("li", {}, "may read what any rule writes: TripleTrail cannot tell what it reads"));
      else if (writeAny.some((j) => j !== i)) any.push(el("li", { class: "muted" }, `may read what ${writeAny.filter((j) => j !== i).length} rules write that may write anything`));
      if (!into.length && !any.length) return [el("li", { class: "muted" }, "reads nothing another rule writes: can run first")];
      return any.concat(into.map((e) => {
        const where = e.from === i ? "itself" : groupOf.get(e.from) === groupOf.get(i) ? `${name(e.from)} (same group)` : `${name(e.from)} (group ${groupOf.get(e.from)})`;
        return el("li", {}, e.positive ? "reads what " : "reads, only under a negation, what ",
          el("b", {}, where), " writes: ", el("span", { class: "mono" }, keys(e.through)),
          e.positive ? null : el("span", { class: "muted" }, " · orders the groups, never makes one repeat"));
      }));
    };
    fill(box, el("div", { class: "wb-strata" },
      plan.truncated ? el("p", { class: "small error" }, `Only the first ${ui.fmtNum(plan.edges.length)} links between rules are listed.`) : null,
      el("p", { class: "small muted" }, "A rule runs after the rules whose triples it may read. Rules that feed each other form a group, which repeats in passes until nothing new comes out; any other group runs once. Groups run in this order (then in sh:order)."),
      plan.groups.map((g) => el("details", { class: "wb-group", open: plan.groups.length <= 8 },
        el("summary", {}, el("b", {}, `Group ${g.group}`), " · ",
          g.recursive ? `repeats to a fixpoint (${passes.get(g.group) || 0} ${passes.get(g.group) === 1 ? "pass" : "passes"})` : "runs once",
          el("span", { class: "muted" }, ` · ${g.rules.length} ${g.rules.length === 1 ? "rule" : "rules"}`)),
        g.rules.map((i) => el("div", { class: "wb-stratum-rule" },
          el("div", {}, el("b", {}, name(i)), el("span", { class: "muted small" }, ` · sh:order ${plan.rules[i].order}`)),
          el("ul", { class: "small" }, why(i),
            el("li", { class: "muted" }, "writes ", el("span", { class: "mono" }, keys(plan.rules[i].writes))))))))));
  }
  // Reason (#407): what RDFS or OWL 2 RL entails from the data, and why.
  let reasoning = null;
  /** What each rule says, for the derivation tree. */
  const ENTAILMENT_RULES = {
    "rdfs2": "the domain of a property types its subjects", "prp-dom": "the domain of a property types its subjects",
    "rdfs3": "the range of a property types its objects", "prp-rng": "the range of a property types its objects",
    "rdfs5": "sub property of a sub property", "scm-spo": "sub property of a sub property",
    "rdfs7": "a sub property's triples hold for its super property", "prp-spo1": "a sub property's triples hold for its super property",
    "rdfs9": "members of a sub class are members of the class", "cax-sco": "members of a sub class are members of the class",
    "rdfs11": "sub class of a sub class", "scm-sco": "sub class of a sub class",
    "cax-eqc1": "members of a class are members of its equivalent", "cax-eqc2": "members of a class are members of its equivalent",
    "prp-eqp1": "a property's triples hold for its equivalent", "prp-eqp2": "a property's triples hold for its equivalent",
    "prp-inv1": "a triple of a property, read backwards, is one of its inverse", "prp-inv2": "a triple of a property, read backwards, is one of its inverse",
    "prp-symp": "a symmetric property holds both ways", "prp-trp": "a transitive property chains",
    "prp-spo2": "a property chain", "prp-fp": "a functional property has one value", "prp-ifp": "an inverse functional property has one subject per value",
    "prp-key": "same keys, same resource", "cls-hv1": "a value restriction met", "cls-hv2": "a value restriction gives its value",
    "cls-svf1": "a some-values-from restriction met", "cls-svf2": "a some-values-from restriction met", "cls-avf": "an all-values-from restriction types the values",
    "cls-int1": "member of every class of an intersection", "cls-int2": "a member of an intersection is a member of each of its classes",
    "cls-uni": "a member of a class of a union is a member of the union", "cls-oo": "an enumerated member",
    "cls-maxc2": "at most one value", "cls-maxqc3": "at most one value of the class", "cls-maxqc4": "at most one value",
    "eq-sym": "owl:sameAs holds both ways", "eq-trans": "owl:sameAs chains",
    "eq-rep-s": "the same resource, so the same triples", "eq-rep-p": "the same resource, so the same triples", "eq-rep-o": "the same resource, so the same triples",
    "scm-cls": "every class is a sub class of itself and of owl:Thing", "scm-op": "every property is a sub property of itself", "scm-dp": "every property is a sub property of itself",
    "scm-eqc1": "equivalent classes are sub classes of each other", "scm-eqc2": "sub classes of each other are equivalent",
    "scm-eqp1": "equivalent properties are sub properties of each other", "scm-eqp2": "sub properties of each other are equivalent",
    "scm-dom1": "the domain widens to a super class", "scm-dom2": "a sub property has its super property's domain",
    "scm-rng1": "the range widens to a super class", "scm-rng2": "a sub property has its super property's range",
    "scm-int": "an intersection is a sub class of each of its classes", "scm-uni": "each class of a union is a sub class of it",
    "scm-hv": "value restrictions over sub properties", "scm-svf1": "some-values-from restrictions over sub classes", "scm-svf2": "some-values-from restrictions over sub properties",
    "scm-avf1": "all-values-from restrictions over sub classes", "scm-avf2": "all-values-from restrictions over sub properties",
  };
  function buildReasonPane() {
    fill($("#wb-pane-reason"),
      el("div", { class: "wb-pane-bar" },
        el("label", { class: "small" }, "Regime ", el("select", { id: "wb-e-regime" }, el("option", { value: "rdfs" }, "RDFS"), el("option", { value: "owl-rl" }, "OWL 2 RL"))),
        el("label", { class: "small" }, "Show ", el("select", { id: "wb-e-view", onchange: renderReason },
          el("option", { value: "triples" }, "entailed triples"), el("option", { value: "classes" }, "by class"),
          el("option", { value: "predicates" }, "by predicate"), el("option", { value: "inconsistencies" }, "inconsistencies (OWL 2 RL)"))),
        el("span", { class: "grow" }),
        el("button", { class: "btn small", type: "button", onclick: () => download("reason", "text/turtle", "entailed.ttl", { regime: $("#wb-e-regime").value }) }, "Entailed (Turtle)")),
      el("div", { class: "wb-summary small", id: "wb-e-summary" }, el("span", { class: "muted" },
        "Run to see what RDFS or OWL 2 RL entails from the data (the files of data/, with the ontology), and why each triple is there.")),
      el("div", { class: "wb-table", id: "wb-e-table" }));
  }
  async function runReason() {
    $("#wb-e-summary").textContent = "Reasoning…";
    try {
      reasoning = await json("POST", base() + "/reason", { params: { regime: $("#wb-e-regime").value, limit: 1000000 }, accept: "application/json" });
      if (reasoning.inconsistencies && reasoning.inconsistencies.count && $("#wb-e-view").value === "triples") $("#wb-e-view").value = "inconsistencies";
      renderReason();
    } catch (e) {
      reasoning = null;
      fill($("#wb-e-summary"), el("span", { class: "error" }, e.message));
    }
  }
  function renderReason() {
    const r = reasoning;
    if (!r) return;
    const bad = r.inconsistencies;
    fill($("#wb-e-summary"), `${ui.fmtNum(r.count)} ${r.count === 1 ? "triple" : "triples"} entailed under ${r.regime === "rdfs" ? "RDFS" : "OWL 2 RL"} · ${r.ms} ms`,
      r.truncated ? el("span", { class: "error" }, ` · only the first ${ui.fmtNum(r.triples.length)} are listed`) : null,
      bad ? el("span", { class: bad.count ? "error" : "muted" }, ` · ${bad.atLeast ? "at least " : ""}${ui.fmtNum(bad.count)} ${bad.count === 1 ? "inconsistency" : "inconsistencies"}`) : null,
      " ", el("span", { class: "muted" }, "Click a triple for its derivation."));
    const view = $("#wb-e-view").value;
    const box = $("#wb-e-table");
    if (view === "classes" || view === "predicates") {
      const key = view === "classes" ? "class" : "predicate";
      table("reason", box, { data: r[view], columns: [termCol(key, view === "classes" ? "Class" : "Predicate"), { title: "Entailed", field: "count", hozAlign: "right" }] });
      return;
    }
    if (view === "inconsistencies") {
      if (!bad) { fill(box, el("p", { class: "muted small" }, "The consistency rules run under OWL 2 RL: pick it and run.")); return; }
      if (!bad.count) { fill(box, el("p", { class: "muted small" }, "The data contradicts nothing under OWL 2 RL.")); return; }
      fill(box, el("ul", { class: "wb-res-list" }, bad.first.map((i) => el("li", { class: "wb-res" },
        el("span", { class: "chip ro mono" }, i.rule), ...i.terms.flatMap((t) => [" ", termNode(t)]),
        el("button", { class: "link small", type: "button", onclick: () => whyInconsistent(i) }, "why?")))));
      return;
    }
    tripleTable("reason", box, r.triples, [], (t) => whyEntailed(t));
  }
  /** Whether a triple is entailed (or asserted), and its derivation tree, from the server. */
  async function derivation(t) {
    const v = await json("POST", base() + "/reason/why", { params: { regime: reasoning ? reasoning.regime : $("#wb-e-regime").value,
      subject: ntTerm(t.subject), predicate: ntTerm(t.predicate), object: ntTerm(t.object) }, accept: "application/json" });
    return v;
  }
  function derivationNode(d) {
    const triple = el("span", { class: "mono small" }, termNode(d.subject), " ", termNode(d.predicate), " ", termNode(d.object));
    if (!d.rule) return el("li", {}, triple, " ", el("span", { class: "chip" }, "asserted"));
    return el("li", {}, el("details", { open: true },
      el("summary", {}, triple, " ", el("span", { class: "chip ro mono", title: ENTAILMENT_RULES[d.rule] || "" }, d.rule),
        ENTAILMENT_RULES[d.rule] ? el("span", { class: "muted small" }, " " + ENTAILMENT_RULES[d.rule]) : null),
      el("ul", { class: "wb-derivation" }, d.premises.map(derivationNode))));
  }
  async function whyEntailed(t) {
    const triple = el("p", { class: "wb-triple mono" }, termNode(t.subject), " ", termNode(t.predicate), " ", termNode(t.object), " .");
    openDrawer("Why?", triple, el("p", { class: "muted" }, "Finding a derivation…"));
    let v;
    try { v = await derivation(t); } catch (e) { openDrawer("Why?", triple, el("p", { class: "error" }, e.message)); return; }
    if (!triple.isConnected) return;
    const d = v.derivation;
    openDrawer("Why?", triple,
      d ? el("p", {}, el("span", { class: "chip ro" }, "entailed"), ` under ${reasoning && reasoning.regime === "owl-rl" ? "OWL 2 RL" : "RDFS"}: by the rules below, from triples of the data.`)
        : el("p", { class: "muted" }, v.entailed
          ? "No derivation found: the triple is entailed, but only through rules the explanation does not cover, or beyond what one search reads."
          : "The triple is not entailed."),
      d ? el("ul", { class: "wb-derivation" }, derivationNode(d)) : null);
  }
  /** An inconsistency's terms with the derivations of the types and triples it is about. */
  async function whyInconsistent(i) {
    const RDF_TYPE = { type: "uri", value: "http://www.w3.org/1999/02/22-rdf-syntax-ns#type" };
    // cax-dw: x, C1, C2 (x a C1, x a C2); others: show the terms.
    const typed = i.rule === "cax-dw" && i.terms.length === 3 ? [i.terms[1], i.terms[2]].map((c) => ({ subject: i.terms[0], predicate: RDF_TYPE, object: c })) : [];
    openDrawer("Why inconsistent?", el("p", {}, el("span", { class: "chip ro mono" }, i.rule), ...i.terms.flatMap((t) => [" ", termNode(t)])),
      typed.length ? el("p", { class: "muted small" }, "The resource is a member of both disjoint classes:") : el("p", { class: "muted small" }, "The OWL 2 RL rule and the terms it found contradicting each other."));
    for (const t of typed) {
      let d;
      try { d = (await derivation(t)).derivation; } catch (e) { continue; }
      if (d) $("#wb-drawer-body").append(el("ul", { class: "wb-derivation" }, derivationNode(d)));
    }
  }
  async function tryInQuery(ruleKey) {
    const r = inference.rules[ruleKey];
    let q = r.construct.trim();
    // $this stands for each focus node: as a query, it is a variable over all of them.
    q = q.replace(/\$this\b/g, "?this");
    const path = "queries/try-rule-" + (Object.keys(inference.rules).indexOf(ruleKey) + 1) + ".rq";
    try {
      await request("PUT", base() + "/files/" + path, { body: q + "\n", type: "application/sparql-query" });
      docs.set(path, { text: q + "\n", state: null, dirty: false, error: null, timer: null });
      await refreshWorkspace();
      openFile(path);
      showPane("query");
      run();
    } catch (e) { ui.fail(e); }
  }

  // ---------- tests ----------
  // A test is a folder of tests/ (see docs/server.md, "Tests"): the server finds and runs them.
  function saveAsTestButton(kind) {
    return el("button", { class: "btn small", type: "button", onclick: () => saveAsTest(kind),
      title: "Keep what this gives now as a test: the data is copied into the test, and the expected output is recorded" }, "Save as test…");
  }
  // ---------- copy as Python ----------
  // The in-process tripletrail.Engine code for what a tab does, to take it to production.
  const pyString = (text) => (!text.includes('"""') && !text.includes("\\") && !text.endsWith('"') ? '"""' + text + '"""' : JSON.stringify(text));
  function copyAsPythonButton(kind) {
    return el("button", { class: "btn small", type: "button", onclick: () => copyAsPython(kind),
      title: "Copy the tripletrail.Engine code that does this, for a notebook or a production script" }, "Copy as Python");
  }
  function pythonFor(kind) {
    const has = (folder) => ws.files.some((f) => folderOf(f.path) === folder);
    const lines = [
      "import tripletrail",
      "from pathlib import Path",
      "",
      "# Paths are the workspace's folders; on this workspace instead:",
      `# ws = tripletrail.connect(${JSON.stringify(location.origin)}, workspace=${JSON.stringify(wsId)})`,
      "e = tripletrail.Engine()  # or tripletrail.Engine(\"/path/to/dataset\") for a dataset on disk",
      "",
      "def load(folder, graph=None):",
      "    for f in sorted(Path(folder).rglob(\"*\")):",
      "        if f.is_file() and not f.name.startswith(\".\"):",
      "            e.load(path=f, to_graph=graph)",
      "",
      "load(\"data\")",
    ];
    if (kind === "query") {
      const d = docs.get(lastQueryPath);
      const text = lastQueryPath === openPath ? view.state.doc.toString() : d && d.state ? d.state.doc.toString() : d ? d.text : "";
      if (lastQueryPath && lastQueryPath.endsWith(".ru")) lines.push("e.update(" + pyString(text) + ")");
      else lines.push("results = e.query(" + pyString(text) + ")", "for row in results:", "    print(row)");
    } else if (kind === "validate") {
      lines.push("shapes = tripletrail.NamedNode(\"urn:tripletrail:workspace:shapes\")", "load(\"shapes\", shapes)");
      // The rules run first when they are in the shapes graph.
      if ($("#wb-v-rules").checked && has("rules")) lines.push("load(\"rules\", shapes)");
      lines.push("report = e.validate(shapes)", "print(report.conforms, len(report))", "report.serialize(\"report.ttl\")");
    } else {
      const folder = has("rules") ? "rules" : "shapes";
      lines.push("rules = tripletrail.NamedNode(\"urn:tripletrail:workspace:rules\")", `load(${JSON.stringify(folder)}, rules)`);
      lines.push("inferred = e.infer(rules" + (ruleParams().single_pass ? ", max_iterations=1" : "") + ")", "print(len(inferred))");
    }
    return lines.join("\n") + "\n";
  }
  async function copyAsPython(kind) {
    if (!wsId) return;
    if (kind === "query" && !lastQueryPath) { ui.toast("Open a query first."); return; }
    const text = pythonFor(kind);
    try {
      await navigator.clipboard.writeText(text);
      ui.toast("Python copied to the clipboard.");
    } catch {
      // No clipboard (a page not served over HTTPS or localhost): show it to copy by hand.
      window.open(URL.createObjectURL(new Blob([text], { type: "text/plain" })), "_blank");
    }
  }
  // A python step does not run on the server: its code runs from a notebook against the store
  // the step works on, served as a Fuseki dataset, and Complete takes what it left.
  function waitingStep(s) {
    const [module, fn] = (s.target || "module:function").split(":");
    const code = [
      "import tripletrail",
      `from ${module.endsWith(".py") ? module.slice(0, -3).replace(/\//g, ".") : module} import ${fn || "main"}`,
      "",
      `ws = tripletrail.connect(${JSON.stringify(location.origin)}, workspace=${JSON.stringify(wsId)})`,
      `ws.run_step(${s.step}, ${fn || "main"})  # calls ${fn || "main"}(base_url, dataset), then completes the step`,
      `# or by hand: base_url, dataset = ws.step_endpoint(${s.step}); ...; ws.complete_step(${s.step})`,
    ].join("\n");
    return el("div", { class: "small" },
      el("p", { class: "muted" }, "Waiting for its code: the server does not run Python. Run it from a notebook against the store this step works on "
        + `(${location.origin}${base()}/pipeline/${s.step}/sparql, /update and /data, as a Fuseki dataset), then complete the step.`),
      el("pre", { class: "mono" }, code),
      el("button", { class: "btn small", type: "button", onclick: async (e) => {
        e.preventDefault();
        try { await navigator.clipboard.writeText(code + "\n"); ui.toast("Python copied to the clipboard."); } catch { window.open(URL.createObjectURL(new Blob([code], { type: "text/plain" })), "_blank"); }
      } }, "Copy as Python"), " ",
      el("button", { class: "btn small", type: "button", title: "Take what the code left in the step's store as its result", onclick: async (e) => {
        e.preventDefault();
        try {
          lastPipeline = await json("POST", base() + "/pipeline/" + s.step + "/complete");
          renderPipeline();
        } catch (err) { ui.fail(err); }
      } }, "Complete step"));
  }
  function exportPipeline(to) {
    if (!wsId) return;
    window.open(base() + "/pipeline/export?to=" + to, "_blank");
  }

  function buildTestsPane() {
    fill($("#wb-pane-tests"),
      el("div", { class: "wb-pane-bar" },
        el("span", { class: "small muted" }, "Each folder of tests/ is a test: an input, a query, update, rules or shapes, and the expected output."),
        el("span", { class: "grow" })),
      el("div", { class: "wb-summary small", id: "wb-t-summary" }, el("span", { class: "muted" }, "Run to run the tests. Make one with “Save as test…” on the Query, Validate or Rules tab.")),
      el("div", { class: "wb-results", id: "wb-t-list" }));
  }
  async function runTests(only) {
    const summary = $("#wb-t-summary");
    summary.textContent = "Running the tests…";
    try {
      const r = await json("POST", base() + "/tests", only ? { body: JSON.stringify({ tests: only }), type: "application/json" } : {});
      if (only && lastTests) {
        const fresh = new Map(r.tests.map((t) => [t.name, t]));
        lastTests = lastTests.map((t) => fresh.get(t.name) || t);
        for (const t of r.tests) if (!lastTests.some((x) => x.name === t.name)) lastTests.push(t);
      } else lastTests = r.tests;
      renderTests();
    } catch (e) { fill(summary, el("span", { class: "error" }, e.message)); }
  }
  function renderTests() {
    const list = $("#wb-t-list"), summary = $("#wb-t-summary");
    if (!lastTests || !lastTests.length) {
      fill(summary, el("span", { class: "muted" }, "No tests yet. Make one with “Save as test…” on the Query, Validate or Rules tab."));
      fill(list);
      return;
    }
    const failed = lastTests.filter((t) => !t.passed).length;
    fill(summary, el("span", { class: failed ? "error" : "ok" }, `${lastTests.length - failed} passed, ${failed} failed`));
    fill(list, ...lastTests.map((t) => {
      const diff = (sign, lines, total) => lines.length
        ? el("pre", { class: "wb-diff " + (sign === "-" ? "minus" : "plus") }, lines.map((l) => sign + " " + l).join("\n") + (total > lines.length ? `\n… ${total - lines.length} more` : ""))
        : null;
      const files = ws.files.map((f) => f.path).filter((p) => p.startsWith("tests/" + t.name + "/"));
      const main = ["test.yaml", "query.rq", "update.ru", "rules.ttl", "shapes.ttl"].map((n) => "tests/" + t.name + "/" + n).find((p) => files.includes(p)) || files[0];
      return el("details", { class: "wb-test", open: !t.passed },
        el("summary", {},
          el("span", { class: "dot " + (t.passed ? "ok" : "bad") }), " ", el("b", {}, t.name), " ",
          el("span", { class: "chip" }, t.kind || "?"), " ", el("span", { class: "muted small" }, `${Math.round(t.ms)} ms`),
          el("span", { class: "grow" }),
          main ? el("button", { class: "link small", type: "button", onclick: (e) => { e.preventDefault(); openTestFile(main); } }, "Open") : null,
          el("button", { class: "link small", type: "button", onclick: (e) => { e.preventDefault(); runTests([t.name]); } }, "Run"),
          t.passed || t.error ? null : el("button", { class: "link small", type: "button", title: "Make what the test gives now its expected output", onclick: (e) => { e.preventDefault(); recordTest(t.name); } }, "Accept output")),
        t.error ? el("p", { class: "error small" }, t.error) : null,
        t.missingCount ? el("p", { class: "small" }, `${t.missingCount} expected but missing:`) : null,
        diff("-", t.missing, t.missingCount),
        t.unexpectedCount ? el("p", { class: "small" }, `${t.unexpectedCount} not expected:`) : null,
        diff("+", t.unexpected, t.unexpectedCount));
    }));
  }
  async function openTestFile(path) {
    try { if (!docs.has(path)) await load(path); openFile(path); renderTree(); } catch (e) { ui.fail(e); }
  }
  async function recordTest(name) {
    const r = await ui.confirmDialog("Accept the output of " + name, "What the test gives now becomes its expected output (expected.nt, expected.srj or violations.yaml).");
    if (!r.ok) return;
    try {
      await request("POST", base() + "/tests", { params: { record: "true", test: name } });
      await refreshWorkspace();
      await runTests([name]);
    } catch (e) { ui.fail(e); }
  }
  // Copies a workspace file as it is (bytes, so a gzipped one too).
  async function copyFile(from, to) {
    const headers = {};
    const token = ui.store.get(ui.TOKEN_KEY);
    if (token) headers.Authorization = "Bearer " + token;
    const r = await fetch(new URL(base() + "/files/" + from, location.origin), { headers });
    if (!r.ok) throw new ui.ApiError(r.status, (await r.text()).trim() || r.statusText);
    await request("PUT", base() + "/files/" + to, { body: await r.blob(), type: mediaType(to) });
  }
  async function saveAsTest(kind) {
    if (!wsId) return;
    if (kind === "query" && !lastQueryPath) { ui.toast("Open a query first."); return; }
    if (!(await saveAll())) { ui.toast("A file has a syntax error: fix it first (its line is marked).", "bad"); return; }
    const paths = ws.files.map((f) => f.path);
    const suggested = kind === "query" ? lastQueryPath.split("/").pop().replace(/\.\w+$/, "") : kind === "validate" ? "shapes" : "rules";
    let name = prompt("Name of the test (a folder in tests/)", suggested);
    if (!name) return;
    name = name.trim().replace(/[^A-Za-z0-9_.-]+/g, "-").replace(/^[.-]+/, "");
    if (!name) return;
    const dir = "tests/" + name + "/";
    const old = paths.filter((p) => p.startsWith(dir));
    if (old.length) {
      const r = await ui.confirmDialog("Replace the test " + name, `tests/${name}/ has ${old.length} files already. They are replaced.`);
      if (!r.ok) return;
    }
    const summary = $("#wb-t-summary");
    try {
      for (const p of old) await request("DELETE", base() + "/files/" + p);
      // The input: a copy of the data files as they are now.
      const data = paths.filter((p) => folderOf(p) === "data");
      for (const p of data) await copyFile(p, dir + "input/" + p.slice("data/".length).replace(/\//g, "_"));
      const yaml = ["# Made in the Workbench. See docs/server.md, \"Tests\".", "input: input"];
      const has = (folder) => paths.some((p) => folderOf(p) === folder);
      if (kind === "query") {
        const d = docs.get(lastQueryPath);
        const text = lastQueryPath === openPath ? view.state.doc.toString() : d.state ? d.state.doc.toString() : d.text;
        const file = lastQueryPath.endsWith(".ru") ? "update.ru" : "query.rq";
        await request("PUT", base() + "/files/" + dir + file, { body: text, type: mediaType(file) });
      } else if (kind === "validate") {
        yaml.push("shapes: ../../shapes");
        if ($("#wb-v-rules").checked && has("rules")) yaml.push("rules: ../../rules");
      } else {
        yaml.push("rules: ../../" + (has("rules") ? "rules" : "shapes"));
        if (ruleParams().single_pass) yaml.push("max_iterations: 1");
      }
      if (!data.length) yaml.splice(1, 1);
      await request("PUT", base() + "/files/" + dir + "test.yaml", { body: yaml.join("\n") + "\n", type: "application/yaml" });
      await request("POST", base() + "/tests", { params: { record: "true", test: name } });
      await refreshWorkspace();
      showPane("tests");
      await runTests([name]);
      ui.toast("Saved the test " + name, "ok");
    } catch (e) {
      ui.fail(e);
      if (summary) fill(summary, el("span", { class: "error" }, e.message));
    }
  }

  // ---------- pipeline ----------
  // The steps of pipeline.yaml (see docs/server.md, "Pipelines"): the server runs them on a copy
  // of the store each step left (a snapshot), so a step runs again from its input.
  const PIPELINE_TEMPLATE = `# The steps of this workspace's ETL, run in order. See docs/server.md, "Pipelines".
parameters: {}
steps:
  - load: data/
  - update: updates/
  - infer: rules/
  - validate: shapes/
    fail_on: violation
  - export: queries/output.nt
`;
  function buildPipelinePane() {
    fill($("#wb-pane-pipeline"),
      el("div", { class: "wb-pane-bar" },
        el("span", { class: "small muted" }, "The steps of pipeline.yaml. Each step runs on what the step before it left, kept, so a step runs again from its input."),
        el("span", { class: "grow" }),
        el("button", { class: "btn small", type: "button", onclick: openPipelineFile }, "Open pipeline.yaml"),
        el("button", { class: "btn small", type: "button", onclick: () => exportPipeline("airflow"),
          title: "An Airflow DAG whose tasks run the steps with tripletrail pipeline run (set LOCATION in it)" }, "Airflow DAG"),
        el("button", { class: "btn small", type: "button", onclick: () => exportPipeline("python"),
          title: "A Python script that runs the steps in process with tripletrail.Engine" }, "Python script"),
        el("button", { class: "btn small", type: "button", title: "Run every step from the first", onclick: () => runPipeline({ from: 1 }) }, "Run all")),
      el("div", { class: "wb-pane-bar small", id: "wb-p-params" }),
      el("div", { class: "wb-summary small", id: "wb-p-summary" }, el("span", { class: "muted" }, "Run to run the steps not run yet or changed since.")),
      el("div", { class: "wb-results", id: "wb-p-list" }));
  }
  async function openPipelineFile() {
    const path = "pipeline.yaml";
    try {
      if (!ws.files.some((f) => f.path === path)) {
        await request("PUT", base() + "/files/" + path, { body: PIPELINE_TEMPLATE, type: mediaType(path) });
        await refreshWorkspace();
        lastPipeline = null;
      }
      if (!docs.has(path)) await load(path);
      openFile(path); renderTree();
      if (!lastPipeline) await loadPipeline();
    } catch (e) { ui.fail(e); }
  }
  async function loadPipeline() {
    try { lastPipeline = await json("GET", base() + "/pipeline"); } catch (e) { lastPipeline = { error: e.message, steps: [] }; }
    renderPipeline();
  }
  function pipelineValues() {
    const values = {};
    for (const input of document.querySelectorAll("#wb-p-params input[data-param]")) if (input.value !== input.dataset.default) values[input.dataset.param] = input.value;
    return values;
  }
  async function runPipeline(range) {
    const summary = $("#wb-p-summary");
    if (!(await saveAll())) { ui.toast("A file has a syntax error: fix it first (its line is marked).", "bad"); return; }
    summary.textContent = "Running…";
    const started = performance.now();
    try {
      lastPipeline = await json("POST", base() + "/pipeline", { body: JSON.stringify({ ...range, values: pipelineValues() }), type: "application/json" });
      renderPipeline(Math.round(performance.now() - started));
      await refreshWorkspace();
    } catch (e) { fill(summary, el("span", { class: "error" }, e.message)); }
  }
  const fmtSeconds = (s) => (s < 1 ? Math.round(s * 1000) + " ms" : s.toFixed(1) + " s");
  function stepDot(step) {
    const r = step.report;
    if (!r) return "off";
    if (step.outdated || r.status === "waiting") return "warn";
    return r.status === "ok" ? "ok" : "bad";
  }
  function renderPipeline(ms) {
    const list = $("#wb-p-list"), summary = $("#wb-p-summary"), params = $("#wb-p-params");
    if (!list) return;
    const p = lastPipeline;
    // The query pane's choice of store: the workspace's, or the store after a step.
    const select = $("#wb-q-store");
    if (select) {
      const kept = (p && p.steps || []).filter((s) => s.snapshot);
      if (!kept.some((s) => String(s.step) === queryStore)) queryStore = "";
      fill(select, el("option", { value: "" }, "Workspace store"),
        ...kept.map((s) => el("option", { value: String(s.step), selected: String(s.step) === queryStore }, `After step ${s.step}: ${s.name}`)));
    }
    if (!p || p.error) {
      fill(params);
      fill(summary, el("span", { class: p && p.error && !/no pipeline\.yaml/i.test(p.error) ? "error" : "muted" },
        p && p.error && !/no pipeline\.yaml/i.test(p.error) ? p.error : "No pipeline.yaml yet: “Open pipeline.yaml” starts one."));
      fill(list);
      return;
    }
    const old = pipelineValues();
    fill(params, ...Object.entries(p.parameters || {}).map(([k, v]) => el("label", { class: "small" }, k + " ",
      el("input", { "data-param": k, "data-default": v, value: k in old ? old[k] : v, size: 12 }))));
    const ran = p.steps.filter((s) => s.report);
    const bad = ran.find((s) => s.report.status !== "ok");
    fill(summary, p.running ? el("span", { class: "muted" }, "A run of this pipeline is in progress (another window or a client): Reload when it is done.")
      : !ran.length ? el("span", { class: "muted" }, `${p.steps.length} steps. Run to run them.`)
      : el("span", { class: bad ? (bad.report.status === "waiting" ? "muted" : "error") : "ok" },
        bad ? `Step ${bad.step} ${bad.report.status}` : `${ran.length} of ${p.steps.length} steps ran`,
        ms != null ? ` (${fmtSeconds(ms / 1000)})` : "",
        p.steps.some((s) => s.outdated) ? el("span", { class: "muted" }, " · some steps changed since they ran") : null));
    // A file opens in the editor; a folder is shown as it is.
    const link = (path) => ws.files.some((f) => f.path === path)
      ? el("button", { class: "link small mono", type: "button", onclick: (e) => { e.preventDefault(); openTestFile(path); } }, path)
      : el("code", { class: "small" }, path);
    fill(list, ...p.steps.map((s) => {
      const r = s.report;
      const d = s.diff;
      const rows = (items, key) => el("table", { class: "small wb-p-table" },
        el("tr", {}, el("th", {}, key === "class" ? "Class" : "Predicate"), el("th", { class: "num" }, "+"), el("th", { class: "num" }, "−")),
        ...items.map((i) => el("tr", {}, el("td", { class: "mono", title: i[key] || "" }, i[key] ? shortIri(i[key]) : "(no class)"),
          el("td", { class: "num" }, ui.fmtNum(i.added)), el("td", { class: "num" }, ui.fmtNum(i.removed)))));
      const samples = (sign, lines, total) => lines && lines.length
        ? el("pre", { class: "wb-diff " + (sign === "-" ? "minus" : "plus") }, lines.map((l) => sign + " " + l).join("\n") + (total > lines.length ? `\n… ${ui.fmtNum(total - lines.length)} more` : ""))
        : null;
      return el("details", { class: "wb-test" },
        el("summary", {},
          el("span", { class: "dot " + stepDot(s), title: s.outdated ? (s.changed ? "Its files changed since it ran" : "A step before it changed") : r ? r.status : "Not run" }), " ",
          el("b", {}, s.name === `${s.step} ${s.kind}` ? `${s.step}.` : `${s.step}. ${s.name}`), " ", el("span", { class: "chip" }, s.kind), " ",
          r ? el("span", { class: "muted small" }, `${fmtSeconds(r.seconds)} · ${ui.fmtNum(r.quadsAfter)} quads`
            + (d ? ` · +${ui.fmtNum(d.added)} −${ui.fmtNum(d.removed)}` : "")) : null,
          el("span", { class: "grow" }),
          el("button", { class: "link small", type: "button", title: "Run the steps up to this one that are not run yet or changed", onclick: (e) => { e.preventDefault(); runPipeline({ to: s.step }); } }, "Run to here"),
          el("button", { class: "link small", type: "button", title: "Run only this step again, from what the step before it left", onclick: (e) => { e.preventDefault(); runPipeline({ from: s.step, to: s.step }); } }, "Run this step"),
          s.snapshot ? el("button", { class: "link small", type: "button", title: "Query the store as this step left it", onclick: (e) => {
            e.preventDefault(); queryStore = String(s.step); const sel = $("#wb-q-store"); if (sel) sel.value = queryStore; showPane("query"); } }, "Query after") : null),
        el("p", { class: "small" }, ...s.files.flatMap((f, i) => [i ? " " : null, link(f)]),
          s.graph ? el("span", { class: "muted" }, " → ", el("code", {}, s.graph)) : null),
        r && r.status === "waiting" ? waitingStep(s) : r && r.error ? el("p", { class: "error small" }, r.error) : null,
        r && r.results ? el("p", { class: "small" }, "Validation results: " + (Object.entries(r.results).map(([k, n]) => `${ui.fmtNum(n)} ${k}`).join(", ") || "none")
          + ` (stops at ${r.failOn})`) : null,
        r && r.triples != null ? el("p", { class: "small muted" }, `${ui.fmtNum(r.triples)} triples ${s.kind === "export" ? "written" : "inferred"}`) : null,
        r && !d && r.status === "ok" ? el("p", { class: "small muted" }, "The store after this step is too large to keep: run the steps after it from the start.") : null,
        d && (d.added || d.removed) ? el("div", { class: "wb-p-diff" }, rows(d.predicates, "predicate"), rows(d.classes, "class")) : null,
        d ? samples("+", d.addedSamples, d.added) : null,
        d ? samples("-", d.removedSamples, d.removed) : null);
    }));
  }

  // ---------- downloads ----------
  async function download(op, accept, name, params) {
    try {
      await saveAll();
      const r = await request("POST", base() + "/" + op, { params: { ...params, format: accept === "text/turtle" ? "turtle" : "ntriples" }, accept });
      save_(r.text, name, accept);
    } catch (e) { ui.fail(e); }
  }
  async function downloadAll() {
    try {
      await saveAll();
      const [data, inferred] = await Promise.all([
        request("POST", base() + "/query", { body: new URLSearchParams({ query: "CONSTRUCT { ?s ?p ?o } WHERE { ?s ?p ?o }" }), type: "application/x-www-form-urlencoded", accept: "application/n-triples" }),
        request("POST", base() + "/infer", { params: { ...ruleParams(), format: "ntriples" }, accept: "application/n-triples" }),
      ]);
      save_(data.text + (data.text.endsWith("\n") || !data.text ? "" : "\n") + inferred.text, "data-and-inferred.nt", "application/n-triples");
    } catch (e) { ui.fail(e); }
  }
  function save_(text, name, type) {
    const a = el("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }

  // ---------- history: what other clients ran (#405) ----------
  // The Python client, a notebook or curl run queries, validations, inferences and pipeline
  // steps against the workspace; the server keeps the last 100. Opening one shows it here as if
  // the workbench had run it: a validation's report, an inference with why? on its triples.
  function buildHistoryPane() {
    fill($("#wb-pane-history"),
      el("div", { class: "wb-pane-bar" },
        el("span", { class: "muted small" }, "Runs by other clients: the Python client (", el("code", {}, "tripletrail.connect"), "), a notebook, curl."),
        el("span", { class: "grow" }),
        el("button", { class: "btn small", type: "button", onclick: clearHistory }, "Clear")),
      el("div", { class: "wb-summary small", id: "wb-h-summary" }),
      el("div", { class: "wb-table", id: "wb-h-table" }));
  }
  async function loadHistory() {
    if (!wsId) return;
    let r;
    try { r = await json("GET", base() + "/history", { params: { since: historyLast || null } }); } catch (e) { return; }
    if (!r || !wsId) return;
    if (r.last < historyLast) historyRuns = []; // cleared, or the server restarted
    const known = new Set(historyRuns.map((h) => h.id));
    historyRuns = r.runs.filter((h) => !known.has(h.id)).concat(historyRuns).slice(0, 100);
    const changed = r.last !== historyLast || r.runs.length;
    historyLast = r.last;
    if (pane === "history") historySeen = historyLast;
    renderHistoryBadge();
    if (changed || !tables.history) renderHistory();
  }
  function renderHistoryBadge() {
    const b = $("#wb-h-badge");
    if (!b) return;
    const n = historyRuns.filter((h) => h.id > historySeen).length;
    b.hidden = n === 0;
    b.textContent = n > 99 ? "99+" : String(n);
  }
  const opLabel = (h) => (h.op === "complete" ? "complete step " + h.step : h.op + (h.step ? " after step " + h.step : ""));
  function historyText(h) {
    if (h.text) return h.text.replace(/\s+/g, " ").trim();
    return Object.entries(h.params || {}).map(([k, v]) => k + "=" + v).join(" ");
  }
  function renderHistory() {
    const holder = $("#wb-h-table");
    if (!holder) return;
    fill($("#wb-h-summary"), historyRuns.length
      ? el("span", { class: "muted" }, `${historyRuns.length} ${historyRuns.length === 1 ? "run" : "runs"}. Click one to open it.`)
      : el("span", { class: "muted" }, "Nothing yet. ", el("code", {}, `ws = tripletrail.connect("${location.origin}", workspace="${ws.name}")`), ", then ", el("code", {}, "ws.query(…)"), ", ", el("code", {}, "ws.validate()"), " or ", el("code", {}, "ws.infer(provenance=True)"), "."));
    const t = table("history", holder, { data: historyRuns, index: "id", columns: [
      { title: "When", field: "at", formatter: (c) => textCell(ui.fmtTime(c.getValue())), width: 175 },
      { title: "Client", field: "client", headerFilter: "input", width: 90, formatter: (c) => textCell(c.getValue()) },
      { title: "Run", field: "op", formatter: (c) => textCell(opLabel(c.getRow().getData())), width: 140 },
      { title: "", field: "text", formatter: (c) => textCell(historyText(c.getRow().getData()).slice(0, 200)), cssClass: "mono small", widthGrow: 3 },
      { title: "Status", field: "status", hozAlign: "right", width: 85,
        formatter: (c) => { c.getElement().classList.toggle("error", c.getValue() >= 400); return textCell(c.getValue()); } },
      { title: "ms", field: "ms", hozAlign: "right", width: 80, formatter: (c) => textCell(c.getValue()) },
      { title: "Result", field: "summary", formatter: (c) => textCell(c.getValue() || ""), widthGrow: 2 }] });
    t.on("rowClick", (e, row) => openRun(row.getData()));
  }
  async function clearHistory() {
    try {
      await request("DELETE", base() + "/history");
      historyRuns = []; historySeen = historyLast;
      renderHistory(); renderHistoryBadge();
    } catch (e) { ui.fail(e); }
  }
  /** Shows a run where the workbench shows its own: a query in the Query tab, an update as a
   *  file, a validation's report, an inference with why?, the pipeline. */
  async function openRun(h) {
    try {
      if (h.status >= 400) { ui.toast(`${opLabel(h)} failed (${h.status}): ${h.summary || ""}`, "bad"); }
      if ((h.op === "query" || h.op === "explain" || h.op === "update") && h.text) {
        const update = h.op === "update";
        const path = (update ? "updates/history-" : "queries/history-") + h.id + (update ? ".ru" : ".rq");
        const text = h.text.trim() + "\n";
        await request("PUT", base() + "/files/" + path, { body: text, type: update ? "application/sparql-update" : "application/sparql-query" });
        docs.set(path, { text, state: null, dirty: false, error: null, timer: null });
        await refreshWorkspace();
        openFile(path);
        queryStore = h.step && !update ? String(h.step) : "";
        const sel = $("#wb-q-store"); if (sel) sel.value = queryStore;
        showPane("query");
        // A query is read only: run it again to show its rows. An update is left to run.
        if (!update) run();
        else ui.toast("The update is in " + path + ": Run applies it to the scratch store.", "ok");
        return;
      }
      if (h.op === "validate" || h.op === "infer") {
        const full = h.result ? await json("GET", base() + "/history/" + h.id) : null;
        if (h.op === "validate") {
          showPane("validate");
          if (full && full.result) {
            const v = full.result;
            v.parents = await parentShapes();
            previousValidation = validation; validation = v;
            const rules = $("#wb-v-rules"); if (rules) rules.checked = (h.params || {}).rules !== "false";
            renderValidation();
          } else run();
        } else {
          showPane("rules");
          if (full && full.result && full.result.provenance) {
            inference = full.result;
            inferredByKey = new Map(inference.triples.map((t, i) => [tripleKey(t.subject, t.predicate, t.object), i]));
            renderRules();
          } else {
            // Answered as triples, without provenance: run the rules again (the files may have
            // changed since) to see why each triple is there.
            await run();
            ui.toast("Ran the rules again for their provenance: the client asked for the triples only.", "ok");
          }
        }
        return;
      }
      if (h.op === "tests") { showPane("tests"); return run(); }
      if (h.op === "pipeline" || h.op === "complete") { showPane("pipeline"); return; }
    } catch (e) { ui.fail(e); }
  }

  // ---------- the drawer: describe and why? ----------
  function openDrawer(title, ...content) {
    fill($("#wb-drawer-title"), title);
    fill($("#wb-drawer-body"), ...content);
    $("#wb-drawer").hidden = false;
  }
  function closeDrawer() { const d = $("#wb-drawer"); if (d) d.hidden = true; }

  /** Where a triple came from: the rule, focus node, pass and bindings, or the data. */
  async function why(t) {
    const key = tripleKey(t.subject, t.predicate, t.object);
    const i = inference ? inferredByKey.get(key) : undefined;
    const triple = el("p", { class: "wb-triple mono" }, termNode(t.subject), " ", termNode(t.predicate), " ", termNode(t.object), " .");
    if (i === undefined) {
      const asserted = await askAsserted(t);
      openDrawer("Why?", triple,
        asserted ? el("p", {}, el("span", { class: "chip" }, "asserted"), " The triple is in the data: a file of data/.")
          : el("p", { class: "muted" }, inference ? "The last rules run did not infer this triple, and it is not in the data." : "Run Rules to see whether a rule infers it."),
        inference ? null : el("button", { class: "btn small", type: "button", onclick: () => { showPane("rules"); run().then(() => why(t)); } }, "Run rules"));
      addBlame(t, triple);
      return;
    }
    const o = inference.provenance[i];
    const r = inference.rules[o.rule] || {};
    // $shapesGraph and $currentShape are pre-bound for every solution: only noise here.
    const bindings = Object.entries(o.bindings || {}).filter(([k]) => k !== "shapesGraph" && k !== "currentShape");
    const facts = { triple: [t.subject, t.predicate, t.object].map(termText), rule: o.rule, shape: termText(o.shape), focus: termText(o.focus), pass: o.pass,
      construct: r.construct ? r.construct.trim() : undefined, bindings: Object.fromEntries(bindings.map(([k, v]) => [k, termText(v)])) };
    openDrawer("Why?", triple,
      el("p", {}, el("span", { class: "chip ro" }, "inferred"), ` by a rule, in pass ${o.pass} (group ${o.group}).`,
        " ", el("button", { class: "link small", type: "button", title: "Ask the AI assistant to explain this in plain words",
          onclick: () => aiExplain("Explain in plain words why this triple was inferred, and whether the rule does what its name says:\n" + JSON.stringify(facts, null, 1)) }, "Explain with AI")),
      el("dl", { class: "kv" },
        el("dt", {}, "Rule"), el("dd", {}, ruleName(o.rule), o.rule.startsWith("<") ? el("div", { class: "mono small muted" }, o.rule) : null),
        el("dt", {}, "Shape"), el("dd", {}, termNode(o.shape)),
        el("dt", {}, "Focus node"), el("dd", {}, termNode(o.focus)),
        r.order != null ? el("dt", {}, "Order") : null, r.order != null ? el("dd", {}, String(r.order)) : null),
      r.construct ? el("div", {}, el("h3", {}, "CONSTRUCT"), el("pre", { class: "wb-pre" }, r.construct.trim()),
        el("button", { class: "btn small", type: "button", onclick: () => tryInQuery(o.rule) }, "Try in Query")) : null,
      r.subject ? el("div", {}, el("h3", {}, "Triple rule"), el("p", { class: "mono small" }, ruleText(r.subject), " ", ruleText(r.predicate), " ", ruleText(r.object))) : null,
      bindings.length ? el("div", {}, el("h3", {}, "Bindings"), el("table", { class: "grid" }, el("tbody", {},
        bindings.map(([k, v]) => el("tr", {}, el("td", { class: "mono" }, "?" + k), el("td", {}, termNode(v))))))) : null);
    addBlame(t, triple);
  }
  /** Which pipeline steps added or removed the triple, and from which file (blame), added to the
   *  why? panel once known, when the pipeline has run. */
  async function addBlame(t, triple) {
    if (!lastPipeline || !(lastPipeline.steps || []).some((s) => s.snapshot)) return;
    let b;
    try {
      b = await json("GET", base() + "/pipeline/blame", { params: { subject: ntTerm(t.subject), predicate: ntTerm(t.predicate), object: ntTerm(t.object) } });
    } catch (e) { return; }
    if (!triple.isConnected) return; // the panel shows something else now
    const open = (path) => el("button", { class: "link small mono", type: "button", onclick: () => openTestFile(path) }, path);
    const partial = b.checkedSteps < b.steps ? ` The stores after step ${b.checkedSteps} are too large to keep, so later steps are not checked.` : "";
    $("#wb-drawer-body").append(el("div", { class: "wb-blame" }, el("h3", {}, "Pipeline"),
      b.events.length
        ? el("ul", { class: "small" }, ...b.events.map((e) => el("li", {},
          el("span", { class: "chip" + (e.change === "added" ? "" : " ro") }, e.change), ` by step ${e.step} (${e.name})`,
          e.file ? [" in ", open(e.file)] : e.kind === "infer" ? " by its rules: " + e.files.join(", ") : null)))
        : el("p", { class: "small muted" }, "No step of the last pipeline run added it."),
      partial ? el("p", { class: "small muted" }, partial.trim()) : null));
  }
  async function askAsserted(t) {
    try {
      const q = `ASK { ${ntTerm(t.subject)} ${ntTerm(t.predicate)} ${ntTerm(t.object)} }`;
      if (t.subject.type === "bnode" || t.object.type === "bnode") return true;
      const r = await json("POST", base() + "/query", { body: new URLSearchParams({ query: q }), type: "application/x-www-form-urlencoded", accept: "application/sparql-results+json" });
      return r.boolean;
    } catch (e) { return false; }
  }

  /** A node's triples in the scratch store, with what the last rules run inferred about it; `mark`
   *  ({path, value}) highlights the triple a validation result is about. */
  async function describe(node, mark) {
    if (!wsId) return;
    const iri = node.type === "uri";
    const s = iri ? `<${node.value}>` : null;
    openDrawer(termText(node), el("p", { class: "muted" }, "Loading…"));
    if (!iri) { openDrawer(termText(node), el("p", { class: "muted small" }, "A blank node: its triples are listed with the node that points to it.")); return; }
    try {
      const q = (query) => json("POST", base() + "/query", { body: new URLSearchParams({ query }), type: "application/x-www-form-urlencoded", accept: "application/sparql-results+json" });
      const [out, inc] = await Promise.all([
        q(`SELECT ?p ?o WHERE { ${s} ?p ?o } ORDER BY ?p LIMIT 1000`),
        q(`SELECT ?s ?p WHERE { ?s ?p ${s} } ORDER BY ?p LIMIT 200`),
      ]);
      const rows = out.results.bindings.map((b) => ({ p: b.p, o: b.o, inferred: false }));
      if (inference) for (const t of inference.triples) if (t.subject.type === "uri" && t.subject.value === node.value
        && !rows.some((r) => termKey(r.p) === termKey(t.predicate) && termKey(r.o) === termKey(t.object))) rows.push({ p: t.predicate, o: t.object, inferred: true });
      const pathIri = mark && mark.path && mark.path.match(/^<([^>]+)>$/);
      const marked = (r) => mark && ((pathIri && r.p.value === pathIri[1]) || (mark.value && termKey(mark.value) === termKey(r.o)));
      const label = rows.find((r) => /#label$|prefLabel$/.test(r.p.value));
      const types = rows.filter((r) => r.p.value === RDF_TYPE).map((r) => termText(r.o));
      openDrawer(termText(node),
        el("p", { class: "mono small muted" }, node.value),
        label || types.length ? el("p", {}, label ? el("b", {}, label.o.value + " ") : null, types.length ? el("span", { class: "muted small" }, types.join(", ")) : null) : null,
        mark ? el("p", { class: "small" }, "The highlighted triples are the ones the validation result is about" + (mark.path ? ` (path ${shortPath(mark.path)})` : "") + ".") : null,
        el("table", { class: "grid wb-describe" }, el("tbody", {}, rows.map((r) => el("tr", { class: (marked(r) ? "marked" : "") + (r.inferred ? " inferred" : "") },
          el("td", {}, termNode(r.p)), el("td", {}, termNode(r.o)),
          el("td", {}, el("button", { class: "link small", type: "button", onclick: () => why({ subject: node, predicate: r.p, object: r.o }) }, r.inferred ? "inferred: why?" : "why?")))))),
        mark && !rows.some(marked) ? el("p", { class: "small muted" }, "No value for this path: the result is about a missing value.") : null,
        inc.results.bindings.length ? el("div", {}, el("h3", {}, "Pointed to by"), el("table", { class: "grid wb-describe" }, el("tbody", {},
          inc.results.bindings.map((b) => el("tr", {}, el("td", {}, termNode(b.s)), el("td", {}, termNode(b.p))))))) : null);
    } catch (e) { openDrawer(termText(node), el("p", { class: "error" }, e.message)); }
  }

  // ---------- sample from a dataset ----------
  async function sampleDialog() {
    let datasets = [];
    try { datasets = (await ui.api("GET", "/$/datasets")).datasets; } catch (e) { ui.fail(e); return; }
    if (!datasets.length) { ui.toast("No dataset to sample from."); return; }
    const d = el("dialog", { class: "dlg" });
    const ds = el("select", { name: "dataset" }, datasets.map((x) => el("option", { value: x.name }, x.name)));
    const classes = el("textarea", { rows: 4, class: "mono", placeholder: "http://data.europa.eu/949/OperationalPoint" });
    const suggestions = el("div", { class: "wb-suggest small" });
    const instances = el("input", { type: "number", value: 100, min: 1, max: 100000 });
    const hops = el("input", { type: "number", value: 2, min: 1, max: 5 });
    const msg = el("p", { class: "small" });
    const suggest = async () => {
      fill(suggestions, el("span", { class: "muted" }, "Classes…"));
      try {
        const p = await ui.api("GET", "/$/datasets/" + encodeURIComponent(ds.value) + "/profile");
        const found = [];
        (function walk(x) { if (Array.isArray(x)) x.forEach(walk); else if (x && typeof x === "object") { if (typeof x.class === "string") found.push([x.class, x.count]); Object.values(x).forEach(walk); } })(p);
        const seen = new Set();
        fill(suggestions, ...found.filter(([c]) => !seen.has(c) && seen.add(c)).slice(0, 40).map(([c, n]) =>
          el("button", { class: "chip", type: "button", title: c, onclick: () => { classes.value = (classes.value.trim() ? classes.value.trim() + "\n" : "") + c; } }, shortIri(c) + (n != null ? ` ${ui.fmtNum(n)}` : ""))));
      } catch (e) { fill(suggestions, el("span", { class: "muted" }, "No profile for this dataset.")); }
    };
    ds.addEventListener("change", suggest);
    d.append(el("form", { method: "dialog" },
      el("div", { class: "dlg-head" }, el("h2", {}, "Sample a dataset"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, "Copies instances of the classes and what they point to into data/ as N-Triples. The dataset is only read."),
        el("label", { class: "field" }, "Dataset", ds),
        el("label", { class: "field" }, "Classes (one IRI per line)", classes), suggestions,
        el("div", { class: "row" }, el("label", { class: "field" }, "Instances per class", instances), el("label", { class: "field" }, "Hops", hops)), msg),
      el("div", { class: "dlg-foot" }, el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Cancel"),
        el("button", { class: "btn primary", type: "submit", onclick: async (e) => {
          e.preventDefault();
          msg.textContent = "Sampling…";
          try {
            const r = await json("POST", base() + "/sample", { body: JSON.stringify({ dataset: ds.value, classes: classes.value.split(/\s+/).filter(Boolean), instances: +instances.value, hops: +hops.value }), type: "application/json" });
            await load(r.file);
            learnPrefixes(); storeTerms = null;
            await refreshWorkspace();
            d.close();
            ui.toast(`${r.file}: ${ui.fmtNum(r.triples)} triples of ${ui.fmtNum(r.instances)} instances` + (r.truncated ? " (cut at the workspace's limit)" : ""), "ok");
          } catch (err) { fill(msg, el("span", { class: "error" }, err.message)); }
        } }, "Sample"))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
    suggest();
  }

  // ---------- promote shapes to a dataset (#400) ----------
  // The files of shapes/ become the next version of a dataset's own shape set (#137), through
  // its admin API: checked there, the version before kept for a rollback. The workspace never
  // writes to a dataset otherwise.
  async function promoteDialog() {
    if (!(await saveAll())) { ui.toast("A file has a syntax error: fix it first (its line is marked).", "bad"); return; }
    await refreshWorkspace();
    let datasets = [];
    try { datasets = (await ui.api("GET", "/$/datasets")).datasets; } catch (e) { ui.fail(e); return; }
    if (!datasets.length) { ui.toast("No dataset to promote the shapes to."); return; }
    const files = {}, refused = [];
    for (const f of ws.files.filter((f) => f.path.startsWith("shapes/"))) {
      const name = f.path.split("/").pop();
      const d = docs.get(f.path);
      if (!name.endsWith(".ttl")) refused.push([f.path, "a shape set holds Turtle (.ttl) files"]);
      else if (!d) refused.push([f.path, "too large to read here"]);
      else if (files[name] != null) refused.push([f.path, "another file of shapes/ has its name (subfolders are one set)"]);
      else files[name] = d.state ? d.state.doc.toString() : d.text;
    }
    const count = Object.keys(files).length;
    const d = el("dialog", { class: "dlg" });
    const ds = el("select", {}, datasets.map((x) => el("option", { value: x.name }, x.name)));
    const now = el("p", { class: "small muted" });
    const msg = el("p", { class: "small" });
    const current = async () => {
      fill(now, "…");
      try {
        const r = await ui.api("GET", "/$/datasets/" + encodeURIComponent(ds.value) + "/shapes/files");
        fill(now, r.version ? `Its own shapes now: version ${r.version}, ${r.files.length} files. They are replaced; that version is kept for a rollback.` : "It has no shapes of its own yet: only generated ones.");
      } catch (e) { fill(now, e.message); }
    };
    ds.addEventListener("change", current);
    const go = async (e) => {
      e.preventDefault();
      fill(msg, el("span", { class: "muted" }, "Promoting…"));
      try {
        const r = await ui.api("PUT", "/$/datasets/" + encodeURIComponent(ds.value) + "/shapes/files", { type: "application/json",
          body: JSON.stringify({ files, source: { workspace: wsId, name: ws.name, at: new Date().toISOString(), git: ws.git || undefined } }) });
        d.close();
        ui.toast(`${ds.value}'s shapes are now version ${r.version} (${r.files} files).`, "ok");
      } catch (err) {
        fill(msg, el("span", { class: "error" }, err.status === 401 || err.status === 403 ? "Promoting shapes needs the admin token." : err.message));
      }
    };
    d.append(el("form", { method: "dialog", onsubmit: go },
      el("div", { class: "dlg-head" }, el("h2", {}, "Promote shapes"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, `The ${count} Turtle files of shapes/ become the next version of the dataset's own shapes, which the dataset's validation and maps read.`),
        el("label", { class: "field" }, "Dataset", ds), now,
        refused.length ? el("div", { class: "small" }, el("p", {}, "Not promoted:"), el("ul", {}, refused.map(([path, why]) => el("li", {}, el("span", { class: "mono" }, path), " (" + why + ")")))) : null,
        msg),
      el("div", { class: "dlg-foot" }, el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Cancel"),
        el("button", { class: "btn primary", type: "submit", disabled: count === 0 }, "Promote"))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
    current();
  }

  // ---------- pull from git (#400) ----------
  // The repository is the master: its files (of the workspace's folders, under a path) are
  // written over the workspace's; files only here stay. An admin's: the server fetches the URL.
  function pullDialog() {
    const last = ws.git || {};
    const d = el("dialog", { class: "dlg" });
    const url = el("input", { class: "mono", value: last.url || "", placeholder: "https://github.com/org/repo.git", required: true });
    const ref = el("input", { value: last.ref || "main" });
    const path = el("input", { class: "mono", value: last.path || "", placeholder: "the repository's root" });
    const msg = el("p", { class: "small" });
    const go = async (e) => {
      e.preventDefault();
      if (!(await saveAll())) { fill(msg, el("span", { class: "error" }, "A file has a syntax error: fix it first.")); return; }
      fill(msg, el("span", { class: "muted" }, "Pulling…"));
      try {
        const r = await json("POST", base() + "/pull", { body: JSON.stringify({ url: url.value.trim(), ref: ref.value.trim() || "HEAD", path: path.value.trim() }), type: "application/json" });
        for (const p of r.changed) await load(p);
        await refreshWorkspace();
        if (openPath && r.changed.includes(openPath)) openFile(openPath);
        d.close();
        ui.toast(`Pulled ${r.git.commit.slice(0, 10)}: ${r.changed.length ? r.changed.length + " files changed" : "nothing changed"}.`, "ok");
      } catch (err) {
        fill(msg, el("span", { class: "error" }, err.status === 403 ? "Pulling from git needs the admin token." : err.message));
      }
    };
    d.append(el("form", { method: "dialog", onsubmit: go },
      el("div", { class: "dlg-head" }, el("h2", {}, "Pull from git"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, "Writes the files of the repository's folder that a workspace holds (data/, shapes/, rules/, queries/, updates/, lift/, tests/, pipeline.yaml, README.md) over the workspace's. Files only here stay."),
        last.commit ? el("p", { class: "small" }, "Last pulled: ", el("span", { class: "mono" }, last.commit.slice(0, 10)), " · " + ui.fmtTime(last.at)) : null,
        el("label", { class: "field" }, "Repository", url),
        el("div", { class: "row" }, el("label", { class: "field" }, "Branch or ref", ref), el("label", { class: "field" }, "Folder", path)), msg),
      el("div", { class: "dlg-foot" }, el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Cancel"),
        el("button", { class: "btn primary", type: "submit" }, "Pull"))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }

  // ---------- share links (#400) ----------
  // A small workspace travels in a link: its files, compressed (deflate) and base64url in the
  // URL's hash, which never reaches a server. Opening the link makes a new scratch workspace
  // with them. What a sample copied from a dataset is never put in a link.
  const SHARE_MAX = 64 << 10; // characters of the hash
  const b64url = (bytes) => {
    let s = "";
    for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
    return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  };
  const unb64url = (t) => Uint8Array.from(atob(t.replace(/-/g, "+").replace(/_/g, "/")), (c) => c.charCodeAt(0));
  async function pack(value) {
    const stream = new Blob([JSON.stringify(value)]).stream().pipeThrough(new CompressionStream("deflate-raw"));
    return b64url(new Uint8Array(await new Response(stream).arrayBuffer()));
  }
  async function unpack(text) {
    const stream = new Blob([unb64url(text)]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
    return JSON.parse(await new Response(stream).text());
  }
  async function shareDialog() {
    if (!(await saveAll())) { ui.toast("A file has a syntax error: fix it first (its line is marked).", "bad"); return; }
    await refreshWorkspace();
    const files = {}, left = [];
    for (const f of ws.files) {
      const d = docs.get(f.path);
      if (f.sampled) left.push([f.path, "sampled from a dataset"]);
      else if (!d) left.push([f.path, f.path.endsWith(".gz") ? "compressed" : "too large"]);
      else files[f.path] = d.state ? d.state.doc.toString() : d.text;
    }
    let link = null, error = null;
    try {
      const packed = await pack({ v: 1, name: ws.name, files });
      if (packed.length > SHARE_MAX) error = `The files take ${ui.fmtBytes(packed.length)} in a link, more than the ${ui.fmtBytes(SHARE_MAX)} a link may hold: keep the workspace and share its name instead, or leave out large data.`;
      else link = location.origin + "/$/ui/workbench#share=" + packed;
    } catch (e) { error = "This browser cannot compress a link: " + e.message; }
    const d = el("dialog", { class: "dlg" });
    const input = link ? el("input", { class: "mono", readonly: true, value: link, onfocus: (e) => e.target.select() }) : null;
    const copy = async () => {
      try { await navigator.clipboard.writeText(link); ui.toast("Link copied to the clipboard."); d.close(); }
      catch (e) { input.focus(); input.select(); ui.toast("Copy it by hand: the clipboard is not available here."); }
    };
    d.append(el("form", { method: "dialog" },
      el("div", { class: "dlg-head" }, el("h2", {}, "Share link"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, "The link holds the files themselves, not a reference to this workspace: whoever opens it on this server gets a new scratch workspace with a copy. Later changes here are not in it."),
        error ? el("p", { class: "error" }, error) : el("label", { class: "field" }, `${Object.keys(files).length} files, ${ui.fmtBytes(link.length)}`, input),
        left.length ? el("div", { class: "small" }, el("p", {}, "Left out:"), el("ul", {}, left.map(([path, why]) => el("li", {}, el("span", { class: "mono" }, path), " (" + why + ")")))) : null),
      el("div", { class: "dlg-foot" }, el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Close"),
        link ? el("button", { class: "btn primary", type: "button", onclick: copy }, "Copy link") : null)));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }
  async function openShared(root) {
    const text = location.hash.slice("#share=".length);
    history.replaceState(null, "", location.pathname);
    let shared;
    try {
      shared = await unpack(text);
      if (!shared || typeof shared.files !== "object" || Array.isArray(shared.files)) throw new Error("no files in it");
    } catch {
      // A link cut short fails to decompress, with a message that says little.
      fill(root, el("div", { class: "card" }, el("p", { class: "empty error" }, "This share link is damaged: it may have been cut short when it was copied.")),
        el("p", {}, el("a", { href: "/$/ui/workbench", "data-route": "" }, "All workspaces")));
      return;
    }
    const files = {};
    for (const [path, value] of Object.entries(shared.files)) if (typeof value === "string") files[path] = value;
    const name = typeof shared.name === "string" && shared.name.trim() ? shared.name.trim().slice(0, 200) : "Shared";
    fill(root, el("p", { class: "muted loading" }, "Making a workspace from the link…"));
    try {
      const w = await json("POST", API, { body: JSON.stringify({ name: name + " (shared)", files }), type: "application/json" });
      ui.go("workbench/" + w.id, true);
    } catch (e) {
      fill(root, el("div", { class: "card" }, el("p", { class: "empty error" }, "The link's workspace could not be made: " + e.message)),
        el("p", {}, el("a", { href: "/$/ui/workbench", "data-route": "" }, "All workspaces")));
    }
  }

  // ---------- AI: configure with your own key (#146) ----------
  // The assistant proposes shape files; the user looks at the change, applies it to the file
  // and promotes it as any other. It reads through the server under the user's own token and
  // never writes: its tools only read, and check a proposed file without keeping it.
  const AI_SYSTEM = `You help develop a TripleTrail workspace: SHACL shapes in Turtle, SHACL-AF rules, SPARQL
queries and updates, and tests. TripleTrail is an RDF database.
TripleTrail's readers (the map, describe pages, search, OGC API Features, the REST and GraphQL APIs) work
from the shapes of a dataset. A dataset starts with shapes generated from its data; a dataset's own
shapes replace the generated shape of each class they target (sh:targetClass), and sh:deactivated true
leaves a class out. Reader settings are annotations in the tts: vocabulary, given below.

How to work:
- Look before you write: call get_profile for the dataset's classes, properties, counts and geometry
  paths, and get_dataset_shapes for the shapes in use now. Never guess IRIs: use the ones they give.
- Write only what the user asked for, starting from the shape in use for that class.
- Where files go: shapes/NAME.ttl (shapes), rules/NAME.ttl (sh:rule with sh:SPARQLRule or sh:TripleRule,
  on a node shape with a target), queries/NAME.rq, updates/NAME.ru. A test is a folder tests/NAME/ with
  input.ttl, one target (query.rq, update.ru, rules.ttl or shapes.ttl) and the expected output
  (expected.ttl for a graph, expected.srj or expected.csv for a SELECT, violations.yaml or none for
  shapes); an optional test.yaml names other files, as "query: ../../queries/q.rq". Use the prefixes,
  classes and properties that get_profile and the workspace's files use.
- Updates never run here: propose them as files. run_query runs a query on the workspace's own data.
- To explain a validation result or a why? answer: say in plain words what the data has, what the
  shape or rule asks, naming the offending value and path, and propose the smallest fix (to the data
  or to the shape) as a file.
- A profile (tts:inProfile) is a use case, such as operations or maintenance, whose readers ask for its
  shapes only; a shape in no profile is in all. To put a class in a profile or take it out, call
  set_profile and propose the shape it answers.
- Check every file you propose with check_file (and a shape file also with check_shapes), fix what
  they report, and check again.
- Give each file you propose whole, as a line "File: <path>" followed by one fenced block (\`\`\`turtle,
  \`\`\`sparql or \`\`\`yaml) with all its prefixes. Keep a file's existing name when you change it. Then say in one or two
  sentences what changes. Do not repeat a file you did not change.
- Answer in the user's language, briefly. If something can't be done in the workspace, say so.`;
  let aiMessages = [], aiAbort = null, aiVocabulary = null, aiPreferred = null;
  /**
   * "Configure with AI" from another page (`/$/ui/workbench?configure=ds`): a scratch workspace
   * holding the dataset's own shape files, if it has any, so that promoting what the assistant
   * adds keeps them; then the AI tab on that dataset.
   */
  async function startConfigure(root, ds) {
    fill(root, el("p", { class: "muted loading" }, "Making a workspace to configure " + ds + "…"));
    try {
      const files = {};
      // An in-memory dataset without --location keeps no shapes of its own (409).
      const own = await ui.api("GET", "/$/datasets/" + encodeURIComponent(ds) + "/shapes/files").catch((e) => { if (e.status === 409) return null; throw e; });
      for (const f of (own && own.files) || []) {
        const name = typeof f === "string" ? f : f.name;
        if (!name) continue;
        files["shapes/" + name] = (await request("GET", "/$/datasets/" + encodeURIComponent(ds) + "/shapes/files/" + encodeURIComponent(name), { accept: "text/turtle" })).text;
      }
      const w = await json("POST", API, { body: JSON.stringify({ name: "Configure " + ds, files }), type: "application/json" });
      ui.go("workbench/" + encodeURIComponent(w.id) + "?ai=" + encodeURIComponent(ds), true);
    } catch (e) {
      fill(root, el("div", { class: "card" }, el("p", { class: "empty error" }, "The workspace could not be made: " + e.message)),
        el("p", {}, el("a", { href: "/$/ui/workbench", "data-route": "" }, "All workspaces")));
    }
  }
  function buildAiPane() {
    const services = el("select", { id: "wb-ai-service", class: "small", "aria-label": "AI service", onchange: aiStatus });
    fill($("#wb-pane-ai"),
      el("div", { class: "wb-pane-bar" },
        el("label", { class: "small" }, "Dataset ", el("select", { id: "wb-ai-ds", title: "The dataset whose profile and shapes the assistant reads" })),
        el("label", { class: "check small", title: "Let the assistant run read-only SPARQL queries on the dataset: it then sees values of the data, not only its classes and properties" },
          el("input", { type: "checkbox", id: "wb-ai-data" }), "may query the data"),
        el("button", { class: "btn small", type: "button", title: "Draw the dataset's map with this workspace's shape files, without saving them",
          onclick: () => previewOnMap($("#wb-ai-ds").value).catch((e) => ui.fail(e)) }, "Preview on map"),
        el("span", { class: "grow" }),
        services,
        el("button", { class: "btn small", type: "button", onclick: aiSettingsDialog }, "Key and model…"),
        el("button", { class: "btn small", type: "button", onclick: aiNew, title: "Forget the conversation" }, "New")),
      el("div", { class: "wb-ai-log", id: "wb-ai-log", "aria-live": "polite" }),
      el("form", { class: "wb-ai-ask", onsubmit: (e) => { e.preventDefault(); aiAsk(); } },
        el("textarea", { id: "wb-ai-q", rows: 2, placeholder: "e.g. Show stations in blue, sections coloured by maximum speed, and hide kilometre posts below zoom 12",
          onkeydown: (e) => { if (e.key === "Enter" && !e.shiftKey && !e.isComposing) { e.preventDefault(); aiAsk(); } } }),
        el("button", { class: "btn primary", type: "submit", id: "wb-ai-go" }, "Ask"),
        el("button", { class: "btn", type: "button", id: "wb-ai-stop", hidden: true, onclick: () => aiAbort && aiAbort.abort() }, "Stop")));
    aiStatus();
    aiDatasets();
  }
  async function aiDatasets() {
    const s = $("#wb-ai-ds");
    if (!s) return;
    try {
      const list = (await ui.api("GET", "/$/datasets")).datasets || [];
      const pick = [aiPreferred, ws && ws.overlay && ws.overlay.dataset].find((x) => x && list.some((d) => d.name === x)) || (list[0] && list[0].name);
      fill(s, list.map((x) => el("option", { value: x.name, selected: x.name === pick }, x.name)));
    } catch (e) { /* the pane says so when asked */ }
  }
  /** The services set up in this browser, or how to set one up. */
  function aiStatus() {
    const s = $("#wb-ai-service");
    const picked = s && s.value;
    const all = window.TripleTrailAI.connections();
    if (s) fill(s, all.length ? all.map((c) => el("option", { value: c.id, selected: c.id === picked }, c.label + (c.model ? " · " + c.model : "")))
      : [el("option", { value: "" }, "No AI service set up")]);
    const log = $("#wb-ai-log");
    if (log && !aiMessages.length) {
      fill(log, el("div", { class: "muted small wb-ai-intro" },
        el("p", {}, "Ask for a change to how the dataset is shown: which classes go on the map and in which colour, facets, labels, what the describe pages group. The assistant reads the dataset's profile and shapes, writes shape files, and checks them; you look at each change and apply it to a file here, then ",
          el("strong", {}, "Promote shapes…"), " as usual."),
        el("p", {}, "It also drafts rules, queries, updates and tests from a description, and explains a validation result or a why? answer: click ",
          el("strong", {}, "explain"), " next to one. What it writes is a change you apply or leave; it never changes a dataset."),
        el("p", {}, all.length
          ? "Requests go from this browser to the AI service only, with your key; the classes, properties and shapes of the dataset are sent, and values of the data only when \"may query the data\" is ticked."
          : el("span", {}, "Bring your own key: ", el("a", { href: "#", onclick: (e) => { e.preventDefault(); aiSettingsDialog(); } }, "set up an AI service"),
            " (OpenAI, Anthropic, Mistral and other OpenAI-compatible services, or a local model with Ollama or LM Studio). It is the same setting as the assistant of the Query page."))));
    }
  }
  function aiSettingsDialog() {
    const AI = window.TripleTrailAI;
    const current = AI.connection($("#wb-ai-service") && $("#wb-ai-service").value);
    const d = el("dialog", { class: "dlg" });
    const provider = el("select", {}, el("option", { value: "" }, "Choose…"), AI.PROVIDERS.map((p, i) => el("option", { value: String(i) }, p.name)));
    const baseUrl = el("input", { type: "url", required: true, value: current ? current.baseUrl : "", placeholder: "https://api.openai.com/v1" });
    const key = el("input", { type: "password", autocomplete: "off", value: current ? current.apiKey : "" });
    const model = el("input", { value: current ? current.model : "" });
    const keys = el("p", { class: "muted small" });
    provider.addEventListener("change", () => {
      const p = AI.PROVIDERS[+provider.value];
      if (!p) return;
      baseUrl.value = p.baseUrl; model.value = p.model;
      fill(keys, p.noKey ? "No key needed. The local server must allow requests from this page (CORS)." : p.keys ? el("a", { href: p.keys, target: "_blank", rel: "noopener" }, "Get a key") : null);
    });
    d.append(el("form", { method: "dialog", onsubmit: (e) => {
      e.preventDefault();
      try {
        const id = AI.saveConnection({ id: current ? current.id : null, baseUrl: baseUrl.value.trim(), apiKey: key.value.trim(), model: model.value.trim() });
        d.close(); aiStatus(); $("#wb-ai-service").value = id;
      } catch (err) { ui.fail(err); }
    } },
      el("div", { class: "dlg-head" }, el("h2", {}, "AI service"), el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕")),
      el("div", { class: "dlg-body" },
        el("p", { class: "muted small" }, "The key stays in this browser and is sent only to the service below. The Query page's assistant uses the same setting."),
        el("label", { class: "field" }, "Provider", provider),
        el("label", { class: "field" }, "Base URL", baseUrl),
        el("label", { class: "field" }, "API key", key), keys,
        el("label", { class: "field" }, "Model", model)),
      el("div", { class: "dlg-foot" },
        current ? el("button", { class: "btn", type: "button", onclick: () => { AI.removeConnection(current.id); d.close(); aiStatus(); } }, "Remove") : null,
        el("span", { class: "grow" }),
        el("button", { class: "btn", type: "button", onclick: () => d.close() }, "Cancel"),
        el("button", { class: "btn primary", type: "submit" }, "Save"))));
    d.addEventListener("close", () => d.remove());
    document.body.append(d);
    d.showModal();
  }
  function aiNew() {
    if (aiAbort) aiAbort.abort();
    aiMessages = [];
    aiStatus();
  }
  /** The text of a workspace file as it is in the editor now. */
  function fileText(path) {
    const d = docs.get(path);
    if (!d) return null;
    return path === openPath && view ? view.state.doc.toString() : d.state ? d.state.doc.toString() : d.text;
  }
  const SH = "http://www.w3.org/ns/shacl#", TTS = "http://w3id.org/tripletrail/shapes#";
  /** An IRI a model gave, safe in a SPARQL query's <…>. */
  const iri = (s) => String(s || "").replace(/[<>"{}|^`\\\s]/g, "");
  /** A pattern for the triples ?s ?p ?o of class `cls`'s node shapes (?shape) and their property shapes. */
  const shapeOf = (cls) => `?shape <${SH}targetClass> <${iri(cls)}> . { BIND(?shape AS ?n) } UNION { ?shape <${SH}property> ?n } ?n (<urn:x>|!<urn:x>)* ?s . ?s ?p ?o . FILTER(?s = ?n || isBlank(?s))`;
  function aiTools(ds) {
    const dsPath = "/$/datasets/" + encodeURIComponent(ds);
    const tools = [
      { name: "get_profile", description: "The dataset's classes with instance counts, labels, properties (counts, datatypes, target classes, code-list values) and geometry paths, as JSON.",
        parameters: { type: "object", properties: {} },
        run: async () => (await request("GET", dsPath + "/profile", { accept: "application/json" })).text },
      { name: "get_dataset_shapes", description: "The SHACL shapes the dataset's readers use now (its own shapes over the generated ones), as Turtle. 'class' (an IRI) keeps only that class's node shape with its property shapes; 'profile' (an IRI) only the shapes in that profile.",
        parameters: { type: "object", properties: { class: { type: "string", description: "Optional class IRI" }, profile: { type: "string", description: "Optional profile IRI" } } },
        run: async ({ class: cls, profile }) => {
          const params = profile ? { profile: iri(profile) } : {};
          if (!cls) return (await request("GET", dsPath + "/shapes", { params, accept: "text/turtle" })).text;
          return (await request("POST", dsPath + "/shapes/sparql", { params, body: `CONSTRUCT { ?s ?p ?o } WHERE { ${shapeOf(cls)} }`, type: "application/sparql-query", accept: "text/turtle" })).text;
        } },
      { name: "set_profile", description: "Puts a class's node shape in a profile (on: true) or takes it out (on: false); a profile is a use case whose readers ask for it, and a shape in no profile is in all. Answers the profiles before and after and the node shape in use with that change, as Turtle, which you then propose as a file. It changes nothing itself.",
        parameters: { type: "object", properties: { class: { type: "string", description: "The class IRI" }, profile: { type: "string", description: "The profile IRI" }, on: { type: "boolean" } }, required: ["class", "profile", "on"] },
        run: async ({ class: cls, profile, on }) => {
          const P = iri(profile), sparql = async (q) => JSON.parse((await request("POST", dsPath + "/shapes/sparql", { body: q, type: "application/sparql-query", accept: "application/sparql-results+json" })).text).results.bindings;
          const of = (await sparql(`SELECT ?p WHERE { ?shape <${SH}targetClass> <${iri(cls)}> ; <${TTS}inProfile> ?p }`)).map((b) => b.p.value);
          if (!(await sparql(`SELECT ?shape WHERE { ?shape <${SH}targetClass> <${iri(cls)}> } LIMIT 1`)).length) return "No shape in use targets " + cls + ": get_profile gives the classes.";
          const known = (await sparql(`SELECT DISTINCT ?p WHERE { { ?s <${TTS}inProfile> ?p } UNION { ?p a <${TTS}Profile> } }`)).map((b) => b.p.value);
          const others = [...new Set(known)].filter((p) => p !== P);
          let after;
          if (on) after = !of.length || of.includes(P) ? null : [...of, P];
          else if (of.length) after = of.includes(P) ? of.filter((p) => p !== P) : null;
          else after = others;
          if (after === null) return JSON.stringify({ before: of, note: "No change: the shape is " + (of.length ? "" : "in no profile, so ") + (on ? "in" : "not in") + " that profile already." });
          if (!after.length) return JSON.stringify({ before: of, note: on ? "" : "A shape in no profile is in all, so this class can only leave this profile by joining another one: name it first (a skos:Concept, " + TTS + "Profile), or hide the class from readers with tts:hiddenFrom." });
          const values = after.map((p) => "<" + p + ">").join(" ");
          const q = `CONSTRUCT { ?s ?p ?o } WHERE { { ${shapeOf(cls)} FILTER(?p != <${TTS}inSet> && !(?s = ?shape && ?p = <${TTS}inProfile>)) } UNION { ?shape <${SH}targetClass> <${iri(cls)}> . VALUES ?o { ${values} } BIND(?shape AS ?s) BIND(<${TTS}inProfile> AS ?p) } }`;
          const turtle = (await request("POST", dsPath + "/shapes/sparql", { body: q, type: "application/sparql-query", accept: "text/turtle" })).text;
          const fresh = known.includes(P) ? "" : " No shape names this profile yet: declare it in the file too, as <" + P + "> a <" + TTS + "Profile>, <http://www.w3.org/2004/02/skos/core#Concept> .";
          return JSON.stringify({ before: of, after, turtle, note: "If a workspace file already has this class's shape, change only its tts:inProfile there instead." + fresh });
        } },
      { name: "list_files", description: "The files of the workspace, with their folders (shapes/, rules/, queries/, updates/, tests/, data/).",
        parameters: { type: "object", properties: {} },
        run: async () => [...docs.keys()].join("\n") || "(no files)" },
      { name: "read_file", description: "A workspace file's text, as it is in the editor.",
        parameters: { type: "object", properties: { path: { type: "string" } }, required: ["path"] },
        run: async ({ path }) => fileText(path) ?? "No such file: " + path },
      { name: "check_shapes", description: "Checks a proposed shape file against the dataset's shape set without keeping it: Turtle syntax, the tts: annotations, and the set as a whole. Answers {valid, errors}.",
        parameters: { type: "object", properties: { file: { type: "string", description: "The file name, such as station.ttl" }, turtle: { type: "string" } }, required: ["file", "turtle"] },
        run: async ({ file, turtle }) => (await request("POST", dsPath + "/shapes/validate", { params: { file: String(file || "proposed.ttl").split("/").pop() }, body: turtle, type: "text/turtle", accept: "application/json" })).text },
      { name: "check_file", description: "Checks a proposed workspace file without keeping it, as the server would read it: RDF syntax, shapes and rules that compile (shapes/, rules/, a test's shapes.ttl or rules.ttl), SPARQL query (.rq) or update (.ru) syntax, YAML and JSON. Answers {valid, errors}.",
        parameters: { type: "object", properties: { path: { type: "string", description: "Where the file would go, such as rules/speed.ttl" }, text: { type: "string" } }, required: ["path", "text"] },
        run: async ({ path, text }) => {
          const where = aiPath(path);
          if (!where) return "Not a path the assistant may propose: " + path;
          return (await request("POST", base() + "/check", { body: JSON.stringify({ path: where, text: String(text || "") }), type: "application/json", accept: "application/json" })).text;
        } },
    ];
    if ($("#wb-ai-data") && $("#wb-ai-data").checked) {
      tools.push({ name: "run_sparql", description: "Runs a read-only SPARQL SELECT or ASK query on the dataset; at most 50 rows come back. Use it to look at values, e.g. the speeds in use.",
        parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
        run: async ({ query }) => {
          const r = await request("POST", "/" + encodeURIComponent(ds) + "/sparql", { body: query, type: "application/sparql-query", accept: "application/sparql-results+json", params: { "max-rows": 51 } });
          const j = JSON.parse(r.text);
          if (j.boolean !== undefined) return String(j.boolean);
          const rows = j.results.bindings;
          return JSON.stringify({ vars: j.head.vars, rows: rows.slice(0, 50).map((b) => Object.fromEntries(Object.entries(b).map(([k, v]) => [k, v.value]))), more: rows.length > 50 });
        } });
      tools.push({ name: "run_query", description: "Runs a read-only SPARQL query (SELECT, ASK or CONSTRUCT) on the workspace's own data (its data/ files, shapes and rules graphs), to try a query you propose; at most 50 rows or triples come back.",
        parameters: { type: "object", properties: { query: { type: "string" } }, required: ["query"] },
        run: async ({ query }) => {
          const r = await request("POST", base() + "/query", { body: query, type: "application/sparql-query", accept: "application/sparql-results+json, application/n-triples;q=0.9", params: { "max-rows": 51 } });
          if (!/json/.test(r.type || "") && !/^\s*\{/.test(r.text)) return r.text.split("\n").slice(0, 50).join("\n");
          const j = JSON.parse(r.text);
          if (j.boolean !== undefined) return String(j.boolean);
          const rows = j.results.bindings;
          return JSON.stringify({ vars: j.head.vars, rows: rows.slice(0, 50).map((b) => Object.fromEntries(Object.entries(b).map(([k, v]) => [k, v.value]))), more: rows.length > 50 });
        } });
      tools.push({ name: "run_tests", description: "Runs the workspace's saved tests (all, or the one named) and answers for each whether it passed, its error, and the first missing and unexpected results.",
        parameters: { type: "object", properties: { test: { type: "string", description: "Optional test name (its folder in tests/)" } } },
        run: async ({ test }) => {
          const r = JSON.parse((await request("POST", base() + "/tests", { params: test ? { test } : {}, accept: "application/json" })).text);
          const trim = (t) => ({ ...t, missing: (t.missing || []).slice(0, 10), unexpected: (t.unexpected || []).slice(0, 10) });
          return JSON.stringify({ ...r, tests: (r.tests || []).map(trim) });
        } });
    }
    return tools;
  }
  /** A term of a result (SPARQL JSON) as text: <iri>, a literal with its datatype or language. */
  function termText(t) {
    if (!t) return undefined;
    if (typeof t === "string") return t;
    if (t.type === "uri") return "<" + t.value + ">";
    if (t.type === "bnode") return "_:" + t.value;
    return JSON.stringify(t.value) + (t["xml:lang"] ? "@" + t["xml:lang"] : t.datatype ? "^^<" + t.datatype + ">" : "");
  }
  /** What the assistant needs of a validation result. */
  function resultFacts(r, parents) {
    const owner = r.sourceShape && r.sourceShape.type !== "uri" && parents ? parents.get(termKey(r.sourceShape)) : null;
    return { focusNode: termText(r.focusNode), path: r.resultPath || undefined, value: termText(r.value), shape: termText(r.sourceShape),
      propertyShapeOf: owner ? termText(owner) : undefined,
      constraint: termText(r.sourceConstraintComponent), severity: termText(r.severity), messages: r.messages || [] };
  }
  /** Asks the assistant, in the AI tab, to explain `question` (#409). */
  function aiExplain(question) {
    showPane("ai");
    const q = $("#wb-ai-q");
    if (!q) return;
    q.value = question;
    aiAsk();
  }
  async function aiAsk() {
    const q = $("#wb-ai-q");
    const text = q.value.trim();
    if (!text || aiAbort) return;
    const c = window.TripleTrailAI.connection($("#wb-ai-service").value);
    if (!c) { aiSettingsDialog(); return; }
    const ds = $("#wb-ai-ds").value;
    if (!ds) { ui.toast("There is no dataset to configure.", "bad"); return; }
    if (!aiVocabulary) {
      try { aiVocabulary = await (await fetch("/$/ui/tts.ttl")).text(); } catch (e) { aiVocabulary = ""; }
    }
    if (!aiMessages.length) {
      aiMessages.push({ role: "system", content: AI_SYSTEM + "\n\nThe tts: vocabulary:\n```turtle\n" + aiVocabulary + "\n```\n\nThe dataset is " + ds
        + ". The workspace's files: " + ([...docs.keys()].join(", ") || "none yet") + "." });
    }
    if (aiMessages.length > 1 && aiMessages[0].content.indexOf("The dataset is " + ds + ".") < 0) aiMessages.push({ role: "user", content: "(The dataset is now " + ds + ".)" });
    aiMessages.push({ role: "user", content: text });
    q.value = "";
    const log = $("#wb-ai-log");
    if (log.querySelector(".wb-ai-intro")) fill(log);
    log.append(el("div", { class: "wb-ai-msg user" }, text));
    const steps = el("div", { class: "wb-ai-steps muted small" }, "Thinking…");
    log.append(steps);
    log.scrollTop = log.scrollHeight;
    aiAbort = new AbortController();
    $("#wb-ai-go").hidden = true; $("#wb-ai-stop").hidden = false;
    const done = [];
    try {
      const answer = await window.TripleTrailAI.converse(c, aiMessages, aiTools(ds), {
        signal: aiAbort.signal,
        onStep: (kind, d) => { done.push(d.name + (d.args && d.args.file ? " " + d.args.file : "")); steps.textContent = "Used: " + done.join(", ") + "…"; },
      });
      steps.textContent = done.length ? "Used: " + done.join(", ") : "";
      log.append(aiAnswer(answer));
    } catch (e) {
      steps.textContent = "";
      log.append(el("div", { class: "wb-ai-msg error" }, e.name === "AbortError" ? "Stopped." : e.message));
      // A failed turn leaves no half-answered tool calls behind.
      while (aiMessages.length && aiMessages[aiMessages.length - 1].role !== "user") aiMessages.pop();
      aiMessages.pop();
    } finally {
      aiAbort = null;
      $("#wb-ai-go").hidden = false; $("#wb-ai-stop").hidden = true;
      log.scrollTop = log.scrollHeight;
    }
  }
  /** The files an answer proposes: a "File: path" line, then a fenced block. */
  function proposedFiles(text) {
    const out = [];
    const re = /(?:^|\n)[*_`\s]*File:?[*_`\s]*([\w./-]+\.\w+)[*_`\s]*\n+```[\w-]*\n([\s\S]*?)```/gi;
    for (const m of text.matchAll(re)) {
      const path = aiPath(m[1]);
      if (path) out.push({ path, text: m[2], at: m.index, end: m.index + m[0].length });
    }
    return out;
  }
  /**
   * Where a proposed file may go (#409): shapes/ and rules/ (.ttl), queries/ (.rq), updates/
   * (.ru), or a test's folder tests/NAME/ (its input, target and expected output). A bare name
   * goes to the folder of its kind (a .ttl to shapes/). Never data/, pipeline.yaml or another
   * folder, and never `.`, `..` or an empty segment, so Apply can't reach outside the workspace
   * (a request URL resolves `..`); the model's text may be steered by the data it read.
   */
  const AI_FOLDERS = { shapes: /\.ttl$/, rules: /\.ttl$/, queries: /\.rq$/, updates: /\.ru$/ };
  function aiPath(name) {
    const plain = (seg) => /^[\w-][\w.-]*$/.test(seg) && seg !== "." && seg !== "..";
    const segs = String(name).replace(/^\/+/, "").split("/");
    if (!segs.every(plain)) return null;
    const last = segs[segs.length - 1];
    if (segs.length === 2 && AI_FOLDERS[segs[0]] && AI_FOLDERS[segs[0]].test(last)) return segs.join("/");
    if (segs.length === 3 && segs[0] === "tests" && /\.(ttl|nt|trig|nq|rq|ru|srj|srx|tsv|csv|yaml)$/.test(last)) return segs.join("/");
    if (segs.length !== 1) return null;
    const folder = Object.keys(AI_FOLDERS).find((f) => f !== "rules" && AI_FOLDERS[f].test(last));
    return folder ? folder + "/" + last : null;
  }
  function aiAnswer(text) {
    const box = el("div", { class: "wb-ai-msg assistant" });
    const files = proposedFiles(text);
    let at = 0;
    const prose = (s) => s.split(/```[\s\S]*?```/).join("").trim().split(/\n{2,}/).filter(Boolean).map((p) => el("p", {}, p.replace(/\*\*([^*]+)\*\*/g, "$1")));
    for (const f of files) {
      box.append(...prose(text.slice(at, f.at)));
      box.append(aiFile(f));
      at = f.end;
    }
    box.append(...prose(text.slice(at)));
    return box;
  }
  /** A proposed file: its diff against the workspace's, and Apply. */
  function aiFile(f) {
    const before = fileText(f.path);
    const diff = lineDiff(before || "", f.text);
    const added = diff.filter((d) => d[0] === "+").length, removed = diff.filter((d) => d[0] === "-").length;
    const pre = el("pre", { class: "wb-ai-diff" }, diff.map(([k, line]) => el("span", { class: k === "+" ? "add" : k === "-" ? "del" : "" }, k + " " + line + "\n")));
    const apply = el("button", { class: "btn small primary", type: "button" }, before == null ? "Add " + f.path : "Apply to " + f.path);
    apply.addEventListener("click", async () => {
      apply.disabled = true;
      try { await applyFile(f.path, f.text); apply.textContent = "Applied"; } catch (e) { apply.disabled = false; ui.fail(e); }
    });
    const look = /^shapes\//.test(f.path) ? el("button", { class: "btn small", type: "button", title: "Draw the dataset's map as if this file were applied and promoted, without saving anything",
      onclick: () => previewOnMap($("#wb-ai-ds").value, f).catch((e) => ui.fail(e)) }, "Preview on map") : null;
    return el("div", { class: "wb-ai-file" },
      el("div", { class: "row" }, el("code", {}, f.path), el("span", { class: "muted small" }, before == null ? "new file" : `+${added} −${removed} lines`), el("span", { class: "grow" }), look, apply),
      pre);
  }
  /**
   * Opens dataset `ds`'s map, in a new tab, drawn with the workspace's shape files (with
   * `extra`, a proposed file, in place of its namesake) as if they were promoted: the server
   * keeps them an hour for the map (POST /{ds}/tiles/preview) and saves nothing.
   */
  async function previewOnMap(ds, extra) {
    if (!ds) throw new Error("Pick a dataset to preview");
    const files = new Map();
    for (const p of docs.keys()) if (/^shapes\/.+\.ttl$/.test(p)) files.set(p, fileText(p));
    if (extra) files.set(extra.path, extra.text);
    if (!files.size) throw new Error("This workspace has no shape files (shapes/*.ttl) to preview");
    // Opened before the request, so that a popup blocker sees the click.
    const tab = window.open("about:blank", "_blank");
    try {
      const body = JSON.stringify({ files: [...files].map(([path, text]) => ({ name: path.slice("shapes/".length), text })) });
      const r = await json("POST", "/" + encodeURIComponent(ds) + "/tiles/preview", { body, type: "application/json" });
      const url = "/$/ui/map/" + encodeURIComponent(ds) + "?preview=" + encodeURIComponent(r.id);
      if (tab) tab.location = url; else location.href = url;
    } catch (e) {
      if (tab) tab.close();
      throw e;
    }
  }
  async function applyFile(path, text) {
    if (aiPath(path) !== path) throw new Error("Not a file the assistant may write: " + path);
    if (!docs.has(path)) {
      await request("PUT", base() + "/files/" + path, { body: text, type: mediaType(path) });
      docs.set(path, { text, state: null, dirty: false, error: null, timer: null });
      await refreshWorkspace();
      openFile(path, { stay: true });
      return;
    }
    openFile(path, { stay: true });
    // One change in the editor, so Ctrl+Z takes it back.
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
    edited(path);
    await save(path, true);
  }
  /** A line diff (longest common subsequence), with unchanged runs cut to 2 lines of context. */
  function lineDiff(a, b) {
    const x = a.split("\n"), y = b.split("\n");
    if (x.length * y.length > 4e6) return [...x.map((l) => ["-", l]), ...y.map((l) => ["+", l])];
    const n = x.length, m = y.length;
    const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) lcs[i][j] = x[i] === y[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    const out = [];
    let i = 0, j = 0;
    while (i < n || j < m) {
      if (i < n && j < m && x[i] === y[j]) { out.push([" ", x[i]]); i++; j++; }
      else if (j < m && (i === n || lcs[i][j + 1] >= lcs[i + 1][j])) out.push(["+", y[j++]]);
      else out.push(["-", x[i++]]);
    }
    if (!a) return out;
    const keep = out.map((d, k) => d[0] !== " " || out.slice(Math.max(0, k - 2), k + 3).some((e) => e[0] !== " "));
    const cut = [];
    out.forEach((d, k) => { if (keep[k]) cut.push(d); else if (keep[k - 1] !== false || k === 0) cut.push([" ", "…"]); });
    return cut;
  }

  // ---------- entry ----------
  function render(api, args) {
    ui = api;
    const root = $("#page-workbench");
    const [id, ...path] = args;
    if (!id) renderList(root);
    else openWorkspace(root, decodeURIComponent(id), path.length ? path.map(decodeURIComponent).join("/") : null);
  }
  function leave() {
    // Keep the workspace open for a return visit; save what is being typed.
    if (wsId) saveAll();
  }
  return { render, leave, EXAMPLES };
})();
