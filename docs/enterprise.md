# TripleTrail Enterprise

TripleTrail is open core: the engine, the server and almost every feature are Apache-2.0. A small
set of features for larger organisations (central sign-on, row-level data governance, audit and
team process) is sold as TripleTrail Enterprise, with a subscription that includes updates and
support.

This page covers the build, the licence key and how customers get updates. It is for
Matdata and for customers' operators.

## Two builds from one tree

| | Open (Apache-2.0) | Enterprise |
|---|---|---|
| Build | `cargo build -p tripletrail-server` | `cargo build -p tripletrail-server --features enterprise` |
| Image | `ghcr.io/matdata-eu/tripletrail` | `docker build --build-arg EDITION=enterprise .`, published as `ghcr.io/matdata-eu/tripletrail-enterprise` |
| Binary | `tripletrail` | `tripletrail` (the same name, so scripts do not change) |
| `tripletrail --version` | `0.1.0` | `0.1.0 (enterprise)` |
| `GET /$/server` | `"edition": "open"` | `"edition": "enterprise"` |

The enterprise code lives in `crates/tripletrail-enterprise`, which is proprietary (see its
`LICENSE`), `publish = false`, and only compiled with the cargo feature `enterprise`. It is kept
in a private repository, which builds this repository's code with that crate added. The open release workflow and the open image build only the default
features.

The enterprise image is public: anyone can pull it, with no account or token.
`docker run ghcr.io/matdata-eu/tripletrail-enterprise` starts a server that checks its licence
at start (below). Without a licence that covers them, the enterprise features stay off and the
server works as the open edition, with the reason in the log.

## The licence key

A licence is one line of text:

```text
ttlic1.<payload>.<signature>
```

The payload is JSON (base64url) naming the licence id, the customer, the issue and expiry
dates, the enterprise features it unlocks (or `*` for all), and optionally the most nodes and
CPU cores it covers and a note. The signature is Ed25519 over the payload, checked against the
public keys built into the binary. **TripleTrail never contacts anything to check a licence**: it
works on closed networks.

### Giving TripleTrail the licence

```sh
tripletrail serve --license-file /etc/tripletrail/license.txt …
# or
TRIPLETRAIL_LICENSE_FILE=/etc/tripletrail/license.txt tripletrail serve …
TRIPLETRAIL_LICENSE="ttlic1.…" tripletrail serve …   # the text itself (wins over the file)
```

In Docker or Kubernetes, mount the licence as a secret file and set `TRIPLETRAIL_LICENSE_FILE`.

### Checking a licence

```sh
tripletrail license inspect license.txt
```

```text
Licence:   2026-001
Customer:  Example Rail AS
Issued:    2026-11-14
Expires:   2027-11-13
Features:  audit, sso
Max nodes: 3
Key:       95322a17
Status:    valid
```

It exits 0 when the licence unlocks the enterprise features and 1 otherwise; `--json` prints
the same JSON as `GET /$/license` on a running server. That endpoint is part of the admin API
and is open to the same callers as `GET /$/server`: with access policies on, only
`tripletrail:admin` and `tripletrail:monitor`.

### What happens when a licence expires

A licence never stops the server.

| When | What happens |
|---|---|
| More than 30 days left | Nothing; the start log names the licence. |
| The last 30 days | Everything works. A warning at start and once a day in the log, and in `/$/license`. |
| Up to 30 days after expiry (grace) | Everything still works, with a daily warning. |
| Later | The enterprise features turn off. The open features keep working. |
| No licence, a changed licence, or an unknown key | As after the grace period, with the reason in the log. |
| More cores than the licence covers | A warning only. |

Features that protect data are the exception: row-level policies keep being enforced after a
licence lapses (only changing them turns off), so a lapsed licence never exposes data.

## Single sign-on

Callers can sign in with the access tokens of an OpenID Connect provider (Keycloak, Entra ID,
Okta, …), sent as `Authorization: Bearer`: `auth.oidc` in the auth configuration lists the
issuers trusted, the audience, and how claims become the caller's agent and groups. See
[single sign-on](server.md#single-sign-on-oidc) for the configuration (#198). It is `sso` in
a licence: without a licence that covers it, tokens are refused (401, with the reason), and
API keys and passwords keep working. The open build does not start with `auth.oidc`.

With a `browser` client, people also sign in to the web UI with the provider (the
authorization code flow with PKCE; the server exchanges the code and keeps the session, the
browser holds an `HttpOnly` cookie). See
[signing in with a browser](server.md#signing-in-with-a-browser).

Signatures are checked with the keys the provider publishes (JWKS), fetched by the server
itself: the provider must be reachable from it.

## Audit log

`tripletrail serve --audit-log FILE` keeps a record of who changed what (#207): a JSON line for
each change and admin call and each refused attempt at one, with what it did to each graph
(`--audit-reads` adds queries and other reads; `--audit-syslog` sends the lines on to a
syslog collector too; `tripletrail audit report FILE` sums it up by agent and dataset), chained by SHA-256 hashes so that
tampering shows, checked with `tripletrail audit verify FILE`. See
[operations](server.md#operations). It comes with the Enterprise build (the open build has no
`--audit-log`) and the licence is not checked for it: a record of changes that stopped when a
licence lapsed, or had not been renewed in time, would have gaps exactly where they matter.
The `audit` licence feature names it in contracts.

## Reviewed access changes

`"review": true` in the auth configuration (#207) makes every change of who may read and write
made through `/$/access` (policies, anonymous access, restores, keys for admins) a proposal
that a second admin approves: `GET /$/access/proposals`, `POST /$/access/proposals/{n}/approve`
or `…/reject`. Nobody approves their own proposal, the built-in `--admin-token` cannot propose
or approve, and no change is accepted while viewing as someone else. See [the access admin API](server.md#dataset-access-with-policies). Like the
audit log, it does not depend on the licence: a safeguard that lapsed with a licence would
fail exactly when nobody is watching.

## Proposed shape changes

On a dataset whose shapes come from a git repository, the shapes editor's changes can be
proposed as a pull request (GitHub) or merge request (GitLab) instead of being made on the
server (#145, licence feature `review`): `POST /$/datasets/{ds}/shapes/proposals`. The server
commits the changed files on a new branch through the forge's API, the repository's review
decides, and the merge goes live through the push webhook. On the Shapes page, **Propose…**
opens the pull request from the changes made there, and **Proposals** lists them. Besides the
dataset's admins, the agents and groups in `datasets.<name>.shapeEditors` of the auth
configuration (SSO groups, say) may propose: editors propose, the forge's reviewers approve,
and the audit log records who did what. See [the shapes API](server.md#proposed-shape-changes-tripletrail-enterprise).

## Row-level policies

Row rules of access policies (`sg:visibleIf`, `sg:hideProperties`, `sg:editableIf`; see
[access control](server.md#dataset-access-with-policies) and the guide's
[chapter 12](guide/12-access-control.md#rows-hidden-from-some-callers)) are an enterprise
feature, `row-policies` in a licence (#818).

- **The open build** cannot enforce them, so it never pretends to: a server does not start
  with a policy file that has row rules, a reload of such a file is refused (the policies in
  force stay), and a query under row rules through the library is refused. The message says
  they need TripleTrail Enterprise.
- **The enterprise build** enforces them always, whatever the licence says: with a valid
  licence, during its grace period, after it lapses, and without one.
- **Changing them** needs a licence that covers `row-policies` and is valid, expiring or in its
  grace period. Otherwise the access pages refuse, with `403`, a policy write, a removal or a
  restore of the access history that changes a policy file with row rules (before or after
  the change, or a restore whose version does not have such a file, since its configuration
  may no longer name it); changes to files without row rules work as before. Files changed on disk are
  still loaded, so the rules in force are always the ones in the files.
- `GET /$/license` has `rowPolicies`: `inUse` (a policy has row rules), `enforced`, `editable`
  and, when they cannot be changed, the `reason`. The start log warns when row rules are in
  use and cannot be changed.
- **The Access page** lists the row rules of each policy and builds new ones from the data,
  without Turtle: a class, conditions on its properties, a preview of what a caller would see
  before the rule is saved (see the guide's
  [chapter 12](guide/12-access-control.md#try-a-rule-before-you-save-it)). The preview works
  without a licence; saving needs one, as above.

In the code, the open engine keeps what a query needs to apply row rules (the rules, the row
filter the store scans with, the cache of compiled filters) and a `RowCompiler` hook;
compiling and judging the rules is `tripletrail_enterprise::rows`, which the enterprise server
installs at start. Their tests run with
`cargo test -p tripletrail-enterprise --features rows` and
`cargo test -p tripletrail-server --features enterprise --lib --test auth --test license --test guide`.

## Updates

- Releases are versioned (`vX.Y.Z`) and built from the same tag in both editions.
- Each release of the enterprise image is published to the public
  `ghcr.io/matdata-eu/tripletrail-enterprise`, tagged with its version. Pulling it needs no
  token; what a server may do with it depends on its licence. A licence that has expired turns
  the enterprise features off as in the table above, while the open features keep working.
