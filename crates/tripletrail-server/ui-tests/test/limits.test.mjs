// A dataset's limits set from the Datasets page (#18), in Chromium: written to the limits file
// and applied at once, as `PUT /$/limits/{name}` does. `TRIPLETRAIL_BIN` names the
// `tripletrail` binary (`target/debug/tripletrail` of the workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, writeFileSync } from "node:fs";
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

const dir = mkdtempSync(join(tmpdir(), "tt-limits-"));
const file = join(dir, "limits.json");
let server;
let browser;
let base;

async function start(args) {
  const child = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network, ...args], {
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
  writeFileSync(file, JSON.stringify({ datasets: { ds: { maxRows: 500 } } }));
  ({ child: server, address: base } = await start(["--limits", file]));
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
  server?.kill();
});

async function open(t) {
  const context = await browser.newContext({ viewport: { width: 1300, height: 1000 } });
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

test("a dataset's limits are set, applied and cleared from its dialog", async (t) => {
  const page = await open(t);
  await page.goto(base + "/$/ui/datasets/ds");
  const form = page.locator("#limits-form");
  await form.waitFor();
  // The file's entry, with the defaults as placeholders.
  assert.equal(await form.locator("[name=maxRows]").inputValue(), "500");
  assert.equal(await form.locator("[name=maxConcurrent]").inputValue(), "");
  await form.locator("[name=maxConcurrent]").fill("2");
  await form.locator("[name=maxMemory]").fill("512MiB");
  await form.locator("[name=maxRows]").fill("");
  await page.click("#limits-save");
  await page.locator(".toast", { hasText: "Limits of ds saved" }).waitFor();
  // Written to the file and applied at once.
  const written = JSON.parse(readFileSync(file, "utf8")).datasets.ds;
  assert.deepEqual(written, { maxConcurrent: 2, maxMemory: "512MiB" });
  const applied = (await (await fetch(base + "/$/limits/ds")).json()).applied;
  assert.equal(applied.maxConcurrent, 2);
  assert.equal(applied.maxMemory, 512 * 1024 * 1024);
  // A refused value changes nothing and says why.
  await form.locator("[name=maxDisk]").fill("lots");
  await page.click("#limits-save");
  await page.locator(".toast.bad").waitFor();
  assert.equal(JSON.parse(readFileSync(file, "utf8")).datasets.ds.maxDisk, undefined);
  // Back to the defaults.
  await page.click("#limits-clear");
  await page.locator(".toast", { hasText: "takes the default limits" }).waitFor();
  assert.equal(JSON.parse(readFileSync(file, "utf8")).datasets?.ds, undefined);
});

test("without a limits file the dialog says how to get one", async (t) => {
  const { child, address } = await start([]);
  t.after(() => child.kill());
  const context = await browser.newContext();
  t.after(() => context.close());
  const page = await context.newPage();
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await page.goto(address + "/$/ui/datasets/ds");
  await page.locator("#limits-note", { hasText: "--limits FILE" }).waitFor();
  assert.equal(await page.locator("#limits-form").isHidden(), true);
});
