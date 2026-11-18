"""Fails when python/Cargo.lock resolves a crate to a version the workspace's Cargo.lock does not.

The Python binding is built outside the workspace, with its own lockfile. Both should build
tripletrail from the same dependency versions; run `cargo update --manifest-path python/Cargo.toml -w`
after changing the workspace's dependencies.
"""

import sys
import tomllib


def versions(path):
    found = {}
    with open(path, "rb") as f:
        for package in tomllib.load(f)["package"]:
            if package.get("source", "").startswith("registry"):
                found.setdefault(package["name"], set()).add(package["version"])
    return found


workspace = versions("Cargo.lock")
python = versions("python/Cargo.lock")
drift = sorted(
    (name, sorted(python[name] - workspace[name]), sorted(workspace[name]))
    for name in python.keys() & workspace.keys()
    if python[name] - workspace[name]
)
for name, only_python, in_workspace in drift:
    print(f"{name}: python/Cargo.lock has {', '.join(only_python)}, Cargo.lock {', '.join(in_workspace)}")
sys.exit(1 if drift else 0)
