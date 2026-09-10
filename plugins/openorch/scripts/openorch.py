#!/usr/bin/env python3
"""Configure and invoke OpenOrch through a verified, package-local runtime.

This helper owns only personal defaults and local project setup. The installed
orch binary remains the authority for harness configuration and invocation.
"""
import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys
import tempfile
import uuid

PACKAGE = Path(__file__).resolve().parents[1]
ASSETS = ("orch", "scripts/wake-multica.sh", "scripts/wake-dsh-stream.sh",
          "scripts/wake-pi-stream.sh", "scripts/wake-zcode-stream.sh")
EXCLUDES = ("/.orch/", "/coordination/consultations/", "/.cowork-temp/channel-capture/")
SUPPORTED_NAMES = ("codex", "claude", "opencode", "cursor", "mimo", "codebuddy",
                   "smartclaw", "dsh", "pi", "agy", "zcode")


class SetupError(Exception):
    """A configuration or environment problem that needs a concrete user action."""


def fail(message):
    """Raise a readable error without launching a provider."""
    raise SetupError(message)


def safe_chain(path, leaf_directory=False):
    """Reject existing symlink components and incompatible file types."""
    path = Path(path)
    if not path.is_absolute():
        fail("需要绝对路径: " + str(path))
    current = Path(path.anchor)
    for index, part in enumerate(path.parts[1:]):
        current = current / part
        try:
            mode = current.lstat().st_mode
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(mode):
            fail("不接受符号链接路径: " + str(current))
        final = index == len(path.parts[1:]) - 1
        if not final or leaf_directory:
            if not stat.S_ISDIR(mode):
                fail("需要目录: " + str(current))
        elif not stat.S_ISREG(mode):
            fail("需要普通文件: " + str(current))


def ensure_dir(path):
    """Create an owned directory hierarchy while rejecting symlink components."""
    path = Path(path)
    safe_chain(path, leaf_directory=True)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    safe_chain(path, leaf_directory=True)
    return path


def read_regular(path):
    """Read one regular file through a no-follow descriptor."""
    path = Path(path)
    safe_chain(path)
    fd = os.open(str(path), os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            fail("需要普通文件: " + str(path))
        return source.read()


@contextlib.contextmanager
def locked(path):
    """Serialize short configuration operations; never hold a lock across a model call."""
    path = Path(path)
    ensure_dir(path.parent)
    safe_chain(path)
    fd = os.open(str(path), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(fd).st_mode):
            fail("锁文件不是普通文件: " + str(path))
        fcntl.flock(fd, fcntl.LOCK_EX)
        yield
    finally:
        os.close(fd)


def atomic_bytes(path, data, replace=False):
    """Publish complete private bytes atomically, refusing an existing file by default."""
    path = Path(path)
    ensure_dir(path.parent)
    safe_chain(path)
    temporary = path.parent / ("." + path.name + "." + uuid.uuid4().hex + ".tmp")
    fd = os.open(str(temporary), os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    try:
        with os.fdopen(fd, "wb") as output:
            output.write(data)
            output.flush()
            os.fsync(output.fileno())
        if replace:
            safe_chain(path)
            os.replace(str(temporary), str(path))
        else:
            os.link(str(temporary), str(path), follow_symlinks=False)
        directory = os.open(str(path.parent), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def encoded(value):
    """Encode helper-owned JSON consistently; JSON is valid core YAML v1 input."""
    return (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


def capture(args, cwd=None):
    """Run a local inspection command with literal argv, without a helper timeout."""
    result = subprocess.run([str(x) for x in args], cwd=cwd, capture_output=True, text=True)
    if result.returncode:
        detail = (result.stderr or result.stdout).strip()
        fail(detail or "命令失败: " + str(args[0]))
    return result.stdout


def git(project, *args):
    """Inspect Git metadata without modifying Git configuration."""
    executable = shutil.which("git")
    if not executable:
        fail("缺少 Git；请先安装 Git 并为项目建立首提交。")
    return capture([executable, *args], cwd=project).strip()


def project_root(value):
    """Resolve the user's actual worktree and require an existing commit."""
    supplied = Path(value or os.getcwd()).expanduser().resolve()
    if not supplied.is_dir():
        fail("项目目录不存在: " + str(supplied))
    root = Path(git(supplied, "rev-parse", "--show-toplevel")).resolve()
    git(root, "rev-parse", "--verify", "HEAD^{commit}")
    return root


def config_root(project):
    """Match the core's main-repository configuration location for linked worktrees."""
    common = Path(git(project, "rev-parse", "--git-common-dir"))
    if not common.is_absolute():
        common = project / common
    return common.resolve().parent


def runtime():
    """Verify all five package-local runtime resources and its six-command surface."""
    directory = PACKAGE / "runtime"
    safe_chain(directory, leaf_directory=True)
    try:
        manifest = json.loads(read_regular(directory / "manifest.json"))
    except FileNotFoundError:
        fail("安装包缺少运行时；请从 OpenOrch Release 重新安装完整插件。")
    if (not isinstance(manifest, dict) or type(manifest.get("version")) is not int
            or manifest["version"] != 1 or not isinstance(manifest.get("files"), dict)
            or set(manifest["files"]) != set(ASSETS)):
        fail("运行时完整性清单不完整或版本不支持。")
    for name in ASSETS:
        data = read_regular(directory / name)
        item = manifest["files"][name]
        if not isinstance(item, dict) or item.get("bytes") != len(data) or item.get("sha256") != hashlib.sha256(data).hexdigest():
            fail("运行时资源缺失或被修改: " + name + "；请重新安装完整版本。")
    binary = directory / "orch"
    if not os.access(binary, os.X_OK):
        fail("运行程序不可执行: " + str(binary))
    output = capture([binary, "guide", "--check"], cwd=PACKAGE)
    if not re.search(r"\bcommands=6\b", output):
        fail("插件需要默认六叶运行核心；当前运行程序不是该产品面。")
    return binary


def core(binary, project, *args):
    """Invoke only an existing core command against an explicit project."""
    return capture([binary, "--root", project, *args], cwd=project)


def add_excludes(project):
    """Append only OpenOrch-generated paths to Git's local exclusion file."""
    value = Path(git(project, "rev-parse", "--git-path", "info/exclude"))
    if not value.is_absolute():
        value = project / value
    path = value.absolute()
    ensure_dir(path.parent)
    safe_chain(path)
    fd = os.open(str(path), os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, "r+b") as output:
        fcntl.flock(output.fileno(), fcntl.LOCK_EX)
        data = output.read()
        lines = data.decode("utf-8").splitlines()
        missing = [line for line in EXCLUDES if line not in lines]
        if missing:
            suffix = ("\n" if data and not data.endswith(b"\n") else "")
            suffix += "# OpenOrch local configuration and invocation evidence\n"
            suffix += "\n".join(missing) + "\n"
            output.write(suffix.encode("utf-8"))
            output.flush()
            os.fsync(output.fileno())


def discovery_rows(output):
    """Read actual core action-discovery rows, keeping unsupported reasons visible."""
    rows = []
    for line in output.splitlines():
        values = line.split("\t", 3)
        if len(values) == 4 and values[2] in ("supported", "unsupported", "unknown"):
            rows.append(dict(zip(("alias", "driver", "status", "reason"), values)))
    return rows


def profile_shape(profile):
    """Validate helper-owned defaults while leaving native entry semantics to the core."""
    if not isinstance(profile, dict) or set(profile) != {"version", "harnesses", "defaults"}:
        fail("配置需包含且仅包含 version、harnesses、defaults。")
    if type(profile["version"]) is not int or profile["version"] != 1:
        fail("仅支持个人配置 version=1。")
    if not isinstance(profile["harnesses"], dict) or not profile["harnesses"]:
        fail("至少配置一个本机 harness。")
    defaults = profile["defaults"]
    if not isinstance(defaults, dict) or set(defaults) != {"single", "fusion"}:
        fail("defaults 需包含 single 和 fusion。")
    single, fusion = defaults["single"], defaults["fusion"]
    if not isinstance(single, str) or single not in profile["harnesses"]:
        fail("默认单席必须是已配置的别名。")
    if not isinstance(fusion, list) or any(not isinstance(x, str) for x in fusion):
        fail("默认 fusion 必须是别名数组。")
    if fusion and (not 2 <= len(fusion) <= 5 or len(set(fusion)) != len(fusion)):
        fail("默认 fusion 需为2–5个不同成员，或留空暂不设置。")
    if any(x not in profile["harnesses"] for x in fusion):
        fail("默认 fusion 含未配置成员。")
    return {"version": 1, "harnesses": profile["harnesses"]}


def validate_in_clone(binary, project, directory, config):
    """Validate a candidate configuration in a disposable local no-checkout clone."""
    ensure_dir(directory)
    with tempfile.TemporaryDirectory(prefix="validate-", dir=directory) as temporary:
        clone = Path(temporary) / "project"
        capture(["git", "clone", "--quiet", "--shared", "--no-checkout", project, clone])
        ensure_dir(clone / ".orch")
        add_excludes(clone)
        atomic_bytes(clone / ".orch/harnesses.yaml", encoded(config))
        core(binary, clone, "harness", "lint")
        return discovery_rows(core(binary, clone, "harness", "list", "--action=consult"))


def load_profile(directory):
    """Read the selected personal profile without silently inventing defaults."""
    path = directory / "profile.json"
    try:
        profile = json.loads(read_regular(path))
    except FileNotFoundError:
        fail("尚未配置 OpenOrch；请先发现并选择通道，再运行 configure。")
    profile_shape(profile)
    return profile


def configure(args, directory, binary, project):
    """Validate and atomically save explicitly chosen personal settings."""
    profile = json.loads(read_regular(Path(args.input).expanduser().absolute()))
    config = profile_shape(profile)
    ensure_dir(directory)
    with locked(directory / ".profile.lock"):
        target = directory / "profile.json"
        safe_chain(target)
        if target.exists() and not args.replace_profile:
            fail("个人配置已存在；明确更新时使用 --replace-profile，已有项目不会自动更新。")
        rows = validate_in_clone(binary, project, directory, config)
        available = {row["alias"] for row in rows if row["status"] == "supported"}
        selected = [profile["defaults"]["single"], *profile["defaults"]["fusion"]]
        if any(alias not in available for alias in selected):
            reasons = {row["alias"]: row["reason"] for row in rows}
            fail("默认成员不支持当前咨询动作: " + "; ".join(alias + ": " + reasons.get(alias, "未发现") for alias in selected if alias not in available))
        atomic_bytes(target, encoded(profile), replace=args.replace_profile)
    return {"saved": str(target), "defaults": profile["defaults"], "projectConfigurationChanged": False, "loginVerified": False}


def attach(directory, binary, project):
    """Create only a missing main-repository configuration; preserve existing bytes."""
    owner = config_root(project)
    state = owner / ".orch"
    ensure_dir(state)
    target = state / "harnesses.yaml"
    with locked(state / ".openorch-setup.lock"):
        safe_chain(target)
        if target.exists():
            core(binary, project, "harness", "lint")
            return {"created": False, "config": str(target), "project": str(project)}
        profile = load_profile(directory)
        config = profile_shape(profile)
        add_excludes(owner)
        atomic_bytes(target, encoded(config))
        try:
            core(binary, project, "harness", "lint")
        except SetupError:
            # Own newly created file is preserved for a clear diagnosis; no provider ran.
            raise
    return {"created": True, "config": str(target), "project": str(project)}


def discover(binary, project, directory):
    """Locate installed command paths without checking credentials or invoking models."""
    found = {}
    for name in SUPPORTED_NAMES:
        command_name = "cursor-agent" if name == "cursor" else name
        candidate = shutil.which(command_name)
        if name == "smartclaw":
            app = Path("/Applications/DewuSmartClaw.app/Contents/MacOS/DewuSmartClaw")
            if app.is_file():
                candidate = str(app)
        if not candidate:
            candidates = [Path.home() / ".local/bin" / command_name]
            if name == "opencode":
                candidates.append(Path.home() / ".opencode/bin/opencode")
            if name == "codex":
                candidates.append(Path("/Applications/Codex.app/Contents/Resources/codex"))
            candidate = next((str(x) for x in candidates if x.is_file() and os.access(x, os.X_OK)), None)
        if candidate:
            found[name] = {"driver": name, "executable": str(Path(candidate).resolve()), "enabled": True, "cwdPolicy": "project-root"}
    if not found:
        return {"candidates": [], "loginVerified": False, "message": "未找到已安装的通道；请先安装并登录所需原生客户端。"}
    rows = validate_in_clone(binary, project, directory, {"version": 1, "harnesses": found})
    return {"candidates": [{**row, "executable": found[row["alias"]]["executable"]} for row in rows], "loginVerified": False, "modelsInvoked": 0,
            "message": "已安装与动作支持不等于已登录；按实际客户端选择模型，不会自动替换。", "configurationExample": found}


def run_invocation(args, directory, binary, project):
    """Delegate single or multi-member advice directly to the existing core process."""
    attach(directory, binary, project)
    if args.harness:
        members = args.harness
    else:
        defaults = load_profile(directory)["defaults"]
        members = [defaults["single"]] if args.mode == "single" else defaults["fusion"]
    minimum = 1 if args.mode == "single" else 2
    maximum = 1 if args.mode == "single" else 5
    if not minimum <= len(members) <= maximum or len(set(members)) != len(members):
        fail("single 需要1个成员；fusion 需要2–5个不同成员。请明确配置或指定。")
    if any(not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]*", alias) for alias in members):
        fail("成员别名格式非法。")
    question = read_regular(Path(args.question_file).expanduser().absolute())
    question.decode("utf-8")
    if not question.strip():
        fail("咨询问题不能为空。")
    add_excludes(config_root(project))
    requests = ensure_dir(project / ".orch/openorch-requests")
    path = requests / (uuid.uuid4().hex + ".md")
    atomic_bytes(path, question)
    argv = [str(binary), "--root", str(project), "consult", str(path)]
    for alias in members:
        argv += ["--harness", alias]
    for attachment in args.attach or []:
        source = Path(attachment).expanduser()
        if not source.is_absolute():
            source = project / source
        argv += ["--attach", str(source.absolute())]
    os.chdir(project)
    os.execv(str(binary), argv)


def parser():
    """Expose only plugin helper operations; no new orch command is created."""
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
    """Validate local state, perform the requested helper action, and report real errors."""
    args = parser().parse_args()
    try:
        directory = Path(args.config_dir).expanduser() if args.config_dir else Path.home() / ".config/openorch"
        if not directory.is_absolute():
            fail("--config-dir 需要绝对路径。")
        safe_chain(directory, leaf_directory=True)
        project = project_root(args.project)
        binary = runtime()
        if args.action == "configure":
            value = configure(args, directory, binary, project)
        elif args.action == "attach":
            value = attach(directory, binary, project)
        elif args.action == "discover":
            value = discover(binary, project, directory)
        elif args.action == "doctor":
            value = {"project": str(project), "runtime": str(binary), "guide": capture([binary, "guide", "--check"]).strip(),
                     "doctor": core(binary, project, "doctor").strip(), "harnesses": discovery_rows(core(binary, project, "harness", "list", "--action=consult")), "loginVerified": False}
        else:
            run_invocation(args, directory, binary, project)
            return
        print(json.dumps(value, ensure_ascii=False, indent=2))
    except (SetupError, OSError, ValueError, TypeError, KeyError) as error:
        print("OpenOrch: " + str(error), file=sys.stderr)
        raise SystemExit(2)


if __name__ == "__main__":
    main()
