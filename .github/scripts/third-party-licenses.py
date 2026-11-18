#!/usr/bin/env python3
"""Writes the third-party license list of a TripleTrail release.

Lists every crate linked into a binary of the workspace (the normal dependencies of the
given package, on every platform, so one list covers every release target) with its
version, license expression and source, followed by the license and notice files each
crate ships, each distinct text once.

    python3 .github/scripts/third-party-licenses.py [--package tripletrail-server] [-o FILE]

Reads `cargo metadata`, so the crates' sources must be downloaded (`cargo fetch --locked`).
Uses only the standard library and cargo.
"""

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys

MIT_TEXT = """MIT License

Copyright (c) {holders}

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE."""

LICENSE_FILE = re.compile(r"^(LICEN[CS]E|COPYING|NOTICE|UNLICENSE|COPYRIGHT)([-._].*)?$", re.IGNORECASE)


def metadata(manifest_path):
    cmd = ["cargo", "metadata", "--format-version", "1", "--locked"]
    if manifest_path:
        cmd += ["--manifest-path", manifest_path]
    return json.loads(subprocess.run(cmd, check=True, capture_output=True, text=True).stdout)


def linked_packages(meta, root_name):
    """The packages the root package links: reachable through normal (not dev or build)
    dependencies, on any platform. Proc-macro crates run in the compiler and are not linked,
    so they, and what only they depend on, are left out."""
    packages = {p["id"]: p for p in meta["packages"]}

    def proc_macro(pid):
        return any("proc-macro" in t["kind"] for t in packages[pid]["targets"])

    nodes = {n["id"]: n for n in meta["resolve"]["nodes"]}
    roots = [p["id"] for p in meta["packages"] if p["name"] == root_name and p["source"] is None]
    if not roots:
        sys.exit(f"no workspace package named {root_name}")
    seen, todo = set(), list(roots)
    while todo:
        pid = todo.pop()
        if pid in seen:
            continue
        seen.add(pid)
        for dep in nodes[pid]["deps"]:
            if any(k["kind"] is None for k in dep["dep_kinds"]) and not proc_macro(dep["pkg"]):
                todo.append(dep["pkg"])
    members = set(meta["workspace_members"])
    return sorted(
        (packages[pid] for pid in seen if pid not in members),
        key=lambda p: (p["name"].lower(), p["version"]),
    )


def license_files(package):
    root = os.path.dirname(package["manifest_path"])
    files = []
    if package.get("license_file"):
        files.append(os.path.join(root, package["license_file"]))
    try:
        names = sorted(os.listdir(root))
    except OSError:
        names = []
    for name in names:
        path = os.path.join(root, name)
        if LICENSE_FILE.match(name) and os.path.isfile(path) and path not in files:
            files.append(path)
    return files


def without_file(package):
    """The license text to write for a crate that ships no license file: None when Apache-2.0
    is one of its options (its text is TripleTrail's own LICENSE), the MIT text with the crate's
    authors when MIT is, and an error otherwise, so that no crate goes out without its text."""
    expression = (package.get("license") or "").strip()
    options = [o.strip() for o in re.split(r"\s+OR\s+|/", expression.strip("()"))]
    if " AND " in expression or not expression:
        options = []
    if any(o.startswith("Apache-2.0") for o in options):
        return None
    if "MIT" in options:
        authors = [re.sub(r"\s*<[^>]*>", "", a).strip() for a in package.get("authors") or []]
        authors = [a for a in authors if a]
        holders = ", ".join(authors) or f"the {package['name']} authors ({source_of(package)})"
        return MIT_TEXT.format(holders=holders)
    sys.exit(
        f"{package['name']} {package['version']} ships no license file and its license "
        f"({expression or 'none given'}) has no text here: add one to "
        ".github/scripts/third-party-licenses.py"
    )


def source_of(package):
    if package.get("repository"):
        return package["repository"]
    if package["source"] and package["source"].startswith("registry+"):
        return f"https://crates.io/crates/{package['name']}"
    return package["source"] or "vendored in this repository"


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--package", default="tripletrail-server", help="the binary's workspace package")
    parser.add_argument("--manifest-path", help="Cargo.toml of the workspace (default: the current one)")
    parser.add_argument("-o", "--output", help="write here instead of standard output")
    args = parser.parse_args()

    packages = linked_packages(metadata(args.manifest_path), args.package)
    texts = {}  # digest -> (text, [crate@version, ...])
    rows, missing = [], []
    for p in packages:
        crate = f"{p['name']} {p['version']}"
        found = []
        for path in license_files(p):
            with open(path, encoding="utf-8", errors="replace") as f:
                text = f.read().strip()
            if not text:
                continue
            digest = hashlib.sha256(text.encode()).hexdigest()
            texts.setdefault(digest, (text, []))[1].append(f"{crate} ({os.path.basename(path)})")
            found.append(digest)
        if not found:
            text = without_file(p)
            if text is None:
                missing.append(crate)
            else:
                digest = hashlib.sha256(text.encode()).hexdigest()
                texts.setdefault(digest, (text, []))[1].append(f"{crate} (standard text)")
        rows.append((p["name"], p["version"], p.get("license") or "see its license file", source_of(p)))

    out = []
    out.append(f"# Third-party licenses of {args.package}\n")
    out.append(
        "TripleTrail is licensed under the Apache License 2.0 (LICENSE); see NOTICE for the "
        "components it adapts or embeds. This file lists the Rust crates linked into the "
        f"`tripletrail` binary ({len(rows)} crates, every platform) with the license and notice "
        "texts they ship. It is generated by .github/scripts/third-party-licenses.py for "
        "each release.\n"
    )
    out.append("| Crate | Version | License | Source |")
    out.append("|---|---|---|---|")
    for name, version, license, source in rows:
        out.append(f"| {name} | {version} | {license} | {source} |")
    if missing:
        out.append(
            "\nThese crates ship no license file and are used under the Apache License 2.0, "
            "one of their options, whose text is TripleTrail's LICENSE: "
            + ", ".join(missing)
            + "."
        )
    out.append("\n## License texts\n")
    for text, crates in sorted(texts.values(), key=lambda t: t[1][0].lower()):
        out.append("### " + crates[0] if len(crates) == 1 else f"### {crates[0]} and {len(crates) - 1} more")
        out.append("")
        if len(crates) > 1:
            out.append("Used by: " + ", ".join(crates) + ".\n")
        out.append("```text")
        out.append(text.replace("```", "'''"))
        out.append("```\n")
    result = "\n".join(out)
    if args.output:
        with open(args.output, "w", encoding="utf-8") as f:
            f.write(result)
        print(f"{args.output}: {len(rows)} crates, {len(texts)} distinct license texts", file=sys.stderr)
    else:
        sys.stdout.write(result)


if __name__ == "__main__":
    main()
