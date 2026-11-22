// Records' edit mode (#82): one record in a form, created, changed or deleted through the REST
// API's writes, which check it against the dataset's own shapes. records.js opens it with
// TripleTrailRecordEdit.open(ctx) for a class whose `/search/classes` entry is `writable`.
//
// - The form has a field per writable key of the class (its `writable` keys in the REST API's
//   dataset `rest`), drawn by forms.js's editors; a key with several values gets a list
//   editor, a reference a picker by label (`/search/labels`), text with a language a row per
//   language. Other keys are shown, read-only.
// - Each change is checked at once by the server (`?dry=true`), and the problems it names are
//   shown at their fields; required keys are marked from the shapes (`required`).
// - Saving sends the keys that changed (`PATCH`, or `POST` for a new record) with the record's
//   `ETag` in `If-Match`; when someone changed the record since, a dialog shows their values
//   against these, to keep these or take theirs.
// - Deleting says first how many links to the record would be left (a dry `DELETE`).
"use strict";

(function (root) {
  // ---------- pure helpers (tested in ui/test/records-edit.test.js) ----------

  /** A JSON value as the list of values a field edits: arrays as they are, one value as a list. */
  function listOf(v) {
    if (v == null) return [];
    return Array.isArray(v) ? v.slice() : [v];
  }

  /** A language map (`{"en": "x", "fr": ["y", "z"]}`) as rows `{lang, text}`, and back. */
  function langRows(map) {
    const rows = [];
    for (const [lang, v] of Object.entries(map || {})) for (const text of listOf(v)) rows.push({ lang, text });
    return rows;
  }
  function langMap(rows) {
    const out = {};
    for (const { lang, text } of rows) {
      if (text === "" || text == null) continue;
      const l = (lang || "").trim();
      if (out[l] === undefined) out[l] = text;
      else out[l] = listOf(out[l]).concat([text]);
    }
    return out;
  }

  /** A value as the REST API writes it: a reference by its IRI, other values as they are. */
  function written(field, v) {
    if (field.kind === "reference" || field.kind === "iri") return v && typeof v === "object" ? v.id : v;
    return v;
  }

  /** The value of a key as the body of a write gives it: one value or `null` for a single key,
   *  a list otherwise; empty values are left out. */
  function bodyValue(field, values) {
    if (field.lang) return langMap(values);
    const list = values.map((v) => written(field, v)).filter((v) => v !== "" && v != null);
    if (field.single) return list.length ? list[0] : null;
    return list;
  }

  /** A body value with nothing in it. */
  const empty = (v) => v == null || (Array.isArray(v) && !v.length) || (typeof v === "object" && !Array.isArray(v) && !Object.keys(v).length);

  /** Two body values alike (order of a list kept: it is what the user sees). */
  const same = (a, b) => JSON.stringify(a) === JSON.stringify(b);

  /** The keys whose values changed between `before` and `after` (both bodies by key). */
  function changes(before, after) {
    const out = {};
    for (const [k, v] of Object.entries(after)) if (!same(v, before[k])) out[k] = v;
    return out;
  }

  /** The problems of a 422 (`application/problem+json`) by key; those of no key under "". */
  function problemsByKey(problem) {
    const out = {};
    for (const e of (problem && problem.errors) || []) {
      const k = e.key || "";
      (out[k] ||= []).push(e.message || e.constraint || "Not allowed");
    }
    return out;
  }

  /** forms.js's field for a key of `/search/classes`: its label, datatype's local name, … */
  function formField(p) {
    const datatype = p.datatype ? p.datatype.replace(/^.*[#/]/, "") : null;
    return {
      key: p.key,
      label: p.title || p.key,
      description: p.description || null,
      kind: p.kind,
      target: p.target || null,
      single: !!p.single,
      required: !!p.required,
      lang: datatype === "langString",
      datatype: p.kind === "boolean" ? "boolean" : p.kind === "date" ? (datatype === "dateTime" ? "dateTime" : "date") : datatype,
      class: p.kind === "reference" || p.kind === "iri" ? p.target || "iri" : null,
    };
  }

  // ---------- the form ----------

  let ui = null;
  let el = null;

  /** Fetches `path` with the page's token and view-as headers; answers status, ETag and body. */
  async function call(method, path, body, headers) {
    const h = { "X-TripleTrail-Client": "ui", Accept: "application/json", ...ui.viewAs.headers(), ...(headers || {}) };
    const token = ui.store.get(ui.TOKEN_KEY);
    if (token) h.Authorization = "Bearer " + token;
    if (body !== undefined) h["Content-Type"] = "application/json";
    const r = await fetch(path, { method, headers: h, body: body === undefined ? undefined : JSON.stringify(body) });
    const text = await r.text();
    let json = null;
    try { json = JSON.parse(text); } catch (e) { /* text */ }
    if (r.status === 401) ui.openToken("Writing records needs a token that may write.");
    return { status: r.status, etag: r.headers.get("ETag"), json, text: text.trim() };
  }

  /**
   * Opens the form in `ctx.box`: `ctx.ds`, `ctx.cls` (the `/search/classes` entry), `ctx.iri`
   * (null for a new record), `ctx.saved(iri)` once written, `ctx.closed()` when left, and
   * `ctx.deleted()` once deleted.
   */
  async function open(shared, ctx) {
    ui = shared;
    el = ui.el;
    const Forms = window.TripleTrailForms;
    const box = ctx.box;
    const enc = encodeURIComponent;
    const api = "/" + enc(ctx.ds) + "/api/v1/" + enc(ctx.cls.rest);
    const recordUrl = (iri) => api + "/" + enc(iri) + "?f=json";
    const fields = ctx.cls.properties.filter((p) => p.writable && p.kind !== "object" && p.kind !== "geometry").map(formField);
    const others = ctx.cls.properties.filter((p) => !p.writable || p.kind === "object" || p.kind === "geometry");
    box.replaceChildren(el("p", { class: "muted small" }, ctx.iri ? "Reading the record to edit…" : "A new record…"));

    let etag = null;
    let record = {};
    if (ctx.iri) {
      const r = await call("GET", recordUrl(ctx.iri));
      if (r.status !== 200) { box.replaceChildren(el("p", { class: "error" }, r.text || "The record cannot be read: " + r.status)); return; }
      etag = r.etag;
      record = r.json || {};
    }
    // The values being edited, by key: a list per key (rows for text with a language).
    const values = {};
    for (const f of fields) values[f.key] = f.lang ? langRows(record[f.key]) : listOf(record[f.key]);
    // A new record leaves its empty keys out; a changed one sends them, to clear them.
    const bodyOf = (all) => {
      const out = {};
      for (const f of fields) {
        const v = bodyValue(f, values[f.key]);
        if (all || !empty(v)) out[f.key] = v;
      }
      return out;
    };
    let original = ctx.iri ? bodyOf(true) : {};
    let iriBox = null;
    /** What a save sends: the keys that changed, or a new record's values and its IRI if given. */
    const payload = () => {
      if (ctx.iri) return changes(original, bodyOf(true));
      const body = bodyOf(false);
      if (iriBox && iriBox.value.trim()) body.id = iriBox.value.trim();
      return body;
    };

    const status = el("p", { class: "muted small records-edit-status", role: "status" });
    const problems = {}; // the problem lists' elements, by key
    let checking = null;
    let checkedFor = null;
    /** Asks the server whether the record as edited would be written (`?dry=true`). */
    async function check() {
      const body = payload();
      const text = JSON.stringify(body);
      if (text === checkedFor) return;
      checkedFor = text;
      const r = ctx.iri
        ? await call("PATCH", api + "/" + enc(ctx.iri) + "?dry=true", body, etag ? { "If-Match": etag } : {})
        : await call("POST", api + "?dry=true", body);
      if (text !== checkedFor) return;
      show(r.status === 422 ? problemsByKey(r.json) : {});
      status.textContent = r.status === 200 ? (Object.keys(body).length ? "Ready to save." : "No changes.")
        : r.status === 422 ? "Fix the fields marked below."
        : r.status === 412 ? "Someone changed the record since you opened it."
        : r.text || "Cannot be saved: " + r.status;
    }
    const later = () => { clearTimeout(checking); checking = setTimeout(check, 400); };
    function show(byKey) {
      for (const [k, node] of Object.entries(problems)) {
        const list = byKey[k] || [];
        node.replaceChildren(...list.map((m) => el("span", {}, m)));
        node.hidden = !list.length;
      }
      const loose = Object.entries(byKey).filter(([k]) => !(k in problems)).flatMap(([k, list]) => list.map((m) => (k ? k + ": " : "") + m));
      problems[""].replaceChildren(...loose.map((m) => el("span", {}, m)));
      problems[""].hidden = !loose.length;
    }

    /** The control for one value of field `f` at `i`. */
    function control(f, i) {
      const list = values[f.key];
      const id = "records-edit-" + f.key + "-" + i;
      const set = (v) => { list[i] = v; later(); };
      if (f.lang) {
        const row = list[i];
        return el("span", { class: "records-edit-lang" },
          el("input", { type: "text", class: "records-edit-langtag", "aria-label": f.label + " language", value: row.lang, size: 4,
            oninput: (e) => { row.lang = e.target.value; later(); } }),
          el("input", { type: "text", id, "aria-label": f.label, value: row.text, oninput: (e) => { row.text = e.target.value; later(); } }));
      }
      if (f.kind === "reference" || f.kind === "iri") return picker(f, list[i], set, id);
      const value = list[i];
      return Forms.editor({ ...f, multiple: false }, { el, id, value: value, name: f.label, editable: true, set });
    }

    /** A reference: the label shown, a box that finds records of the target class by label. */
    function picker(f, value, set, id) {
      const shown = value && typeof value === "object" ? value.label || value.id : value || "";
      const listId = id + "-list";
      const options = el("datalist", { id: listId });
      const input = el("input", { type: "text", id, list: listId, "aria-label": f.label, value: shown, size: 30,
        placeholder: f.target ? "Find by name" : "IRI" });
      let found = [];
      let asking = null;
      input.addEventListener("input", () => {
        const text = input.value.trim();
        const hit = found.find((r) => (r.label || r.iri) === text);
        if (hit) { set({ id: hit.iri, label: hit.label }); return; }
        if (!text || /^[a-z][a-z0-9+.-]*:\S+$/i.test(text)) set(text);
        clearTimeout(asking);
        if (!f.target || text.length < 2) return;
        asking = setTimeout(async () => {
          try {
            const out = await ui.api("GET", "/" + enc(ctx.ds) + "/search/labels", { params: { q: text, class: f.target, limit: "10" } });
            found = out.results || [];
            options.replaceChildren(...found.map((r) => el("option", { value: r.label || r.iri }, r.iri)));
          } catch (e) { /* no suggestions */ }
        }, 250);
      });
      return el("span", {}, input, options);
    }

    /** The field of key `f`: its label (required marked), its values, and its problems. */
    function field(f) {
      const list = values[f.key];
      const rows = el("div", { class: "records-edit-values" });
      const draw = () => {
        if (!list.length && (f.single || f.lang || f.required)) list.push(f.lang ? { lang: "", text: "" } : f.kind === "boolean" ? null : "");
        rows.replaceChildren(...list.map((_, i) => el("div", { class: "records-edit-value" }, control(f, i),
          f.single && !f.lang ? null : el("button", { type: "button", class: "icon-btn", "aria-label": "Remove this value of " + f.label, title: "Remove",
            onclick: () => { list.splice(i, 1); draw(); later(); } }, "×"))));
        if (!f.single || f.lang) rows.append(el("button", { type: "button", class: "btn small", onclick: () => { list.push(f.lang ? { lang: "", text: "" } : ""); draw(); } }, "+ Add"));
      };
      draw();
      const problem = el("span", { class: "records-edit-problem", role: "alert", hidden: true });
      problems[f.key] = problem;
      return el("div", { class: "shapes-field records-field records-edit-field", "data-key": f.key },
        el("label", { class: "records-label", for: "records-edit-" + f.key + "-0" }, f.label, f.required ? el("span", { class: "records-required", title: "Required" }, " *") : null),
        el("span", {}, rows, problem, f.description ? el("span", { class: "muted small" }, f.description) : null));
    }

    const head = el("div", { class: "records-record-head" },
      el("div", {}, el("h2", {}, ctx.iri ? "Edit " + (ctx.label || ctx.iri) : "New " + (ctx.cls.title || ctx.cls.id)),
        ctx.iri ? el("p", { class: "muted small mono" }, ctx.iri) : null));
    if (!ctx.iri && !ctx.cls.iriTemplate) {
      iriBox = el("input", { type: "url", id: "records-edit-iri", size: 40, oninput: later, placeholder: "urn:uuid:… when left empty", "aria-label": "The new record's IRI" });
    }
    problems[""] = el("p", { class: "records-edit-problem", role: "alert", hidden: true });
    const save = el("button", { type: "button", class: "btn primary", id: "records-save" }, ctx.iri ? "Save" : "Create");
    const cancel = el("button", { type: "button", class: "btn", onclick: () => ctx.closed() }, "Cancel");
    const del = ctx.iri ? el("button", { type: "button", class: "btn danger", id: "records-delete" }, "Delete…") : null;
    box.replaceChildren(...[head,
      iriBox ? el("label", { class: "small records-edit-iri" }, "IRI ", iriBox) : null,
      ctx.cls.iriTemplate && !ctx.iri ? el("p", { class: "muted small" }, "The IRI is made from ", el("code", {}, ctx.cls.iriTemplate), ".") : null,
      el("fieldset", { class: "records-group" }, el("legend", {}, "Values"), ...fields.map(field),
        fields.length ? null : el("p", { class: "muted small" }, "No key of this class can be written here: its paths are not one property.")),
      others.length ? el("p", { class: "muted small" }, "Not edited here: ", others.map((p) => p.title || p.key).join(", "), ".") : null,
      problems[""], status,
      el("div", { class: "records-actions" }, save, cancel, el("span", { class: "grow" }), del)].filter(Boolean));

    save.onclick = async () => {
      clearTimeout(checking);
      const body = payload();
      if (ctx.iri && !Object.keys(body).length) { ctx.closed(); return; }
      save.disabled = true;
      let again = null;
      try {
        const r = ctx.iri
          ? await call("PATCH", api + "/" + enc(ctx.iri), body, etag ? { "If-Match": etag } : {})
          : await call("POST", api, body);
        if (r.status === 200 || r.status === 201) {
          ui.toast(ctx.iri ? "Saved" : "Created", "ok");
          ctx.saved((r.json && r.json.id) || ctx.iri);
          return;
        }
        if (r.status === 422) { show(problemsByKey(r.json)); status.textContent = "Not saved: fix the fields marked below."; return; }
        if (r.status === 412) again = await conflict(body);
        else status.textContent = "Not saved: " + (r.text || r.status);
      } finally { save.disabled = false; }
      if (again === "mine") save.onclick();
      else if (again === "theirs") open(shared, ctx);
    };

    /** Someone changed the record since it was read: their values against these, by key.
     *  Answers "mine" (save again over theirs) or "theirs" (open the record as it is now). */
    async function conflict(mine) {
      const r = await call("GET", recordUrl(ctx.iri));
      if (r.status !== 200) { status.textContent = "The record is gone: " + (r.text || r.status); return; }
      const theirs = {};
      for (const f of fields) theirs[f.key] = bodyValue(f, f.lang ? langRows(r.json[f.key]) : listOf(r.json[f.key]));
      const text = (v) => (v == null ? "" : typeof v === "object" ? JSON.stringify(v) : String(v));
      const d = el("dialog", { class: "dlg records-conflict", "aria-label": "The record changed" },
        el("div", { class: "dlg-body" },
          el("h3", {}, "Someone changed this record"),
          el("p", { class: "muted small" }, "Since you opened it, the record was saved by someone else. These are the keys you changed:"),
          el("table", { class: "records-subtable" },
            el("thead", {}, el("tr", {}, el("th", {}, "Key"), el("th", {}, "Theirs"), el("th", {}, "Yours"))),
            el("tbody", {}, Object.keys(mine).map((k) => el("tr", {}, el("td", {}, k), el("td", {}, text(theirs[k])), el("td", {}, text(mine[k]))))))),
        el("div", { class: "dlg-foot" },
          el("button", { type: "button", class: "btn", value: "theirs", onclick: () => d.close("theirs") }, "Take theirs"),
          el("button", { type: "button", class: "btn primary", value: "mine", onclick: () => d.close("mine") }, "Keep mine")));
      document.body.append(d);
      d.showModal();
      const choice = await new Promise((resolve) => d.addEventListener("close", () => resolve(d.returnValue), { once: true }));
      d.remove();
      if (choice === "mine") {
        // Mine over theirs: the keys I changed, against the record as it is now; the
        // keys I did not change take their values, so that a save does not undo them.
        for (const f of fields) if (!(f.key in mine)) values[f.key] = f.lang ? langRows(r.json[f.key]) : listOf(r.json[f.key]);
        etag = r.etag;
        original = theirs;
        checkedFor = null;
      }
      return choice;
    }

    if (del) del.onclick = async () => {
      const dry = await call("DELETE", api + "/" + enc(ctx.iri) + "?dry=true", undefined, etag ? { "If-Match": etag } : {});
      if (dry.status !== 200) { status.textContent = "Cannot be deleted: " + (dry.text || dry.status); return; }
      const links = (dry.json && dry.json.referencedBy) || 0;
      const answer = await ui.confirmDialog("Delete " + (ctx.label || ctx.iri),
        links ? `${links} link${links === 1 ? "" : "s"} from other resources to this record will be left pointing at nothing.`
          : "No other resource links to this record.");
      if (!answer.ok) return;
      const r = await call("DELETE", api + "/" + enc(ctx.iri), undefined, etag ? { "If-Match": etag } : {});
      if (r.status === 204) { ui.toast("Deleted", "ok"); ctx.deleted(); return; }
      status.textContent = r.status === 412 ? "Someone changed the record since you opened it: open it again." : "Not deleted: " + (r.text || r.status);
    };

    // The first check: required keys of a new record are named at once.
    if (!ctx.iri) later();
  }

  const api = { open, listOf, langRows, langMap, bodyValue, changes, problemsByKey, formField };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.TripleTrailRecordEdit = api;
})(typeof window !== "undefined" ? window : globalThis);
