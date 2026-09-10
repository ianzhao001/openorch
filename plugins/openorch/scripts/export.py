#!/usr/bin/env python3
"""Export a fixed product snapshot without private history or local data.

This command creates local files only. Publication and ref ownership remain
separate maintainer actions; the exporter never initializes Git or pushes refs.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
from install import InstallError, absolute, check_path, read_file, require

EXACT_FILES = {".githooks/reference-transaction", "coordination/scripts/wake-multica.sh"}
EXCLUDED_PARTS = {"target", "node_modules", "__pycache__", ".git", ".orch", ".cowork-temp", ".worktrees"}
PUBLIC_DOCS = {"README.md": "plugins/openorch/PUBLIC-README.md",
               "CHANGELOG.md": "plugins/openorch/PUBLIC-CHANGELOG.md",
               "LICENSE": "plugins/openorch/LICENSE"}


def product_path(name):
    """Select explicit product roots, excluding generated runtime/cache data."""
    if name not in EXACT_FILES and not name.startswith(("orch/", "plugins/openorch/")):
        return False
    path = PurePosixPath(name)
    require(not path.is_absolute() and path.as_posix() == name and ".." not in path.parts
            and "\\" not in name, "Unsafe tracked product path: " + name)
    if name in EXACT_FILES:
        return True
    if any(part in EXCLUDED_PARTS for part in path.parts) or name.endswith((".pyc", ".pyo", ".DS_Store")):
        return False
    if name.startswith("plugins/openorch/runtime/"):
        return False
    return name.startswith("orch/") or name.startswith("plugins/openorch/")


def git(source, *args):
    """Read Git metadata using literal argv without changing source configuration."""
    result = subprocess.run(["git", "-C", str(source), *args], capture_output=True)
    require(result.returncode == 0, "Git inspection failed: " + result.stderr.decode("utf-8", errors="replace").strip())
    return result.stdout


def product_clean(source):
    """Reject staged or working product changes; unrelated local records may differ."""
    changed = git(source, "diff", "--name-only", "--no-renames", "-z", "HEAD", "--").split(b"\0")
    dirty = [os.fsdecode(name) for name in changed if name and product_path(os.fsdecode(name))]
    require(not dirty, "Commit product changes before export: " + ", ".join(dirty))


def tracked_products(source, head):
    """Select fixed-HEAD blobs, rejecting product symlinks and submodules."""
    entries = {}
    for row in git(source, "ls-tree", "-r", "-z", head).split(b"\0"):
        if not row:
            continue
        metadata, raw_name = row.split(b"\t", 1)
        mode, kind, object_id = metadata.decode("ascii").split()
        name = os.fsdecode(raw_name)
        if not product_path(name):
            continue
        require(kind == "blob" and mode in ("100644", "100755"), "Product symlink/submodule/special mode is not exportable: " + name)
        entries[name] = (mode, object_id)
    require(entries, "No tracked product files were found.")
    require(set(PUBLIC_DOCS.values()) <= set(entries), "Tracked PUBLIC-README.md, PUBLIC-CHANGELOG.md and plugin LICENSE are required.")
    return entries


def blob_identity(data, length):
    """Hash raw bytes as a Git blob for SHA-1 or SHA-256 repositories."""
    algorithm = hashlib.sha256 if length == 64 else hashlib.sha1
    return algorithm(b"blob " + str(len(data)).encode("ascii") + b"\0" + data).hexdigest()


def snapshot(source):
    """Validate every selected byte before creating any export directory."""
    check_path(source, directory=True)
    actual_root = absolute(os.fsdecode(git(source, "rev-parse", "--show-toplevel")).strip())
    require(actual_root == source, "--source must name the Git repository root.")
    head = git(source, "rev-parse", "--verify", "HEAD^{commit}").decode("ascii").strip()
    product_clean(source)
    selected = tracked_products(source, head)
    files = {}
    for name, (mode, object_id) in selected.items():
        path = source / name
        data = read_file(path)
        require(blob_identity(data, len(object_id)) == object_id, "Product bytes differ from fixed HEAD: " + name)
        require(bool(path.stat().st_mode & 0o111) == (mode == "100755"), "Product executable mode differs from HEAD: " + name)
        files[name] = (data, 0o755 if mode == "100755" else 0o644)
    require(git(source, "rev-parse", "HEAD").decode("ascii").strip() == head, "Source HEAD changed during export; retry from a fixed tree.")
    product_clean(source)
    for destination, origin in PUBLIC_DOCS.items():
        files[destination] = (files[origin][0], 0o644)
    return head, files


def empty_output(output):
    """Require a missing or empty ordinary destination, preserving foreign contents."""
    check_path(output, directory=True)
    require(not output.exists() or not any(output.iterdir()), "Output must be a new empty directory; existing contents are preserved: " + str(output))


def export(source, output):
    """Publish a validated local snapshot and a relative-path SHA-256 inventory."""
    empty_output(output)
    head, files = snapshot(source)
    inventory = {name: hashlib.sha256(data).hexdigest() for name, (data, _) in sorted(files.items())}
    metadata = (json.dumps({"schemaVersion": 1, "files": inventory}, ensure_ascii=False, indent=2) + "\n").encode("utf-8")
    check_path(output.parent, directory=True)
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".openorch-export-", dir=output.parent) as temporary:
        staging = Path(temporary) / "snapshot"
        staging.mkdir()
        for name, (data, mode) in files.items():
            path = staging / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
            path.chmod(mode)
        (staging / "SOURCE-MANIFEST.json").write_bytes(metadata)
        empty_output(output)
        staging.rename(output)
    return {"sourceCommit": head, "output": str(output), "files": len(files),
            "sourceManifestSha256": hashlib.sha256(metadata).hexdigest(), "published": False}


def main():
    """Export only local product artifacts and report actionable refusal as exit 2."""
    parser = argparse.ArgumentParser(description="Export tracked OpenOrch product source and public docs; omit private history/configuration/runtime data.")
    parser.add_argument("--source", required=True, help="Git repository root with committed product files")
    parser.add_argument("--output", required=True, help="New empty export directory with no symlink path components")
    args = parser.parse_args()
    try:
        print(json.dumps(export(absolute(args.source), absolute(args.output)), ensure_ascii=False, indent=2))
    except (InstallError, OSError, ValueError, TypeError, KeyError) as error:
        print("OpenOrch export: " + str(error), file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    main()
