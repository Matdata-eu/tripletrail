# Security model

This page says what TripleTrail protects, from whom, where the boundaries are, what defends
each one, and what the operator has to do. Use it to check a change against, and to see what
is yours to set up. The options themselves are in [server.md](server.md); to report a
vulnerability, see [SECURITY.md](../SECURITY.md).

## What is protected

- **The data:**
  - the quads of each dataset, its files on disk, and its write-ahead log;
  - what can be inferred from answers: counts, timings, error messages, map tiles, search
    scores.
- **Credentials:**
  - API keys, `--update-token` and `--admin-token`;
  - user passwords (stored only as PBKDF2 or Shiro hashes);
  - single sign-on sessions (TripleTrail Enterprise);
  - database connection URLs (`--fx-connection`);
  - OpenTelemetry export headers.
- **AI keys:** the AI assistant's key (MatGUI, the Workbench), kept in the user's browser.
- **Backups:** `--backup-dir`, WAL archives (`--wal-archive`) and store copies. They hold the
  whole dataset, unencrypted.
- **Logs:** the request log, the slow query log, the audit log and traces. They can hold
  query text, and so data.

## Trust boundaries

| Boundary | What crosses it | Trusted side |
|---|---|---|
| Network | Every HTTP request to `serve` | The server process |
| Reverse proxy | TLS, and user and group headers (`trustedHeaders`) | The proxy, from the addresses in `trustedProxies` only |
| Browser | MatGUI, the Workbench, the HTML pages (describe, Linked Data, OGC API); the browser sends saved passwords and cookies with any page's request | The server's own pages, on its own origin |
| Model providers | The AI assistant's prompts and tool results, from the browser | The user, who picks the provider and holds the key |
| Remote SPARQL endpoints | `SERVICE` calls and their answers | Only the endpoints in `--service-allow` |
| Facade-X sources | Inline `content=`, and tables and SQL of the `--fx-connection` databases | The database's own permissions for that role |
| The data itself | Literals, IRIs and geometries, written by whoever may write, or loaded from elsewhere | Nothing: data is never trusted as code or markup |
| Disk | Dataset directories, backups, logs | The operating system's account and file permissions |

## Who attacks, and what stops them

### A caller on the network without credentials

- **Defaults:**
  - With no `--update-token`, `--admin-token` or `--auth-config`, there is no
    authentication.
  - The server binds to `127.0.0.1` by default. The Docker image binds `0.0.0.0` and so
    is open (#401 tracks refusing that).
- **With authentication on:**
  - Every request needs an identity, except `/$/ping`, `/$/ready` and `/$/role/{name}`.
  - Unknown keys and wrong passwords get 401, never anonymous access.
  - After 10 failures in a minute, an address gets 429.
  - Keys are compared by their SHA-256 hash in constant time.
  - Passwords are checked against slow hashes.
- **Limits:** `--timeout`, `--max-rows`, the per-dataset and per-caller limits, and the
  `SERVICE` and Facade-X size caps bound what one request can cost.

### A caller who may do less than everything

- **Groups:** a reader may not write (`tripletrail:writer`) or administer
  (`tripletrail:admin`).
- **Datasets:** a key or user limited to some datasets gets 403 on the others, also through
  their Linked Data IRIs, and sees no `/$/` statistics.
- **Graphs and rows:** a caller limited to some graphs, or under row rules (TripleTrail
  Enterprise), gets answers as if the store held only what they may read. This applies to
  queries, full-text search, map tiles, OGC API, MCP tools, counts and scores.
  - `tests/leaks.rs` checks this against a second server holding only the readable data.
  - Such a caller may not call `SERVICE`.
- **Facade-X databases are not filtered by dataset policies.** Any caller who may query a
  dataset may read what the `--fx-connection` role can read.

### Another web site, in the user's browser

- **Cross-site changes:**
  - A page of another site cannot change data, whatever the user signed in with. A change
    needs `Sec-Fetch-Site: same-origin`, an `Origin` of the server itself or of `--cors`,
    or neither header (not a browser).
  - With `--cors` for every origin, form bodies are still refused.
- **DNS rebinding:** without `--auth-config`, the server answers only its own host names
  (IP addresses, `localhost`, `--allowed-host`). This stops a page from pointing its own
  name at the server and reading the answers. `/mcp` also checks `Origin`.
- **Reading answers:** CORS answers only the origins of `--cors`, and `--cors` alone answers
  all of them.
- **The server's own pages:**
  - MatGUI and the API pages carry a `Content-Security-Policy` with
    `frame-ancestors 'self'` and `X-Content-Type-Options: nosniff`.
  - The other HTML pages escape every value; #404 gives them the same headers.

### Malicious data in the store

- **No code from data:**
  - Data is shown, never run.
  - HTML pages escape literals and IRIs.
  - The describe page links IRIs only for `http` and `https`.
  - Facade-X sources are read with size limits (YAML aliases, spreadsheet cells and bytes).
    A query may lower these limits but not raise them.
- **No files or URLs:**
  - Facade-X on a server reads no files and no URLs (`location=` is refused).
  - SPARQL `LOAD` is not supported.
- **Bounded queries:** a query built from data, such as a `SERVICE ?endpoint` bound by a
  triple, still only reaches allowed endpoints.

### A malicious remote endpoint or database

- **`SERVICE` calls:**
  - There are no remote endpoints until `--service-allow` lists them.
  - Endpoints are matched by scheme, host, port and a path prefix on a `/` boundary.
  - Only `http` and `https` are called, endpoints with user info never, and redirects
    are not followed.
  - Answers are capped (`--service-max-bytes`, `--service-timeout`) and parsed as results,
    never as queries.
- **Facade-X databases:**
  - Calls run in a read-only transaction.
  - `--fx-timeout` sets the statement timeout, and TripleTrail cancels the query at its
    end.
  - Rows and bytes are capped.
  - Each session is reset (`DISCARD ALL`) after a call.

### The AI assistant's provider

- **Where the key lives:** the browser holds the key, and calls the provider directly. The
  server never sees the key or the prompts.
- **What the model can do:**
  - It reads through the server with the user's own access. In the Workbench it runs
    queries only when the user turns on **may query the data**.
  - The Workbench assistant's tools write nothing: what it proposes, the user reviews and
    saves.
- **What the provider sees:** whatever the user lets the assistant send, including query
  answers.

## The operator's part

TripleTrail cannot do these for you:

- **Authentication** before the port is reachable by anyone who should not write:
  `--auth-config`, or at least `--update-token` and `--admin-token`.
- **TLS:** HTTPS from a reverse proxy or the server itself. Passwords and keys travel with
  every request.
- **Host names:** `--allowed-host` with the names the server is reached by, and `--cors`
  with exact origins only.
- **The proxy:**
  - `trustedProxies` set to your proxy's addresses only.
  - The proxy strips user and group headers that clients send.
- **Least privilege:**
  - A `--fx-connection` role with `SELECT` on what callers may see, and nothing more.
  - `--service-allow` with only the endpoints you need.
- **Files:**
  - The dataset directories, `--backup-dir`, `--wal-archive` and logs readable only by
    the server's account.
  - Backups kept encrypted and away from the server.
- **Logs:**
  - The slow query log (`--slow-query-ms`) and `--audit-reads` record query text. Treat
    them as data.
  - The request log and traces record only a hash of it.
- **Updates:** take new releases. Before 1.0, only the latest release gets security fixes
  ([SECURITY.md](../SECURITY.md)).

## Checking a change

A pull request that adds an endpoint, an outbound request, an option naming a file or
directory, or HTML built from data answers the security checklist in
[the pull request template](../.github/pull_request_template.md):

- **Leaks:** an entry in `tests/leaks.rs` for anything that reads the data.
- **Auth group:** the group or dataset access the endpoint needs.
- **Cross-site requests:** changes go through the `Origin` and `Sec-Fetch-Site` check.
- **Outbound requests:** they go through an allowlist and set no redirects, user info or
  unbounded answers.
- **Logs:** they record no secrets, and no query text outside the slow query and audit logs.
- **HTML:** pages escape every value and carry the CSP and `nosniff`.
