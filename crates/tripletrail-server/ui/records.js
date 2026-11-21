// The Records page (#81): browse a dataset's records read-only, in three panes. Plain
// JavaScript like app.js, which calls TripleTrailRecords.render(ui, args).
//
// /$/ui/records/{dataset}?class=ID&iri=IRI
//
// - The navigator lists the classes with their number of records (`/search/classes`).
// - The grid shows a class's records, a column per key, paged, sorted and filtered on the
//   server (`/search/results` with `offset`, `sort`, `q` and `where`), and downloads the
//   filtered rows as CSV (`f=csv`).
// - The record pane shows one record as a form: the shapes' groups in their order, each row
//   drawn by its `dash:viewer` (forms.js), `sh:node` values as tables, inherited rows of
//   superclass shapes (the describe view, read for the forms reader: `reader=forms`), and the
//   resources that link to it.
//
// Everything is read through the dataset's endpoints, so the caller sees only what it may read.
"use strict";

window.TripleTrailRecords = (function () {
  const VENDOR = "/$/ui/vendor/";
  /** Rows a grid page holds. */
  const PAGE = 50;
  /** Columns a grid shows at most (a search shows 20 keys). */
  const MAX_COLUMNS = 20;
  let ui = null;
  let state = null; // { ds, classes, cls, iri, q, table }
  const page = () => document.getElementById("page-records");
  const el = (...a) => ui.el(...a);
  const enc = encodeURIComponent;
  const base = () => "/" + enc(state.ds);

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
  const tabulator = () => Promise.all([script(VENDOR + "tabulator/tabulator.min.js"), css(VENDOR + "tabulator/tabulator.min.css")]);

  // ---------- the link ----------
  function writeLink() {
    const q = new URLSearchParams();
    if (state.cls) q.set("class", state.cls.id);
    if (state.iri) q.set("iri", state.iri);
    const search = q.toString();
    history.replaceState(null, "", "/$/ui/records/" + enc(state.ds) + (search ? "?" + search : ""));
  }

  // ---------- the page ----------
  async function render(shared, args) {
    ui = shared;
    const root = page();
    let datasets = [];
    try { datasets = (await ui.api("GET", "/$/datasets")).datasets.map((d) => d.name); } catch (e) { ui.fail(e); }
    let ds = args[0] ? decodeURIComponent(args[0]) : null;
    if (!ds || !datasets.includes(ds)) ds = state && datasets.includes(state.ds) ? state.ds : datasets[0];
    if (!ds) { root.replaceChildren(el("p", { class: "muted loading" }, "No dataset yet.")); return; }
    root.replaceChildren(el("p", { class: "muted loading" }, "Loading the classes…"));
    let classes;
    try { classes = (await ui.api("GET", "/" + enc(ds) + "/search/classes")).classes; }
    catch (e) { root.replaceChildren(el("p", { class: "error loading" }, e.message)); return; }
    const params = new URLSearchParams(location.search);
    const asked = params.get("class");
    const cls = classes.find((c) => c.id === asked) || classes.find((c) => c.class === asked) || classes[0] || null;
    state = { ds, classes, cls, iri: params.get("iri") || null, q: "", table: null, filter: "" };
    const select = el("select", { id: "records-ds", "aria-label": "Dataset", onchange: (e) => ui.go("records/" + enc(e.target.value)) },
      datasets.map((d) => el("option", { value: d, selected: d === ds }, d)));
    const find = el("input", { type: "search", id: "records-classes-filter", placeholder: "Filter classes", "aria-label": "Filter classes",
      oninput: (e) => { state.filter = e.target.value.trim().toLowerCase(); navigator(); } });
    root.replaceChildren(
      el("div", { class: "records" },
        el("aside", { class: "records-nav card", "aria-label": "Classes" },
          el("label", { class: "small" }, "Dataset ", select), find,
          el("nav", { id: "records-classes" })),
        el("section", { class: "records-grid card", id: "records-grid", "aria-label": "Records" }),
        el("section", { class: "records-record card", id: "records-record", "aria-label": "Record", hidden: true })));
    navigator();
    if (state.cls) await grid();
    if (state.iri) record(state.iri);
    writeLink();
  }

  /** The classes, each with its number of records; the one shown marked. */
  function navigator() {
    const nav = document.getElementById("records-classes");
    if (!nav) return;
    const shown = state.classes.filter((c) => !state.filter || (c.title || c.id).toLowerCase().includes(state.filter));
    nav.replaceChildren(shown.length
      ? el("ul", { class: "records-class-list" }, shown.map((c) => el("li", {},
        el("button", { type: "button", class: "records-class" + (state.cls && c.id === state.cls.id ? " active" : ""), title: c.class,
          "aria-current": state.cls && c.id === state.cls.id ? "true" : null,
          onclick: () => { state.cls = c; state.iri = null; navigator(); grid(); closeRecord(); writeLink(); } },
        el("span", {}, c.title || c.id), el("span", { class: "muted small" }, c.count == null ? "" : ui.fmtNum(c.count))))))
      : el("p", { class: "muted small" }, state.classes.length ? "No class matches." : "No class with records yet."));
  }

  // ---------- the grid ----------
  /** The keys a grid shows: those with values to show in a cell (not geometries or objects),
   *  and not the label, which the first column shows. */
  const columnsOf = (c) => c.properties
    .filter((p) => p.kind !== "geometry" && p.kind !== "object" && !(c.label && p.path === "<" + c.label + ">"))
    .slice(0, MAX_COLUMNS);

  /** A header filter's text as a search condition for a key of `kind`: numbers take `>5`,
   *  `<=5`, `5..10` or `5`; booleans `yes` or `no`; others their text (contained). */
  function condition(p, text) {
    const t = String(text || "").trim();
    if (!t) return null;
    if (p.kind === "number") {
      let m = /^(<=|>=|<|>|=|!=)\s*(-?\d+(\.\d+)?)$/.exec(t);
      if (m) return { key: p.key, op: { "<": "lt", "<=": "le", ">": "gt", ">=": "ge", "=": "eq", "!=": "ne" }[m[1]], value: m[2] };
      m = /^(-?\d+(\.\d+)?)\s*\.\.\s*(-?\d+(\.\d+)?)$/.exec(t);
      if (m) return { key: p.key, op: "between", value: m[1] + ".." + m[3] };
      if (/^-?\d+(\.\d+)?$/.test(t)) return { key: p.key, op: "eq", value: t };
      return null;
    }
    if (p.kind === "boolean") return /^(yes|true|1)$/i.test(t) ? { key: p.key, op: "true" } : /^(no|false|0)$/i.test(t) ? { key: p.key, op: "false" } : null;
    if (p.kind === "date") return { key: p.key, op: "on", value: t };
    if (p.kind === "reference" || p.kind === "iri") return null; // filtered by their text below
    return { key: p.key, op: "contains", value: t };
  }

  /** The search the grid runs: its class, the header filters, the label text and the sort. */
  function searchModel(filters, sorters) {
    const keys = columnsOf(state.cls);
    const where = [];
    for (const f of filters || []) {
      const p = keys.find((k) => k.key === f.field);
      const c = p && condition(p, f.value);
      if (c) where.push(c);
    }
    const m = { class: state.cls.id, where, show: keys.map((p) => p.key) };
    if (state.q) m.q = state.q;
    const s = (sorters || [])[0];
    if (s) m.sort = { key: s.field === "_label" ? "label" : s.field, desc: s.dir === "desc" };
    return m;
  }

  /** A cell's value as text: a resource by its label, several values joined. */
  function cellText(v) {
    if (v == null) return "";
    if (Array.isArray(v)) return v.map(cellText).join(", ");
    if (typeof v === "object") return v.label || v.iri || "";
    if (v === true) return "yes";
    if (v === false) return "no";
    return String(v);
  }

  async function grid() {
    const box = document.getElementById("records-grid");
    if (!box || !state.cls) return;
    const c = state.cls;
    const keys = columnsOf(c);
    const q = el("input", { type: "search", id: "records-q", placeholder: "Label contains…", "aria-label": "Search the labels", value: state.q });
    const count = el("span", { class: "muted small", id: "records-count", role: "status" });
    const csv = el("a", { class: "btn small", id: "records-csv", download: "", title: "The filtered records as CSV (up to 10,000)" }, "CSV");
    const sparql = el("button", { type: "button", class: "btn small", title: "Open the grid's query on the Query page" }, "Open as SPARQL");
    const holder = el("div", { class: "records-table", id: "records-table" });
    box.replaceChildren(
      el("div", { class: "records-grid-head" }, el("h2", {}, c.title || c.id), count, el("span", { class: "grow" }), q, csv, sparql),
      holder);
    let last = null; // the search of the page shown
    const setLinks = () => {
      if (!last) return;
      csv.href = base() + "/search/results?" + new URLSearchParams({ m: JSON.stringify(last), f: "csv", limit: "10000" });
    };
    sparql.onclick = async () => {
      if (!last) return;
      try {
        const text = await ui.api("GET", base() + "/search/sparql?m=" + enc(JSON.stringify(last)));
        try { sessionStorage.setItem("tripletrail-open-query", text); } catch (e) { /* the query page opens empty */ }
        ui.go("query/" + enc(state.ds));
      } catch (e) { ui.toast(e.message, "bad"); }
    };
    try { await tabulator(); } catch (e) { holder.replaceChildren(el("p", { class: "error" }, e.message)); return; }
    if (state.table) { try { state.table.destroy(); } catch (e) { /* gone with its element */ } }
    const filterOf = (p) => (p.kind === "boolean" ? { headerFilter: "list", headerFilterParams: { values: { "": "", yes: "yes", no: "no" } } }
      : p.kind === "reference" || p.kind === "iri" ? { headerFilter: false } : { headerFilter: "input", headerFilterPlaceholder: p.kind === "number" ? ">5, 5..10" : "" });
    const table = new window.Tabulator(holder, {
      layout: "fitDataStretch",
      height: "100%",
      placeholder: "No records",
      index: "iri",
      pagination: true,
      paginationMode: "remote",
      paginationSize: PAGE,
      sortMode: "remote",
      filterMode: "remote",
      headerFilterLiveFilterDelay: 500,
      ajaxURL: base() + "/search/results",
      ajaxRequestFunc: async (url, config, params) => {
        const m = searchModel(params.filter, params.sort);
        const offset = ((params.page || 1) - 1) * PAGE;
        const out = await ui.api("GET", url, { params: { m: JSON.stringify(m), limit: String(PAGE), offset: String(offset) } });
        last = m;
        setLinks();
        count.textContent = ui.fmtNum(out.count) + (out.count === 1 ? " record" : " records");
        return { last_page: Math.max(1, Math.ceil(out.count / PAGE)), data: out.rows.map((r) => ({ iri: r.iri, _label: r.label || r.iri, ...r.values })) };
      },
      columns: [
        { title: "Label", field: "_label", frozen: true, headerFilter: false, formatter: (cell) => {
          const r = cell.getRow().getData();
          return el("a", { href: "/$/ui/records/" + enc(state.ds) + "?class=" + enc(c.id) + "&iri=" + enc(r.iri), title: r.iri,
            onclick: (e) => { e.preventDefault(); record(r.iri); } }, r._label);
        } },
        ...keys.map((p) => ({ title: p.title || p.key, field: p.key, headerSort: p.single, ...filterOf(p),
          hozAlign: p.kind === "number" ? "right" : "left",
          formatter: (cell) => cellText(cell.getValue()) })),
      ],
    });
    table.on("rowClick", (e, row) => { if (!e.target.closest("a")) record(row.getData().iri); });
    state.table = table;
    let typing = null;
    q.addEventListener("input", () => {
      clearTimeout(typing);
      typing = setTimeout(() => { state.q = q.value.trim(); table.setData(); }, 400);
    });
  }

  // ---------- the record ----------
  function closeRecord() {
    const box = document.getElementById("records-record");
    if (box) { box.hidden = true; box.replaceChildren(); }
  }

  /** A value of the describe view as the form module takes it: an IRI with its label, or a literal's text. */
  const valueOf = (v) => (v.type === "uri" ? { value: v.value, label: v.label || null } : v.type === "bnode" ? "…" : v.value);

  async function record(iri) {
    state.iri = iri;
    writeLink();
    const box = document.getElementById("records-record");
    if (!box) return;
    box.hidden = false;
    box.replaceChildren(el("p", { class: "muted small" }, "Reading the record…"));
    let view;
    try { view = await ui.api("GET", base() + "/describe", { params: { iri, format: "view", reader: "forms" } }); }
    catch (e) { box.replaceChildren(el("p", { class: "error" }, e.message)); return; }
    if (state.iri !== iri) return;
    const Forms = window.TripleTrailForms;
    const title = view.label || iri;
    const actions = el("div", { class: "records-actions" },
      el("a", { class: "btn small", href: base() + "/describe?iri=" + enc(iri), target: "_blank", rel: "noopener" }, "Describe"),
      view.geometries && view.geometries.length ? el("a", { class: "btn small", href: "/$/ui/map/" + enc(state.ds) + "?feature=" + enc(iri) }, "Map") : null,
      el("a", { class: "btn small", href: "/$/ui/explore/" + enc(state.ds) + "?iri=" + enc(iri) }, "Explore"),
      el("button", { type: "button", class: "btn small", title: "Open the record's triples on the Query page", onclick: () => {
        const text = "SELECT ?property ?value WHERE {\n  <" + iri + "> ?property ?value .\n}";
        try { sessionStorage.setItem("tripletrail-open-query", text); } catch (e) { /* the query page opens empty */ }
        ui.go("query/" + enc(state.ds));
      } }, "Open as SPARQL"),
      el("button", { type: "button", class: "btn small", "aria-label": "Close the record", title: "Close", onclick: () => { state.iri = null; closeRecord(); writeLink(); } }, "×"));
    const groups = (view.groups || []).filter((g) => g.rows && g.rows.length).map((g) =>
      el("fieldset", { class: "records-group" }, el("legend", {}, g.label || "Properties"),
        g.rows.map((r, i) => row(r, Forms, "records-f-" + i + "-" + (r.key || "")))));
    box.replaceChildren(
      el("div", { class: "records-record-head" },
        el("div", {}, el("h2", {}, title), el("p", { class: "muted small mono" }, iri, " ", ui.copyButton(iri))),
        actions),
      el("p", { class: "muted small" }, (view.classes || []).map((k) => k.label || k.iri.replace(/^.*[#/]/, "")).join(", ")),
      ...(groups.length ? groups : [el("p", { class: "muted" }, "Nothing to show for this record.")]),
      el("details", { class: "records-group records-incoming", id: "records-incoming", open: true }, el("summary", {}, "Referenced by"), referencedBy(view)));
  }

  /** One row of the record: its label and its values, drawn by its viewer, or a table for `sh:node` values. */
  function row(r, Forms, id) {
    const label = r.label || r.key || "";
    let control;
    if (r.columns && r.cells) {
      control = el("table", { class: "records-subtable" },
        el("thead", {}, el("tr", {}, r.columns.map((c) => el("th", {}, c.label || c.key)))),
        el("tbody", {}, r.cells.map((line) => el("tr", {}, line.map((cell) =>
          el("td", {}, Forms.viewer({ viewer: "LiteralViewer" }, { el, value: (cell || []).map((v) => (typeof v === "object" ? cellLabel(v) : v)) })))))));
    } else {
      const all = r.values || [];
      const own = r.viewer && r.viewer !== "DetailsViewer" && r.viewer !== "ValueTableViewer" && r.viewer !== "LabelViewer" && r.viewer !== "URIViewer";
      if (!own && all.length && all.every((v) => v.type === "uri" || v.type === "bnode")) {
        // Resources open as records here; a blank node (such as a geometry) shows its values.
        control = el("span", { class: "form-values" }, all.map((v, i) => el("span", {}, i ? ", " : null, v.type === "uri"
          ? el("a", { href: recordHref(v.value), title: v.value, onclick: (e) => { e.preventDefault(); record(v.value); } }, cellLabel(v))
          : el("span", { class: "muted small", title: blankText(v) }, short(blankText(v))))));
      } else {
        control = Forms.viewer({ viewer: own ? r.viewer : "LiteralViewer", label }, { el, value: all.map(valueOf) });
      }
    }
    const more = r.more ? el("span", { class: "muted small" }, ` and ${ui.fmtNum(r.more)} more`) : null;
    const unit = r.unit ? el("span", { class: "muted small" }, " " + r.unit) : null;
    return el("div", { class: "shapes-field records-field", id }, el("span", { class: "records-label", title: r.property || null }, label), el("span", {}, control, unit, more));
  }
  const cellLabel = (v) => (v.type === "uri" ? v.label || v.value.replace(/^.*[#/]/, "") : v.value);
  /** A blank node's values as text: `property value; …`. */
  const blankText = (v) => (v.rows || []).map((r) => (r.property.label || r.property.iri.replace(/^.*[#/]/, "")) + " " + (r.value.type === "bnode" ? "…" : r.value.value)).join("; ");
  const short = (t) => (t.length > 80 ? t.slice(0, 79) + "…" : t || "…");
  /** The Records page's link to a record, in the class shown. */
  const recordHref = (iri) => "/$/ui/records/" + enc(state.ds) + "?" + (state.cls ? "class=" + enc(state.cls.id) + "&" : "") + "iri=" + enc(iri);

  /** The resources that link to the record, by property: the describe view's own list, so
   *  only links the caller may read. */
  function referencedBy(view) {
    const list = view.incoming || [];
    if (!list.length) return el("p", { class: "muted small" }, "No resource links to it.");
    return el("div", {},
      el("dl", { class: "records-incoming-list" }, list.flatMap((i) => [
        el("dt", { title: i.property.iri }, i.property.label || i.property.iri.replace(/^.*[#/]/, ""),
          i.count > i.subjects.length ? el("span", { class: "muted small" }, " (" + ui.fmtNum(i.count) + ")") : null),
        el("dd", {}, i.subjects.filter((s) => s.type === "uri").map((s, n) => el("span", {}, n ? ", " : null,
          el("a", { href: recordHref(s.value), title: s.value,
            onclick: (e) => { e.preventDefault(); record(s.value); } }, s.label || s.value.replace(/^.*[#/]/, "")))))])),
      view.incomingTruncated ? el("p", { class: "muted small" }, "More resources link to it; Describe lists them.") : null);
  }

  function leave() {
    if (state && state.table) { try { state.table.destroy(); } catch (e) { /* gone */ } state.table = null; }
  }

  return { render, leave, condition };
})();
