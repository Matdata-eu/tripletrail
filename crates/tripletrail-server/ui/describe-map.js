// The map of a describe page (#140): the resource's geometries, and muted what is around them,
// over the server's base map, with MapLibre GL JS from the web UI. The page's drawing stays when
// this cannot run.
const element = document.getElementById("mg-live-map");
const source = document.getElementById("mg-map-data");
const vendor = new URL("vendor/maplibre/", import.meta.url);

async function show() {
  if (!element || !source) return;
  const data = JSON.parse(source.textContent);
  const css = document.createElement("link");
  css.rel = "stylesheet";
  css.href = new URL("maplibre-gl.css", vendor).href;
  document.head.append(css);
  const lib = await import(new URL("maplibre-gl.mjs", vendor).href);
  if (!lib.Map) return;
  element.hidden = false;
  const accent = getComputedStyle(document.documentElement).getPropertyValue("--accent").trim() || "#1f5fbf";
  // The base map the server names (--map-tiles), or none: then nothing comes from elsewhere.
  const tiles = element.dataset.tiles;
  const style = tiles
    ? {
        version: 8,
        sources: {
          base: {
            type: "raster",
            tiles: [tiles],
            tileSize: 256,
            maxzoom: 19,
            attribution: element.dataset.attribution || "",
          },
        },
        layers: [{ id: "base", type: "raster", source: "base" }],
      }
    : {
        version: 8,
        sources: {},
        layers: [
          {
            id: "background",
            type: "background",
            paint: { "background-color": getComputedStyle(document.body).backgroundColor || "#fff" },
          },
        ],
      };
  const map = new lib.Map({
    container: element,
    style,
    bounds: data.bbox,
    fitBoundsOptions: { padding: 40, maxZoom: 17 },
    cooperativeGestures: true,
  });
  map.addControl(new lib.NavigationControl(), "top-right");
  map.addControl(new lib.ScaleControl(), "bottom-left");
  const muted = getComputedStyle(document.documentElement).getPropertyValue("--muted").trim() || "#5b6475";
  map.on("load", () => {
    map.addSource("resource", { type: "geojson", data });
    // Multi-geometries are of their part's type, whatever the version reports.
    const of = (...types) => ["in", ["geometry-type"], ["literal", types]];
    const polygons = of("Polygon", "MultiPolygon");
    // What the page shows around the resource (rd:context), muted and under it.
    for (const context of [true, false]) {
      const where = (f) => ["all", f, ["==", ["get", "context"], context]];
      const colour = context ? muted : accent;
      const id = context ? "context-" : "";
      map.addLayer({
        id: `${id}fill`,
        type: "fill",
        source: "resource",
        filter: where(polygons),
        paint: { "fill-color": colour, "fill-opacity": context ? 0.1 : 0.2 },
      });
      map.addLayer({
        id: `${id}line`,
        type: "line",
        source: "resource",
        filter: where(of("LineString", "MultiLineString", "Polygon", "MultiPolygon")),
        paint: { "line-color": colour, "line-width": context ? 2 : 3 },
      });
      map.addLayer({
        id: `${id}point`,
        type: "circle",
        source: "resource",
        filter: where(of("Point", "MultiPoint")),
        paint: {
          "circle-color": colour,
          "circle-radius": context ? 4 : 6,
          "circle-stroke-color": "#fff",
          "circle-stroke-width": context ? 1 : 2,
        },
      });
    }
  });
  const drawing = document.getElementById("mg-svg-map");
  if (drawing) drawing.style.display = "none";
}

show().catch((e) => {
  if (element) element.hidden = true;
  console.warn("TripleTrail: the interactive map could not start", e);
});
