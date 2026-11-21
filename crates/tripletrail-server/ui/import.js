// The Import page (#20): drop a file, see it as Facade-X triples, edit the drafted CONSTRUCT,
// preview what it makes, and add that to a named graph. Plain JavaScript like app.js, which
// calls TripleTrailImport.render(ui, args).
//
// /$/ui/import/{dataset}   the dataset the file goes into
//
// The file is kept in the server's memory for an hour (`POST /{ds}/import`), never written to
// disk, and only someone who may write to the dataset can send one. The mapping may read only
// that file (`location=upload:NAME`). "Download query" saves it as a lift query that reads the
// file by its name, for `tripletrail lift`.
"use strict";

window.TripleTrailImport = (function () {
  const UI = "/$/ui/";
  const TOKEN_KEY = "tripletrail-admin-token";
  let ui = null;
  let ds = null;
  let upload = null; // the server's answer to the upload: { id, name, bytes, options, tree, triples, truncated, query }
  const page = () => document.getElementById("page-import");
  const el = (...a) => ui.el(...a);
  const $ = (id) => document.getElementById(id);
  const base = () => "/" + encodeURIComponent(ds) + "/import";

  /** A request with the admin token, answered as text; errors as ApiError like ui.api. */
  async function send(method, path, body, accept) {
    const headers = { "X-TripleTrail-Client": "ui" };
    const token = ui.store.get(TOKEN_KEY);
    if (token) headers.Authorization = "Bearer " + token;
    if (accept) headers.Accept = accept;
    if (typeof body === "string") headers["Content-Type"] = "application/sparql-query";
    const r = await fetch(path, { method, headers, body });
    const text = await r.text();
    if (!r.ok) {
      if (r.status === 401) ui.openToken("Importing needs a token that may write to the dataset.");
      throw new ui.ApiError(r.status, text.trim() || r.statusText);
    }
    return text;
  }

  async function render(shared, args) {
    ui = shared;
    const root = page();
    let datasets = [];
    try {
      datasets = (await ui.api("GET", "/$/datasets")).datasets.map((d) => d.name).filter((n) => !n.startsWith("$"));
    } catch (e) { ui.fail(e); }
    let want = args[0] ? decodeURIComponent(args[0]) : null;
    if (!want || !datasets.includes(want)) want = ds && datasets.includes(ds) ? ds : datasets[0];
    if (!want) { root.replaceChildren(el("p", { class: "muted loading" }, "No dataset to import into yet.")); return; }
    if (want !== ds) forget();
    ds = want;
    if (decodeURIComponent(location.pathname.split("/")[4] || "") !== ds) {
      history.replaceState(null, "", UI + "import/" + encodeURIComponent(ds));
    }
    const select = el("select", { "aria-label": "Dataset", onchange: (e) => ui.go("import/" + encodeURIComponent(e.target.value)) },
      datasets.map((d) => el("option", { value: d, selected: d === ds }, d)));
    const file = el("input", { type: "file", id: "imp-file", "aria-label": "File to import", onchange: (e) => { if (e.target.files[0]) read(e.target.files[0]); } });
    const drop = el("label", { class: "imp-drop card", for: "imp-file" },
      el("strong", {}, "Drop a file here, or choose one"),
      el("span", { class: "muted small" }, "CSV, JSON, XML, YAML, Excel, HTML, Markdown, a shapefile in a zip, or an archive. It stays in the server's memory for an hour."),
      file);
    drop.addEventListener("dragover", (e) => { e.preventDefault(); drop.classList.add("over"); });
    drop.addEventListener("dragleave", () => drop.classList.remove("over"));
    drop.addEventListener("drop", (e) => {
      e.preventDefault(); drop.classList.remove("over");
      if (e.dataTransfer.files[0]) read(e.dataTransfer.files[0]);
    });
    root.replaceChildren(
      el("div", { class: "query-bar" }, el("label", {}, "Dataset ", select), el("span", { class: "grow" }),
        el("span", { class: "muted small" }, "Turn a file into RDF with a query, and add it to a graph.")),
      drop,
      el("div", { id: "imp-work" }));
    if (upload) show();
  }

  async function read(f) {
    forget();
    const work = $("imp-work");
    work.replaceChildren(el("p", { class: "muted loading" }, "Reading " + f.name + "…"));
    // The server takes names of letters, digits, '.', '-' and '_'.
    const name = f.name.replace(/[^A-Za-z0-9._-]/g, "_").replace(/^\.+/, "") || "upload";
    try {
      upload = JSON.parse(await send("POST", base() + "?name=" + encodeURIComponent(name), f));
    } catch (e) {
      work.replaceChildren(el("p", { class: "error" }, e.message));
      return;
    }
    show();
  }

  function show() {
    const u = upload;
    const graph = "urn:import:" + u.name.replace(/\.[^.]*$/, "");
    $("imp-work").replaceChildren(
      el("div", { class: "card imp-card" },
        el("div", { class: "toolbar" }, el("h2", {}, u.name),
          el("span", { class: "muted small" }, ui.fmtBytes(u.bytes) + " · " + ui.fmtNum(u.triples) + (u.truncated ? "+" : "") + " triples as Facade-X" + (u.options ? " · " + u.options : "")),
          el("span", { class: "grow" }),
          el("button", { class: "btn small", type: "button", onclick: () => { forget(); $("imp-work").replaceChildren(); } }, "Forget the file")),
        el("details", { open: true }, el("summary", {}, "The file as triples" + (u.truncated ? " (the first ones)" : "")),
          el("pre", { class: "imp-tree", id: "imp-tree" }, u.tree))),
      el("div", { class: "card imp-card" },
        el("div", { class: "toolbar" }, el("h2", {}, "The query"), el("span", { class: "grow" }),
          el("span", { class: "muted small" }, "A draft that maps every key; change the IRIs, classes and datatypes.")),
        el("textarea", { class: "imp-query", id: "imp-query", rows: 18, spellcheck: "false", "aria-label": "The CONSTRUCT query" }, u.query),
        el("div", { class: "sb-actions" },
          el("button", { class: "btn", type: "button", onclick: preview }, "Preview"),
          el("label", {}, "Into graph ", el("input", { id: "imp-graph", type: "text", size: 40, value: graph, "aria-label": "Graph IRI" })),
          el("button", { class: "btn primary", type: "button", onclick: run }, "Add to the graph"),
          el("button", { class: "btn", type: "button", onclick: download, title: "Save the query to run with tripletrail lift, reading the file by its name" }, "Download query"),
          el("span", { class: "muted small", id: "imp-status", role: "status" }))),
      el("div", { class: "card imp-card", id: "imp-out", hidden: true }));
  }

  const query = () => $("imp-query").value;
  const status = (t) => { $("imp-status").textContent = t; };

  async function preview() {
    status("Running…");
    const out = $("imp-out");
    try {
      const nt = await send("POST", base() + "/" + upload.id + "/preview", query(), "application/n-triples");
      const lines = nt.split("\n").filter(Boolean);
      out.replaceChildren(el("h2", {}, "Preview: " + ui.fmtNum(lines.length) + " triples, nothing written"),
        el("pre", { class: "imp-tree" }, lines.slice(0, 500).join("\n") + (lines.length > 500 ? "\n…" : "")));
      out.hidden = false;
      status("");
    } catch (e) { status(""); fail(e); }
  }

  async function run() {
    const graph = $("imp-graph").value.trim();
    if (!/^[a-z][a-z0-9+.-]*:\S+$/i.test(graph)) { ui.toast("The graph needs an IRI, such as urn:import:stations.", "bad"); return; }
    status("Adding…");
    try {
      const r = JSON.parse(await send("POST", base() + "/" + upload.id + "/run?graph=" + encodeURIComponent(graph), query()));
      status("");
      const out = $("imp-out");
      out.replaceChildren(el("h2", {}, "Added to " + r.graph),
        el("p", {}, "The graph had " + ui.fmtNum(r.before) + " triples and now has " + ui.fmtNum(r.after) + ". ",
          el("a", { href: UI + "query/" + encodeURIComponent(ds) + "?query=" + encodeURIComponent("SELECT * WHERE { GRAPH <" + r.graph + "> { ?s ?p ?o } } LIMIT 100") }, "Look at it")));
      out.hidden = false;
      ui.toast("Added to " + r.graph);
    } catch (e) { status(""); fail(e); }
  }

  function fail(e) {
    if (e.status === 404) { upload = null; $("imp-work").replaceChildren(el("p", { class: "error" }, e.message + ". Drop the file again.")); return; }
    const out = $("imp-out");
    out.replaceChildren(el("p", { class: "error" }, e.message));
    out.hidden = false;
  }

  /** The query as a lift query: it reads the file by its name instead of the upload. */
  function download() {
    const text = query().split("location=upload:" + upload.name).join("location=" + upload.name)
      .split("from-archive=upload:" + upload.name).join("from-archive=" + upload.name);
    const a = el("a", { href: URL.createObjectURL(new Blob([text], { type: "application/sparql-query" })), download: upload.name.replace(/\.[^.]*$/, "") + ".rq" });
    document.body.append(a); a.click(); a.remove();
    setTimeout(() => URL.revokeObjectURL(a.href), 1000);
  }

  /** Tells the server to drop the file now rather than in an hour. */
  function forget() {
    if (!upload || !ds) { upload = null; return; }
    const path = base() + "/" + upload.id;
    upload = null;
    send("DELETE", path).catch(() => { /* gone already */ });
  }

  return { render };
})();
