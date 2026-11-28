// The top bar's tabs at common screen widths (#365): every tab can be clicked, or scrolled to
// and then clicked, and none lies under the search box. `TRIPLETRAIL_BIN` names the
// `tripletrail` binary (`target/debug/tripletrail` of the workspace by default).

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
  server = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network], { stdio: ["ignore", "pipe", "pipe"] });
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

for (const width of [1280, 1440, 1600, 1920]) {
  test(`every top-bar tab can be reached at ${width} px`, async (t) => {
    const context = await browser.newContext({ viewport: { width, height: 900 } });
    await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
    await context.route("**/*", (route) =>
      route.request().url().startsWith(base) ? route.continue() : route.abort());
    const page = await context.newPage();
    t.after(() => context.close());

    await page.goto(base + "/$/ui/datasets");
    const tabs = page.locator(".topbar .tabs a");
    await tabs.first().waitFor();
    const names = await tabs.allTextContents();
    assert.ok(names.length >= 8, `the top bar has its tabs (${names.join(", ")})`);
    // Each tab, scrolled into view as a click would, is the element at its own centre: not the
    // search box, nor anything else, and inside the window.
    const hidden = await page.evaluate(() => [...document.querySelectorAll(".topbar .tabs a")].flatMap((a) => {
      a.scrollIntoView({ block: "nearest", inline: "nearest" });
      const r = a.getBoundingClientRect();
      const x = r.left + r.width / 2, y = r.top + r.height / 2;
      const hit = document.elementFromPoint(x, y);
      const inside = r.left >= 0 && r.right <= window.innerWidth + 0.5;
      return a.contains(hit) && inside ? [] : [`${a.textContent.trim()} (under ${hit?.tagName.toLowerCase()}${hit?.className ? "." + hit.className : ""})`];
    }));
    assert.deepEqual(hidden, [], `tabs that can't be clicked at ${width} px`);
    // Nor does the bar widen the page.
    assert.ok(await page.evaluate(() => document.documentElement.scrollWidth <= window.innerWidth), "no sideways scroll of the page");
  });
}

test("tabs that overflow follow the wheel and keep the current tab in view", async (t) => {
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await context.route("**/*", (route) =>
    route.request().url().startsWith(base) ? route.continue() : route.abort());
  const page = await context.newPage();
  t.after(() => context.close());

  await page.goto(base + "/$/ui/server");
  const current = page.locator('.topbar .tabs a[aria-current="page"]');
  await current.waitFor();
  assert.equal((await current.textContent()).trim(), "Server");
  const box = await page.locator(".topbar .tabs").boundingBox();
  const r = await current.boundingBox();
  assert.ok(r.x >= box.x && r.x + r.width <= box.x + box.width + 0.5, "the current tab is in view on its page");

  await page.goto(base + "/$/ui/datasets");
  await page.locator('.topbar .tabs a[aria-current="page"]').waitFor();
  const tabs = page.locator(".topbar .tabs");
  assert.equal(await tabs.evaluate((e) => e.scrollLeft), 0);
  await tabs.hover();
  await page.mouse.wheel(0, 400);
  await page.waitForFunction(() => document.querySelector(".topbar .tabs").scrollLeft > 0);
});
