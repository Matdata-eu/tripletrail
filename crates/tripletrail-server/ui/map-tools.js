// The Map page's tools (#33): an area to filter the map by (drawn, or a feature's geometry),
// measuring lengths and areas, and the coordinates under the pointer. map.js calls
// TripleTrailMapTools.panel(ctx) once the map is made.
//
// The area goes into the page's link and the tiles', facets' and OGC API's requests as
// `area` (WKT, or a feature's IRI), `rel` and `dist`, as the server takes them. A rectangle
// and a polygon are drawn as POLYGON, a circle as its centre (POINT) with `rel=dwithin` and
// the radius as `dist`, a corridor as its line (LINESTRING) with `rel=dwithin`.
//
// The geometry at the top (geodesic lengths, areas, bearings, WKT) needs no browser, so
// `node --test "crates/tripletrail-server/ui/test/*.test.js"` checks it.
"use strict";

(function (root) {
  /** The Earth's mean radius, in metres, and its circumference at the equator. */
  const R = 6371008.8;
  const EQUATOR = 40075016.686;
  const rad = (d) => (d * Math.PI) / 180;
  const deg = (r) => (r * 180) / Math.PI;

  /** The great-circle distance between two [lon, lat] points, in metres. */
  function distance(a, b) {
    const dφ = rad(b[1] - a[1]);
    const dλ = rad(b[0] - a[0]);
    const h = Math.sin(dφ / 2) ** 2 + Math.cos(rad(a[1])) * Math.cos(rad(b[1])) * Math.sin(dλ / 2) ** 2;
    return 2 * R * Math.asin(Math.min(1, Math.sqrt(h)));
  }

  /** The initial bearing from `a` to `b`, in degrees from north, clockwise (0 to 360). */
  function bearing(a, b) {
    const φ1 = rad(a[1]);
    const φ2 = rad(b[1]);
    const dλ = rad(b[0] - a[0]);
    const y = Math.sin(dλ) * Math.cos(φ2);
    const x = Math.cos(φ1) * Math.sin(φ2) - Math.sin(φ1) * Math.cos(φ2) * Math.cos(dλ);
    return (deg(Math.atan2(y, x)) + 360) % 360;
  }

  /** The point `metres` from `p` on bearing `degrees`. */
  function destination(p, metres, degrees) {
    const δ = metres / R;
    const θ = rad(degrees);
    const φ1 = rad(p[1]);
    const λ1 = rad(p[0]);
    const φ2 = Math.asin(Math.sin(φ1) * Math.cos(δ) + Math.cos(φ1) * Math.sin(δ) * Math.cos(θ));
    const λ2 = λ1 + Math.atan2(Math.sin(θ) * Math.sin(δ) * Math.cos(φ1), Math.cos(δ) - Math.sin(φ1) * Math.sin(φ2));
    return [((deg(λ2) + 540) % 360) - 180, deg(φ2)];
  }

  /** A line's length, in metres. */
  function length(points) {
    let sum = 0;
    for (let i = 1; i < points.length; i++) sum += distance(points[i - 1], points[i]);
    return sum;
  }

  /** The area of a ring on the sphere, in square metres (open or closed alike). */
  function ringArea(ring) {
    const pts = closed(ring);
    let sum = 0;
    for (let i = 1; i < pts.length; i++) {
      const [a, b] = [pts[i - 1], pts[i]];
      sum += rad(b[0] - a[0]) * (2 + Math.sin(rad(a[1])) + Math.sin(rad(b[1])));
    }
    return Math.abs((sum * R * R) / 2);
  }

  /** A ring's length all round, in metres. */
  const perimeter = (ring) => length(closed(ring));

  /** A ring with its first point again at its end. */
  function closed(ring) {
    if (!ring.length) return [];
    const [f, l] = [ring[0], ring[ring.length - 1]];
    return f[0] === l[0] && f[1] === l[1] ? ring.slice() : [...ring, f];
  }

  /** A circle of `metres` around `center`, as a closed ring of `n` sides. */
  function circle(center, metres, n = 64) {
    const ring = [];
    for (let i = 0; i < n; i++) ring.push(destination(center, metres, (360 * i) / n));
    return closed(ring);
  }

  /** A coordinate to 6 decimals (about 0.1 m), as WKT writes it. */
  const fix = (x) => Math.round(x * 1e6) / 1e6;
  const pair = (p) => fix(p[0]) + " " + fix(p[1]);

  /** A GeoJSON Point, LineString or Polygon (its outer ring) as WKT in longitude and latitude. */
  function toWkt(g) {
    if (g.type === "Point") return "POINT(" + pair(g.coordinates) + ")";
    if (g.type === "LineString") return "LINESTRING(" + g.coordinates.map(pair).join(", ") + ")";
    if (g.type === "Polygon") return "POLYGON((" + closed(g.coordinates[0]).map(pair).join(", ") + "))";
    throw new Error("No WKT for a " + g.type);
  }

  /** WKT of a POINT, LINESTRING or POLYGON in longitude and latitude, as GeoJSON; null for
   *  anything else (another CRS, other types), which the map does not draw. */
  function fromWkt(text) {
    let t = String(text || "").trim();
    const crs = /^<([^>]*)>\s*/.exec(t);
    if (crs) {
      if (!/CRS84$/i.test(crs[1])) return null;
      t = t.slice(crs[0].length);
    }
    const m = /^(POINT|LINESTRING|POLYGON)\s*\(([\s\S]*)\)$/i.exec(t);
    if (!m) return null;
    const points = (s) => s.split(",").map((p) => p.trim().split(/\s+/).slice(0, 2).map(Number));
    const ok = (pts) => pts.length && pts.every((p) => p.length === 2 && p.every(Number.isFinite));
    const kind = m[1].toUpperCase();
    if (kind === "POINT") {
      const p = points(m[2]);
      return p.length === 1 && ok(p) ? { type: "Point", coordinates: p[0] } : null;
    }
    if (kind === "LINESTRING") {
      const p = points(m[2]);
      return p.length >= 2 && ok(p) ? { type: "LineString", coordinates: p } : null;
    }
    const outer = /^\s*\(([^()]*)\)/.exec(m[2]);
    const p = outer && points(outer[1]);
    return p && p.length >= 4 && ok(p) ? { type: "Polygon", coordinates: [p] } : null;
  }

  /** Metres as people read them: "850 m", "1.25 km", "125 km". */
  function fmtLength(m) {
    if (m < 10) return m.toFixed(1) + " m";
    if (m < 1000) return Math.round(m) + " m";
    const km = m / 1000;
    return (km < 10 ? km.toFixed(2) : km < 100 ? km.toFixed(1) : String(Math.round(km))) + " km";
  }

  /** Square metres as people read them: "450 m²", "12.5 ha", "3.21 km²". */
  function fmtArea(m2) {
    if (m2 < 10000) return Math.round(m2) + " m²";
    if (m2 < 1e6) return (m2 / 1e4).toFixed(m2 < 1e5 ? 2 : 1) + " ha";
    const km2 = m2 / 1e6;
    return (km2 < 10 ? km2.toFixed(2) : km2 < 100 ? km2.toFixed(1) : String(Math.round(km2))) + " km²";
  }

  /** A bearing as degrees and the nearest of eight compass points: "45° NE". */
  function fmtBearing(b) {
    const points = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    return Math.round(b) % 360 + "° " + points[Math.round(b / 45) % 8];
  }

  /** A [lon, lat] point as latitude, longitude: "50.85012, 4.35123". */
  const fmtPoint = (p) => p[1].toFixed(5) + ", " + p[0].toFixed(5);

  /** A band `metres` wide on each side of a line, as a line width that grows with the zoom
   *  (MapLibre's 512-pixel world at zoom 0), at latitude `lat`. */
  function bandWidth(metres, lat) {
    const perPixel = (EQUATOR * Math.cos(rad(lat))) / 512;
    return (2 * metres) / perPixel;
  }

  const geometry = { distance, bearing, destination, length, ringArea, perimeter, circle, toWkt, fromWkt, fmtLength, fmtArea, fmtBearing, fmtPoint, bandWidth };

  /** The relations a feature's geometry can have to the area, as `rel` takes them. */
  const RELATIONS = [
    ["intersects", "meets it"],
    ["within", "lies within it"],
    ["contains", "contains it"],
    ["touches", "touches its edge"],
    ["crosses", "crosses it"],
    ["overlaps", "overlaps it"],
    ["disjoint", "lies outside it"],
    ["dwithin", "is within a distance of it"],
  ];
  /** The width of a corridor, and the radius of the area round a feature, unless set. */
  const DEFAULT_DIST = 250;
  const MAX_DIST = 1000000;
  /** The tools' colour on the map: an orange no layer is given by default. */
  const INK = "#e8590c";
  const SOURCE = "map-tools";
  const IS_IRI = /^(https?:|urn:)/i;
  /** What each drawing tool asks for while it is on. */
  const HINTS = {
    rect: "Click two opposite corners.",
    polygon: "Click the corners; click the first again, double-click or press Enter to finish.",
    circle: "Click the centre, then a point on the edge.",
    corridor: "Click along the line; double-click or press Enter to finish.",
    distance: "Click along the way; double-click or press Enter to finish.",
    "measure-area": "Click the corners; click the first again, double-click or press Enter to finish.",
    point: "Click a point to copy it as WKT.",
  };
  const END = "Backspace takes the last point back, Esc stops.";

  /** The tools' map control and panel. `ctx`: {map, ui, icon(name), getArea(), setArea(area
   *  or null)}, the area being {area, rel, dist}. Returns {control, attach(), click(e),
   *  busy(), useFeature(iri, label), refresh()}. */
  function panel(ctx) {
    const { map, ui } = ctx;
    const el = (...a) => ui.el(...a);
    /** The drawing under way: {kind, pts: [[lon, lat]], hover: [lon, lat] | null}. */
    let mode = null;
    /** The last measurement, shown until another starts or it is cleared: {kind, pts}. */
    let measured = null;
    /** The relation and distance the next area is drawn with. */
    const pref = { rel: "intersects", dist: DEFAULT_DIST };
    /** The label of the feature whose geometry is the area. */
    let areaLabel = null;
    let pointer = null;
    let liveResults = null;
    let liveCoords = null;

    const wrap = el("div", { class: "maplibregl-ctrl maplibregl-ctrl-group map-ctrl map-tools-wrap" });
    const box = el("div", { class: "map-tools card", id: "map-tools", role: "region", "aria-label": "Map tools", hidden: true });
    const toggle = el("button", { type: "button", class: "map-ctrl-btn", title: "Tools: filter by an area, measure, coordinates", "aria-label": "Map tools",
      "aria-expanded": "false", "aria-controls": "map-tools", onclick: () => show(box.hidden) }, ctx.icon("tools"));
    wrap.append(toggle, box);
    const show = (on) => {
      box.hidden = !on;
      toggle.setAttribute("aria-expanded", String(on));
      toggle.classList.toggle("on", on);
      if (!on && mode) stop();
      if (on) fill();
    };

    const near = (point, p) => {
      const q = map.project(p);
      return Math.hypot(q.x - point.x, q.y - point.y) <= 6;
    };
    const lngLat = (e) => { const w = e.lngLat.wrap(); return [w.lng, w.lat]; };

    function start(kind) {
      if (mode && mode.kind === kind) return stop();
      mode = { kind, pts: [], hover: null };
      if (kind === "distance" || kind === "measure-area") measured = null;
      map.doubleClickZoom.disable();
      map.getCanvas().style.cursor = "crosshair";
      draw();
      fill();
    }
    function stop() {
      mode = null;
      map.doubleClickZoom.enable();
      map.getCanvas().style.cursor = "";
      draw();
      fill();
    }
    /** A click on the map: taken by the tool that is on (true), or left to the map. */
    function click(e) {
      if (!mode) return false;
      const p = lngLat(e);
      const { kind, pts } = mode;
      if (kind === "point") {
        ui.copyText(geometry.toWkt({ type: "Point", coordinates: p }), "Point");
        stop();
        return true;
      }
      // A double-click's second click lands on the point just put.
      if (pts.length && near(e.point, pts[pts.length - 1])) return true;
      if ((kind === "polygon" || kind === "measure-area") && pts.length >= 3 && near(e.point, pts[0])) {
        finish();
        return true;
      }
      pts.push(p);
      if ((kind === "rect" || kind === "circle") && pts.length === 2) finish();
      else { draw(); live(); }
      return true;
    }
    function finish() {
      if (!mode) return;
      const { kind, pts } = mode;
      const rel = pref.rel === "dwithin" ? "intersects" : pref.rel;
      let area = null;
      if (kind === "rect" && pts.length === 2) {
        const [a, b] = pts;
        if (a[0] === b[0] || a[1] === b[1]) return stop();
        area = { area: geometry.toWkt({ type: "Polygon", coordinates: [[a, [b[0], a[1]], b, [a[0], b[1]]]] }), rel };
      } else if (kind === "polygon") {
        if (pts.length < 3) return;
        area = { area: geometry.toWkt({ type: "Polygon", coordinates: [pts] }), rel };
      } else if (kind === "circle" && pts.length === 2) {
        const r = Math.max(1, Math.round(geometry.distance(pts[0], pts[1])));
        area = { area: geometry.toWkt({ type: "Point", coordinates: pts[0] }), rel: "dwithin", dist: Math.min(r, MAX_DIST) };
      } else if (kind === "corridor") {
        if (pts.length < 2) return;
        area = { area: geometry.toWkt({ type: "LineString", coordinates: pts }), rel: "dwithin", dist: pref.dist };
      } else if (kind === "distance") {
        if (pts.length < 2) return;
        measured = { kind, pts: pts.slice() };
      } else if (kind === "measure-area") {
        if (pts.length < 3) return;
        measured = { kind, pts: pts.slice() };
      }
      mode = null;
      if (area) { areaLabel = null; ctx.setArea(area); }
      stop();
    }

    /** The area, the measurement and the drawing under way, on the map. */
    function draw() {
      const src = map.getSource(SOURCE);
      if (!src) return;
      const features = [];
      const add = (role, g, props = {}) => features.push({ type: "Feature", properties: { role, ...props }, geometry: g });
      const vertices = (pts) => pts.forEach((p) => add("vertex", { type: "Point", coordinates: p }));
      const a = ctx.getArea();
      const g = a && !IS_IRI.test(a.area) ? geometry.fromWkt(a.area) : null;
      if (g) {
        const dist = a.rel === "dwithin" ? Number(a.dist) || 0 : 0;
        if (g.type === "Point" && dist > 0) add("area", { type: "Polygon", coordinates: [geometry.circle(g.coordinates, dist)] });
        else add("area", g);
        if (g.type !== "Point" && dist > 0) {
          const pts = g.type === "Polygon" ? g.coordinates[0] : g.coordinates;
          const lat = pts.reduce((s, p) => s + p[1], 0) / pts.length;
          const line = g.type === "Polygon" ? { type: "LineString", coordinates: pts } : g;
          add("band", line, { w0: geometry.bandWidth(dist, lat) });
        }
      }
      if (measured) {
        const pts = measured.pts;
        add("measure", measured.kind === "distance" ? { type: "LineString", coordinates: pts } : { type: "Polygon", coordinates: [[...pts, pts[0]]] });
        vertices(pts);
      }
      if (mode && mode.kind !== "point") {
        const pts = mode.hover ? [...mode.pts, mode.hover] : mode.pts;
        const k = mode.kind;
        if (k === "rect" && pts.length === 2) {
          const [p, q] = pts;
          add("sketch", { type: "Polygon", coordinates: [[p, [q[0], p[1]], q, [p[0], q[1]], p]] });
        } else if (k === "circle" && pts.length === 2) {
          add("sketch", { type: "Polygon", coordinates: [geometry.circle(pts[0], geometry.distance(pts[0], pts[1]))] });
          add("sketch", { type: "LineString", coordinates: pts });
        } else if ((k === "polygon" || k === "measure-area") && pts.length >= 3) {
          add("sketch", { type: "Polygon", coordinates: [[...pts, pts[0]]] });
        } else if (pts.length >= 2) {
          add("sketch", { type: "LineString", coordinates: pts });
          if (k === "corridor") {
            add("band", { type: "LineString", coordinates: pts }, { w0: geometry.bandWidth(pref.dist, pts[0][1]) });
          }
        }
        vertices(mode.pts);
      }
      src.setData({ type: "FeatureCollection", features });
    }

    /** The source and layers of the tools, over the dataset's: called once the map loaded. */
    function attach() {
      if (map.getSource(SOURCE)) return;
      map.addSource(SOURCE, { type: "geojson", data: { type: "FeatureCollection", features: [] } });
      const role = (...r) => ["in", ["get", "role"], ["literal", r]];
      const polygon = ["==", ["geometry-type"], "Polygon"];
      map.addLayer({ id: "map-tools-band", type: "line", source: SOURCE, filter: ["==", ["get", "role"], "band"],
        layout: { "line-cap": "round", "line-join": "round" },
        paint: { "line-color": INK, "line-opacity": 0.14,
          "line-width": ["interpolate", ["exponential", 2], ["zoom"], 0, ["get", "w0"], 24, ["*", ["get", "w0"], 16777216]] } });
      map.addLayer({ id: "map-tools-fill", type: "fill", source: SOURCE, filter: ["all", polygon, role("area", "sketch", "measure")],
        paint: { "fill-color": INK, "fill-opacity": ["case", ["==", ["get", "role"], "area"], 0.1, 0.06] } });
      map.addLayer({ id: "map-tools-line", type: "line", source: SOURCE, filter: ["all", ["!=", ["geometry-type"], "Point"], role("area", "sketch", "measure")],
        layout: { "line-join": "round" },
        paint: { "line-color": INK, "line-width": 2, "line-dasharray": ["case", ["==", ["get", "role"], "area"], ["literal", [1, 0]], ["literal", [2, 1.5]]] } });
      map.addLayer({ id: "map-tools-point", type: "circle", source: SOURCE, filter: ["all", ["==", ["geometry-type"], "Point"], role("area", "vertex")],
        paint: { "circle-radius": ["case", ["==", ["get", "role"], "area"], 6, 4], "circle-color": "#fff", "circle-stroke-color": INK, "circle-stroke-width": 2 } });
      map.on("mousemove", (e) => {
        pointer = lngLat(e);
        if (mode) { mode.hover = pointer; draw(); }
        live();
      });
      map.on("dblclick", (e) => { if (mode) { e.preventDefault(); finish(); } });
      document.addEventListener("keydown", onKey);
      draw();
    }
    /** Esc, Enter and Backspace while a tool is on; Enter and Backspace stay with a focused
     *  field, button or link. */
    function onKey(e) {
      if (!mode) return;
      if (e.key === "Escape") return stop();
      const t = e.target;
      if (t && (/^(INPUT|SELECT|TEXTAREA|BUTTON|A)$/.test(t.tagName) || t.isContentEditable)) return;
      if (e.key === "Enter") { e.preventDefault(); finish(); }
      else if (e.key === "Backspace" && mode.pts.length) { e.preventDefault(); mode.pts.pop(); draw(); live(); }
    }
    /** The map is gone (another page opened): the key listener goes, and the tool with it. */
    function detach() {
      document.removeEventListener("keydown", onKey);
      mode = null;
      measured = null;
      wrap.remove();
    }

    /** The measurement under way or last made, as lines of text. */
    function results() {
      const m = mode && (mode.kind === "distance" || mode.kind === "measure-area") ? mode : measured;
      if (!m) return [];
      const pts = m === mode && m.hover ? [...m.pts, m.hover] : m.pts;
      if (m.kind === "distance") {
        if (pts.length < 2) return [];
        const legs = [];
        for (let i = 1; i < pts.length; i++) legs.push(el("li", {}, geometry.fmtLength(geometry.distance(pts[i - 1], pts[i])), el("span", { class: "muted" }, " · " + geometry.fmtBearing(geometry.bearing(pts[i - 1], pts[i])))));
        return [el("p", {}, el("strong", {}, "Length " + geometry.fmtLength(geometry.length(pts)))),
          legs.length > 1 ? el("ol", { class: "map-tools-legs small" }, legs.slice(-20)) : el("p", { class: "small muted" }, "Bearing " + geometry.fmtBearing(geometry.bearing(pts[0], pts[1])))];
      }
      if (pts.length < 3) return [];
      return [el("p", {}, el("strong", {}, "Area " + geometry.fmtArea(geometry.ringArea(pts)))),
        el("p", { class: "small muted" }, "Perimeter " + geometry.fmtLength(geometry.perimeter(pts)))];
    }
    /** The parts of the panel that follow the pointer. */
    function live() {
      if (box.hidden) return;
      if (liveCoords) liveCoords.textContent = pointer ? geometry.fmtPoint(pointer) : "–";
      if (liveResults) liveResults.replaceChildren(...results());
    }

    /** What the area is, in words. */
    function describe(a) {
      if (!a) return "No area: draw one, or open a feature and filter by it.";
      if (IS_IRI.test(a.area)) return "The geometry of " + (areaLabel || a.area) + ".";
      const g = geometry.fromWkt(a.area);
      if (!g) return "An area given as WKT.";
      if (g.type === "Point") return a.rel === "dwithin" ? "A circle of " + geometry.fmtLength(Number(a.dist) || 0) + " round a point." : "A point.";
      if (g.type === "LineString") return (a.rel === "dwithin" ? "A corridor along a line, " + geometry.fmtLength(geometry.length(g.coordinates)) + " long." : "A line.");
      return "An area of " + geometry.fmtArea(geometry.ringArea(g.coordinates[0])) + ".";
    }

    /** The panel, from the area and the tool that is on. */
    function fill() {
      if (box.hidden) return;
      const a = ctx.getArea();
      const rel = a ? a.rel || "intersects" : pref.rel;
      const dist = a && a.rel === "dwithin" ? a.dist : pref.dist;
      const tool = (kind, label, title) => el("button", { type: "button", class: "btn small" + (mode && mode.kind === kind ? " primary" : ""),
        "aria-pressed": String(!!(mode && mode.kind === kind)), title, onclick: () => start(kind) }, label);
      const relation = el("select", { "aria-label": "How a feature's geometry relates to the area", onchange: (e) => {
        pref.rel = e.target.value;
        const cur = ctx.getArea();
        if (cur) ctx.setArea(withRel(cur, pref.rel, pref.dist));
        refresh();
      } }, RELATIONS.map(([v, text]) => el("option", { value: v, selected: v === rel }, text)));
      const metres = el("input", { type: "number", min: "0", max: String(MAX_DIST), step: "any", value: String(dist), class: "map-tools-dist",
        "aria-label": "Distance in metres", onchange: (e) => {
          const d = Number(e.target.value);
          if (!(d >= 0 && d <= MAX_DIST)) { ui.toast("A distance from 0 to 1,000,000 m.", "bad"); e.target.value = String(pref.dist); return; }
          pref.dist = d;
          const cur = ctx.getArea();
          if (cur && cur.rel === "dwithin") ctx.setArea({ ...cur, dist: d });
          refresh();
        } });
      const close = el("button", { type: "button", class: "icon-btn close", title: "Close the tools", "aria-label": "Close the tools", onclick: () => { show(false); toggle.focus(); } }, "✕");
      liveResults = el("div", { class: "map-tools-results", "aria-live": "polite" });
      liveCoords = el("span", { class: "mono" }, pointer ? geometry.fmtPoint(pointer) : "–");
      const wkt = a && !IS_IRI.test(a.area) ? a.area : null;
      box.replaceChildren(
        el("div", { class: "map-tools-head" }, el("strong", {}, "Tools"), close),
        el("section", {},
          el("h4", {}, "Filter by an area"),
          el("p", { class: "small muted" }, describe(a)),
          el("div", { class: "map-tools-row" },
            tool("rect", "Rectangle", "Draw a rectangle"),
            tool("polygon", "Polygon", "Draw a polygon"),
            tool("circle", "Circle", "Draw a circle: its centre, then its edge"),
            tool("corridor", "Corridor", "Draw a line: the features within the distance of it")),
          el("label", { class: "map-tools-rel small" }, "Keep the features whose geometry ", relation),
          rel === "dwithin" || (mode && mode.kind === "corridor") ? el("label", { class: "map-tools-rel small" }, "Distance ", metres, " m") : null,
          a ? el("div", { class: "map-tools-row" },
            wkt ? el("button", { type: "button", class: "btn small", title: "Copy the area as WKT", onclick: () => ui.copyText(wkt, "WKT") }, "Copy WKT") : null,
            el("button", { type: "button", class: "btn small", title: "Show every feature again", onclick: () => { areaLabel = null; ctx.setArea(null); refresh(); } }, "Clear area")) : null),
        el("section", {},
          el("h4", {}, "Measure"),
          el("div", { class: "map-tools-row" },
            tool("distance", "Distance", "Measure a length, leg by leg"),
            tool("measure-area", "Area", "Measure an area and its perimeter"),
            measured ? el("button", { type: "button", class: "btn small", onclick: () => { measured = null; draw(); fill(); } }, "Clear") : null),
          liveResults),
        el("section", {},
          el("h4", {}, "Coordinates"),
          el("p", { class: "small" }, "Pointer ", liveCoords, el("span", { class: "muted" }, " (latitude, longitude)")),
          el("div", { class: "map-tools-row" }, tool("point", "Copy a point", "Click the map to copy that point as WKT"))),
        mode ? el("p", { class: "map-tools-hint small", role: "status" }, HINTS[mode.kind] + (mode.kind === "point" || mode.kind === "rect" || mode.kind === "circle" ? " Esc stops." : " " + END)) : "");
      live();
    }
    const withRel = (a, rel, dist) => (rel === "dwithin" ? { area: a.area, rel, dist: a.rel === "dwithin" ? a.dist : dist } : { area: a.area, rel });

    /** Draws and lists the area again, after it changed. */
    function refresh() { draw(); fill(); }

    /** The geometry of feature `iri` as the area, from the detail panel. */
    function useFeature(iri, label) {
      areaLabel = label || null;
      const rel = pref.rel;
      ctx.setArea(rel === "dwithin" ? { area: iri, rel, dist: pref.dist } : { area: iri, rel });
      show(true);
      refresh();
    }

    return { control: { onAdd: () => wrap, onRemove: detach }, attach, click, busy: () => !!mode, useFeature, refresh };
  }

  const api = { panel, geometry };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.TripleTrailMapTools = api;
})(typeof window !== "undefined" ? window : globalThis);
