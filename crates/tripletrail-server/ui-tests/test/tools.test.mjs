// The reading tools of the web UI as a user sees them (#40): Search, Map, Explore, Describe,
// Query and Records (#81), in Chromium, on the guide's sample network. `TRIPLETRAIL_BIN` names the
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

test("Records: a class's records sort, filter and open as a form", async (t) => {
  const page = await open(t);
  await page.goto(base + "/$/ui/records/ds?class=Section");
  const count = page.locator("#records-count");
  await count.filter({ hasText: "12 records" }).waitFor();
  assert.equal(await page.locator("#records-table .tabulator-row").count(), 12);
  const classes = (await page.locator("#records-classes").innerText()).replace(/\s+/g, " ");
  assert.match(classes, /Section 12/);
  assert.match(classes, /Station 10/);
  // Sorted by length, longest first (a second click on the header).
  const length = page.locator('.tabulator-col[tabulator-field="lengthKm"] .tabulator-col-title');
  const sorted = (dir) => page.waitForResponse((r) => r.url().includes("/search/results") && decodeURIComponent(r.url()).includes('"desc":' + dir));
  let answer = sorted("false");
  await length.click();
  await answer;
  answer = sorted("true");
  await length.click();
  await answer;
  await page.waitForFunction(() => /S07/.test(document.querySelector("#records-table .tabulator-row")?.innerText || ""));
  // A header filter: the sections faster than 200 km/h.
  await page.locator('.tabulator-col[tabulator-field="maxSpeed"] input').pressSequentially(">200");
  await count.filter({ hasText: /^1 record$/ }).waitFor();
  // CSV: read with the page's token and saved as a file.
  const [download] = await Promise.all([page.waitForEvent("download"), page.click("#records-csv")]);
  assert.equal(download.suggestedFilename(), "Section.csv");
  // The record: a form of its values, its links open records here.
  await page.locator("#records-table .tabulator-row a").first().click();
  await page.locator(".records-record-head h2", { hasText: "S07" }).waitFor();
  const form = (await page.locator("#records-record").innerText()).replace(/\s+/g, " ");
  assert.match(form, /maxSpeed 300/);
  assert.match(form, /from Leuven/);
  assert.match(page.url(), /iri=https%3A%2F%2Fexample\.org%2Frail%23S07/);
  await page.locator("#records-record a", { hasText: "Leuven" }).click();
  await page.locator(".records-record-head h2", { hasText: "Leuven" }).waitFor();
  assert.match(await page.locator("#records-incoming").innerText(), /S06/);
});

test("Records: markup in the data stays text in the grid", async (t) => {
  const page = await open(t);
  // A search answer whose values and title hold markup, as data from anywhere could.
  const markup = '<img src=x onerror="window.ran=1">';
  await page.route(/\/ds\/search\/results/, (route) => route.fulfill({
    contentType: "application/json",
    body: JSON.stringify({ class: "Station", title: "Station", count: 1, offset: 0, more: false, columns: [], sparql: "",
      rows: [{ iri: EX + "X", label: markup, values: { code: markup, label: markup } }] }),
  }));
  await page.route(/\/ds\/search\/classes/, (route) => route.fulfill({
    contentType: "application/json",
    body: JSON.stringify({ classes: [{ id: "Station", title: "Station", class: EX + "Station", count: 1, geometry: false,
      properties: [{ key: "code", title: markup, path: "<" + EX + "code>", kind: "text", single: true }] }] }),
  }));
  await page.goto(base + "/$/ui/records/ds?class=Station");
  await page.locator("#records-count", { hasText: "1 record" }).waitFor();
  await page.waitForTimeout(300);
  assert.equal(await page.locator("#page-records img").count(), 0, "no element made from the data");
  assert.equal(await page.evaluate(() => window.ran), undefined);
  assert.match(await page.locator("#records-table").innerText(), /onerror/);
});

test("Records: a record is created, edited, kept on a conflict and deleted in its form", async (t) => {
  const page = await open(t);
  await page.goto(base + "/$/ui/records/ds?class=Station");
  await page.locator("#records-count", { hasText: "10 records" }).waitFor();
  // A new station: the server checks it as it is typed, and gives it an IRI.
  await page.click("#records-new");
  await page.locator(".records-record-head h2", { hasText: "New Station" }).waitFor();
  await page.fill("#records-edit-iri", EX + "DUF");
  await page.fill("#records-edit-label-0", "Duffel");
  await page.fill("#records-edit-code-0", "DUF");
  await page.locator("#records-edit-code-0").blur();
  await page.locator(".records-edit-status", { hasText: "Ready to save." }).waitFor();
  await page.click("#records-save");
  await page.locator(".records-record-head h2", { hasText: "Duffel" }).waitFor();
  await page.locator("#records-count", { hasText: "11 records" }).waitFor();
  // Edited: only the key that changed is sent, with the record's ETag.
  await page.click("#records-edit");
  await page.locator("#records-edit-label-0").waitFor();
  const sent = page.waitForRequest((r) => r.method() === "PATCH" && !r.url().includes("dry=true"));
  await page.fill("#records-edit-label-0", "Duffel-Centrum");
  await page.locator("#records-edit-label-0").blur();
  await page.click("#records-save");
  const patch = await sent;
  assert.deepEqual(patch.postDataJSON(), { label: "Duffel-Centrum" });
  assert.ok(patch.headers()["if-match"], "the ETag goes with the change");
  await page.locator(".records-record-head h2", { hasText: "Duffel-Centrum" }).waitFor();
  // Someone else saves the record meanwhile: a dialog shows theirs against mine.
  await page.click("#records-edit");
  await page.locator("#records-edit-code-0").waitFor();
  const url = base + "/ds/api/v1/Station/" + encodeURIComponent(EX + "DUF");
  const r = await fetch(url, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ code: "DFL" }) });
  assert.equal(r.status, 200);
  await page.fill("#records-edit-code-0", "DFF");
  await page.locator("#records-edit-code-0").blur();
  await page.click("#records-save");
  const dialog = page.locator("dialog.records-conflict");
  await dialog.waitFor();
  assert.match((await dialog.innerText()).replace(/\s+/g, " "), /code DFL DFF/);
  await dialog.locator("button", { hasText: "Keep mine" }).click();
  await page.locator("#records-edit").waitFor(); // the record again, saved
  assert.equal((await (await fetch(url + "?f=json")).json()).code, "DFF");
  // Deleted, after saying what links to it.
  await page.click("#records-edit");
  await page.click("#records-delete");
  await page.locator("#confirm-dialog", { hasText: "No link you can see from another resource points to this record." }).waitFor();
  await page.click("#confirm-ok");
  await page.locator("#records-count", { hasText: "10 records" }).waitFor();
  assert.equal((await fetch(url + "?f=json")).status, 404);
});

test("Records: a reference is picked by name, and the server's problems show at their fields", async (t) => {
  const page = await open(t);
  // The shapes of the sample network ask nothing more, so the check's answer is the server's 422 as a shape would give it.
  await page.route(/\/ds\/api\/v1\/Section\/[^?]+\?dry=true/, (route) => route.fulfill({
    status: 422, contentType: "application/problem+json",
    body: JSON.stringify({ title: "Not valid", errors: [{ key: "maxSpeed", value: 900, message: "At most 350", constraint: "sh:maxInclusive" }] }),
  }));
  await page.goto(base + "/$/ui/records/ds?class=Section&iri=" + encodeURIComponent(EX + "S07"));
  await page.click("#records-edit");
  const to = page.locator("#records-edit-to-0");
  await to.waitFor();
  assert.equal(await to.inputValue(), "Liège-Guillemins");
  await to.fill("Leu");
  await page.locator("#records-edit-to-0-list option[value='Leuven']").waitFor({ state: "attached" });
  const checked = page.waitForRequest((r) => r.method() === "PATCH" && r.url().includes("dry=true"));
  await to.fill("Leuven");
  assert.deepEqual((await checked).postDataJSON(), { to: EX + "LV" });
  await page.fill("#records-edit-maxSpeed-0", "900");
  await page.locator("#records-edit-maxSpeed-0").blur();
  await page.locator('.records-edit-field[data-key="maxSpeed"] .records-edit-problem', { hasText: "At most 350" }).waitFor();
  assert.match(await page.locator(".records-edit-status").innerText(), /Fix the fields/);
  // Left without saving: the record is as it was.
  await page.click("text=Cancel");
  await page.locator("#records-edit").waitFor();
  assert.match(await page.locator("#records-record").innerText(), /Liège-Guillemins/);
});
