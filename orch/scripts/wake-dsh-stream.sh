#!/bin/sh
# wake-dsh-stream.sh <message> -- DeepSeek Harness managed native projection.
# Complete managed envelopes use private model-settings snapshots. Authenticated
# Consult additionally validates the native final message and publishes exact
# native history in the calling project. The legacy wake projection/capability-floor notes below describe
# the retained non-Consult route; they do not substitute for Consult evidence.
#
# Reserved wrapper outcomes. Provider-native non-zero codes are preserved as
# failed exact reasons and therefore deliberately remain outside this manifest.
# orch-exit-code: 0 answered exact-terminal
# orch-exit-code: 3 failed launch-failure
# orch-exit-code: 64 failed invalid-arguments
# orch-exit-code: 65 failed invalid-workspace
# orch-exit-code: 66 failed invalid-envelope
# orch-exit-code: 70 empty zero-frame-eof
# orch-exit-code: 71 empty truncated-no-terminal
# orch-exit-code: 72 timedOut hard-deadline
# orch-exit-code: 74 failed identity-drift
#
# This is intentionally a narrow, nongate-review transport.  It parses the
# review site and runtime target from their authoritative message lines.  It
# keeps ambient DSH_HOME for the installed profile/credentials, but uses DSH's
# native --patch layer to redirect session persistence to an attempt-local
# directory under CARGO_TARGET_DIR.  That directory is outside the DSH
# workspace-write root, so tools cannot forge the transcript; a user's web
# daemon and its ambient sessions are never candidates for this invocation.
# The wrapper launches DSH with WORKTREE as cwd, then incrementally decodes
# DSH's own multi-frame zstd session log.
# Only harness-owned completed `tool/result` text is translated to the generic
# `tool_result` grammar consumed by orch.  Prompt, tool/call, assistant text,
# and DSH's final stdout are never evidence and are discarded.
#
# Capability floor -- this wrapper explicitly DOES NOT provide:
# - implement, critical-implement, or takeover authority;
# - serve operation or automatic takeover;
# - formal-review authority (r71 admits DSH as nongate-review only);
# - a backend receipt in legacy/no-envelope mode (managed envelope mode emits
#   one `dsh.session` only after the authenticated request/header pin matches);
# - managed process topology or managed termination semantics;
# - cost/usage accounting;
# - session continuity across wakes; or
# - interactive approval handling.
# `turn/start` -> `turn.started` is an H89-style faithful translation frame,
# not a provider-native receipt.  The wrapper never claims otherwise.
#
# Two observed traps define the implementation shape:
# - Node zstdDecompressSync stops after the first frame.  We use the zstd CLI,
#   re-decode the complete prefix on every poll, and deduplicate by record.
# - zstd can successfully decode a truncated prefix.  Decode success is never
#   completion; only a selected session's `turn/end` permits exit 0.

msg="$1"
[ -n "$msg" ] || {
  echo "usage: wake-dsh-stream.sh <message>  (env: ORCH_DSH_BIN, DSH_HOME, ORCH_DSH_ZSTD_BIN, ORCH_DSH_TIMEOUT)" >&2
  exit 64
}

MSG="$msg" python3 - <<'PY'
import json
import os
import secrets
import shutil
import stat
import subprocess
import sys
import tempfile
import time


MAX_FRAME_BYTES = 64 * 1024
MAX_BUFFERED_PROJECTION_BYTES = 4 * 1024 * 1024


def diag(text):
    print("[wake-dsh-stream] " + text, file=sys.stderr, flush=True)


ENVELOPE_KEYS = (
    "ORCH_HARNESS_ID",
    "ORCH_HARNESS_ACTION_ID",
    "ORCH_HARNESS_WAKE_ID",
    "ORCH_HARNESS_ROUND",
    "ORCH_HARNESS_TASK_ID",
    "ORCH_HARNESS_ATTEMPT_ID",
    "ORCH_HARNESS_ROLE",
    "ORCH_HARNESS_CWD",
    "ORCH_HARNESS_FIXED_HEAD",
    "ORCH_HARNESS_PROVIDER",
    "ORCH_HARNESS_MODEL",
    "ORCH_HARNESS_EFFORT",
    "ORCH_HARNESS_PROVIDER_BIN",
    "ORCH_HARNESS_REVIEW_OUTPUT_PATH",
    "ORCH_HARNESS_ORCH_BIN",
    "ORCH_HARNESS_DEADLINE_SECS",
)


def exact_executable(value, key):
    if not isinstance(value, str) or not os.path.isabs(value):
        diag("%s 必须是绝对可执行路径: %r" % (key, value))
        sys.exit(66)
    resolved = os.path.realpath(value)
    if not os.path.isfile(resolved) or not os.access(resolved, os.X_OK):
        diag("%s 不存在或不可执行: %s" % (key, value))
        sys.exit(66)
    return resolved


def legacy_conflict(alias, envelope_key, selected):
    legacy = os.environ.get(alias)
    if legacy is not None and legacy != selected:
        diag("legacy alias conflict: %s ignored; %s wins" % (alias, envelope_key))


def executable(value, label):
    expanded = os.path.expanduser(value)
    if os.sep in expanded:
        path = os.path.realpath(expanded)
        if os.path.isfile(path) and os.access(path, os.X_OK):
            return path
    else:
        found = shutil.which(expanded)
        if found:
            return found
    diag("%s 不可执行: %s" % (label, value))
    sys.exit(3)


def message_directory(message, key):
    values = []
    for line in message.splitlines():
        prefix = key + "="
        if line.startswith(prefix):
            values.append(line[len(prefix) :].strip())
    if len(values) != 1 or not values[0]:
        diag("消息必须恰含一条非空 %s= 行" % key)
        sys.exit(65)
    value = values[0]
    if not os.path.isabs(value):
        diag("%s= 必须是绝对路径: %s" % (key, value))
        sys.exit(65)
    canonical = os.path.realpath(value)
    if not os.path.isdir(canonical):
        diag("%s= 不是目录: %s" % (key, canonical))
        sys.exit(65)
    return canonical


def private_directory(path, label):
    try:
        os.mkdir(path, 0o700)
    except FileExistsError:
        pass
    except OSError as exc:
        diag("无法创建 %s: %s" % (label, exc))
        sys.exit(65)
    try:
        path_stat = os.lstat(path)
    except OSError as exc:
        diag("无法核验 %s: %s" % (label, exc))
        sys.exit(65)
    if not stat.S_ISDIR(path_stat.st_mode) or stat.S_ISLNK(path_stat.st_mode):
        diag("%s 必须是非符号链接目录: %s" % (label, path))
        sys.exit(65)
    return os.path.realpath(path)


def install_exact_file(path, payload, label):
    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        fd = os.open(path, flags, 0o600)
    except FileExistsError:
        try:
            path_stat = os.lstat(path)
            if not stat.S_ISREG(path_stat.st_mode) or stat.S_ISLNK(path_stat.st_mode):
                raise OSError("not a regular non-symlink file")
            with open(path, "rb") as stream:
                current = stream.read()
        except OSError as exc:
            diag("无法核验 %s: %s" % (label, exc))
            sys.exit(65)
        if current != payload:
            diag("%s 已存在但内容漂移: %s" % (label, path))
            sys.exit(65)
        return
    except OSError as exc:
        diag("无法创建 %s: %s" % (label, exc))
        sys.exit(65)
    try:
        with os.fdopen(fd, "wb") as stream:
            stream.write(payload)
            stream.flush()
            os.fsync(stream.fileno())
    except OSError as exc:
        diag("无法写入 %s: %s" % (label, exc))
        sys.exit(65)


def cwd_slug(path):
    # DSH 0.1.0-rc.6 uses --<absolute components joined by hyphens>--.
    body = path.strip(os.sep).replace(os.sep, "-")
    return "--%s--" % body


def bounded_tool_result(value):
    text = "" if value is None else str(value)
    frame = {"type": "tool_result", "output": text}
    encoded = json.dumps(
        frame, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    if len(encoded) <= MAX_FRAME_BYTES:
        return encoded.decode("utf-8")

    marker = " [truncated]"
    low = 0
    high = len(text)
    best = None
    while low <= high:
        middle = (low + high) // 2
        candidate = {
            "type": "tool_result",
            "output": text[:middle] + marker,
            "truncated": True,
        }
        encoded = json.dumps(
            candidate, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        if len(encoded) <= MAX_FRAME_BYTES:
            best = encoded.decode("utf-8")
            low = middle + 1
        else:
            high = middle - 1
    if best is not None:
        return best
    return json.dumps(
        {"type": "tool_result", "output": marker, "truncated": True},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def completed_tool_text(record):
    if not isinstance(record, dict) or record.get("type") != "tool/result":
        return None
    data = record.get("data")
    message = data.get("message") if isinstance(data, dict) else None
    outer_content = message.get("content") if isinstance(message, dict) else None
    if not isinstance(outer_content, list):
        return None

    chunks = []
    for result in outer_content:
        if not isinstance(result, dict) or result.get("type") != "tool-result":
            continue
        content = result.get("content")
        if not isinstance(content, list):
            continue
        for item in content:
            if (
                isinstance(item, dict)
                and item.get("type") == "text"
                and isinstance(item.get("text"), str)
            ):
                chunks.append(item["text"])
    return "\n".join(chunks) if chunks else None


# Use the selected installation's parser and persistence reader. The small
# helper lives in this authenticated wrapper, not in the calling repository.
# Native !!js profile values are inspected as data, never evaluated.
NATIVE_CONSULT_HELPER = r"""
import fs from 'node:fs';
import path from 'node:path';
import {createRequire} from 'node:module';
import {pathToFileURL} from 'node:url';
import {createHash, randomBytes} from 'node:crypto';
const action=process.argv[2], info=JSON.parse(fs.readFileSync(process.argv[3],'utf8'));
const req=createRequire(fs.realpathSync(info.bin));
const nativeImport=async name=>import(pathToFileURL(req.resolve(name)).href);
const sha=bytes=>createHash('sha256').update(bytes).digest('hex');
function requireValue(ok, message) { if(!ok) throw Error(message); }
function utf8(bytes) {
 const text=bytes.toString('utf8');requireValue(Buffer.from(text).equals(bytes),'invalid UTF-8');return text;
}
function noLinks(value, create=false) {
 requireValue(path.isAbsolute(value),'native path must be absolute');
 let current=path.parse(value).root;
 for(const part of value.slice(current.length).split(path.sep).filter(Boolean)) {
  current=path.join(current,part);
  if(create&&!fs.existsSync(current))fs.mkdirSync(current,{mode:0o700});
  const st=fs.lstatSync(current);requireValue(st.isDirectory()&&!st.isSymbolicLink(),'native directory is not a plain directory');
 }
 return value;
}
function fileBytes(file) {
 noLinks(path.dirname(file));
 const fd=fs.openSync(file,fs.constants.O_RDONLY|fs.constants.O_NOFOLLOW);
 try {const before=fs.fstatSync(fd);requireValue(before.isFile()&&before.size<=64*1024*1024,'native file type/size rejected');
  const bytes=fs.readFileSync(fd);const after=fs.fstatSync(fd);
  requireValue(before.size===bytes.length&&before.size===after.size&&before.mtimeMs===after.mtimeMs,'native file changed');return bytes;
 }finally{fs.closeSync(fd);}
}
function syncDir(dir) {const fd=fs.openSync(dir,'r');try{fs.fsyncSync(fd);}finally{fs.closeSync(fd);}}
function privateInventory(root, source) {
 noLinks(root);
 const inventory=[];
 const identity=st=>[st.dev,st.ino,st.mode].map(String);
 for(const project of fs.readdirSync(root,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))) {
  requireValue(!project.isSymbolicLink(),'private project entry is a symlink');
  if(!project.isDirectory())continue;
  const projectPath=path.join(root,project.name);
  for(const entry of fs.readdirSync(projectPath,{withFileTypes:true}).sort((a,b)=>a.name.localeCompare(b.name))) {
   if(!entry.name.startsWith('session-'))continue;
   const dir=path.join(projectPath,entry.name), ds=fs.lstatSync(dir,{bigint:true});
   requireValue(ds.isDirectory()&&!ds.isSymbolicLink(),'private session entry is not a plain directory');
   const file=path.join(dir,'session.jsonl.zstd'), st=fs.lstatSync(file,{bigint:true});
   requireValue(st.isFile()&&!st.isSymbolicLink(),'private canonical log is unresolved or not regular');
   inventory.push({dir,file,directory:identity(ds),log:[...identity(st),String(st.size),String(st.mtimeNs)]});
  }
 }
 requireValue(inventory.length===1&&inventory[0].file===source,'private session set is not uniquely selected');
 return JSON.stringify(inventory);
}
async function validatePrivateSet(store, info, source) {
 const before=privateInventory(info.privateRoot,source);
 const listed=await store.list();
 requireValue(listed.length===1&&listed[0].id===info.id&&listed[0].cwd===info.cwd,'native private session metadata is not uniquely selected');
 const after=privateInventory(info.privateRoot,source);
 requireValue(before===after,'private session identity/stat changed during native observation');
 return after;
}
if(action==='prepare') {
 const yaml=await import(pathToFileURL(createRequire(req.resolve('@deepseek-ai/dsh-settings-file')).resolve('yaml')).href);
 const profile=yaml.parseDocument(utf8(fileBytes(info.profile)),{customTags:[{tag:'tag:yaml.org,2002:js',resolve:value=>({nativeExpression:value})}]});
 requireValue(!profile.errors.length&&!profile.warnings?.length,'native profile cannot be parsed');
 const tree=profile.toJS();requireValue(Array.isArray(tree),'native profile must be a flat composition');
 function entry(id) {const rows=tree.filter(x=>x.id===id);requireValue(rows.length===1&&!rows[0].disabled,'native profile entry unavailable: '+id);return rows[0].config??{};}
 const settingConfig=entry('settings');
 let nativeRoot=null;
 if(info.consult!==false) {
  const storage=entry('session-persistence-jsonl');
  requireValue(!storage.compression||storage.compression==='zstd','native history requires zstd storage');
  nativeRoot=storage.root;
  if(nativeRoot?.nativeExpression==="dshHomePath('sessions')"||nativeRoot?.nativeExpression==='dshHomePath("sessions")')nativeRoot=path.join(info.home,'sessions');
  requireValue(typeof nativeRoot==='string'&&path.isAbsolute(nativeRoot),'unsupported native session-root expression');
  nativeRoot=path.resolve(nativeRoot);
  requireValue(nativeRoot!==info.cwd&&!nativeRoot.startsWith(info.cwd+path.sep),'native history root cannot be inside consultation workspace');
 }
 requireValue(!settingConfig.dshHome,'custom settings dshHome is not modeled');
 requireValue(settingConfig.path===undefined||typeof settingConfig.path==='string','unsupported native settings-path expression');
 const source=settingConfig.path===undefined?path.join(info.home,'settings.yaml'):path.resolve(info.cwd,settingConfig.path);
 const sourceExists=(()=>{try{fs.lstatSync(source);return true;}catch(error){if(error.code==='ENOENT')return false;throw error;}})();
 const original=sourceExists?fileBytes(source):Buffer.from('{}');
 const doc=yaml.parseDocument(utf8(original));requireValue(!doc.errors.length&&!doc.warnings?.length,'native settings cannot be parsed');
 const settings=doc.toJS()??{};requireValue(typeof settings==='object'&&!Array.isArray(settings),'native settings must be a map');
 const previous=settings['agent-default-model'];
 settings['agent-default-model']=info.consult===false&&previous&&typeof previous==='object'&&!Array.isArray(previous)?{...previous,...info.pins}:info.pins;
 if(info.consult!==false)settings.permission={defaultPreset:'read-only'};
 const fd=fs.openSync(info.settings,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);
 try{fs.writeFileSync(fd,JSON.stringify(settings)+'\n');fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
 console.log(JSON.stringify({nativeRoot,settingsPath:info.settings,originalSettingsPath:source,originalSettingsSha256:sha(original)}));
} else if(action==='publish') {
 const {Context}=await nativeImport('@deepseek-ai/cordis');
 const {SessionStore}=await nativeImport('@deepseek-ai/dsh-session');
 const {JsonlSessionPersistence}=await nativeImport('@deepseek-ai/dsh-session-persistence-jsonl');
 const ctx=new Context();new SessionStore(ctx);const contexts=[ctx];
 let publishStage='private-read', linked='not-attempted', destination=null;
 const effectDiagnostic=()=>console.error('publish-stage='+publishStage+' selected='+info.id+' destination='+String(destination)+' linked='+linked+'; retained evidence');
 async function disposeContexts() {
  let failure;
  while(contexts.length){try{await contexts.pop().fiber.dispose();}catch(error){failure??=error;}}
  if(failure)throw failure;
 }
 try {
  const privateStore=new JsonlSessionPersistence(ctx,{root:info.privateRoot,compression:'zstd'});
  const source=privateStore.locate({id:info.id,cwd:info.cwd}).path;
  const privateSet=privateInventory(info.privateRoot,source);
  const bytes=fileBytes(source), loaded=await privateStore.loadStored(info.id);
  requireValue(loaded&&!loaded.tornMarker,'native history is absent or torn');
  requireValue(loaded.meta.id===info.id&&loaded.meta.cwd===info.cwd,'native history identity drift');
  const events=loaded.events, end=events.at(-1);
  requireValue(events.filter(x=>x.type==='turn/start').length===1&&events.filter(x=>x.type==='turn/end').length===1&&end?.type==='turn/end'&&end.data?.reason?.kind==='completed','native turn did not complete exactly once');
  const headers=events.filter(x=>x.type==='request/header');requireValue(headers.length>0,'native request pin absent');
  for(const h of headers)for(const key of ['provider','model','reasoningEffort'])requireValue(h.data?.header?.config?.[key]===info.pins[key],'native request pin drift');
  const pending=new Set(), seenCalls=new Set();
  const mutations=new Set(['write','edit','str_replace_editor','subagent','subagent_fork','subagent_codex','subagent_claude_code','ralph','workflow']);
  for(const e of events) {
   if(e.type==='tool/call') {const d=e.data;requireValue(typeof d?.callId==='string'&&!seenCalls.has(d.callId),'native tool identity is ambiguous');const editorView=d.name==='str_replace_editor'&&d.arguments?.command==='view';requireValue(editorView||!mutations.has(d.name),'mutating/delegating tool in read-only consultation');seenCalls.add(d.callId);pending.add(d.callId);}
   if(e.type==='tool/result')for(const block of e.data?.message?.content??[])if(block.type==='tool-result')requireValue(pending.delete(block.toolCallId),'native result has no unique pending call');
  }
  requireValue(pending.size===0,'native tools are still pending');
  const finalEvent=events.filter(x=>x.type==='assistant/message').at(-1);
  requireValue(finalEvent?.data?.turn===end.data.turn,'native final belongs to another turn');
  const last=finalEvent?.data?.message;
  requireValue(last?.role==='assistant'&&Array.isArray(last.content),'native final message missing');
  const text=last.content.filter(x=>x.type==='text').map(x=>{requireValue(typeof x.text==='string','invalid native text');return x.text;}).join('');
  requireValue(text.trim().length>0&&Buffer.byteLength(text)<=60*1024,'native final is empty or oversized');
  const raw=await privateStore.readRaw(info.id);requireValue(raw&&fileBytes(source).equals(bytes),'private native evidence changed');
  publishStage='pre-mutation';
  requireValue(await validatePrivateSet(privateStore,info,source)===privateSet,'private identity/stat changed during initial native reads');
  noLinks(info.nativeRoot,true);
  const nativeContext=new Context();new SessionStore(nativeContext);contexts.push(nativeContext);
  const nativeStore=new JsonlSessionPersistence(nativeContext,{root:info.nativeRoot,compression:'zstd'});
  requireValue(!(await nativeStore.loadStored(info.id)),'native publication ID collision');
  requireValue(!(await nativeStore.list()).some(x=>x.id===info.id),'native publication ID collision');
  for(const project of fs.readdirSync(info.nativeRoot,{withFileTypes:true}))if(project.isDirectory())requireValue(!fs.existsSync(path.join(info.nativeRoot,project.name,info.id)),'existing native session directory');
  destination=nativeStore.locate(loaded.meta).path;
  requireValue(destination.startsWith(info.nativeRoot+path.sep),'native locator escaped root');
  const dir=path.dirname(destination);noLinks(path.dirname(dir),true);fs.mkdirSync(dir,{mode:0o700});syncDir(path.dirname(dir));
  const temporary=path.join(dir,'.orch-publish-'+randomBytes(12).toString('hex'));
  publishStage='pre-link';
  const fd=fs.openSync(temporary,fs.constants.O_WRONLY|fs.constants.O_CREAT|fs.constants.O_EXCL|fs.constants.O_NOFOLLOW,0o600);
  const ownedTemp=fs.fstatSync(fd,{bigint:true});
  try {
   try{fs.writeFileSync(fd,bytes);fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
   requireValue(await validatePrivateSet(privateStore,info,source)===privateSet,'private set changed before link');
   linked='unknown';fs.linkSync(temporary,destination);linked='yes';publishStage='post-link';syncDir(dir);
  } finally {
   const current=fs.lstatSync(temporary,{bigint:true});
   requireValue(current.isFile()&&!current.isSymbolicLink()&&current.dev===ownedTemp.dev&&current.ino===ownedTemp.ino,'owned staging identity changed; retained');
   fs.unlinkSync(temporary);
  }
  const discovered=(await nativeStore.list()).filter(x=>x.id===info.id&&x.cwd===info.cwd);
  const opened=await nativeStore.loadStored(info.id), reopenedRaw=await nativeStore.readRaw(info.id);
  requireValue(discovered.length===1&&opened&&!opened.tornMarker&&opened.meta.cwd===info.cwd&&reopenedRaw?.content===raw.content,'published native history failed lookup/open');
  requireValue(fileBytes(destination).equals(bytes)&&fileBytes(source).equals(bytes),'published native bytes differ');
  requireValue(await validatePrivateSet(privateStore,info,source)===privateSet,'private set changed after link');
  await disposeContexts();
  console.log(JSON.stringify({finalText:text,finalTextSha256:sha(Buffer.from(text)),nativeHistoryPath:destination,nativeHistorySha256:sha(bytes)}));
 }catch(error){effectDiagnostic();throw error;}finally{try{await disposeContexts();}catch(error){effectDiagnostic();throw error;}}
}else{throw Error('unknown native consultation operation');}
"""


def native_consult_operation(action, info):
    metadata = os.path.join(runtime_target, ".orch-dsh-native-" + action + "-" + launch_nonce + ".json")
    install_exact_file(metadata, json.dumps(info).encode("utf-8"), "native consultation input")
    errors = metadata + ".stderr"
    fd = os.open(errors, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    env = os.environ.copy()
    env["DSH_HOME"] = dsh_home
    with os.fdopen(fd, "wb") as stderr:
        result = subprocess.run(
            [executable("node", "DSH native API Node"), "--input-type=module", "-", action, metadata],
            input=NATIVE_CONSULT_HELPER.encode("utf-8"), stdout=subprocess.PIPE,
            stderr=stderr, cwd=workdir, env=env, timeout=30,
        )
    if result.returncode != 0:
        raise RuntimeError("native consultation %s failed; private diagnostics: %s" % (action, errors))
    value = json.loads(result.stdout.decode("utf-8"))
    if not isinstance(value, dict):
        raise RuntimeError("native consultation helper did not return an object")
    return value


message = os.environ["MSG"]
present_envelope_keys = [key for key in ENVELOPE_KEYS if key in os.environ]
envelope_mode = bool(present_envelope_keys)
consult_mode = envelope_mode and os.environ.get("ORCH_HARNESS_ROLE") == "consult"
if envelope_mode:
    missing = [
        key
        for key in ENVELOPE_KEYS
        if not isinstance(os.environ.get(key), str) or not os.environ[key].strip()
    ]
    if missing:
        diag("incomplete invocation envelope; missing/blank: %s" % ",".join(missing))
        sys.exit(66)
    if os.environ["ORCH_HARNESS_ID"] != "dsh":
        diag("ORCH_HARNESS_ID 与 dsh wrapper 不匹配")
        sys.exit(66)
    workdir = os.environ["ORCH_HARNESS_CWD"]
    dsh_bin = exact_executable(
        os.environ["ORCH_HARNESS_PROVIDER_BIN"], "ORCH_HARNESS_PROVIDER_BIN"
    )
    pin_provider = os.environ["ORCH_HARNESS_PROVIDER"]
    pin_model = os.environ["ORCH_HARNESS_MODEL"]
    pin_effort = os.environ["ORCH_HARNESS_EFFORT"]
    timeout_text = os.environ["ORCH_HARNESS_DEADLINE_SECS"]
    for alias, envelope_key, selected in (
        ("ORCH_DSH_CWD", "ORCH_HARNESS_CWD", workdir),
        ("ORCH_DSH_BIN", "ORCH_HARNESS_PROVIDER_BIN", dsh_bin),
        ("ORCH_DSH_PROVIDER", "ORCH_HARNESS_PROVIDER", pin_provider),
        ("ORCH_DSH_MODEL", "ORCH_HARNESS_MODEL", pin_model),
        ("ORCH_DSH_EFFORT", "ORCH_HARNESS_EFFORT", pin_effort),
        ("ORCH_DSH_TIMEOUT", "ORCH_HARNESS_DEADLINE_SECS", timeout_text),
    ):
        legacy_conflict(alias, envelope_key, selected)
else:
    workdir = message_directory(message, "WORKTREE")
    dsh_bin = executable(os.environ.get("ORCH_DSH_BIN") or "dsh", "DSH 入口")
    pin_provider = os.environ.get("ORCH_DSH_PROVIDER")
    pin_model = os.environ.get("ORCH_DSH_MODEL")
    pin_effort = os.environ.get("ORCH_DSH_EFFORT")
    timeout_text = os.environ.get("ORCH_DSH_TIMEOUT") or "7200"

if not os.path.isdir(workdir) or not os.path.isabs(workdir):
    diag("ORCH_HARNESS_CWD/WORKTREE 必须是绝对目录: %s" % workdir)
    sys.exit(65)
if envelope_mode:
    # ORCH_HARNESS_ORCH_BIN is an already validated absolute executable such
    # as <target>/debug/orch.  The parent of its profile directory is the
    # runtime-owned Cargo target root; envelope mode must not keep borrowing a
    # second, unauthenticated path from human prompt text.
    runtime_target = os.path.dirname(
        os.path.dirname(os.path.realpath(os.environ["ORCH_HARNESS_ORCH_BIN"]))
    )
else:
    runtime_target = message_directory(message, "CARGO_TARGET_DIR")
try:
    target_is_in_workspace = os.path.commonpath([workdir, runtime_target]) == workdir
    temporary_roots = {
        os.path.realpath("/tmp"),
        os.path.realpath(tempfile.gettempdir()),
    }
    target_is_temporary = any(
        os.path.commonpath([root, runtime_target]) == root for root in temporary_roots
    )
except ValueError:
    target_is_in_workspace = True
    target_is_temporary = True
manual_envelope = envelope_mode and (
    os.environ["ORCH_HARNESS_TASK_ID"] == "MANUAL"
    and os.environ["ORCH_HARNESS_ATTEMPT_ID"] == "MANUAL-A0000"
)
consult_envelope = consult_mode and (
    os.environ.get("ORCH_HARNESS_TASK_ID") == "CONSULT"
    and os.environ.get("ORCH_HARNESS_ATTEMPT_ID") == "CONSULT-A0000"
)
if consult_mode and not consult_envelope:
    diag("consult requires the exact CONSULT/CONSULT-A0000 envelope")
    sys.exit(66)
if (target_is_in_workspace and not (manual_envelope or consult_envelope)) or target_is_temporary:
    diag("CARGO_TARGET_DIR 必须位于 WORKTREE 与 /tmp 之外: %s" % runtime_target)
    sys.exit(65)

dsh_home = os.path.realpath(
    os.path.expanduser(os.environ.get("DSH_HOME") or "~/.dsh")
)
if not os.path.isdir(dsh_home):
    diag("ambient DSH_HOME 不是目录: %s" % dsh_home)
    sys.exit(65)
try:
    home_is_in_workspace = os.path.commonpath([workdir, dsh_home]) == workdir
    home_is_temporary = any(
        os.path.commonpath([root, dsh_home]) == root for root in temporary_roots
    )
except ValueError:
    home_is_in_workspace = True
    home_is_temporary = True
if home_is_in_workspace or home_is_temporary:
    diag("ambient DSH_HOME 必须位于 WORKTREE 与 /tmp 之外: %s" % dsh_home)
    sys.exit(65)

launch_nonce = secrets.token_hex(16)
session_store = private_directory(
    os.path.join(runtime_target, ".orch-dsh-sessions-" + launch_nonce),
    "attempt-local DSH session root",
)
patch_path = os.path.join(
    runtime_target, ".orch-dsh-session-patch-" + launch_nonce + ".json"
)
patch_entries = [
    {"id": "session-persistence-jsonl", "config": {"root": session_store}}
]
preset = os.environ.get("ORCH_DSH_PRESET") or "minimal"
profile = os.environ.get("ORCH_DSH_PROFILE") or "headless"
consult_prepared = None
if envelope_mode:
    if consult_mode and os.environ.get("ORCH_DSH_PRESET"):
        diag("DSH consult does not model the minimal preset; select headless")
        sys.exit(66)
    profile_path = os.path.join(runtime_target, ".orch-dsh-profile-" + launch_nonce + ".yaml")
    profile_errors = profile_path + ".stderr"
    profile_fd = os.open(profile_errors, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    try:
        profile_env = os.environ.copy()
        profile_env["DSH_HOME"] = dsh_home
        with os.fdopen(profile_fd, "wb") as errors:
            composed = subprocess.run([dsh_bin, "--profile", profile, "--dump-config"], cwd=workdir,
                env=profile_env, stdout=subprocess.PIPE, stderr=errors, timeout=30)
        if composed.returncode != 0 or len(composed.stdout) > 2 * 1024 * 1024:
            raise RuntimeError("native profile dump failed or exceeded its size bound")
        install_exact_file(profile_path, composed.stdout, "native profile snapshot")
        consult_prepared = native_consult_operation("prepare", {
            "bin": dsh_bin, "home": dsh_home, "cwd": workdir, "profile": profile_path,
            "settings": os.path.join(runtime_target, ".orch-dsh-settings-" + launch_nonce + ".json"),
            "pins": {"provider": pin_provider, "model": pin_model, "reasoningEffort": pin_effort},
            "consult": consult_mode,
        })
        patch_entries.append(
            {"id": "settings", "config": {"path": consult_prepared["settingsPath"], "watch": False}}
        )
        if consult_mode:
            patch_entries.extend([
                {"id": "permission", "config": {"presets": {"read-only": {"sandbox": "read-only", "approval": "ask"}}, "defaultPreset": "read-only"}},
                {"id": "tool-subagent", "disabled": True},
                {"id": "tool-subagent-fork", "disabled": True},
                {"id": "tool-ralph", "disabled": True},
                {"id": "tool-workflow", "disabled": True},
            ])
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
        diag(("consult" if consult_mode else "envelope") + " preparation failed: %s" % exc)
        sys.exit(74)
if envelope_mode:
    patch_entries.extend(
        [
            {
                "id": "agent-default-model",
                "config": {
                    "provider": pin_provider,
                    "model": pin_model,
                    "reasoningEffort": pin_effort,
                },
            },
        ]
    )
    if not consult_mode:
        patch_entries.append({"id": "agent-presets", "config": {"default": preset}})
patch_payload = (
    json.dumps(
        patch_entries, ensure_ascii=False, separators=(",", ":")
    )
    + "\n"
).encode("utf-8")
install_exact_file(patch_path, patch_payload, "DSH session-root patch")

zstd_bin = executable(
    os.environ.get("ORCH_DSH_ZSTD_BIN") or "zstd", "zstd 解码器"
)
try:
    timeout = float(timeout_text)
except ValueError:
    diag("调用 deadline 必须是秒数")
    sys.exit(64)
if timeout <= 0:
    diag("调用 deadline 必须大于 0")
    sys.exit(64)

session_root = os.path.join(session_store, cwd_slug(workdir))
try:
    baseline = {
        entry.path
        for entry in os.scandir(session_root)
        if entry.is_dir(follow_symlinks=False) and entry.name.startswith("session-")
    }
except FileNotFoundError:
    baseline = set()

profile = os.environ.get("ORCH_DSH_PROFILE") or "headless"
argv = [dsh_bin, "--profile", profile, "--patch", patch_path, message]
child_env = os.environ.copy()
child_env["DSH_HOME"] = dsh_home
diag(
    "cwd=%s sessionRoot=%s profile=%s preset=%s provider=%s model=%s effort=%s timeout=%.0fs"
    % (workdir, session_store, profile, preset, pin_provider, pin_model, pin_effort, timeout)
)
try:
    process = subprocess.Popen(
        argv,
        cwd=workdir,
        env=child_env,
        stdout=subprocess.DEVNULL,
        # Runtime stores wrapper stdout and stderr in one evidence log.  Raw
        # provider stderr could therefore masquerade as a projected JSON frame
        # or leak prompt/assistant text.  Exit status plus our own prefixed
        # diagnostics are the only stderr evidence this wrapper admits.
        stderr=subprocess.DEVNULL,
    )
except Exception as exc:
    diag("DSH 启动失败: %s" % exc)
    sys.exit(3)

selected_dir = None
selected_id = None
seen_candidate_dirs = set()
seen_records = 0
record_frames = 0
projected = 0
terminal_seen = False
terminal_record = None
pin_verified = not envelope_mode
receipt_emitted = False
buffered_projection = []
buffered_projection_bytes = 0
pending_writes = []
timed_out = False
selection_error = None
started_wall = time.time()
started_mono = time.monotonic()
last_provider_frame = started_mono
next_heartbeat = started_mono + 15.0


def decode_records(session_dir):
    path = os.path.join(session_dir, "session.jsonl.zstd")
    if not os.path.isfile(path):
        return []
    try:
        decoded = subprocess.run(
            [zstd_bin, "-d", "-c", "-q", path],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            check=False,
            timeout=10,
        ).stdout.decode("utf-8", errors="strict")
    except (OSError, subprocess.TimeoutExpired, UnicodeDecodeError):
        return []

    records = []
    for raw in decoded.splitlines():
        if not raw.strip():
            continue
        try:
            value = json.loads(raw)
        except Exception:
            # A live, incomplete tail is not a record.  Stop at the first gap
            # so the next whole-prefix decode can recover it deterministically.
            break
        if not isinstance(value, dict):
            break
        records.append(value)
    return records


def valid_identity(session_dir, records):
    if not records:
        return None
    first = records[0]
    if first.get("type") != "session":
        return None
    session_id = first.get("id")
    recorded_cwd = first.get("cwd")
    if not isinstance(session_id, str) or session_id != os.path.basename(session_dir):
        return None
    if not isinstance(recorded_cwd, str):
        return None
    if os.path.realpath(recorded_cwd) != workdir:
        return None
    return session_id


def new_candidates():
    try:
        entries = list(os.scandir(session_root))
    except FileNotFoundError:
        return []
    return sorted(
        entry.path
        for entry in entries
        if entry.is_dir(follow_symlinks=False)
        and entry.name.startswith("session-")
        and entry.path not in baseline
    )


def projection_for_record(record):
    kind = record.get("type")
    if kind == "turn/start":
        return ["turn.started"]
    if kind == "tool/result":
        text = completed_tool_text(record)
        if text is not None:
            return [bounded_tool_result(text)]
    return []


def request_header_pin(record):
    if record.get("type") != "request/header":
        return None
    data = record.get("data")
    header = data.get("header") if isinstance(data, dict) else None
    config = header.get("config") if isinstance(header, dict) else None
    if not isinstance(config, dict):
        raise RuntimeError("request/header.data.header.config 缺失或畸形")
    values = (
        config.get("provider"),
        config.get("model"),
        config.get("reasoningEffort"),
    )
    if not all(isinstance(value, str) and value for value in values):
        raise RuntimeError("request/header provider/model/reasoningEffort 缺失或畸形")
    return values


def pending_write_from_record(record, ordinal):
    if not envelope_mode or record.get("type") != "tool/call":
        return None
    data = record.get("data")
    if not isinstance(data, dict) or data.get("name") != "write":
        return None
    arguments = data.get("arguments")
    if not isinstance(arguments, dict):
        return None
    path = arguments.get("path")
    content = arguments.get("content")
    expected_path = os.environ["ORCH_HARNESS_REVIEW_OUTPUT_PATH"]
    if path != expected_path or not isinstance(content, str) or not content:
        return None
    return (ordinal, path, content)


def emit_projection(frame):
    global projected
    print(frame, flush=True)
    projected += 1


def project_record(record, ordinal):
    global terminal_seen, terminal_record, pin_verified, receipt_emitted
    global buffered_projection, buffered_projection_bytes

    if envelope_mode and record.get("type") == "request/header":
        observed = request_header_pin(record)
        expected = (pin_provider, pin_model, pin_effort)
        if observed != expected:
            raise RuntimeError(
                "request/header signed pin 漂移: expected=%r observed=%r"
                % (expected, observed)
            )
        if not pin_verified:
            pin_verified = True
            if receipt_emitted:
                raise RuntimeError("dsh.session receipt 重复")
            print(
                json.dumps(
                    {
                        "type": "dsh.session",
                        "sessionId": selected_id,
                        "provider": pin_provider,
                        "model": pin_model,
                        "effort": pin_effort,
                    },
                    ensure_ascii=False,
                    separators=(",", ":"),
                ),
                flush=True,
            )
            receipt_emitted = True
            for frame in buffered_projection:
                emit_projection(frame)
            buffered_projection = []
            buffered_projection_bytes = 0

    candidate = pending_write_from_record(record, ordinal)
    if candidate is not None:
        pending_writes.append(candidate)

    if record.get("type") == "turn/end":
        terminal_seen = True
        terminal_record = record

    for frame in projection_for_record(record):
        if envelope_mode and not pin_verified:
            buffered_projection.append(frame)
            buffered_projection_bytes += len(frame.encode("utf-8")) + 1
            if buffered_projection_bytes > MAX_BUFFERED_PROJECTION_BYTES:
                raise RuntimeError("request/header 前 buffered_projection 超过安全上限")
        else:
            emit_projection(frame)


def candidate_snapshot():
    """Observe this invocation's current-slug candidates without choosing a winner."""
    paths = new_candidates()
    seen_candidate_dirs.update(paths)
    valid = []
    unresolved = []
    fingerprints = []
    for directory in paths:
        identity = None
        records = []
        try:
            directory_stat = os.lstat(directory)
            log_stat = os.lstat(os.path.join(directory, "session.jsonl.zstd"))
            plain = stat.S_ISDIR(directory_stat.st_mode) and not stat.S_ISLNK(directory_stat.st_mode)
            plain = plain and stat.S_ISREG(log_stat.st_mode) and not stat.S_ISLNK(log_stat.st_mode)
            fingerprint = (directory, directory_stat.st_dev, directory_stat.st_ino,
                           directory_stat.st_mode, log_stat.st_dev, log_stat.st_ino,
                           log_stat.st_mode, log_stat.st_size, log_stat.st_mtime_ns)
            if plain:
                records = decode_records(directory)
                identity = valid_identity(directory, records)
        except OSError as exc:
            fingerprint = (directory, "unresolved", exc.errno)
        fingerprints.append((fingerprint, identity))
        if identity is None:
            unresolved.append(directory)
        else:
            valid.append((directory, identity, records))
    return {"paths": paths, "valid": valid, "unresolved": unresolved,
            "fingerprint": fingerprints}


def validate_bound_candidates(snapshot):
    if len(snapshot["valid"]) > 1:
        raise RuntimeError("multiple valid invocation sessions; refusing recency selection: %s"
                           % ",".join(item[1] for item in snapshot["valid"]))
    if selected_dir is not None and not any(
        directory == selected_dir and identity == selected_id
        for directory, identity, _ in snapshot["valid"]
    ):
        raise RuntimeError("bound session disappeared or its id/cwd became unverifiable")


def final_candidate_set_error():
    """A bounded dirty-set observation, separate from confirmed identity errors."""
    before = candidate_snapshot()
    validate_bound_candidates(before)
    after = candidate_snapshot()
    validate_bound_candidates(after)
    if before["fingerprint"] != after["fingerprint"] or before["paths"] != after["paths"]:
        return "final candidate identity/stat changed during observation"
    if seen_candidate_dirs != set(after["paths"]):
        return "an observed candidate disappeared; final set is unresolved"
    if selected_dir is None:
        return "final candidates lack a selected identity" if after["paths"] else None
    if after["paths"] != [selected_dir] or after["unresolved"]:
        return "final set contains additional or unresolved candidates: %s" % after["paths"]
    return None


def capture_last_complete_pending_write():
    global final_set_error
    final_set_error = final_candidate_set_error()
    if final_set_error is not None:
        diag("pending write suppressed: " + final_set_error)
        return False
    if (
        not envelope_mode
        or selected_id is None
        or not pin_verified
        or not pending_writes
    ):
        return False
    _, path, content = pending_writes[-1]
    expected_path = os.environ["ORCH_HARNESS_REVIEW_OUTPUT_PATH"]
    if path != expected_path or not os.path.isabs(path):
        return False
    expected_parent = os.path.realpath(
        os.path.join(workdir, ".cowork-temp", "review-spool")
    )
    parent = os.path.dirname(path)
    if os.path.realpath(parent) != expected_parent or not os.path.isdir(parent):
        return False
    payload = content.encode("utf-8")
    if not payload.endswith(b"\n"):
        payload += b"\n"
    install_exact_file(path, payload, "DSH pending review write")
    return True


def scan_selected_session():
    global selected_dir, selected_id, seen_records, record_frames, last_provider_frame

    snapshot = candidate_snapshot()
    validate_bound_candidates(snapshot)
    if selected_dir is None:
        if not snapshot["valid"]:
            return 0
        selected_dir, selected_id, records = snapshot["valid"][0]
    else:
        records = next(records for directory, identity, records in snapshot["valid"]
                       if directory == selected_dir and identity == selected_id)

    if len(records) < seen_records:
        raise RuntimeError("已绑定会话记录前缀缩短，拒绝重放或换档")
    added = len(records) - seen_records
    for ordinal, record in enumerate(records[seen_records:], start=seen_records):
        record_frames += 1
        project_record(record, ordinal)
    seen_records = len(records)
    if added:
        last_provider_frame = time.monotonic()
    return added


while True:
    try:
        scan_selected_session()
    except RuntimeError as exc:
        selection_error = str(exc)
        process.kill()
        process.wait()
        break

    rc = process.poll()
    if rc is not None:
        # DSH has closed the file: one final whole-prefix decode observes every
        # completed frame without treating decoder status as terminal proof.
        try:
            scan_selected_session()
        except RuntimeError as exc:
            selection_error = str(exc)
        break

    now_wall = time.time()
    now_mono = time.monotonic()
    if now_wall - started_wall >= timeout:
        timed_out = True
        process.kill()
        process.wait()
        try:
            scan_selected_session()
        except RuntimeError as exc:
            selection_error = str(exc)
        break
    if now_mono >= next_heartbeat and now_mono - last_provider_frame >= 15.0:
        print(
            "[wake-hb] t=+%ds provider=%d frames=%d inflight=1"
            % (int(now_mono - started_mono), process.pid, record_frames),
            flush=True,
        )
        next_heartbeat = now_mono + 15.0
    time.sleep(0.05)

if process.poll() is None:
    process.wait()
provider_rc = process.returncode

final_set_error = None
if selection_error is None:
    try:
        final_set_error = final_candidate_set_error()
    except RuntimeError as exc:
        selection_error = str(exc)
if final_set_error is not None:
    diag("final candidate set dirty; success/capture withheld: " + final_set_error)

captured_pending_write = False
if selection_error is None and envelope_mode and selected_id is not None and not pin_verified:
    selection_error = (
        "selected DSH session ended without an exact request/header pin; "
        "request/context alone is insufficient"
    )
if selection_error is None and final_set_error is None and (provider_rc != 0 or not terminal_seen):
    try:
        captured_pending_write = capture_last_complete_pending_write()
    except (OSError, RuntimeError) as exc:
        selection_error = "pending write capture failed closed: %s" % exc

if selection_error is not None:
    diag(
        "frames=%d projected=%d terminal=no cause=identity-error rc=%s exit=74 pendingWrite=%s detail=%s"
        % (record_frames, projected, provider_rc, "yes" if captured_pending_write else "no", selection_error)
    )
    sys.exit(74)
if timed_out:
    diag(
        "frames=%d projected=%d terminal=%s cause=timeout rc=%s exit=72 pendingWrite=%s"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc, "yes" if captured_pending_write else "no")
    )
    sys.exit(72)
if provider_rc != 0:
    code = provider_rc if isinstance(provider_rc, int) and 0 < provider_rc <= 255 else 1
    diag(
        "frames=%d projected=%d terminal=%s cause=provider-exit rc=%s exit=%d pendingWrite=%s"
        % (record_frames, projected, "yes" if terminal_seen else "no", provider_rc, code, "yes" if captured_pending_write else "no")
    )
    sys.exit(code)
if not terminal_seen:
    code = 70 if record_frames == 0 else 71
    diag(
        "frames=%d projected=%d terminal=no cause=eof rc=0 exit=%d pendingWrite=%s"
        % (record_frames, projected, code, "yes" if captured_pending_write else "no")
    )
    sys.exit(code)

if final_set_error is not None:
    diag("terminal=no cause=unresolved-final-set rc=0 exit=74 detail=" + final_set_error)
    sys.exit(74)

usage = terminal_record.get("usage") if isinstance(terminal_record, dict) else None
consult_final = {}
if consult_mode:
    try:
        consult_final = native_consult_operation("publish", {
            "bin": dsh_bin, "home": dsh_home, "cwd": workdir,
            "privateRoot": session_store, "nativeRoot": consult_prepared["nativeRoot"], "id": selected_id,
            "pins": {"provider": pin_provider, "model": pin_model, "reasoningEffort": pin_effort},
        })
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
        diag("consult native final/history failed (private evidence retained): %s" % exc)
        sys.exit(74)
print(
    json.dumps(
        {
            "type": "dsh.terminal",
            "sessionId": selected_id,
            "exactReason": "turn/end",
            "usage": usage if isinstance(usage, dict) else None,
            "usageAbsentReason": None
            if isinstance(usage, dict)
            else "dsh turn/end record omitted usage",
            **consult_final,
        },
        ensure_ascii=False,
        separators=(",", ":"),
    ),
    flush=True,
)
diag(
    "session=%s frames=%d projected=%d terminal=yes cause=terminal rc=0 exit=0"
    % (selected_id, record_frames, projected)
)
sys.exit(0)
PY
