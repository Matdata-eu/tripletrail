// The REST API's docs page (#143): Swagger UI over the dataset's OpenAPI definition.
(function () {
  const root = document.getElementById("swagger-ui");
  const dataset = root.dataset.dataset;
  // `?preview=` (from the Shapes page): the definition with proposed shapes, not saved.
  const preview = new URLSearchParams(location.search).get("preview");
  const spec = "/" + encodeURIComponent(dataset) + "/api/openapi.json" + (preview && /^[0-9a-f]{32}$/.test(preview) ? "?preview=" + preview : "");
  document.getElementById("api-docs-spec").href = spec;
  // eslint-disable-next-line no-undef
  SwaggerUIBundle({
    url: spec,
    dom_id: "#swagger-ui",
    deepLinking: true,
    // Requests from "Try it out" carry the browser's credentials, like the rest of the UI.
    withCredentials: true,
    defaultModelsExpandDepth: 0,
  });
})();
