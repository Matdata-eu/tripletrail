// The Search page (#142): build a question from the dataset's shapes ("Sections where
// maxSpeed > 120 and from is Brussels-South"), without writing SPARQL. Plain JavaScript like
// app.js, which calls TripleTrailSearch.render(ui, args).
//
// /$/ui/search/{dataset}?m={model}   the model, as JSON, so a search is a link to share
//
// The classes and their properties come from /{ds}/search/classes (the `search` reader's
// shapes); values are picked from /{ds}/search/values, with how many resources have each.
// Conditions hold together ("and"); "or" makes a group of which any may hold. The server turns the
// model into SPARQL and runs it (/{ds}/search/results), shown as a table that links each row to
// its describe page; /{ds}/search/sparql gives the query to open on the Query page.
"use strict";

window.TripleTrailSearch = (function () {
  /** Rows a search shows; one more is asked for to know there are more. */
  const LIMIT = 100;
  /** The operators per kind of property, with how each reads. */
  const OPS = {
    text: [["contains", "contains"], ["is", "is"], ["starts", "starts with"], ["exists", "has a value"], ["missing", "has no value"]],
    number: [["eq", "="], ["ne", "≠"], ["lt", "<"], ["le", "≤"], ["gt", ">"], ["ge", "≥"], ["between", "between"], ["exists", "has a value"], ["missing", "has no value"]],
    boolean: [["true", "is yes"], ["false", "is no"], ["exists", "has a value"], ["missing", "has no value"]],
    date: [["before", "before"], ["after", "after"], ["on", "on"], ["exists", "has a value"], ["missing", "has no value"]],
    reference: [["is", "is"], ["exists", "has a value"], ["missing", "has no value"]],
    other: [["exists", "has a value"], ["missing", "has no value"]],
  };
  const NO_VALUE = new Set(["exists", "missing", "true", "false"]);
  let ui = null;
  let shown = null; // { ds, classes }
  let model = null; // { class, where: [{ key, op, value, label } | { any: [condition] }], show: [key] }
  let asked = 0;
  const page = () => document.getElementById("page-search");
  const el = (...a) => ui.el(...a);

  const opsOf = (p) => OPS[p.kind] || OPS.other;
  /** A class by its id, or by its IRI (as the Map page links it). */
  const classOf = (id) => shown.classes.find((c) => c.id === id) || shown.classes.find((c) => c.class === id);
  const propOf = (c, key) => c && c.properties.find((p) => p.key === key);
  const iriOk = (s) => /^[a-z][a-z0-9+.-]*:[^\s<>"{}|^`\\]*$/i.test(s);
  /** The id of proposed shapes the page reads (`?preview=`, from the Shapes page), or null. */
  const previewId = () => { const p = new URLSearchParams(location.search).get("preview"); return p && /^[0-9a-f]{32}$/.test(p) ? p : null; };
  const withPreview = () => (previewId() ? "&preview=" + previewId() : "");
  const searchUrl = (what, extra) => "/" + encodeURIComponent(shown.ds) + "/search/" + what + "?m=" + encodeURIComponent(JSON.stringify(model)) + withPreview() + (extra || "");

  // ---------- the link ----------
  function readModel() {
    try {
      const m = JSON.parse(new URLSearchParams(location.search).get("m") || "null");
      if (m && typeof m.class === "string") {
        const area = Array.isArray(m.area) && m.area.length === 4 && m.area.every(Number.isFinite) ? m.area : undefined;
        return { class: m.class, where: Array.isArray(m.where) ? m.where : [], show: Array.isArray(m.show) ? m.show : [], area };
      }
    } catch (e) { /* a broken link starts afresh */ }
    return null;
  }
  function writeModel() {
    const url = location.pathname + "?m=" + encodeURIComponent(JSON.stringify(model)) + withPreview();
    history.replaceState(null, "", url);
  }

  // ---------- the page ----------
  async function render(shared, args) {
    ui = shared;
    const root = page();
    let datasets = [];
    try { datasets = (await ui.api("GET", "/$/datasets")).datasets.map((d) => d.name); } catch (e) { ui.fail(e); }
    let ds = args[0] ? decodeURIComponent(args[0]) : null;
    if (!ds || !datasets.includes(ds)) ds = shown && datasets.includes(shown.ds) ? shown.ds : datasets[0];
    if (!ds) { root.replaceChildren(el("p", { class: "muted loading" }, "No dataset to search yet.")); return; }
    if (decodeURIComponent((location.pathname.split("/")[4] || "")) !== ds) {
      history.replaceState(null, "", "/$/ui/search/" + encodeURIComponent(ds) + location.search);
    }
    root.replaceChildren(el("p", { class: "muted loading" }, "Loading the classes…"));
    let classes;
    try { classes = (await ui.api("GET", "/" + encodeURIComponent(ds) + "/search/classes", { params: { preview: previewId() } })).classes; }
    catch (e) { root.replaceChildren(el("p", { class: "error loading" }, e.message)); return; }
    shown = { ds, classes };
    const linked = readModel();
    model = linked && classOf(linked.class) ? { ...linked, class: classOf(linked.class).id } : { class: (classes[0] || {}).id, where: [], show: [] };
    const select = el("select", { id: "search-ds", "aria-label": "Dataset", onchange: (e) => ui.go("search/" + encodeURIComponent(e.target.value)) },
      datasets.map((d) => el("option", { value: d, selected: d === ds }, d)));
    root.replaceChildren(
      el("div", { class: "query-bar" }, el("label", {}, "Dataset ", select), el("span", { class: "grow" }),
        previewId() ? el("span", { class: "map-preview", role: "status" }, "Preview of proposed shapes, not saved. ",
          el("a", { href: "/$/ui/search/" + encodeURIComponent(ds), "data-route": "" }, "The dataset's own search")) : null,
        el("span", { class: "muted small" }, "Build a question from the dataset's classes and properties; the link keeps it."),
        el("a", { class: "btn small", href: "/$/ui/workbench?configure=" + encodeURIComponent(ds), "data-route": "", title: "Change which properties are searchable, labels and more, by asking an AI model with your own key, in a new workspace" }, "Configure with AI")),
      el("div", { class: "card sb-card" }, el("div", { class: "sb-builder", id: "sb-builder" }),
        el("div", { class: "sb-actions" },
          el("button", { class: "btn primary", type: "button", id: "sb-run", onclick: run }, "Search"),
          el("button", { class: "btn", type: "button", onclick: copyLink }, "Copy link"),
          el("button", { class: "btn", type: "button", onclick: openInQuery, title: "Open the SPARQL on the Query page" }, "Open in Query"),
          el("span", { class: "muted small", id: "sb-count", role: "status" }))),
      el("div", { class: "card sb-results", id: "sb-results", hidden: true }),
      el("details", { class: "sb-sparql" }, el("summary", { class: "small muted" }, "The SPARQL of the last search"), el("pre", { id: "sb-sparql" })),
      el("div", { class: "card sb-card sb-similar", id: "sb-similar", hidden: true }));
    similar(ds);
    if (!classes.length) { builder(); return; }
    builder();
    if (linked) run();
  }

  /** The sentence: Find [class] where [property] [operator] [value]… and show […]. */
  function builder() {
    const box = document.getElementById("sb-builder");
    const c = classOf(model.class);
    if (!c) { box.replaceChildren(el("p", { class: "muted" }, "This dataset has no classes to search.")); return; }
    const changed = () => { writeModel(); };
    const classSelect = el("select", { "aria-label": "What to find", onchange: (e) => {
      model = { class: e.target.value, where: [], show: [], area: classOf(e.target.value).geometry ? model.area : undefined };
      builder(); changed(); hideResults();
    } },
      shown.classes.map((x) => el("option", { value: x.id, selected: x.id === c.id }, x.title + (x.count != null ? ` (${ui.fmtNum(x.count)})` : ""))));
    const rows = model.where.flatMap((w, i) => {
      const first = i === 0 ? "where" : "and";
      if (!Array.isArray(w.any)) {
        return [condition(c, w, first, i, (x) => { model.where[i] = x; }, () => model.where.splice(i, 1),
          () => { model.where[i] = { any: [w, blank(c, w.key)] }; }, changed)];
      }
      // An "or" group: any of its conditions may hold.
      return w.any.map((a, j) => condition(c, a, j === 0 ? first : "or", i, (x) => { w.any[j] = x; }, () => {
        w.any.splice(j, 1);
        if (w.any.length === 1) model.where[i] = w.any[0];
      }, () => { w.any.splice(j + 1, 0, blank(c, a.key)); }, changed, j > 0));
    });
    const searchable = c.properties;
    const add = el("button", { class: "btn sb-add", type: "button", onclick: () => {
      if (!searchable[0]) return;
      model.where.push(blank(c));
      builder(); changed();
      const last = box.querySelectorAll(".sb-row select.sb-prop");
      if (last.length) last[last.length - 1].focus();
    } }, "+ Condition");
    const shows = el("span", { class: "sb-show" }, (model.show || []).map((key) => {
      const p = propOf(c, key);
      return p ? el("span", { class: "chip" }, p.title, el("button", { class: "sb-x", type: "button", "aria-label": "Don't show " + p.title,
        onclick: () => { model.show = model.show.filter((k) => k !== key); builder(); changed(); } }, "×")) : null;
    }));
    const addShow = el("select", { class: "sb-add-show", "aria-label": "Show a property as a column", onchange: (e) => {
      if (e.target.value) { model.show = [...(model.show || []), e.target.value]; builder(); changed(); }
    } }, el("option", { value: "" }, "+ Column"), c.properties.filter((p) => !(model.show || []).includes(p.key)).map((p) => el("option", { value: p.key }, p.title)));
    // The map area, from the Map page's "Search this area".
    const area = model.area ? el("div", { class: "sb-line sb-row" }, el("span", { class: "sb-word" }, "in"),
      el("span", { class: "chip", title: "West, south, east, north (longitude and latitude)" }, "the map area " + model.area.map((x) => x.toFixed(3)).join(", "),
        el("button", { class: "sb-x", type: "button", "aria-label": "Search everywhere", title: "Search everywhere",
          onclick: () => { model.area = undefined; builder(); changed(); } }, "×"))) : null;
    box.replaceChildren(
      el("div", { class: "sb-line" }, el("span", { class: "sb-word" }, "Find"), classSelect),
      ...(area ? [area] : []), // replaceChildren would write a null as the text "null"
      ...rows,
      el("div", { class: "sb-line" }, add, el("span", { class: "sb-word muted" }, "show"), shows, addShow));
  }

  /** A new condition on property `key` (else the first one). */
  function blank(c, key) {
    const p = propOf(c, key) || c.properties[0];
    return { key: p.key, op: opsOf(p)[0][0], value: "" };
  }

  /** One condition: [property] [operator] [value], with "or" and ×. `at` is the index of its
   *  place in model.where, left out when suggesting its values; `set` replaces it. */
  function condition(c, w, wordText, at, set, remove, or, changed, alt) {
    const p = propOf(c, w.key) || c.properties[0];
    const word = el("span", { class: "sb-word" + (alt ? " sb-or" : "") }, wordText);
    const redo = () => { builder(); changed(); };
    const prop = el("select", { class: "sb-prop", "aria-label": "Property", onchange: (e) => { set(blank(c, e.target.value)); redo(); } },
      c.properties.map((q) => el("option", { value: q.key, selected: q.key === p.key, title: q.description || null }, q.title)));
    const op = el("select", { "aria-label": "Operator", onchange: (e) => { w.op = e.target.value; redo(); } },
      opsOf(p).map(([k, t]) => el("option", { value: k, selected: k === w.op }, t)));
    const orBtn = el("button", { class: "sb-x sb-or-btn", type: "button", title: "Add an alternative: either this or that",
      onclick: () => { or(); redo(); } }, "or");
    const del = el("button", { class: "sb-x", type: "button", "aria-label": "Remove this condition", title: "Remove",
      onclick: () => { remove(); redo(); } }, "×");
    let value = null;
    if (!NO_VALUE.has(w.op)) {
      if (p.kind === "reference" || p.kind === "iri") value = picker(p, w, at, changed, true);
      else if (p.kind === "date") {
        value = el("input", { type: "date", class: "sb-value", value: w.value || "", "aria-label": "Value",
          oninput: (e) => { w.value = e.target.value; changed(); }, onkeydown: (e) => { if (e.key === "Enter") run(); } });
      } else if (w.op === "between") {
        value = el("input", { type: "text", class: "sb-value", value: w.value || "", "aria-label": "Value", placeholder: "10..20",
          oninput: (e) => { w.value = e.target.value; changed(); }, onkeydown: (e) => { if (e.key === "Enter") run(); } });
      } else value = picker(p, w, at, changed, false);
    }
    return el("div", { class: "sb-line sb-row" + (alt ? " sb-alt" : "") }, word, prop, op, value, orBtn, del);
  }

  /** A value, typed or picked from the values the property takes among the resources the
   *  other conditions find, with how many have each (/search/values). For a reference the
   *  pick is its IRI, shown by its label. */
  function picker(p, w, at, changed, ref) {
    const wrap = el("span", { class: "sb-pick" });
    const list = el("div", { class: "top-search-list", role: "listbox", hidden: true });
    const input = el("input", { type: "search", class: "sb-value", value: ref ? (w.label || w.value || "") : (w.value || ""),
      placeholder: ref ? "type a name" : p.kind === "number" ? "number" : "text", "aria-label": "Value", autocomplete: "off",
      inputmode: p.kind === "number" ? "decimal" : null });
    let n = 0;
    let timer = null;
    const choose = (x) => {
      w.value = x.value;
      if (ref) w.label = x.label || undefined;
      input.value = ref ? (x.label || x.value) : x.value;
      list.hidden = true;
      changed();
    };
    const suggest = async () => {
      const q = input.value.trim();
      if (ref && iriOk(q)) { list.hidden = true; return; }
      const mine = ++n;
      // The other complete conditions narrow the list; this one (`at`) is left out.
      const done = (x) => Array.isArray(x.any) ? x.any.every(done) : NO_VALUE.has(x.op) || String(x.value || "").trim() !== "";
      const kept = model.where.map((x, k) => [x, k]).filter(([x, k]) => k === at || done(x));
      const others = { class: model.class, where: kept.map(([x]) => x), show: [], area: model.area };
      const url = "/" + encodeURIComponent(shown.ds) + "/search/values?m=" + encodeURIComponent(JSON.stringify(others)) + withPreview();
      try {
        const r = await ui.api("GET", url, { params: { key: p.key, at: kept.findIndex(([, k]) => k === at), q, limit: 12 } });
        if (mine !== n || document.activeElement !== input) return;
        list.replaceChildren(...r.values.map((x) => el("a", { class: "top-search-item", href: "#", role: "option",
          onmousedown: (e) => e.preventDefault(), onclick: (e) => { e.preventDefault(); choose(x); } },
          el("span", { class: "top-search-label" }, x.label || (ref ? short(x.value) : x.value)),
          el("span", { class: "top-search-class" }, ui.fmtNum(x.count)))));
        if (!r.values.length) list.append(el("div", { class: "top-search-msg" }, "No such value"));
        else if (r.more) list.append(el("div", { class: "top-search-msg" }, "Type to narrow the list"));
        list.hidden = false;
      } catch (e) { list.hidden = true; }
    };
    input.addEventListener("input", () => {
      const q = input.value.trim();
      if (ref) { w.value = iriOk(q) ? q : ""; w.label = undefined; } else w.value = input.value;
      changed();
      clearTimeout(timer);
      timer = setTimeout(suggest, 150);
    });
    input.addEventListener("focus", () => { clearTimeout(timer); timer = setTimeout(suggest, 0); });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Enter") {
        const first = list.querySelector(".top-search-item");
        if (ref && !list.hidden && first && !w.value) { e.preventDefault(); first.click(); } else { list.hidden = true; run(); }
      } else if (e.key === "Escape") list.hidden = true;
    });
    input.addEventListener("blur", () => setTimeout(() => { list.hidden = true; }, 150));
    wrap.append(input, list);
    return wrap;
  }

  function hideResults() {
    const r = document.getElementById("sb-results");
    if (r) r.hidden = true;
    document.getElementById("sb-count").textContent = "";
  }

  async function run() {
    const count = document.getElementById("sb-count");
    const box = document.getElementById("sb-results");
    count.className = "muted small";
    count.textContent = "Searching…";
    const mine = ++asked;
    writeModel();
    try {
      const r = await ui.api("GET", searchUrl("results", "&limit=" + LIMIT));
      if (mine !== asked) return;
      count.textContent = r.count === 1 ? "1 result" : ui.fmtNum(r.count) + " results" + (r.more ? `, the first ${r.rows.length} shown` : "");
      document.getElementById("sb-sparql").textContent = r.sparql;
      table(box, r);
    } catch (e) {
      if (mine !== asked) return;
      count.textContent = e.message;
      count.className = "error small";
      box.hidden = true;
    }
  }

  function table(box, r) {
    // A property with several values shows them all (the first 10, then how many more).
    const cell = (v, col, more) => {
      const vals = v == null ? [] : Array.isArray(v) ? v : [v];
      if (!vals.length) return el("td", { class: "muted" }, "–");
      const one = (x) => x && typeof x === "object"
        ? el("a", { href: x.describe, title: x.iri }, x.label || short(x.iri))
        : typeof x === "boolean" ? (x ? "yes" : "no") : String(x);
      const parts = vals.flatMap((x, i) => (i ? [", ", one(x)] : [one(x)]));
      if (more) parts.push(el("span", { class: "muted small" }, ` and ${ui.fmtNum(more)} more`));
      return el("td", { class: col.kind === "number" && vals.length === 1 ? "num" : null }, parts);
    };
    box.replaceChildren(r.rows.length
      ? el("table", { class: "grid" },
        el("thead", {}, el("tr", {}, el("th", {}, r.title), r.columns.map((c) => el("th", {}, c.title)))),
        el("tbody", {}, r.rows.map((row) => el("tr", {},
          el("td", { class: "name" }, el("a", { href: row.describe, title: row.iri }, row.label || short(row.iri)), ui.copyButton(row.iri),
            el("a", { class: "muted small", href: "/$/ui/explore/" + encodeURIComponent(shown.ds) + "?iri=" + encodeURIComponent(row.iri), "data-route": true,
              title: "Walk the graph from " + (row.label || row.iri) }, "explore")),
          r.columns.map((c) => cell(row.values[c.key], c, row.moreValues && row.moreValues[c.key]))))))
      : el("p", { class: "muted sb-none" }, "Nothing matches."));
    box.hidden = false;
  }
  const short = (iri) => iri.replace(/^.*[/#](?=[^/#]+$)/, "");

  // ---------- Find similar (#219, #236) ----------
  // The resources whose vector (a `tt:vector` property) is nearest a text or a resource, from
  // /{ds}/search/similar. A text is turned into a vector in this browser by the user's own AI
  // service (the one set up for the Workbench's AI tab), so the key never reaches the server.
  const VECTOR = "http://w3id.org/tripletrail/fn#vector";
  const MODELS_KEY = "tripletrail-ui_embedding-models";
  const savedModels = () => { try { return JSON.parse(localStorage.getItem(MODELS_KEY) || "{}") || {}; } catch (e) { return {}; } };
  const saveModel = (id, model) => { try { localStorage.setItem(MODELS_KEY, JSON.stringify({ ...savedModels(), [id]: model })); } catch (e) { /* asked again next time */ } };
  let similarAsked = 0;

  /** The card, shown when the dataset has a property with `tt:vector` values. */
  async function similar(ds) {
    let props = [];
    try {
      const profile = await ui.api("GET", "/$/datasets/" + encodeURIComponent(ds) + "/profile", { params: { sample: 1000 } });
      const seen = new Map();
      for (const c of profile.classes || []) for (const p of c.properties || []) {
        const n = ((p.datatypes || []).find((d) => d.datatype === VECTOR) || {}).count || 0;
        if (n) seen.set(p.property, { iri: p.property, label: p.label || short(p.property), count: (seen.get(p.property) || { count: 0 }).count + n });
      }
      props = [...seen.values()].sort((a, b) => b.count - a.count);
    } catch (e) { return; } // no profile to read: no card
    const box = document.getElementById("sb-similar");
    if (!box || !shown || shown.ds !== ds || !props.length) return;
    const AI = window.TripleTrailAI;
    const services = AI ? AI.connections() : [];
    const property = el("select", { "aria-label": "Vector property" }, props.map((p) => el("option", { value: p.iri, title: p.iri }, p.label)));
    const text = el("input", { class: "sb-value sb-similar-text", type: "search", placeholder: "Text, or a resource's IRI", "aria-label": "Text or IRI to find similar resources to" });
    const service = el("select", { "aria-label": "AI service for the text's vector" },
      services.length ? services.map((c) => el("option", { value: c.id }, c.label)) : el("option", { value: "" }, "No AI service set up"));
    const current = services.length ? AI.connection() : null;
    if (current) service.value = current.id;
    const model = el("input", { class: "sb-value sb-similar-model", "aria-label": "Embedding model", placeholder: "Embedding model" });
    const fillModel = () => {
      const c = services.find((x) => x.id === service.value);
      model.value = c ? savedModels()[c.id] || AI.embeddingModel(c) : "";
      model.disabled = !c;
    };
    service.addEventListener("change", fillModel);
    fillModel();
    const status = el("span", { class: "muted small", role: "status" });
    const results = el("div", { class: "sb-results sb-similar-results", hidden: true });
    const find = async () => {
      const q = text.value.trim();
      if (!q) { text.focus(); return; }
      const mine = ++similarAsked;
      status.className = "muted small";
      const params = { property: property.value, k: 10 };
      try {
        if (iriOk(q)) params.iri = q;
        else {
          const c = services.find((x) => x.id === service.value);
          if (!c) throw new Error("Set up an AI service (Workbench, AI tab, “Key and model…”) to search by text, or give a resource's IRI.");
          if (!model.value.trim()) { model.focus(); throw new Error("Name the service's embedding model, the one that made the dataset's vectors."); }
          saveModel(c.id, model.value.trim());
          status.textContent = "Asking " + c.label + " for the text's vector…";
          params.vector = JSON.stringify(await AI.embed(c, model.value.trim(), q));
        }
        status.textContent = "Searching…";
        const r = await ui.api("GET", "/" + encodeURIComponent(ds) + "/search/similar", { params });
        if (mine !== similarAsked) return;
        status.textContent = r.results.length ? "" : "Nothing found: is the property's vector as long as the model's?";
        resultsTable(ds, results, r.results);
      } catch (e) {
        if (mine !== similarAsked) return;
        status.textContent = e.message;
        status.className = "error small";
        results.hidden = true;
      }
    };
    text.addEventListener("keydown", (e) => { if (e.key === "Enter") find(); });
    box.replaceChildren(
      el("h3", { class: "sb-similar-title" }, "Find similar"),
      el("p", { class: "muted small" }, "The resources whose vector is nearest a text or a resource. A text is turned into a vector in this browser by your AI service, with your key; the server is given only the numbers."),
      el("div", { class: "sb-line" }, el("span", { class: "sb-word" }, "near"), text, el("span", { class: "sb-word" }, "by"), property),
      el("div", { class: "sb-line" }, el("span", { class: "sb-word" }, "with"), service, model,
        services.length ? null : el("a", { class: "small", href: "/$/ui/workbench", "data-route": "" }, "Set up an AI service")),
      el("div", { class: "sb-actions" }, el("button", { class: "btn primary", type: "button", onclick: find }, "Find similar"), status),
      results);
    box.hidden = false;
  }

  /** The label of `iri` in its JSON-LD description, if it has one. */
  function labelOf(iri, doc) {
    const nodes = Array.isArray(doc) ? doc : doc && Array.isArray(doc["@graph"]) ? doc["@graph"] : [doc];
    const node = nodes.find((n) => n && n["@id"] === iri);
    if (!node) return null;
    for (const key of Object.keys(node)) {
      if (!/[/#](label|prefLabel|name|title)$/.test(key)) continue;
      const v = [].concat(node[key])[0];
      const s = v && typeof v === "object" ? v["@value"] : v;
      if (typeof s === "string" && s) return s;
    }
    return null;
  }

  function resultsTable(ds, box, rows) {
    box.replaceChildren(rows.length
      ? el("table", { class: "grid" },
        el("thead", {}, el("tr", {}, el("th", {}, "Resource"), el("th", { class: "num" }, "Similarity"))),
        el("tbody", {}, rows.map((r) => {
          const label = labelOf(r.iri, r.description) || short(r.iri);
          return el("tr", {},
            el("td", { class: "name" }, el("a", { href: "/" + encodeURIComponent(ds) + "/describe?iri=" + encodeURIComponent(r.iri), title: r.iri }, label), " ",
              el("a", { class: "muted small", href: "/$/ui/explore/" + encodeURIComponent(ds) + "?iri=" + encodeURIComponent(r.iri), "data-route": true, title: "Walk the graph from " + label }, "explore")),
            el("td", { class: "num" }, r.score.toFixed(3)));
        })))
      : el("p", { class: "muted sb-none" }, "Nothing found."));
    box.hidden = false;
  }

  async function copyLink() {
    writeModel();
    await ui.copyText(location.href, "Link");
  }
  async function openInQuery() {
    let q;
    try { q = await ui.api("GET", searchUrl("sparql")); } catch (e) { ui.toast(e.message, "bad"); return; }
    try { sessionStorage.setItem("tripletrail-open-query", q); } catch (e) { /* the query page opens empty */ }
    ui.go("query/" + encodeURIComponent(shown.ds));
  }

  return { render };
})();
