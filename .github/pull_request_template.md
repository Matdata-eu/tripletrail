## What and why

<!-- What changes, and its issue: "Closes #…" when this finishes it, "Part of #…" when it is one step of it. -->

## How it was tested

<!-- Tests added, and numbers for a performance change. -->

## Security checklist

<!-- Only for a PR that adds an endpoint, an outbound request, an option naming a file or directory, or HTML built from data; delete otherwise. See docs/security.md#checking-a-change. -->

- [ ] Leaks: an entry in `crates/tripletrail-server/tests/leaks.rs` for anything that reads the data
- [ ] Auth group: the group or dataset access it needs, and its 401/403 tested
- [ ] Origin/CSRF: a change goes through the cross-site check (`Origin`, `Sec-Fetch-Site`)
- [ ] SSRF: an outbound request goes through an allowlist, follows no redirects, sends no user info and caps its answer
- [ ] Logs: no secrets, and no query text outside the slow query and audit logs
- [ ] CSP: HTML escapes every value and carries the `Content-Security-Policy` and `nosniff`

## Documentation

- [ ] No user-facing change, or:
- [ ] The user guide (`docs/guide/`) shows it (a chapter updated or added, examples tested with `cargo test -p tripletrail-server --test guide`)
- [ ] The reference page in `docs/` and the CHANGELOG entry describe it
