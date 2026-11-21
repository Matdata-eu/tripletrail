// The shapes editor (#145): browse a dataset's shapes by class, change the settings the
// readers read (names, order, groups, what each reader shows) in a table, edit the Turtle of
// the dataset's own files with live checks, and roll back to a kept version. Saving goes
// through the shapes admin API (/$/datasets/{ds}/shapes/…), which checks every version before
// it is kept. Plain JavaScript like app.js, which calls TripleTrailShapes.render(ui, args).
//
// /$/ui/shapes             the first dataset
// /$/ui/shapes/{dataset}   that dataset's shapes
"use strict";

window.TripleTrailShapes = (function () {
  const READERS = ["validation", "map", "describe", "search", "ogc-features", "rest-api", "graphql", "ai", "forms"];
  let ui = null;
  let state = null;
  const el = (...a) => ui.el(...a);
  const page = () => document.getElementById("page-shapes");
  const fill = (node, ...children) => node.replaceChildren(...children.flat().filter((c) => c != null && c !== false));
  const base = (ds) => "/$/datasets/" + encodeURIComponent(ds) + "/shapes";
  const short = (iri) => (iri || "").replace(/^.*[/#:]/, "") || iri;
  const title = (s) => s.label || s.name || (s.classes && s.classes[0] ? short(s.classes[0]) : short(s.iri)) || "(unnamed shape)";

  async function render(shared, args) {
    ui = shared;
    const list = await ui.api("GET", "/$/datasets").catch((e) => { ui.fail(e); return { datasets: [] }; });
    const names = list.datasets.map((d) => d.name).filter((n) => !n.startsWith("$"));
    const ds = args[0] ? decodeURIComponent(args[0]) : names[0];
    if (!ds) {
      fill(page(), el("h2", {}, "Shapes"), el("p", { class: "muted" }, "There is no dataset yet."));
      return;
    }
    // The link's shape (`?shape=IRI`), else the one picked before on this dataset.
    const linked = new URLSearchParams(location.search).get("shape");
    state = { ds, names, outline: null, selected: linked || (state && state.ds === ds ? state.selected : null), changes: new Map(), search: "", profile: "", newProfiles: [], formFor: null, form: state ? state.form : null };
    await load();
  }

  async function load() {
    try {
      state.outline = await ui.api("GET", base(state.ds) + "/outline");
      if (!state.form) state.form = await ui.api("GET", base(state.ds) + "/form").catch(() => null);
      // Reviewed changes (TripleTrail Enterprise): proposals are pull requests on the bound
      // repository. The open build answers 501, and the page then offers the export only.
      state.review = state.outline.bound ? await ui.api("GET", base(state.ds) + "/proposals").catch(() => null) : null;
    } catch (e) {
      fill(page(), el("h2", {}, "Shapes"), el("p", { class: "error" }, e.message));
      return;
    }
    const shapes = state.outline.shapes;
    if (!shapes.some((s) => s.iri === state.selected)) state.selected = shapes.length ? shapes[0].iri : null;
    state.changes.clear();
    state.newProfiles = [];
    if (state.profile && !profiles().some((p) => p.iri === state.profile)) state.profile = "";
    draw();
  }

  /** The profiles: those the shapes name, and those made on the page but not saved yet. */
  const profiles = () => [...(state.outline.profiles || []), ...state.newProfiles];
  const profileName = (iri) => { const p = profiles().find((x) => x.iri === iri); return p && p.label ? p.label : short(iri); };
  /** The edit's body: the changes and the new profiles. */
  const editBody = () => JSON.stringify({ changes: [...state.changes.values()], profiles: state.newProfiles });
  /** Whether a shape is in the profile shown: one of its profiles, or in none (so in all). */
  function inProfile(s) {
    if (!state.profile) return true;
    const v = state.changes.get(JSON.stringify([s.iri, null, "profiles", ""]));
    const list = v ? v.value : s.profiles || [];
    return !list.length || list.includes(state.profile);
  }

  /** The shape being shown. */
  const current = () => state.outline.shapes.find((s) => s.iri === state.selected);

  function header() {
    const o = state.outline;
    const picker = el("select", { "aria-label": "Dataset", onchange: (e) => ui.go("shapes/" + encodeURIComponent(e.target.value)) },
      state.names.map((n) => el("option", { value: n, selected: n === state.ds }, n)));
    const notes = [];
    if (o.bound) notes.push(el("p", { class: "access-warning", role: "note" }, "These shapes come from ", el("code", {}, o.bound),
      proposing() ? ": change them there. Here you can try changes and propose them as a pull request, or export them as Turtle files."
        : ": change them there. Here you can try changes and export them as Turtle files."));
    else if (!o.editable) notes.push(el("p", { class: "access-warning", role: "note" }, "This dataset can't keep shapes of its own (it is read-only, or in memory without --location): the shapes are shown, not changed."));
    return [
      el("div", { class: "shapes-head" },
        el("h2", {}, "Shapes"), picker,
        el("span", { class: "muted small" }, "In use: ", o.source),
        o.version ? el("button", { type: "button", class: "btn small", onclick: versions }, `Versions (${o.versions.length})`) : null,
        state.review ? el("button", { type: "button", class: "btn small", onclick: proposalList }, `Proposals (${state.review.proposals.length})`) : null,
        el("a", { class: "btn small", href: base(state.ds) + "?format=text/turtle", target: "_blank", rel: "noopener" }, "All shapes as Turtle")),
      ...notes,
    ];
  }

  function sidebar() {
    const q = state.search.toLowerCase();
    const shapes = state.outline.shapes.filter((s) => inProfile(s) && (!q || [title(s), s.iri, ...(s.classes || [])].some((t) => (t || "").toLowerCase().includes(q))));
    const items = shapes.map((s) => el("li", {},
      el("button", {
        type: "button", class: "shapes-item" + (s.iri === state.selected ? " active" : ""), "aria-current": s.iri === state.selected ? "true" : null,
        onclick: () => { if (leaveChanges()) { state.selected = s.iri; state.formFor = null; state.changes.clear(); draw(); } },
      },
      el("span", { class: "shapes-item-title" }, title(s)),
      el("span", { class: "shapes-badges" },
        el("span", { class: "shapes-layer " + s.layer, title: layerText(s) }, s.layer),
        s.overrides ? (s.refines
          ? el("span", { class: "shapes-layer overrides", title: "Changes the shape of a lower layer; the rest still comes from it" }, "refines")
          : el("span", { class: "shapes-layer overrides", title: "Replaces the shape of a lower layer" }, "overrides")) : null,
        s.instances != null ? el("span", { class: "muted small" }, ui.fmtNum(s.instances)) : null))));
    const search = el("input", { type: "search", placeholder: "Find a shape or class", "aria-label": "Find a shape or class", value: state.search,
      oninput: (e) => { state.search = e.target.value; const pos = e.target.selectionStart; draw(); const box = page().querySelector(".shapes-side input"); box.focus(); box.setSelectionRange(pos, pos); } });
    const profilePicker = profiles().length ? el("select", { "aria-label": "Profile", onchange: (e) => { state.profile = e.target.value; draw(); } },
      el("option", { value: "", selected: !state.profile }, "All profiles"),
      profiles().map((p) => el("option", { value: p.iri, selected: p.iri === state.profile, title: p.iri }, profileName(p.iri)))) : null;
    return el("aside", { class: "shapes-side" }, search, profilePicker,
      el("p", { class: "muted small" }, `${shapes.length} of ${state.outline.shapes.length} node shapes`),
      el("ul", { class: "shapes-list" }, items));
  }

  function layerText(s) {
    return s.layer === "own" ? "In the dataset's own set" + (s.file ? ", file " + s.file : "")
      : s.layer === "pack" ? "From the installed shape pack" : "Generated from the data";
  }

  // ---------- the table of settings ----------
  const key = (shape, path, setting, lang) => JSON.stringify([shape, path || null, setting, lang || ""]);
  /** The settings given per language: the language chosen above the table. */
  const BY_LANGUAGE = new Set(["name", "description"]);
  function value(row, setting, path) {
    const k = key(state.selected, path, setting, BY_LANGUAGE.has(setting) ? lang() : "");
    if (state.changes.has(k)) return state.changes.get(k).value;
    if (BY_LANGUAGE.has(setting)) {
      const by = row[setting + "ByLanguage"];
      return by ? (by[lang()] ?? (lang() === "" ? row[setting] : "")) : (lang() === "" ? row[setting] : "");
    }
    return row[setting];
  }
  const lang = () => state.lang ?? "";
  function set(path, setting, v) {
    const l = BY_LANGUAGE.has(setting) ? lang() : "";
    state.changes.set(key(state.selected, path, setting, l), { shape: state.selected, path: path || null, setting, value: v, lang: l || null });
    const save = page().querySelector("#shapes-save");
    if (save) { save.disabled = false; save.textContent = `Save ${state.changes.size} change${state.changes.size > 1 ? "s" : ""}`; }
    const preview = page().querySelector("#shapes-preview");
    if (preview) preview.disabled = false;
    for (const b of page().querySelectorAll("#shapes-propose, #shapes-map, #shapes-describe, #shapes-search, #shapes-api")) b.disabled = false;
  }
  function input(row, path, setting, kind) {
    const editable = state.outline.editable || state.outline.bound;
    const v = value(row, setting, path);
    const common = { disabled: !editable, "aria-label": `${setting} of ${path ? short(path) : "the node shape"}` };
    if (kind === "flag") {
      return el("input", { ...common, type: "checkbox", checked: v === true, onchange: (e) => set(path, setting, e.target.checked) });
    }
    if (kind === "readers" || kind === "profiles") {
      const chosen = new Set(Array.isArray(v) ? v : []);
      const options = kind === "readers" ? READERS : profiles().map((p) => p.iri);
      const label = kind === "readers" ? (r) => r : profileName;
      const none = kind === "readers" ? "none" : "none (in all)";
      const text = (list) => (list.length ? list.map(label).join(", ") : el("span", { class: "muted" }, none));
      const summary = el("summary", {}, text(options.filter((x) => chosen.has(x))));
      return el("details", { class: "shapes-readers" }, summary,
        options.map((r) => el("label", { class: "small", title: kind === "profiles" ? r : null },
          el("input", { type: "checkbox", disabled: !editable, checked: chosen.has(r), onchange: (e) => {
            if (e.target.checked) chosen.add(r); else chosen.delete(r);
            const list = options.filter((x) => chosen.has(x));
            summary.replaceChildren(text(list));
            set(path, setting, list);
          } }), " ", label(r))),
        kind === "profiles" && editable ? newProfileForm() : null);
    }
    return el("input", { ...common, type: kind === "number" ? "number" : "text", step: kind === "number" ? "any" : null, size: kind === "number" ? 4 : 14,
      value: v == null ? "" : v, onchange: (e) => set(path, setting, kind === "number" && e.target.value !== "" ? Number(e.target.value) : e.target.value) });
  }

  // ---------- the diagram: the shape, the shapes it links to, and those linking to it ----------
  const SVG = "http://www.w3.org/2000/svg";
  function svg(tag, attrs, ...children) {
    const n = document.createElementNS(SVG, tag);
    for (const [k, v] of Object.entries(attrs || {})) if (v != null) n.setAttribute(k, v);
    for (const c of children.flat()) if (c != null) n.append(c);
    return n;
  }
  const clip = (t, n) => (t.length > n ? t.slice(0, n - 1) + "…" : t);
  /** The shapes whose class is `cls`. */
  const shapesOf = (cls) => state.outline.shapes.filter((n) => (n.classes || []).includes(cls));
  function diagram(s) {
    const W = 220, ROW = 18, HEAD = 26, GAP = 70, MAX_ROWS = 12, MAX_SIDE = 12;
    // The properties that link to another shape first, so that their rows have arrows.
    const ordered = [...s.properties.filter((p) => p.class), ...s.properties.filter((p) => !p.class)];
    const props = ordered.slice(0, MAX_ROWS);
    const hidden = ordered.length - props.length;
    // Out: per shape of an sh:class of a property (all of them, also past the rows shown), the
    // properties that link to it. In: the other shapes whose properties' sh:class is one of
    // this shape's classes. A link to itself is said in the caption.
    const out = [];
    let self = [];
    ordered.forEach((p, i) => {
      if (!p.class) return;
      for (const t of shapesOf(p.class)) {
        if (t.iri === s.iri) { self.push(short(p.path)); continue; }
        const o = out.find((x) => x.shape.iri === t.iri);
        if (o) o.labels.push(short(p.path)); else out.push({ shape: t, row: Math.min(i, MAX_ROWS), labels: [short(p.path)] });
      }
    });
    self = [...new Set(self)];
    const into = [];
    for (const n of state.outline.shapes) {
      if (n.iri === s.iri) continue;
      for (const p of n.properties) {
        if (!p.class || !(s.classes || []).includes(p.class)) continue;
        const o = into.find((x) => x.shape.iri === n.iri);
        if (o) o.labels.push(short(p.path)); else into.push({ shape: n, labels: [short(p.path)] });
      }
    }
    const centerH = HEAD + Math.max(props.length, 1) * ROW + (hidden ? ROW : 0) + 6;
    const sideH = (k) => Math.min(k, MAX_SIDE + 1) * (HEAD + 12) - 12;
    const height = Math.max(centerH, sideH(into.length), sideH(out.length), 60) + 20;
    const width = W * 3 + GAP * 2 + 20;
    const cx = 10 + W + GAP, cy = 10 + (height - 20 - centerH) / 2;
    const box = (x, y, n, h, main) => svg("g", { class: "sd-box" + (main ? " main" : ""), transform: `translate(${x},${y})`,
      tabindex: main ? null : 0, role: main ? null : "button", "aria-label": main ? null : "Show " + title(n) },
      svg("rect", { width: W, height: h, rx: 6 }),
      svg("text", { x: 10, y: 17, class: "sd-title" }, clip(title(n), 28)),
      main ? null : svg("text", { x: W - 10, y: 17, "text-anchor": "end", class: "sd-muted" }, n.instances != null ? ui.fmtNum(n.instances) : ""));
    const go = (g, n) => {
      const pick = () => { if (leaveChanges()) { state.selected = n.iri; state.formFor = null; state.changes.clear(); draw(); } };
      g.addEventListener("click", pick);
      g.addEventListener("keydown", (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); pick(); } });
      return g;
    };
    const parts = [];
    // The shape, with its properties.
    const main = box(cx, cy, s, centerH, true);
    main.append(svg("line", { x1: 0, x2: W, y1: HEAD, y2: HEAD, class: "sd-rule" }));
    props.forEach((p, i) => main.append(svg("text", { x: 10, y: HEAD + 13 + i * ROW, class: p.class ? "sd-link" : null },
      svg("title", {}, p.path), clip(short(p.path), 22),
      svg("tspan", { class: "sd-muted", x: W - 10, "text-anchor": "end" }, p.class ? short(p.class) : p.datatype ? short(p.datatype) : ""))));
    if (hidden) main.append(svg("text", { x: 10, y: HEAD + 13 + props.length * ROW, class: "sd-muted" }, `and ${hidden} more`));
    const column = (list, x, edge) => {
      const shown = list.slice(0, MAX_SIDE);
      const top = 10 + (height - 20 - sideH(list.length)) / 2;
      shown.forEach((o, i) => {
        const y = top + i * (HEAD + 12);
        parts.push(edge(o, y));
        parts.push(go(box(x, y, o.shape, HEAD, false), o.shape));
      });
      if (list.length > MAX_SIDE) {
        parts.push(svg("text", { x: x + 10, y: top + MAX_SIDE * (HEAD + 12) + 17, class: "sd-muted" }, `and ${list.length - MAX_SIDE} more shapes`));
      }
    };
    const label = (o) => clip(o.labels.join(", "), 18);
    const arrow = (x1, y1, x2, y2, text, full) => svg("g", { class: "sd-edge" },
      svg("title", {}, full),
      svg("path", { d: `M${x1},${y1} C${(x1 + x2) / 2},${y1} ${(x1 + x2) / 2},${y2} ${x2},${y2}`, "marker-end": "url(#sd-arrow)" }),
      // At the arrow's start, where arrows to one box are still apart.
      svg("text", { x: x1 + 6, y: y1 - 4, class: "sd-muted" }, text));
    column(into, 10, (o, y) => arrow(10 + W, y + HEAD / 2, cx, cy + HEAD / 2, label(o), o.labels.join(", ")));
    column(out, cx + W + GAP, (o, y) => arrow(cx + W, cy + HEAD + 9 + o.row * ROW, cx + W + GAP, y + HEAD / 2, label(o), o.labels.join(", ")));
    const drawing = svg("svg", { class: "shapes-diagram", viewBox: `0 0 ${width} ${height}`, role: "img",
      "aria-label": `${title(s)}: links to ${out.length} shape(s)${self.length ? " and to itself" : ""}, linked from ${into.length}` },
      svg("defs", {}, svg("marker", { id: "sd-arrow", viewBox: "0 0 10 10", refX: 9, refY: 5, markerWidth: 7, markerHeight: 7, orient: "auto-start-reverse" },
        svg("path", { d: "M0,0 L10,5 L0,10 z", class: "sd-head" }))),
      parts, main);
    const itself = self.length ? ` It also links to itself (${self.join(", ")}).` : "";
    return el("figure", { class: "shapes-diagram-wrap" }, drawing,
      el("figcaption", { class: "muted small" }, into.length || out.length
        ? "Left: shapes whose properties link to this one. Right: the shapes its properties link to (sh:class). Pick one to show it." + itself
        : self.length ? "No property links this shape to another." + itself : "No property links this shape to another (sh:class)."));
  }

  // ---------- forms from the editor shapes (GET …/shapes/form) ----------
  /** The form of the node shape (`path` null) or of its property shape of `path`. */
  function form(row, path) {
    const spec = state.form && state.form[path ? "property" : "node"];
    if (!spec) return el("p", { class: "muted small" }, "Loading the form…");
    const editable = state.outline.editable || state.outline.bound;
    const Forms = window.TripleTrailForms;
    // The settings that only this page knows how to pick; the rest come from the shared editors.
    const custom = (f) => {
      if (f.class === "http://w3id.org/tripletrail/shapes#Profile") return input(row, path, f.setting, "profiles");
      if (f.class === "http://www.w3.org/ns/shacl#PropertyGroup") return groupPicker(row);
      if (f.in && f.multiple) return input(row, path, f.setting, "readers");
      return null;
    };
    const fields = spec.map((f) => {
      const id = "shapes-f-" + (path ? "p-" : "n-") + f.setting;
      const lang = BY_LANGUAGE.has(f.setting) && state.lang ? el("span", { class: "muted small" }, " (" + state.lang + ")") : null;
      return Forms.field(f, { el, id, value: value(row, f.setting, path), editable, custom,
        name: `${f.setting} of ${path ? short(path) : "the node shape"}`,
        set: (v) => set(path, f.setting, v) }, lang);
    });
    return el("fieldset", { class: "shapes-form" },
      el("legend", {}, path ? "Settings of " + short(path) : "Settings of " + title(row)),
      fields,
      el("p", { class: "muted small" }, "Anything else is edited in the Turtle of an own file."));
  }

  /** A field for a new profile's name: adding it puts the shape shown in it. */
  function newProfileForm() {
    const box = el("input", { type: "text", size: 14, placeholder: "e.g. Maintenance", "aria-label": "Name of a new profile" });
    const add = () => newProfile(box.value);
    box.addEventListener("keydown", (e) => { if (e.key === "Enter") { e.preventDefault(); add(); } });
    return el("span", { class: "shapes-newprofile small" }, "New profile ", box, " ",
      el("button", { type: "button", class: "btn small", onclick: add }, "Add"));
  }

  /** Makes a profile named `name` and puts the shape shown in it (saved with the next save). */
  function newProfile(name) {
    name = (name || "").trim();
    if (!name) return;
    const slug = name.toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-|-$/g, "") || "profile";
    let iri = "urn:tripletrail:profile:" + slug;
    for (let n = 2; profiles().some((p) => p.iri === iri); n++) iri = "urn:tripletrail:profile:" + slug + "-" + n;
    state.newProfiles.push({ iri, label: name });
    const s = current();
    const k = JSON.stringify([s.iri, null, "profiles", ""]);
    const before = state.changes.has(k) ? state.changes.get(k).value : s.profiles || [];
    state.selected = s.iri;
    set(null, "profiles", [...before, iri]);
    draw();
  }

  // The groups of the property shapes: those any shape uses, by their short names.
  function groupPicker(p) {
    const editable = state.outline.editable || state.outline.bound;
    const groups = new Set();
    for (const n of state.outline.shapes) for (const q of n.properties) if (q.group) groups.add(q.group);
    const v = value(p, "group", p.path);
    if (v) groups.add(v);
    return el("select", { disabled: !editable, "aria-label": "group of " + short(p.path), onchange: (e) => {
      let g = e.target.value;
      if (g === "+") { g = (prompt("IRI of the group (a sh:PropertyGroup):") || "").trim(); if (!g) { e.target.value = v || ""; return; } }
      set(p.path, "group", g);
      if (!groups.has(g)) draw();
    } },
    el("option", { value: "", selected: !v }, "none"),
    [...groups].sort().map((g) => el("option", { value: g, selected: g === v, title: g }, short(g).replace(/^Group-/, ""))),
    el("option", { value: "+" }, "other…"));
  }

  function languages(s) {
    const found = new Set([""]);
    for (const r of [s, ...s.properties]) for (const l of Object.keys(r.nameByLanguage || {})) found.add(l);
    return [...found];
  }

  function details() {
    const s = current();
    if (!s) return el("section", { class: "shapes-main" }, el("p", { class: "muted" }, "This dataset has no node shapes."));
    if (!languages(s).includes(lang())) state.lang = "";
    const langs = el("select", { "aria-label": "Language of the names", onchange: (e) => {
      let l = e.target.value;
      if (l === "+") {
        const asked = prompt("Language tag of the names, such as nl or fr:");
        l = asked && asked.trim() ? asked.trim() : "";
        // The language is offered from now on, with no names yet.
        if (l) s.nameByLanguage = { ...(s.nameByLanguage || {}), [l]: (s.nameByLanguage || {})[l] || "" };
      }
      state.lang = l;
      draw();
    } }, [...languages(s), "+"].map((l) => el("option", { value: l, selected: l === lang() }, l === "" ? "no language" : l === "+" ? "another language…" : l)));
    const rows = s.properties.map((p) => el("tr", {},
      el("th", { scope: "row", class: "small", title: p.path },
        el("button", { type: "button", class: "link", "aria-expanded": state.formFor === p.path ? "true" : "false",
          title: "All settings of " + p.path, onclick: () => { state.formFor = state.formFor === p.path ? null : p.path; draw(); } }, short(p.path))),
      p.path && p.path.startsWith("(") ? el("td", { colspan: 8, class: "muted small" }, "A complex path: edit it in Turtle") : [
        el("td", {}, input(p, p.path, "name", "text")),
        el("td", {}, input(p, p.path, "order", "number")),
        el("td", {}, groupPicker(p)),
        el("td", {}, input(p, p.path, "key", "text")),
        el("td", { class: "center" }, input(p, p.path, "hidden", "flag")),
        el("td", {}, input(p, p.path, "hiddenFrom", "readers")),
        el("td", { class: "center" }, input(p, p.path, "deactivated", "flag")),
        el("td", {}, input(p, p.path, "viewer", "iri")),
      ]));
    const canSave = state.outline.editable;
    return el("section", { class: "shapes-main" },
      el("h3", {}, title(s), " ", el("span", { class: "shapes-layer " + s.layer }, s.layer), " ",
        el("button", { type: "button", class: "btn small", "aria-pressed": state.diagram ? "true" : "false",
          onclick: () => { state.diagram = !state.diagram; draw(); } }, state.diagram ? "Hide diagram" : "Diagram")),
      state.diagram ? diagram(s) : null,
      el("p", { class: "muted small" }, el("code", {}, s.iri), ui.copyButton(s.iri), " · ", layerText(s),
        s.classes.length ? " · targets " : null,
        ...s.classes.flatMap((c, i) => [i ? ", " : null, el("code", { title: c }, short(c))])),
      s.layer !== "own" && canSave ? el("p", { class: "muted small" }, "Saving a change writes the changes to the dataset's own set; the rest of the shape still comes from the ", s.layer, " one.") : null,
      el("div", { class: "shapes-node" },
        el("label", {}, "Name ", input(s, null, "name", "text")),
        el("label", {}, "Key ", input(s, null, "key", "text")),
        el("label", {}, input(s, null, "deactivated", "flag"), " Deactivated"),
        el("span", {}, "Hidden from ", input(s, null, "hiddenFrom", "readers")),
        el("span", {}, "Only for ", input(s, null, "onlyFor", "readers")),
        el("span", {}, "Profiles ", input(s, null, "profiles", "profiles")),
        el("button", { type: "button", class: "btn small", "aria-expanded": state.formFor === "" ? "true" : "false",
          onclick: () => { state.formFor = state.formFor === "" ? null : ""; draw(); } }, "All settings…")),
      state.formFor === "" ? form(s, null) : null,
      el("div", { class: "shapes-tablebar" }, el("label", { class: "small" }, "Names in ", langs)),
      el("div", { class: "table-wrap" }, el("table", { class: "grid shapes-table" },
        el("caption", { class: "visually-hidden" }, "Property shapes of " + title(s)),
        el("thead", {}, el("tr", {}, ["Property", "Name", "Order", "Group", "Key", "Hidden", "Hidden from", "Deactivated", "Viewer"].map((h) => el("th", { scope: "col" }, h)))),
        el("tbody", {}, rows))),
      state.formFor ? (() => { const p = s.properties.find((q) => q.path === state.formFor); return p ? form(p, p.path) : null; })() : null,
      el("div", { class: "shapes-actions" },
        canSave ? el("button", { type: "button", class: "btn primary", id: "shapes-save", disabled: !state.changes.size, onclick: save }, state.changes.size ? `Save ${state.changes.size} change${state.changes.size > 1 ? "s" : ""}` : "Save changes") : null,
        proposing() ? el("button", { type: "button", class: "btn primary", id: "shapes-propose", disabled: !state.changes.size, onclick: proposeForm }, "Propose…") : null,
        el("button", { type: "button", class: "btn", id: "shapes-preview", disabled: !state.changes.size, onclick: preview }, state.outline.bound ? "Export changes" : "Preview the Turtle"),
        el("button", { type: "button", class: "btn", id: "shapes-map", disabled: !state.changes.size, onclick: previewOnMap }, "Preview on map"),
        el("button", { type: "button", class: "btn", id: "shapes-describe", disabled: !state.changes.size, onclick: previewDescribe }, "Preview a describe page"),
        el("button", { type: "button", class: "btn", id: "shapes-search", disabled: !state.changes.size, onclick: previewSearch }, "Preview search"),
        el("button", { type: "button", class: "btn", id: "shapes-api", disabled: !state.changes.size, onclick: previewApi }, "Preview the REST API"),
        el("button", { type: "button", class: "btn", onclick: () => { state.changes.clear(); state.newProfiles = []; draw(); } }, "Discard")),
      el("div", { id: "shapes-out" }),
      s.layer === "own" && s.file ? turtle(s.file) : null);
  }

  function leaveChanges() {
    return state.changes.size === 0 || confirm(`Leave ${state.changes.size} unsaved change(s)?`);
  }

  async function save() {
    try {
      const r = await ui.api("POST", base(state.ds) + "/edit", { body: editBody(), type: "application/json" });
      ui.toast(`Saved as version ${r.version} (${r.changed.join(", ")})`, "ok");
      await load();
    } catch (e) { ui.fail(e); }
  }

  async function preview() {
    try {
      const r = await ui.api("POST", base(state.ds) + "/edit", { params: { dry: "true" }, body: editBody(), type: "application/json" });
      const out = page().querySelector("#shapes-out");
      fill(out, Object.entries(r.files).map(([name, text]) => el("details", { open: true, class: "shapes-file" },
        el("summary", {}, name, " ",
          el("button", { type: "button", class: "btn small", onclick: () => download(name, text) }, "Download")),
        el("pre", { class: "small" }, text))));
    } catch (e) { ui.fail(e); }
  }

  /**
   * Opens the dataset's map, in a new tab, drawn with the shapes as the unsaved changes would
   * make them: the whole own set (dry edit, all=true), kept an hour for the map
   * (POST /{ds}/tiles/preview). Nothing is saved.
   */
  async function previewOnMap() {
    await previewIn(async (id) => "/$/ui/map/" + encodeURIComponent(state.ds) + "?preview=" + encodeURIComponent(id));
  }

  /** Opens the Search page on the shape's class, with the unsaved changes. */
  async function previewSearch() {
    const s = current();
    const cls = s && s.classes && s.classes[0];
    await previewIn(async (id) => "/$/ui/search/" + encodeURIComponent(state.ds) + "?" +
      new URLSearchParams({ ...(cls ? { m: JSON.stringify({ class: cls, where: [], show: [] }) } : {}), preview: id }));
  }

  /** Opens the REST API's docs page, with the unsaved changes. */
  async function previewApi() {
    await previewIn(async (id) => "/$/ui/api/" + encodeURIComponent(state.ds) + "?preview=" + encodeURIComponent(id));
  }

  /** Opens the describe page of one instance of the shape shown, with the unsaved changes. */
  async function previewDescribe() {
    const s = current();
    const cls = s && s.classes && s.classes[0];
    await previewIn(async (id) => {
      if (!cls) throw new Error("This shape targets no class, so it has no instance to show");
      const q = "SELECT ?s WHERE { ?s a <" + cls + "> FILTER(isIRI(?s)) } LIMIT 1";
      const r = await ui.api("GET", "/" + encodeURIComponent(state.ds) + "/query", { params: { query: q, format: "json" } });
      const iri = r.results.bindings.length ? r.results.bindings[0].s.value : null;
      if (!iri) throw new Error("The data has no instance of " + short(cls) + " to show");
      return "/" + encodeURIComponent(state.ds) + "/describe?" + new URLSearchParams({ iri, preview: id });
    });
  }

  /**
   * Opens, in a new tab, the page `where(id)` gives for a preview of the shapes as the unsaved
   * changes would make them: the whole own set (dry edit, all=true), kept an hour by the
   * server (POST /{ds}/tiles/preview). Nothing is saved.
   */
  async function previewIn(where) {
    // Opened before the requests, so that a popup blocker sees the click.
    const tab = window.open("about:blank", "_blank");
    try {
      const r = await ui.api("POST", base(state.ds) + "/edit", { params: { dry: "true", all: "true" }, body: editBody(), type: "application/json" });
      const body = JSON.stringify({ files: Object.entries(r.files).map(([name, text]) => ({ name, text })) });
      const p = await ui.api("POST", "/" + encodeURIComponent(state.ds) + "/tiles/preview", { body, type: "application/json" });
      const url = await where(p.id);
      if (tab) tab.location = url; else location.href = url;
    } catch (e) {
      if (tab) tab.close();
      ui.fail(e);
    }
  }

  function download(name, text) {
    const a = el("a", { href: URL.createObjectURL(new Blob([text], { type: "text/turtle" })), download: name });
    document.body.append(a); a.click(); a.remove();
  }

  // ---------- the Turtle of an own file ----------
  function turtle(file) {
    const area = el("textarea", { class: "shapes-turtle", rows: 18, spellcheck: "false", "aria-label": "Turtle of " + file, disabled: !state.outline.editable });
    const status = el("p", { class: "small", role: "status" });
    const saveBtn = el("button", { type: "button", class: "btn", disabled: true }, "Save the file");
    let timer = null;
    let original = "";
    ui.api("GET", base(state.ds) + "/files/" + encodeURIComponent(file)).then((t) => { area.value = original = t; }).catch(ui.fail);
    area.addEventListener("input", () => {
      saveBtn.disabled = area.value === original;
      clearTimeout(timer);
      timer = setTimeout(async () => {
        try {
          const r = await ui.api("POST", base(state.ds) + "/validate", { params: { file }, body: area.value, type: "text/turtle" });
          status.className = "small " + (r.valid ? "ok" : "error");
          status.textContent = r.valid ? "The set checks out with this file." : r.errors.join("\n");
          saveBtn.disabled = !r.valid || area.value === original;
        } catch (e) { status.className = "small error"; status.textContent = e.message; }
      }, 400);
    });
    saveBtn.addEventListener("click", async () => {
      try {
        const r = await ui.api("PUT", base(state.ds) + "/files/" + encodeURIComponent(file), { body: area.value, type: "text/turtle" });
        ui.toast(`Saved ${file} as version ${r.version}`, "ok");
        await load();
      } catch (e) { ui.fail(e); }
    });
    return el("details", { class: "shapes-file" }, el("summary", {}, "Turtle of ", el("code", {}, file)), area, status, saveBtn);
  }

  // ---------- versions ----------
  async function versions() {
    const o = state.outline;
    const dialog = el("dialog", { class: "shapes-versions", "aria-label": "Versions of the shapes" },
      el("h3", {}, "Versions of the shapes"),
      el("p", { class: "muted small" }, "The last versions are kept. A rollback makes a kept version the current one again, as a new version."),
      el("ul", {}, o.versions.slice().reverse().map((v) => el("li", {}, "Version ", v, v === o.version ? el("strong", {}, " (in use)") :
        o.editable ? el("button", { type: "button", class: "btn small", onclick: async () => {
          try {
            const r = await ui.api("POST", base(state.ds) + "/rollback", { params: { version: v } });
            ui.toast(`Version ${v} is back, as version ${r.version}`, "ok");
            dialog.close(); dialog.remove(); await load();
          } catch (e) { ui.fail(e); }
        } }, "Roll back to it") : null))),
      el("button", { type: "button", class: "btn", onclick: () => { dialog.close(); dialog.remove(); } }, "Close"));
    document.body.append(dialog);
    dialog.showModal();
  }

  /** A link the forge gave, only when it is http(s). */
  const webLink = (u) => (/^https?:\/\//i.test(u || "") ? u : null);
  /** Whether changes can be proposed: a bound set, and a licence that covers review. */
  const proposing = () => !!(state.review && state.review.licensed);

  /**
   * The proposal form, under the changes: a title and a description, what the changes would
   * do for each reader (a dry run), and the button that opens the pull request (POST
   * …/shapes/proposals). The server validates all of the data with the proposed shapes in
   * the background and comments on the pull request; the Proposals list shows how it went.
   */
  function proposeForm() {
    const out = page().querySelector("#shapes-out");
    const titleBox = el("input", { type: "text", id: "proposal-title", maxlength: "200", required: true, size: "60" });
    const text = el("textarea", { id: "proposal-description", rows: "4", cols: "60" });
    const effectsOut = el("div", { id: "proposal-effects" });
    const body = () => {
      const b = JSON.parse(editBody());
      b.title = titleBox.value;
      b.description = text.value;
      return JSON.stringify(b);
    };
    const check = async () => {
      if (!titleBox.value.trim()) { titleBox.reportValidity(); return; }
      try {
        const r = await ui.api("POST", base(state.ds) + "/proposals", { params: { dry: "true", validate: "false" }, body: body(), type: "application/json" });
        fill(effectsOut, el("h4", {}, "What it changes"), effectsList(r.effects),
          el("p", { class: "muted small" }, "Files: ", r.files.map((f) => f.path + (f.new ? " (new)" : "")).join(", ")));
      } catch (e) { ui.fail(e); }
    };
    const open = async (e) => {
      e.preventDefault();
      try {
        const r = await ui.api("POST", base(state.ds) + "/proposals", { body: body(), type: "application/json" });
        ui.toast(`Opened ${r.url || "the pull request"}`, "ok");
        await load();
        fill(page().querySelector("#shapes-out"), el("p", {}, "Proposal ", r.proposal, " is open: ",
          webLink(r.url) ? el("a", { href: webLink(r.url), target: "_blank", rel: "noopener" }, r.url) : r.branch,
          ". TripleTrail will validate all of the data with the proposed shapes and comment on it with the counts."));
      } catch (err) { ui.fail(err); }
    };
    fill(out, el("form", { class: "shapes-propose", onsubmit: open },
      el("h4", {}, `Propose ${state.changes.size} change${state.changes.size > 1 ? "s" : ""}`),
      el("p", { class: "muted small" }, "The changed files are committed on a new branch of ", el("code", {}, state.outline.bound),
        " and a pull request is opened for review. Nothing changes here until it is merged and pulled."),
      el("label", { for: "proposal-title" }, "Title"), titleBox,
      el("label", { for: "proposal-description" }, "Description"), text,
      el("div", {},
        el("button", { type: "button", class: "btn", id: "proposal-check", onclick: check }, "Show what it changes"),
        el("button", { type: "submit", class: "btn primary", id: "proposal-open" }, "Open the pull request")),
      effectsOut));
    titleBox.focus();
  }

  /** What a proposal changes, per reader (those with the same effect together) and profile. */
  function effectsList(effects) {
    const names = (list) => list.map((n) => short(n)).join(", ");
    const groups = [];
    for (const [reader, d] of Object.entries((effects && effects.readers) || {})) {
      const same = groups.find((g) => JSON.stringify(g.d) === JSON.stringify(d));
      if (same) same.readers.push(reader); else groups.push({ readers: [reader], d });
    }
    const items = groups.map(({ readers, d }) => {
      const parts = [];
      if (d.added) parts.push("adds " + names(d.added));
      if (d.removed) parts.push("drops " + names(d.removed));
      for (const c of d.changed || []) {
        const what = [];
        if (c.settings) what.push("settings");
        if (c.rowsAdded) what.push("rows added " + names(c.rowsAdded));
        if (c.rowsRemoved) what.push("rows removed " + names(c.rowsRemoved));
        if (c.rowsChanged) what.push("rows changed " + names(c.rowsChanged));
        parts.push(`changes ${short(c.shape)} (${what.join("; ")})`);
      }
      return el("li", {}, el("strong", {}, readers.join(", ")), ": ", parts.join("; "));
    });
    for (const [profile, d] of Object.entries((effects && effects.profiles) || {})) {
      const parts = [];
      if (d.added && d.added.length) parts.push("adds " + names(d.added));
      if (d.removed && d.removed.length) parts.push("drops " + names(d.removed));
      if (parts.length) items.push(el("li", {}, "Profile ", el("strong", {}, profileName(profile)), ": ", parts.join("; ")));
    }
    return items.length ? el("ul", {}, items) : el("p", { class: "muted" }, "No reader sees a difference.");
  }

  /** The proposals made from this page, newest first, with their review and validation. */
  async function proposalList() {
    // The open ones' pull requests asked of the forge first: merged or closed since.
    if (proposing() && state.review.proposals.some((p) => p.status === "open")) {
      try {
        state.review = await ui.api("GET", base(state.ds) + "/proposals", { params: { refresh: "true" } });
      } catch (e) { /* only the dataset's admins refresh: the list as it stands */ }
    }
    const validation = (v) => {
      if (!v) return "";
      if (v.status === "queued") return "validation waiting its turn";
      if (v.status === "running") return "validating…";
      if (v.status === "failed" || v.status === "interrupted") return "validation " + v.status + ": " + v.error;
      if (v.compared === false) return `${v.after} validation results (${v.before} before), too many to compare`;
      return `${v.new} new, ${v.gone} gone validation results`;
    };
    const dialog = el("dialog", { class: "shapes-versions", "aria-label": "Proposed changes" },
      el("h3", {}, "Proposed changes"),
      state.review.proposals.length ? el("ul", {}, state.review.proposals.map((p) => el("li", {},
        `${p.proposal}. `, webLink(p.url) ? el("a", { href: webLink(p.url), target: "_blank", rel: "noopener" }, p.title) : p.title,
        el("span", { class: "muted small" }, " · ", p.status, p.statusError ? " (could not check: " + p.statusError + ")" : "", p.by ? " · by " + p.by : "", " · ", new Date(p.at).toLocaleString(),
          p.error ? " · " + p.error : "", p.validation ? " · " + validation(p.validation) : ""))))
        : el("p", { class: "muted" }, "Nothing has been proposed yet."),
      el("button", { type: "button", class: "btn", onclick: () => { dialog.close(); dialog.remove(); } }, "Close"));
    document.body.append(dialog);
    dialog.showModal();
  }

  function draw() {
    fill(page(), header(), el("div", { class: "shapes-layout" }, sidebar(), details()));
    // The shape shown is in the page's link, so the link can be shared.
    const url = "/$/ui/shapes/" + encodeURIComponent(state.ds) + (state.selected ? "?shape=" + encodeURIComponent(state.selected) : "");
    if (document.body.dataset.page === "shapes" && url !== location.pathname + location.search) history.replaceState(null, "", url + location.hash);
  }

  return { render };
})();
