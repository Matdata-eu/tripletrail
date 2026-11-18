// Renders the PNG files and favicon.ico in brand/ from the SVGs that build.py writes.
// Needs Playwright with Chromium, installed locally or globally:
//   NODE_PATH=$(npm root -g) node brand/render.mjs
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createRequire } from "node:module";

// require() also finds a globally installed Playwright through NODE_PATH, which import does not.
const { chromium } = createRequire(import.meta.url)("playwright");

const here = dirname(fileURLToPath(import.meta.url));
const svg = (name) => readFileSync(join(here, name), "utf8");

// [output, source svg, width, height, background]
const jobs = [
  ["png/tripletrail-mark-16.png", "tripletrail-mark-small.svg", 16, 16],
  ["png/tripletrail-mark-32.png", "tripletrail-mark-small.svg", 32, 32],
  ["png/tripletrail-mark-48.png", "tripletrail-mark.svg", 48, 48],
  // apple-touch-icon: iOS rounds the corners itself and fills transparent ones with black, so square.
  ["png/tripletrail-mark-180.png", "tripletrail-mark.svg", 180, 180, "#0563bb"],
  ["png/tripletrail-mark-192.png", "tripletrail-mark.svg", 192, 192],
  ["png/tripletrail-mark-512.png", "tripletrail-mark.svg", 512, 512],
  ["png/tripletrail-logo-800.png", "tripletrail-logo.svg", 800, 0],
  ["png/tripletrail-logo-dark-800.png", "tripletrail-logo-dark.svg", 800, 0, "#070b12"],
  ["png/tripletrail-social-1200x630.png", null, 1200, 630],
];

// The card that link previews show (Open Graph): the light logo centred on the site's light ground.
const social = () => {
  const logo = svg("tripletrail-logo.svg").replace(/ width="\d+" height="\d+"/, ' width="760"');
  return `<div style="width:1200px;height:630px;display:grid;place-items:center;background:#f5f8fc">${logo}</div>`;
};

const browser = await chromium.launch();
const page = await browser.newPage({ deviceScaleFactor: 1 });
const shots = {};
for (const [out, src, w, h, bg] of jobs) {
  let html = src ? svg(src) : social();
  if (src) {
    const [, vw, vh] = html.match(/viewBox="0 0 ([\d.]+) ([\d.]+)"/).map(Number);
    const height = h || Math.round((w * vh) / vw);
    html = html.replace(/ width="[\d.]+" height="[\d.]+"/, ` width="${w}" height="${height}"`);
    html = `<div style="width:${w}px;height:${height}px;${bg ? `background:${bg}` : ""}">${html}</div>`;
  }
  await page.setContent(`<style>html,body{margin:0;background:transparent}svg{display:block}</style>${html}`);
  const png = await page.locator("div").first().screenshot({ omitBackground: !bg });
  writeFileSync(join(here, out), png);
  shots[out] = png;
  console.log(out, png.length, "bytes");
}
await browser.close();

// favicon.ico holds PNG images (allowed since Windows Vista; every current browser reads them).
const icons = ["png/tripletrail-mark-16.png", "png/tripletrail-mark-32.png", "png/tripletrail-mark-48.png"].map((k) => shots[k]);
const sizes = [16, 32, 48];
const head = Buffer.alloc(6 + 16 * icons.length);
head.writeUInt16LE(0, 0); head.writeUInt16LE(1, 2); head.writeUInt16LE(icons.length, 4);
let offset = head.length;
icons.forEach((png, i) => {
  const e = 6 + 16 * i;
  head.writeUInt8(sizes[i], e); head.writeUInt8(sizes[i], e + 1);
  head.writeUInt16LE(1, e + 4); head.writeUInt16LE(32, e + 6);
  head.writeUInt32LE(png.length, e + 8); head.writeUInt32LE(offset, e + 12);
  offset += png.length;
});
writeFileSync(join(here, "favicon.ico"), Buffer.concat([head, ...icons]));
console.log("favicon.ico", offset, "bytes");
