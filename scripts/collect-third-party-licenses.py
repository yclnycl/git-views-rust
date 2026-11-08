#!/usr/bin/env python3
"""Collect license files shipped in the Cargo dependency archives for a target.

Run after `cargo fetch --locked` and `cargo build --locked` on the target runner.
This records available material, not a legal-completeness determination.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


NAME = re.compile(r"^(?:licen[cs]e|copying|notice|copyright)(?:$|[._-])", re.I)
ROOT = Path(__file__).resolve().parent.parent
SUPPLEMENT = ROOT / "crates/gitviews-app/assets/licenses/THIRD-PARTY-NOTICES.txt"


def collect(target: str, output: Path) -> int:
    if output.is_symlink() or (output.exists() and (not output.is_dir() or any(output.iterdir()))):
        raise ValueError(f"Output directory must be empty: {output}")
    command = ["cargo", "metadata", "--locked", "--offline", "--format-version", "1", "--filter-platform", target]
    metadata = json.loads(subprocess.run(command, cwd=ROOT, check=True, stdout=subprocess.PIPE).stdout)
    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    pending = list(metadata["workspace_members"])
    reachable = set()
    while pending:
        package_id = pending.pop()
        if package_id in reachable:
            continue
        if package_id not in nodes or package_id not in packages:
            raise ValueError(f"Cargo metadata omitted reachable package: {package_id}")
        reachable.add(package_id)
        pending.extend(nodes[package_id]["dependencies"])

    output.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(SUPPLEMENT, output / "SUPPLEMENTAL-WINDOWS-NOTICES.txt")
    supplemental_text = SUPPLEMENT.read_text(encoding="utf-8")
    entries = []
    for package in sorted((packages[item] for item in reachable), key=lambda item: item["id"]):
        if package["source"] is None:
            continue
        root = Path(package["manifest_path"]).parent.resolve(strict=True)
        safe_id = re.sub(r"[^A-Za-z0-9._-]", "_", package["name"] + "-" + package["version"])
        safe_id += "-" + hashlib.sha256(package["id"].encode("utf-8")).hexdigest()[:12]
        dest = output / "crates" / safe_id
        files = set()
        errors = []
        for current, dirs, names in os.walk(root, followlinks=False):
            dirs[:] = sorted(d for d in dirs if not (Path(current) / d).is_symlink())
            for name in sorted(names):
                path = Path(current) / name
                if NAME.match(name):
                    if path.is_symlink() or not path.is_file():
                        errors.append(f"Skipped non-regular or symbolic-link file: {path.relative_to(root).as_posix()}")
                    else:
                        files.add(path.relative_to(root))
        declared = package.get("license_file")
        declared_relative = None
        if declared:
            path = Path(declared)
            path = path if path.is_absolute() else root / path
            try:
                relative = path.resolve(strict=True).relative_to(root)
                if path.is_symlink() or not path.is_file():
                    raise ValueError("not a regular non-symlink file")
                files.add(relative)
                declared_relative = relative.as_posix()
            except (ValueError, OSError) as error:
                errors.append(f"Declared license_file is inaccessible or outside crate: {error}")
        for manifest in ("Cargo.toml", "Cargo.toml.orig", ".cargo_vcs_info.json"):
            path = root / manifest
            if path.is_file() and not path.is_symlink():
                files.add(Path(manifest))
        copied = []
        for relative in sorted(files):
            destination = dest / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(root / relative, destination)
            copied.append(relative.as_posix())
        license_files = [name for name in copied if NAME.match(Path(name).name) or name == declared_relative]
        marker = re.compile(r"^" + re.escape(package["name"] + " " + package["version"]) + r"\s+[—-]", re.M)
        entries.append({
            "name": package["name"], "version": package["version"], "source": package["source"],
            "license_expression": package.get("license"), "declared_license_file": declared and Path(declared).name,
            "archive_directory": "crates/" + safe_id, "copied_files": copied,
            "license_files": license_files, "supplemental_catalog_entry_found": bool(marker.search(supplemental_text)),
            "warnings": errors,
        })
    missing = [entry for entry in entries if not entry["license_files"]]
    inventory = {
        "target": target,
        "scope": "Reachable registry/git Cargo packages in target-filtered metadata, including build and dev dependencies; vendored non-Cargo code may require further review.",
        "supplement": "Windows-oriented supplemental notice; a matching catalog entry does not prove that full license terms are present.",
        "packages_without_license_file": [entry["name"] + " " + entry["version"] for entry in missing],
        "packages": entries,
    }
    (output / "inventory.json").write_text(json.dumps(inventory, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"Collected {len(entries)} crates; {len(missing)} have no license file in their crate archive.")
    problems = [entry for entry in entries if entry["warnings"]]
    unresolved = [entry for entry in missing if not entry["supplemental_catalog_entry_found"]]
    if missing:
        print("Warning: Some crate archives omit license files; see inventory.json. A supplemental catalog entry is not proof of complete license terms.", file=sys.stderr)
    if unresolved:
        print(f"Failed: {len(unresolved)} crates have neither a license file nor a supplemental catalog entry:", file=sys.stderr)
        for entry in unresolved:
            print(f"  {entry['name']} {entry['version']}: {entry['license_expression']}", file=sys.stderr)
    if problems:
        print(f"Failed to collect license files for {len(problems)} crates; see inventory.json.", file=sys.stderr)
    if problems or unresolved:
        return 1
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("target", help="Cargo target triple")
    parser.add_argument("output", type=Path, help="Empty output directory")
    args = parser.parse_args()
    try:
        return collect(args.target, args.output)
    except (OSError, ValueError, subprocess.CalledProcessError, json.JSONDecodeError) as error:
        print(f"License collection failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
