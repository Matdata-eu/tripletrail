// The five reading tools of the web UI as a user sees them (#40): Search, Map, Explore,
// Describe and Query, in Chromium, on the guide's sample network. `TRIPLETRAIL_BIN` names the
// `tripletrail` binary (`target/debug/tripletrail` of the workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

// require() also finds a Playwright installed globally (NODE_PATH), which import does not.
const { chromium } = createRequire(import.meta.url)("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const workspace = join(here, "..", "..", "..", "..");
const bin =
  process.env.TRIPLETRAIL_BIN ??
  join(resolve(workspace, process.env.CARGO_TARGET_DIR ?? "target"), "debug", "tripletrail");
const network = join(workspace, "docs", "guide", "data", "network.ttl");
const EX = "https://example.org/rail#";

let server;
let browser;
let base;

/** Starts `tripletrail serve` with the sample network on a free port, and waits until it is ready. */
async function start() {
  const child = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const address = await new Promise((resolve, reject) => {
    let seen = "";
    const look = (chunk) => {
      seen += chunk;
      const m = seen.match(/listening on (https?:\/\/[^/\s]+)\//);
      if (m) resolve(m[1]);
    };
    child.stdout.on("data", look);
    child.stderr.on("data", look);
    child.on("exit", (code) => reject(new Error(`tripletrail exited (${code}): ${seen}`)));
  });
  for (let i = 0; i < 200; i++) {
    const r = await fetch(address + "/$/ready").catch(() => null);
    if (r && r.ok && (await r.json()).ready) break;
    await new Promise((r) => setTimeout(r, 50));
  }
  return { child, address };
}

before(async () => {
  assert.ok(existsSync(bin), `no tripletrail binary at ${bin}: cargo build -p tripletrail-server`);
  ({ child: server, address: base } = await start());
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
  server?.kill();
});

/**
 * A page of a fresh browser context: wide enough for the top bar's search box, without the
 * first-visit tour, and reaching only the test's server (the map's base tiles are refused, so
 * the tests need no network). Errors thrown on the page fail the test.
 */
async function open(t) {
  const context = await browser.newContext({ viewport: { width: 1500, height: 1000 } });
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await context.route("**/*", (route) =>
    route.request().url().startsWith(base) ? route.continue() : route.abort());
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  t.after(async () => {
    await context.close();
    assert.deepEqual(errors, [], "errors on the page");
  });
  return page;
}

test("Search: a name finds its resource, and the builder runs a query", async (t) => {
  const page = await open(t);
  await page.goto(base + "/$/ui/search/ds");
  // The top bar's box: a name gives the resource, linking to its describe page.
  await page.fill("#top-search input", "Mechelen");
  const hit = page.locator("#top-search-list a").first();
  await hit.waitFor();
  assert.match(await hit.textContent(), /Mechelen/);
  assert.equal(
    await hit.getAttribute("href"),
    base + "/ds/describe?iri=" + encodeURIComponent(EX + "MEC"),
  );
  // The builder: its first class, the 12 sections, as a table.
  await page.click("#sb-run");
  await page.locator("#sb-count", { hasText: "12 results" }).waitFor();
  assert.equal(await page.locator("#sb-results tbody tr").count(), 12);
  assert.match(await page.textContent("#sb-sparql"), /SELECT/);
});

test("Map: the style loads and a layer draws features", async (t) => {
  const page = await open(t);
  const style = page.waitForResponse((r) => r.url() === base + "/ds/tiles/style.json");
  // A tile of the area the sample lies in, with features in it.
  const tile = page.waitForResponse(async (r) =>
    /\/ds\/tiles\/[^/]+\/\d+\/\d+\/\d+\.mvt$/.test(r.url()) && r.status() === 200 && (await r.body()).length > 0);
  await page.goto(base + "/$/ui/map/ds");
  assert.equal((await style).status(), 200);
  await tile;
  // The layers list each class with its number of features.
  const layers = page.locator("#map-layers");
  await layers.getByText("Station").waitFor();
  const text = (await layers.innerText()).replace(/\s+/g, " ");
  assert.match(text, /Section 12/);
  assert.match(text, /Station 10/);
  assert.ok(await page.locator("#map-canvas canvas").count(), "the map draws on a canvas");
});

test("Explore: a start IRI draws its node and its neighbours", async (t) => {
  const page = await open(t);
  await page.goto(base + "/$/ui/explore/ds?iri=" + encodeURIComponent(EX + "BXS"));
  const nodes = page.locator(".ex-node");
  await nodes.filter({ hasText: "S01" }).first().waitFor();
  const labels = await nodes.allTextContents();
  assert.ok(labels.some((l) => l.includes("Brussels-South")), labels.join(" | "));
  // Brussels-South starts sections S01, S08 and S09.
  for (const s of ["S01", "S08", "S09"]) assert.ok(labels.some((l) => l.includes(s)), `${s} in ${labels.join(" | ")}`);
  assert.ok(await page.locator(".ex-node.start").count(), "the start node is marked");
});

test("Describe: a resource's page shows its sections", async (t) => {
  const page = await open(t);
  await page.goto(base + "/ds/describe?iri=" + encodeURIComponent(EX + "BXS"));
  assert.equal(await page.textContent("h1"), "Brussels-South");
  const headings = await page.locator("h2").allTextContents();
  for (const h of ["Attributes", "All properties", "Incoming links", "Graph", "RDF"]) {
    assert.ok(headings.includes(h), `"${h}" in ${headings.join(", ")}`);
  }
  // The incoming links include the sections that end at the station.
  assert.match(await page.locator("#tab-incoming").innerText(), /S0\d/);
});

test("Query: a SELECT answers a table of results", async (t) => {
  const page = await open(t);
  const q = `SELECT ?s WHERE { ?s a <${EX}Station> } ORDER BY ?s`;
  await page.goto(base + "/$/ui/query/ds?query=" + encodeURIComponent(q));
  // The query editor's own tour, on a first visit.
  const skip = page.getByRole("button", { name: "Skip" });
  await page.locator(".yasgui-runButton").waitFor();
  if (await skip.count()) await skip.first().click();
  await page.click(".yasgui-runButton");
  // The table may be drawn more than once (the editor can also run the linked query on
  // opening): wait until it holds the ten stations, then read it in one go.
  const first = await page.waitForFunction(() => {
    const rows = document.querySelectorAll(".yasr .tabulator-row");
    return rows.length === 10 && rows[0].innerText;
  });
  assert.match(await first.jsonValue(), /ANT/);
});
