// The GraphQL playground (#144): a query editor over `/{ds}/graphql`, with the schema's
// datasets and keys from introspection. Plain JavaScript like app.js; CodeMirror is vendored.
(function () {
  const root = document.getElementById("gq");
  const dataset = root.dataset.dataset;
  const endpoint = "/" + encodeURIComponent(dataset) + "/graphql";
  const out = document.getElementById("gq-out");
  const vars = document.getElementById("gq-vars");
  const time = document.getElementById("gq-time");
  const CM = window.CM;
  const stored = (key, fallback) => {
    try { return localStorage.getItem("mg-gq-" + dataset + "-" + key) || fallback; } catch (e) { return fallback; }
  };
  const store = (key, value) => {
    try { localStorage.setItem("mg-gq-" + dataset + "-" + key, value); } catch (e) { /* private window */ }
  };

  // A small GraphQL highlighter: keywords, names, strings, numbers, variables, comments.
  const graphql = CM.StreamLanguage.define({
    token(stream) {
      if (stream.eatSpace()) return null;
      if (stream.match("#")) { stream.skipToEnd(); return "comment"; }
      if (stream.match(/^"(?:[^"\\]|\\.)*"?/)) return "string";
      if (stream.match(/^-?\d+(\.\d+)?([eE][+-]?\d+)?/)) return "number";
      if (stream.match(/^\$[_A-Za-z][_0-9A-Za-z]*/)) return "variableName";
      if (stream.match(/^(query|mutation|subscription|fragment|on|true|false|null)\b/)) return "keyword";
      if (stream.match(/^[_A-Za-z][_0-9A-Za-z]*(?=\s*:)/)) return "propertyName";
      if (stream.match(/^[_A-Za-z][_0-9A-Za-z]*/)) return "variableName.special";
      stream.next();
      return null;
    },
  });
  // Follows the page's light or dark colours.
  const dark = window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches;
  const theme = CM.EditorView.theme({
    "&": { backgroundColor: "var(--bg)", color: "var(--text)" },
    ".cm-gutters": { backgroundColor: "var(--surface)", color: "var(--muted)", borderRight: "1px solid var(--border)" },
    ".cm-content": { caretColor: "var(--text)" },
    "&.cm-focused .cm-cursor": { borderLeftColor: "var(--text)" },
  }, { dark });
  const t = CM.tags;
  const highlight = CM.HighlightStyle.define([
    { tag: t.keyword, color: "var(--hl-keyword)", fontWeight: "600" },
    { tag: t.string, color: "var(--hl-string)" },
    { tag: t.number, color: "var(--hl-number)" },
    { tag: t.comment, color: "var(--muted)", fontStyle: "italic" },
    { tag: t.variableName, color: "var(--hl-variable)" },
    { tag: t.propertyName, color: "var(--hl-property)" },
    { tag: t.special(t.variableName), color: "var(--text)" },
  ]);
  const run = () => { query(); return true; };
  const view = new CM.EditorView({
    parent: document.getElementById("gq-editor"),
    state: CM.EditorState.create({
      doc: stored("query", "# Pick a dataset on the left, or write a query.\n{\n  __typename\n}\n"),
      extensions: [
        CM.lineNumbers(), CM.history(), CM.drawSelection(), CM.bracketMatching(), CM.closeBrackets(),
        CM.syntaxHighlighting(highlight), graphql, theme,
        CM.keymap.of([{ key: "Mod-Enter", run }, ...CM.closeBracketsKeymap, ...CM.defaultKeymap, ...CM.historyKeymap, CM.indentWithTab]),
        CM.EditorView.updateListener.of((u) => { if (u.docChanged) store("query", u.state.doc.toString()); }),
      ],
    }),
  });
  vars.value = stored("vars", "{}");
  vars.addEventListener("input", () => store("vars", vars.value));
  vars.addEventListener("keydown", (e) => { if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) { e.preventDefault(); query(); } });
  document.getElementById("gq-run").addEventListener("click", query);

  async function post(body) {
    const r = await fetch(endpoint, {
      method: "POST",
      credentials: "same-origin",
      headers: { "Content-Type": "application/json", Accept: "application/json" },
      body: JSON.stringify(body),
    });
    const text = await r.text();
    try { return JSON.parse(text); } catch (e) { return { errors: [{ message: r.status + " " + text }] }; }
  }

  async function query() {
    let variables;
    try { variables = JSON.parse(vars.value.trim() || "{}"); } catch (e) {
      out.className = "error";
      out.textContent = "The variables are not JSON: " + e.message;
      return;
    }
    out.className = "muted";
    out.textContent = "Running…";
    const started = performance.now();
    const answer = await post({ query: view.state.doc.toString(), variables });
    time.textContent = Math.round(performance.now() - started) + " ms";
    out.className = answer.errors ? "error" : "";
    out.textContent = JSON.stringify(answer, null, 2);
  }

  // The schema, from introspection: each root field with its type's own fields.
  const el = (tag, attrs, ...kids) => {
    const e = document.createElement(tag);
    Object.entries(attrs || {}).forEach(([k, v]) => (k === "onclick" ? (e.onclick = v) : e.setAttribute(k, v)));
    kids.flat().forEach((k) => k != null && e.append(k));
    return e;
  };
  const typeName = (t) => (t.kind === "NON_NULL" ? typeName(t.ofType) + "!" : t.kind === "LIST" ? "[" + typeName(t.ofType) + "]" : t.name);
  const base = (t) => (t.ofType ? base(t.ofType) : t);
  const TYPE = "{ kind name ofType { kind name ofType { kind name ofType { kind name } } } }";

  async function schema() {
    const box = document.getElementById("gq-schema");
    const answer = await post({ query: `{ __schema { queryType { fields { name description args { name type ${TYPE} } type ${TYPE} } }
      types { name kind fields { name description type ${TYPE} } } } }` });
    box.textContent = "";
    if (answer.errors) {
      box.append(el("pre", { class: "error" }, answer.errors.map((e) => e.message).join("\n")));
      return;
    }
    const types = new Map(answer.data.__schema.types.map((t) => [t.name, t]));
    // The fields of a record type as a selection block, references followed one level.
    const selection = (name, depth, indent) => {
      const t = types.get(name);
      if (!t || !t.fields) return "";
      const pad = "  ".repeat(indent + 1);
      const lines = t.fields.map((f) => {
        const b = base(f.type);
        if (b.kind === "SCALAR" || b.kind === "ENUM") return pad + f.name;
        if (depth > 0) return pad + f.name + " { id }";
        const inner = selection(b.name, depth + 1, indent + 1);
        return inner ? pad + f.name + " " + inner : null;
      }).filter(Boolean);
      return "{\n" + lines.join("\n") + "\n" + "  ".repeat(indent) + "}";
    };
    const insert = (text, variables) => {
      vars.value = variables;
      store("vars", variables);
      view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
      view.focus();
    };
    for (const f of answer.data.__schema.queryType.fields) {
      const b = base(f.type);
      const record = b.name.endsWith("Connection") ? types.get(b.name).fields.find((x) => x.name === "nodes") : null;
      const recordType = record ? base(record.type).name : b.name;
      const t = types.get(recordType);
      const example = record
        ? `{\n  ${f.name}(first: 10) {\n    nodes ${selection(recordType, 0, 2)}\n    pageInfo { hasNextPage endCursor }\n  }\n}\n`
        : `query($id: ID!) {\n  ${f.name}(id: $id) ${selection(recordType, 0, 1)}\n}\n`;
      box.append(el("details", {},
        el("summary", {}, f.name, " ", el("span", { class: "type" }, typeName(f.type))),
        f.description ? el("p", { class: "muted" }, f.description) : null,
        el("ul", {},
          el("li", {}, el("button", { type: "button", onclick: () => insert(example, record ? "{}" : '{ "id": "" }') }, "Use as query")),
          f.args.map((a) => el("li", {}, "(" + a.name + ": ", el("span", { class: "type" }, typeName(a.type)), ")")),
          ((t && t.fields) || []).map((x) => el("li", {}, x.name + ": ", el("span", { class: "type" }, typeName(x.type))))),
      ));
    }
  }
  schema().catch((e) => { document.getElementById("gq-schema").textContent = String(e); });
})();
