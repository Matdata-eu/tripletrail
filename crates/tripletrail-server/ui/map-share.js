// The Map page's time slider, PNG export and "Cite this map" (#37). map.js calls
// TripleTrailMapShare.controls(ctx) once the map is made.
//
// The time goes into the page's link and the tiles', facets' and OGC API's requests as
// `datetime` (an instant, or `start/end` with `..` for an open end), as the server takes it;
// collections whose shapes mark no time (`tts:time`) ignore it. The export draws the map as
// it is, with a scale bar, a north arrow, the legend of the layers shown and the sources'
// credit, into one PNG. The citation gives the link of the map, the data version it shows
// and, per layer shown, the SPARQL that picks its features and its OGC API items.
//
// The parts at the top (days, scale bars, the citation's text) need no browser, so
// `node --test "crates/tripletrail-server/ui/test/*.test.js"` checks them.
"use strict";

(function (root) {
  const DAY = 86400000;
  const EQUATOR = 40075016.686;

  /** The day of an RFC 3339 date or date-time, `YYYY-MM-DD` (UTC), or null. */
  function day(value) {
    if (!value) return null;
    const t = Date.parse(/T/.test(value) ? value : value + "T00:00:00Z");
    return Number.isFinite(t) ? new Date(t).toISOString().slice(0, 10) : null;
  }
  /** The day `n` days after `from` (`YYYY-MM-DD`). */
  const addDays = (from, n) => new Date(Date.parse(from + "T00:00:00Z") + n * DAY).toISOString().slice(0, 10);
  /** Days from `a` to `b`. */
  const daysBetween = (a, b) => Math.round((Date.parse(b + "T00:00:00Z") - Date.parse(a + "T00:00:00Z")) / DAY);

  /** The span the slider covers: the union of the collections' temporal extents
   *  (`[[start, end]]`, either null for open), an open end as `today`, an open start a year
   *  before the end. Null without any time. */
  function span(intervals, today) {
    let lo = null;
    let hi = null;
    let any = false;
    for (const [start, end] of intervals) {
      any = true;
      const s = day(start);
      const e = end == null ? today : day(end);
      if (s && (!lo || s < lo)) lo = s;
      if (e && (!hi || e > hi)) hi = e;
    }
    if (!any) return null;
    if (!hi) hi = today;
    if (!lo) lo = addDays(hi, -365);
    if (lo > hi) [lo, hi] = [hi, lo];
    return { from: lo, to: hi, days: daysBetween(lo, hi) };
  }

  /** `datetime` for the days `from` to `to` (either "" for open): one day alone, an interval,
   *  or "" for no filter. */
  function datetime(from, to) {
    if (!from && !to) return "";
    if (from && from === to) return from;
    return (from || "..") + "/" + (to || "..");
  }
  /** The days of a `datetime` the slider can show: {from, to}, "" for an open end. */
  function parse(value) {
    if (!value) return { from: "", to: "" };
    const [a, b] = value.includes("/") ? value.split("/") : [value, value];
    const side = (v) => (v && v !== ".." ? day(v) || "" : "");
    return { from: side(a), to: side(b) };
  }

  /** A scale bar for `metresPerPixel`: the longest round length (1, 2 or 5 times a power of
   *  ten) at most `maxPixels` long, as {metres, pixels, label}. */
  function scaleBar(metresPerPixel, maxPixels) {
    const most = metresPerPixel * maxPixels;
    const power = 10 ** Math.floor(Math.log10(most));
    const metres = [5, 2, 1].map((m) => m * power).find((m) => m <= most) || power;
    const label = metres >= 1000 ? `${metres / 1000} km` : `${metres} m`;
    return { metres, pixels: metres / metresPerPixel, label };
  }
  /** Metres per CSS pixel at `lat` and `zoom` (512-pixel tiles, as MapLibre draws them). */
  const metresPerPixel = (lat, zoom) => (EQUATOR * Math.cos((lat * Math.PI) / 180)) / (512 * 2 ** zoom);

  /** The server's name (`--site-name`), as the page carries it. */
  function siteName() {
    return (typeof document !== "undefined" && document.documentElement.dataset.siteName) || "TripleTrail";
  }

  /** The citation of a map: {title, dataset, version, link, when (`YYYY-MM-DD`), time
   *  (`datetime` or ""), layers: [{title, sparql, items}]}, as plain text. */
  function citation(c) {
    const lines = [];
    lines.push(`${c.title}. Map of the dataset "${c.dataset}" in ${c.site || siteName()}` +
      (c.version ? `, data version ${c.version}` : "") + `. Retrieved ${c.when}.`);
    lines.push(c.link);
    if (c.time) lines.push(`Time: ${c.time}`);
    for (const l of c.layers) {
      lines.push("");
      lines.push(`Layer "${l.title}"`);
      if (l.items) lines.push(`OGC API items: ${l.items}`);
      if (l.sparql) lines.push("SPARQL:", l.sparql.trim());
    }
    return lines.join("\n");
  }

  /** The time, export and citation controls. `ctx`: {map, ui, ds, collections (each {id, title,
   *  color, time}), version, getTime(), setTime(datetime), extents() → Promise of
   *  [[start, end]] of the collections with a time, shown() → collections shown,
   *  layer(c) → Promise of {sparql, items}}. Returns {control}. */
  function controls(ctx) {
    const { map, ui } = ctx;
    const el = (...a) => ui.el(...a);
    const svg = (paths) => {
      const s = document.createElementNS("http://www.w3.org/2000/svg", "svg");
      s.setAttribute("viewBox", "0 0 24 24");
      s.setAttribute("aria-hidden", "true");
      s.setAttribute("class", "map-icon");
      s.innerHTML = paths;
      return s;
    };
    const wrap = el("div", { class: "maplibregl-ctrl maplibregl-ctrl-group map-ctrl map-tools-wrap" });
    const box = el("div", { class: "map-tools card", id: "map-share", role: "region", "aria-label": "Time, export and citation", hidden: true });
    const timed = ctx.collections.filter((c) => c.time);
    const buttons = [];
    const button = (key, title, paths, fill) => {
      const b = el("button", { type: "button", class: "map-ctrl-btn", title, "aria-label": title, "aria-expanded": "false", "aria-controls": "map-share",
        onclick: () => open(open.key === key ? null : key) }, svg(paths));
      b.dataset.key = key;
      buttons.push([b, fill]);
      return b;
    };
    const timeBtn = timed.length ? button("time", "Time: show the features of a day or a span", '<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>', fillTime) : null;
    button("export", "Export the map as a PNG", '<path d="M12 3v12M7 10l5 5 5-5"/><path d="M4 17v3h16v-3"/>', fillExport);
    button("cite", "Cite this map", '<path d="M7 7h4v4c0 3-2 5-4 6M15 7h4v4c0 3-2 5-4 6"/>', fillCite);
    wrap.append(...buttons.map(([b]) => b), box);
    function open(key) {
      open.key = key;
      box.hidden = !key;
      for (const [b, fill] of buttons) {
        const on = b.dataset.key === key;
        b.classList.toggle("on", on || (b === timeBtn && !!ctx.getTime()));
        b.setAttribute("aria-expanded", String(on));
        if (on) fill();
      }
    }
    const head = (title) => el("div", { class: "map-tools-head" }, el("strong", {}, title),
      el("button", { type: "button", class: "icon-btn", "aria-label": "Close", title: "Close", onclick: () => open(null) }, "×"));

    // ---------- time ----------
    let range = null; // {from, to, days}, once the extents are in
    async function fillTime() {
      box.replaceChildren(head("Time"), el("p", { class: "muted small" }, "Reading the time span…"));
      if (!range) {
        try { range = span(await ctx.extents(), new Date().toISOString().slice(0, 10)); } catch (e) { range = null; }
      }
      if (open.key !== "time") return;
      if (!range) { box.replaceChildren(head("Time"), el("p", { class: "muted small" }, "No feature has a time yet.")); return; }
      const now = parse(ctx.getTime());
      const at = (d, fallback) => (d ? Math.max(0, Math.min(range.days, daysBetween(range.from, d))) : fallback);
      const fromIn = el("input", { type: "date", min: range.from, max: range.to, value: now.from, "aria-label": "From" });
      const toIn = el("input", { type: "date", min: range.from, max: range.to, value: now.to, "aria-label": "To" });
      const fromSl = el("input", { type: "range", min: 0, max: range.days, step: 1, value: at(now.from, 0), "aria-label": "From day" });
      const toSl = el("input", { type: "range", min: 0, max: range.days, step: 1, value: at(now.to, range.days), "aria-label": "To day" });
      const note = el("p", { class: "muted small", role: "status" });
      const say = () => {
        const t = datetime(fromIn.value, toIn.value);
        note.textContent = t ? "Features of " + (fromIn.value && fromIn.value === toIn.value ? fromIn.value : (fromIn.value || "the start") + " to " + (toIn.value || "the end")) + ", and those of layers without a time." : "Every time.";
      };
      const set = () => { ctx.setTime(datetime(fromIn.value, toIn.value)); say(); if (timeBtn) timeBtn.classList.toggle("on", true); };
      // The sliders move the dates; a date typed moves its slider.
      const slide = (sl, input, other, otherIn, low) => {
        sl.addEventListener("input", () => {
          if (low ? Number(sl.value) > Number(other.value) : Number(sl.value) < Number(other.value)) { other.value = sl.value; otherIn.value = addDays(range.from, Number(sl.value)); }
          input.value = addDays(range.from, Number(sl.value));
          say();
        });
        sl.addEventListener("change", set);
        input.addEventListener("change", () => { if (input.value) sl.value = at(input.value, sl.value); set(); });
      };
      slide(fromSl, fromIn, toSl, toIn, true);
      slide(toSl, toIn, fromSl, fromIn, false);
      const day1 = el("button", { type: "button", class: "btn small", title: "One day: the From day", onclick: () => {
        if (!fromIn.value) fromIn.value = range.from;
        toIn.value = fromIn.value; toSl.value = fromSl.value = at(fromIn.value, 0); set();
      } }, "One day");
      const clear = el("button", { type: "button", class: "btn small", onclick: () => {
        fromIn.value = ""; toIn.value = ""; fromSl.value = 0; toSl.value = range.days;
        ctx.setTime(""); say(); if (timeBtn) timeBtn.classList.remove("on");
      } }, "Every time");
      say();
      box.replaceChildren(head("Time"),
        el("p", { class: "muted small" }, `The layers with a time: ${timed.map((c) => c.title).join(", ")}. From ${range.from} to ${range.to}.`),
        el("div", { class: "map-time-row" }, el("label", { class: "small" }, "From ", fromIn), fromSl),
        el("div", { class: "map-time-row" }, el("label", { class: "small" }, "To ", toIn), toSl),
        el("div", { class: "map-tools-row" }, day1, clear), note);
    }

    // ---------- export ----------
    function fillExport() {
      const title = el("input", { type: "text", value: ctx.ds, size: 28, "aria-label": "Title of the image" });
      const go = el("button", { type: "button", class: "btn small primary", onclick: async () => {
        go.disabled = true;
        try { await exportPng(title.value.trim()); } catch (e) { ui.toast("The map could not be exported: " + (e.message || e), "bad"); }
        go.disabled = false;
      } }, "Download PNG");
      box.replaceChildren(head("Export the map"),
        el("p", { class: "muted small" }, "The map as it is now, with a scale bar, a north arrow, the legend of the layers shown and the sources' credit."),
        el("label", { class: "small" }, "Title ", title),
        el("div", { class: "map-tools-row" }, go));
    }
    /** The map's canvas as an image, taken while it draws (WebGL clears it after). */
    const snapshot = () => new Promise((resolve, reject) => {
      map.once("render", () => {
        try { resolve(map.getCanvas().toDataURL("image/png")); } catch (e) { reject(e); }
      });
      map.triggerRepaint();
    });
    const loadImage = (src) => new Promise((resolve, reject) => {
      const img = new Image();
      img.onload = () => resolve(img);
      img.onerror = () => reject(new Error("the map image did not load"));
      img.src = src;
    });
    async function exportPng(title) {
      const img = await loadImage(await snapshot());
      const canvas = document.createElement("canvas");
      const ratio = img.width / map.getCanvas().clientWidth || 1;
      const credit = (document.querySelector("#map-canvas .maplibregl-ctrl-attrib-inner") || {}).textContent || "";
      const version = ctx.version ? `, data version ${ctx.version}` : "";
      const time = ctx.getTime();
      const foot = `${credit ? credit.trim() + " · " : ""}${siteName()} · ${ctx.ds}${version}${time ? " · time " + time : ""} · ${new Date().toISOString().slice(0, 10)}`;
      const top = title ? 34 * ratio : 0;
      const bottom = 22 * ratio;
      canvas.width = img.width;
      canvas.height = img.height + top + bottom;
      const g = canvas.getContext("2d");
      g.fillStyle = "#ffffff";
      g.fillRect(0, 0, canvas.width, canvas.height);
      g.drawImage(img, 0, top);
      const px = (n) => n * ratio;
      g.textBaseline = "middle";
      if (title) {
        g.fillStyle = "#1b1f24";
        g.font = `600 ${px(16)}px sans-serif`;
        g.fillText(title, px(10), top / 2);
      }
      // The credit under the map.
      g.fillStyle = "#4a5058";
      g.font = `${px(11)}px sans-serif`;
      g.fillText(foot, px(10), top + img.height + bottom / 2, canvas.width - px(20));
      // The scale bar, bottom left.
      const c = map.getCenter();
      const bar = scaleBar(metresPerPixel(c.lat, map.getZoom()), 120);
      const x = px(12);
      const y = top + img.height - px(16);
      g.fillStyle = "rgba(255,255,255,0.85)";
      g.fillRect(x - px(4), y - px(16), px(bar.pixels + 8 + 40), px(24));
      g.strokeStyle = "#1b1f24";
      g.lineWidth = px(2);
      g.beginPath();
      g.moveTo(x, y - px(5)); g.lineTo(x, y); g.lineTo(x + px(bar.pixels), y); g.lineTo(x + px(bar.pixels), y - px(5));
      g.stroke();
      g.fillStyle = "#1b1f24";
      g.font = `${px(11)}px sans-serif`;
      g.fillText(bar.label, x + px(bar.pixels + 6), y - px(4));
      // The north arrow, top right, turned with the map.
      const nx = canvas.width - px(28);
      const ny = top + px(30);
      g.save();
      g.translate(nx, ny);
      g.rotate((-map.getBearing() * Math.PI) / 180);
      g.fillStyle = "rgba(255,255,255,0.85)";
      g.beginPath(); g.arc(0, 0, px(16), 0, 2 * Math.PI); g.fill();
      g.fillStyle = "#1b1f24";
      g.beginPath(); g.moveTo(0, -px(12)); g.lineTo(px(6), px(6)); g.lineTo(0, px(2)); g.lineTo(-px(6), px(6)); g.closePath(); g.fill();
      g.font = `600 ${px(9)}px sans-serif`;
      g.textAlign = "center";
      g.fillText("N", 0, px(11));
      g.restore();
      // The legend of the layers shown, bottom right.
      const shown = ctx.shown();
      if (shown.length) {
        g.font = `${px(12)}px sans-serif`;
        const w = Math.max(...shown.map((s) => g.measureText(s.title).width)) + px(32);
        const h = px(18) * shown.length + px(10);
        const lx = canvas.width - w - px(10);
        const ly = top + img.height - h - px(10);
        g.fillStyle = "rgba(255,255,255,0.9)";
        g.fillRect(lx, ly, w, h);
        shown.forEach((s, i) => {
          const rowY = ly + px(5) + px(18) * i + px(9);
          g.fillStyle = s.color || "#8a8f98";
          g.fillRect(lx + px(8), rowY - px(5), px(12), px(10));
          g.fillStyle = "#1b1f24";
          g.textAlign = "left";
          g.fillText(s.title, lx + px(26), rowY);
        });
      }
      const blob = await new Promise((resolve) => canvas.toBlob(resolve, "image/png"));
      const a = el("a", { href: URL.createObjectURL(blob), download: (title || ctx.ds).replace(/[^\w.-]+/g, "-").replace(/^-|-$/g, "") + ".png" });
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(a.href), 10000);
    }

    // ---------- citation ----------
    async function fillCite() {
      box.replaceChildren(head("Cite this map"), el("p", { class: "muted small" }, "Collecting the queries of the layers…"));
      const shown = ctx.shown();
      const layers = [];
      for (const c of shown) {
        let parts = {};
        try { parts = await ctx.layer(c); } catch (e) { parts = {}; }
        layers.push({ title: c.title, ...parts });
      }
      if (open.key !== "cite") return;
      const text = citation({ title: ctx.ds, dataset: ctx.ds, version: ctx.version, link: location.href, when: new Date().toISOString().slice(0, 10), time: ctx.getTime(), layers });
      const area = el("textarea", { class: "map-cite", rows: 10, readonly: true, "aria-label": "Citation" });
      area.value = text;
      box.replaceChildren(head("Cite this map"),
        el("p", { class: "muted small" }, "The link opens this map again; the data version and the queries say what it showed."),
        area,
        el("div", { class: "map-tools-row" }, el("button", { type: "button", class: "btn small primary", onclick: () => ui.copyText(text, "Citation") }, "Copy")));
    }

    return { control: { onAdd: () => wrap, onRemove: () => wrap.remove() }, open };
  }

  const api = { controls, span, datetime, parse, scaleBar, metresPerPixel, citation, day };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.TripleTrailMapShare = api;
})(typeof window !== "undefined" ? window : globalThis);
