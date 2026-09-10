//! A bounded, read-only native observation after the socket client has ended.
//! No monitor, backend cancellation or new lifecycle authority is created here.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use wait_timeout::ChildExt;

/// This generation requires a bound native final instead of concatenated payload text.
pub(crate) const OBSERVATION_SOURCE: &str = "dewusmartclaw-native-final-and-stream-v1";

// Embedded in a Rust build input: freshness checks cannot miss an external script.
// Python is already required by this driver. -I excludes repository/user imports.
const NATIVE_READ: &str = r#"
import hashlib,json,os,sqlite3,sys,time,urllib.parse
db,sid,cwd,wanted=sys.argv[1:]
started=time.monotonic()
out={'source':'dewusmartclaw-native-final-and-stream-v1','database':db,
     'runtimeSessionId':sid,'cwd':cwd,'promptSha256':wanted,
     'nativeTerminated':False,'projectionStatus':'unavailable','reason':'native observation unavailable',
     'nativeSession':None,'request':None,'final':None}
def stop(reason):
    out['reason']=reason
    raise RuntimeError(reason)
try:
    uri='file:'+urllib.parse.quote(os.path.abspath(db),safe='/')+'?mode=ro'
    con=sqlite3.connect(uri,uri=True,timeout=0.5)
    con.row_factory=sqlite3.Row
    con.execute('PRAGMA query_only=ON')
    con.set_progress_handler(lambda: int(time.monotonic()-started>2),1000)
    con.execute('BEGIN')
    rows=con.execute('SELECT id,title,status,cwd FROM cowork_sessions WHERE title=? LIMIT 2',('multica:'+sid,)).fetchall()
    if len(rows)!=1: stop('native session missing or ambiguous')
    session=dict(rows[0]); out['nativeSession']=session
    if session['cwd']!=cwd: stop('native cwd differs from invocation')
    users=con.execute("SELECT id,sequence,content FROM cowork_messages WHERE session_id=? AND type='user' ORDER BY sequence LIMIT 2",(session['id'],)).fetchall()
    if len(users)!=1: stop('native request missing or ambiguous')
    user=users[0]
    if not isinstance(user['content'],str) or hashlib.sha256(user['content'].encode('utf8')).hexdigest()!=wanted: stop('native request bytes differ from invocation')
    if not isinstance(user['sequence'],int) or user['sequence']<0: stop('native request sequence invalid')
    out['request']={'id':user['id'],'sequence':user['sequence'],'sha256':wanted}
    pending=set(); declared=set(); results={}; finals=[]
    for row in con.execute('SELECT id,type,sequence,metadata FROM cowork_messages WHERE session_id=? AND sequence>? ORDER BY sequence',(session['id'],user['sequence'])):
        if time.monotonic()-started>2: stop('native observation budget exceeded')
        meta=json.loads(row['metadata'] or '{}')
        if not isinstance(meta,dict): stop('native metadata is not an object')
        if row['type']=='tool_use':
            tool_id=meta.get('toolUseId') or row['id']
            if not isinstance(tool_id,str) or not tool_id: stop('native tool identity missing')
            if tool_id in declared: stop('native tool use identity repeated')
            declared.add(tool_id)
            pending.add(tool_id)
        elif row['type']=='tool_result':
            tool_id=meta.get('toolUseId')
            if not isinstance(tool_id,str) or tool_id not in declared: stop('native tool result identity ambiguous')
            if tool_id in pending:
                pending.remove(tool_id)
                results[tool_id]={'allErrors':meta.get('isError') is True,'ids':[row['id']]}
            else:
                first=results[tool_id]
                if not first['allErrors'] or meta.get('isError') is not True:
                    stop('native repeated tool result is not strictly an error receipt')
                first['ids'].append(row['id'])
        elif row['type']=='assistant' and meta.get('isFinal') is True:
            finals.append((row['id'],row['sequence']))
    if pending: stop('native tools remain pending')
    # The app publishes error before asynchronous turn termination. Error/client
    # EOF alone therefore cannot authorize native cleanup or a completed wave.
    if session['status']!='completed': stop('native session completion is not established')
    out['nativeTerminated']=True
    repeated=[{'toolUseId':key,'resultCount':len(value['ids']),'resultIds':value['ids']}
              for key,value in results.items() if len(value['ids'])>1]
    if repeated: out['duplicateErrorReceipts']=repeated
    if len(finals)!=1: stop('native final missing or ambiguous')
    final_id,sequence=finals[0]
    text=con.execute('SELECT content FROM cowork_messages WHERE session_id=? AND id=?',(session['id'],final_id)).fetchone()[0]
    if not isinstance(text,str) or not text.strip(): stop('native final is empty')
    body=text.encode('utf8')
    out['final']={'id':final_id,'sequence':sequence,'sha256':hashlib.sha256(body).hexdigest(),'bytes':len(body),'text':text,'isFinal':True}
    out['projectionStatus']='available'; out['reason']='unique completed native final'
except Exception as error:
    if out['reason']=='native observation unavailable': out['reason']=type(error).__name__+': '+str(error)
    out['projectionStatus']='unavailable'; out['final']=None
encoded=json.dumps(out,ensure_ascii=False,sort_keys=True,separators=(',',':')).encode('utf8')
if len(encoded)>64*1024*1024:
    out['final']=None; out['projectionStatus']='unavailable'; out['reason']='native projection exceeds capture bound'
    encoded=json.dumps(out,ensure_ascii=False,sort_keys=True,separators=(',',':')).encode('utf8')
sys.stdout.buffer.write(encoded+b'\n')
"#;

/// Preserve an observation failure as unknown, without claiming native termination.
pub(crate) fn unavailable(reason: &str) -> Value {
    serde_json::json!({"source": OBSERVATION_SOURCE, "nativeTerminated": false,
        "projectionStatus": "unavailable", "reason": crate::redact::redact_full(reason), "final": null})
}

/// Retain provenance and hashes without duplicating the full answer in metadata.
pub(crate) fn without_text(observation: &Value) -> Value {
    let mut value = observation.clone();
    if let Some(final_value) = value.get_mut("final").and_then(Value::as_object_mut) {
        final_value.remove("text");
    }
    value
}

/// Read one consistent, bounded native database snapshot and bind it to captured raw bytes.
pub(crate) fn inspect_native_final(
    root: &Path, database: &Path, session: &str, cwd: &Path, prompt_sha: &str, raw: &[u8],
    stream_closed: bool,
) -> Result<Value> {
    if !stream_closed {
        let mut observation = unavailable("raw writer has not closed; native read not attempted; HOLD");
        observation["runtimeSessionId"] = session.into();
        observation["cwd"] = serde_json::json!(cwd);
        observation["promptSha256"] = prompt_sha.into();
        observation["raw"] = serde_json::json!({"sha256":hex::encode(Sha256::digest(raw)),"bytes":raw.len(),"streamClosed":false});
        return Ok(observation);
    }
    let (output_path, output) = super::channel_capture_file(root, "native-observation")?;
    let (error_path, error_file) = super::channel_capture_file(root, "native-observation-error")?;
    let mut child = Command::new("/usr/bin/python3")
        .args(["-I", "-c", NATIVE_READ]).arg(database).arg(session).arg(cwd).arg(prompt_sha)
        .current_dir(root).env_clear().env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null()).stdout(Stdio::from(output.try_clone()?))
        .stderr(Stdio::from(error_file.try_clone()?)).spawn()
        .context("start bounded read-only native observer")?;
    let status = match child.wait_timeout(Duration::from_secs(5))? {
        Some(status) => status,
        None => {
            let _ = child.kill(); // only this owned read-only observer, never the native backend.
            let reaped = child.wait_timeout(Duration::from_secs(2))?.is_some();
            if reaped { let _ = fs::remove_file(&output_path); let _ = fs::remove_file(&error_path); }
            bail!("bounded native observation timed out; observerReaped={reaped}");
        }
    };
    let bytes = super::read_channel_capture(&output);
    let stderr = super::read_channel_capture(&error_file);
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(&error_path);
    if !status.success() {
        bail!("native observation failed: {}", crate::redact::redact_full(&String::from_utf8_lossy(&stderr?)));
    }
    let mut observation: Value = serde_json::from_slice(&bytes?).context("native observation is not JSON")?;
    if observation["source"] != OBSERVATION_SOURCE || observation["runtimeSessionId"] != session
        || observation["cwd"].as_str() != cwd.to_str() || observation["promptSha256"] != prompt_sha {
        bail!("native observation request binding mismatch");
    }
    if observation["projectionStatus"] == "available" {
        let text = observation.pointer("/final/text").and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty()).context("native final text is empty")?;
        if observation["nativeTerminated"] != true || observation["nativeSession"]["status"] != "completed"
            || observation["nativeSession"]["cwd"].as_str() != cwd.to_str()
            || observation["nativeSession"]["title"] != format!("multica:{session}")
            || observation["request"]["sha256"] != prompt_sha || observation["final"]["isFinal"] != true
            || observation["final"]["bytes"].as_u64() != Some(text.len() as u64)
            || observation["final"]["sha256"] != hex::encode(Sha256::digest(text.as_bytes()))
            || observation["final"]["sequence"].as_u64().zip(observation["request"]["sequence"].as_u64())
                .is_none_or(|(final_sequence, request_sequence)| final_sequence <= request_sequence)
            || ["/nativeSession/id", "/request/id", "/final/id"].into_iter()
                .any(|key| observation.pointer(key).and_then(Value::as_str).is_none_or(|id| id.trim().is_empty())) {
            bail!("native final provenance is inconsistent");
        }
    } else if !observation["final"].is_null() {
        bail!("unavailable native observation contains an answer");
    }
    observation["raw"] = serde_json::json!({"sha256":hex::encode(Sha256::digest(raw)),"bytes":raw.len(),"streamClosed":stream_closed});
    // The bound native record owns final-answer authority. Raw is immutable
    // evidence of what the bridge returned; framing, truncation or transcript
    // content neither grants nor vetoes that independent native proof.
    Ok(observation)
}

/// Derive the store from the invocation's captured HOME, never from a later reader's environment.
pub(crate) fn database_from_environment(env: &std::collections::BTreeMap<String, String>) -> Option<std::path::PathBuf> {
    env.get("HOME").map(|home| std::path::PathBuf::from(home)
        .join("Library/Application Support/DewuSmartClaw/dewusmartclaw.sqlite"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture(root: &Path, status: &str, prompt: &str, final_text: &str, variant: &str) -> std::path::PathBuf {
        fs::create_dir_all(root).unwrap();
        let database = root.join(format!("{variant}.sqlite"));
        let input = json!({"cwd":root,"status":status,"prompt":prompt,"final":final_text,"variant":variant});
        let script = r#"
import json,sqlite3,sys
db,data=sys.argv[1:]; x=json.loads(data); c=sqlite3.connect(db)
c.executescript('CREATE TABLE cowork_sessions(id TEXT PRIMARY KEY,title TEXT,status TEXT,cwd TEXT); CREATE TABLE cowork_messages(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,content TEXT,metadata TEXT,sequence INTEGER);')
c.execute('INSERT INTO cowork_sessions VALUES(?,?,?,?)',('native-1','multica:orch-wake-native-fixture',x['status'],x['cwd']))
def add(i,t,body,meta,seq): c.execute('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',(i,'native-1',t,body,json.dumps(meta),seq))
add('user-1','user',x['prompt'],{},0)
add('tool-1','tool_use','',{'toolUseId':'tool-1'},1)
if x['variant']!='pending-tool': add('result-1','tool_result','tool output',{'toolUseId':'tool-1'},2)
add('process-1','assistant','PROCESS PASS text that is not the final',{},3)
if x['variant']!='missing-final': add('final-1','assistant',x['final'],{'isFinal': True if x['variant']!='string-final-flag' else 'true'},4)
if x['variant']=='duplicate-final': add('final-2','assistant','another final',{'isFinal':True},5)
if x['variant']=='duplicate-user': add('user-2','user',x['prompt'],{},6)
c.commit(); c.close()
"#;
        assert!(Command::new("/usr/bin/python3").args(["-I", "-c", script]).arg(&database)
            .arg(input.to_string()).status().unwrap().success());
        database
    }

    fn raw() -> Vec<u8> {
        br#"{"payloads":[{"text":"PROCESS text and a quoted PASS; this is not a native projection"}],"meta":{"agentMeta":{"sessionId":"orch-wake-native-fixture"}}}"#.to_vec()
    }

    #[test]
    fn rendered_request_crosses_actual_bridge_and_native_store_without_byte_drift() {
        use crate::channel::{capture_attachment_manifest_v1, prepare_invocation,
            render_invocation_v1, preflight_invocation_v1, InvocationAction,
            InvocationContextV1, InvocationRequest};
        use std::time::Instant;
        let root = crate::util::test_scratch_dir("b327-rendered-native-chain");
        let root = fs::canonicalize(root).unwrap();
        let wrapper = root.join("coordination/scripts/wake-multica.sh");
        fs::create_dir_all(wrapper.parent().unwrap()).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap()
            .join("coordination/scripts/wake-multica.sh");
        fs::copy(source, &wrapper).unwrap();
        for args in [vec!["init", "-q"], vec!["add", "coordination/scripts/wake-multica.sh"],
            vec!["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid", "commit", "-qm", "fixture"]] {
            assert!(Command::new("git").arg("-C").arg(&root).args(args).status().unwrap().success());
        }
        let head = crate::gitx::rev_parse(&root, "HEAD").unwrap();
        let config = crate::harness_config::parse_harness_config_snapshot(&root.join(".orch/harnesses.yaml"),
            "version: 1\nharnesses:\n  bridge:\n    driver: smartclaw\n    executable: /bin/sh\n    enabled: true\n    cwdPolicy: project-root\n").unwrap();
        let prompt = "完整材料 知🧭 e\u{301}\nQuoted \\\"body\\\"\r\n";
        let request = InvocationRequest { alias: "bridge".into(), action: InvocationAction::Consult,
            prompt: prompt.into(), project_root: root.clone(), target_worktree: Default::default(),
            target_head: head, attachments: capture_attachment_manifest_v1(&[]).unwrap() };
        let context = InvocationContextV1 { action_id: "fixture".into(), wake_id: "native-fixture".into(),
            round: "r84".into(), task_id: "CONSULT".into(), attempt_id: "CONSULT-A0000".into(),
            review_output: None, orch_executable: "/bin/sh".into(), deadline_secs: 5 };
        let rendered = render_invocation_v1(prepare_invocation(&config, request.clone()).unwrap(), context.clone()).unwrap();
        let admitted = preflight_invocation_v1(rendered).unwrap();
        let rendered = admitted.rendered();
        let expected_sha = hex::encode(Sha256::digest(rendered.prepared().prompt().as_bytes()));
        let database = root.join("peer.sqlite");
        let peer_script = r#"
import json,pathlib,socket,sqlite3
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(5); s.bind('bridge.sock'); s.listen(1)
pathlib.Path('ready').write_text('ready'); c,_=s.accept(); c.settimeout(5); text=''
while not text.endswith('\n'):
    chunk=c.recv(1)
    if not chunk: raise RuntimeError('request truncated')
    text+=chunk.decode('utf8','replace')
r=json.loads(text); db=sqlite3.connect('peer.sqlite')
db.executescript('CREATE TABLE cowork_sessions(id TEXT,title TEXT,status TEXT,cwd TEXT); CREATE TABLE cowork_messages(id TEXT,session_id TEXT,type TEXT,content TEXT,metadata TEXT,sequence INTEGER);')
db.execute('INSERT INTO cowork_sessions VALUES(?,?,?,?)',('native-chain','multica:'+r['sessionId'],'completed',r['cwd']))
db.execute('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',('user-chain','native-chain','user',r['prompt'],'{}',0))
db.execute('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',('final-chain','native-chain','assistant','精确终答 🧭',json.dumps({'isFinal':True}),1))
db.commit(); db.close()
response=json.dumps({'payloads':[{'text':'PROCESS text is not the native final'}],'meta':{'agentMeta':{'sessionId':r['sessionId']}}},ensure_ascii=False).encode('utf8')
pathlib.Path('response.bin').write_bytes(response)
for b in response: c.sendall(bytes([b]))
c.close(); s.close()
"#;
        let mut peer = Command::new("/usr/bin/python3").args(["-I", "-c", peer_script])
            .current_dir(&root).spawn().unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while !root.join("ready").exists() && Instant::now() < until {
            assert!(peer.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(10));
        }
        if !root.join("ready").exists() {
            let _ = peer.kill(); let _ = peer.wait(); panic!("fixture peer did not become ready");
        }
        // Use the production render's exact argv, cwd and controlled environment.
        // Only the socket address selects the owned peer, never the user's store.
        let output = Command::new(&rendered.argv()[0]).args(&rendered.argv()[1..])
            .current_dir(rendered.cwd()).env_clear().envs(rendered.env())
            .env("ORCH_MULTICA_SOCK", "bridge.sock").output().unwrap();
        assert!(peer.wait().unwrap().success());
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, fs::read(root.join("response.bin")).unwrap());
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root,
            &expected_sha, &output.stdout, true).unwrap();
        assert_eq!(value["projectionStatus"], "available", "{value}");
        assert_eq!(value["request"]["sha256"], expected_sha);
        assert_eq!(value["final"]["text"], "精确终答 🧭");
        assert_eq!(value["raw"]["bytes"], output.stdout.len());

        // Admission rejects a symlink identity; it does not normalize away a
        // cwd mismatch after the request has already been sent to the backend.
        let alias = root.join("cwd-alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        let mut aliased = request;
        aliased.project_root = alias;
        let rejection = prepare_invocation(&config, aliased)
            .and_then(|prepared| render_invocation_v1(prepared, context))
            .and_then(preflight_invocation_v1).unwrap_err();
        assert!(format!("{rejection:#}").contains("symlink"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_authority_does_not_depend_on_raw_framing_or_transcript_content() {
        let root = crate::util::test_scratch_dir("b327-native-raw-separation");
        let prompt = "exact source request";
        let expected = hex::encode(Sha256::digest(prompt.as_bytes()));
        let database = fixture(&root, "completed", prompt, "authoritative native final", "raw-separation");
        let mut duplicate = raw(); duplicate.push(b'\n'); duplicate.extend_from_slice(&raw());
        let mut truncated = raw(); truncated.extend_from_slice(b"\n{\"payl");
        let mut crlf = raw(); crlf.extend_from_slice(b"\r\n\n");
        for captured in [Vec::new(), raw(), duplicate, truncated, crlf, b"PROCESS PASS tool-only text".to_vec()] {
            let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root,
                &expected, &captured, true).unwrap();
            assert_eq!(value["projectionStatus"], "available");
            assert_eq!(value["final"]["text"], "authoritative native final");
            assert_eq!(value["raw"]["sha256"], hex::encode(Sha256::digest(&captured)));
            assert_eq!(value["raw"]["bytes"], captured.len());
        }
        // Legacy consumers still require an LF-complete payload. Native proof
        // does not change or lend authority to that historical parser.
        assert!(crate::wake::validate_smartclaw_payload_terminal_for_test(&raw(), "orch-wake-native-fixture").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unclosed_capture_never_starts_the_native_reader() {
        let root = crate::util::test_scratch_dir("b327-unclosed-reader");
        let database = root.join("does-not-exist.sqlite");
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root,
            &"a".repeat(64), &raw(), false).unwrap();
        assert_eq!(value["nativeTerminated"], false);
        assert_eq!(value["projectionStatus"], "unavailable");
        assert_eq!(value["raw"]["streamClosed"], false);
        assert!(!root.join(".cowork-temp").exists(), "native observer capture creation proves a query was started");
        assert!(!database.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_projection_uses_exact_completed_final_and_binds_received_raw() {
        let root = crate::util::test_scratch_dir("b327-native-final");
        let prompt = "input 知🧭";
        let final_text = "完整终答\nQuoted PROCESS and PASS markers stay in the final.\n";
        let database = fixture(&root, "completed", prompt, final_text, "good");
        let expected = hex::encode(Sha256::digest(prompt.as_bytes()));
        let raw = raw();
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root, &expected, &raw, true).unwrap();
        assert_eq!(value["nativeTerminated"], true);
        assert_eq!(value["projectionStatus"], "available");
        assert_eq!(value["final"]["text"], final_text);
        assert_eq!(value["final"]["sequence"], 4);
        assert_eq!(value["raw"]["sha256"], hex::encode(Sha256::digest(&raw)));
        assert_eq!(value["raw"]["bytes"], raw.len());
        assert!(without_text(&value)["final"].get("text").is_none());
        let missing_wire = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root, &expected, b"tool-only raw", true).unwrap();
        assert_eq!(missing_wire["projectionStatus"], "available");
        assert_eq!(missing_wire["final"]["text"], final_text);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_running_ambiguous_input_pending_tools_and_bad_finals_never_become_answers() {
        let root = crate::util::test_scratch_dir("b327-native-negatives");
        let prompt = "exact request";
        let expected = hex::encode(Sha256::digest(prompt.as_bytes()));
        for (variant, status, text) in [
            ("running", "running", "final-looking text"),
            ("error", "error", "final-looking text"),
            ("pending-tool", "completed", "final"),
            ("duplicate-user", "completed", "final"),
            ("duplicate-final", "completed", "final"),
            ("missing-final", "completed", "final"),
            ("string-final-flag", "completed", "final"),
            ("empty-final", "completed", " \n\t"),
        ] {
            let database = fixture(&root, status, prompt, text, variant);
            let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root, &expected, &raw(), true).unwrap();
            assert_eq!(value["projectionStatus"], "unavailable", "{variant}: {value}");
            assert!(value["final"].is_null(), "{variant}");
            if matches!(variant, "running" | "error" | "pending-tool" | "duplicate-user") {
                assert_eq!(value["nativeTerminated"], false, "{variant}");
            }
        }
        let database = fixture(&root, "completed", "different request", "final", "input-drift");
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture", &root, &expected, &raw(), true).unwrap();
        assert_eq!(value["nativeTerminated"], false);
        assert!(value["final"].is_null());
        fs::remove_dir_all(root).unwrap();
    }

    fn receipt_graph_fixture(root: &Path, variant: &str) -> std::path::PathBuf {
        let database = root.join(format!("{variant}.sqlite"));
        let input = json!({"cwd": root, "variant": variant});
        let script = r#"
import json,sqlite3,sys
database,encoded=sys.argv[1:]; data=json.loads(encoded); variant=data['variant']
con=sqlite3.connect(database)
con.executescript('CREATE TABLE cowork_sessions(id TEXT PRIMARY KEY,title TEXT,status TEXT,cwd TEXT); CREATE TABLE cowork_messages(id TEXT PRIMARY KEY,session_id TEXT,type TEXT,content TEXT,metadata TEXT,sequence INTEGER);')
status=variant if variant in ('running','error') else 'completed'
cwd=data['cwd'] if variant!='wrong-cwd' else data['cwd']+'/different'
con.execute('INSERT INTO cowork_sessions VALUES(?,?,?,?)',('native-errors','multica:orch-wake-native-fixture',status,cwd))
rows=[]
def add(identity,kind,text,metadata): rows.append((identity,'native-errors',kind,text,json.dumps(metadata),len(rows)))
prompt='exact denial request' if variant!='wrong-request' else 'different request'
add('user','user',prompt,{})
add('use','tool_use','',{'toolUseId':'tool-1'})
if variant=='duplicate-use': add('use-copy','tool_use','',{'toolUseId':'tool-1'})
first={'toolUseId':'tool-1','toolName':'Bash','isError':True}
second=dict(first)
if variant in ('duplicate-success','mixed-first-success'): first['isError']=False
if variant in ('duplicate-success','mixed-second-success'): second['isError']=False
if variant=='missing-error-flag': second.pop('isError')
if variant=='string-error-flag': second['isError']='true'
if variant=='numeric-error-flag': second['isError']=1
if variant=='unknown-id': second['toolUseId']='never-declared'
if variant!='pending':
    add('result-first','tool_result','The tool was not executed. Permission denied.',first)
    if variant=='reused-use': add('use-again','tool_use','',{'toolUseId':'tool-1'})
    if variant!='duplicate-use':
        add('denial-note','system','The native policy rejected the tool.',{})
        add('result-second','tool_result','Permission denied.',second)
if variant=='duplicate-user': add('second-user','user',prompt,{})
if variant!='missing-final':
    text=' ' if variant=='empty-final' else 'complete native review'
    final_flag='true' if variant=='string-final-flag' else True
    add('final','assistant',text,{'isFinal':final_flag})
    if variant=='duplicate-final': add('final-copy','assistant',text,{'isFinal':True})
con.executemany('INSERT INTO cowork_messages VALUES(?,?,?,?,?,?)',rows)
con.commit();con.close()
"#;
        assert!(Command::new("/usr/bin/python3").args(["-I", "-c", script])
            .arg(&database).arg(input.to_string()).status().unwrap().success());
        database
    }

    #[test]
    fn completed_native_duplicate_error_receipts_preserve_each_receipt() {
        let root = crate::util::test_scratch_dir("r86-native-duplicate-errors");
        let database = receipt_graph_fixture(&root, "duplicate-errors");
        let expected = hex::encode(Sha256::digest(b"exact denial request"));
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture",
            &root, &expected, b"closed but truncated raw", true).unwrap();
        assert_eq!(value["nativeTerminated"], true, "{value}");
        assert_eq!(value["projectionStatus"], "available", "{value}");
        assert_eq!(value["final"]["text"], "complete native review");
        assert_eq!(value["duplicateErrorReceipts"], json!([{
            "toolUseId":"tool-1", "resultCount":2,
            "resultIds":["result-first","result-second"]
        }]));
        assert!(without_text(&value)["final"].get("text").is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn duplicate_uses_unknown_results_and_successful_replays_remain_untrusted() {
        let root = crate::util::test_scratch_dir("r86-native-error-graph-negative");
        let expected = hex::encode(Sha256::digest(b"exact denial request"));
        for variant in ["unknown-id", "duplicate-use", "reused-use",
            "duplicate-success", "mixed-first-success", "mixed-second-success",
            "missing-error-flag", "string-error-flag", "numeric-error-flag"] {
            let database = receipt_graph_fixture(&root, variant);
            let value = inspect_native_final(&root, &database, "orch-wake-native-fixture",
                &root, &expected, &raw(), true).unwrap();
            assert_eq!(value["nativeTerminated"], false, "{variant}: {value}");
            assert_eq!(value["projectionStatus"], "unavailable", "{variant}: {value}");
            assert!(value["final"].is_null(), "{variant}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn error_receipt_replays_never_replace_completion_or_final_identity() {
        let root = crate::util::test_scratch_dir("r86-native-error-anchor-negative");
        let expected = hex::encode(Sha256::digest(b"exact denial request"));
        for variant in ["running", "error", "pending", "wrong-cwd", "wrong-request",
            "duplicate-user", "missing-final", "duplicate-final", "empty-final",
            "string-final-flag"] {
            let database = receipt_graph_fixture(&root, variant);
            let value = inspect_native_final(&root, &database, "orch-wake-native-fixture",
                &root, &expected, &raw(), true).unwrap();
            assert_eq!(value["projectionStatus"], "unavailable", "{variant}: {value}");
            assert!(value["final"].is_null(), "{variant}");
        }
        let database = fixture(&root, "completed", "exact denial request", "ordinary final", "ordinary");
        let value = inspect_native_final(&root, &database, "orch-wake-native-fixture",
            &root, &expected, &raw(), true).unwrap();
        assert_eq!(value["projectionStatus"], "available");
        assert!(value.get("duplicateErrorReceipts").is_none(),
            "normal historical observations must not acquire an empty new field");
        fs::remove_dir_all(root).unwrap();
    }
}
