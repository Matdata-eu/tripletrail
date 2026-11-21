# Browser tests of the web UI

What a user sees in the five reading tools (Search, Map, Explore, Describe, Query), opened in
Chromium on the guide's sample network (`docs/guide/data/network.ttl`). Playwright is a dev and
CI dependency only: nothing here is part of the server or its image.

```sh
cargo build -p tripletrail-server
cd crates/tripletrail-server/ui-tests
npm install --no-package-lock
npx playwright install chromium   # once
node --test
```

`TRIPLETRAIL_BIN` names another `tripletrail` binary (`target/debug/tripletrail` by default).
The page may reach only the test's server: other requests, such as the map's base tiles, are
refused, so the tests need no network.
