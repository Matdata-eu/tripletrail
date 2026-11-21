// Forms from SHACL shapes (#80): one renderer for every page that draws a form from shapes, the
// Shapes page's settings today and Records' record forms next. Editors and viewers are looked up
// by the local name of a field's `dash:editor` or `dash:viewer` in two registries, so a page can
// add its own. Nothing here is global state: each call gets a context.
//
// A field is a plain object, as `GET …/shapes/form` answers them: `label`, `description`,
// `editor` (`BooleanSelectEditor`…), `viewer`, `datatype` (`integer`…), `in` (`{value, label}`),
// `multiple`, `pattern`, `min`, `max`, `class`.
//
// A context: `el` (the page's element maker), `id` (for the control), `value` (now), `set(v)`
// (called with a new value), `editable`, `name` (a check box's aria-label), `view` (show the
// viewer when not editable), and optionally
// `custom(field, ctx)`, which may answer a control of its own before the registry is asked.
"use strict";

(function (root) {
  const editors = new Map();
  const viewers = new Map();

  /** Adds or replaces the editor called `name` (a `dash:editor`'s local name). */
  function registerEditor(name, make) { editors.set(name, make); }
  /** Adds or replaces the viewer called `name` (a `dash:viewer`'s local name). */
  function registerViewer(name, make) { viewers.set(name, make); }

  const NUMBERS = new Set(["integer", "decimal", "double", "float", "long", "int", "short", "byte",
    "nonNegativeInteger", "positiveInteger", "negativeInteger", "nonPositiveInteger",
    "unsignedLong", "unsignedInt", "unsignedShort", "unsignedByte"]);

  /** The editor for a field: its `dash:editor`, else one that fits its values, as DASH scores them. */
  function editorName(f) {
    if (f.editor && editors.has(f.editor)) return f.editor;
    if (f.datatype === "boolean") return "BooleanSelectEditor";
    if (f.in) return "EnumSelectEditor";
    if (f.datatype === "date") return "DatePickerEditor";
    if (f.datatype === "dateTime") return "DateTimePickerEditor";
    if (f.class) return "URIEditor";
    return "TextFieldEditor";
  }

  /** The viewer for a field: its `dash:viewer`, else one for its kind of values. */
  function viewerName(f) {
    if (f.viewer && viewers.has(f.viewer)) return f.viewer;
    if (f.class || f.nodeKind === "IRI") return "LabelViewer";
    return "LiteralViewer";
  }

  /** The control that edits the field's value. */
  function editor(f, ctx) {
    if (ctx.custom) {
      const own = ctx.custom(f, ctx);
      if (own) return own;
    }
    return editors.get(editorName(f))(f, ctx);
  }

  /** The field's value, read-only. */
  function viewer(f, ctx) {
    return viewers.get(viewerName(f))(f, ctx);
  }

  /** A labelled field: the label, the editor (or the viewer, when not editable) and the description. */
  function field(f, ctx, extra) {
    const control = ctx.editable === false && ctx.view ? viewer(f, ctx) : editor(f, ctx);
    return ctx.el("div", { class: "shapes-field" },
      ctx.el("label", { for: ctx.id }, f.label, extra || null),
      control,
      f.description ? ctx.el("span", { class: "muted small" }, f.description) : null);
  }

  // The label names a control through `for`; a box also gets `name` as its aria-label.
  const common = (f, ctx) => ({ id: ctx.id, disabled: ctx.editable === false });

  // ---------- editors ----------
  registerEditor("BooleanSelectEditor", (f, ctx) =>
    ctx.el("input", { ...common(f, ctx), type: "checkbox", "aria-label": ctx.name || null, checked: ctx.value === true, onchange: (e) => ctx.set(e.target.checked) }));

  registerEditor("EnumSelectEditor", (f, ctx) => {
    if (f.multiple) {
      // Several values: a box per value, in a list that opens.
      const chosen = new Set(Array.isArray(ctx.value) ? ctx.value : []);
      const label = (v) => (f.in.find((o) => o.value === v) || { label: v }).label;
      const text = (list) => (list.length ? list.map(label).join(", ") : ctx.el("span", { class: "muted" }, "none"));
      const summary = ctx.el("summary", {}, text(f.in.map((o) => o.value).filter((v) => chosen.has(v))));
      return ctx.el("details", { class: "shapes-readers" }, summary,
        f.in.map((o) => ctx.el("label", { class: "small", title: o.value },
          ctx.el("input", { type: "checkbox", disabled: ctx.editable === false, checked: chosen.has(o.value), onchange: (e) => {
            if (e.target.checked) chosen.add(o.value); else chosen.delete(o.value);
            const list = f.in.map((x) => x.value).filter((v) => chosen.has(v));
            summary.replaceChildren(text(list));
            ctx.set(list);
          } }), " ", o.label)));
    }
    return ctx.el("select", { ...common(f, ctx), onchange: (e) => ctx.set(e.target.value) },
      ctx.el("option", { value: "", selected: !ctx.value }, "none"),
      f.in.map((o) => ctx.el("option", { value: o.value, selected: o.value === ctx.value, title: o.value }, o.label)));
  });

  const textArea = (f, ctx) => ctx.el("textarea", { ...common(f, ctx), rows: 2, cols: 40, onchange: (e) => ctx.set(e.target.value) },
    ctx.value == null ? "" : String(ctx.value));
  registerEditor("TextAreaEditor", textArea);
  registerEditor("TextAreaWithLangEditor", textArea);

  registerEditor("TextFieldEditor", (f, ctx) => {
    const number = NUMBERS.has(f.datatype);
    const integer = number && f.datatype !== "decimal" && f.datatype !== "double" && f.datatype !== "float";
    return ctx.el("input", { ...common(f, ctx), type: number ? "number" : "text", value: ctx.value == null ? "" : ctx.value,
      step: integer ? 1 : number ? "any" : null, min: f.min ?? null, max: f.max ?? null, pattern: f.pattern || null,
      onchange: (e) => {
        if (!e.target.checkValidity()) { e.target.reportValidity(); return; }
        ctx.set(number && e.target.value !== "" ? Number(e.target.value) : e.target.value);
      } });
  });
  registerEditor("TextFieldWithLangEditor", editors.get("TextFieldEditor"));

  registerEditor("DatePickerEditor", (f, ctx) =>
    ctx.el("input", { ...common(f, ctx), type: "date", value: ctx.value == null ? "" : String(ctx.value).slice(0, 10), onchange: (e) => ctx.set(e.target.value) }));
  registerEditor("DateTimePickerEditor", (f, ctx) =>
    ctx.el("input", { ...common(f, ctx), type: "datetime-local", step: 1, value: ctx.value == null ? "" : String(ctx.value).replace(/(Z|[+-]\d\d:\d\d)$/, "").slice(0, 19),
      onchange: (e) => ctx.set(e.target.value) }));

  registerEditor("URIEditor", (f, ctx) =>
    ctx.el("input", { ...common(f, ctx), type: "url", size: 40, value: ctx.value == null ? "" : ctx.value,
      onchange: (e) => {
        if (!e.target.checkValidity()) { e.target.reportValidity(); return; }
        ctx.set(e.target.value);
      } }));
  registerEditor("InstancesSelectEditor", editors.get("URIEditor"));

  // ---------- viewers ----------
  const values = (ctx) => (Array.isArray(ctx.value) ? ctx.value : ctx.value == null || ctx.value === "" ? [] : [ctx.value]);
  const list = (ctx, items) => (items.length ? ctx.el("span", { class: "form-values" }, items) : ctx.el("span", { class: "muted" }, "none"));
  /** A value's label: `{label}` when the page gave one, else the IRI's last segment. */
  const labelOf = (v) => (v && typeof v === "object" ? v.label || v.value : String(v).replace(/^.*[#/:](?=[^#/:]+$)/, ""));
  const iriOf = (v) => (v && typeof v === "object" ? v.value : String(v));
  /** The IRI as a link target when it is a web address (http or https), else null: values come
   *  from the data, and a `javascript:` IRI must never become a link or an image source. */
  const webLink = (iri) => (/^https?:\/\//i.test(iri) ? iri : null);
  /** A link to the IRI when it is a web address, else the text alone. */
  const link = (ctx, v, text, title) => (webLink(iriOf(v))
    ? ctx.el("a", { href: webLink(iriOf(v)), title: title || null, rel: "noopener noreferrer" }, text)
    : ctx.el("span", { title: title || null }, text));

  registerViewer("LiteralViewer", (f, ctx) => list(ctx, values(ctx).map((v, i) =>
    ctx.el("span", {}, i ? ", " : null, v && typeof v === "object" ? (v.label ?? v.value) : String(v)))));
  registerViewer("URIViewer", (f, ctx) => list(ctx, values(ctx).map((v, i) =>
    ctx.el("span", {}, i ? ", " : null, link(ctx, v, iriOf(v))))));
  registerViewer("LabelViewer", (f, ctx) => list(ctx, values(ctx).map((v, i) =>
    ctx.el("span", {}, i ? ", " : null, link(ctx, v, labelOf(v), iriOf(v))))));
  // Only web images, fetched without telling their host which page shows them.
  registerViewer("ImageViewer", (f, ctx) => list(ctx, values(ctx).map((v) => (webLink(iriOf(v))
    ? ctx.el("img", { src: webLink(iriOf(v)), alt: f.label || "", loading: "lazy", referrerpolicy: "no-referrer", style: "max-width: 240px; max-height: 160px" })
    : ctx.el("span", {}, iriOf(v))))));
  registerViewer("HTMLViewer", viewers.get("LiteralViewer")); // shown as text: no markup from data
  registerViewer("BlankNodeViewer", viewers.get("LiteralViewer"));

  const api = { registerEditor, registerViewer, editorName, viewerName, editor, viewer, field, webLink };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.TripleTrailForms = api;
})(typeof window !== "undefined" ? window : globalThis);
