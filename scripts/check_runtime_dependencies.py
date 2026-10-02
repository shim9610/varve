#!/usr/bin/env python3
"""redb is retained as an oracle; it must never enter Varve's runtime graph."""
import json
from pathlib import Path
import subprocess

ROOT = Path(__file__).resolve().parents[1]
metadata = json.loads(subprocess.check_output([
    "cargo", "metadata", "--locked", "--format-version=1", "--all-features",
], cwd=ROOT))
packages = {p["id"]: p for p in metadata["packages"]}
nodes = {n["id"]: n for n in metadata["resolve"]["nodes"]}
pending = list(metadata["workspace_members"])
seen = set()
while pending:
    package = pending.pop()
    if package in seen:
        continue
    seen.add(package)
    if packages[package]["name"] == "redb":
        raise SystemExit("redb entered the production dependency graph")
    for dep in nodes[package]["deps"]:
        if any(kind["kind"] != "dev" for kind in dep["dep_kinds"]):
            pending.append(dep["pkg"])
if (ROOT / "crates/varve-core/src/redb").exists():
    raise SystemExit("embedded redb runtime sources remain")
print("Runtime dependency graph excludes redb; oracle dev-dependencies are permitted.")
