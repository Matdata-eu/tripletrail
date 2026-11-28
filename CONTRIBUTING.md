# Contributing to TripleTrail

Thank you for your interest in TripleTrail. Bug reports, questions and ideas are welcome as
GitHub issues.
Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).

## Licensing of contributions

TripleTrail is developed by Matdata as open core: this repository is open source under the
Apache License 2.0 (see [LICENSE](LICENSE)), and Matdata offers commercial editions and
support on top of it. There is no Contributor License Agreement to sign. As section 5 of the
Apache License 2.0 says, a contribution you submit for inclusion is licensed under the Apache
License 2.0, the same licence the project uses, and you keep the copyright in your work.
Only submit work you have the right to license this way.

Pull requests are welcome. For a larger change, open an issue first so we can agree on the
approach; to work on an open issue, say so in a comment on it.

## Pull requests

- Use [Conventional Commits](https://www.conventionalcommits.org) for commit messages
  (`feat:`, `fix:`, `perf:`, `docs:`, `test:`, `refactor:`, `chore:`).
- Run the checks CI runs before you push:

  ```sh
  cargo fmt --check
  cargo clippy --workspace --all-targets -- -D warnings
  cargo test --all
  cargo deny check
  ```

  A pull request that changes only documentation (top-level Markdown, `docs/`, `bench/`,
  `LICENSE`, `NOTICE`) skips the `test`, `audit` and `licenses` jobs, which still count as
  passed for the merge; any other file runs them all (`.github/scripts/code-changed.sh`).

  A change to the REST API or OGC API Features also passes `.github/scripts/lint-openapi.sh target/debug/tripletrail`
  (after `cargo build -p tripletrail-server`; needs Node.js): Spectral's OpenAPI rules on the
  definitions it makes, with no warning.

  A change to the web UI also passes its browser tests, which open Search, Map, Explore,
  Describe and Query in Chromium on the guide's sample network: see
  [crates/tripletrail-server/ui-tests](crates/tripletrail-server/ui-tests/README.md) (needs
  Node.js; Playwright is a dev dependency only, never shipped).

- Add a test that fails without your change.
- A test or bench that needs a folder on disk makes it with
  `tripletrail_testdir::tempdir("name")` (crate `tripletrail-testdir`, a dev-dependency), not
  with `std::env::temp_dir().join(...)`: the folder is named `tripletrail-name-<random>` and is
  removed when the value is dropped, also when the test fails or panics. Keep it until a server
  or process that writes into it has stopped. To look at the files afterwards, run the tests
  with `TRIPLETRAIL_KEEP_TMP=1`: the folders are kept and their paths printed. CI fails when a
  test leaves a `tripletrail-*` folder behind.
- Keep the documentation true. A change users see (a command, option, endpoint, default, answer
  or page of the web UI) updates the [user guide](docs/guide/README.md) where it shows that
  part, or adds to it, and the reference page in `docs/` that describes it. The guide's examples
  run in CI (`cargo test -p tripletrail-server --test guide`), so a change that alters an answer
  the guide shows fails until the guide follows; see the guide's README for how its blocks are
  tested.
- Don't commit data you may not publish, such as data received from a customer under a
  confidentiality agreement.

## Releasing

1. Set the version in `[workspace.package]` of `Cargo.toml` (and run `cargo check` so
   `Cargo.lock` follows).
2. Rename `## [Unreleased]` in `CHANGELOG.md` to `## 1.2.3` and start a new, empty
   `## [Unreleased]` above it.
3. Add the release's entry at the top of `crates/tripletrail-server/ui/whats-new.json` (the web
   UI's "What's new"; a test checks that its newest entry is the crate version), and set the
   same version in `python/Cargo.toml` (then `cargo update --manifest-path python/Cargo.toml -w`)
   and `clients/js/package.json`.
4. Check with `.github/scripts/release-notes.sh --check 1.2.3`, merge, then tag `v1.2.3`
   on main and push the tag. The tag is `v` and the version exactly as `Cargo.toml` has it: a
   release candidate is `v0.1.0-rc.2`, with the dot, not `v0.1.0-rc2`.
   Creating the release on GitHub's Releases page with that new tag on main works too: it
   pushes the tag, and the workflow adds the files and, when the release has none, the notes.

The "Release" workflow then builds `tripletrail` for Linux (x86_64, aarch64), macOS (x86_64,
arm64) and Windows (x86_64). Each archive holds `LICENSE`, `NOTICE` and
`THIRD_PARTY_LICENSES.md`: every crate linked into the binary with its license and the license
texts it ships, written by `.github/scripts/third-party-licenses.py` (run it locally after
`cargo fetch --locked` to see the list). It publishes them as a GitHub release with `SHA256SUMS`, and
the notes are the CHANGELOG section followed by the commits since the last tag, grouped by
type. Starting the workflow by hand does a dry run that keeps everything as a workflow
artifact. The tag also starts "Docker image", which publishes the image as `1.2.3`, `1.2` and
`latest` (a pre-release tag such as `v0.1.0-rc.3` publishes only `0.1.0-rc.3`, so `latest` stays
on the last release), and "Helm chart" with "publish" ticked publishes the chart.

The same run builds the Python wheels (each with `THIRD_PARTY_LICENSES.md` for `python/Cargo.lock`; there
is no source package, which would need the workspace and `vendor/` around `python/`) and packs
the JavaScript client.
After the GitHub release, it publishes them to PyPI (`tripletrail`) and npm
(`@matdata/tripletrail-client`, a pre-release under the `next` tag), each only while the repository
variable `PUBLISH_PYPI` or `PUBLISH_NPM` is `true`. Both use trusted publishing (no token):
the publisher on pypi.org and npmjs.com names `Matdata-eu/tripletrail`, the workflow
`release.yml` and the environment `pypi` or `npm`. PyPI takes a "pending" publisher before the
project exists. npm does not, so the client's first version is published once by hand, by a
member of the `matdata` npm organisation (`cd clients/js && cp ../../LICENSE ../../NOTICE . &&
npm publish --access public`), and its trusted publisher is added after that. The Rust crates are not published to crates.io: they
build on the patched Oxigraph crates in `vendor/`, which a published crate cannot use.

## Security issues

Please don't report vulnerabilities in public issues. See [SECURITY.md](SECURITY.md).
