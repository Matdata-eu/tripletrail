// "Add data" (#275), in Chromium: one entry in a dataset's dialog for any file. RDF loads as
// it is; a CSV or JSON file opens the Import page with the file sent and its lift drafted in
// its own vocabulary, three clicks from the drop to a named graph, and "Save as lift query"
// puts the lift into a Workbench workspace whose pipeline lifts the file again. A file larger
// than the import keeps is drafted from its first lines. `TRIPLETRAIL_BIN` names the
// `tripletrail` binary (`target/debug/tripletrail` of the workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
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

const CSV = "id,name,km,opened\nBE1,Brussels,0.0,1835-05-05\nBE2,Leuven,30,1837-09-22\nBE3,Aalst,,\n";
const JSON_FILE = JSON.stringify({ network: "rail", stations: [{ id: "BE1", tracks: [{ no: 1 }, { no: 2 }] }] });
const TTL = "<https://example.org/a> <https://example.org/p> 1 .\n";

let browser;

async function start(args, env = {}) {
  const child = spawn(bin, ["serve", "--bind", "127.0.0.1:0", "--dataset", "ds", ...args], {
    stdio: ["ignore", "pipe", "pipe"],
    env: { ...process.env, ...env },
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
  browser = await chromium.launch();
});

after(async () => {
  await browser?.close();
});

async function open(t, base) {
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

async function ask(base, query) {
  const r = await fetch(base + "/ds/query?query=" + encodeURIComponent(query), { headers: { Accept: "text/csv" } });
  return (await r.text()).trim().split("\r\n");
}

const file = (name, text, mimeType = "text/plain") => ({ name, mimeType, buffer: Buffer.from(text) });

test("files go from the drop to a named graph, and the lift into a workspace", async (t) => {
  // A folder of its own for the workspace, removed after: a killed server leaves its
  // temporary one behind.
  const workspaces = mkdtempSync(join(tmpdir(), "ui-add-data-"));
  const { child, address: base } = await start(["--workspace-dir", workspaces]);
  t.after(async () => {
    child.kill();
    await new Promise((r) => (child.exitCode === null ? child.on("exit", r) : r()));
    rmSync(workspaces, { recursive: true, force: true });
  });
  const page = await open(t, base);
  await page.goto(base + "/$/ui/datasets/ds");
  await page.locator("#ds-upload").waitFor();
  // One drop: RDF loads as it is, the others are lifted.
  await page.setInputFiles("#upload-file", [
    file("stations.csv", CSV, "text/csv"),
    file("network.json", JSON_FILE, "application/json"),
    file("more.ttl", TTL, "text/turtle"),
  ]);
  const choices = page.locator("#upload-files select");
  await choices.nth(2).waitFor();
  assert.deepEqual(await choices.evaluateAll((s) => s.map((x) => x.value)), ["lift", "lift", "rdf"]);
  await page.click("#upload-btn");

  // The Import page, with the CSV sent and drafted in its own vocabulary.
  await page.waitForURL(/\/\$\/ui\/import\/ds$/);
  const query = page.locator("#imp-query");
  await query.waitFor();
  assert.match(await query.inputValue(), /\?stations a src:Stations/);
  assert.equal(await page.locator("#imp-ns").inputValue(), "https://data.example.org/src/stations/");
  assert.equal(await page.locator("#imp-kinds select").inputValue(), "id");
  assert.deepEqual(await ask(base, "SELECT (COUNT(*) AS ?n) { ?s ?p ?o }"), ["n", "1"], "the Turtle file loaded");

  await page.click("#imp-run");
  await page.locator("#imp-out", { hasText: "Added to urn:import:stations" }).waitFor();
  assert.deepEqual(
    await ask(base, "SELECT ?s ?km { GRAPH <urn:import:stations> { ?s <https://data.example.org/src/stations/km> ?km } } ORDER BY ?s"),
    ["s,km", "https://data.example.org/src/stations/stations/BE1,0.0", "https://data.example.org/src/stations/stations/BE2,30"],
  );
  assert.deepEqual(
    await ask(base, "SELECT (DATATYPE(?d) AS ?t) { GRAPH <urn:import:stations> { ?s <https://data.example.org/src/stations/opened> ?d } } LIMIT 1"),
    ["t", "http://www.w3.org/2001/XMLSchema#date"],
  );

  // Drafted again with IRIs from the row number.
  await page.selectOption("#imp-kinds select", "");
  await page.click("#imp-redraft");
  await page.waitForFunction(() => document.querySelector("#imp-query").value.includes("STRAFTER(STR("));

  // Saved as a lift query: the query, the file beside it and a lift step in a new workspace.
  await page.click("#imp-save");
  await page.locator("#imp-ws").waitFor();
  await page.click("#imp-ws-save");
  await page.locator("#imp-ws-msg", { hasText: "Saved." }).waitFor();
  const list = (await (await fetch(base + "/$/workspaces")).json()).workspaces;
  const ws = list.find((w) => w.name === "lift stations.csv");
  assert.ok(ws, JSON.stringify(list));
  const files = base + "/$/workspaces/" + ws.id + "/files/";
  assert.equal(await (await fetch(files + "lift/stations.csv")).text(), CSV);
  assert.match(await (await fetch(files + "lift/stations.rq")).text(), /location=lift\/stations\.csv/);
  const yaml = await (await fetch(files + "pipeline.yaml")).text();
  assert.match(yaml, /^steps:\n {2}- name: lift stations\.csv\n {4}lift: lift\/stations\.rq\n {4}graph: urn:import:stations\n {4}clear: true\n/);
  const run = await (await fetch(base + "/$/workspaces/" + ws.id + "/pipeline", { method: "POST", headers: { "Content-Type": "application/json" }, body: "{}" })).json();
  const step = run.steps[0];
  assert.equal(step.report?.status, "ok", JSON.stringify(step));
  assert.ok(step.report.quadsAfter >= 15, JSON.stringify(step));

  // The next file handed over: the JSON one, a class per kind of record.
  await page.click("#imp-run");
  await page.locator("#imp-next", { hasText: "Next file: network.json" }).waitFor();
  await page.click("#imp-next");
  await page.waitForFunction(() => document.querySelector("#imp-query")?.value.includes("src:Tracks"));
  const kinds = await page.locator("#imp-kinds select").evaluateAll((s) => s.map((x) => x.dataset.kind));
  assert.deepEqual(kinds, ["network", "stations", "tracks"]);
});

test("a file larger than the import keeps is drafted from its first lines", async (t) => {
  const { child, address: base } = await start(["--max-body", "4096"]);
  t.after(() => child.kill());
  const page = await open(t, base);
  await page.goto(base + "/$/ui/import/ds");
  await page.locator("#imp-file").waitFor();
  let big = "id,name\n";
  for (let i = 0; big.length < 10000; i++) big += `S${i},Station ${i}\n`;
  await page.setInputFiles("#imp-file", file("big.csv", big, "text/csv"));
  await page.locator("#imp-sample").waitFor();
  assert.match(await page.locator("#imp-sample").innerText(), /more than the import keeps/);
  assert.equal(await page.locator("#imp-run").isDisabled(), true);
  assert.match(await page.locator("#imp-query").inputValue(), /src:id/);
  // A large file of another kind: what to do instead.
  await page.setInputFiles("#imp-file", file("big.json", JSON.stringify({ a: "x".repeat(10000) }), "application/json"));
  await page.locator("#imp-work", { hasText: "tripletrail lift" }).waitFor();
});

// A stand-in for xsd2rdf (#276): writes the files the tool writes.
const FAKE_XSD2RDF = `#!/bin/sh
file="$2"; out="$4"; ns="$6"; base=$(basename "$file")
echo "<\${ns}Station> a <http://www.w3.org/2002/07/owl#Class> ." > "$out/$base.owl.ttl"
echo "<\${ns}shapes/Station> a <http://www.w3.org/ns/shacl#NodeShape> ; <http://www.w3.org/ns/shacl#targetClass> <\${ns}Station> ." > "$out/$base.shape.ttl"
echo "<\${ns}concepts/Kind> a <http://www.w3.org/2004/02/skos/core#ConceptScheme> ." > "$out/$base.skos.Kind.ttl"
echo "<\${ns}shapes/Station> <http://purl.org/dc/terms/source> 'station' ." > "$out/$base.provenance.ttl"
`;

test("an XML Schema becomes an ontology, shapes and code lists in the dataset", async (t) => {
  const tools = mkdtempSync(join(tmpdir(), "ui-schema-tools-"));
  writeFileSync(join(tools, "xsd2rdf"), FAKE_XSD2RDF);
  chmodSync(join(tools, "xsd2rdf"), 0o755);
  const { child, address: base } = await start(["--schema-tools", "--no-workspaces"], { PATH: tools + ":" + process.env.PATH });
  t.after(async () => {
    child.kill();
    await new Promise((r) => (child.exitCode === null ? child.on("exit", r) : r()));
    rmSync(tools, { recursive: true, force: true });
  });
  const page = await open(t, base);
  await page.goto(base + "/$/ui/datasets/ds");
  await page.locator("#ds-upload").waitFor();
  await page.setInputFiles("#upload-file", [file("stations.xsd", "<xs:schema xmlns:xs='http://www.w3.org/2001/XMLSchema'/>", "application/xml")]);
  await page.click("#upload-btn");
  await page.waitForURL(/\/\$\/ui\/import\/ds$/);
  await page.locator("#imp-schema-card", { hasText: "an XML Schema" }).waitFor();
  await page.click("#imp-schema-make");
  await page.locator("#imp-schema-made", { hasText: "1 ontology, 2 shape, 1 SKOS (1 concept schemes)" }).waitFor();
  assert.deepEqual(await ask(base, "SELECT (COUNT(*) AS ?n) { GRAPH ?g { ?s ?p ?o } }"), ["n", "0"], "nothing written yet");
  await page.click("#imp-schema-write");
  await page.locator("#imp-schema-graphs").waitFor();
  const ns = "https://data.example.org/src/stations/";
  assert.deepEqual(
    await ask(base, `SELECT ?c { GRAPH <${ns}schema/owl> { ?c a <http://www.w3.org/2002/07/owl#Class> } }`),
    ["c", ns + "Station"],
  );
  assert.deepEqual(
    await ask(base, `SELECT (COUNT(*) AS ?n) { GRAPH <${ns}schema/shapes> { ?s ?p ?o } }`),
    ["n", "2"],
  );
});
