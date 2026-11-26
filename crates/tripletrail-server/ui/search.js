// The top bar's search box (#142): type a name, pick a resource, go to its describe page.
// Searches one dataset (`/{ds}/search/labels`): the one the page shows, else the last one
// chosen. Plain JavaScript like app.js.
(function () {
  const UI = "/$/ui/";
  const DS_KEY = "tripletrail-search-dataset";
  const TOKEN_KEY = "tripletrail-admin-token";
  const store = {
    get(k) { try { return localStorage.getItem(k); } catch (e) { return null; } },
    set(k, v) { try { localStorage.setItem(k, v); } catch (e) { /* private window */ } },
  };
  const box = document.getElementById("top-search");
  if (!box) return;
  const input = box.querySelector("input");
  const select = box.querySelector("select");
  const list = box.querySelector("[role=listbox]");
  let datasets = [];
  let results = [];
  let active = -1;
  let timer = null;
  let asked = 0;

  // The admin token, and whom to run as when the tab views as someone else (#205; the
  // same session entry app.js keeps).
  const VIEW_AS_KEY = "tripletrail-view-as";
  const headers = () => {
    const token = store.get(TOKEN_KEY);
    const h = token ? { Authorization: "Bearer " + token } : {};
    let v = null;
    try { v = JSON.parse(sessionStorage.getItem(VIEW_AS_KEY)); } catch (e) { /* none */ }
    if (v) {
      h["X-TripleTrail-View-As"] = v.agent || "anonymous";
      if (v.groups && v.groups.length) h["X-TripleTrail-View-As-Groups"] = v.groups.join(",");
    }
    return h;
  };
  // The dataset of the page's path (query/kg, map/kg, datasets/kg), if any.
  const fromPath = () => {
    const [page, arg] = location.pathname.slice(UI.length).split("/");
    return ["query", "map", "search", "datasets", "shapes", "import"].includes(page) && arg ? decodeURIComponent(arg) : null;
  };
  async function loadDatasets() {
    try {
      const r = await fetch("/$/datasets", { headers: headers() });
      if (!r.ok) return;
      datasets = (await r.json()).datasets.map((d) => d.name).filter((n) => !n.startsWith("$"));
    } catch (e) { datasets = []; }
    // The server's --default-dataset when the viewer may see it, else the first.
    const preferred = document.documentElement.dataset.defaultDataset;
    const current = fromPath() || store.get(DS_KEY) || (datasets.includes(preferred) ? preferred : datasets[0]);
    select.replaceChildren(...datasets.map((n) => {
      const o = document.createElement("option");
      o.value = n; o.textContent = n; o.selected = n === current;
      return o;
    }));
    box.hidden = datasets.length === 0;
  }
  // Follows the page: opening a dataset's page makes it the one searched.
  function follow() {
    const ds = fromPath();
    if (ds && datasets.includes(ds) && select.value !== ds) select.value = ds;
  }

  function close() {
    list.hidden = true;
    input.setAttribute("aria-expanded", "false");
    input.removeAttribute("aria-activedescendant");
    active = -1;
  }
  function render(message) {
    list.replaceChildren();
    if (message) {
      const p = document.createElement("div");
      p.className = "top-search-msg";
      p.textContent = message;
      list.append(p);
    }
    results.forEach((r, i) => {
      const item = document.createElement("a");
      item.id = "top-search-" + i;
      item.href = r.describe;
      item.setAttribute("role", "option");
      item.className = "top-search-item" + (i === active ? " active" : "");
      item.setAttribute("aria-selected", i === active ? "true" : "false");
      const label = document.createElement("span");
      label.className = "top-search-label";
      label.textContent = r.label;
      const cls = document.createElement("span");
      cls.className = "top-search-class";
      cls.textContent = r.classTitle || r.class;
      item.append(label, cls);
      item.title = r.iri;
      list.append(item);
    });
    list.hidden = false;
    input.setAttribute("aria-expanded", "true");
    if (active >= 0) input.setAttribute("aria-activedescendant", "top-search-" + active);
    else input.removeAttribute("aria-activedescendant");
  }
  async function search() {
    const q = input.value.trim();
    const ds = select.value;
    if (!q || !ds) { results = []; close(); return; }
    const n = ++asked;
    try {
      const url = "/" + encodeURIComponent(ds) + "/search/labels?q=" + encodeURIComponent(q);
      const r = await fetch(url, { headers: headers() });
      if (n !== asked) return; // an answer to an older text
      if (!r.ok) { results = []; render(r.status === 401 || r.status === 403 ? "Not allowed to search " + ds : "The search failed (" + r.status + ")"); return; }
      results = (await r.json()).results;
      active = results.length ? 0 : -1;
      render(results.length ? null : "Nothing found in " + ds);
    } catch (e) {
      if (n === asked) { results = []; render("The search failed"); }
    }
  }

  input.addEventListener("input", () => { clearTimeout(timer); timer = setTimeout(search, 150); });
  input.addEventListener("focus", () => { if (results.length) render(); });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      if (!results.length) return;
      e.preventDefault();
      active = (active + (e.key === "ArrowDown" ? 1 : results.length - 1)) % results.length;
      render();
      const el = document.getElementById("top-search-" + active);
      if (el) el.scrollIntoView({ block: "nearest" });
    } else if (e.key === "Enter") {
      if (active >= 0 && results[active]) { e.preventDefault(); location.href = results[active].describe; }
    } else if (e.key === "Escape") {
      close();
      input.blur();
    }
  });
  select.addEventListener("change", () => { store.set(DS_KEY, select.value); search(); input.focus(); });
  // Clicks inside the list must not close it before the link opens.
  list.addEventListener("mousedown", (e) => e.preventDefault());
  // On a narrow screen the box is an icon: a click on it opens the box.
  box.addEventListener("click", (e) => { if (e.target !== select) input.focus(); });
  input.addEventListener("blur", () => setTimeout(close, 100));
  // "/" anywhere outside a text field goes to the search box.
  document.addEventListener("keydown", (e) => {
    if (e.key !== "/" || e.ctrlKey || e.metaKey || e.altKey || box.hidden) return;
    const t = e.target;
    if (t.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(t.tagName) || t.closest(".cm-editor")) return;
    e.preventDefault();
    input.focus();
    input.select();
  });
  window.addEventListener("popstate", follow);
  document.addEventListener("click", () => setTimeout(follow, 0));

  loadDatasets();
})();
