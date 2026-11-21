# 12. Access control

Out of the box, `tripletrail serve` listens only on your own machine (`127.0.0.1`) and anyone who
reaches it may read and write. Before others can reach it, decide who may do what. There are
three levels; pick the simplest one that fits.

| You need | Use |
|---|---|
| Everyone may read; only your scripts write | two tokens |
| People and programs each with their own key, some writing, some only reading | API keys and groups |
| Per dataset or per graph: who reads and who writes, or rows hidden from some | policies |

## Level 1: two tokens

`--update-token` guards every write (SPARQL updates and Graph Store uploads), and
`--admin-token` guards the admin API under `/$/` (creating, deleting and backing up datasets).
Queries stay open:

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --update-token s3cret --admin-token adm1n
```

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/ds/update \
  --data-urlencode 'update=INSERT DATA { <urn:example:a> <urn:example:b> 1 }'
curl -s -o /dev/null -w '%{http_code}\n' http://localhost:7878/ds/update \
  -H 'Authorization: Bearer s3cret' \
  --data-urlencode 'update=INSERT DATA { <urn:example:a> <urn:example:b> 1 }'
```

<!-- test: output -->
```text
401
200
```

Without the token the write is refused (`401`); with it, it is done. Use long random tokens,
and give them through environment variables (`TRIPLETRAIL_UPDATE_TOKEN`,
`TRIPLETRAIL_ADMIN_TOKEN`) or secrets rather than on the command line. The web UI asks for the
admin token when a change needs it.

## Level 2: API keys and groups

An auth configuration file gives each person or program a key of its own. The file stores
only a key's SHA-256 hash, which `tripletrail hash-key` prints:

<!-- test: run -->
```sh
tripletrail hash-key reader-secret-1
tripletrail hash-key writer-secret-2
```

<!-- test: output -->
```text
baa1aadafabc6fa591820f3e8f2970ad6fe813c5e09804eb932059684b9b8478
b9f571a529bd6992b1eec384ba20cf9be4fb2f854049cb180b7a13976f11019f
```

(Without an argument it reads the key from standard input, which keeps it out of your shell
history.) Each key names an agent, an IRI for who holds it, and its groups. Save as
`auth.json`:

<!-- test: file auth.json -->
```json
{
  "auth": {
    "anonymous": true,
    "apiKeys": [
      { "sha256": "b9f571a529bd6992b1eec384ba20cf9be4fb2f854049cb180b7a13976f11019f",
        "agent": "https://example.org/people/ann", "groups": ["planners"] },
      { "sha256": "baa1aadafabc6fa591820f3e8f2970ad6fe813c5e09804eb932059684b9b8478",
        "agent": "https://example.org/people/bob" }
    ],
    "users": [
      { "name": "carol", "groups": ["planners"],
        "password": "$pbkdf2-sha256$600000$Xxwqnot9PE9qDhssPU5fYA$jlMK./TqSeTJ36OpwHNG7Zzasd22YFlKPsSDCv0UW3E" }
    ]
  },
  "datasets": {
    "ds": { "policies": ["ds-policies.ttl"] }
  }
}
```

A caller sends its key as `Authorization: Bearer <key>` (or `X-API-Key: <key>`).
`"anonymous": true` lets callers without a key in too, as nobody in particular. Without
policies, everyone identified may read, the groups `tripletrail:writer` and `tripletrail:admin` may
write, and `tripletrail:admin` may administer. Keys can also expire (`"expires": "2027-06-30"`)
and be limited to some datasets (`"datasets": ["rail"]`).

People can also sign in with a user name and password, as on Fuseki, sent with HTTP Basic
(`curl -u`, a browser's sign-in prompt, SPARQLWrapper's `setCredentials`). The `users` entry
above is Carol, a planner. The file holds only a hash of her password, which
`tripletrail hash-password` prints (each time a different one, with a new random salt):

```sh
tripletrail hash-password carol-secret-3
```

A user's agent is `urn:sparql-guard:user:` and the name, unless the entry gives an `agent`.
Users can also expire and be limited to datasets, as keys can. `tripletrail import-shiro
shiro.ini` turns the users of a Fuseki `shiro.ini` into `users` entries: Shiro's
`$shiro1$` hashes are kept as they are, passwords written in the clear are hashed, and roles
become groups (`admin` becomes `tripletrail:admin`). Passwords are sent with every request, so
serve over HTTPS ([chapter 13](13-production.md)).

Behind a reverse proxy that signs people in (oauth2-proxy, an ingress with external
authentication), the configuration can trust the user and groups headers the proxy sets
instead: see [authentication](../server.md#authentication).

## Level 3: policies per dataset

The `datasets` part of `auth.json` names policy files for a dataset. A policy says which
agents or groups it applies to and what they may do, in the
[sparql-guard](https://matdata-eu.github.io/sparql-guard/) vocabulary. Save as
`ds-policies.ttl`: anyone may read, and the planners write:

<!-- test: file ds-policies.ttl -->
```turtle
@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
@prefix ex: <https://example.org/policies/> .

ex:Readers  a sg:Policy ; sg:agent sg:Anyone ;     sg:readAllGraphs true .
ex:Planners a sg:Policy ; sg:group "planners" ;    sg:writeAllGraphs true .
```

<!-- test: serve -->
```sh
tripletrail serve --load network.ttl --auth-config auth.json
```

Now an anonymous caller, Bob and Ann each try a query and an update:

<!-- test: run -->
```sh
for key in "" reader-secret-1 writer-secret-2; do
  auth=(); [ -n "$key" ] && auth=(-H "Authorization: Bearer $key")
  query=$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" http://localhost:7878/ds/query \
    --data-urlencode 'query=ASK { ?s ?p ?o }')
  update=$(curl -s -o /dev/null -w '%{http_code}' "${auth[@]}" http://localhost:7878/ds/update \
    --data-urlencode 'update=INSERT DATA { <urn:example:a> <urn:example:b> 1 }')
  echo "${key:-anonymous}: query $query, update $update"
done
```

<!-- test: output -->
```text
anonymous: query 200, update 401
reader-secret-1: query 200, update 403
writer-secret-2: query 200, update 200
```

Everyone reads. The anonymous caller is asked to identify itself (`401`); Bob is known but not
allowed to write (`403`); Ann, a planner, writes.

Carol signs in with her password, and is a planner too; a wrong password is refused:

<!-- test: run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' -u carol:carol-secret-3 http://localhost:7878/ds/update \
  --data-urlencode 'update=INSERT DATA { <urn:example:a> <urn:example:b> 2 }'
curl -s -o /dev/null -w '%{http_code}\n' -u carol:wrong http://localhost:7878/ds/query \
  --data-urlencode 'query=ASK { ?s ?p ?o }'
```

<!-- test: output -->
```text
200
401
```

Policies go further:

- `sg:readGraph <g>` and `sg:writeGraph <g>` grant single named graphs: a partner sees only
  the graph made for them, and the rest of the dataset looks empty to them. They may still
  call the `st:` service ([chapter 4](04-spatial.md)), routing ([chapter 5](05-routing.md)),
  path search and the window service, which read only their graphs, in queries and in
  updates; other services are refused to them.
- `sg:visibleIf` and `sg:hideProperties` hide resources or properties that a SHACL shape
  describes, inside every query, map tile and API answer (next section; TripleTrail Enterprise).
- Once one dataset has policies, datasets without any are closed to all but admins.

## Rows hidden from some callers

Row-level policies are a feature of **TripleTrail Enterprise** (see
[enterprise](../enterprise.md)): the open build refuses to start with a policy file that has
row rules, rather than show what they would hide. An enterprise server keeps enforcing them
when its licence lapses; only changing them then needs a licence that covers `row-policies`.

Say Bob is a contractor who maintains lines 25 and 36: he should see only their sections, and
change only those of line 25. A row rule names a SHACL shape: `sg:visibleIf` hides every
target of the shape that does not conform to it, and `sg:editableIf` lets the caller change
only the targets that conform. The shapes go in the same file. Save as `rows-policies.ttl`:

<!-- test: enterprise file rows-policies.ttl -->
```turtle
@prefix sg: <https://matdata.eu/ns/sparql-guard#> .
@prefix sh: <http://www.w3.org/ns/shacl#> .
@prefix ex: <https://example.org/rail#> .
@prefix pol: <https://example.org/policies/> .

pol:Planners a sg:Policy ; sg:group "planners" ; sg:readAllGraphs true ; sg:writeAllGraphs true .
pol:Contractor a sg:Policy ; sg:agent <https://example.org/people/bob> ;
  sg:readAllGraphs true ; sg:writeAllGraphs true ;
  sg:visibleIf ex:Lines25And36 ; sg:editableIf ex:Line25 .

ex:Lines25And36 a sh:NodeShape ; sh:targetClass ex:Section ;
  sh:property [ sh:path ex:line ; sh:in ( "25" "36" ) ] .
ex:Line25 a sh:NodeShape ; sh:targetClass ex:Section ;
  sh:property [ sh:path ex:line ; sh:hasValue "25" ] .
```

Point the dataset at it, and give yourself an admin key (`admin-secret-3`) for the last step.
Save as `rows-auth.json`:

<!-- test: enterprise file rows-auth.json -->
```json
{
  "auth": {
    "apiKeys": [
      { "sha256": "b9f571a529bd6992b1eec384ba20cf9be4fb2f854049cb180b7a13976f11019f",
        "agent": "https://example.org/people/ann", "groups": ["planners"] },
      { "sha256": "baa1aadafabc6fa591820f3e8f2970ad6fe813c5e09804eb932059684b9b8478",
        "agent": "https://example.org/people/bob" },
      { "sha256": "57ed99c004d0a5fedec135055639665edd3c73129a2d235e89ae25cd12efe880",
        "agent": "https://example.org/people/you", "groups": ["tripletrail:admin"] }
    ]
  },
  "datasets": {
    "ds": { "policies": ["rows-policies.ttl"] }
  }
}
```

<!-- test: enterprise serve -->
```sh
tripletrail serve --load network.ttl --auth-config rows-auth.json
```

Ann and Bob count the sections:

<!-- test: enterprise run -->
```sh
for key in writer-secret-2 reader-secret-1; do
  curl -s -H "Authorization: Bearer $key" -H 'Accept: text/csv' http://localhost:7878/ds/query \
    --data-urlencode 'query=SELECT (COUNT(*) AS ?sections) { ?s a <https://example.org/rail#Section> }'
done
```

<!-- test: enterprise output -->
```text
sections
12
sections
3
```

Ann sees all twelve; Bob sees the three sections of lines 25 and 36, and every query he sends
(joins, counts, paths, map tiles, the OGC API) works on that view: the other sections are not
there for him. The stations stay visible, as the shapes target sections only.

Full-text search works for him too, on what he sees: the index he searches holds only the
labels of his view, and ranks them as a store holding only those would.

<!-- test: enterprise run -->
```sh
curl -s -H 'Authorization: Bearer reader-secret-1' -H 'Accept: text/csv' http://localhost:7878/ds/query \
  --data-urlencode 'query=SELECT ?station { ?station <http://w3id.org/tripletrail/fn#search> "brussels" } ORDER BY ?station'
```

<!-- test: enterprise output -->
```text
station
https://example.org/rail#BXC
https://example.org/rail#BXN
https://example.org/rail#BXS
```

Routing ([chapter 5](05-routing.md)) and the other built-in services (`st:`, the window
service, path search) read his view too. The shortest way from Mechelen to Leuven is section
S06 for Ann; Bob does not see it, so his route goes through Brussels-North:

<!-- test: enterprise run -->
```sh
for key in writer-secret-2 reader-secret-1; do
  curl -s -H "Authorization: Bearer $key" -H 'Accept: text/csv' http://localhost:7878/ds/query \
    --data-urlencode 'query=PREFIX ex: <https://example.org/rail#>
PREFIX pgr: <http://w3id.org/tripletrail/pgr#>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
SELECT ?seq ?station ?km {
  SERVICE <http://w3id.org/tripletrail/pgr> {
    [] pgr:function pgr:pgr_dijkstra ; pgr:start_vid ex:MEC ; pgr:end_vid ex:LV ;
       pgr:directed false ; pgr:seq ?seq ; pgr:node ?node ; pgr:agg_cost ?km .
    { SELECT ?id ?source ?target ?cost {
        ?id a ex:Section ; ex:from ?source ; ex:to ?target ; ex:lengthKm ?cost } }
  }
  ?node rdfs:label ?station
} ORDER BY ?seq'
done
```

<!-- test: enterprise output -->
```text
seq,station,km
1,Mechelen,0
2,Leuven,24
seq,station,km
1,Mechelen,0
2,Brussels-North,20
3,Leuven,50
```

A `SERVICE` other than these is refused to him, as is one of them called inside another.

Now Bob raises the speed of section S03 (line 25), then of S05 (line 36), then tries to move
S03 to another line:

<!-- test: enterprise run -->
```sh
speed() {
  curl -s -o /dev/null -w '%{http_code}\n' -H 'Authorization: Bearer reader-secret-1' \
    http://localhost:7878/ds/update --data-urlencode "update=PREFIX ex: <https://example.org/rail#>
      DELETE { $1 } INSERT { $2 } WHERE { $1 }"
}
speed 'ex:S03 ex:maxSpeed ?v' 'ex:S03 ex:maxSpeed 200'
speed 'ex:S05 ex:maxSpeed ?v' 'ex:S05 ex:maxSpeed 200'
speed 'ex:S03 ex:line ?l' 'ex:S03 ex:line "50A"'
```

<!-- test: enterprise output -->
```text
200
403
403
```

The first update is done. S05 he may see but not change (`403`, `sg:editableIf`), and moving
S03 to line 50A would take it out of his own view, which is refused too: an update may not
hide what it touches from the one who sends it. A refused update changes nothing. What Bob
cannot see he cannot change either: a `DELETE` matches only the rows he sees, and writing
about a hidden section is refused.

To find out why someone does not see a resource, a server admin asks the access explain page:

<!-- test: enterprise run -->
```sh
curl -s -H 'Authorization: Bearer admin-secret-3' \
  'http://localhost:7878/$/access/datasets/ds/explain?agent=https://example.org/people/bob&resource=https://example.org/rail%23S01'
```

<!-- test: enterprise output contains -->
```text
"hiddenBy":["<https://example.org/rail#Lines25And36>"]
"notEditableBy":["<https://example.org/rail#Line25>"]
"visible":false
```

The answer (a longer JSON object; these are parts of it) says that S01 is hidden from Bob by
`ex:Lines25And36`, and that `ex:Line25` would also keep him from changing it. Add `query=`
to see, for each pattern of a query, how many matches there are and how many Bob sees.
Admins, and callers whose policies have no row rules, see everything.

The files are reloaded when they change, without a restart. The **Access** page of the web UI
(`/$/ui/access`) edits the same files: a matrix of graphs and groups, API keys created and
revoked, **View as** someone else to check what they see, and a history of every change that
can be restored. While you view as someone, the query page, the map, the graph explorer and the
search box show their view, under a banner that names them; describe pages still show yours.

## Signing in with your organisation's accounts

Keys and passwords are one more list to keep. With **TripleTrail Enterprise** (a licence that
covers `sso`), callers sign in with the accounts your organisation already has, through an
OpenID Connect provider such as Keycloak or Microsoft Entra ID: a script or app gets an access
token from the provider and sends it as `Authorization: Bearer`. The `oidc` part of the auth
configuration says which provider to trust, and how the token's claims become the caller's
agent and groups. Save as `sso-auth.json`:

<!-- test: enterprise file sso-auth.json -->
```json
{
  "auth": {
    "oidc": {
      "issuers": [{
        "issuer": "https://login.example.org/realms/rail",
        "audience": "tripletrail",
        "agent": "https://example.org/people/{preferred_username}",
        "groups": "groups",
        "groupNames": { "rail-editors": "tripletrail:writer" },
        "browser": {
          "name": "Example Rail",
          "clientId": "tripletrail-ui",
          "redirectUri": "https://tripletrail.example.org/$/sso/callback"
        }
      }]
    }
  }
}
```

Jan, signed in as `jan` and in the provider's group `rail-editors`, is then the agent
`https://example.org/people/jan` with the group `tripletrail:writer`, and policies name him as
they would name the owner of a key. The server's own groups come only through `groupNames`.

<!-- test: enterprise serve -->
```sh
tripletrail serve --load network.ttl --auth-config sso-auth.json
```

A token from any other issuer, for another application (`aud`), expired, or with a signature
that does not check out is refused, with the reason:

<!-- test: enterprise run -->
```sh
token=eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJodHRwczovL2Vsc2V3aGVyZS5leGFtcGxlLm9yZyIsImF1ZCI6InRyaXBsZXRyYWlsIn0.c2ln
curl -s -w '\n%{http_code}\n' -H "Authorization: Bearer $token" http://localhost:7878/ds/query \
  --data-urlencode 'query=ASK {}'
```

<!-- test: enterprise output contains -->
```text
The token's issuer is not trusted here
401
```

The `browser` part lets people sign in to the web UI too: register TripleTrail at the provider
as a web application with the redirect URI above (your server's address and
`/$/sso/callback`). Someone who opens `/$/ui/` is sent to the provider to sign in, comes back
signed in, and the key button at the top shows who they are, with **Sign out**. The browser
keeps only a session cookie that scripts cannot read, and changes made with it must come from
the server's own pages. `/$/sso` says which sign-ins there are:

<!-- test: enterprise run -->
```sh
curl -s http://localhost:7878/\$/sso
```

<!-- test: enterprise output contains -->
```text
"name":"Example Rail"
"signedIn":null
```

What to set up at the provider, step by step for Keycloak and for Microsoft Entra ID (the
clients, the audience, and the claims that carry groups), is in the server reference:
[setting up Keycloak](../server.md#setting-up-keycloak) and
[setting up Entra ID](../server.md#setting-up-microsoft-entra-id).

When someone gets other groups than you expect, a server admin sends their token to the
access explain page in the header `X-TripleTrail-Explain-Token`: the answer is what that person
may do, and `token.groupClaims` says which claim gave each group.

The server fetches the provider's signing keys itself, so it must reach the provider. Every
setting, with an Entra ID example, is in [single sign-on](../server.md#single-sign-on-oidc).

## Four eyes on access changes

With **TripleTrail Enterprise**, `"review": true` in the auth configuration makes a second admin
approve every change of who may read and write: a policy written or removed, anonymous access,
a restore of the access history, a key for an admin. The admin who makes the change only
proposes it. Two admins, Ann (key `ann-secret`) and Bob (`bob-secret`); save as
`review-auth.json`:

<!-- test: enterprise file review-auth.json -->
```json
{
  "auth": {
    "review": true,
    "apiKeys": [
      { "sha256": "ee8dae49b99a4f8203b217d8274209533399ef04a3643e4a15ddcd8a516b2015",
        "agent": "https://example.org/people/ann", "groups": ["tripletrail:admin"] },
      { "sha256": "9f03ef1533a68d2f506f81ef463c1183a82a6bd40e45613f36e6fe1889cf1b99",
        "agent": "https://example.org/people/bob", "groups": ["tripletrail:admin"] }
    ]
  }
}
```

<!-- test: enterprise serve -->
```sh
tripletrail serve --load network.ttl --auth-config review-auth.json
```

Ann gives the group `planners` read access to the dataset. The answer is `202`, a proposal,
and nothing has changed yet:

<!-- test: enterprise run -->
```sh
curl -s -w '\n%{http_code}\n' -X PUT -H 'Authorization: Bearer ann-secret' \
  http://localhost:7878/\$/access/datasets/ds/policies \
  -d '{"id": "https://example.org/policies/planners", "groups": ["planners"], "readAllGraphs": true}'
```

<!-- test: enterprise output contains -->
```text
"status":"pending"
"proposal":1
202
```

Ann cannot approve it herself (`403`); Bob, who sees it in `GET /$/access/proposals`, can,
and that makes the change:

<!-- test: enterprise run -->
```sh
curl -s -o /dev/null -w '%{http_code}\n' -X POST -H 'Authorization: Bearer ann-secret' \
  http://localhost:7878/\$/access/proposals/1/approve
curl -s -X POST -H 'Authorization: Bearer bob-secret' \
  http://localhost:7878/\$/access/proposals/1/approve
```

<!-- test: enterprise output contains -->
```text
403
"status":"approved"
"decidedBy":"https://example.org/people/bob"
```

The access history records the change as Bob's, made on Ann's proposal; `…/reject` drops a
proposal instead. On the **Access** page of the web UI, the pending proposals are listed at
the top with **Approve** and **Reject**, and a change saved there says that it was proposed. Revoking a key, and making a key for someone who is no admin, are not held
back: neither can widen what an admin may do alone. When the proposal is a key for a new
admin, the approver receives that key, so until they hand it over they hold two admin
identities. The built-in `--admin-token` is nobody in particular, so with review on it can
reject but not propose or approve, and no change is accepted while viewing as someone else.

## Before you open the port

- Bind to the network only when access control is on: `--bind 0.0.0.0:7878` listens on every
  interface (the Docker image does).
- Serve HTTPS, from a reverse proxy or with `--tls-cert` and `--tls-key`, so keys do not cross
  the network in clear text.
- Set `--cors https://your.app` rather than `--cors` alone.

See [dataset access with policies](../server.md#dataset-access-with-policies) for every term,
and [running it in production](13-production.md) for the rest.

## What you learnt

- `--update-token` and `--admin-token` are the quick way to protect writes and the admin API.
- `--auth-config` gives each person or program a key, an agent and groups.
- Policies decide per dataset, per graph and per resource who reads and who writes.
- `sg:visibleIf` hides the resources a shape rejects from every answer; `sg:editableIf` limits
  which of them a caller may change; the explain page says which rule hides what. Row-level
  policies need TripleTrail Enterprise.
- With TripleTrail Enterprise, `auth.oidc` lets callers sign in with your identity provider's
  tokens, whose claims become their agent and groups.

Next: [running TripleTrail in production](13-production.md).
