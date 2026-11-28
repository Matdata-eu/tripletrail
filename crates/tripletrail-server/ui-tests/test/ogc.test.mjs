// OGC API Features as a browser user meets it (QA journey 7): from the landing page to a
// collection's items in a box, page by page, to one feature, on the guide's sample network.
// `TRIPLETRAIL_BIN` names the `tripletrail` binary (`target/debug/tripletrail` by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

const { chromium } = createRequire(import.meta.url)("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const workspace = join(here, "..", "..", "..", "..");
const bin =
  process.env.TRIPLETRAIL_BIN ??
  join(resolve(workspace, process.env.CARGO_TARGET_DIR ?? "target"), "debug", "tripletrail");
const network = join(workspace, "docs", "guide", "data", "network.ttl");

let server;
let browser;
let base;

before(async () => {
  assert.ok(existsSync(bin), `no tripletrail binary at ${bin}: cargo build -p tripletrail-server`);
  server = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  base = await new Promise((resolve, reject) => {
    let seen = "";
    const look = (chunk) => {
      seen += chunk;
      const m = seen.match(/listening on (https?:\/\/[^/\s]+)\//);
      if (m) resolve(m[1]);
    };
    server.stdout.on("data", look);
    server.stderr.on("data", look);
    server.on("exit", (code) => reject(new Error(`tripletrail exited (${code}): ${seen}`)));
  });
  for (let i = 0; i < 200; i++) {
    const r = await fetch(base + "/$/ready").catch(() => null);
    if (r && r.ok && (await r.json()).ready) break;
    await new Promise((r) => setTimeout(r, 50));
  }
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
  server?.kill();
});

async function open(t, width) {
  const context = await browser.newContext({ viewport: { width, height: 900 } });
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

for (const width of [1440, 390]) {
  test(`OGC API pages at ${width} px: landing page to the features in a box, page by page`, async (t) => {
    const page = await open(t, width);
    await page.goto(base + "/ds/ogc?f=html");
    await page.getByRole("link", { name: "Collections" }).first().click();
    await page.getByRole("link", { name: "Station", exact: true }).first().click();
    await page.waitForURL(/\/collections\/Station\?/);

    // The box over Brussels, Mechelen and Leuven, as the guide's chapter 10 asks for it.
    const box = "4.2,50.75,4.8,51.05";
    const json = await (await fetch(`${base}/ds/ogc/collections/Station/items?bbox=${box}&limit=2`)).json();
    assert.equal(json.numberMatched, 5);

    await page.goto(`${base}/ds/ogc/collections/Station/items?f=html&bbox=${box}&limit=2`);
    const seen = [];
    for (;;) {
      const codes = await page.locator("main table tbody tr td:nth-child(2)").allTextContents();
      assert.ok(codes.length > 0 && codes.length <= 2, `a page holds 1 or 2 features: ${codes}`);
      assert.equal(await page.locator("svg.sketch a").count(), codes.length, "each feature is drawn");
      seen.push(...codes);
      const next = page.getByRole("link", { name: "Next page" });
      if (!(await next.count())) break;
      await next.click();
      await page.waitForLoadState("load");
    }
    assert.equal(new Set(seen).size, json.numberMatched, `every feature once over the pages: ${seen}`);

    await page.goto(`${base}/ds/ogc/collections/Station/items?f=html&bbox=${box}&limit=2`);
    await page.locator("main table tbody tr a").first().click();
    await page.waitForURL(/\/items\/[^?]+\?f=html/);
    assert.match(await page.locator("main").innerText(), /Brussels/);
    // A feature page does not scroll sideways, even with a long IRI as its heading (#372).
    const wide = () => page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
    assert.ok((await wide()) <= 0, `the feature page is ${await wide()} px too wide`);
    const iri = "https://example.org/rail/functionalInfrastructure/operationalPoints/" + "x".repeat(40);
    assert.ok(iri.length >= 100);
    const update = `PREFIX geo: <http://www.opengis.net/ont/geosparql#>
      INSERT DATA { <${iri}> a <https://example.org/rail#Station> ;
        <http://www.w3.org/2000/01/rdf-schema#label> "A station with a long IRI" ;
        geo:hasGeometry [ geo:asWKT "POINT(5.9 49.6)"^^geo:wktLiteral ] }`;
    const r = await fetch(`${base}/ds/update`, { method: "POST", body: new URLSearchParams({ update }) });
    assert.ok(r.ok, `update: ${r.status} ${await r.text()}`);
    await page.goto(`${base}/ds/ogc/collections/Station/items/${encodeURIComponent(iri)}?f=html`);
    assert.match(await page.locator("main").innerText(), /long IRI/);
    assert.ok((await wide()) <= 0, `a long IRI's page is ${await wide()} px too wide`);
  });
}
