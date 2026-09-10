#!/usr/bin/env python3
"""Package one plugin source and an explicitly supplied default runtime.

The runtime's build provenance is recorded by the release builder. Packaging
validates bytes and command surface; it does not attest publisher identity.
"""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tarfile
import tempfile

sys.dont_write_bytecode = True
from install import (ASSETS, PLUGIN, SEMVER, InstallError, absolute, check_path,
                     command, digest, files_under, read_file, require, verify_bundle)

SOURCE_FILES = (".codex-plugin/plugin.json", "package.json", "cordis.patch.yml",
                "dsh-entry.mjs", "skills/openorch/SKILL.md", "scripts/openorch.py",
                "scripts/install.py", "scripts/package.py", "HELPER.md", "README.md",
                "LICENSE", "catalog.json")


def json_bytes(value):
    """Serialize portable release metadata without machine-specific paths."""
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def collect(source, runtime, version):
    """Read all selected source/runtime bytes and check the native core before publishing."""
    require(SEMVER.fullmatch(version), "Version must be strict semver.")
    selected = {PLUGIN + name: read_file(source / name) for name in SOURCE_FILES}
    for name in (".codex-plugin/plugin.json", "package.json"):
        value = json.loads(selected[PLUGIN + name])
        require(value.get("name") == "openorch", "Source plugin name must be openorch.")
        value["version"] = version
        selected[PLUGIN + name] = json_bytes(value)
    binary = runtime / "orch"
    require(os.access(binary, os.X_OK), "Supplied runtime/orch must be executable.")
    resources = {name: read_file(runtime / name) for name in ASSETS}
    require(re.search(r"\bcommands=6\b", command(binary, "guide", "--check", timeout=30)), "Supply the default six-command core, not a selfhost build.")
    require(command(binary, "--version", timeout=30).strip() == "orch 0.1.0", "Expected orch 0.1.0 runtime.")
    for name, data in resources.items():
        selected[PLUGIN + "runtime/" + name] = data
    selected[PLUGIN + "runtime/manifest.json"] = json_bytes({"version": 1, "files": {name: digest(data) for name, data in resources.items()}})
    for name in ("LICENSE", "provenance.json"):
        path = runtime / name
        if path.exists() or path.is_symlink():
            data = read_file(path)
            if name.endswith(".json"):
                json.loads(data)
            selected[PLUGIN + "runtime/" + name] = data
    selected[".agents/plugins/marketplace.json"] = selected[PLUGIN + "catalog.json"]
    selected["install.py"] = selected[PLUGIN + "scripts/install.py"]
    selected["README.md"] = selected[PLUGIN + "README.md"]
    selected["LICENSE"] = selected[PLUGIN + "LICENSE"]
    return selected


def build(source, runtime, output, version):
    """Write a complete verified distribution and adjacent archive, refusing existing outputs."""
    check_path(runtime, directory=True)
    check_path(output, directory=True)
    archive = check_path(Path(str(output) + ".tar.gz"))
    require(not output.exists() and not archive.exists(), "Choose a new output directory and archive name.")
    selected = collect(source, runtime, version)
    check_path(output.parent, directory=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".openorch-package-", dir=output.parent) as temporary:
        staging = Path(temporary) / "bundle"
        staging.mkdir()
        for name, data in selected.items():
            path = staging / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(0o755 if name.endswith((".py", ".sh")) or name == PLUGIN + "runtime/orch" else 0o644)
        manifest = {"schemaVersion": 1, "name": "openorch", "version": version, "platform": "darwin-arm64",
                    "runtimeVersion": "0.1.0", "files": {name: digest(data) for name, data in sorted(selected.items())}}
        (staging / "manifest.json").write_bytes(json_bytes(manifest))
        verify_bundle(staging)
        packed = Path(temporary) / "bundle.tar.gz"
        with tarfile.open(packed, "w:gz") as tar:
            for name in sorted(files_under(staging)):
                path = staging / name
                info = tar.gettarinfo(str(path), arcname=output.name + "/" + name)
                info.uid = info.gid = 0
                info.uname = info.gname = ""
                with path.open("rb") as data:
                    tar.addfile(info, data)
        require(not output.exists() and not archive.exists(), "Output appeared during packaging; choose a new path.")
        staging.rename(output)
        packed.rename(archive)
    return {"bundle": str(output), "archive": str(archive), "version": version,
            "files": len(manifest["files"]), "archiveIntegrity": digest(read_file(archive))}


def main():
    """Build from explicit source/runtime inputs and report validation failures as exit 2."""
    parser = argparse.ArgumentParser(description="Build a self-contained OpenOrch release directory and .tar.gz archive.")
    parser.add_argument("--runtime-dir", required=True, help="Directory containing default orch and four scripts/ resources")
    parser.add_argument("--output", required=True, help="New distribution directory; its adjacent .tar.gz must also be absent")
    parser.add_argument("--version", required=True, help="Strict plugin semver; core remains orch 0.1.0")
    args = parser.parse_args()
    try:
        result = build(Path(__file__).resolve().parents[1], absolute(args.runtime_dir), absolute(args.output), args.version)
        print(json.dumps(result, ensure_ascii=False, indent=2))
    except (InstallError, OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        print("OpenOrch package: " + str(error), file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    main()
