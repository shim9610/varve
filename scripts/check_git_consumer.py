#!/usr/bin/env python3
"""Build a standalone Git consumer and verify it inherits the fixed Crossbeam sources."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=ROOT)
    args = parser.parse_args()
    repository = args.repository.resolve()
    revision = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repository, text=True
    ).strip()
    manifest = tomllib.loads(subprocess.check_output(
        ["git", "show", f"{revision}:Cargo.toml"], cwd=repository, text=True
    ))
    pin = manifest["workspace"]["dependencies"]["crossbeam-skiplist"]
    expected_source = f"git+{pin['git']}?rev={pin['rev']}#{pin['rev']}"
    expected_crates = {
        "crossbeam-skiplist", "crossbeam-epoch", "crossbeam-utils"
    }
    with tempfile.TemporaryDirectory(prefix="varve-git-consumer-") as directory:
        consumer = Path(directory)
        (consumer / "src").mkdir()
        shutil.copyfile(ROOT / "tools/public-api-fixture/src/main.rs", consumer / "src/main.rs")
        (consumer / "Cargo.toml").write_text(
            '[package]\nname = "varve-git-consumer"\nversion = "0.0.0"\n'
            'edition = "2024"\npublish = false\n\n[dependencies]\n'
            f'varve = {{ git = {json.dumps(repository.as_uri())}, '
            f'rev = "{revision}", features = ["compression-zstd", "integrity"] }}\n'
            '\n[workspace]\n', encoding="utf-8"
        )
        # There is deliberately no root [patch] in this independent project.
        metadata = json.loads(subprocess.check_output(
            ["cargo", "metadata", "--format-version=1"], cwd=consumer
        ))
        seen = set()
        for package in metadata["packages"]:
            if package["name"] in expected_crates:
                if package["source"] != expected_source:
                    raise SystemExit(f"Unexpected Crossbeam source: {package['id']}")
                seen.add(package["name"])
            if package["name"] == "redb":
                raise SystemExit("redb entered the downstream runtime graph")
        if seen != expected_crates:
            raise SystemExit(f"Missing Crossbeam packages: {expected_crates - seen}")
        print(f"Git consumer inherits Crossbeam revision {pin['rev']}", flush=True)
        subprocess.run(["cargo", "run", "--locked"], cwd=consumer, check=True)


if __name__ == "__main__":
    main()
