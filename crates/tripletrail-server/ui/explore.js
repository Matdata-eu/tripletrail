// The Explore page (#226): start at a resource and walk out from it, as a node-link graph.
// Plain JavaScript like app.js, which calls TripleTrailExplore.render(ui, args). No library: the
// graph is SVG, laid out by a small force simulation.
//
// /$/ui/explore/{dataset}?iri=IRI[&open=IRI,…][&groups=IRI|DIR|PREDICATE|PAGES,…][&hide=IRI,…]
//
// Everything is read through the dataset's SPARQL endpoint with the caller's credentials, so
// access control applies as it does to any query. A node's links are grouped by predicate
// and direction: a group of at most INLINE neighbours is drawn at once, a larger one as one
// group node with its count, which opens a page of PAGE neighbours at a time. The link holds
// the start, the opened nodes and groups and the hidden nodes, so it reopens the same view.
"use strict";

window.TripleTrailExplore = (function () {
  const RDF_TYPE = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
  const LABELS = [
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://schema.org/name",
    "https://schema.org/name",
    "http://purl.org/dc/terms/title",
  ];
  /** Neighbours of a group drawn at once, without a group node. */
  const INLINE = 8;
  /** Neighbours a group node opens per page. */
  const PAGE = 50;
  /** Nodes on the page at most: expanding past it asks to hide or collapse first. */
  const MAX_NODES = 1500;
  /** Literals listed in the side panel. */
  const MAX_LITERALS = 200;
  /** The longest path the path search looks for, in links. */
  const MAX_PATH = 8;
  /** Colours of the classes, by first class (Tableau 10). */
  const COLOURS = ["#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7", "#9c755f", "#bab0ac"];
  const SVG = "http://www.w3.org/2000/svg";

  let ui = null;
  let state = null;
  const page = () => document.getElementById("page-explore");

  // ---------- SPARQL ----------
  /** The caller's headers: the admin token, and whom to run as ("view as", #205). */
  const headers = () => ui.viewAs.headers();
  /** An IRI as SPARQL, or null when it cannot be written as one. */
  const iriRef = (iri) => (/[\s<>"{}|^`\\]/.test(iri) ? null : "<" + iri + ">");
  async function select(query) {
    const r = await fetch("/" + encodeURIComponent(state.ds) + "/sparql", {
      method: "POST",
      headers: { "Content-Type": "application/sparql-query", Accept: "application/sparql-results+json", ...headers() },
      body: query,
    });
    const text = await r.text();
    if (!r.ok) throw new ui.ApiError(r.status, text.trim() || r.statusText);
    return JSON.parse(text).results.bindings;
  }

  // ---------- names ----------
  /** The last part of an IRI, for a node without a label. */
  function shortName(iri) {
    const tail = iri.replace(/[/#:]+$/, "").replace(/^.*[/#:]/, "");
    try { return decodeURIComponent(tail) || iri; } catch (e) { return tail || iri; }
  }
  const nodeId = (term) => (term.type === "bnode" ? "_:" + term.value : term.value);

  // ---------- state ----------
  /** A fresh view of `ds` from `start`. */
  function fresh(ds, start) {
    return {
      ds, start,
      nodes: new Map(), // id -> node
      paths: [], // [from IRI, to IRI] of the paths shown, in order
      pathEdges: new Set(), // keys of the edges on a shown path
      pathFrom: null, // the node a path is to start from, once "Path from here" is picked
      edges: new Map(), // "from|pred|to" -> edge
      groups: new Map(), // "iri|dir|pred" -> group
      open: new Set(), // expanded IRIs, in order
      openGroups: new Map(), // group key -> pages shown
      hidden: new Set(),
      hiddenPredicates: new Set(),
      hiddenClasses: new Set(),
      selected: null,
      classColour: new Map(),
      view: { x: 0, y: 0, k: 1 },
      ticks: 0,
    };
  }
  function addNode(term, near) {
    const id = nodeId(term);
    let n = state.nodes.get(id);
    if (n) return n;
    const angle = Math.random() * 2 * Math.PI;
    const r = near ? 60 + Math.random() * 40 : 0;
    n = {
      id, iri: term.type === "uri" ? term.value : null, blank: term.type === "bnode",
      label: term.type === "uri" ? shortName(term.value) : "blank node",
      classes: [], x: (near ? near.x : 0) + r * Math.cos(angle), y: (near ? near.y : 0) + r * Math.sin(angle),
      vx: 0, vy: 0, pinned: false,
    };
    state.nodes.set(id, n);
    return n;
  }
  function addEdge(from, pred, to) {
    const key = from.id + "|" + pred + "|" + to.id;
    if (!state.edges.has(key)) state.edges.set(key, { key, from, to, pred });
  }

  /** The links of `node` grouped by predicate and direction, with their counts. */
  async function summary(node) {
    const x = iriRef(node.iri);
    if (!x) return [];
    const rows = await select(
      "SELECT ?p ?dir (COUNT(*) AS ?n) WHERE { { " + x + " ?p ?o FILTER(!isLiteral(?o)) BIND(\"out\" AS ?dir) } UNION { ?s ?p " + x +
      " BIND(\"in\" AS ?dir) } FILTER(?p != <" + RDF_TYPE + ">) } GROUP BY ?p ?dir ORDER BY ?dir ?p");
    return rows.map((b) => ({ pred: b.p.value, dir: b.dir.value, count: Number(b.n.value) }));
  }
  /** Page `k` (from 0) of the neighbours of `node` through `pred` in `dir`. */
  async function neighbours(node, dir, pred, k, size) {
    const x = iriRef(node.iri);
    const p = iriRef(pred);
    if (!x || !p) return [];
    const pattern = dir === "out" ? x + " " + p + " ?o FILTER(!isLiteral(?o))" : "?o " + p + " " + x;
    const rows = await select("SELECT DISTINCT ?o WHERE { " + pattern + " } ORDER BY ?o LIMIT " + size + " OFFSET " + k * size);
    return rows.map((b) => b.o);
  }

  /** Expands `node`: its groups, small ones drawn at once. */
  async function expand(node, quiet) {
    if (!node.iri || state.open.has(node.iri)) return;
    state.open.add(node.iri);
    const groups = await summary(node);
    for (const g of groups) {
      const key = node.iri + "|" + g.dir + "|" + g.pred;
      const group = { key, node, dir: g.dir, pred: g.pred, count: g.count, shown: 0 };
      state.groups.set(key, group);
      if (g.count <= INLINE) await openGroup(group, 1, true);
      else if (state.openGroups.has(key)) await openGroup(group, state.openGroups.get(key), true);
    }
    if (!quiet) { await describeNodes(); restart(); save(); }
  }
  /** Shows `pages` pages of `group`'s neighbours. */
  async function openGroup(group, pages, quiet) {
    const size = group.count <= INLINE ? INLINE : PAGE;
    const want = Math.min(group.count, pages * size);
    if (state.nodes.size + (want - group.shown) > MAX_NODES) {
      ui.toast("At most " + MAX_NODES.toLocaleString() + " nodes: hide or collapse some first.", "bad");
      return;
    }
    for (let k = Math.floor(group.shown / size); k * size < want; k++) {
      for (const term of await neighbours(group.node, group.dir, group.pred, k, size)) {
        const other = addNode(term, group.node);
        if (group.dir === "out") addEdge(group.node, group.pred, other);
        else addEdge(other, group.pred, group.node);
      }
    }
    group.shown = want;
    if (group.count > INLINE) state.openGroups.set(group.key, Math.ceil(want / size));
    if (!quiet) { await describeNodes(); restart(); save(); }
  }
  /** Collapses `node`: the nodes only it led to go, with their own expansions. */
  function collapse(node) {
    if (!node.iri) return;
    state.open.delete(node.iri);
    for (const [key, g] of state.groups) if (g.node === node) { state.groups.delete(key); state.openGroups.delete(key); }
    prune();
    restart(); save();
  }
  /** Drops the edges of groups that are no longer open, and the nodes nothing reaches. */
  function prune() {
    const startId = state.start;
    for (const [key, e] of state.edges) {
      const owner = (n, dir) => n.iri && state.open.has(n.iri) && state.groups.has(n.iri + "|" + dir + "|" + e.pred) &&
        state.groups.get(n.iri + "|" + dir + "|" + e.pred).shown > 0;
      if (!owner(e.from, "out") && !owner(e.to, "in") && !state.pathEdges.has(key)) state.edges.delete(key);
    }
    // What the start reaches over the remaining edges.
    const reach = new Set([startId]);
    let grown = true;
    while (grown) {
      grown = false;
      for (const e of state.edges.values()) {
        if (reach.has(e.from.id) !== reach.has(e.to.id)) { reach.add(e.from.id); reach.add(e.to.id); grown = true; }
      }
    }
    for (const id of [...state.nodes.keys()]) {
      if (!reach.has(id)) {
        const n = state.nodes.get(id);
        state.nodes.delete(id);
        if (n.iri) state.open.delete(n.iri);
      }
    }
    for (const [key, e] of state.edges) if (!state.nodes.has(e.from.id) || !state.nodes.has(e.to.id)) state.edges.delete(key);
    for (const [key, g] of state.groups) if (!state.nodes.has(g.node.id)) { state.groups.delete(key); state.openGroups.delete(key); }
  }

  /** Labels and classes of the nodes that have none yet. */
  async function describeNodes() {
    const todo = [...state.nodes.values()].filter((n) => n.iri && !n.described && iriRef(n.iri));
    for (let i = 0; i < todo.length; i += 200) {
      const part = todo.slice(i, i + 200);
      const values = "VALUES ?s { " + part.map((n) => iriRef(n.iri)).join(" ") + " }";
      const [labels, types] = await Promise.all([
        select("SELECT ?s (SAMPLE(?l) AS ?label) WHERE { " + values + " VALUES ?p { " + LABELS.map((p) => "<" + p + ">").join(" ") +
          " } ?s ?p ?l } GROUP BY ?s"),
        select("SELECT ?s ?c WHERE { " + values + " ?s <" + RDF_TYPE + "> ?c FILTER(isIRI(?c)) } ORDER BY ?s ?c"),
      ]);
      const byIri = new Map(part.map((n) => [n.iri, n]));
      for (const b of labels) if (b.label && byIri.has(b.s.value)) byIri.get(b.s.value).label = b.label.value;
      for (const b of types) if (byIri.has(b.s.value)) byIri.get(b.s.value).classes.push(b.c.value);
      for (const n of part) n.described = true;
    }
  }
  /**
   * Adds a shortest path from `a` to `b` (IRIs), over links of any predicate followed either
   * way (TripleTrail's path search, at most MAX_PATH links), leaving out rdf:type, hidden
   * predicates and blank nodes. Answers whether there was one.
   */
  async function addPath(a, b, quiet) {
    const [x, y] = [iriRef(a), iriRef(b)];
    if (!x || !y || a === b) return false;
    const hidden = [...state.hiddenPredicates].map(iriRef).filter(Boolean);
    // Over every predicate but these, searched on the indexes from both ends (#819).
    const except = ["<" + RDF_TYPE + ">", ...hidden].join(" ");
    const rows = await select("PREFIX path: <http://w3id.org/tripletrail/path#> SELECT ?step ?from ?edge ?to WHERE { SERVICE <http://w3id.org/tripletrail/path> { " +
      "[] path:source " + x + " ; path:target " + y + " ; path:exceptPredicate (" + except + ") ; path:direction \"both\" ; path:maxLength " + MAX_PATH +
      " ; path:step ?step ; path:from ?from ; path:edge ?edge ; path:to ?to } } ORDER BY ?step");
    if (!rows.length) return false;
    // The search walks links either way: which way each one goes in the data.
    const steps = rows.map((r) => [r.from.value, r.edge.value, r.to.value]);
    if (steps.some((st) => st.some((iri) => !iriRef(iri)))) return false;
    const both = steps.flatMap(([f, p, t]) => ["(" + iriRef(f) + " " + iriRef(p) + " " + iriRef(t) + ")", "(" + iriRef(t) + " " + iriRef(p) + " " + iriRef(f) + ")"]);
    const found = await select("SELECT ?s ?p ?o WHERE { VALUES (?s ?p ?o) { " + both.join(" ") + " } ?s ?p ?o }");
    const there = new Set(found.map((r) => r.s.value + "|" + r.p.value + "|" + r.o.value));
    let prev = state.nodes.get(a) || addNode({ type: "uri", value: a });
    for (const [f, p, t] of steps) {
      const next = addNode({ type: "uri", value: t }, prev);
      const [s, o] = there.has(f + "|" + p + "|" + t) ? [prev, next] : [next, prev];
      addEdge(s, p, o);
      state.pathEdges.add(s.id + "|" + p + "|" + o.id);
      prev = next;
    }
    if (!state.paths.some(([f, t]) => f === a && t === b)) state.paths.push([a, b]);
    if (!quiet) { await describeNodes(); restart(); save(); }
    return true;
  }
  function clearPaths() {
    state.paths = [];
    state.pathEdges.clear();
    prune();
    restart(); save();
    const n = state.nodes.get(state.selected);
    select_(n || null);
  }
  const colourOf = (n) => {
    const c = n.classes[0];
    if (!c) return "var(--muted)";
    if (!state.classColour.has(c)) state.classColour.set(c, COLOURS[state.classColour.size % COLOURS.length]);
    return state.classColour.get(c);
  };

  // ---------- what is shown ----------
  function visibleNode(n) {
    if (state.hidden.has(n.id)) return false;
    if (n.classes.length && n.classes.every((c) => state.hiddenClasses.has(c))) return false;
    return true;
  }
  function visibleEdges() {
    return [...state.edges.values()].filter((e) => !state.hiddenPredicates.has(e.pred) && visibleNode(e.from) && visibleNode(e.to));
  }
  /** The visible nodes: the start, and those a visible edge reaches. */
  function visibleNodes(edges) {
    const ids = new Set([state.start]);
    for (const e of edges) { ids.add(e.from.id); ids.add(e.to.id); }
    return [...state.nodes.values()].filter((n) => ids.has(n.id) && visibleNode(n));
  }
  /** The group nodes: groups too large to draw at once, not fully shown yet. */
  function visibleGroups() {
    return [...state.groups.values()].filter((g) => g.count > INLINE && g.shown < g.count && !state.hiddenPredicates.has(g.pred) &&
      visibleNode(g.node));
  }

  // ---------- layout ----------
  /** Gives new group boxes a place next to their node. */
  function placeGroups(groups) {
    for (const g of groups) {
      if (g.x == null) {
        const a = Math.random() * 2 * Math.PI;
        Object.assign(g, { x: g.node.x + 80 * Math.cos(a), y: g.node.y + 80 * Math.sin(a), vx: 0, vy: 0 });
      }
    }
    return groups;
  }

  let frame = null;
  /** Whether the user moved or zoomed the view since the layout last started. */
  let userView = false;
  function restart() {
    state.ticks = 0;
    userView = false;
    if (!frame) frame = requestAnimationFrame(tick);
    draw();
    updateMap();
  }
  function tick() {
    frame = null;
    if (!state) return;
    const edges = visibleEdges();
    const nodes = visibleNodes(edges);
    const groups = placeGroups(visibleGroups());
    const bodies = [...nodes, ...groups];
    const alpha = Math.max(0.02, 1 - state.ticks / 300);
    // Repulsion between every pair (a few hundred bodies: fine).
    for (let i = 0; i < bodies.length; i++) {
      const a = bodies[i];
      for (let j = i + 1; j < bodies.length; j++) {
        const b = bodies[j];
        let dx = a.x - b.x, dy = a.y - b.y;
        let d2 = dx * dx + dy * dy;
        if (d2 < 1) { dx = Math.random() - 0.5; dy = Math.random() - 0.5; d2 = 1; }
        if (d2 > 250000) continue;
        const f = (1600 / d2) * alpha;
        a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
      }
    }
    const spring = (a, b, len) => {
      const dx = b.x - a.x, dy = b.y - a.y;
      const d = Math.sqrt(dx * dx + dy * dy) || 1;
      const f = ((d - len) / d) * 0.06 * alpha;
      a.vx += dx * f; a.vy += dy * f; b.vx -= dx * f; b.vy -= dy * f;
    };
    for (const e of edges) spring(e.from, e.to, 90);
    for (const g of groups) spring(g.node, g, 70);
    for (const b of bodies) {
      b.vx -= b.x * 0.002 * alpha; b.vy -= b.y * 0.002 * alpha;
      if (b.pinned || b === dragging) { b.vx = 0; b.vy = 0; continue; }
      b.vx *= 0.6; b.vy *= 0.6;
      b.x += b.vx; b.y += b.vy;
    }
    state.ticks++;
    // While the layout settles, the view follows it unless the user took over.
    if (!userView && (state.ticks % 30 === 0 || state.ticks === 300)) fit(true);
    draw();
    if (state.ticks < 300) frame = requestAnimationFrame(tick);
  }

  /** Fits the view to what is shown (at most 1.5 times), drawing it unless `quiet`. */
  function fit(quiet) {
    if (!svg || !state) return;
    const edges = visibleEdges();
    const bodies = [...visibleNodes(edges), ...visibleGroups()].filter((b) => b.x != null);
    if (!bodies.length) return;
    const r = svg.getBoundingClientRect();
    if (!r.width || !r.height) return;
    const xs = bodies.map((b) => b.x), ys = bodies.map((b) => b.y);
    const [x0, x1, y0, y1] = [Math.min(...xs) - 110, Math.max(...xs) + 160, Math.min(...ys) - 30, Math.max(...ys) + 30];
    const k = Math.min(1.5, r.width / (x1 - x0), r.height / (y1 - y0));
    state.view = { k, x: r.width / 2 - ((x0 + x1) / 2) * k, y: r.height / 2 - ((y0 + y1) / 2) * k };
    if (!quiet) draw();
  }

  // ---------- drawing ----------
  let svg = null, layer = null, dragging = null;
  const svgEl = (tag, attrs = {}) => {
    const e = document.createElementNS(SVG, tag);
    for (const [k, v] of Object.entries(attrs)) if (v != null) e.setAttribute(k, v);
    return e;
  };
  const predName = (p) => shortName(p);
  function draw() {
    if (!svg || !state) return;
    const v = state.view;
    layer.setAttribute("transform", "translate(" + v.x + "," + v.y + ") scale(" + v.k + ")");
    // Zoomed out, labels keep about 9 px on screen; edge labels go, and so do all below 0.2.
    layer.style.setProperty("--ex-label", Math.max(11, 9 / v.k).toFixed(1) + "px");
    layer.classList.toggle("ex-far", v.k < 0.6);
    layer.classList.toggle("ex-very-far", v.k < 0.2);
    const edges = visibleEdges();
    const nodes = visibleNodes(edges);
    const groups = visibleGroups();
    const out = [];
    for (const e of edges) {
      const line = svgEl("line", { x1: e.from.x, y1: e.from.y, x2: e.to.x, y2: e.to.y, class: "ex-edge" + (state.pathEdges.has(e.key) ? " ex-path" : ""), "marker-end": "url(#ex-arrow)" });
      const title = svgEl("title");
      title.textContent = predName(e.pred) + " (" + e.pred + ")";
      line.append(title);
      out.push(line);
      if (edges.length <= 150) {
        const t = svgEl("text", { x: (e.from.x + e.to.x) / 2, y: (e.from.y + e.to.y) / 2 - 3, class: "ex-edge-label" });
        t.textContent = predName(e.pred);
        out.push(t);
      }
    }
    for (const g of placeGroups(groups)) {
      out.push(svgEl("line", { x1: g.node.x, y1: g.node.y, x2: g.x, y2: g.y, class: "ex-edge ex-group-edge" }));
      const box = svgEl("g", { class: "ex-group", transform: "translate(" + g.x + "," + g.y + ")", tabindex: "0", role: "button" });
      box.dataset.group = g.key;
      const text = (g.dir === "in" ? "← " : "") + predName(g.pred) + " (" + (g.count - g.shown).toLocaleString() + (g.shown ? " more" : "") + ")";
      const f = Math.max(1, 9 / 11 / state.view.k); // the label's scale when zoomed out
      box.append(svgEl("rect", { x: (-4 - text.length * 3.3) * f, y: -11 * f, width: (8 + text.length * 6.6) * f, height: 22 * f, rx: 11 * f }));
      const t = svgEl("text", { y: 4 * f, "text-anchor": "middle" });
      t.textContent = text;
      box.append(t);
      const title = svgEl("title");
      title.textContent = "Show " + Math.min(PAGE, g.count - g.shown) + " more of " + g.pred + (g.dir === "in" ? " (incoming)" : "");
      box.append(title);
      out.push(box);
    }
    for (const n of nodes) {
      const open = n.iri && state.open.has(n.iri);
      const g = svgEl("g", {
        class: "ex-node" + (n.id === state.selected ? " selected" : "") + (n.pinned ? " pinned" : "") + (n.id === state.start ? " start" : ""),
        transform: "translate(" + n.x + "," + n.y + ")", tabindex: "0", role: "button",
      });
      g.dataset.node = n.id;
      g.append(svgEl("circle", { r: n.id === state.start ? 11 : 8, fill: colourOf(n), "stroke-dasharray": n.blank ? "2 2" : null }));
      if (n.iri && !open) g.append(svgEl("circle", { r: 3, class: "ex-more" }));
      const t = svgEl("text", { x: 13, y: 4 });
      t.textContent = n.label.length > 40 ? n.label.slice(0, 39) + "…" : n.label;
      g.append(t);
      const title = svgEl("title");
      title.textContent = (n.iri || n.label) + (n.classes.length ? "\n" + n.classes.map(shortName).join(", ") : "");
      g.append(title);
      out.push(g);
    }
    layer.replaceChildren(...out);
    const status = page().querySelector(".ex-status");
    if (status) status.textContent = nodes.length.toLocaleString() + " nodes, " + edges.length.toLocaleString() + " links shown";
  }

  // ---------- link ----------
  function save() {
    if (!state) return;
    const q = new URLSearchParams({ iri: state.start });
    const opened = [...state.open].filter((i) => i !== state.start);
    if (opened.length) q.set("open", opened.join(" "));
    const groups = [...state.openGroups].map(([key, pages]) => key + "|" + pages);
    if (groups.length) q.set("groups", groups.join(" "));
    if (state.hidden.size) q.set("hide", [...state.hidden].join(" "));
    if (state.hiddenPredicates.size) q.set("hidePredicates", [...state.hiddenPredicates].join(" "));
    if (state.hiddenClasses.size) q.set("hideClasses", [...state.hiddenClasses].join(" "));
    if (state.showMap) q.set("map", "1");
    if (state.paths.length) q.set("paths", state.paths.map(([a, b]) => a + "|" + b).join(" "));
    const url = "/$/ui/explore/" + encodeURIComponent(state.ds) + "?" + q;
    if (url !== location.pathname + location.search) history.replaceState(null, "", url);
  }
  const words = (q, k) => (q.get(k) || "").split(" ").filter(Boolean);

  /** Opens what the link holds: the start, then the opened nodes in order, then the groups. */
  async function restore(q) {
    const start = addNode({ type: "uri", value: state.start });
    start.x = 0; start.y = 0;
    for (const [key, pages] of words(q, "groups").map((g) => { const i = g.lastIndexOf("|"); return [g.slice(0, i), Number(g.slice(i + 1)) || 1]; })) {
      state.openGroups.set(key, pages);
    }
    for (const id of words(q, "hide")) state.hidden.add(id);
    for (const p of words(q, "hidePredicates")) state.hiddenPredicates.add(p);
    for (const c of words(q, "hideClasses")) state.hiddenClasses.add(c);
    await expand(start, true);
    for (const iri of words(q, "open")) {
      const n = state.nodes.get(iri);
      if (n) await expand(n, true);
    }
    for (const p of words(q, "paths").slice(0, 10)) {
      const i = p.indexOf("|");
      if (i > 0) await addPath(p.slice(0, i), p.slice(i + 1), true);
    }
    await describeNodes();
    select_(start);
    restart();
    save();
  }

  // ---------- side panel ----------
  async function select_(n) {
    state.selected = n ? n.id : null;
    draw();
    updateMap();
    if (n && n.geometry !== undefined) followSelection(n);
    const panel = page().querySelector(".ex-panel");
    if (!n) { panel.replaceChildren(ui.el("p", { class: "muted" }, "Select a node.")); return; }
    const { el } = ui;
    const head = [
      el("h3", {}, n.label),
      n.iri ? el("p", { class: "mono small ex-iri" }, n.iri) : el("p", { class: "muted small" }, "A blank node: it can be shown, not expanded."),
      n.classes.length ? el("p", { class: "small" }, "Type: ", ...n.classes.flatMap((c, i) => [i ? ", " : "", el("span", { title: c }, shortName(c))])) : null,
      el("div", { class: "toolbar" },
        n.iri ? (state.open.has(n.iri)
          ? el("button", { class: "btn small", onclick: () => collapse(n) }, "Collapse")
          : el("button", { class: "btn small", onclick: () => expand(n).then(() => select_(n)).catch(ui.fail) }, "Expand")) : null,
        el("button", { class: "btn small", onclick: () => { n.pinned = !n.pinned; select_(n); restart(); } }, n.pinned ? "Unpin" : "Pin"),
        n.id !== state.start ? el("button", { class: "btn small", onclick: () => { state.hidden.add(n.id); select_(null); restart(); save(); } }, "Hide") : null,
        n.iri ? el("a", { class: "btn small", href: "/" + encodeURIComponent(state.ds) + "/describe?iri=" + encodeURIComponent(n.iri), target: "_blank", rel: "noopener" }, "Open page") : null,
        n.iri && n.id !== state.start ? el("button", { class: "btn small", onclick: () => ui.go("explore/" + encodeURIComponent(state.ds) + "?iri=" + encodeURIComponent(n.iri)) }, "Start here") : null,
        n.iri ? el("button", { class: "btn small", title: "Then select another node to find the shortest path between them", onclick: () => { state.pathFrom = n.id; select_(n); } },
          state.pathFrom === n.id ? "Path from here: select another node" : "Path from here") : null,
        n.iri && state.pathFrom && state.pathFrom !== n.id && state.nodes.has(state.pathFrom) ? el("button", { class: "btn small primary", onclick: async () => {
          const from = state.nodes.get(state.pathFrom);
          try {
            if (await addPath(from.iri, n.iri)) { state.pathFrom = null; select_(n); }
            else ui.toast("No path of at most " + MAX_PATH + " links between " + from.label + " and " + n.label + " (rdf:type, hidden links and blank nodes left out).");
          } catch (e) { ui.fail(e); }
        } }, "Shortest path from " + state.nodes.get(state.pathFrom).label) : null,
        state.paths.length ? el("button", { class: "btn small", onclick: clearPaths }, "Clear paths") : null),
      n.iri && state.pathFrom === n.id ? el("form", { class: "toolbar ex-path-to", onsubmit: async (ev) => {
        ev.preventDefault();
        const to = ev.target.elements.to.value.trim();
        if (!to) return;
        try {
          if (await addPath(n.iri, to)) { state.pathFrom = null; const t = state.nodes.get(to); select_(t || n); }
          else ui.toast("No path of at most " + MAX_PATH + " links from " + n.label + " to " + to + " (rdf:type, hidden links and blank nodes left out).");
        } catch (e) { ui.fail(e); }
      } }, el("input", { name: "to", class: "small", placeholder: "or to the IRI…", "aria-label": "Path to the IRI" }), el("button", { class: "btn small", type: "submit" }, "Find")) : null,
    ];
    const show = (...parts) => panel.replaceChildren(...parts.flat().filter((x) => x != null));
    show(head, el("p", { class: "muted small" }, "Loading values…"));
    if (!n.iri || !iriRef(n.iri)) { show(head); return; }
    try {
      const rows = await select("SELECT ?p ?o WHERE { " + iriRef(n.iri) + " ?p ?o FILTER(isLiteral(?o)) } ORDER BY ?p ?o LIMIT " + (MAX_LITERALS + 1));
      if (state.selected !== n.id) return;
      const table = el("dl", { class: "kv ex-values" });
      for (const b of rows.slice(0, MAX_LITERALS)) {
        table.append(el("dt", { title: b.p.value }, predName(b.p.value)), el("dd", {}, b.o.value + (b.o["xml:lang"] ? " @" + b.o["xml:lang"] : "")));
      }
      const groups = [...state.groups.values()].filter((g) => g.node === n);
      const links = groups.length ? el("table", { class: "ex-groups" },
        ...groups.map((g) => el("tr", {},
          el("td", { title: g.pred }, (g.dir === "in" ? "← " : "") + predName(g.pred)),
          el("td", { class: "num" }, g.shown.toLocaleString() + " / " + g.count.toLocaleString()),
          el("td", {}, g.shown < g.count ? el("button", { class: "btn small", onclick: () => openGroup(g, (state.openGroups.get(g.key) || 0) + 1).then(() => select_(n)).catch(ui.fail) }, "More") : null)))) : null;
      show(head,
        rows.length ? el("h3", {}, "Values") : null, rows.length ? table : null,
        rows.length > MAX_LITERALS ? el("p", { class: "muted small" }, "The first " + MAX_LITERALS + "; the page has them all.") : null,
        links ? el("h3", {}, "Links") : null, links);
    } catch (e) { ui.fail(e); }
  }

  // ---------- filters and export ----------
  function filters() {
    const { el } = ui;
    const preds = [...new Set([...state.edges.values()].map((e) => e.pred).concat([...state.groups.values()].map((g) => g.pred)))].sort();
    const classes = [...new Set([...state.nodes.values()].flatMap((n) => n.classes))].sort();
    const box = (set, value) => el("label", { class: "small", title: value },
      el("input", { type: "checkbox", checked: !set.has(value), onchange: (ev) => { if (ev.target.checked) set.delete(value); else set.add(value); restart(); save(); } }),
      " ", shortName(value));
    const dialog = el("dialog", { class: "ex-filters" },
      el("h3", {}, "Links"), el("div", { class: "ex-checks" }, ...preds.map((p) => box(state.hiddenPredicates, p))),
      el("h3", {}, "Classes"), classes.length ? el("div", { class: "ex-checks" }, ...classes.map((c) => box(state.hiddenClasses, c))) : el("p", { class: "muted small" }, "No typed nodes yet."),
      state.hidden.size ? el("p", {}, el("button", { class: "btn small", onclick: () => { state.hidden.clear(); restart(); save(); } }, "Show the " + state.hidden.size + " hidden nodes")) : null,
      el("div", { class: "toolbar" }, el("button", { class: "btn", onclick: () => { dialog.close(); dialog.remove(); } }, "Close")));
    document.body.append(dialog);
    dialog.showModal();
  }
  /** The CONSTRUCT query of the links shown: the triples, as long as the data still has them. */
  function constructQuery() {
    const rows = visibleEdges().map((e) => [e.from, e.pred, e.to]).filter(([a, p, b]) => a.iri && b.iri && iriRef(a.iri) && iriRef(p) && iriRef(b.iri));
    const values = rows.map(([a, p, b]) => "    (" + iriRef(a.iri) + " " + iriRef(p) + " " + iriRef(b.iri) + ")").join("\n");
    return "# The links shown on the Explore page of " + state.ds + ", from " + state.start + "\n" +
      "CONSTRUCT { ?s ?p ?o }\nWHERE {\n  VALUES (?s ?p ?o) {\n" + values + "\n  }\n  ?s ?p ?o\n}\n";
  }
  function download(name, type, text) {
    const a = ui.el("a", { href: URL.createObjectURL(new Blob([text], { type })), download: name });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }
  function exportSvg() {
    const copy = svg.cloneNode(true);
    const css = getComputedStyle(page());
    const colour = (v) => css.getPropertyValue(v).trim() || "#888";
    const style = document.createElementNS(SVG, "style");
    style.textContent = ".ex-edge{stroke:" + colour("--muted") + ";stroke-width:1}.ex-edge-label,.ex-node text,.ex-group text{font:11px sans-serif;fill:" + colour("--text") +
      "}.ex-edge-label{fill:" + colour("--muted") + ";font-size:9px}.ex-group rect{fill:" + colour("--surface-2") + ";stroke:" + colour("--border") + "}.ex-more{fill:#fff}.ex-node circle{stroke:#fff;stroke-width:1.5}";
    copy.prepend(style);
    copy.setAttribute("xmlns", SVG);
    download("explore.svg", "image/svg+xml", new XMLSerializer().serializeToString(copy));
  }

  // ---------- page ----------
  function render(shared, args) {
    ui = shared;
    const { el } = ui;
    const ds = args[0] ? decodeURIComponent(args[0]) : null;
    const q = new URLSearchParams(location.search);
    const start = q.get("iri");
    const form = el("form", { class: "toolbar ex-start", onsubmit: (ev) => {
      ev.preventDefault();
      const d = form.querySelector("select").value, i = form.querySelector("input").value.trim();
      if (d && i) ui.go("explore/" + encodeURIComponent(d) + "?iri=" + encodeURIComponent(i));
    } },
    el("select", { "aria-label": "Dataset" }),
    el("input", { type: "url", placeholder: "Start at an IRI", value: start || "", "aria-label": "Start IRI", required: true }),
    el("button", { class: "btn primary", type: "submit" }, "Explore"));
    ui.api("GET", "/$/datasets").then((r) => {
      const names = r.datasets.map((d) => d.name).filter((n) => !n.startsWith("$"));
      form.querySelector("select").replaceChildren(...names.map((n) => el("option", { value: n, selected: n === ds }, n)));
    }).catch(() => {});
    if (!ds || !start) {
      state = null;
      page().replaceChildren(el("div", { class: "card" }, el("h1", {}, "Explore"),
        el("p", { class: "muted" }, "Start at a resource and walk out from it: pick a dataset and give an IRI, or use Explore on a search result or a map feature."), form));
      return;
    }
    if (state && state.ds === ds && state.start === start && svg && page().contains(svg)) { save(); return; }
    state = fresh(ds, start);
    if (map) { try { map.remove(); } catch (e) { /* gone */ } }
    map = null; mapReady = null;
    state.showMap = q.get("map") === "1";
    svg = svgEl("svg", { class: "ex-graph", role: "img", "aria-label": "The graph around " + start });
    const defs = svgEl("defs");
    const marker = svgEl("marker", { id: "ex-arrow", viewBox: "0 0 10 10", refX: "18", refY: "5", markerWidth: "6", markerHeight: "6", orient: "auto-start-reverse" });
    marker.append(svgEl("path", { d: "M 0 0 L 10 5 L 0 10 z", class: "ex-arrow" }));
    defs.append(marker);
    layer = svgEl("g");
    svg.append(defs, layer);
    const panel = el("aside", { class: "ex-panel card" });
    page().replaceChildren(
      el("div", { class: "toolbar ex-bar" }, form,
        el("button", { class: "btn", onclick: () => fit(false) }, "Fit"),
        el("button", { class: "btn", onclick: filters }, "Filter"),
        el("button", { class: "btn ex-map-btn", "aria-pressed": "false", onclick: toggleMap, title: "Show the nodes that have a geometry on a map beside the graph" }, "Map"),
        el("button", { class: "btn", onclick: exportSvg }, "SVG"),
        el("button", { class: "btn", onclick: () => download("explore.rq", "application/sparql-query", constructQuery()) }, "CONSTRUCT"),
        el("span", { class: "muted small ex-status" })),
      el("div", { class: "ex-body" }, el("div", { class: "ex-canvas card" }, svg),
        el("div", { class: "ex-map card", hidden: true }), panel),
      el("p", { class: "muted small ex-map-status" }));
    interact();
    const box = svg.getBoundingClientRect();
    state.view = { x: box.width / 2 || 400, y: box.height / 2 || 300, k: 1 };
    if (state.showMap) showMap();
    restore(q).catch((e) => {
      ui.fail(e);
      panel.replaceChildren(el("p", { class: "error" }, e.message || String(e)));
    });
  }

  // ---------- the map beside the graph ----------
  // Nodes with a GeoSPARQL geometry (geo:asWKT, or geo:hasGeometry/geo:asWKT) are also drawn on a
  // map over the dataset's base map; selecting in one selects in the other.
  const GEO = "http://www.opengis.net/ont/geosparql#";
  let map = null, mapReady = null, mapTimer = null;
  function toggleMap() {
    state.showMap = !state.showMap;
    showMap();
    save();
  }
  function showMap() {
    const box = page().querySelector(".ex-map");
    const btn = page().querySelector(".ex-map-btn");
    if (btn) btn.setAttribute("aria-pressed", String(!!state.showMap));
    if (!box) return;
    box.hidden = !state.showMap;
    if (!state.showMap) return;
    if (!map) mapReady = startMap(box).catch((e) => { box.replaceChildren(ui.el("p", { class: "error loading" }, "The map could not start: " + (e.message || e))); });
    else map.resize();
    updateMap();
  }
  async function startMap(box) {
    const VENDOR = "/$/ui/vendor/maplibre/";
    if (!document.querySelector('link[href="' + VENDOR + 'maplibre-gl.css"]')) {
      document.head.append(Object.assign(document.createElement("link"), { rel: "stylesheet", href: VENDOR + "maplibre-gl.css" }));
    }
    const m = await import(VENDOR + "maplibre-gl.mjs");
    const lib = m.Map ? m : m.default;
    // The dataset's map style for its base map only: the graph's nodes are the features here.
    let style;
    try {
      const r = await fetch("/" + encodeURIComponent(state.ds) + "/tiles/style.json", { headers: headers() });
      style = r.ok ? await r.json() : null;
    } catch (e) { style = null; }
    const bg = getComputedStyle(document.body).backgroundColor || "#fff";
    if (style && style.sources) {
      style.layers = style.layers.filter((l) => l.source !== "tripletrail");
      delete style.sources.tripletrail;
      for (const l of style.layers) if (l.type === "background") l.paint = { "background-color": bg };
    } else {
      style = { version: 8, sources: {}, layers: [{ id: "background", type: "background", paint: { "background-color": bg } }] };
    }
    const container = ui.el("div", { class: "ex-map-canvas" });
    box.replaceChildren(container);
    map = new lib.Map({ container, style, center: [4.4, 50.8], zoom: 3, attributionControl: { compact: true } });
    map.addControl(new lib.NavigationControl({ showCompass: false }), "top-right");
    // The box gets its size from the page's layout after the map starts, and again on resizes.
    new ResizeObserver(() => { if (!map) return; map.resize(); if (!map.userMoved && map.shown) fitMap(map.shown); }).observe(container);
    // Until the user moves the map, it follows what the graph shows.
    for (const ev of ["dragstart", "zoomstart", "rotatestart"]) map.on(ev, (e) => { if (e.originalEvent) map.userMoved = true; });
    await new Promise((resolve) => map.on("load", resolve));
    map.addSource("explore", { type: "geojson", data: { type: "FeatureCollection", features: [] } });
    const colour = ["case", ["get", "selected"], "#d6336c", ["get", "colour"]];
    const of = (...types) => ["in", ["geometry-type"], ["literal", types]];
    map.addLayer({ id: "ex-fill", type: "fill", source: "explore", filter: of("Polygon", "MultiPolygon"), paint: { "fill-color": colour, "fill-opacity": 0.2 } });
    map.addLayer({ id: "ex-line", type: "line", source: "explore", filter: of("LineString", "MultiLineString", "Polygon", "MultiPolygon"),
      paint: { "line-color": colour, "line-width": ["case", ["get", "selected"], 5, 3] } });
    map.addLayer({ id: "ex-point", type: "circle", source: "explore", filter: of("Point", "MultiPoint"),
      paint: { "circle-color": colour, "circle-radius": ["case", ["get", "selected"], 8, 6], "circle-stroke-color": "#fff", "circle-stroke-width": 2 } });
    for (const id of ["ex-fill", "ex-line", "ex-point"]) {
      map.on("click", id, (ev) => {
        const n = ev.features[0] && state.nodes.get(ev.features[0].properties.id);
        if (n) select_(n);
      });
      map.on("mouseenter", id, () => { map.getCanvas().style.cursor = "pointer"; });
      map.on("mouseleave", id, () => { map.getCanvas().style.cursor = ""; });
    }
  }
  /** Geometries of the nodes that have none looked up yet, as GeoJSON (the first of each). */
  async function geometries() {
    const todo = [...state.nodes.values()].filter((n) => n.iri && n.geometry === undefined && iriRef(n.iri));
    for (let i = 0; i < todo.length; i += 200) {
      const part = todo.slice(i, i + 200);
      const rows = await select("PREFIX geo: <" + GEO + "> PREFIX geof: <http://www.opengis.net/def/function/geosparql/> " +
        "SELECT ?s (SAMPLE(geof:asGeoJSON(?w)) AS ?g) WHERE { VALUES ?s { " + part.map((n) => iriRef(n.iri)).join(" ") + " } " +
        "{ ?s geo:asWKT ?w } UNION { ?s geo:hasGeometry/geo:asWKT ?w } } GROUP BY ?s");
      for (const n of part) n.geometry = null;
      for (const r of rows) {
        const n = state.nodes.get(r.s.value);
        if (n && r.g) try { n.geometry = JSON.parse(r.g.value); } catch (e) { /* not drawn */ }
      }
    }
  }
  /** Redraws the map's features soon (after a burst of changes, once). */
  function updateMap() {
    if (!state || !state.showMap) return;
    clearTimeout(mapTimer);
    mapTimer = setTimeout(async () => {
      try {
        await mapReady;
        if (!map || !map.getSource("explore")) return;
        await geometries();
        const nodes = visibleNodes(visibleEdges()).filter((n) => n.geometry);
        const features = nodes.map((n) => ({ type: "Feature", geometry: n.geometry,
          properties: { id: n.id, label: n.label, colour: colourOf(n), selected: n.id === state.selected } }));
        map.getSource("explore").setData({ type: "FeatureCollection", features });
        const status = page().querySelector(".ex-map-status");
        if (status) status.textContent = features.length ? "" : "No node shown has a geometry.";
        map.shown = features;
        if (features.length && !map.userMoved) fitMap(features);
      } catch (e) { ui.fail(e); }
    }, 150);
  }
  function fitMap(features) {
    const box = [Infinity, Infinity, -Infinity, -Infinity];
    const walk = (c) => { if (typeof c[0] === "number") { box[0] = Math.min(box[0], c[0]); box[1] = Math.min(box[1], c[1]); box[2] = Math.max(box[2], c[0]); box[3] = Math.max(box[3], c[1]); } else c.forEach(walk); };
    for (const f of features) if (f.geometry && f.geometry.coordinates) walk(f.geometry.coordinates);
    if (box[0] <= box[2]) map.fitBounds([[box[0], box[1]], [box[2], box[3]]], { padding: 40, maxZoom: 15, duration: 0 });
  }
  /** Moves the map to the selected node when it has a geometry off screen. */
  function followSelection(n) {
    if (!map || !state.showMap || !n || !n.geometry) return;
    const c = [];
    const walk = (x) => { if (typeof x[0] === "number") c.push(x); else x.forEach(walk); };
    walk(n.geometry.coordinates || []);
    if (!c.length) return;
    const b = map.getBounds();
    if (!c.some(([x, y]) => b.contains([x, y]))) map.easeTo({ center: c[0], duration: 400 });
  }

  /** Clicks, drags, pan and zoom on the graph. */
  function interact() {
    let pan = null, moved = false, down = null;
    const point = (ev) => {
      const r = svg.getBoundingClientRect();
      const v = state.view;
      return { x: (ev.clientX - r.left - v.x) / v.k, y: (ev.clientY - r.top - v.y) / v.k };
    };
    svg.addEventListener("pointerdown", (ev) => {
      moved = false;
      // Pointer capture makes the svg the target of the click that follows: what was pressed.
      down = ev.target;
      const node = ev.target.closest("[data-node]");
      if (node) { dragging = state.nodes.get(node.dataset.node); svg.setPointerCapture(ev.pointerId); return; }
      if (!ev.target.closest("[data-group]")) { pan = { x: ev.clientX - state.view.x, y: ev.clientY - state.view.y }; svg.setPointerCapture(ev.pointerId); }
    });
    svg.addEventListener("pointermove", (ev) => {
      if (dragging) { const p = point(ev); dragging.x = p.x; dragging.y = p.y; moved = true; draw(); }
      else if (pan) { state.view.x = ev.clientX - pan.x; state.view.y = ev.clientY - pan.y; moved = true; userView = true; draw(); }
    });
    svg.addEventListener("pointerup", () => {
      if (dragging && moved) { dragging.pinned = true; restart(); }
      dragging = null; pan = null;
    });
    svg.addEventListener("wheel", (ev) => {
      ev.preventDefault();
      const r = svg.getBoundingClientRect();
      const v = state.view;
      const k = Math.min(4, Math.max(0.15, v.k * Math.exp(-ev.deltaY * 0.0015)));
      const cx = ev.clientX - r.left, cy = ev.clientY - r.top;
      v.x = cx - ((cx - v.x) * k) / v.k; v.y = cy - ((cy - v.y) * k) / v.k; v.k = k;
      userView = true;
      draw();
    }, { passive: false });
    const activate = (target) => {
      const group = target.closest("[data-group]");
      if (group) { const g = state.groups.get(group.dataset.group); if (g) openGroup(g, (state.openGroups.get(g.key) || 0) + 1).then(() => { const n = state.nodes.get(state.selected); if (n) select_(n); }).catch(ui.fail); return; }
      const node = target.closest("[data-node]");
      if (!node) return;
      const n = state.nodes.get(node.dataset.node);
      if (!n) return;
      select_(n);
      if (n.iri && !state.open.has(n.iri)) expand(n).then(() => { if (state.selected === n.id) select_(n); }).catch(ui.fail);
    };
    svg.addEventListener("click", (ev) => { if (!moved) activate(down && svg.contains(down) ? down : ev.target); down = null; });
    svg.addEventListener("keydown", (ev) => { if (ev.key === "Enter" || ev.key === " ") { ev.preventDefault(); activate(ev.target); } });
  }

  /** Called when another page opens. */
  function leave() {
    if (frame) cancelAnimationFrame(frame);
    frame = null;
  }

  return { render, leave, constructQuery: () => (state ? constructQuery() : null) };
})();
