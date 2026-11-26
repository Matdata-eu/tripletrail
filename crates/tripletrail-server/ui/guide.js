// The tour ("How the TripleTrail server works"), About and What's new, as in rinf-atlas.
// The tour opens on the first visit and from the Help button. What's new reads whats-new.json
// (newest release first; add an entry with every release) and never opens by itself: a
// returning browser that has not seen the latest release gets a dot on the About button.
"use strict";

(function () {
  const REPO = "https://github.com/Matdata-eu/tripletrail";
  const VERSION = document.currentScript.dataset.version;
  const MATGUI = document.currentScript.dataset.matgui;
  // The server's own name, help page and e-mail (--site-name, --support-url, --contact-email; #262).
  const SITE = document.documentElement.dataset.siteName || "TripleTrail";
  const SUPPORT = document.documentElement.dataset.supportUrl || "";
  const CONTACT = document.documentElement.dataset.contactEmail || "";
  const TOUR_KEY = "tripletrail-tour";
  const TOUR_VERSION = "2026-11-11"; // bump when the pages change enough to show them again
  const NEWS_KEY = "tripletrail-whats-new-seen";

  const store = {
    get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch (e) { /* not kept */ } },
  };
  function el(tag, attrs = {}, ...children) {
    const e = document.createElement(tag);
    for (const [k, v] of Object.entries(attrs)) {
      if (v == null || v === false) continue;
      if (k === "class") e.className = v;
      else if (k.startsWith("on")) e.addEventListener(k.slice(2), v);
      else e.setAttribute(k, v === true ? "" : v);
    }
    for (const c of children.flat()) if (c != null) e.append(c);
    return e;
  }
  const link = (href, text) => el("a", { href, target: "_blank", rel: "noopener" }, text);
  const linkBtn = (text, onclick) => el("button", { class: "link", type: "button", onclick }, text);
  function dialog(cls, label) {
    const d = el("dialog", { class: "dlg " + cls, "aria-label": label });
    d.addEventListener("click", (e) => { if (e.target === d) d.close(); });
    document.body.append(d);
    return d;
  }
  const head = (title, d) => el("div", { class: "dlg-head" }, title,
    el("button", { class: "icon-btn close", type: "button", "aria-label": "Close", onclick: () => d.close() }, "✕"));

  // ---------- tour ----------
  // Drawings on a 64×48 canvas; the classes pick theme colours (app.css, .tour-art).
  const svg = (body) => `<svg viewBox="0 0 64 48" width="64" height="48" aria-hidden="true">${body}</svg>`;
  const ART = {
    datasets: svg('<ellipse class="ta-fill-soft" cx="24" cy="10" rx="16" ry="5"/>'
      + '<path class="ta-fill-soft" d="M8 10v26c0 3 7 5 16 5s16-2 16-5V10c0 3-7 5-16 5S8 13 8 10z"/>'
      + '<path class="ta-line" d="M8 19c0 3 7 5 16 5s16-2 16-5M8 28c0 3 7 5 16 5s16-2 16-5"/>'
      + '<circle class="ta-okf" cx="48" cy="32" r="10"/><path class="ta-on-line" d="m43.5 32 3 3 6-6"/>'),
    add: svg('<rect class="ta-card" x="4" y="6" width="40" height="36" rx="4"/>'
      + '<path class="ta-muted" d="M10 14h20M10 22h26M10 30h16"/>'
      + '<circle class="ta-fill" cx="48" cy="32" r="11"/><path class="ta-on-line" d="M48 26v12M42 32h12"/>'),
    mmap: svg('<rect class="ta-card" x="4" y="6" width="24" height="36" rx="3"/>'
      + '<path class="ta-muted" d="M9 14h14M9 21h14M9 28h14M9 35h10"/>'
      + '<path class="ta-line ta-dash" d="M30 17h8M30 31h8"/>'
      + '<rect class="ta-fill-soft" x="40" y="10" width="20" height="28" rx="3"/>'
      + '<path class="ta-line" d="M45 18h10M45 24h10M45 30h6"/>'
),
    query: svg('<rect class="ta-card" x="4" y="4" width="56" height="40" rx="4"/>'
      + '<path class="ta-tab" d="M10 12h10"/><path class="ta-muted" d="M24 12h8"/>'
      + '<path class="ta-line" d="M10 21h6"/><path class="ta-muted" d="M19 21h20M10 28h14M27 28h18M10 35h24"/>'
      + '<circle class="ta-fill" cx="50" cy="34" r="6"/><path class="ta-on-fill" d="m48 31 5 3-5 3z"/>'),
    views: svg('<rect class="ta-soft" x="2" y="4" width="28" height="18" rx="3"/>'
      + '<path class="ta-muted" d="M6 10h20M6 16h14"/>'
      + '<rect class="ta-soft" x="34" y="4" width="28" height="18" rx="3"/><path class="ta-rail" d="M37 19c6-2 8-9 14-10s6-3 8-4"/>'
      + '<rect class="ta-soft" x="2" y="26" width="28" height="18" rx="3"/>'
      + '<rect class="ta-b1f" x="7" y="34" width="4" height="7" rx="1"/><rect class="ta-b2f" x="14" y="30" width="4" height="11" rx="1"/><rect class="ta-b3f" x="21" y="36" width="4" height="5" rx="1"/>'
      + '<rect class="ta-soft" x="34" y="26" width="28" height="18" rx="3"/>'
      + '<path class="ta-line" d="M41 35l8-4m0 0 7 6"/><circle class="ta-fill" cx="41" cy="35" r="2.5"/><circle class="ta-fill" cx="49" cy="31" r="2.5"/><circle class="ta-fill" cx="56" cy="37" r="2.5"/>'),
    explain: svg('<rect class="ta-fill-soft" x="4" y="4" width="22" height="9" rx="3"/>'
      + '<path class="ta-line" d="M10 13v8m0 0h8M10 21v14m0 0h8"/>'
      + '<rect class="ta-soft" x="20" y="17" width="20" height="9" rx="3"/><rect class="ta-soft" x="20" y="31" width="20" height="9" rx="3"/>'
      + '<path class="ta-bar" d="M44 21.5h14"/><path class="ta-bar" d="M44 35.5h5"/>'),
    alias: svg('<rect class="ta-fill" x="4" y="18" width="18" height="12" rx="6"/><text class="ta-on" x="13" y="26.5" text-anchor="middle">net</text>'
      + '<path class="ta-line" d="M22 24c8 0 8-12 16-12"/><path class="ta-muted ta-dash" d="M22 24c8 0 8 12 16 12"/>'
      + '<rect class="ta-card" x="38" y="6" width="22" height="12" rx="3"/><path class="ta-line" d="M42 12h14"/>'
      + '<rect class="ta-card" x="38" y="30" width="22" height="12" rx="3"/><path class="ta-muted" d="M42 36h14"/>'),
    swap: svg('<path class="ta-line" d="M10 16h40l-7-7"/><path class="ta-line" d="M54 32H14l7 7"/>'
      + '<circle class="ta-okf" cx="10" cy="16" r="4"/><circle class="ta-sig" cx="54" cy="32" r="4"/>'),
    backup: svg('<path class="ta-fill-soft" d="M8 8h28l8 8v26H8z"/><path class="ta-line" d="M36 8v8h8"/>'
      + '<path class="ta-muted" d="M14 22h22M14 28h18M14 34h22"/>'
      + '<circle class="ta-fill" cx="50" cy="34" r="10"/><path class="ta-on-line" d="M50 28v11m-4.5-4.5L50 39l4.5-4.5"/>'),
    token: svg('<rect class="ta-soft" x="4" y="16" width="56" height="16" rx="8"/>'
      + '<circle class="ta-line" cx="16" cy="24" r="4"/><path class="ta-line" d="M20 24h10m-4 0v4"/>'
      + '<circle class="ta-fill" cx="38" cy="24" r="2"/><circle class="ta-fill" cx="45" cy="24" r="2"/><circle class="ta-fill" cx="52" cy="24" r="2"/>'),
    ready: svg('<rect class="ta-soft" x="4" y="14" width="56" height="20" rx="10"/>'
      + '<circle class="ta-okf" cx="16" cy="24" r="4.5"/><path class="ta-muted" d="M26 24h26"/>'),
    server: svg('<rect class="ta-card" x="6" y="6" width="52" height="16" rx="3"/><rect class="ta-card" x="6" y="26" width="52" height="16" rx="3"/>'
      + '<circle class="ta-okf" cx="14" cy="14" r="2.5"/><circle class="ta-okf" cx="14" cy="34" r="2.5"/>'
      + '<path class="ta-muted" d="M22 14h16M22 34h22"/><path class="ta-line" d="M46 14h6"/>'),
  };

  const PAGES = [
    { title: "Manage your datasets", items: [
      { art: ART.datasets, title: "Datasets",
        text: "Datasets lists every dataset and alias the server serves, with its size, requests and whether it is ready. Click one for its statistics and actions." },
      { art: ART.add, title: "New dataset",
        text: "Create an empty dataset in memory or on disk, attach a dataset directory, or restore one from a backup." },
      { art: ART.mmap, title: "Read-only releases",
        text: "Attach a published release read-only: its files are memory-mapped, not loaded, so it opens at once and shares memory with the page cache." },
    ] },
    { title: "Query", items: [
      { art: ART.query, title: "MatGUI",
        text: "Query opens MatGUI on the dataset you pick. Its tabs, prefixes and history are kept in this browser." },
      { art: ART.views, title: "Table, map, chart and graph",
        text: "Results show as a table, and geometries on a map. Charts and a graph view are one click away." },
      { art: ART.explain, title: "Explain",
        text: "Explain runs the query and shows TripleTrail's plan with row counts and timings, and where the spatial and range indexes were used." },
    ] },
    { title: "Develop shapes and rules", items: [
      { art: ART.add, title: "Workbench",
        text: "A workspace is a folder of data, shapes, rules and queries with a scratch store of its own. Start from an example, or copy a sample of a dataset into it." },
      { art: ART.explain, title: "Validate and infer",
        text: "Validate groups the results by shape and constraint and marks what is new or fixed since the last run. Rules lists every inferred triple with its rule, focus node and pass." },
      { art: ART.query, title: "Why is this triple here?",
        text: "Click a triple anywhere in the Workbench: the why? panel shows the rule, focus node and bindings that made it. The datasets are never changed." },
    ] },
    { title: "Release without downtime", items: [
      { art: ART.alias, title: "Aliases",
        text: "Clients query a stable name, such as network, that points at a dataset version, such as network_20261107." },
      { art: ART.swap, title: "Swap and roll back",
        text: "Point the alias at the new version: new requests go there at once, running ones finish on the old one. Roll back the same way." },
      { art: ART.backup, title: "Backups and snapshots",
        text: "Back up a dataset to a compressed N-Quads file, or snapshot it as a new dataset directory. Tasks & backups shows how they went." },
    ] },
    { title: "Run the server", items: [
      { art: ART.token, title: "Admin token",
        text: "Changes need the server's admin token. You are asked for it when needed, and it is kept in this browser only." },
      { art: ART.ready, title: "Readiness",
        text: "The dot in the top bar shows whether every dataset is open and indexed. Load balancers ask the same at /$/ready." },
      { art: ART.server, title: "Server",
        text: "Server shows the version, uptime and settings. The same endpoints and admin API as Fuseki, so existing clients keep working." },
    ] },
  ];

  let tour = null, page = 0, goTo = () => {}, tourSeenNow = false;
  const tourSeen = () => tourSeenNow || (store.get(TOUR_KEY) || "") >= TOUR_VERSION;

  function buildTour() {
    const d = dialog("tour-dialog", "How the TripleTrail server works");
    const heading = el("h2");
    const step = el("span", { class: "tour-step muted" });
    const body = el("div", { class: "tour-body", "aria-live": "polite" });
    const dots = el("div", { class: "tour-dots" });
    const back = el("button", { class: "btn", type: "button", onclick: () => goTo(page - 1) }, "Back");
    const next = el("button", { class: "btn primary tour-next", type: "button",
      onclick: () => (page < PAGES.length - 1 ? goTo(page + 1) : d.close()) }, "Next");
    const skip = el("button", { class: "link tour-skip", type: "button", onclick: () => d.close() }, "Skip the tour");
    goTo = (i) => {
      page = Math.max(0, Math.min(PAGES.length - 1, i));
      const p = PAGES[page];
      heading.textContent = p.title;
      step.textContent = `${page + 1} of ${PAGES.length}`;
      body.replaceChildren(...p.items.map((it) => {
        const art = el("div", { class: "tour-art" });
        art.innerHTML = it.art;
        return el("div", { class: "tour-item" }, art, el("div", {}, el("h3", {}, it.title), el("p", {}, it.text)));
      }));
      dots.replaceChildren(...PAGES.map((pg, j) => el("button", {
        class: "tour-dot", type: "button", "aria-current": j === page ? "step" : null,
        "aria-label": `Page ${j + 1}: ${pg.title}`, onclick: () => goTo(j),
      })));
      back.hidden = page === 0;
      skip.hidden = page === PAGES.length - 1;
      next.textContent = page < PAGES.length - 1 ? "Next" : "Start";
    };
    d.addEventListener("close", () => { tourSeenNow = true; store.set(TOUR_KEY, TOUR_VERSION); });
    d.addEventListener("keydown", (e) => {
      if (e.target.closest("input, select, textarea")) return;
      if (e.key === "ArrowRight") { goTo(page + 1); e.preventDefault(); }
      if (e.key === "ArrowLeft") { goTo(page - 1); e.preventDefault(); }
    });
    d.append(
      head(el("div", { class: "tour-title" }, el("span", { class: "tour-kicker" }, "How the TripleTrail server works"), heading), d),
      body,
      el("div", { class: "tour-foot" },
        el("div", { class: "tour-nav-left" }, dots, step),
        el("div", { class: "tour-nav" }, skip, back, next)));
    return d;
  }
  function openTour() {
    if (!tour) tour = buildTour();
    goTo(0);
    tour.showModal();
    tour.querySelector(".tour-next").focus();
  }

  // ---------- what's new ----------
  let releases = [];
  const latest = () => (releases[0] && releases[0].version) || "";
  function newer(a, b) {
    const pa = a.split(".").map(Number), pb = b.split(".").map(Number);
    for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
      if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) > (pb[i] || 0);
    }
    return false;
  }
  function showDot(on) {
    const b = document.getElementById("about-btn");
    b.classList.toggle("has-news", on);
    const label = on ? `About, credits and feedback. New in version ${latest()}` : "About, credits and feedback";
    b.title = label;
    b.setAttribute("aria-label", label);
  }
  function markNewsSeen() { store.set(NEWS_KEY, latest()); showDot(false); }
  const GROUPS = [["new", "New"], ["improved", "Improved"], ["fixed", "Fixed"]];
  const dateFmt = new Intl.DateTimeFormat("en-GB", { day: "numeric", month: "short", year: "numeric", timeZone: "UTC" });

  let news = null;
  function openWhatsNew() {
    const before = store.get(NEWS_KEY);
    if (news) news.remove();
    news = dialog("about-dialog wn-dialog", "What's new");
    news.append(head(el("h2", {}, "What's new in the TripleTrail server"), news),
      el("div", { class: "dlg-body about-body" }, releases.length ? releases.map((r) =>
        el("section", { class: "wn-release" },
          el("h3", {}, `Version ${r.version}`,
            el("span", { class: "muted wn-date" }, dateFmt.format(new Date(r.date + "T00:00:00Z"))),
            before != null && newer(r.version, before) ? el("span", { class: "wn-unseen" }, "New for you") : null),
          GROUPS.filter(([k]) => r[k] && r[k].length).map(([k, title]) =>
            el("div", { class: "wn-group wn-" + k }, el("span", { class: "wn-tag" }, title),
              el("ul", {}, r[k].map((item) => el("li", {}, item)))))))
        : el("p", { class: "muted" }, "The release notes could not be loaded.")));
    news.showModal();
    markNewsSeen();
  }
  const whatsNewLink = (from) => linkBtn(`What's new in version ${latest() || VERSION}`, () => { from.close(); openWhatsNew(); });

  // ---------- about ----------
  let about = null;
  function openAbout() {
    if (about) about.remove();
    about = dialog("about-dialog", "About the TripleTrail server");
    const d = about;
    d.append(head(el("h2", {}, "About the TripleTrail server"), d),
      el("div", { class: "dlg-body about-body" },
        el("p", {}, "tripletrail is an RDF triple store on the ", link("https://github.com/oxigraph/oxigraph", "Oxigraph"),
          " crates with GeoSPARQL 1.1, the PostGIS ST_* functions and pgRouting, built to be fast. ",
          "This server answers the SPARQL 1.1 Protocol and the Graph Store Protocol with Fuseki's URLs and admin API, ",
          "so it can replace Fuseki without changing clients."),
        el("p", {}, "New here? Take the ", linkBtn("short tour", () => { d.close(); openTour(); }),
          ", or read the ", link(`${REPO}/blob/main/docs/server.md`, "server guide"), "."),
        el("p", {}, whatsNewLink(d), ": what is new, improved and fixed in each release."),
        el("h3", {}, "Feedback"),
        // --support-url and --contact-email (#262): this server's own help first.
        SUPPORT || CONTACT
          ? el("p", {}, "Found a bug, missing a feature or have a question? ",
            ...(SUPPORT ? ["Ask ", link(SUPPORT, SITE), CONTACT ? " or write to " : "."] : ["Write to "]),
            ...(CONTACT ? [link(`mailto:${CONTACT}`, CONTACT), "."] : []),
            " TripleTrail's source code and documentation: ", link(REPO, REPO.replace(/^https:\/\//, "")), ".")
          : el("p", {}, "Found a bug, missing a feature or have a question? Open an issue on ",
            link(`${REPO}/issues/new`, "GitHub"), ". Source code and documentation: ", link(REPO, REPO.replace(/^https:\/\//, "")), "."),
        el("h3", {}, "Support the project"),
        el("p", {}, "tripletrail and the tools next to it, such as ", link("https://github.com/Matdata-eu/Matgui", "MatGUI"),
          " and ", link("https://github.com/Matdata-eu/rinf-atlas", "RINF Atlas"),
          ", are open source. Need a feature built, a hosted instance or help with integration? ",
          link("https://matdata.eu", "Matdata"), " offers this as paid work, which also funds these projects."),
        el("h3", {}, "Credits"),
        el("ul", { class: "credits" },
          el("li", {}, "Built on ", link("https://github.com/oxigraph/oxigraph", "Oxigraph"), " (MIT or Apache-2.0), ",
            link("https://georust.org/", "GeoRust"), "'s geo and rstar, and ", link("https://github.com/tiny-http/tiny-http", "tiny-http")),
          el("li", {}, "Query editor: ", link("https://github.com/Matdata-eu/Matgui", "MatGUI"), " (MIT, ",
            link(MATGUI + "LICENSE", "licence"),
            "), with ", link("https://leafletjs.com/", "Leaflet"), " and ", link("https://www.openstreetmap.org/copyright", "© OpenStreetMap contributors"), " on its map"),
          el("li", {}, "Made by ", link("https://matdata.eu", "Matdata"))),
        el("p", { class: "muted small" }, `Version ${VERSION}`)));
    d.showModal();
  }

  // ---------- start ----------
  document.getElementById("help-btn").addEventListener("click", openTour);
  document.getElementById("about-btn").addEventListener("click", openAbout);
  fetch("/$/ui/whats-new.json").then((r) => (r.ok ? r.json() : [])).catch(() => []).then((list) => {
    releases = Array.isArray(list) ? list : [];
    if (!latest()) return;
    if (store.get(NEWS_KEY) == null) {
      // A first visit only remembers the latest release; a browser that used the UI before
      // (it keeps the theme or the token) gets the dot. The tour's key ships with this
      // dialog, so it does not count.
      const returning = store.get("tripletrail-theme") != null || store.get("tripletrail-admin-token") != null;
      if (!returning) { markNewsSeen(); return; }
    }
    showDot(newer(latest(), store.get(NEWS_KEY) || "0"));
  });
  if (!tourSeen()) openTour();
})();
