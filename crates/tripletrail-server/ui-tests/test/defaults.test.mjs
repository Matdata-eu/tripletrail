// What a new viewer starts on (#262): `--ui-home`, `--default-dataset`, `--ui-theme`,
// `--map-view`, `--examples` and `--default-query` in the web UI. `TRIPLETRAIL_BIN` names the `tripletrail` binary (`target/debug/tripletrail` of the
// workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
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
let dir;

before(async () => {
  assert.ok(existsSync(bin), `no tripletrail binary at ${bin}: cargo build -p tripletrail-server`);
  dir = mkdtempSync(join(tmpdir(), "tripletrail-ui-defaults-"));
  mkdirSync(join(dir, "examples"));
  writeFileSync(join(dir, "examples", "1-count.rq"), "# Count the triples\nSELECT (COUNT(*) AS ?n) WHERE { ?s ?p ?o }\n");
  writeFileSync(join(dir, "examples", "2-classes.rq"), "# The classes\nSELECT DISTINCT ?c WHERE { ?s a ?c }\n");
  writeFileSync(join(dir, "default.rq"), "PREFIX ex: <http://example.com/>\nSELECT * WHERE { ?s ?p ?o } LIMIT 5\n");
  server = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network,
    "--ui-home", "map", "--default-dataset", "rail", "--ui-theme", "dark", "--map-view", "7/50.8/4.4",
    "--examples", join(dir, "examples"), "--default-query", "rail=" + join(dir, "default.rq")],
  { stdio: ["ignore", "pipe", "pipe"] });
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
  // A second dataset after the first, which the pages open on by --default-dataset.
  const made = await fetch(base + "/$/datasets", { method: "POST", body: new URLSearchParams({ dbName: "rail", dbType: "mem" }) });
  assert.equal(made.status, 200);
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
  server?.kill();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

test("a new viewer lands on the map of the default dataset, dark, at the start view", async () => {
  const context = await browser.newContext({ colorScheme: "light" });
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  // Only the test's server: the base map's tiles are refused.
  await context.route((url) => !url.href.startsWith(base), (route) => route.abort());
  const page = await context.newPage();
  await page.goto(base + "/");
  await page.waitForURL(/\/\$\/ui\/map/);
  assert.equal(await page.locator("html").getAttribute("data-theme"), "dark");
  // The map of rail (empty, so no extent): the start view in the map's link.
  await page.waitForFunction(() => /^#7\/50\.8\/4\.4/.test(location.hash), null, { timeout: 15000 });
  assert.equal(await page.locator("#map-ds").inputValue(), "rail");
  // The brand link goes home.
  assert.equal(await page.locator("a.brand").getAttribute("href"), "/$/ui/map");
  // The Query page opens on rail too.
  await page.locator('.tabs a[data-page="query"]').click();
  await page.waitForFunction(() => document.querySelector("#query-ds") && document.querySelector("#query-ds").value === "rail");
  // The viewer's own theme wins over the server's, and auto is a choice too.
  await page.locator("#theme-btn").click();
  assert.equal(await page.evaluate(() => localStorage.getItem("tripletrail-theme")), "auto");
  assert.equal(await page.locator("html").getAttribute("data-theme"), "light");
  await page.reload();
  assert.equal(await page.locator("html").getAttribute("data-theme"), "light");
  await context.close();
});

test("the Query page offers the examples, and a first visit opens them as tabs", async () => {
  const context = await browser.newContext();
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await context.route((url) => !url.href.startsWith(base), (route) => route.abort());
  const page = await context.newPage();
  // A first visit before the dataset had examples seeds nothing, and keeps the first visit for later.
  const none = (route) => route.fulfill({ json: { examples: [], defaultQuery: null } });
  await page.route("**/rail/examples", none);
  await page.goto(base + "/$/ui/query/rail");
  await page.waitForResponse("**/rail/examples").catch(() => {});
  await page.locator("#yasgui .tabsList .tab").first().waitFor();
  await page.waitForTimeout(300);
  assert.equal(await page.evaluate(() => localStorage.getItem("tripletrail-query-seeded")), null);
  await page.unroute("**/rail/examples", none);
  await page.reload();
  const examples = page.locator("#query-examples");
  await examples.waitFor();
  assert.deepEqual(await examples.locator("option").allTextContents(), ["Examples…", "Count the triples", "The classes"]);
  const tabs = page.locator("#yasgui .tabsList .tab");
  await page.waitForFunction(() => document.querySelectorAll("#yasgui .tabsList .tab").length === 3);
  assert.deepEqual((await tabs.allTextContents()).map((t) => t.trim()).slice(1), ["Count the triples", "The classes"]);
  // The first tab, selected, holds the dataset's default query text.
  await page.waitForFunction(() => document.querySelector("#yasgui .CodeMirror").CodeMirror.getValue().startsWith("PREFIX ex:"));
  // Picking an example opens it in a new tab.
  await examples.selectOption({ label: "The classes" });
  await page.waitForFunction(() => document.querySelectorAll("#yasgui .tabsList .tab").length === 4);
  // A reload keeps the viewer's tabs: no examples opened again.
  await page.reload();
  await page.locator("#query-examples").waitFor();
  await page.waitForTimeout(500);
  assert.equal(await tabs.count(), 4);
  // Another dataset has the examples too, but not rail's default query text.
  const other = await (await fetch(base + "/ds/examples")).json();
  assert.equal(other.examples.length, 2);
  assert.equal(other.defaultQuery, null);
  await context.close();
});
