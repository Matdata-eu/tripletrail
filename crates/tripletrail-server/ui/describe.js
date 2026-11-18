// A describe page (#140) with the web UI: its sections become tabs (the one in `?tab=` first),
// and the buttons work: Copy IRI, Share (a link to this page and tab), the RDF tab's formats with
// Copy and Download, and the Mermaid text's Copy. Without this script the page reads as it is,
// one section under the other.

const KEY = "tripletrail.describe.rdfFormat";

/** Writes `text` to the clipboard; in a frame without clipboard access, through a selection. */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = document.createElement("textarea");
    area.value = text;
    area.setAttribute("readonly", "");
    area.style.position = "fixed";
    area.style.opacity = "0";
    document.body.append(area);
    area.select();
    let ok = false;
    try { ok = document.execCommand("copy"); } catch { ok = false; }
    area.remove();
    return ok;
  }
}

/** Shows on a button that a copy worked (or not) for a moment. */
function flash(button, ok) {
  const text = button.dataset.text || button.textContent;
  button.dataset.text = text;
  button.textContent = ok ? "Copied" : "Copy failed";
  setTimeout(() => { button.textContent = text; }, 1500);
}

// --- Tabs -------------------------------------------------------------------------------------

const panels = [...document.querySelectorAll("section.tab-panel[data-tab]")];
let current = null;
const web = location.protocol === "http:" || location.protocol === "https:";

function select(key, focus) {
  const panel = panels.find((p) => p.dataset.tab === key) || panels[0];
  if (!panel) return;
  current = panel.dataset.tab;
  for (const p of panels) p.hidden = p !== panel;
  for (const b of document.querySelectorAll(".tabs [role=tab]")) {
    const on = b.dataset.tab === current;
    b.setAttribute("aria-selected", String(on));
    b.tabIndex = on ? 0 : -1;
    if (on && focus) b.focus();
  }
  if (web) {
    const url = new URL(location.href);
    if (current === panels[0].dataset.tab) url.searchParams.delete("tab");
    else url.searchParams.set("tab", current);
    if (url.href !== location.href) history.replaceState(history.state, "", url.href);
  }
  // A map drawn while its tab was hidden has no size yet.
  window.dispatchEvent(new Event("resize"));
}

if (panels.length > 1) {
  const bar = document.createElement("nav");
  bar.className = "tabs";
  bar.setAttribute("role", "tablist");
  bar.setAttribute("aria-label", "Sections");
  for (const p of panels) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = "tab-btn";
    b.setAttribute("role", "tab");
    b.setAttribute("aria-controls", p.id);
    b.dataset.tab = p.dataset.tab;
    b.textContent = p.dataset.label;
    if (p.dataset.count) {
      const c = document.createElement("span");
      c.className = "tab-count";
      c.textContent = Number(p.dataset.count).toLocaleString("en");
      b.append(c);
    }
    b.addEventListener("click", () => select(p.dataset.tab, false));
    b.addEventListener("keydown", (e) => {
      const i = panels.indexOf(p);
      const next = e.key === "ArrowRight" ? panels[(i + 1) % panels.length]
        : e.key === "ArrowLeft" ? panels[(i - 1 + panels.length) % panels.length]
          : e.key === "Home" ? panels[0] : e.key === "End" ? panels[panels.length - 1] : null;
      if (next) { e.preventDefault(); select(next.dataset.tab, true); }
    });
    bar.append(b);
  }
  for (const p of panels) p.setAttribute("role", "tabpanel");
  panels[0].before(bar);
  document.body.classList.add("tabbed");
  const wanted = web ? new URLSearchParams(location.search).get("tab") : null;
  select(wanted, false);
}

// --- Copy and Share ---------------------------------------------------------------------------

for (const b of document.querySelectorAll("[data-copy]")) {
  b.hidden = false;
  b.addEventListener("click", async () => flash(b, await copyText(b.dataset.copy)));
}
for (const b of document.querySelectorAll("[data-copy-from]")) {
  b.hidden = false;
  b.addEventListener("click", async () => {
    const from = document.getElementById(b.dataset.copyFrom);
    flash(b, !!from && await copyText(from.textContent));
  });
}
for (const b of document.querySelectorAll("[data-share]")) {
  b.hidden = false;
  b.addEventListener("click", async () => {
    // The page's own address, also inside the map's panel (a frame without one).
    const url = new URL(b.dataset.share, document.baseURI);
    if (current && panels.length && current !== panels[0].dataset.tab && !url.hash) url.searchParams.set("tab", current);
    if (navigator.share && matchMedia("(pointer: coarse)").matches) {
      try { await navigator.share({ title: document.title, url: url.href }); return; } catch { /* fall back to copying */ }
    }
    flash(b, await copyText(url.href));
  });
}

// --- RDF ----------------------------------------------------------------------------------------

const TURTLE = /(#[^\n]*)|("(?:[^"\\\n]|\\.)*")|(<[^>\s]*>)|(@prefix|@base|\ba\b(?=\s))|((?:[A-Za-z][\w-]*)?:[\w.-]*\w|\^\^|@[a-zA-Z-]+)/g;
const NTRIPLES = /("(?:[^"\\\n]|\\.)*")|(<[^>\s]*>)|(\^\^|@[a-zA-Z-]+)/g;
const XML = /(<\?[^?]*\?>|<!--[\s\S]*?-->)|(<\/?[\w:.-]+|\/?>)|(\s[\w:.-]+=)|("[^"]*")/g;
const JSON_RE = /("(?:[^"\\]|\\.)*")(\s*:)?/g;

/** The text as highlighted pieces. */
function highlight(text, format) {
  const out = document.createDocumentFragment();
  const re = format === "rdf" ? XML : format === "jsonld" ? JSON_RE : format === "nt" ? NTRIPLES : TURTLE;
  re.lastIndex = 0;
  let last = 0;
  const piece = (kind, t) => {
    if (!kind) { out.append(t); return; }
    const s = document.createElement("span");
    s.className = "tok-" + kind;
    s.textContent = t;
    out.append(s);
  };
  for (let m = re.exec(text); m; m = re.exec(text)) {
    if (m.index > last) piece(null, text.slice(last, m.index));
    if (format === "rdf") piece(m[1] ? "comment" : m[2] ? "tag" : m[3] ? "attr" : "string", m[0]);
    else if (format === "jsonld") {
      if (m[2]) { piece("key", m[1]); piece(null, m[2]); } else piece("string", m[0]);
    } else if (format === "nt") piece(m[1] ? "string" : m[2] ? "iri" : "name", m[0]);
    else piece(m[1] ? "comment" : m[2] ? "string" : m[3] ? "iri" : m[4] ? "keyword" : "name", m[0]);
    last = m.index + m[0].length;
  }
  if (last < text.length) piece(null, text.slice(last));
  return out;
}

const code = document.getElementById("mg-rdf");
const formats = [...document.querySelectorAll(".seg-btn[data-format]")];
const download = document.getElementById("mg-rdf-download");
const note = document.getElementById("mg-rdf-note");
if (code && formats.length) {
  const texts = new Map();
  // Long texts stay plain: highlighting them would hold up the page.
  const show = (text, format) => {
    if (text.length < 400_000) code.replaceChildren(highlight(text, format));
    else code.textContent = text;
    code.dataset.format = format;
    code.parentElement.hidden = false;
  };
  if (code.dataset.format) {
    texts.set(code.dataset.format, Promise.resolve(code.textContent));
    show(code.textContent, code.dataset.format);
  }
  const choose = async (button) => {
    const format = button.dataset.format;
    for (const f of formats) f.setAttribute("aria-pressed", String(f === button));
    if (download) download.href = button.href;
    try { localStorage.setItem(KEY, format); } catch { /* private mode */ }
    if (!texts.has(format)) {
      texts.set(format, fetch(button.href, { credentials: "same-origin" }).then(async (r) => {
        const text = await r.text();
        if (!r.ok) throw new Error(text.trim() || r.statusText);
        // JSON-LD comes on one line: indented, it reads.
        if (format === "jsonld" && text.length < 2_000_000) {
          try { return JSON.stringify(JSON.parse(text), null, 2) + "\n"; } catch { /* as it came */ }
        }
        return text;
      }));
    }
    code.textContent = "Loading…";
    try {
      const text = await texts.get(format);
      if (code.dataset.want === format) show(text, format);
    } catch (e) {
      texts.delete(format);
      if (code.dataset.want === format) code.textContent = "Could not load: " + (e.message || e) + ". Download it instead.";
    }
  };
  for (const f of formats) {
    f.addEventListener("click", (e) => {
      if (e.ctrlKey || e.metaKey || e.shiftKey || e.button !== 0) return;
      e.preventDefault();
      code.dataset.want = f.dataset.format;
      if (note && !code.dataset.format && !texts.size) note.hidden = false;
      choose(f);
    });
  }
  let saved = null;
  try { saved = localStorage.getItem(KEY); } catch { /* private mode */ }
  const first = formats.find((f) => f.dataset.format === saved);
  // Only a small description is fetched again in another format by itself.
  if (first && first.dataset.format !== code.dataset.format && code.dataset.format) {
    code.dataset.want = first.dataset.format;
    choose(first);
  }
}
