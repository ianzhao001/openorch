#!/usr/bin/env python3
"""Verified compatibility launcher. All profile/configuration semantics live in Rust."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
sys.dont_write_bytecode = True
from install import ASSETS, InstallError, check_path, read_file
PACKAGE = Path(__file__).resolve().parents[1]
SetupError = InstallError

def fail(message):
    raise SetupError(message)

def capture(args, cwd=None):
    result = subprocess.run([str(x) for x in args], cwd=cwd, capture_output=True, text=True)
    if result.returncode:
        fail((result.stderr or result.stdout).strip() or "Runtime inspection failed.")
    return result.stdout

def file_identity(path):
    """Hash a stable ordinary executable in bounded memory."""
    path = check_path(path)
    before = path.stat()
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        opened = os.fstat(source.fileno())
        if not stat.S_ISREG(opened.st_mode) or not opened.st_mode & 0o111:
            fail("Runtime resource is not an executable regular file: " + str(path))
        if (before.st_dev, before.st_ino) != (opened.st_dev, opened.st_ino):
            fail("Runtime identity changed.")
        sha = hashlib.sha256()
        size = 0
        for block in iter(lambda: source.read(1024 * 1024), b""):
            size += len(block)
            sha.update(block)
        after = os.fstat(source.fileno())
    current = path.lstat()
    identity = lambda m: (m.st_dev, m.st_ino, m.st_size, m.st_mtime_ns, m.st_ctime_ns)
    if stat.S_ISLNK(current.st_mode) or identity(opened) != identity(after) or identity(after) != identity(current) or size != after.st_size:
        fail("Runtime resource changed during verification.")
    return {"sha256": sha.hexdigest(), "bytes": size}

def runtime():
    """Verify the complete shared inventory and all three code-owned binary identities."""
    directory = PACKAGE / "runtime"
    check_path(directory, directory=True)
    try:
        manifest = json.loads(read_file(directory / "manifest.json"))
    except FileNotFoundError:
        fail("安装包缺少运行时；请重新安装完整 OpenOrch Release。")
    if (not isinstance(manifest, dict) or type(manifest.get("version")) is not int or manifest["version"] != 1
            or not isinstance(manifest.get("files"), dict) or set(manifest["files"]) != set(ASSETS)):
        fail("Incomplete runtime inventory.")
    for name in ASSETS:
        if file_identity(directory / name) != manifest["files"][name]:
            fail("Runtime resource missing or modified: " + name)
    for name in ("orch", "orch-mcp", "orch-acp"):
        if capture([directory / name, "--version"], cwd=PACKAGE).strip() != name + " 0.1.0":
            fail("Unsupported runtime identity: " + name)
    binary = directory / "orch"
    if not re.search(r"\bcommands=6\b", capture([binary, "guide", "--check"], cwd=PACKAGE)):
        fail("Plugin requires the default six-command core.")
    return binary

def parser():
    """Keep the established helper flags; Rust owns their setup and invocation semantics."""
    result = argparse.ArgumentParser(description="OpenOrch：配置一次，在 Git 项目中调用已支持的咨询通道。")
    result.add_argument("--config-dir", help="个人配置绝对目录（默认 ~/.config/openorch）")
    sub = result.add_subparsers(dest="action", required=True)
    for name in ("discover", "configure", "attach", "run", "doctor"):
        item = sub.add_parser(name, help={"discover": "发现已安装通道，不调用模型", "configure": "验证并保存明确选择的个人配置", "attach": "仅初始化缺失的项目配置", "run": "以单席或多席方式咨询", "doctor": "核对运行时和当前项目配置"}[name])
        item.add_argument("--project", default=os.getcwd(), help="已有首提交的 Git 项目或工作树")
        if name == "configure":
            item.add_argument("--input", required=True, help="version/harnesses/defaults JSON 文件")
            item.add_argument("--replace-profile", action="store_true", help="明确替换个人配置，已有项目不自动改变")
        if name == "run":
            item.add_argument("--mode", choices=("single", "fusion"), required=True)
            item.add_argument("--question-file", required=True, help="UTF-8 问题文件")
            item.add_argument("--harness", action="append", help="本次成员，可重复指定不同别名；覆盖默认")
            item.add_argument("--attach", action="append", help="交给核心核验的项目内文本附件")
    return result


def main():
    """Forward literal typed arguments to the shared Rust compatibility entry."""
    args = parser().parse_args()
    try:
        project = Path(args.project).expanduser().absolute()
        binary = runtime()
        argv = [str(binary), "--root", str(project), "__openorch"]
        if args.config_dir:
            argv += ["--config-dir", str(Path(args.config_dir).expanduser())]
        argv += [args.action]
        if args.action == "configure":
            argv += ["--input", str(Path(args.input).expanduser().absolute())]
            if args.replace_profile:
                argv += ["--replace-profile"]
        elif args.action == "run":
            argv += ["--mode", args.mode, "--question-file", str(Path(args.question_file).expanduser().absolute())]
            for alias in args.harness or []:
                argv += ["--harness", alias]
            for attachment in args.attach or []:
                argv += ["--attach", str(Path(attachment).expanduser())]
        os.chdir(project)
        os.execv(str(binary), argv)
    except (SetupError, OSError, ValueError, TypeError, KeyError) as error:
        print("OpenOrch: " + str(error), file=sys.stderr)
        raise SystemExit(2)

if __name__ == "__main__":
    main()
