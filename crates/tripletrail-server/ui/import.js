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
          el("span", { class: "muted small", id: "imp-status", role: "status" })),
        el("details", { class: "imp-ai", id: "imp-ai" }, el("summary", {}, "Draft with AI"),
          el("p", { class: "muted small" }, "Your AI service writes the query from a sample of the file and the dataset's shapes, and checks it by previewing it on the file. The sample and the shapes go from this browser to that service, with your key."),
          el("div", { class: "sb-actions" },
            el("input", { id: "imp-ai-ask", type: "text", size: 60, placeholder: "What should it become? (optional, e.g. one station per row, with its name and position)", "aria-label": "What the query should make" }),
            el("button", { class: "btn", type: "button", id: "imp-ai-go", onclick: draft }, "Draft"),
            el("button", { class: "btn", type: "button", id: "imp-ai-back", hidden: true, onclick: back }, "Back to the previous query")),
          el("div", { class: "muted small", id: "imp-ai-log", role: "status" }))),
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

  // "Draft with AI" (#20): the model writes the CONSTRUCT from a sample of the file and the
  // dataset's shapes, previewing it on the file until it runs, through the user's own AI
  // service (ai.js). It writes nothing: the query lands in the editor, and the previous one
  // stays a click away.
  const SH = "http://www.w3.org/ns/shacl#";
  const SYSTEM = `You write a SPARQL CONSTRUCT query that turns an uploaded file into RDF for a TripleTrail dataset.
- The file is read through Facade-X: the draft query's SERVICE <x-sparql-anything:...> clause reads it (location=upload:NAME). Keep that clause and its options as they are; change only the CONSTRUCT template and the patterns and BINDs inside the WHERE.
- In Facade-X a file is a tree: fx:root is the document, containers are blank nodes, rdf:_1, rdf:_2... are positions (rows of a CSV, items of a list), and xyz: properties are the keys (CSV column names, JSON keys) with literal values. The sample shows the file's own triples.
- Use the classes and properties of the dataset's shapes (get_shapes; with a class IRI, one class's node shape) where they fit, with their datatypes. Make IRIs for the things from a key's value with BIND(IRI(CONCAT(...)) AS ?x), never blank nodes for things that have an id. Never invent prefixes you do not declare.
- Check the query with preview, which runs it on the whole file and answers how many triples it makes and the first ones, or the error. Fix what it reports and check again.
- Answer with the whole query, with all its prefixes, in one fenced block (\`\`\`sparql), then one or two sentences on what it makes. Answer in the user's language.`;
  let previous = null, aiAbort = null;

  async function draft() {
    const AI = window.TripleTrailAI;
    const c = AI && AI.connection();
    const log = $("imp-ai-log");
    if (!c) {
      log.replaceChildren("Set up an AI service first: ", el("a", { href: UI + "workbench", "data-route": "" }, "Workbench, AI tab, “Key and model…”"), ".");
      return;
    }
    if (aiAbort) { aiAbort.abort(); return; }
    const go = $("imp-ai-go");
    const u = upload;
    const tools = [
      { name: "preview", description: "Runs a CONSTRUCT on the whole uploaded file, writing nothing. Answers how many triples it makes and the first 40, as N-Triples, or the error.",
        parameters: { type: "object", properties: { query: { type: "string", description: "The whole CONSTRUCT query" } }, required: ["query"] },
        run: async ({ query: q }) => {
          const lines = (await send("POST", base() + "/" + u.id + "/preview", String(q || ""), "application/n-triples")).split("\n").filter(Boolean);
          return lines.length + " triples" + (lines.length ? ":\n" + lines.slice(0, 40).join("\n") : ". The patterns match nothing in the file: compare them with the sample.");
        } },
      { name: "get_shapes", description: "The SHACL shapes of the dataset the file goes into, as Turtle: those its readers use now. 'class' (an IRI) keeps only that class's node shape and its property shapes.",
        parameters: { type: "object", properties: { class: { type: "string", description: "Optional class IRI" } } },
        run: async ({ class: cls }) => {
          const path = "/$/datasets/" + encodeURIComponent(ds) + "/shapes";
          if (!cls) return send("GET", path, null, "text/turtle");
          const iri = String(cls).replace(/[<>"\s]/g, "");
          return send("POST", path + "/sparql", `CONSTRUCT { ?s ?p ?o } WHERE { ?shape <${SH}targetClass> <${iri}> . { BIND(?shape AS ?s) ?s ?p ?o } UNION { ?shape <${SH}property> ?s . ?s ?p ?o } }`, "text/turtle");
        } },
    ];
    const ask = $("imp-ai-ask").value.trim();
    const messages = [
      { role: "system", content: SYSTEM },
      { role: "user", content: `File: ${u.name} (${u.bytes} bytes${u.options ? ", options " + u.options : ""}).\n\nIts Facade-X triples${u.truncated ? " (the first ones)" : ""}:\n${u.tree.slice(0, 12000)}\n\nThe query now:\n\`\`\`sparql\n${query()}\n\`\`\`\n\n${ask || "Map it to the dataset's classes and properties where they fit."}` },
    ];
    aiAbort = new AbortController();
    go.textContent = "Stop";
    log.className = "muted small";
    log.textContent = "Asking " + c.label + "…";
    try {
      const answer = await AI.converse(c, messages, tools, {
        signal: aiAbort.signal, maxSteps: 8,
        onStep: (kind, d) => { log.textContent = d.name === "preview" ? "Checking the query on the file…" : "Reading the dataset's shapes…"; },
      });
      const blocks = [...answer.matchAll(/```(?:sparql|rq)?\s*\n([\s\S]*?)```/g)].map((m) => m[1].trim());
      const found = blocks.reverse().find((b) => /\bCONSTRUCT\b/i.test(b));
      if (!found) { log.textContent = answer || "The model gave no query."; return; }
      previous = query();
      $("imp-query").value = found;
      $("imp-ai-back").hidden = false;
      log.textContent = answer.replace(/```[\s\S]*?```/g, "").trim();
      await preview();
    } catch (e) {
      if (e && e.name === "AbortError") { log.textContent = "Stopped."; return; }
      log.className = "error small";
      log.textContent = e.message;
    } finally {
      aiAbort = null;
      go.textContent = "Draft";
    }
  }

  function back() {
    if (previous === null) return;
    $("imp-query").value = previous;
    previous = null;
    $("imp-ai-back").hidden = true;
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
    previous = null;
    if (aiAbort) aiAbort.abort();
    if (!upload || !ds) { upload = null; return; }
    const path = base() + "/" + upload.id;
    upload = null;
    send("DELETE", path).catch(() => { /* gone already */ });
  }

  return { render };
})();
