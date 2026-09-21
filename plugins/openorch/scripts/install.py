#!/usr/bin/env python3
"""Install verified OpenOrch releases using native Codex and DSH registrations.

Personal defaults, project configuration, evidence and old payloads are retained.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import platform
import re
import shutil
import stat
import subprocess
import sys
import tempfile

NAME = "openorch"
SELECTOR = "openorch@openorch"
PLUGIN = "plugins/openorch/"
ASSETS = ("orch", "orch-mcp", "orch-acp", "scripts/wake-multica.sh", "scripts/wake-dsh-stream.sh",
          "scripts/wake-pi-stream.sh", "scripts/wake-zcode-stream.sh")
SEMVER = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9][0-9]*|[0-9]*[A-Za-z-][0-9A-Za-z-]*))*)?(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?")


class InstallError(Exception):
    """An installation failure whose existing state is preserved for recovery."""


def require(condition, message):
    """Stop with a concrete diagnostic when an installation requirement fails."""
    if not condition:
        raise InstallError(message)


def absolute(value):
    """Expand a local path without following symlinks before validation."""
    return Path(os.path.abspath(os.path.expanduser(str(value))))


def check_path(path, directory=False):
    """Reject symlinks and nonordinary existing path components."""
    path = absolute(path)
    current = Path(path.anchor)
    for part in path.parts[1:]:
        current /= part
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError:
            continue
        require(not stat.S_ISLNK(mode), "Symlink path is not accepted: " + str(current))
        is_dir = current != path or directory
        require(stat.S_ISDIR(mode) if is_dir else stat.S_ISREG(mode),
                "Expected ordinary " + ("directory: " if is_dir else "file: ") + str(current))
    return path


def read_file(path):
    """Read an ordinary file through a no-follow descriptor."""
    path = check_path(path)
    fd = os.open(str(path), os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        require(stat.S_ISREG(os.fstat(source.fileno()).st_mode), "Not a regular file: " + str(path))
        return source.read()


def read_json(path):
    """Decode JSON from an ordinary file and propagate malformed input."""
    return json.loads(read_file(path))


def digest(data):
    """Describe exact file bytes using the release inventory format."""
    return {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}


def files_under(root):
    """Enumerate all regular files, rejecting symlink directories and special files."""
    root = check_path(root, directory=True)
    require(root.is_dir(), "Directory does not exist: " + str(root))
    result = set()
    for parent, directories, files in os.walk(root, followlinks=False):
        for name in directories:
            check_path(Path(parent) / name, directory=True)
        for name in files:
            path = check_path(Path(parent) / name)
            result.add(path.relative_to(root).as_posix())
    return result


def verify_bundle(root, legacy_runtime=False):
    """Verify the complete inventory, native manifests and five runtime resources."""
    root = check_path(root, directory=True)
    manifest = read_json(root / "manifest.json")
    require(isinstance(manifest, dict) and manifest.get("schemaVersion") == 1
            and manifest.get("name") == NAME, "Unsupported OpenOrch inventory.")
    version = manifest.get("version")
    require(isinstance(version, str) and SEMVER.fullmatch(version), "Version must be strict semver.")
    files = manifest.get("files")
    require(isinstance(files, dict) and files, "Missing full release inventory.")
    for name, entry in files.items():
        path = PurePosixPath(name)
        require(not path.is_absolute() and path.as_posix() == name and ".." not in path.parts
                and "\\" not in name and name != ".", "Unsafe inventory path: " + name)
        require(isinstance(entry, dict) and set(entry) == {"sha256", "bytes"}, "Invalid inventory entry: " + name)
        require(digest(read_file(root / name)) == entry, "Release file missing or changed: " + name)
    require(files_under(root) == set(files) | {"manifest.json"}, "Release has unlisted files or an incomplete inventory.")
    essential = {"install.py", ".agents/plugins/marketplace.json", PLUGIN + ".codex-plugin/plugin.json",
                 PLUGIN + "package.json", PLUGIN + "dsh-entry.mjs", PLUGIN + "cordis.patch.yml",
                 PLUGIN + "skills/openorch/SKILL.md", PLUGIN + "scripts/openorch.py",
                 PLUGIN + "runtime/manifest.json", PLUGIN + "LICENSE"}
    require(essential <= set(files), "Inventory omits a required plugin resource.")
    for name in (".codex-plugin/plugin.json", "package.json"):
        value = read_json(root / PLUGIN / name)
        require(value.get("name") == NAME and value.get("version") == version, "Plugin name/version mismatch: " + name)
    package = read_json(root / PLUGIN / "package.json")
    require(package.get("type") == "module" and package.get("dsh", {}).get("bundle", {}).get("patch") == "./cordis.patch.yml",
            "Missing native DSH ESM bundle declaration.")
    catalog = read_json(root / ".agents/plugins/marketplace.json")
    entries = catalog.get("plugins", [])
    require(catalog.get("name") == NAME and len(entries) == 1 and entries[0].get("name") == NAME
            and entries[0].get("source") == {"source": "local", "path": "./plugins/openorch"}
            and entries[0].get("policy") == {"installation": "AVAILABLE", "authentication": "ON_INSTALL"},
            "Marketplace must contain only its local OpenOrch plugin.")
    runtime = read_json(root / PLUGIN / "runtime/manifest.json")
    legacy = set(ASSETS) - {"orch-mcp", "orch-acp"}
    actual = set(runtime.get("files", {}))
    accepted = actual == set(ASSETS) or (legacy_runtime and actual == legacy)
    require(runtime.get("version") == 1 and accepted, "Incomplete runtime inventory.")
    if actual == legacy:
        descriptor = read_json(root / PLUGIN / ".codex-plugin/plugin.json")
        new_resources = {PLUGIN + ".mcp.json", PLUGIN + "scripts/openorch-mcp.py", PLUGIN + "runtime/orch-mcp", PLUGIN + "runtime/orch-acp"}
        require("mcpServers" not in descriptor and not new_resources.intersection(files), "Legacy runtime cannot contain MCP declarations or protocol resources.")
    if actual == set(ASSETS):
        descriptor = read_json(root / PLUGIN / ".codex-plugin/plugin.json")
        require(descriptor.get("mcpServers") == "./.mcp.json", "Missing MCP companion declaration.")
        mcp = read_json(root / PLUGIN / ".mcp.json")
        require(mcp == {"mcpServers": {"openorch": {"command": "./scripts/openorch-mcp.py", "cwd": "."}}}, "Unexpected MCP startup declaration.")
        require(PLUGIN + "scripts/openorch-mcp.py" in files and os.access(root / PLUGIN / "scripts/openorch-mcp.py", os.X_OK), "Missing executable MCP launcher.")
    for name in actual:
        require(os.access(root / PLUGIN / "runtime" / name, os.X_OK), "Runtime resource is not executable: " + name)
        require(digest(read_file(root / PLUGIN / "runtime" / name)) == runtime["files"][name], "Runtime resource changed: " + name)
    return manifest


def command(binary, *args, json_output=False, timeout=300):
    """Run a bounded native command with literal argv and return its real result."""
    result = subprocess.run([str(binary), *map(str, args)], capture_output=True, text=True, timeout=timeout)
    require(result.returncode == 0, "Native command failed: " + str(binary) + " " + " ".join(map(str, args))
            + "\n" + (result.stderr or result.stdout).strip())
    return json.loads(result.stdout) if json_output else result.stdout


def native_state(host, binary):
    """Read Codex registrations or the DSH Web profile's real dependency state."""
    if host == "codex":
        markets = command(binary, "plugin", "marketplace", "list", "--json", json_output=True)
        plugins = command(binary, "plugin", "list", "--json", json_output=True)
        require(isinstance(markets.get("marketplaces"), list) and isinstance(plugins.get("installed"), list), "Unexpected Codex plugin listing.")
        selected = [x for x in markets["marketplaces"] if x.get("name") == NAME]
        installed = [x for x in plugins["installed"] if x.get("name") == NAME or x.get("pluginId", "").split("@")[0] == NAME]
        require(len(selected) <= 1 and len(installed) <= 1, "Ambiguous Codex OpenOrch registrations.")
        return {"market": selected[0] if selected else None, "plugin": installed[0] if installed else None}
    rows = command(binary, "plugin", "--profile", "web", "list", "--json", json_output=True)
    require(isinstance(rows, list) and len(rows) == 1 and isinstance(rows[0].get("path"), str), "Unexpected DSH Web profile listing.")
    profile = absolute(rows[0]["path"])
    manifest = read_json(profile / "package.json")
    return {"profile": profile, "source": manifest.get("dependencies", {}).get(NAME),
            "bundles": manifest.get("dsh", {}).get("profile", {}).get("bundles", []),
            "plugin": rows[0].get("dependencies", {}).get(NAME)}


def source_path(state):
    """Resolve a native file/link dependency relative to its DSH profile."""
    spec = state["source"]
    require(isinstance(spec, str) and spec.startswith(("file:", "link:")), "Foreign DSH OpenOrch dependency; resolve it explicitly.")
    value = Path(spec.split(":", 1)[1])
    return absolute(value if value.is_absolute() else state["profile"] / value)


def owned_root(path, prefix, plugin=False):
    """Require an intact versioned release under this installer prefix."""
    path = absolute(path)
    root = path.parents[1] if plugin else path
    require(not plugin or path == root / "plugins/openorch", "Unknown plugin path: " + str(path))
    relative = root.relative_to(prefix / "versions") if root.is_relative_to(prefix / "versions") else None
    require(relative is not None and len(relative.parts) == 1, "Foreign OpenOrch registration: " + str(path))
    value = verify_bundle(root, legacy_runtime=True)
    require(value["version"] == root.name, "Version directory and inventory disagree: " + str(root))
    return root


def preflight_registration(host, state, prefix):
    """Reject every selected host's foreign collision before native registration writes."""
    if host == "codex":
        market, plugin = state["market"], state["plugin"]
        if market:
            require(market.get("marketplaceSource", {}).get("sourceType", "local") == "local", "OpenOrch marketplace is not local.")
            root = owned_root(market["root"], prefix)
            if plugin:
                require(plugin.get("pluginId") == SELECTOR and plugin.get("source", {}).get("source") == "local"
                        and absolute(plugin["source"]["path"]) == root / "plugins/openorch", "Foreign Codex OpenOrch plugin.")
        else:
            require(plugin is None, "OpenOrch plugin lacks an owned marketplace.")
    elif state["source"] is not None:
        owned_root(source_path(state), prefix, plugin=True)
    else:
        require(NAME not in state["bundles"] and state["plugin"] is None, "Unknown DSH OpenOrch bundle; resolve it explicitly.")


def verify_registration(host, state, payload, manifest):
    """Verify native references and installed DSH resource bytes after registration."""
    if host == "codex":
        market, plugin = state["market"], state["plugin"]
        require(market is not None and absolute(market["root"]) == payload and plugin is not None
                and plugin.get("pluginId") == SELECTOR and plugin.get("version") == manifest["version"]
                and plugin.get("installed") is True and plugin.get("enabled") is True
                and absolute(plugin.get("source", {}).get("path", "")) == payload / "plugins/openorch", "Codex did not confirm the enabled plugin version.")
    else:
        require(state["source"] is not None and state["source"].startswith("file:")
                and source_path(state) == payload / "plugins/openorch" and NAME in state["bundles"]
                and isinstance(state["plugin"], dict) and state["plugin"].get("path"), "DSH did not confirm a materialized OpenOrch bundle.")
        installed = Path(state["plugin"]["path"]).resolve()
        if not (installed / "package.json").is_file():
            # Hoisted pnpm layouts can list a virtual-store path that is absent.
            # Resolve the package as Node does from the native profile instead.
            node = shutil.which("node")
            require(node, "DSH listed a virtual package path; Node.js is needed to resolve its installed files.")
            resolved = command(node, "-e", "process.stdout.write(require.resolve('openorch/package.json', {paths: [process.argv[1]]}));", state["profile"], timeout=30)
            installed = Path(resolved).resolve().parent
        for name, entry in manifest["files"].items():
            if name.startswith(PLUGIN):
                require(digest(read_file(installed / name[len(PLUGIN):])) == entry, "DSH installed resource differs: " + name)


@contextlib.contextmanager
def install_lock(prefix):
    """Serialize this prefix's installers, including native registration updates."""
    check_path(prefix, directory=True)
    prefix.mkdir(parents=True, exist_ok=True, mode=0o700)
    fd = os.open(str(check_path(prefix / ".install.lock")), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        require(stat.S_ISREG(os.fstat(fd).st_mode), "Installer lock is not a regular file.")
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def materialize(bundle, prefix, manifest):
    """Publish an immutable version, refusing different bytes at the same version."""
    parent = check_path(prefix / "versions", directory=True)
    parent.mkdir(exist_ok=True, mode=0o700)
    payload = check_path(parent / manifest["version"], directory=True)
    if payload.exists():
        require(verify_bundle(payload) == manifest, "Same version has different bytes; use a new version or another prefix.")
        return payload
    require(not prefix.is_relative_to(bundle), "Install prefix must be outside the bundle.")
    with tempfile.TemporaryDirectory(prefix=".openorch-stage-", dir=parent) as temporary:
        staged = Path(temporary) / "payload"
        shutil.copytree(bundle, staged)
        require(verify_bundle(staged) == manifest, "Copied release did not verify.")
        require(not payload.exists(), "Version appeared during installation; rerun to inspect it.")
        staged.rename(payload)
    return payload


def register(host, binary, state, payload, manifest):
    """Register natively, normalizing DSH directory links into dependency-bearing file packages."""
    if host == "codex":
        if state["market"] and absolute(state["market"]["root"]) != payload:
            if state["plugin"]:
                command(binary, "plugin", "remove", SELECTOR)
            command(binary, "plugin", "marketplace", "remove", NAME)
            state = {"market": None, "plugin": None}
        if not state["market"]:
            command(binary, "plugin", "marketplace", "add", payload)
        plugin = state["plugin"]
        if not plugin or not plugin.get("enabled") or plugin.get("version") != manifest["version"]:
            if plugin:
                command(binary, "plugin", "remove", SELECTOR)
            command(binary, "plugin", "add", SELECTOR)
    else:
        wanted = payload / "plugins/openorch"
        if state["source"] is None or source_path(state) != wanted or not state["plugin"] or NAME not in state["bundles"]:
            command(binary, "plugin", "--profile", "web", "add", wanted)
            state = native_state(host, binary)
        if state["source"] is not None and state["source"].startswith("link:"):
            command(binary, "plugin", "--profile", "web", "add", "file:" + str(wanted))
    verify_registration(host, native_state(host, binary), payload, manifest)


def unregister(host, binary, state):
    """Remove only owned registrations, retaining every payload and user file."""
    if host == "codex":
        if state["plugin"]:
            command(binary, "plugin", "remove", SELECTOR)
        if state["market"]:
            command(binary, "plugin", "marketplace", "remove", NAME)
        after = native_state(host, binary)
        require(after["plugin"] is None and after["market"] is None, "Codex still reports OpenOrch registered.")
    else:
        if state["source"] is not None:
            command(binary, "plugin", "--profile", "web", "remove", NAME)
        after = native_state(host, binary)
        require(after["source"] is None and after["plugin"] is None and NAME not in after["bundles"], "DSH still reports OpenOrch registered.")


def main():
    """Preflight all hosts, preserve partial outcomes, and print verified registrations."""
    parser = argparse.ArgumentParser(description="Install/remove OpenOrch native plugins on Apple Silicon macOS; preserve user configuration.")
    parser.add_argument("--bundle", default=str(Path(__file__).resolve().parent), help="Complete release root (default: this installer directory)")
    parser.add_argument("--prefix", default=str(Path.home() / ".local/share/openorch"), help="Owned versions prefix, default ~/.local/share/openorch")
    parser.add_argument("--host", choices=("codex", "dsh", "both"), required=True)
    parser.add_argument("--codex-bin", help="Native Codex executable, otherwise discovered on PATH")
    parser.add_argument("--dsh-bin", help="Native DSH executable, otherwise discovered on PATH")
    parser.add_argument("--uninstall", action="store_true", help="Remove selected registrations only; retain payloads and configuration")
    args = parser.parse_args()
    completed = []
    try:
        require(sys.version_info >= (3, 9), "Python 3.9 or newer is required.")
        bundle, prefix = absolute(args.bundle), absolute(args.prefix)
        manifest = verify_bundle(bundle)
        require(not prefix.is_relative_to(bundle), "Install prefix must be outside the bundle.")
        require(platform.system() == "Darwin" and platform.machine() == "arm64"
                and manifest.get("platform") == "darwin-arm64", "This release requires Apple Silicon macOS and a darwin-arm64 bundle.")
        binary = bundle / PLUGIN / "runtime/orch"
        require(os.access(binary, os.X_OK), "Bundled orch is not executable; re-extract the release.")
        require(re.search(r"\bcommands=6\b", command(binary, "guide", "--check", timeout=30)), "Plugin requires the default six-command runtime.")
        hosts = ["codex", "dsh"] if args.host == "both" else [args.host]
        bins = {host: getattr(args, host + "_bin") or shutil.which(host) for host in hosts}
        for host, value in bins.items():
            require(value and os.access(value, os.X_OK), "Install native " + host + " or provide --" + host + "-bin.")
        with install_lock(prefix):
            states = {host: native_state(host, bins[host]) for host in hosts}
            for host in hosts:
                preflight_registration(host, states[host], prefix)
            payload = None if args.uninstall else materialize(bundle, prefix, manifest)
            for host in hosts:
                if args.uninstall:
                    unregister(host, bins[host], states[host])
                else:
                    register(host, bins[host], states[host], payload, manifest)
                record = {"host": host, "status": "unregistered" if args.uninstall else "registered",
                          "version": manifest["version"], "payload": str(payload) if payload else None,
                          "userConfigurationPreserved": True, "skillConsumptionVerified": False}
                completed.append(record)
                print(json.dumps(record, ensure_ascii=False), flush=True)
        print(json.dumps({"complete": True, "hosts": completed,
                          "next": "Payloads/configuration retained." if args.uninstall else "Open a new Codex task / DSH Web session and explicitly ask to use OpenOrch."}, ensure_ascii=False))
    except (InstallError, OSError, ValueError, TypeError, KeyError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"complete": False, "completedHosts": completed, "error": str(error),
                          "recovery": "Payloads are retained. Resolve the reported conflict/dependency and rerun for the selected host."}, ensure_ascii=False), file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    main()
