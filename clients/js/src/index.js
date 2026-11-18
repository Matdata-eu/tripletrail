// TripleTrail client for browsers and Node 18+ (#223): SPARQL queries and updates, the Graph
// Store Protocol and the admin API, over `fetch`. No dependencies.

/** An answer the server refused: `status` is the HTTP status, `body` its text. */
export class TripleTrailError extends Error {
  constructor(message, status, body) {
    super(message);
    this.name = "TripleTrailError";
    this.status = status;
    this.body = body;
  }
}

const XSD = "http://www.w3.org/2001/XMLSchema#";
const INTEGERS = new Set(
  [
    "integer", "int", "long", "short", "byte", "nonNegativeInteger", "positiveInteger",
    "nonPositiveInteger", "negativeInteger", "unsignedLong", "unsignedInt", "unsignedShort",
    "unsignedByte",
  ].map((t) => XSD + t),
);

/**
 * A SPARQL JSON term as a plain JavaScript value: a number for a numeric literal (a `bigint`
 * for an integer too large for a number), a boolean for `xsd:boolean`, the IRI, label or text
 * otherwise. `undefined` for an unbound variable.
 */
export function toValue(term) {
  if (!term) return undefined;
  if (term.type !== "literal" || !term.datatype) return term.value;
  const d = term.datatype;
  if (d === XSD + "boolean") {
    if (["true", "1"].includes(term.value)) return true;
    if (["false", "0"].includes(term.value)) return false;
    return term.value;
  }
  const v = term.value.trim();
  // An ill-typed literal ("abc"^^xsd:integer) stays text.
  if (INTEGERS.has(d)) {
    if (!/^[+-]?\d+$/.test(v)) return term.value;
    const n = Number(v);
    return Number.isSafeInteger(n) ? n : BigInt(v.replace(/^\+/, ""));
  }
  if (d === XSD + "decimal" || d === XSD + "double" || d === XSD + "float") {
    if (v === "INF") return Infinity;
    if (v === "-INF") return -Infinity;
    if (v === "NaN") return NaN;
    const n = Number(v);
    return v === "" || Number.isNaN(n) ? term.value : n;
  }
  return term.value;
}

/** The rows of a `select` answer with every term as [`toValue`] gives it. */
export function toValues(result) {
  return result.rows.map((row) =>
    Object.fromEntries(Object.entries(row).map(([k, t]) => [k, toValue(t)])),
  );
}

function join(base, path) {
  return base.replace(/\/+$/, "") + "/" + path.replace(/^\/+/, "");
}

/**
 * A TripleTrail server, or one dataset of it.
 *
 * ```js
 * const rail = new TripleTrail("http://localhost:7878", { dataset: "rail", token });
 * const { rows } = await rail.select("SELECT ?s { ?s a <urn:Station> }");
 * ```
 */
export class TripleTrail {
  /**
   * @param {string} url the server, such as `http://localhost:7878`
   * @param {object} [options]
   * @param {string} [options.dataset] the dataset of queries, updates and graphs (`ds`)
   * @param {string} [options.token] sent as `Authorization: Bearer` (`--admin-token`,
   *   `--update-token`, or an API key)
   * @param {string} [options.apiKey] sent as `X-API-Key`
   * @param {Record<string, string>} [options.headers] sent with every request
   * @param {typeof fetch} [options.fetch] the `fetch` to use (the global one by default)
   */
  constructor(url, options = {}) {
    this.url = url;
    this.datasetName = options.dataset ?? "ds";
    this.options = options;
    this.graphs = new GraphStore(this);
  }

  /** The same server, for another dataset. */
  dataset(name) {
    return new TripleTrail(this.url, { ...this.options, dataset: name });
  }

  /** A request to the server; answers the response, or throws a [`TripleTrailError`]. */
  async request(method, path, { params, headers, body, signal } = {}) {
    const url = new URL(join(this.url, path));
    for (const [k, v] of Object.entries(params ?? {})) {
      if (v === undefined || v === null) continue;
      for (const one of Array.isArray(v) ? v : [v]) url.searchParams.append(k, String(one));
    }
    const all = { ...(this.options.headers ?? {}), ...(headers ?? {}) };
    if (this.options.token) all.Authorization = `Bearer ${this.options.token}`;
    if (this.options.apiKey) all["X-API-Key"] = this.options.apiKey;
    const f = this.options.fetch ?? globalThis.fetch;
    const response = await f(url, { method, headers: all, body, signal });
    if (!response.ok) {
      const text = await response.text().catch(() => "");
      const first = text.split("\n").find((l) => l.trim()) ?? "";
      throw new TripleTrailError(
        `${method} ${url.pathname}: ${response.status} ${first}`.trim(),
        response.status,
        text,
      );
    }
    return response;
  }

  /**
   * A SPARQL query, answered as the raw response in the `accept` format.
   * @param {string} query
   * @param {QueryOptions} [options]
   */
  async query(query, options = {}) {
    return this.request("POST", `${encodeURIComponent(this.datasetName)}/query`, {
      params: {
        "default-graph-uri": options.defaultGraphs,
        "named-graph-uri": options.namedGraphs,
        timeout: options.timeout,
        "max-rows": options.maxRows,
        partial: options.partial ? "true" : undefined,
      },
      headers: {
        "Content-Type": "application/sparql-query",
        Accept: options.accept ?? "application/sparql-results+json",
      },
      body: query,
      signal: options.signal,
    });
  }

  /**
   * A `SELECT` query: its variables and rows, each row a map from variable to SPARQL JSON term
   * (`{type, value, datatype?, "xml:lang"?}`; [`toValues`] gives plain values). `incomplete`
   * is true when the server cut the answer at its row budget (`partial: true`).
   */
  async select(query, options = {}) {
    const response = await this.query(query, {
      ...options,
      accept: "application/sparql-results+json",
    });
    const json = await response.json();
    if (!json.results) throw new TripleTrailError("not a SELECT query", response.status, "");
    return {
      variables: json.head?.vars ?? [],
      rows: json.results.bindings,
      incomplete: response.headers.has("X-Result-Incomplete"),
    };
  }

  /** An `ASK` query. */
  async ask(query, options = {}) {
    const response = await this.query(query, {
      ...options,
      accept: "application/sparql-results+json",
    });
    const json = await response.json();
    if (typeof json.boolean !== "boolean") {
      throw new TripleTrailError("not an ASK query", response.status, "");
    }
    return json.boolean;
  }

  /**
   * A `CONSTRUCT` or `DESCRIBE` query, as RDF text (N-Triples unless `accept` names another
   * format, such as `text/turtle` or `application/ld+json`).
   */
  async construct(query, options = {}) {
    const response = await this.query(query, {
      ...options,
      accept: options.accept ?? "application/n-triples",
    });
    return response.text();
  }

  /**
   * A SPARQL update.
   * @param {string} update
   * @param {UpdateOptions} [options]
   */
  async update(update, options = {}) {
    await this.request("POST", `${encodeURIComponent(this.datasetName)}/update`, {
      params: {
        "using-graph-uri": options.usingGraphs,
        "using-named-graph-uri": options.usingNamedGraphs,
        timeout: options.timeout,
      },
      headers: { "Content-Type": "application/sparql-update" },
      body: update,
      signal: options.signal,
    });
  }

  /** The admin API (`/$/…`). */
  get admin() {
    return new Admin(this);
  }
}

/** A graph of the Graph Store Protocol: an IRI, or `null` for the default graph. */
function graphParams(graph) {
  return graph === null || graph === undefined || graph === "default"
    ? { default: "" }
    : { graph };
}

/** The Graph Store Protocol of a dataset (`/{dataset}/data`). */
export class GraphStore {
  constructor(client) {
    this.client = client;
  }

  path() {
    return `${encodeURIComponent(this.client.datasetName)}/data`;
  }

  /** A graph (`null` for the default graph) as RDF text, N-Triples unless `accept` says. */
  async get(graph, { accept = "application/n-triples", signal } = {}) {
    const response = await this.client.request("GET", this.path(), {
      params: graphParams(graph),
      headers: { Accept: accept },
      signal,
    });
    return response.text();
  }

  /** Replaces a graph with `data` (RDF text in `contentType`, Turtle by default). */
  async put(graph, data, { contentType = "text/turtle", signal } = {}) {
    await this.client.request("PUT", this.path(), {
      params: graphParams(graph),
      headers: { "Content-Type": contentType },
      body: data,
      signal,
    });
  }

  /** Adds `data` to a graph. */
  async post(graph, data, { contentType = "text/turtle", signal } = {}) {
    await this.client.request("POST", this.path(), {
      params: graphParams(graph),
      headers: { "Content-Type": contentType },
      body: data,
      signal,
    });
  }

  /** Removes a graph (empties the default graph). */
  async delete(graph, { signal } = {}) {
    await this.client.request("DELETE", this.path(), { params: graphParams(graph), signal });
  }
}

/** The admin API (`/$/…`): needs the admin token when the server sets one. */
export class Admin {
  constructor(client) {
    this.client = client;
  }

  async json(method, path, params) {
    const response = await this.client.request(method, `$/${path}`, {
      params,
      headers: { Accept: "application/json" },
    });
    const text = await response.text();
    return text ? JSON.parse(text) : null;
  }

  /** True when the server answers `/$/ping`. */
  async ping() {
    const response = await this.client.request("GET", "$/ping");
    return (await response.text()).trim() === "ok";
  }

  /** `/$/ready`: whether every dataset is open, per dataset (does not throw on 503). */
  async ready() {
    try {
      return { ready: true, ...(await this.json("GET", "ready")) };
    } catch (e) {
      if (e instanceof TripleTrailError && e.status === 503) {
        let detail = {};
        try {
          detail = JSON.parse(e.body);
        } catch {
          // Not JSON: no detail.
        }
        return { ...detail, ready: false };
      }
      throw e;
    }
  }

  /** `/$/server`: version, uptime, settings and datasets. */
  server() {
    return this.json("GET", "server");
  }

  /** `/$/datasets`: the datasets and aliases, as `datasets` (Fuseki's list). */
  async datasets() {
    const json = await this.json("GET", "datasets");
    return json.datasets ?? json;
  }

  /** `/$/datasets/{name}`: one dataset, with triple and graph counts. */
  dataset(name) {
    return this.json("GET", `datasets/${encodeURIComponent(name)}`);
  }

  /**
   * Creates a dataset: `type` `mem` (the default) or `disk`, or `location` and `mode` to
   * attach a directory, `aliasOf` for an alias, and the other parameters of `POST
   * /$/datasets` by their own names (`unionDefaultGraph`, `entailment`…).
   */
  async createDataset(name, { type, location, mode, aliasOf, ...rest } = {}) {
    await this.json("POST", "datasets", {
      name,
      type: type ?? (location || aliasOf ? undefined : "mem"),
      location,
      mode,
      "alias-of": aliasOf,
      ...rest,
    });
  }

  /** Detaches a dataset; `deleteFiles: true` also removes its directory. */
  async deleteDataset(name, { deleteFiles = false, force = false } = {}) {
    await this.json("DELETE", `datasets/${encodeURIComponent(name)}`, {
      "delete-files": deleteFiles ? "true" : undefined,
      force: force ? "true" : undefined,
    });
  }

  /** `/$/stats` or `/$/stats/{name}`: request and data counts. */
  stats(name) {
    return this.json("GET", name ? `stats/${encodeURIComponent(name)}` : "stats");
  }
}
