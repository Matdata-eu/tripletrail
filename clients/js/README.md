# TripleTrail JavaScript client

A client for [TripleTrail](../../README.md) in browsers and Node 18 and later: SPARQL queries and
updates, the Graph Store Protocol and the admin API, over `fetch`, with no dependencies and
TypeScript types.

```js
import { TripleTrail, toValues } from "./src/index.js";

const rail = new TripleTrail("http://localhost:7878", { dataset: "rail" });
const result = await rail.select("SELECT ?s ?label { ?s <http://www.w3.org/2000/01/rdf-schema#label> ?label }");
console.log(toValues(result));
```

The calls are listed in [Using TripleTrail as a library](../../docs/library.md#javascript-client),
and chapter 10 of the [user guide](../../docs/guide/10-maps-and-apis.md#from-javascript) has an
example. Releases publish it to npm: `npm install @matdata/tripletrail-client`, then
`import { TripleTrail, toValues } from "@matdata/tripletrail-client"`.

Tests: `npm test` (or `node --test`) starts `tripletrail serve` from `target/debug/tripletrail`
(build it with `cargo build -p tripletrail-server`, or set `TRIPLETRAIL_BIN`) and runs against it.
