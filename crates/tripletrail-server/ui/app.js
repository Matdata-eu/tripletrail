// The TripleTrail server UI: dataset management over the admin API (/$/...) and MatGUI for queries.
// Plain JavaScript, no build step. Everything the page shows comes from the server's own JSON.
"use strict";

(function () {
  const MATGUI = document.currentScript.dataset.matgui;
  const UI = "/$/ui/";
  const $ = (sel, root = document) => root.querySelector(sel);
  const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

  // ---------- storage (may be unavailable: private windows, blocked site data) ----------
  const store = {
    get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
    set(k, v) { try { v == null ? localStorage.removeItem(k) : localStorage.setItem(k, v); } catch (e) { /* ignore */ } },
  };
  const TOKEN_KEY = "tripletrail-admin-token";
  // "View as" (#205): an admin runs the query page as another caller, for this tab only.
  const VIEW_AS_KEY = "tripletrail-view-as";
  const viewAs = {
    get() { try { return JSON.parse(sessionStorage.getItem(VIEW_AS_KEY)); } catch (e) { return null; } },
    set(v) {
      try { v ? sessionStorage.setItem(VIEW_AS_KEY, JSON.stringify(v)) : sessionStorage.removeItem(VIEW_AS_KEY); } catch (e) { /* ignore */ }
      viewAsBanner();
    },
    /** The request headers of the view: the admin's token plus whom to run as. */
    headers() {
      const v = viewAs.get();
      const h = {};
      const token = store.get(TOKEN_KEY);
      if (token) h.Authorization = "Bearer " + token;
      if (!v) return h;
      h["X-TripleTrail-View-As"] = v.agent || "anonymous";
      if (v.groups && v.groups.length) h["X-TripleTrail-View-As-Groups"] = v.groups.join(",");
      return h;
    },
  };
  function viewAsBanner() {
    const banner = document.getElementById("view-as-banner");
    if (!banner) return;
    const v = viewAs.get();
    banner.hidden = !v;
    if (!v) { banner.replaceChildren(); return; }
    const who = v.agent ? v.agent + (v.groups && v.groups.length ? " (groups " + v.groups.join(", ") + ")" : " (no groups)") : "an anonymous caller";
    banner.replaceChildren(
      document.createTextNode("Queries on this page run as " + who + ": you see what they see. "),
      Object.assign(document.createElement("button"), { type: "button", className: "btn small", textContent: "Stop viewing as", onclick: () => viewAs.set(null) }));
  }
  const THEME_KEY = "tripletrail-theme";

  // ---------- formatting ----------
  const nf = new Intl.NumberFormat();
  const fmtNum = (n) => (n == null ? "–" : nf.format(n));
  function fmtBytes(n) {
    if (n == null) return "–";
    const u = ["B", "KB", "MB", "GB", "TB"];
    let i = 0;
    while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
    return (i ? n.toFixed(n < 10 ? 1 : 0) : n) + " " + u[i];
  }
  function fmtTime(s) {
    if (!s) return "–";
    const d = new Date(s);
    return isNaN(d) ? s : d.toLocaleString();
  }
  function fmtDuration(sec) {
    if (sec == null) return "–";
    const d = Math.floor(sec / 86400), h = Math.floor(sec / 3600) % 24, m = Math.floor(sec / 60) % 60;
    return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : m ? `${m}m ${sec % 60}s` : `${sec}s`;
  }
  function el(tag, attrs = {}, ...children) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") e.className = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v === true ? "" : v);
    }
    for (const c of children.flat()) if (c != null) e.append(c.nodeType ? c : String(c));
    return e;
  }
  function stats(target, items) {
    target.replaceChildren(...items.map(([label, value, extra]) =>
      el("div", {}, el("dt", {}, label), el("dd", {}, el("strong", {}, value), extra ? el("span", { class: "muted small" }, extra) : null))));
  }
  function kv(target, items) {
    target.replaceChildren(...items.filter(([, v]) => v != null && v !== "").flatMap(([k, v]) => [el("dt", {}, k), el("dd", {}, v)]));
  }
  function toast(message, kind = "") {
    const t = el("div", { class: "toast " + kind, role: kind === "bad" ? "alert" : "status" }, message);
    $("#toasts").append(t);
    setTimeout(() => t.remove(), kind === "bad" ? 8000 : 4000);
  }

  // ---------- API ----------
  class ApiError extends Error {
    constructor(status, message) { super(message); this.status = status; }
  }
  async function api(method, path, { params, form, body, type } = {}) {
    const url = new URL(path, location.origin);
    if (params) for (const [k, v] of Object.entries(params)) if (v != null && v !== "") url.searchParams.set(k, v);
    // The UI's own requests: a workspace's history leaves them out.
    const headers = { "X-TripleTrail-Client": "ui" };
    const token = store.get(TOKEN_KEY);
    if (token) headers.Authorization = "Bearer " + token;
    let payload = body;
    if (form) { payload = new URLSearchParams(form); headers["Content-Type"] = "application/x-www-form-urlencoded"; }
    if (type) headers["Content-Type"] = type;
    const r = await fetch(url, { method, headers, body: payload });
    const text = await r.text();
    if (!r.ok) {
      if (r.status === 401) openToken("That needs the admin token.");
      throw new ApiError(r.status, text.trim() || r.statusText);
    }
    try { return JSON.parse(text); } catch (e) { return text; }
  }
  const fail = (e) => toast(e.message || String(e), "bad");

  // ---------- theme: auto, light or dark, shared with MatGUI's data-theme ----------
  const media = matchMedia("(prefers-color-scheme: dark)");
  let yasgui = null;
  function applyTheme() {
    const pref = store.get(THEME_KEY) || "auto";
    const theme = pref === "auto" ? (media.matches ? "dark" : "light") : pref;
    document.documentElement.setAttribute("data-theme", theme);
    if (yasgui && yasgui.themeManager && yasgui.themeManager.getTheme() !== theme) {
      if (pref === "auto") yasgui.themeManager.followSystem(); else yasgui.themeManager.setTheme(theme);
    }
    const b = $("#theme-btn");
    b.title = "Theme: " + pref;
    b.setAttribute("aria-label", "Theme: " + pref + " (click to change)");
  }
  $("#theme-btn").addEventListener("click", () => {
    const order = ["auto", "light", "dark"];
    const next = order[(order.indexOf(store.get(THEME_KEY) || "auto") + 1) % 3];
    store.set(THEME_KEY, next === "auto" ? null : next);
    applyTheme();
    toast("Theme: " + next);
  });
  media.addEventListener("change", applyTheme);
  // MatGUI's own theme switch writes data-theme: remember that choice too.
  new MutationObserver(() => {
    const t = document.documentElement.getAttribute("data-theme");
    const pref = store.get(THEME_KEY) || "auto";
    const expected = pref === "auto" ? (media.matches ? "dark" : "light") : pref;
    if (t && t !== expected) store.set(THEME_KEY, t);
  }).observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] });

  // ---------- dialogs ----------
  for (const d of $$("dialog")) {
    d.addEventListener("click", (e) => {
      if (e.target === d || e.target.closest("[data-close]")) d.close();
    });
  }
  function confirmDialog(title, text, extra) {
    const d = $("#confirm-dialog");
    $("#confirm-title").textContent = title;
    $("#confirm-text").textContent = text;
    $("#confirm-extra").hidden = !extra;
    $("#confirm-check").checked = false;
    if (extra) $("#confirm-extra span").textContent = extra;
    d.returnValue = "";
    d.showModal();
    return new Promise((resolve) => d.addEventListener("close", () =>
      resolve(d.returnValue === "ok" ? { ok: true, checked: $("#confirm-check").checked } : { ok: false }), { once: true }));
  }

  // ---------- admin token ----------
  let serverInfo = null;
  function openToken(why) {
    const d = $("#token-dialog");
    if (d.open) return;
    $("#token-why").textContent = (why ? why + " " : "") + "Changes to datasets need the server's admin token. It is kept in this browser only.";
    $("#token-form").token.value = store.get(TOKEN_KEY) || "";
    d.showModal();
  }
  function tokenState() {
    const has = !!store.get(TOKEN_KEY);
    const signedIn = sso && sso.signedIn;
    const b = $("#token-btn");
    b.classList.toggle("on", has || !!signedIn);
    b.title = signedIn ? "Signed in as " + signedIn.agent : serverInfo && !serverInfo.adminToken ? "No admin token is configured on this server" : has ? "Admin token saved" : "Admin token";
  }
  $("#token-btn").addEventListener("click", () => openToken());
  $("#token-form").addEventListener("submit", () => {
    store.set(TOKEN_KEY, $("#token-form").token.value.trim() || null);
    tokenState();
  });
  $("#token-clear").addEventListener("click", () => { store.set(TOKEN_KEY, null); tokenState(); $("#token-dialog").close(); });

  // ---------- single sign-on (#198, TripleTrail Enterprise) ----------
  // A browser signs in with the organisation's identity provider; the session is an
  // HttpOnly cookie the page never sees. Without browser sign-in /$/sso is a 404.
  let sso = null;
  async function loadSso() {
    try {
      const r = await fetch("/$/sso", { headers: { Accept: "application/json" } });
      sso = r.ok ? await r.json() : null;
    } catch (e) { sso = null; }
    const box = $("#sso-box");
    box.hidden = !sso;
    if (!sso) return;
    const here = location.pathname + location.search + location.hash;
    const who = $("#sso-who"), buttons = $("#sso-buttons");
    if (sso.signedIn) {
      const g = sso.signedIn.groups || [];
      who.textContent = "Signed in as " + sso.signedIn.agent + (g.length ? " (groups " + g.join(", ") + ")" : "") + ".";
      buttons.replaceChildren(el("button", { class: "btn", type: "button", onclick: async () => {
        await fetch("/$/sso/logout", { method: "POST" }).catch(() => {});
        location.reload();
      } }, "Sign out"));
    } else {
      who.textContent = "Sign in with your organisation's account:";
      buttons.replaceChildren(...sso.signIn.map((c) => el("a", { class: "btn primary", href: c.login + "&return=" + encodeURIComponent(here) }, c.name)));
    }
    tokenState();
  }

  // ---------- readiness ----------
  async function pollReady() {
    let state = "bad", text = "unreachable";
    try {
      const r = await fetch("/$/ready");
      const j = await r.json().catch(() => ({}));
      if (r.ok) { state = "ok"; text = "ready"; }
      else if (j.stopping) { state = "bad"; text = "stopping"; }
      else if (j.starting) { state = "warn"; text = "starting"; }
      else { state = "warn"; text = "building indexes"; }
    } catch (e) { /* unreachable */ }
    $("#status-dot").className = "dot " + state;
    $("#status-text").textContent = text;
  }
  $("#status-btn").addEventListener("click", () => go("server"));

  // ---------- datasets ----------
  let datasets = [];
  let allStats = {};
  const byName = (n) => datasets.find((d) => d.name === n);
  const realDatasets = () => datasets.filter((d) => !d.alias);

  async function loadDatasets() {
    const [list, s, info] = await Promise.all([api("GET", "/$/datasets"), api("GET", "/$/stats"), api("GET", "/$/server")]);
    datasets = list.datasets;
    allStats = s.datasets || {};
    serverInfo = info;
    tokenState();
  }

  function modeChip(d) {
    if (d.alias) return el("span", { class: "chip alias" }, "alias");
    return el("span", { class: "chip " + (d.mode === "read-only" ? "ro" : "") }, d.mode);
  }
  function readyDot(d) {
    if (d.alias) return el("span", { class: "dot " + (d["ds.state"] ? "ok" : "bad"), title: d["ds.state"] ? "target attached" : "target missing" });
    return el("span", { class: "dot " + (d.spatialIndexReady ? "ok" : "warn"), title: d.spatialIndexReady ? "spatial index ready" : "building the spatial index" });
  }

  async function renderDatasets() {
    try { await loadDatasets(); } catch (e) { fail(e); return; }
    const real = realDatasets();
    const total = (k) => real.reduce((a, d) => a + ((allStats["/" + d.name] || {})[k] || 0), 0);
    stats($("#overview"), [
      ["Datasets", fmtNum(real.length), datasets.length > real.length ? `+ ${datasets.length - real.length} aliases` : null],
      ["Triples", fmtNum(total("triples"))],
      ["On disk", fmtBytes(total("sizeOnDisk"))],
      ["Memory-mapped", fmtBytes(total("mappedBytes"))],
      ["Requests", fmtNum(total("Requests")), total("RequestsBad") ? `${fmtNum(total("RequestsBad"))} failed` : null],
      ["Uptime", fmtDuration(serverInfo.uptime)],
    ]);
    const tbody = $("#datasets-table tbody");
    tbody.replaceChildren(...datasets.map((d) => {
      const s = allStats["/" + (d.alias ? d.target : d.name)] || {};
      return el("tr", { class: "clickable", onclick: (e) => { if (!e.target.closest("button")) go("datasets/" + encodeURIComponent(d.name)); } },
        el("td", {}, el("span", { class: "name" }, readyDot(d), d.name, ...(d.aliases || []).map((a) => el("span", { class: "chip alias", title: "alias" }, a)))),
        el("td", {}, modeChip(d)),
        el("td", { class: "num" }, d.alias ? "" : fmtNum(s.triples)),
        el("td", { class: "num" }, d.alias ? "" : fmtBytes(s.sizeOnDisk)),
        el("td", { class: "num" }, d.alias ? "" : fmtNum(s.Requests)),
        el("td", { class: "path", title: d.alias ? d.target : d.location || "" }, d.alias ? "→ " + d.target : d.location || el("span", { class: "muted" }, "in memory")),
        el("td", { class: "row-actions" },
          el("button", { class: "btn small", type: "button", onclick: () => go("query/" + encodeURIComponent(d.name)) }, "Query"),
          el("button", { class: "btn small", type: "button", onclick: () => go("map/" + encodeURIComponent(d.name)) }, "Map")));
    }));
    $("#datasets-empty").hidden = datasets.length > 0;
  }
  $("#refresh-btn").addEventListener("click", renderDatasets);

  // ---------- one dataset ----------
  let current = null;
  async function openDataset(name) {
    const d = $("#ds-dialog");
    try { await loadDatasets(); } catch (e) { fail(e); return; }
    const ds = byName(name);
    if (!ds) { toast("No dataset " + name, "bad"); go("datasets", true); return; }
    current = ds;
    $("#ds-title").replaceChildren(readyDot(ds), ds.name, modeChip(ds));
    const actions = [];
    const btn = (label, fn, cls = "") => el("button", { class: "btn " + cls, type: "button", onclick: fn }, label);
    actions.push(btn("Query", () => go("query/" + encodeURIComponent(ds.name)), "primary"));
    actions.push(btn("Map", () => go("map/" + encodeURIComponent(ds.name))));
    if (ds.alias) {
      const s = allStats["/" + ds.target] || {};
      stats($("#ds-stats"), [["Points at", ds.target], ["Triples", fmtNum(s.triples)], ["Previous", ds.previous || "–"]]);
      kv($("#ds-kv"), [["Since", fmtTime(ds.since)], ["Mode", ds.mode], ["Endpoint", location.origin + "/" + ds.name + "/sparql"]]);
      $("#swap-to").replaceChildren(...realDatasets().map((r) => el("option", { value: r.name, selected: r.name === ds.target }, r.name)));
      $("#rollback-btn").disabled = !ds.previous || !byName(ds.previous);
      $("#rollback-btn").title = ds.previous ? "Swap back to " + ds.previous : "Nothing to roll back to";
      actions.push(btn("Remove alias", () => detach(ds), "danger"));
    } else {
      const s = await api("GET", "/$/stats/" + encodeURIComponent(ds.name)).then((j) => j.datasets["/" + ds.name]).catch(() => ({}));
      stats($("#ds-stats"), [
        ["Triples", fmtNum(s.triples), s.graphs ? `${fmtNum(s.graphs)} graphs` : null],
        ["On disk", fmtBytes(s.sizeOnDisk)],
        ["Mapped", fmtBytes(s.mappedBytes)],
        ["Queries", fmtNum(s.queries)],
        ["Updates", fmtNum(s.updates)],
        ["Failed", fmtNum(s.RequestsBad)],
      ]);
      kv($("#ds-kv"), [
        ["Store", s.store], ["Location", s.location], ["Attached", fmtTime(s.attached)],
        ["Last write", s.lastWrite ? fmtTime(s.lastWrite) : null], ["Last swap", s.lastSwap ? fmtTime(s.lastSwap) : null],
        ["Aliases", (s.aliases || []).join(", ")], ["Spatial index", s.spatialIndexReady ? "ready" : "building"],
        ["Endpoint", location.origin + "/" + ds.name + "/sparql"],
      ]);
      actions.push(btn("Back up", () => backup(ds)));
      if (ds.store === "tripletrail" || ds.mode === "read-write") actions.push(btn("Snapshot…", () => snapshot(ds)));
      actions.push(btn("Detach…", () => detach(ds), "danger"));
    }
    $("#ds-actions").replaceChildren(...actions);
    $("#ds-swap").hidden = !ds.alias;
    $("#ds-upload").hidden = ds.alias || ds.mode !== "read-write";
    $("#upload-msg").textContent = "";
    $("#upload-progress").hidden = true;
    if (!d.open) d.showModal();
  }
  $("#ds-dialog").addEventListener("close", () => {
    if (/\/datasets\/./.test(location.pathname)) history.replaceState(null, "", UI + "datasets");
    current = null;
    if (page === "datasets") renderDatasets();
  });

  async function backup(ds) {
    try {
      const r = await api("POST", "/$/backup/" + encodeURIComponent(ds.name));
      toast(`Backup started: ${r.file}`, "ok");
      watchTask(r.taskId);
    } catch (e) { fail(e); }
  }
  async function watchTask(id) {
    for (let i = 0; i < 3600; i++) {
      await new Promise((r) => setTimeout(r, 1000));
      const t = await api("GET", "/$/tasks/" + id).catch(() => null);
      if (t && t.finished) {
        toast(t.success ? `${t.task} of ${t.dataset} done: ${fmtNum(t.quads)} quads` : `${t.task} of ${t.dataset} failed: ${t.message || ""}`, t.success ? "ok" : "bad");
        if (page === "tasks") renderTasks();
        return;
      }
    }
  }
  async function snapshot(ds) {
    const today = new Date().toISOString().slice(0, 10).replaceAll("-", "");
    const base = serverInfo && serverInfo.location ? serverInfo.location.replace(/\/$/, "") + "/" : "";
    const target = prompt("Directory to write the snapshot to (it must not exist yet):", `${base}${ds.name}_${today}`);
    if (!target) return;
    try {
      await api("POST", `/$/datasets/${encodeURIComponent(ds.name)}/snapshot`, { params: { location: target } });
      toast("Snapshot written to " + target + ". Attach it read-only from New dataset.", "ok");
    } catch (e) { fail(e); }
  }
  async function detach(ds) {
    const r = ds.alias
      ? await confirmDialog("Remove alias " + ds.name, `Requests to /${ds.name} will answer 404. ${ds.target} stays attached.`)
      : await confirmDialog("Detach " + ds.name, "The dataset stops being served once the requests running on it finish.", ds.location ? "Also delete its files on disk" : null);
    if (!r.ok) return;
    try {
      await api("DELETE", "/$/datasets/" + encodeURIComponent(ds.name), { params: { "delete-files": r.checked ? "true" : null } });
      toast((ds.alias ? "Removed " : "Detached ") + ds.name, "ok");
      $("#ds-dialog").close();
    } catch (e) { fail(e); }
  }
  async function swapTo(to, release) {
    if (!current) return;
    try {
      await api("POST", `/$/datasets/${encodeURIComponent(current.name)}/swap`, { params: { to, release: release ? "true" : null } });
      toast(`${current.name} now points at ${to}`, "ok");
      openDataset(current.name);
    } catch (e) { fail(e); }
  }
  $("#swap-btn").addEventListener("click", () => swapTo($("#swap-to").value, $("#swap-release").checked));
  $("#rollback-btn").addEventListener("click", () => current && current.previous && swapTo(current.previous, false));

  // Upload through the Graph Store Protocol, with progress (fetch has none for uploads).
  const EXT_TYPES = { ttl: "text/turtle", nt: "application/n-triples", nq: "application/n-quads", trig: "application/trig", rdf: "application/rdf+xml", owl: "application/rdf+xml", xml: "application/rdf+xml", jsonld: "application/ld+json" };
  $("#upload-btn").addEventListener("click", () => {
    const file = $("#upload-file").files[0];
    if (!current || !file) { $("#upload-msg").textContent = "Choose a file first."; return; }
    const ext = file.name.toLowerCase().split(".").pop();
    const type = EXT_TYPES[ext] || file.type;
    if (!type) { $("#upload-msg").textContent = "Unknown format: use .ttl, .nt, .nq, .trig, .rdf or .jsonld."; return; }
    const graph = $("#upload-graph").value.trim();
    const quads = type === "application/n-quads" || type === "application/trig";
    const url = new URL(`/${current.name}/data`, location.origin);
    if (graph) url.searchParams.set("graph", graph); else if (!quads) url.searchParams.set("default", "");
    const xhr = new XMLHttpRequest();
    xhr.open($("#upload-method").value, url);
    xhr.setRequestHeader("Content-Type", type);
    const token = store.get(TOKEN_KEY);
    if (token) xhr.setRequestHeader("Authorization", "Bearer " + token);
    const bar = $("#upload-progress");
    bar.hidden = false; bar.value = 0;
    const started = performance.now();
    xhr.upload.onprogress = (e) => { if (e.lengthComputable) bar.value = e.loaded / e.total; $("#upload-msg").textContent = `Sending ${fmtBytes(e.loaded)} of ${fmtBytes(e.total)}…`; };
    xhr.upload.onload = () => { $("#upload-msg").textContent = "Loading into the store…"; bar.removeAttribute("value"); };
    xhr.onload = () => {
      bar.hidden = true;
      const secs = ((performance.now() - started) / 1000).toFixed(1);
      if (xhr.status < 300) { $("#upload-msg").textContent = `Loaded ${file.name} in ${secs} s.`; toast("Loaded " + file.name, "ok"); openDataset(current.name); }
      else $("#upload-msg").textContent = `Failed (${xhr.status}): ${xhr.responseText}`;
    };
    xhr.onerror = () => { bar.hidden = true; $("#upload-msg").textContent = "The upload failed: the connection was lost."; };
    xhr.send(file);
  });

  // ---------- new dataset ----------
  const newForm = $("#new-form");
  function newKind() { return newForm.kind.value; }
  function showNewFields() {
    const k = newKind();
    for (const f of $$("[data-for]", newForm)) f.hidden = !f.dataset.for.split(" ").includes(k);
    newForm.location.required = k === "attach";
    $("#new-submit").textContent = k === "attach" ? "Attach" : "Create";
  }
  newForm.addEventListener("change", showNewFields);
  $("#new-btn").addEventListener("click", async () => {
    newForm.reset();
    $("#new-error").textContent = "";
    newForm["alias-of"].replaceChildren(...realDatasets().map((d) => el("option", { value: d.name }, d.name)));
    const backups = await api("GET", "/$/backups-list").catch(() => ({ backups: [] }));
    newForm.from.replaceChildren(el("option", { value: "" }, "(empty)"), ...backups.backups.map((b) => el("option", { value: b }, b)));
    showNewFields();
    $("#new-dialog").showModal();
  });
  newForm.addEventListener("submit", async (e) => {
    e.preventDefault();
    const k = newKind();
    const p = { name: newForm.name.value.trim() };
    if (k === "attach") { p.location = newForm.location.value.trim(); p.mode = newForm.mode.value; }
    else if (k === "alias") p["alias-of"] = newForm["alias-of"].value;
    else { p.type = k; if (newForm.from.value) p.from = newForm.from.value; }
    $("#new-submit").disabled = true;
    try {
      await api("POST", "/$/datasets", { params: p });
      $("#new-dialog").close();
      toast((k === "attach" ? "Attached " : "Created ") + p.name, "ok");
      renderDatasets();
    } catch (err) {
      $("#new-error").textContent = err.message;
    } finally {
      $("#new-submit").disabled = false;
    }
  });

  // ---------- query (MatGUI) ----------
  let matguiLoading = null;
  function loadMatgui() {
    if (window.Yasgui) return Promise.resolve();
    if (!matguiLoading) {
      matguiLoading = new Promise((resolve, reject) => {
        const s = el("script", { src: MATGUI + "yasgui.min.js" });
        s.onload = resolve;
        s.onerror = () => reject(new Error("MatGUI could not be loaded from " + MATGUI));
        document.head.append(s);
      });
    }
    return matguiLoading;
  }
  // A Workbench workspace is queried as "workspace:<id>" (from the Workbench's Open in MatGUI).
  const WS = "workspace:";
  const serviceOf = (name, service) => name.startsWith(WS)
    ? "/$/workspaces/" + encodeURIComponent(name.slice(WS.length)) + "/" + service
    : "/" + encodeURIComponent(name) + "/" + service;
  const endpointOf = (name) => location.origin + serviceOf(name, "sparql");
  let workspaceOpen = null; // the workspace last opened in the Query page
  async function renderQuery(name) {
    try { await loadDatasets(); } catch (e) { fail(e); }
    const sel = $("#query-ds");
    if (name && name.startsWith(WS)) workspaceOpen = name;
    const known = (n) => byName(n) || (n && n === workspaceOpen);
    if (!name || !known(name)) name = (known(sel.value) && sel.value) || (datasets[0] && datasets[0].name);
    sel.replaceChildren(...datasets.map((d) => el("option", { value: d.name, selected: d.name === name }, d.alias ? `${d.name} → ${d.target}` : d.name)),
      workspaceOpen ? el("option", { value: workspaceOpen, selected: workspaceOpen === name }, "Workbench workspace " + workspaceOpen.slice(WS.length, WS.length + 8)) : null);
    const ds = byName(name);
    $("#query-mode").textContent = ds ? (ds.alias ? "alias, " : "") + (ds.mode || "") : name && name.startsWith(WS) ? "workspace, read-only" : "";
    $("#query-mode").className = "chip" + ((ds && ds.mode === "read-only") || (name && name.startsWith(WS)) ? " ro" : "");
    if (!ds && !(name && name.startsWith(WS))) { $("#yasgui").replaceChildren(el("p", { class: "muted loading" }, "No dataset to query yet.")); return; }
    try { await loadMatgui(); } catch (e) { $("#yasgui").replaceChildren(el("p", { class: "error loading" }, e.message)); return; }
    if (!yasgui) {
      $("#yasgui").replaceChildren();
      yasgui = new window.Yasgui($("#yasgui"), {
        requestConfig: { endpoint: endpointOf(name), method: "POST", headers: () => viewAs.headers() },
        copyEndpointOnNewTab: true,
        persistenceId: "tripletrail-ui",
        endpointCatalogueOptions: { getData: () => datasets.map((d) => ({ endpoint: endpointOf(d.name) })) },
      });
      applyTheme();
      yasgui.on("tabSelect", () => setTimeout(syncSelectFromTab));
    }
    // A query another page opened here (the Search page's "Open in Query"), in a new tab.
    let pending = null;
    try { pending = sessionStorage.getItem("tripletrail-open-query"); sessionStorage.removeItem("tripletrail-open-query"); } catch (e) { /* none */ }
    if (pending) {
      yasgui.addTab(true);
      const t = yasgui.getTab();
      if (t) t.setQuery(pending);
    }
    const tab = yasgui.getTab();
    if (tab && tab.getEndpoint() !== endpointOf(name)) tab.setEndpoint(endpointOf(name));
  }
  function syncSelectFromTab() {
    const tab = yasgui && yasgui.getTab();
    if (!tab) return;
    const m = tab.getEndpoint().match(/\/([^/]+)\/sparql$/);
    if (m && byName(decodeURIComponent(m[1])) && $("#query-ds").value !== decodeURIComponent(m[1])) {
      history.replaceState(null, "", UI + "query/" + m[1] + location.hash);
      renderQuery(decodeURIComponent(m[1]));
    }
  }
  $("#query-ds").addEventListener("change", (e) => go("query/" + encodeURIComponent(e.target.value)));

  // Explain: the plan TripleTrail ran, with row counts and timings per step.
  $("#explain-btn").addEventListener("click", () => {
    const name = $("#query-ds").value;
    const tab = yasgui && yasgui.getTab();
    const query = tab ? tab.getQuery() : "";
    if (!name || !query) { toast("Write a query first."); return; }
    explain(serviceOf(name, "explain"), query);
  });
  async function explain(url, query) {
    const d = $("#explain-dialog");
    $("#explain-tree").replaceChildren(el("p", { class: "muted" }, "Running…"));
    $("#explain-stats").replaceChildren();
    d.showModal();
    try {
      const r = await api("POST", url, { body: query, type: "application/sparql-query" });
      const sp = r.spatial || {};
      stats($("#explain-stats"), [
        ["Rows", fmtNum(r.rows)],
        ["Spatial filters", fmtNum(sp.filters), sp.joins ? `${sp.joins} joins` : null],
        ["Candidates", fmtNum(sp.candidates), sp.skipped ? `${fmtNum(sp.skipped)} skipped` : null],
        ["Index lookup", sp.lookup_ms != null ? sp.lookup_ms.toFixed(1) + " ms" : "–"],
      ]);
      const plan = r.plan && (r.plan.plan || r.plan);
      const root = plan && (plan.name || plan.children) ? plan : null;
      const max = root ? maxDuration(root) : 0;
      $("#explain-tree").replaceChildren(
        root ? el("ul", {}, planNode(root, max)) : el("p", { class: "muted" }, "No plan."),
        sp.decisions && sp.decisions.length ? el("div", {}, el("h3", {}, "Spatial rewrites"), el("ul", {}, sp.decisions.map((x) => el("li", {}, x)))) : null,
        el("details", {}, el("summary", { class: "small muted" }, "Raw JSON"), el("pre", {}, JSON.stringify(r, null, 2))));
    } catch (e) {
      $("#explain-tree").replaceChildren(el("p", { class: "error" }, e.message));
    }
  }
  const durationOf = (n) => n["duration in seconds"] ?? n.duration ?? null;
  function maxDuration(n) { return Math.max(durationOf(n) || 0, ...(n.children || []).map(maxDuration)); }
  const shorten = (s) => (s.length > 160 ? s.slice(0, 157) + "…" : s);
  function planNode(n, max) {
    const dur = durationOf(n);
    const rows = n["number of results"] ?? n.rows;
    const label = n.name || "step";
    return el("li", {},
      el("span", { class: "node" },
        el("b", {}, label.split(" ")[0]),
        label.includes(" ") ? el("span", { class: "mono", title: label.length > 160 ? label : null }, shorten(label.slice(label.indexOf(" ") + 1))) : null,
        rows != null ? el("span", { class: "muted small" }, fmtNum(rows) + " rows") : null,
        dur != null ? el("span", { class: "muted small" }, (dur * 1000).toFixed(1) + " ms") : null,
        dur != null && max > 0 ? el("span", { class: "bar", style: `width:${Math.round((dur / max) * 80)}px` }) : null),
      n.children && n.children.length ? el("ul", {}, n.children.map((c) => planNode(c, max))) : null);
  }

  // ---------- tasks and backups ----------
  async function renderTasks() {
    try {
      const [tasks, backups, info] = await Promise.all([api("GET", "/$/tasks"), api("GET", "/$/backups-list").catch(() => ({ files: [] })), api("GET", "/$/server")]);
      const list = (Array.isArray(tasks) ? tasks : tasks.tasks || []).slice().reverse();
      $("#tasks-table tbody").replaceChildren(...list.map((t) => el("tr", {},
        el("td", {}, t.task), el("td", {}, t.dataset), el("td", {}, fmtTime(t.started)), el("td", {}, t.finished ? fmtTime(t.finished) : el("span", { class: "chip" }, "running")),
        el("td", { class: "num" }, fmtNum(t.quads)),
        el("td", {}, t.finished ? el("span", { class: "name" }, el("span", { class: "dot " + (t.success ? "ok" : "bad") }), t.success ? (t.file || "ok") : t.message || "failed") : ""))));
      $("#tasks-empty").hidden = list.length > 0;
      const files = (backups.files || []).slice().reverse();
      $("#backups-table tbody").replaceChildren(...files.map((f) => el("tr", {},
        el("td", { class: "path" }, f.name), el("td", { class: "num" }, fmtBytes(f.size)), el("td", {}, fmtTime(f.modified)),
        el("td", { class: "row-actions" }, el("button", { class: "btn small", type: "button", onclick: () => restore(f.name) }, "Restore…")))));
      $("#backups-empty").hidden = files.length > 0;
      $("#backup-dir").textContent = info.backupDir ? "in " + info.backupDir : "";
    } catch (e) { fail(e); }
  }
  $("#tasks-refresh").addEventListener("click", renderTasks);
  async function restore(file) {
    const guess = file.replace(/_\d{4}-\d{2}-\d{2}_\d{2}-\d{2}-\d{2}.*$/, "") + "_restored";
    const name = prompt(`Restore ${file} into a new dataset named:`, guess);
    if (!name) return;
    const onDisk = serverInfo && serverInfo.location;
    try {
      await api("POST", "/$/datasets", { params: { name, from: file, type: onDisk ? "disk" : "mem" } });
      toast(`Restored ${file} as ${name}`, "ok");
    } catch (e) { fail(e); }
  }

  // ---------- server ----------
  async function renderServer() {
    try {
      const [info, ready] = await Promise.all([api("GET", "/$/server"), fetch("/$/ready").then((r) => r.json()).catch(() => ({}))]);
      serverInfo = info;
      stats($("#server-stats"), [
        ["Version", info.version], ["Uptime", fmtDuration(info.uptime)], ["Datasets", fmtNum(info.datasets.length)], ["Running tasks", fmtNum(info.runningTasks)],
      ]);
      kv($("#server-kv"), [
        ["Started", fmtTime(info.startDateTime)],
        ["Ready", ready.ready ? "yes" : "no" + (ready.starting ? " (opening datasets)" : ready.stopping ? " (stopping)" : " (building spatial indexes)")],
        ["Query timeout", info.timeoutSeconds != null ? info.timeoutSeconds + " s" : "none"],
        ["Admin token", info.adminToken ? "required for changes" : "not configured: anyone who reaches the server can change datasets"],
        ["Dataset location", info.location], ["Backups", info.backupDir], ["MatGUI", new URL(MATGUI, location.href).href],
      ]);
      tokenState();
    } catch (e) { fail(e); }
  }

  // ---------- routing ----------
  let page = null;
  function route() {
    const [p = "datasets", arg] = location.pathname.slice(UI.length).split("/").filter(Boolean);
    const name = arg ? decodeURIComponent(arg) : null;
    page = ["datasets", "query", "map", "search", "explore", "tasks", "server", "workbench", "access", "shapes"].includes(p) ? p : "datasets";
    if (page !== "workbench" && document.body.dataset.page === "workbench" && window.TripleTrailWorkbench) window.TripleTrailWorkbench.leave();
    if (page !== "map" && window.TripleTrailMap) window.TripleTrailMap.leave();
    if (page !== "explore" && window.TripleTrailExplore) window.TripleTrailExplore.leave();
    document.body.dataset.page = page;
    if (!(page === "datasets" && name)) for (const d of $$("dialog[open]")) if (d.id === "ds-dialog") d.close();
    for (const s of $$(".page")) s.hidden = s.id !== "page-" + page;
    for (const a of $$(".tabs a")) {
      if (a.dataset.page === page) a.setAttribute("aria-current", "page"); else a.removeAttribute("aria-current");
    }
    if (page === "datasets") { renderDatasets(); if (name) openDataset(name); }
    else if (page === "query") renderQuery(name);
    else if (page === "map") {
      if (window.TripleTrailMap) window.TripleTrailMap.render(shared, name ? [encodeURIComponent(name)] : []);
      else $("#page-map").replaceChildren(el("p", { class: "error loading" }, "The map could not be loaded."));
    }
    else if (page === "search") {
      if (window.TripleTrailSearch) window.TripleTrailSearch.render(shared, name ? [encodeURIComponent(name)] : []);
      else $("#page-search").replaceChildren(el("p", { class: "error loading" }, "The search page could not be loaded."));
    }
    else if (page === "explore") {
      if (window.TripleTrailExplore) window.TripleTrailExplore.render(shared, name ? [encodeURIComponent(name)] : []);
      else $("#page-explore").replaceChildren(el("p", { class: "error loading" }, "The Explore page could not be loaded."));
    }
    else if (page === "tasks") renderTasks();
    else if (page === "workbench") {
      const args = location.pathname.slice(UI.length).split("/").filter(Boolean).slice(1);
      if (window.TripleTrailWorkbench) window.TripleTrailWorkbench.render(shared, args);
      else $("#page-workbench").replaceChildren(el("p", { class: "error loading" }, "The Workbench could not be loaded."));
    }
    else if (page === "shapes") {
      const args = location.pathname.slice(UI.length).split("/").filter(Boolean).slice(1);
      if (window.TripleTrailShapes) window.TripleTrailShapes.render(shared, args);
      else $("#page-shapes").replaceChildren(el("p", { class: "error loading" }, "The Shapes page could not be loaded."));
    }
    else if (page === "access") {
      const args = location.pathname.slice(UI.length).split("/").filter(Boolean).slice(1);
      if (window.TripleTrailAccess) window.TripleTrailAccess.render(shared, args);
      else $("#page-access").replaceChildren(el("p", { class: "error loading" }, "The access pages could not be loaded."));
    }
    else renderServer();
  }
  // What the Workbench (workbench.js) uses of this page.
  viewAsBanner();
  const shared = { api, el, toast, fail, go, store, viewAs, TOKEN_KEY, ApiError, openToken, confirmDialog, fmtNum, fmtBytes, fmtTime, explain: (url, q) => explain(url, q) };
  window.addEventListener("popstate", route);
  // MatGUI keeps its share links and map position in the URL's hash, so pages are paths.
  function go(path, replace) {
    const url = UI + path;
    if (url !== location.pathname) history[replace ? "replaceState" : "pushState"](null, "", url);
    route();
  }
  document.addEventListener("click", (e) => {
    const a = e.target.closest("a[data-route]");
    if (!a || e.ctrlKey || e.metaKey || e.shiftKey || e.button !== 0) return;
    e.preventDefault();
    go(a.getAttribute("href").slice(UI.length));
  });

  applyTheme();
  tokenState();
  loadSso();
  pollReady();
  setInterval(pollReady, 5000);
  route();
})();
