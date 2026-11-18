# Security policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub: on the repository's **Security**
tab, choose **Report a vulnerability**. Don't open a public issue.

Include what you found, the version or commit, and how to reproduce it. We confirm receipt
within 5 business days, keep you informed while we work on a fix, and credit you in the
advisory unless you prefer otherwise. We aim to publish a fix within 90 days of the report,
sooner for issues that are actively exploited.

## Supported versions

TripleTrail has not had a stable release yet. Until 1.0, only the latest release and `main`
get security fixes.

## Running TripleTrail safely

Without `--update-token`, `--admin-token` or `--auth-config`, `tripletrail serve` has no
authentication: anyone who can reach the port can query, update and delete. It binds to
`127.0.0.1` by default. Before serving other machines, turn on authentication (API keys, users
with passwords, or trusted headers from an authenticating proxy) and HTTPS, or put it behind a
reverse proxy that does both. Use `--cors` only with the origins you trust. See
[Running it in production](docs/server.md#running-it-in-production) and
[Authentication](docs/server.md#authentication).

## Dependencies

CI runs `cargo audit` on both lockfiles and `cargo deny check`. Known advisories that are
waiting for an upstream fix are listed, with their issue, in `.cargo/audit.toml`.
