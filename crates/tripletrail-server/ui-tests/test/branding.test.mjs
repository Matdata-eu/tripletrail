// Server branding as a viewer sees it (#262): the name, logo and colour of `--site-*` in the
// web UI. `TRIPLETRAIL_BIN` names the `tripletrail` binary (`target/debug/tripletrail` of the
// workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, writeFileSync, rmSync } from "node:fs";
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
  dir = mkdtempSync(join(tmpdir(), "tripletrail-ui-branding-"));
  const logo = join(dir, "logo.svg");
  writeFileSync(logo, '<svg xmlns="http://www.w3.org/2000/svg" width="96" height="24"><rect width="96" height="24" fill="#00634f"/></svg>');
  server = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--load", network,
    "--site-name", "Rail KG", "--site-logo", logo, "--site-color", "#00634f",
    "--banner", "Demo: data is reset nightly", "--banner-level", "warning",
    "--site-link", "Privacy|https://example.org/privacy", "--support-url", "https://example.org/help",
    "--contact-email", "data@example.org"], { stdio: ["ignore", "pipe", "pipe"] });
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
  if (dir) rmSync(dir, { recursive: true, force: true });
});

test("the pages carry the server's name, logo and colour", async (t) => {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await context.route("**/*", (route) =>
    route.request().url().startsWith(base) ? route.continue() : route.abort());
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  t.after(() => context.close());

  await page.goto(base + "/$/ui/datasets");
  await page.waitForFunction(() => document.title === "Datasets · Rail KG");
  assert.equal((await page.locator(".topbar .brand").textContent()).trim(), "Rail KG");
  const logo = page.locator(".topbar .logo-img");
  await logo.waitFor();
  assert.ok(await logo.evaluate((img) => img.complete && img.naturalWidth === 96), "the logo loads");
  // The accent colour reaches the app's and MatGUI's variables.
  const brand = await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--brand-fill").trim());
  assert.equal(brand, "#00634f");

  await page.locator(".tabs a[data-page=query]").click();
  await page.waitForFunction(() => document.title === "Query · Rail KG");
  assert.deepEqual(errors, [], "errors on the page");
});

test("the banner can be dismissed for the session, and the footer and About name the operator", async (t) => {
  const context = await browser.newContext({ viewport: { width: 1400, height: 900 } });
  await context.addInitScript(() => localStorage.setItem("tripletrail-tour", "9999"));
  await context.route("**/*", (route) =>
    route.request().url().startsWith(base) ? route.continue() : route.abort());
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  t.after(() => context.close());

  await page.goto(base + "/$/ui/map");
  const banner = page.locator("#site-banner");
  await banner.waitFor();
  assert.equal((await banner.textContent()).replace("×", "").trim(), "Demo: data is reset nightly");
  // Full-height pages make room for it.
  assert.equal(await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--banner-h").trim()), "33px");
  await page.locator(".site-banner-close").click();
  assert.ok(await banner.isHidden());
  await page.reload();
  await page.locator("#main").waitFor();
  assert.ok(await page.locator("#site-banner").isHidden(), "still dismissed after a reload");
  assert.equal(await page.locator(".site-footer a").getAttribute("href"), "https://example.org/privacy");

  // About points feedback to the operator's help and e-mail.
  await page.locator("#about-btn").click();
  const about = page.locator("dialog.about-dialog");
  await about.waitFor({ timeout: 5000 });
  assert.ok(await about.locator("a[href='https://example.org/help']").count() === 1);
  assert.ok(await about.locator("a[href='mailto:data@example.org']").count() === 1);
  assert.deepEqual(errors, [], "errors on the page");
});
