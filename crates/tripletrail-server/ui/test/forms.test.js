// The shared form module (#80): `node --test "crates/tripletrail-server/ui/test/*.test.js"`.
"use strict";
const test = require("node:test");
const assert = require("node:assert");
const forms = require("../forms.js");

/** An element maker like the page's, building plain objects. */
const el = (tag, attrs = {}, ...children) => ({ tag, attrs, children: children.flat().filter((c) => c != null) });
/** Every element under `node` with tag `tag`. */
const all = (node, tag) => (node && typeof node === "object"
  ? [...(node.tag === tag ? [node] : []), ...node.children.flatMap((c) => all(c, tag))] : []);

test("only web IRIs become links and images", () => {
  const bad = ["javascript:alert(1)", "JaVaScRiPt:alert(1)", "data:text/html,<script>alert(1)</script>", "urn:x:1", "vbscript:x"];
  for (const viewer of ["URIViewer", "LabelViewer"]) {
    const out = forms.viewer({ viewer }, { el, value: [...bad, "https://example.org/a#b", "http://example.org/c"] });
    const hrefs = all(out, "a").map((a) => a.attrs.href);
    assert.deepStrictEqual(hrefs, ["https://example.org/a#b", "http://example.org/c"], viewer);
  }
  const images = forms.viewer({ viewer: "ImageViewer" }, { el, value: [...bad, "https://example.org/i.png"] });
  const img = all(images, "img");
  assert.deepStrictEqual(img.map((i) => i.attrs.src), ["https://example.org/i.png"]);
  assert.strictEqual(img[0].attrs.referrerpolicy, "no-referrer");
  // The others are shown as text.
  assert.strictEqual(all(images, "span").filter((s) => s.children[0] === "javascript:alert(1)").length, 1);
});

test("a field's editor follows dash:editor, else its values", () => {
  assert.strictEqual(forms.editorName({ editor: "TextAreaEditor" }), "TextAreaEditor");
  assert.strictEqual(forms.editorName({ datatype: "boolean" }), "BooleanSelectEditor");
  assert.strictEqual(forms.editorName({ in: [{ value: "a", label: "A" }] }), "EnumSelectEditor");
  assert.strictEqual(forms.editorName({ datatype: "date" }), "DatePickerEditor");
  assert.strictEqual(forms.editorName({ class: "https://example.org/C" }), "URIEditor");
  assert.strictEqual(forms.editorName({ editor: "NoSuchEditor" }), "TextFieldEditor");
  assert.strictEqual(forms.viewerName({ class: "https://example.org/C" }), "LabelViewer");
  assert.strictEqual(forms.viewerName({}), "LiteralViewer");
});
