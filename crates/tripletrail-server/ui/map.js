// The Map page (#139): a dataset's features over the server's base map, with MapLibre GL JS.
// Plain JavaScript like app.js, which calls TripleTrailMap.render(ui, args).
//
// /$/ui/map/{dataset}   the map of one dataset
//
// The page's link holds what it shows, so it can be shared: the position in the hash
// (#zoom/lat/lon, MapLibre's), the hidden layers as `hide=id,id`, the filters as the tiles
// take them (`{collection}.{key}=value`, `{collection}.{key}.min`/`.max`) and the facets
// collections are coloured by (`color.{collection}={key}`) in the query, and the map view
// picked (`view={key}`), whose layers, filters and colours they were set from.
//
// Everything the map shows comes from the dataset's style (/{ds}/tiles/style.json): a layer
// per collection the `map` reader reads, in its colour and from its zoom, over the vector
// tiles. The layer list toggles them; a click lists the features under the pointer, with their
// labels, and links each to its describe page.
"use strict";

window.TripleTrailMap = (function () {
  const VENDOR = "/$/ui/vendor/maplibre/";
  const LABELS = [
    "http://www.w3.org/2000/01/rdf-schema#label",
    "http://www.w3.org/2004/02/skos/core#prefLabel",
    "http://schema.org/name",
    "https://schema.org/name",
    "http://purl.org/dc/terms/title",
  ];
  /** Features listed for one click. */
  const MAX_PICKED = 25;
  /** Colours of a facet's most frequent values when a collection is coloured by it
   *  (Tableau 10, told apart by most colour-blind readers), and of the other values. */
  const VALUE_COLOURS = ["#4e79a7", "#f28e2b", "#e15759", "#76b7b2", "#59a14f", "#edc948", "#b07aa1", "#ff9da7", "#9c755f", "#bab0ac"];
  /** Colours of a range facet's classes, low (blue) to high (red). */
  const RANGE_COLOURS = ["#313695", "#4575b4", "#74add1", "#66bd63", "#fdae61", "#f46d43", "#d73027", "#a50026"];
  const OTHER_COLOUR = "#8a8f98";
  /** How the values of a boolean facet read. */
  const YES_NO = Object.assign(Object.create(null), { true: "yes", false: "no" });
  let ui = null;
  let lib = null;
  let map = null;
  let shown = null; // the dataset on the map
  let preview = null; // the id of proposed shapes it is drawn with (/{ds}/tiles/preview/{id}), or null
  /** A path under the dataset's tiles, or under the preview's. */
  const tilesPath = (rest) => (preview ? "tiles/preview/" + preview + "/" : "tiles/") + rest;
  const page = () => document.getElementById("page-map");
  const el = (...a) => ui.el(...a);

  function loadLibrary() {
    if (lib) return Promise.resolve(lib);
    const css = document.createElement("link");
    css.rel = "stylesheet";
    css.href = VENDOR + "maplibre-gl.css";
    document.head.append(css);
    return import(VENDOR + "maplibre-gl.mjs").then((m) => (lib = m.Map ? m : m.default));
  }

  /** The caller's headers for the dataset's own URLs (the admin token, "view as"). */
  const headers = () => ui.viewAs.headers();
  async function getJson(url) {
    const r = await fetch(url, { headers: { Accept: "application/json", ...headers() } });
    if (!r.ok) throw new Error((await r.text()).trim() || r.statusText);
    return r.json();
  }
  const service = (ds, path) => "/" + encodeURIComponent(ds) + "/" + path;

  async function render(shared, args) {
    ui = shared;
    const root = page();
    let datasets = [];
    try {
      datasets = (await ui.api("GET", "/$/datasets")).datasets.map((d) => d.name);
    } catch (e) { ui.fail(e); }
    let name = args[0] ? decodeURIComponent(args[0]) : null;
    if (!name || !datasets.includes(name)) name = datasets.includes(shown) ? shown : datasets[0];
    if (!name) {
      leave();
      root.replaceChildren(el("p", { class: "muted loading" }, "No dataset to map yet."));
      return;
    }
    // Proposed shapes to draw with instead of the dataset's own (#146), from the Workbench.
    const wanted = new URLSearchParams(location.search).get("preview");
    const previewId = wanted && /^[0-9a-f]{32}$/.test(wanted) ? wanted : null;
    if (name === shown && previewId === preview && map && root.querySelector(".map-canvas")) return;
    leave();
    shown = name;
    preview = previewId;
    const select = el("select", { id: "map-ds", onchange: (e) => ui.go("map/" + encodeURIComponent(e.target.value)) },
      datasets.map((d) => el("option", { value: d, selected: d === name }, d)));
    const status = el("span", { class: "muted small", id: "map-status" });
    const layers = el("div", { class: "map-layers", id: "map-layers" }, el("p", { class: "muted small" }, "Loading the layers…"));
    const views = el("div", { class: "map-views", hidden: true });
    const canvas = el("div", { class: "map-canvas", id: "map-canvas" });
    const found = el("ul", { class: "map-found card", id: "map-found", hidden: true });
    const find = el("input", { type: "search", class: "map-find", placeholder: "Find a feature by name…", "aria-label": "Find a feature by name",
      "aria-controls": "map-found", autocomplete: "off" });
    root.replaceChildren(
      el("div", { class: "query-bar" },
        el("label", {}, "Dataset ", select),
        el("span", { class: "map-find-wrap" }, find, found),
        status,
        el("span", { class: "grow" }),
        preview ? el("span", { class: "map-preview", role: "status" }, "Preview of proposed shapes, not saved. ",
          el("a", { href: "/$/ui/map/" + encodeURIComponent(name) }, "The dataset's own map")) : null,
        el("a", { class: "btn", href: service(name, "ogc/collections?f=html"), title: "The same collections as OGC API – Features" }, "OGC API"),
        el("a", { class: "btn", href: "/$/ui/workbench?configure=" + encodeURIComponent(name), "data-route": "", title: "Change what the map shows by asking an AI model, with your own key, in a new workspace" }, "Configure with AI")),
      el("div", { class: "map-wrap" }, el("aside", { class: "map-side card" }, views, el("h3", {}, "Layers"), layers), canvas,
        el("aside", { class: "map-detail card", id: "map-detail", hidden: true })));
    let style;
    try {
      await loadLibrary();
      style = await getJson(service(name, tilesPath("style.json")));
    } catch (e) {
      canvas.replaceChildren(el("p", { class: "error loading" }, "The map could not start: " + (e.message || e)));
      return;
    }
    if (shown !== name) return;
    const collections = (style.metadata && style.metadata["tripletrail:collections"]) || [];
    // The link's hidden layers and filters, drawn from the start.
    const state = readState(collections);
    const tiles = style.sources.tripletrail.tiles[0];
    const q = filterQuery(state.chosen, state.colors);
    if (q) style.sources.tripletrail.tiles = [tiles + "?" + q];
    for (const l of style.layers) {
      if (l.source === "tripletrail" && state.hidden.has(l["source-layer"])) l.layout = { ...(l.layout || {}), visibility: "none" };
    }
    // The page's own background when there is no base map.
    const bg = getComputedStyle(document.body).backgroundColor;
    for (const l of style.layers) if (l.type === "background" && bg) l.paint = { "background-color": bg };
    const positioned = /^#\d/.test(location.hash);
    map = new lib.Map({
      container: canvas,
      style,
      hash: true,
      center: [0, 20],
      zoom: 1,
      transformRequest: (url) => (url.startsWith(location.origin) ? { url, headers: headers() } : { url }),
    });
    map.addControl(new lib.NavigationControl(), "top-right");
    map.addControl(new lib.ScaleControl(), "bottom-left");
    const ids = new Set(style.layers.filter((l) => l.source === "tripletrail").map((l) => l.id));
    // A view picked: its layers, filters and colours, on the map as it is.
    const apply = (next) => {
      Object.assign(state, next);
      for (const c of collections) {
        for (const [kind, property] of [["fill", "fill-color"], ["line", "line-color"], ["point", "circle-color"]]) {
          const id = c.id + "-" + kind;
          if (!map.getLayer(id)) continue;
          map.setLayoutProperty(id, "visibility", state.hidden.has(c.id) ? "none" : "visible");
          map.setPaintProperty(id, property, c.color);
        }
      }
      const q = filterQuery(state.chosen, state.colors);
      map.getSource("tripletrail").setTiles([tiles + (q ? "?" + q : "")]);
      writeState(state);
      legend(layers, collections, name, tiles, state);
    };
    state.views = (style.metadata && style.metadata["tripletrail:views"]) || [];
    viewPicker(views, state.views, collections, state, apply);
    legend(layers, collections, name, tiles, state);
    const zoomNote = () => {
      const z = map.getZoom();
      for (const row of layers.querySelectorAll("[data-min]")) row.classList.toggle("map-off", z < Number(row.dataset.min));
      status.textContent = `zoom ${z.toFixed(1)}`;
    };
    map.on("zoomend", zoomNote);
    map.on("load", zoomNote);
    let warned = false;
    map.on("error", (e) => {
      const s = e && e.error && e.error.status;
      if (s === 413 && !warned) { warned = true; ui.toast("A tile has too many features to draw here: zoom in.", "bad"); }
    });
    const pickable = () => [...ids].filter((id) => map.getLayer(id) && map.getLayoutProperty(id, "visibility") !== "none");
    map.on("mousemove", (e) => {
      const hits = map.queryRenderedFeatures(box(e.point, 3), { layers: pickable() });
      map.getCanvas().style.cursor = hits.length ? "pointer" : "";
    });
    map.on("click", (e) => pick(name, e, collections, pickable()));
    searchBox(name, find, found, collections);
    if (!positioned) fit(name);
    // The link's open feature (`feature=IRI`), beside the map again.
    const open = new URLSearchParams(location.search).get("feature");
    if (open) {
      detail(name, open, short(open));
      labels(name, [open]).then((found) => {
        for (const [iri, label] of found) for (const t of document.querySelectorAll("#map-detail .map-detail-head strong")) if (t.title === iri) t.textContent = label;
      }).catch(() => { /* the short name stays */ });
    }
  }

  /** The search box: the features whose labels hold the words typed (the server's full-text
   *  search), best first; one chosen goes on the map, its layer shown, and opens beside it. */
  function searchBox(ds, input, list, collections) {
    const byId = new Map(collections.map((c) => [c.id, c]));
    let timer = null;
    let asked = 0;
    const close = () => { list.hidden = true; list.replaceChildren(); };
    const go = (r) => {
      close();
      input.value = r.label;
      const c = byId.get(r.collection);
      const [w, s, e, n] = r.bbox;
      const minZoom = (c && c.minZoom) || 0;
      map.fitBounds([[w, s], [e, n]], { padding: 80, maxZoom: Math.max(15, minZoom), minZoom, duration: 600 });
      // Its layer on, if it was hidden.
      const box = c && document.querySelector(`.map-entry[data-id="${CSS.escape(c.id)}"] input[type=checkbox]`);
      if (box && !box.checked) box.click();
      detail(ds, r.iri, r.label);
    };
    const ask = async () => {
      const q = input.value.trim();
      const mine = ++asked;
      if (q.length < 2) return close();
      let out;
      try {
        out = await getJson(service(ds, tilesPath("search?q=" + encodeURIComponent(q))));
      } catch (e) {
        if (mine !== asked) return;
        list.replaceChildren(el("li", { class: "muted small" }, "Search is not available: " + (e.message || e)));
        list.hidden = false;
        return;
      }
      if (mine !== asked) return;
      const results = out.results || [];
      list.replaceChildren(...(results.length ? results.map((r) => {
        const c = byId.get(r.collection) || { title: r.collection, color: "#888" };
        return el("li", {}, el("button", { type: "button", class: "map-found-item", title: r.iri, onclick: () => go(r) },
          el("span", { class: "map-swatch", style: "background:" + c.color }),
          el("span", { class: "map-name" }, r.label), el("span", { class: "muted small" }, " " + c.title)));
      }) : [el("li", { class: "muted small" }, "No feature has those words in its name.")]));
      list.hidden = false;
    };
    input.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(ask, 250); });
    input.addEventListener("keydown", (e) => {
      if (e.key === "Escape") close();
      if (e.key === "Enter") {
        e.preventDefault();
        clearTimeout(timer);
        const first = list.querySelector(".map-found-item");
        if (first && !list.hidden) first.click(); else ask();
      }
      if (e.key === "ArrowDown") { const first = list.querySelector(".map-found-item"); if (first) { e.preventDefault(); first.focus(); } }
    });
    list.addEventListener("keydown", (e) => {
      const items = [...list.querySelectorAll(".map-found-item")];
      const i = items.indexOf(document.activeElement);
      if (e.key === "ArrowDown" && i >= 0 && i < items.length - 1) { e.preventDefault(); items[i + 1].focus(); }
      if (e.key === "ArrowUp") { e.preventDefault(); (i > 0 ? items[i - 1] : input).focus(); }
      if (e.key === "Escape") { close(); input.focus(); }
    });
    document.addEventListener("click", (e) => { if (!input.parentElement.contains(e.target)) close(); });
  }

  /** The map views of the shapes (`tts:MapView`), when there are any: picking one shows its
   *  layers only, with its filters and colours (`apply` puts them on the map); "All layers"
   *  shows every layer unfiltered. The link keeps the view's key. */
  function viewPicker(target, views, collections, state, apply) {
    if (!views.length) return;
    const current = views.find((v) => v.key === state.view);
    const about = el("p", { class: "muted small" }, (current && current.description) || "");
    const pick = (key) => {
      const v = views.find((x) => x.key === key);
      const next = { hidden: new Set(), chosen: Object.create(null), colors: Object.create(null), view: v ? v.key : "" };
      if (v) {
        const shows = new Set(v.collections);
        for (const c of collections) if (!shows.has(c.id)) next.hidden.add(c.id);
        for (const [id, keys] of Object.entries(v.filters || {})) {
          const mine = (next.chosen[id] = Object.create(null));
          for (const [key, choice] of Object.entries(keys)) {
            mine[key] = Array.isArray(choice) ? new Set(choice.map(String)) : { min: String(choice.min ?? ""), max: String(choice.max ?? "") };
          }
        }
        for (const [id, key] of Object.entries(v.colors || {})) next.colors[id] = key;
      }
      about.textContent = (v && v.description) || "";
      apply(next);
    };
    const select = el("select", { id: "map-view", onchange: (e) => pick(e.target.value) },
      el("option", { value: "" }, "All layers"),
      views.map((v) => el("option", { value: v.key, selected: current && v.key === current.key, title: v.description || "" }, v.title)));
    target.replaceChildren(el("label", { class: "small" }, "View ", select), about);
    target.hidden = false;
  }

  function box(p, r) { return [[p.x - r, p.y - r], [p.x + r, p.y + r]]; }

  /** A facet's choice as query parameters: values, a range's `.min` and `.max`, or text's
   *  `.text`. */
  function appendChoice(q, prefix, key, choice) {
    if (choice instanceof Set) for (const v of choice) q.append(prefix + key, v);
    else if ("text" in choice) q.append(prefix + key + ".text", choice.text);
    else for (const end of ["min", "max"]) if (choice[end] !== "") q.append(prefix + key + "." + end, choice[end]);
  }

  /** The filters of all collections as the `all` tiles take them, and the facets the
   *  collections are coloured by (`color.{id}={key}`). */
  function filterQuery(chosen, colors) {
    const q = new URLSearchParams();
    for (const [id, keys] of Object.entries(chosen)) for (const [key, choice] of Object.entries(keys)) appendChoice(q, id + ".", key, choice);
    for (const [id, key] of Object.entries(colors || {})) q.append("color." + id, key);
    return q.toString();
  }

  /** The hidden layers and the filters of the page's link: {hidden: Set of ids, chosen:
   *  {id: {key: Set of values | {min, max}}}}. Unknown collections are left out. */
  function readState(collections) {
    const ids = new Set(collections.map((c) => c.id));
    const params = new URLSearchParams(location.search);
    const hidden = new Set((params.get("hide") || "").split(",").filter((id) => ids.has(id)));
    const chosen = Object.create(null);
    const colors = Object.create(null);
    for (const [name, value] of params) {
      if (name.startsWith("color.")) {
        // The first, as the tiles take it.
        if (ids.has(name.slice(6)) && value && !(name.slice(6) in colors)) colors[name.slice(6)] = value;
        continue;
      }
      const dot = name.indexOf(".");
      const id = name.slice(0, dot);
      if (dot < 1 || !ids.has(id) || value === "") continue;
      let key = name.slice(dot + 1);
      const end = /\.(min|max|text)$/.exec(key);
      // Names that would reach Object.prototype through a plain object.
      if (["__proto__", "constructor", "prototype"].includes(end ? key.slice(0, -end[0].length) : key)) continue;
      const keys = (chosen[id] = chosen[id] || Object.create(null));
      if (end && end[1] === "text") {
        key = key.slice(0, -end[0].length);
        if (!keys[key]) keys[key] = { text: value };
      } else if (end) {
        key = key.slice(0, -end[0].length);
        const range = keys[key] instanceof Set || (keys[key] && "text" in keys[key]) ? null : (keys[key] = keys[key] || { min: "", max: "" });
        if (range) range[end[1]] = value;
      } else {
        const set = keys[key] instanceof Set ? keys[key] : keys[key] ? null : (keys[key] = new Set());
        if (set) set.add(value);
      }
    }
    return { hidden, chosen, colors, view: params.get("view") || "" };
  }

  /** Writes the hidden layers, filters and colourings into the page's link, keeping the
   *  map's hash. */
  function writeState({ hidden, chosen, colors, view }) {
    const q = new URLSearchParams(filterQuery(chosen, colors));
    if (hidden.size) q.set("hide", [...hidden].join(","));
    if (view) q.set("view", view);
    if (preview) q.set("preview", preview);
    const open = new URLSearchParams(location.search).get("feature");
    if (open) q.set("feature", open);
    const search = q.toString();
    history.replaceState(history.state, "", location.pathname + (search ? "?" + search : "") + location.hash);
  }

  /** A collection's filters as CQL2 text for the OGC API: values with `IN`, ranges with `>=`
   *  and `<=` (numbers as numbers, dates with DATE or TIMESTAMP). */
  function cql(keys) {
    const text = (v) => "'" + String(v).replace(/'/g, "''") + "'";
    const bound = (v) => {
      if (/^-?\d+(\.\d+)?([eE][-+]?\d+)?$/.test(v)) return v;
      if (/^\d{4}-\d{2}-\d{2}$/.test(v)) return "DATE(" + text(v) + ")";
      if (/^\d{4}-\d{2}-\d{2}T/.test(v)) return "TIMESTAMP(" + text(v) + ")";
      return text(v);
    };
    const parts = [];
    for (const [name, choice] of Object.entries(keys || {})) {
      // A key that is no plain identifier is quoted.
      const key = /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) ? name : '"' + name.replace(/"/g, '""') + '"';
      if (choice instanceof Set) parts.push(`${key} IN (${[...choice].map(text).join(", ")})`);
      else if ("text" in choice) parts.push(`CASEI(${key}) LIKE CASEI(${text("%" + choice.text.replace(/[\\%_]/g, "\\$&") + "%")})`); // % and _ as text
      else {
        if (choice.min !== "") parts.push(`${key} >= ${bound(choice.min)}`);
        if (choice.max !== "") parts.push(`${key} <= ${bound(choice.max)}`);
      }
    }
    return parts.join(" AND ");
  }

  /** The query page of `ds` opening `query` in a new tab (MatGUI reads `query` and
   *  `endpoint` from the link). */
  function queryLink(ds, query) {
    const q = new URLSearchParams({ query, endpoint: new URL(service(ds, "sparql"), location.href).href });
    return "/$/ui/query/" + encodeURIComponent(ds) + "?" + q;
  }

  /** The collection's features in the OGC API, filtered as on the map: `f=html` for a page,
   *  `f=json` for a GIS. */
  function ogcItems(ds, c, keys, f) {
    const q = new URLSearchParams({ f });
    const filter = cql(keys);
    if (filter) q.set("filter", filter);
    return new URL(service(ds, "ogc/collections/" + encodeURIComponent(c.id) + "/items?" + q), location.href).href;
  }

  /** The layer list: a checkbox, colour and count per collection, greyed below its zoom, and
   *  its facets to filter it by. */
  function legend(target, collections, ds, tiles, state) {
    if (!collections.length) {
      target.replaceChildren(el("p", { class: "muted small" }, "Nothing with a geometry in this dataset: features need a GeoSPARQL geometry (geo:hasGeometry/geo:asWKT)."));
      return;
    }
    // Chosen values per collection and facet key: {id: {key: Set | {min, max}}}.
    const chosen = state.chosen;
    const hidden = state.hidden;
    // The values facet each collection is coloured by: {id: key}.
    const colors = state.colors;
    const append = appendChoice;
    const refilter = () => {
      const q = filterQuery(chosen, colors);
      const source = map && map.getSource("tripletrail");
      if (source) source.setTiles([tiles + (q ? "?" + q : "")]);
      writeState(state);
    };
    // A collection's layers in its own colour, or in a colour per value of facet `f` (its
    // most frequent values; the others grey).
    const paint = (c, f) => {
      let color;
      let values;
      if (f && f.range) {
        // A colour per class, by the feature's highest number; features without one grey.
        values = rangeClasses(f);
        const steps = values.slice(0, -1).flatMap((k, i) => [k.upTo + 1e-9, values[i + 1].colour]);
        color = ["case", ["has", "value"], ["step", ["to-number", ["get", "value"]], values[0].colour, ...steps], OTHER_COLOUR];
      } else {
        values = f ? f.values.slice(0, VALUE_COLOURS.length) : [];
        const pairs = values.flatMap((v, i) => [v.value, VALUE_COLOURS[i]]);
        color = !f ? c.color : pairs.length ? ["match", ["get", "value"], ...pairs, OTHER_COLOUR] : OTHER_COLOUR;
      }
      for (const [kind, property] of [["fill", "fill-color"], ["line", "line-color"], ["point", "circle-color"]]) {
        const id = c.id + "-" + kind;
        if (map && map.getLayer(id)) map.setPaintProperty(id, property, color);
      }
      return values;
    };
    const toggle = (c, on) => {
      for (const kind of ["fill", "line", "point"]) {
        const id = c.id + "-" + kind;
        if (map && map.getLayer(id)) map.setLayoutProperty(id, "visibility", on ? "visible" : "none");
      }
      if (on) hidden.delete(c.id); else hidden.add(c.id);
      writeState(state);
    };
    target.replaceChildren(...collections.map((c) => {
      const box = el("input", { type: "checkbox", checked: !hidden.has(c.id), onchange: (e) => toggle(c, e.target.checked) });
      const title = c.description ? c.title + ": " + c.description : c.title;
      const count = el("span", { class: "muted small num" }, c.instances != null ? ui.fmtNum(c.instances) : "");
      const panel = el("div", { class: "map-facets", hidden: true });
      const open = el("button", { class: "map-filter", type: "button", title: "Filter " + c.title + " by its values", "aria-expanded": "false" }, "Filter");
      const load = async () => {
        const params = new URLSearchParams();
        for (const [key, choice] of Object.entries(chosen[c.id] || {})) append(params, "", key, choice);
        panel.replaceChildren(el("p", { class: "muted small" }, "Counting…"));
        let out;
        try {
          out = await getJson(service(ds, tilesPath("facets/" + encodeURIComponent(c.id) + (params.toString() ? "?" + params : ""))));
        } catch (e) {
          panel.replaceChildren(el("p", { class: "error small" }, e.message || String(e)));
          return;
        }
        // Keys from a link that are no facet of the collection: the tiles ignore them, the
        // OGC API would refuse them.
        const known = new Set(out.facets.map((f) => f.key));
        const keys = chosen[c.id] || {};
        const unknown = Object.keys(keys).filter((k) => !known.has(k));
        if (unknown.length) {
          for (const k of unknown) delete keys[k];
          if (!Object.keys(keys).length) delete chosen[c.id];
          refilter();
        }
        // A colouring from a link by a key that is no values facet: the tiles leave it out.
        if (colors[c.id] && !out.facets.some((f) => f.key === colors[c.id] && !f.text)) {
          delete colors[c.id];
          refilter();
        }
        const filtered = Object.keys(chosen[c.id] || {}).length > 0;
        count.textContent = filtered ? `${ui.fmtNum(out.features)} of ${ui.fmtNum(c.instances)}` : ui.fmtNum(out.features);
        open.classList.toggle("on", filtered);
        facets(panel, c, out, filtered);
      };
      const facets = (target, c, out, filtered) => {
        // Facets with a choice first; those with one value tell what all features share.
        const ranges = out.facets.filter((f) => f.range && f.min != null && (f.min !== f.max || (chosen[c.id] || {})[f.key]));
        const lists = out.facets.filter((f) => !f.range && !f.text);
        const texts = out.facets.filter((f) => f.text);
        const searched = (f) => !!((chosen[c.id] || {})[f.key] || {}).text;
        const isName = (f) => /(^|[^a-z])(label|name|title)$/i.test(f.key) || /label|name|title/i.test(f.title);
        const textBoxes = (fs) => fs.map((f) => {
          const now = ((chosen[c.id] || {})[f.key] || {}).text || "";
          return el("label", { class: "map-text small" }, f.title,
            el("input", { type: "search", value: now, placeholder: "contains…", "aria-label": f.title + " contains",
              onchange: (e) => search(f, e.target.value) }));
        });
        const search = (f, value) => {
          const keys = (chosen[c.id] = chosen[c.id] || Object.create(null));
          if (value.trim()) keys[f.key] = { text: value.trim() }; else delete keys[f.key];
          if (!Object.keys(keys).length) delete chosen[c.id];
          refilter();
          load();
        };
        // The facets the picked view lists (tts:listsFacets) come first, in its order.
        const view = (state.views || []).find((v) => v.key === state.view);
        const listed = (view && view.facets && view.facets[c.id]) || [];
        const rank = (f) => { const i = listed.indexOf(f.key); return i < 0 ? Infinity : i; };
        const choice = lists.filter((f) => f.values.length > 1 || (chosen[c.id] || {})[f.key]);
        const same = lists.filter((f) => !choice.includes(f) && f.values.length === 1);
        const between = (f, end, value) => {
          const keys = (chosen[c.id] = chosen[c.id] || Object.create(null));
          const range = (keys[f.key] = keys[f.key] || { min: "", max: "" });
          range[end] = value.trim();
          if (range.min === "" && range.max === "") delete keys[f.key];
          if (!Object.keys(keys).length) delete chosen[c.id];
          refilter();
          load();
        };
        const numeric = (f) => typeof f.min === "number" && typeof f.max === "number";
        const valuesBox = (f) => {
          const set = (chosen[c.id] || {})[f.key] || new Set();
          return el("details", { class: "map-facet", open: set.size > 0 || choice.length <= 3 },
            el("summary", {}, f.title, set.size ? el("span", { class: "muted small" }, ` (${set.size})`) : null),
            ...f.values.map((v) => el("label", { class: "map-value", title: v.value },
              el("input", { type: "checkbox", checked: set.has(v.value), onchange: (e) => pick(f, v, e.target.checked) }),
              el("span", { class: "map-name" }, v.label || YES_NO[v.value] || short(v.value)),
              el("span", { class: "muted small num" }, ui.fmtNum(v.count)))),
            f.more ? el("p", { class: "muted small" }, "The 50 most frequent values.") : null);
        };
        const rangeBox = (f) => {
          const range = (chosen[c.id] || {})[f.key] || { min: "", max: "" };
          const input = (end) => el("input", {
            type: numeric(f) ? "number" : "text", class: "map-bound", value: range[end], placeholder: String(f[end]),
            "aria-label": f.title + (end === "min" ? " from" : " to"), step: "any",
            onchange: (e) => between(f, end, e.target.value),
          });
          const set = range.min !== "" || range.max !== "";
          return el("details", { class: "map-facet", open: set },
            el("summary", {}, f.title, el("span", { class: "muted small" }, set ? " (range)" : ` ${f.min} – ${f.max}${f.unit ? " " + f.unit : ""}`)),
            el("div", { class: "map-range" }, input("min"), el("span", { class: "muted" }, "–"), input("max"),
              el("span", { class: "muted small num" }, ui.fmtNum(f.count))));
        };
        const pick = (f, v, on) => {
          const keys = (chosen[c.id] = chosen[c.id] || Object.create(null));
          const set = (keys[f.key] = keys[f.key] || new Set());
          if (on) set.add(v.value); else set.delete(v.value);
          if (!set.size) delete keys[f.key];
          if (!Object.keys(keys).length) delete chosen[c.id];
          refilter();
          load();
        };
        const copy = async () => {
          const url = ogcItems(ds, c, chosen[c.id], "json");
          try {
            await navigator.clipboard.writeText(url);
            ui.toast("Copied the GeoJSON link of " + c.title + (filtered ? ", filtered" : "") + ": open it in QGIS or another GIS.");
          } catch (e) { ui.toast("Could not copy; the link is " + url, "bad"); }
        };
        // Colour by: the facets with a choice of values, and the ranges of numbers; those the
        // shapes suggest (tts:colorBy) first, lowest first.
        const suggested = (f) => (f.colorBy == null ? Infinity : f.colorBy);
        const colourable = [...choice, ...ranges.filter((f) => numeric(f) && f.min < f.max)]
          .map((f, i) => [f, i]).sort((a, b) => suggested(a[0]) - suggested(b[0]) || a[1] - b[1]).map(([f]) => f);
        const by = colors[c.id] ? colourable.find((f) => f.key === colors[c.id]) || null : null;
        const coloured = paint(c, by);
        const unit = (f) => (f.unit ? " " + f.unit : "");
        const key = by && by.range ? el("ul", { class: "map-key" },
            ...coloured.map((k, i) => el("li", { class: "small" }, el("span", { class: "map-swatch", style: "background:" + k.colour }),
              el("span", { class: "map-name" }, (k.label ? k.label + " " : ""),
                el("span", { class: "muted" }, k.upTo != null ? "≤ " + fmtBound(k.upTo) + unit(by) : i ? "> " + fmtBound(coloured[i - 1].upTo) + unit(by) : "")))),
            el("li", { class: "small muted" }, el("span", { class: "map-swatch", style: "background:" + OTHER_COLOUR }), "no number"))
          : by ? el("ul", { class: "map-key" },
            ...coloured.map((v, i) => el("li", { class: "small" }, el("span", { class: "map-swatch", style: "background:" + VALUE_COLOURS[i] }),
              el("span", { class: "map-name", title: (v.label ? v.label + "\n" : "") + v.value }, v.label || short(v.value)))),
            by.values.length > coloured.length || by.more ? el("li", { class: "small muted" }, el("span", { class: "map-swatch", style: "background:" + OTHER_COLOUR }), "other values") : null) : null;
        const colourBy = colourable.length ? el("div", { class: "map-colour" },
          el("label", { class: "small" }, "Colour by ",
            el("select", { onchange: (e) => {
              if (e.target.value) colors[c.id] = e.target.value; else { delete colors[c.id]; paint(c, null); }
              refilter();
              load();
            } }, el("option", { value: "" }, "one colour"),
            ...colourable.map((f) => el("option", { value: f.key, selected: by === f }, f.title + unit(f))))),
          key) : null;
        target.replaceChildren(...[
          colourBy,
          el("div", { class: "map-facet-tools" },
            filtered ? el("button", { class: "btn small", type: "button", onclick: () => { delete chosen[c.id]; refilter(); load(); } }, "Clear filters") : null,
            el("a", { class: "btn small", href: ogcItems(ds, c, chosen[c.id], "html"), target: "_blank", rel: "noopener", title: "These features in the OGC API – Features" }, "OGC API"),
            el("button", { class: "btn small", type: "button", onclick: copy, title: "Copy the OGC API link of these features, as GeoJSON, for a GIS" }, "Copy GIS link"),
            c.class ? el("a", { class: "btn small", href: "#", title: "Find " + c.title + " in the area the map shows, with conditions on any property, on the Search page",
              onclick: (e) => { e.preventDefault(); location.href = searchArea(ds, c); } }, "Search this area") : null,
            out.query ? el("a", { class: "btn small", href: queryLink(ds, out.query), target: "_blank", rel: "noopener", title: "These features as a SPARQL query, in a new query tab" }, "Query") : null),
          // Names first; other text, and any searched, fold away when unused.
          ...textBoxes(texts.filter((f) => isName(f) || searched(f))),
          texts.some((f) => !isName(f) && !searched(f)) ? el("details", { class: "map-facet" },
            el("summary", { class: "muted" }, `Search other text (${texts.filter((f) => !isName(f) && !searched(f)).length})`),
            ...textBoxes(texts.filter((f) => !isName(f) && !searched(f)))) : null,
          // The facets the picked view lists first, then the others: values before ranges.
          ...[...choice, ...ranges].map((f, i) => [f, i]).sort((a, b) => rank(a[0]) - rank(b[0]) || a[1] - b[1])
            .map(([f]) => (f.range ? rangeBox(f) : valuesBox(f))),
          same.length ? el("details", { class: "map-facet" },
            el("summary", { class: "muted" }, `Same for all (${same.length})`),
            ...same.map((f) => el("p", { class: "small map-same" }, el("span", { class: "muted" }, f.title + ": "), f.values[0].label || YES_NO[f.values[0].value] || short(f.values[0].value)))) : null,
          !out.facets.length ? el("p", { class: "muted small" }, "No facets: give a property shape an tts:facet.") : null,
        ].filter(Boolean));
      };
      open.addEventListener("click", () => {
        const show = panel.hidden;
        panel.hidden = !show;
        open.setAttribute("aria-expanded", String(show));
        if (show) load();
      });
      // A filter from the link: its counts and choices show at once.
      if (chosen[c.id] || colors[c.id]) {
        panel.hidden = false;
        open.setAttribute("aria-expanded", "true");
        load();
      }
      return el("div", { class: "map-entry", "data-id": c.id },
        el("div", { class: "map-layer", "data-min": String(c.minZoom || 0), title },
          el("label", { class: "map-check" }, box, el("span", { class: "map-swatch", style: "background:" + c.color }), el("span", { class: "map-name" }, c.title)),
          count,
          c.minZoom ? el("span", { class: "muted small map-from" }, "from zoom " + c.minZoom) : null,
          open),
        panel);
    }));
  }

  /** The Search page for collection `c`'s class within the area the map shows. */
  function searchArea(ds, c) {
    const b = map.getBounds();
    const r = (x, lo, hi) => Math.round(Math.min(hi, Math.max(lo, x)) * 1e5) / 1e5;
    const area = [r(b.getWest(), -180, 180), r(b.getSouth(), -90, 90), r(b.getEast(), -180, 180), r(b.getNorth(), -90, 90)];
    return "/$/ui/search/" + encodeURIComponent(ds) + "?m=" + encodeURIComponent(JSON.stringify({ class: c.class, where: [], show: [], area }));
  }

  /** Fits the map to the collections' extents (from the OGC API), or keeps the world. */
  async function fit(ds) {
    try {
      const out = await getJson(service(ds, "ogc/collections"));
      let b = null;
      for (const c of out.collections || []) {
        const bbox = c.extent && c.extent.spatial && c.extent.spatial.bbox && c.extent.spatial.bbox[0];
        if (!bbox || bbox.length < 4 || !bbox.every(Number.isFinite)) continue;
        b = b ? [Math.min(b[0], bbox[0]), Math.min(b[1], bbox[1]), Math.max(b[2], bbox[2]), Math.max(b[3], bbox[3])] : bbox.slice(0, 4);
      }
      if (b && map && shown === ds) map.fitBounds([[b[0], Math.max(b[1], -85)], [b[2], Math.min(b[3], 85)]], { padding: 40, maxZoom: 16, animate: false });
    } catch (e) { /* the world view stays */ }
  }

  /** Lists the features under a click, with labels, each linking to its describe page. */
  async function pick(ds, e, collections, layers) {
    const hits = map.queryRenderedFeatures(box(e.point, 4), { layers });
    const byId = new Map(collections.map((c) => [c.id, c]));
    const seen = new Map();
    for (const f of hits) {
      const iri = f.properties && f.properties.iri;
      if (iri && !seen.has(iri)) seen.set(iri, byId.get(f.sourceLayer) || { title: f.sourceLayer, color: "#888" });
    }
    if (!seen.size) return;
    const picked = [...seen].slice(0, MAX_PICKED);
    const links = new Map();
    const list = el("ul", { class: "map-picked" }, picked.map(([iri, c]) => {
      const a = el("a", { href: describeUrl(ds, iri), title: iri, onclick: (ev) => {
        if (ev.ctrlKey || ev.metaKey || ev.shiftKey || ev.button !== 0) return;
        ev.preventDefault();
        detail(ds, iri, a.textContent);
      } }, short(iri));
      links.set(iri, a);
      return el("li", {}, el("span", { class: "map-swatch", style: "background:" + c.color, title: c.title }), el("span", {}, a, el("span", { class: "muted small" }, " " + c.title)), ui.copyButton(iri));
    }));
    const body = el("div", { class: "map-popup" }, list,
      seen.size > MAX_PICKED ? el("p", { class: "muted small" }, `and ${seen.size - MAX_PICKED} more: zoom in`) : null);
    new lib.Popup({ maxWidth: "360px" }).setLngLat(e.lngLat).setDOMContent(body).addTo(map);
    // One feature: its page opens beside the map at once.
    if (seen.size === 1) detail(ds, picked[0][0], short(picked[0][0]));
    try {
      for (const [iri, label] of await labels(ds, picked.map(([iri]) => iri))) {
        const a = links.get(iri);
        if (a) a.textContent = label;
        // The panel opened before the labels came: its title too.
        for (const t of document.querySelectorAll("#map-detail .map-detail-head strong")) if (t.title === iri) t.textContent = label;
      }
    } catch (err) { /* the short names stay */ }
  }

  /** The feature open beside the map, in the page's link (`feature=IRI`), so a shared link
   *  opens it too; null when the panel closes. */
  function setFeature(iri) {
    const url = new URL(location.href);
    if (iri) url.searchParams.set("feature", iri); else url.searchParams.delete("feature");
    if (url.href !== location.href) history.replaceState(history.state, "", url.pathname + url.search + url.hash);
  }

  const describeUrl = (ds, iri) => service(ds, "describe?iri=" + encodeURIComponent(iri));

  /** The detail panel: the feature's describe page beside the map, fetched with the caller's
   *  headers (so it works on datasets that need a key), its links to other resources opening
   *  in the panel too. */
  async function detail(ds, iri, title) {
    const panel = document.getElementById("map-detail");
    if (!panel) return;
    const wrap = panel.parentElement;
    const close = el("button", { class: "icon-btn close", type: "button", title: "Close", "aria-label": "Close", onclick: () => {
      setFeature(null);
      panel.hidden = true;
      wrap.classList.remove("with-detail");
      if (map) map.resize();
    } }, "✕");
    const frame = el("iframe", { class: "map-frame", title: "Description of " + title });
    panel.replaceChildren(
      el("div", { class: "map-detail-head" },
        el("strong", { class: "map-name", title: iri }, title),
        ui.copyButton(iri),
        el("a", { class: "btn small", href: describeUrl(ds, iri), target: "_blank", rel: "noopener" }, "Open page"),
        el("a", { class: "btn small", href: "/$/ui/explore/" + encodeURIComponent(ds) + "?iri=" + encodeURIComponent(iri), "data-route": true,
          title: "Walk the graph from this feature" }, "Explore"),
        close),
      frame);
    panel.hidden = false;
    setFeature(iri);
    if (!wrap.classList.contains("with-detail")) {
      wrap.classList.add("with-detail");
      if (map) map.resize();
    }
    let html;
    try {
      const r = await fetch(describeUrl(ds, iri) + "&format=html", { headers: { Accept: "text/html", ...headers() } });
      html = await r.text();
      if (!r.ok) throw new Error(html.trim() || r.statusText);
    } catch (e) {
      frame.replaceWith(el("p", { class: "error small map-frame-error" }, "The description could not be loaded: " + (e.message || e)));
      return;
    }
    // Root-relative links and scripts resolve against the server.
    const base = '<base href="' + location.origin + '/" target="_blank">';
    frame.srcdoc = /<head[^>]*>/i.test(html) ? html.replace(/<head[^>]*>/i, (h) => h + base) : base + html;
    frame.addEventListener("load", () => {
      const doc = frame.contentDocument;
      if (!doc) return;
      doc.addEventListener("click", (ev) => {
        const a = ev.target.closest && ev.target.closest("a[href]");
        if (!a || ev.ctrlKey || ev.metaKey || ev.shiftKey || ev.button !== 0) return;
        const url = new URL(a.getAttribute("href"), location.origin + "/");
        const next = url.origin === location.origin && url.pathname === "/" + encodeURIComponent(ds) + "/describe" && url.searchParams.get("iri");
        if (next) {
          ev.preventDefault();
          detail(ds, next, a.textContent.trim() || short(next));
        } else if (url.origin === location.origin) {
          // Any other page of the server opens in a new tab, not inside the panel.
          a.target = "_blank";
        }
      });
    }, { once: true });
  }

  /** The end of an IRI, after its last `/`, `#` or `:`. */
  /** A range facet's colour classes, low to high: `{upTo, label, colour}`, the last with
   *  `upTo` null. Its shape's tts:colorClasses, else five of equal width over its values,
   *  on round numbers. */
  function rangeClasses(f) {
    let classes = (f.classes || []).filter((k, i, all) => i === all.length - 1 || typeof k.upTo === "number");
    if (classes.length < 2 || classes.length > RANGE_COLOURS.length) {
      const raw = (f.max - f.min) / 5;
      const power = Math.pow(10, Math.floor(Math.log10(raw)));
      const width = [1, 2, 2.5, 5, 10].map((m) => m * power).find((w) => w >= raw) || raw;
      classes = [];
      for (let up = Math.floor(f.min / width) * width + width; up < f.max && classes.length < RANGE_COLOURS.length - 1; up += width) {
        classes.push({ upTo: Number(up.toPrecision(12)), label: null });
      }
      classes.push({ upTo: null, label: null });
    }
    const n = classes.length;
    return classes.map((k, i) => ({
      upTo: i === n - 1 ? null : k.upTo,
      label: k.label || null,
      colour: RANGE_COLOURS[n === 1 ? 0 : Math.round((i * (RANGE_COLOURS.length - 1)) / (n - 1))],
    }));
  }

  /** A class bound as people write it. */
  function fmtBound(n) {
    return Number.isInteger(n) ? ui.fmtNum(n) : String(Number(n.toPrecision(6)));
  }

  function short(iri) {
    const tail = iri.replace(/[/#:]+$/, "").replace(/^.*[/#:]/, "");
    try { return decodeURIComponent(tail) || iri; } catch (e) { return tail || iri; }
  }

  /** A label of each IRI (rdfs:label, skos:prefLabel, schema:name, dct:title), by one query. */
  async function labels(ds, iris) {
    const safe = iris.filter((i) => !/[\s<>"{}|^`\\]/.test(i));
    if (!safe.length) return [];
    const query = "SELECT ?s (SAMPLE(?l) AS ?label) WHERE { VALUES ?s { " + safe.map((i) => "<" + i + ">").join(" ") +
      " } VALUES ?p { " + LABELS.map((p) => "<" + p + ">").join(" ") + " } ?s ?p ?l } GROUP BY ?s";
    const r = await fetch(service(ds, "sparql"), {
      method: "POST",
      headers: { "Content-Type": "application/sparql-query", Accept: "application/sparql-results+json", ...headers() },
      body: query,
    });
    if (!r.ok) return [];
    const out = await r.json();
    return out.results.bindings.filter((b) => b.label).map((b) => [b.s.value, b.label.value]);
  }

  /** Called when another page opens: the map stops. */
  function leave() {
    if (map) { map.remove(); map = null; }
    shown = null;
  }

  return { render, leave };
})();
