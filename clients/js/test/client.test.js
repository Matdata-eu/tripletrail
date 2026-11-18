// The client against a real server: `TRIPLETRAIL_BIN` names the `tripletrail` binary
// (`target/debug/tripletrail` of the workspace by default).

import { test, before, after } from "node:test";
import assert from "node:assert/strict";
import { spawn, execFile } from "node:child_process";
import { promisify } from "node:util";
import { mkdtempSync, rmSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { TripleTrail, TripleTrailError, toValue, toValues } from "../src/index.js";

const here = dirname(fileURLToPath(import.meta.url));
const workspace = join(here, "..", "..", "..");
const bin =
  process.env.TRIPLETRAIL_BIN ??
  join(resolve(workspace, process.env.CARGO_TARGET_DIR ?? "target"), "debug", "tripletrail");

const run = promisify(execFile);
const ADMIN = "admin-secret";
const WRITER = "writer-secret";
let server;
let dir;
let url;

/** Starts `tripletrail serve` on a free port, and waits until it answers. */
async function start(args) {
  const child = spawn(bin, ["serve", "--bind", "127.0.0.1:0", ...args], {
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
  const client = new TripleTrail(address);
  for (let i = 0; i < 100; i++) {
    if ((await client.admin.ready()).ready) break;
    await new Promise((r) => setTimeout(r, 50));
  }
  return { child, address };
}

before(async () => {
  assert.ok(existsSync(bin), `no tripletrail binary at ${bin}: cargo build -p tripletrail-server`);
  dir = mkdtempSync(join(tmpdir(), "tripletrail-js-"));
  ({ child: server, address: url } = await start([
    "--dataset",
    "rail",
    "--admin-token",
    ADMIN,
    "--update-token",
    WRITER,
    "--max-rows",
    "1000",
  ]));
});

after(() => {
  server?.kill();
  if (dir) rmSync(dir, { recursive: true, force: true });
});

const PREFIX = "PREFIX : <https://example.org/rail#> ";

test("updates write, and queries answer rows, booleans and RDF", async () => {
  const rail = new TripleTrail(url, { dataset: "rail", token: WRITER });
  await rail.update(
    PREFIX +
      `INSERT DATA {
        :BXC a :Station ; :name "Bruxelles-Central"@fr ; :tracks 6 ; :open true ; :km 0.5 .
        :ANT a :Station ; :name "Antwerpen-Centraal"@nl ; :tracks 14 ; :open false .
        GRAPH :plan { :BXC :next :ANT } }`,
  );

  const result = await rail.select(
    PREFIX + "SELECT ?s ?name ?tracks ?open ?km { ?s a :Station ; :name ?name ; :tracks ?tracks ; :open ?open OPTIONAL { ?s :km ?km } } ORDER BY ?tracks",
  );
  assert.deepEqual(result.variables, ["s", "name", "tracks", "open", "km"]);
  assert.equal(result.rows.length, 2);
  assert.deepEqual(result.rows[0].name, {
    type: "literal",
    value: "Bruxelles-Central",
    "xml:lang": "fr",
  });
  assert.equal(result.incomplete, false);
  assert.deepEqual(toValues(result), [
    { s: "https://example.org/rail#BXC", name: "Bruxelles-Central", tracks: 6, open: true, km: 0.5 },
    { s: "https://example.org/rail#ANT", name: "Antwerpen-Centraal", tracks: 14, open: false },
  ]);

  assert.equal(await rail.ask(PREFIX + "ASK { :BXC :next :ANT }"), false);
  assert.equal(await rail.ask(PREFIX + "ASK { GRAPH :plan { :BXC :next :ANT } }"), true);
  // The dataset of a query: the plan graph as the default graph.
  assert.equal(
    await rail.ask(PREFIX + "ASK { :BXC :next :ANT }", {
      defaultGraphs: ["https://example.org/rail#plan"],
    }),
    true,
  );

  const nt = await rail.construct(PREFIX + "CONSTRUCT { ?s :n ?t } WHERE { ?s :tracks ?t }");
  assert.equal(nt.trim().split("\n").length, 2);
  assert.match(nt, /<https:\/\/example.org\/rail#ANT> <https:\/\/example.org\/rail#n> "14"/);
  const turtle = await rail.construct(PREFIX + "DESCRIBE :ANT", { accept: "text/turtle" });
  assert.match(turtle, /Antwerpen-Centraal/);

  // USING from the protocol: the update reads the plan graph.
  await rail.update(PREFIX + "INSERT { ?b :after ?a } WHERE { ?a :next ?b }", {
    usingGraphs: ["https://example.org/rail#plan"],
  });
  assert.equal(await rail.ask(PREFIX + "ASK { :ANT :after :BXC }"), true);
});

test("the row budget refuses a larger answer, or cuts it with partial", async () => {
  const rail = new TripleTrail(url, { dataset: "rail" });
  const q = "SELECT ?n { VALUES ?n { 1 2 3 4 5 } }";
  await assert.rejects(rail.select(q, { maxRows: 3 }), (e) => e instanceof TripleTrailError && e.status === 413);
  const cut = await rail.select(q, { maxRows: 3, partial: true });
  assert.equal(cut.rows.length, 3);
  assert.equal(cut.incomplete, true);
});

test("errors carry the status and the server's message", async () => {
  const rail = new TripleTrail(url, { dataset: "rail" });
  await assert.rejects(rail.select("SELECT ?s {"), (e) => {
    assert.ok(e instanceof TripleTrailError);
    assert.equal(e.status, 400);
    assert.ok(e.body.length > 0);
    return true;
  });
  // Writes need the update token.
  await assert.rejects(rail.update("INSERT DATA { <urn:a> <urn:b> <urn:c> }"), (e) => e.status === 401 || e.status === 403);
  // An ASK helper for a SELECT query says so.
  await assert.rejects(rail.ask("SELECT * {}"), /not an ASK query/);
});

test("the Graph Store Protocol reads, replaces, adds and removes graphs", async () => {
  const rail = new TripleTrail(url, { dataset: "rail", token: WRITER });
  const g = "https://example.org/rail#lines";
  await rail.graphs.put(g, "@prefix : <https://example.org/rail#> . :L1 :from :BXC .");
  await rail.graphs.post(g, "<https://example.org/rail#L1> <https://example.org/rail#to> <https://example.org/rail#ANT> .", {
    contentType: "application/n-triples",
  });
  const text = await rail.graphs.get(g);
  assert.equal(text.trim().split("\n").length, 2);
  await rail.graphs.put(g, "@prefix : <https://example.org/rail#> . :L2 :from :ANT .");
  assert.equal((await rail.graphs.get(g)).trim().split("\n").length, 1);
  await rail.graphs.delete(g);
  await assert.rejects(rail.graphs.get(g), (e) => e.status === 404);
  // The default graph.
  const all = await rail.graphs.get(null, { accept: "text/turtle" });
  assert.match(all, /Bruxelles-Central/);
});

test("the admin API lists, creates and removes datasets", async () => {
  const anonymous = new TripleTrail(url);
  assert.equal(await anonymous.admin.ping(), true);
  assert.equal((await anonymous.admin.ready()).ready, true);
  await assert.rejects(anonymous.admin.createDataset("scratch"), (e) => e.status === 401 || e.status === 403);

  const admin = new TripleTrail(url, { token: ADMIN });
  const server = await admin.admin.server();
  assert.ok(server.version, JSON.stringify(server));
  await admin.admin.createDataset("scratch");
  const names = (await admin.admin.datasets()).map((d) => d["ds.name"]);
  assert.ok(names.includes("/scratch"), JSON.stringify(names));

  const scratch = admin.dataset("scratch");
  await scratch.update("INSERT DATA { <urn:a> <urn:b> <urn:c> }");
  const rows = await scratch.select("SELECT (COUNT(*) AS ?n) { ?s ?p ?o }");
  assert.equal(toValue(rows.rows[0].n), 1);
  const info = await admin.admin.dataset("scratch");
  assert.ok(JSON.stringify(info).includes("scratch"));
  const stats = await admin.admin.stats("scratch");
  assert.ok(stats);

  await admin.admin.deleteDataset("scratch");
  await assert.rejects(scratch.select("SELECT * {}"), (e) => e.status === 404);
});

test("toValue reads numbers, booleans and large integers", () => {
  const xsd = (t) => `http://www.w3.org/2001/XMLSchema#${t}`;
  assert.equal(toValue({ type: "literal", value: "42", datatype: xsd("integer") }), 42);
  assert.equal(
    toValue({ type: "literal", value: "123456789012345678901234", datatype: xsd("integer") }),
    123456789012345678901234n,
  );
  assert.equal(toValue({ type: "literal", value: "1.5E2", datatype: xsd("double") }), 150);
  assert.equal(toValue({ type: "literal", value: "-INF", datatype: xsd("double") }), -Infinity);
  assert.equal(toValue({ type: "literal", value: "1", datatype: xsd("boolean") }), true);
  assert.equal(toValue({ type: "literal", value: "2026-11-14", datatype: xsd("date") }), "2026-11-14");
  assert.equal(toValue({ type: "bnode", value: "b0" }), "b0");
  // Ill-typed literals stay text instead of throwing.
  assert.equal(toValue({ type: "literal", value: "abc", datatype: xsd("integer") }), "abc");
  assert.equal(toValue({ type: "literal", value: "1.5", datatype: xsd("integer") }), "1.5");
  assert.equal(toValue({ type: "literal", value: "x", datatype: xsd("double") }), "x");
  assert.equal(toValue({ type: "literal", value: "yes", datatype: xsd("boolean") }), "yes");
  assert.equal(toValue(undefined), undefined);
});

test("the guide's example prints what the guide says", async () => {
  const chapter = readFileSync(join(workspace, "docs", "guide", "10-maps-and-apis.md"), "utf8");
  const block = (marker) => {
    const at = chapter.indexOf(marker);
    assert.ok(at >= 0, `no ${marker} in chapter 10`);
    const m = chapter.slice(at).match(/```\w*\n([\s\S]*?)```/);
    return m[1];
  };
  const { child, address } = await start([
    "--load",
    join(workspace, "docs", "guide", "data", "network.ttl"),
  ]);
  try {
    const script = block("<!-- test: file stations.mjs -->")
      .replace("./clients/js/src/index.js", new URL("../src/index.js", import.meta.url).href)
      .replace("http://localhost:7878", address);
    const file = join(dir, "stations.mjs");
    writeFileSync(file, script);
    const { stdout } = await run(process.execPath, [file]);
    assert.equal(stdout, block("<!-- output of stations.mjs"));
  } finally {
    child.kill();
  }
});
