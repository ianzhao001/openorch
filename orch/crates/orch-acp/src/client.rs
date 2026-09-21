//! One finite ACP v1 client. Native startup varies; protocol and answer rules do not.
#![deny(missing_docs)]
use agent_client_protocol::{self as acp, ByteStreams, ConnectionTo, Agent};
use acp::schema::{ProtocolVersion,v1::*};
use orch_core::{acp::{AcpRequest,AcpAnswer,AcpFailure,AcpTarget},protocol_io::{BoundedReader,BoundedWriter}};
use serde::{Serialize,Deserialize};
use sha2::{Digest,Sha256};
use serde_json::{json,Value};
use std::{collections::{BTreeMap,BTreeSet},io,pin::Pin,sync::{Arc,Mutex,atomic::{AtomicBool,Ordering}},task::{Context,Poll},time::Duration};
use tokio::io::{AsyncRead,AsyncReadExt,AsyncWrite,ReadBuf};
use tokio_util::compat::{TokioAsyncReadCompatExt,TokioAsyncWriteCompatExt};
const FRAME_LIMIT:usize=2*1024*1024;
const STREAM_LIMIT:usize=16*1024*1024;
const ANSWER_LIMIT:usize=512*1024;
#[derive(Default)]
struct Observation {
    session:Option<String>,phase:&'static str,prompt_sent:bool,pinned:bool,
    pins:BTreeMap<String,String>,expected_model:Option<String>,text:String,violation:Option<&'static str>,
    denied_permissions:u64,observed_models:BTreeSet<String>,native_success:bool,
}
impl Observation {
    fn evidence(&self)->Value {json!({"phase":self.phase,"selected":self.pins,"deniedPermissions":self.denied_permissions,"observedModels":self.observed_models,"nativeResultSuccess":self.native_success})}
    fn fail(&mut self,code:&'static str) {if self.violation.is_none(){self.violation=Some(code);}}
    fn session_matches(&mut self,id:&str)->bool {
        if self.session.as_deref().is_some_and(|expected|expected!=id){self.fail("acp_session_identity_changed");return false;}true
    }
}
type Shared=Arc<Mutex<Observation>>;
fn lock(s:&Shared)->std::sync::MutexGuard<'_,Observation>{s.lock().unwrap_or_else(|e|e.into_inner())}
fn rpc_error(code:&'static str)->acp::Error {acp::Error::new(acp::ErrorCode::InternalError.into(),code)}
fn check_options(options:&Value,pins:&BTreeMap<String,String>)->bool {
    pins.iter().all(|(id,value)|find_option(options,id).and_then(|o|o.get("currentValue")).and_then(Value::as_str)==Some(value.as_str()))
}
fn find_option<'a>(options:&'a Value,id:&str)->Option<&'a Value> {options.as_array()?.iter().find(|o|o["id"]==id)}
fn choices(option:&Value)->Vec<&Value> {
    option.get("options").and_then(Value::as_array).into_iter().flatten().flat_map(|o|{
        if let Some(group)=o.get("options").and_then(Value::as_array){group.iter().collect::<Vec<_>>()}else{vec![o]}
    }).collect()
}
fn model_id(request:&AcpRequest)->Result<String,&'static str> {
    if request.target==AcpTarget::OpenCode {
        if let Some(provider)=&request.provider {
            if request.model.contains('/') {
                if !request.model.starts_with(&format!("{provider}/")){return Err("model_provider_mismatch");}
                return Ok(request.model.clone());
            }
            return Ok(format!("{provider}/{}",request.model));
        }
    }
    Ok(request.model.clone())
}
fn model_choice(request:&AcpRequest,options:&Value)->Result<String,&'static str> {
    let option=find_option(options,"model").ok_or("acp_model_option_missing")?;
    let expected=model_id(request)?;
    let rows=choices(option);
    if rows.iter().any(|row|row["value"]==expected){return Ok(expected);}
    // The native Claude CLI exposes custom model IDs through its picker aliases.
    // Every such alias is explicitly pinned in this launch; no fuzzy match is accepted.
    if request.target==AcpTarget::Claude {
        if let Some(row)=rows.iter().find(|row|row["name"]==request.model && matches!(row["value"].as_str(),Some("opus"|"sonnet"|"haiku"))) {
            return Ok(row["value"].as_str().unwrap().into());
        }
    }
    Err("acp_model_not_advertised")
}
async fn select(connection:&ConnectionTo<Agent>,session:&SessionId,id:&str,value:&str,options:&Value)->Result<Value,acp::Error> {
    let option=find_option(options,id).ok_or_else(||rpc_error(if id=="effort"||id=="reasoning_effort"{"unsupported_effort"}else{"acp_option_missing"}))?;
    if !choices(option).iter().any(|row|row["value"]==value){return Err(rpc_error(if id=="effort"||id=="reasoning_effort"{"unsupported_effort"}else{"acp_option_value_unavailable"}));}
    let response=connection.send_request(SetSessionConfigOptionRequest::new(session.clone(),id.to_string(),SessionConfigOptionValue::value_id(value.to_string()))).block_task().await?;
    let options=serde_json::to_value(response.config_options).map_err(|_|rpc_error("acp_config_invalid"))?;
    if find_option(&options,id).and_then(|o|o.get("currentValue")).and_then(Value::as_str)!=Some(value){return Err(rpc_error("acp_config_not_honored"));}
    Ok(options)
}
#[derive(Debug,Clone,Serialize,Deserialize,acp::JsonRpcNotification)]
#[notification(method="_claude/sdkMessage")]
#[serde(rename_all="camelCase")]
struct ClaudeMessage {session_id:String,message:Value}
struct Closable(Arc<Mutex<Option<tokio::process::ChildStdin>>>);
impl AsyncWrite for Closable {
    fn poll_write(self:Pin<&mut Self>,cx:&mut Context<'_>,bytes:&[u8])->Poll<io::Result<usize>>{
        let mut guard=self.0.lock().unwrap_or_else(|e|e.into_inner());match guard.as_mut(){Some(w)=>Pin::new(w).poll_write(cx,bytes),None=>Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))}
    }
    fn poll_flush(self:Pin<&mut Self>,cx:&mut Context<'_>)->Poll<io::Result<()>>{
        let mut guard=self.0.lock().unwrap_or_else(|e|e.into_inner());match guard.as_mut(){Some(w)=>Pin::new(w).poll_flush(cx),None=>Poll::Ready(Ok(()))}
    }
    fn poll_shutdown(self:Pin<&mut Self>,_:&mut Context<'_>)->Poll<io::Result<()>>{
        self.0.lock().unwrap_or_else(|e|e.into_inner()).take();Poll::Ready(Ok(()))
    }
}
struct TrackEof<R> {inner:R,eof:Arc<AtomicBool>}
impl<R:AsyncRead+Unpin> AsyncRead for TrackEof<R> {
    fn poll_read(mut self:Pin<&mut Self>,cx:&mut Context<'_>,buffer:&mut ReadBuf<'_>)->Poll<io::Result<()>> {
        let before=buffer.filled().len();let capacity=buffer.remaining();
        let result=Pin::new(&mut self.inner).poll_read(cx,buffer);
        if capacity>0 && matches!(result,Poll::Ready(Ok(()))) && buffer.filled().len()==before {self.eof.store(true,Ordering::Release);}
        result
    }
}
fn command(request:&AcpRequest,policy_probe:bool)->Result<tokio::process::Command,&'static str> {
    if std::fs::canonicalize(&request.project).ok().as_ref()!=Some(&request.project){return Err("acp_project_identity_changed");}
    if !request.home.is_dir() || !request.isolation_dir.is_dir(){return Err("acp_native_directories_unavailable");}
    let entry=request.adapter.as_ref().unwrap_or(&request.executable);
    let mut cmd=if let Some(node)=&request.node {let mut c=tokio::process::Command::new(node);c.arg(entry);c} else {tokio::process::Command::new(entry)};
    cmd.current_dir(&request.project).env("HOME",&request.home)
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).kill_on_drop(true);
    // No process_group(0): existing channel owns the bridge group and descendants.
    match request.target {
        AcpTarget::OpenCode=>{
            if policy_probe {cmd.args(["debug","agent","plan","--pure"]);} else {cmd.args(["acp","--pure","--cwd"]).arg(&request.project);}
            if let Some(config)=&request.config_dir {cmd.env("XDG_CONFIG_HOME",config);}
            let model=model_id(request)?;
            cmd.env("OPENCODE_CONFIG_CONTENT",json!({"model":model,"small_model":model,"permission":"deny","agent":{"plan":{"permission":"deny","model":model}},"compaction":{"auto":false,"prune":false},"share":"disabled","autoupdate":false}).to_string());
        }
        AcpTarget::Codex=>{
            cmd.env("CODEX_PATH",&request.executable).env("INITIAL_AGENT_MODE","read-only");
            if let Some(config)=&request.config_dir {cmd.env("CODEX_HOME",config);}
            if let Some(provider)=&request.provider {cmd.env("MODEL_PROVIDER",provider);}
            // Provider definitions/authentication belong to the captured CODEX_HOME.
            let mut config=json!({"model":request.model,"sandbox_mode":"read-only","approval_policy":"never","features":{"multi_agent":false}});
            if let Some(provider)=&request.provider {config["model_provider"]=json!(provider);}
            if let Some(effort)=&request.effort {config["model_reasoning_effort"]=json!(effort);}
            cmd.env("CODEX_CONFIG",config.to_string());
        }
        AcpTarget::Claude=>{
            if request.provider.as_deref().is_some_and(|p|p!="deepseek" && p!="anthropic") {return Err("acp_provider_mapping_unsupported");}
            if let Some(url)=&request.provider_url {cmd.env("ANTHROPIC_BASE_URL",url).env_remove("CLAUDE_CODE_USE_BEDROCK").env_remove("CLAUDE_CODE_USE_VERTEX");}
            cmd.env("CLAUDE_CODE_EXECUTABLE",&request.executable).env("DISABLE_AUTOUPDATER","1")
                .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC","1").env("DISABLE_TELEMETRY","1").env("DISABLE_AUTO_COMPACT","1");
            if let Some(config)=&request.config_dir {cmd.env("CLAUDE_CONFIG_DIR",config);}
            for key in ["ANTHROPIC_MODEL","ANTHROPIC_DEFAULT_OPUS_MODEL","ANTHROPIC_DEFAULT_SONNET_MODEL","ANTHROPIC_DEFAULT_HAIKU_MODEL","ANTHROPIC_SMALL_FAST_MODEL"] {cmd.env(key,&request.model);}
            if let Some(effort)=&request.effort {cmd.env("CLAUDE_CODE_EFFORT_LEVEL",effort);}
        }
    }
    Ok(cmd)
}
fn failure(request:&AcpRequest,shared:&Shared,code:impl Into<String>)->AcpFailure {
    let state=lock(shared);AcpFailure{version:1,id:request.id.clone(),request_digest:request.request_digest.clone(),code:code.into(),prompt_sent:state.prompt_sent,evidence:state.evidence()}
}
async fn policy_bytes(reader:impl AsyncRead+Unpin)->io::Result<Vec<u8>> {
    let mut bytes=Vec::new();reader.take((FRAME_LIMIT+1) as u64).read_to_end(&mut bytes).await?;
    if bytes.len()>FRAME_LIMIT {return Err(io::ErrorKind::InvalidData.into());}Ok(bytes)
}
async fn native_policy(request:&AcpRequest)->Result<Option<Value>,&'static str> {
    if request.target!=AcpTarget::OpenCode {return Ok(None);}
    // Agent-level rules are appended after global rules by this native version.
    // A scalar deny overlay can leave older specific allows behind after deep merge.
    // Inspect the final ordered rules in memory, with exactly the ACP launch env.
    let mut child=command(request,true)?.spawn().map_err(|_|"acp_native_policy_probe_failed")?;
    drop(child.stdin.take());
    let stdout=child.stdout.take().unwrap();let stderr=child.stderr.take().unwrap();
    let result=tokio::time::timeout(Duration::from_secs(request.timeout_secs.min(10)),async {
        let (status,bytes,_stderr)=futures::try_join!(child.wait(),policy_bytes(stdout),policy_bytes(stderr))?;
        if !status.success(){return Err(io::ErrorKind::InvalidData.into());}
        Ok::<_,io::Error>(bytes)
    }).await;
    let bytes=match result {
        Ok(Ok(bytes))=>bytes,
        _=>{let _=child.kill().await;let _=child.wait().await;return Err("acp_native_policy_probe_failed");}
    };
    let value:Value=serde_json::from_slice(&bytes).map_err(|_|"acp_native_policy_unverified")?;
    if value["name"]!="plan" {return Err("acp_native_policy_unverified");}
    let rules=value["permission"].as_array().ok_or("acp_native_policy_unverified")?;
    if rules.iter().any(|r|r["permission"].as_str().is_none() || r["pattern"].as_str().is_none() || !matches!(r["action"].as_str(),Some("deny"|"allow"|"ask"))) {return Err("acp_native_policy_unverified");}
    let boundary=rules.iter().rposition(|r|r["permission"]=="*" && r["pattern"]=="*" && r["action"]=="deny").ok_or("acp_native_policy_unverified")?;
    // external_directory is a secondary path check, not an executable tool.
    // Native OpenCode appends its own truncated-output path allowance. Every
    // actual tool remains disabled by the final wildcard unless a later rule
    // re-enables it; reject every such allow/ask, including unknown tool names.
    if rules[boundary+1..].iter().any(|r|r["permission"]!="external_directory" && r["action"]!="deny") {return Err("acp_native_policy_allows_tools");}
    let digest=hex::encode(Sha256::digest(serde_json::to_vec(rules).map_err(|_|"acp_native_policy_unverified")?));
    Ok(Some(json!({"kind":"effective-plan-permissions","verified":true,"rulesSha256":digest,"rules":rules.len(),"probeModelPrompts":0})))
}
/// Production bridge entry: verify native effective policy before the ACP prompt.
/// OpenCode's merged agent rules are inspected without retaining raw config or
/// stderr; other fixed adapters use their explicit native tool-disable options.
/// Metadata processes inherit the caller's existing managed group, as does ACP.
pub async fn consult_verified(mut request:AcpRequest)->Result<AcpAnswer,AcpFailure> {
    let shared=Arc::new(Mutex::new(Observation::default()));lock(&shared).phase="native-policy";
    request.validate().map_err(|code|failure(&request,&shared,code))?;
    let began=std::time::Instant::now();
    let policy=native_policy(&request).await.map_err(|code|failure(&request,&shared,code))?;
    if policy.is_some() {
        let spent=((began.elapsed().as_millis()+999)/1000) as u64;
        request.timeout_secs=request.timeout_secs.saturating_sub(spent);
        if request.timeout_secs<4 {return Err(failure(&request,&shared,"acp_deadline_exceeded"));}
    }
    let mut answer=consult(request).await?;
    if let Some(policy)=policy {answer.evidence["nativePolicy"]=policy;}
    Ok(answer)
}
/// Run the wire-level ACP client with explicit mode/model/effort negotiation.
/// Production callers must use [`consult_verified`] to verify merged native
/// permissions first; this entry supports protocol-only adapter contract tests.

/// The caller retains outer process-group ownership and verifies the returned envelope.
pub async fn consult(request:AcpRequest)->Result<AcpAnswer,AcpFailure> {
    let shared=Arc::new(Mutex::new(Observation::default()));
    request.validate().map_err(|code|failure(&request,&shared,code))?;
    let mut command=command(&request,false).map_err(|code|failure(&request,&shared,code))?;
    let mut child=command.spawn().map_err(|_|failure(&request,&shared,"acp_spawn_failed"))?;
    let stdin=Arc::new(Mutex::new(child.stdin.take()));
    let stdout_eof=Arc::new(AtomicBool::new(false));let stderr_eof=Arc::new(AtomicBool::new(false));
    let stdout=TrackEof{inner:child.stdout.take().unwrap(),eof:stdout_eof.clone()};
    let stderr=TrackEof{inner:child.stderr.take().unwrap(),eof:stderr_eof.clone()};
    let mut stderr_job=tokio::spawn(async move {
        let mut reader=BoundedReader::new(stderr,FRAME_LIMIT).with_total_limit(STREAM_LIMIT);
        let mut buf=[0u8;8192];let mut total=0usize;
        loop {match reader.read(&mut buf).await {Ok(0)=>return Ok(total),Ok(n)=>total+=n,Err(e)=>return Err(e)}}
    });
    let notifications=shared.clone();let raw=shared.clone();let permissions=shared.clone();
    let flow_request=request.clone();let flow_state=shared.clone();let close=stdin.clone();
    let flow=acp::Client.builder()
        .on_receive_notification(async move |notification:SessionNotification,_| {
            let mut state=lock(&notifications);
            if !state.session_matches(&notification.session_id.to_string()){return Ok(());}
            match notification.update {
                SessionUpdate::ConfigOptionUpdate(update) if state.pinned=>{
                    let options=serde_json::to_value(update.config_options).unwrap_or(Value::Null);
                    if !check_options(&options,&state.pins){state.fail("acp_config_changed_after_pin");}
                }
                SessionUpdate::CurrentModeUpdate(update) if state.pinned=>{
                    if state.pins.get("mode").map(String::as_str)!=Some(update.current_mode_id.to_string().as_str()){state.fail("acp_mode_changed_after_pin");}
                }
                SessionUpdate::AgentMessageChunk(chunk) if state.prompt_sent=>{
                    if state.phase=="drain" {state.fail("acp_text_after_terminal");return Ok(());}
                    if let ContentBlock::Text(text)=chunk.content {
                        if state.text.len().saturating_add(text.text.len())>ANSWER_LIMIT {state.fail("acp_answer_too_large");}
                        else {state.text.push_str(&text.text);}
                    } else {state.fail("acp_nontext_answer");}
                }
                SessionUpdate::ToolCall(_)|SessionUpdate::ToolCallUpdate(_) if state.prompt_sent=>{
                    state.text.clear();state.fail("acp_tools_not_permitted");
                }
                _=>{}
            }
            Ok(())
        },acp::on_receive_notification!())
        .on_receive_notification(async move |notification:ClaudeMessage,_| {
            let mut state=lock(&raw);
            if !state.session_matches(&notification.session_id) || !state.prompt_sent {return Ok(());}
            let message=&notification.message;
            if let Some(model)=message.get("model").or_else(||message.pointer("/message/model")).and_then(Value::as_str) {
                let expected=state.expected_model.as_deref();
                if Some(model)!=expected {state.fail("acp_native_model_changed");state.observed_models.insert(format!("sha256:{}",hex::encode(Sha256::digest(model.as_bytes()))));}
                else {state.observed_models.insert(model.into());}
            }
            if message["type"]=="result" {
                state.native_success=message["subtype"]=="success" && message["is_error"]!=true;
                if !state.native_success {state.fail("acp_native_result_failed");}
                if let Some(models)=message.get("modelUsage").and_then(Value::as_object) {for model in models.keys(){if state.expected_model.as_ref()==Some(model) {state.observed_models.insert(model.clone());} else {state.fail("acp_native_model_changed");state.observed_models.insert(format!("sha256:{}",hex::encode(Sha256::digest(model.as_bytes()))));}}}
            }
            if message["type"]=="system" && message["subtype"]=="compact_boundary" {state.fail("acp_unexpected_compaction");}
            Ok(())
        },acp::on_receive_notification!())
        .on_receive_request(async move |_request:RequestPermissionRequest,responder,_| {
            {let mut state=lock(&permissions);state.denied_permissions+=1;state.fail("acp_permission_denied");}
            responder.respond(RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled))
        },acp::on_receive_request!())
        .connect_with(ByteStreams::new(BoundedWriter::new(Closable(stdin.clone()),FRAME_LIMIT).compat_write(),BoundedReader::new(stdout,FRAME_LIMIT).with_total_limit(STREAM_LIMIT).compat()),async move |connection| {
            let result=conversation(&connection,&flow_request,&flow_state).await;
            close.lock().unwrap_or_else(|e|e.into_inner()).take();
            connection.incoming_closed().await;
            result
        });
    let budget=Duration::from_secs(request.timeout_secs.saturating_sub(3).max(1));
    let result=tokio::time::timeout(budget,flow).await;
    stdin.lock().unwrap_or_else(|e|e.into_inner()).take();
    let exit=match tokio::time::timeout(Duration::from_secs(2),child.wait()).await {
        Ok(Ok(status))=>Some(status),_=>{let _=child.kill().await;child.wait().await.ok()}
    };
    let stderr_result=tokio::time::timeout(Duration::from_secs(1),&mut stderr_job).await;
    if stderr_result.is_err(){stderr_job.abort();let _=stderr_job.await;}
    let violation={lock(&shared).violation};
    if let Some(code)=violation {return Err(failure(&request,&shared,code));}
    let (selected_model,stop)=match result {
        Err(_)=>return Err(failure(&request,&shared,"acp_deadline_exceeded")),
        Ok(Err(error))=>{
            let message=error.message.as_str();
            let code=if ["unsupported_effort","acp_config_not_honored","acp_model_not_advertised","acp_agent_version_mismatch","acp_agent_identity_mismatch","acp_protocol_version_mismatch","acp_option_missing","acp_option_value_unavailable","acp_session_identity_changed"].contains(&message){message}else if serde_json::to_value(error.code).ok().and_then(|v|v.as_i64())==Some(-32000){"acp_authentication_required"}else{"acp_request_rejected"};
            return Err(failure(&request,&shared,code));
        }
        Ok(Ok(value))=>value,
    };
    if !matches!(stderr_result,Ok(Ok(Ok(_)))) || !stdout_eof.load(Ordering::Acquire) || !stderr_eof.load(Ordering::Acquire) || exit.as_ref().and_then(|s|s.code())!=Some(0) {
        return Err(failure(&request,&shared,"acp_native_termination_unverified"));
    }
    let state=lock(&shared);
    if request.target==AcpTarget::Claude && (!state.native_success || state.observed_models.is_empty() || state.observed_models.iter().any(|m|m!=&request.model)) {
        drop(state);return Err(failure(&request,&shared,"acp_native_model_unverified"));
    }
    let answer=AcpAnswer{version:1,id:request.id.clone(),request_digest:request.request_digest.clone(),target:request.target,provider:request.provider.clone(),model:request.model.clone(),effort:request.effort.clone(),mode:request.mode.clone(),agent_version:request.agent_version.clone(),selected_model,text:state.text.clone(),stop_reason:stop,native_exit_code:0,stdout_eof:true,stderr_eof:true,evidence:state.evidence()};
    drop(state);answer.validate(&request).map_err(|code|failure(&request,&shared,code))?;Ok(answer)
}
async fn conversation(connection:&ConnectionTo<Agent>,request:&AcpRequest,shared:&Shared)->Result<(String,String),acp::Error> {
    lock(shared).phase="initialize";
    let init=connection.send_request(InitializeRequest::new(ProtocolVersion::V1)).block_task().await?;
    if init.protocol_version!=ProtocolVersion::V1 {return Err(rpc_error("acp_protocol_version_mismatch"));}
    let expected_name=match request.target {AcpTarget::OpenCode=>"OpenCode",AcpTarget::Codex=>"@agentclientprotocol/codex-acp",AcpTarget::Claude=>"@agentclientprotocol/claude-agent-acp"};
    if init.agent_info.as_ref().map(|i|i.name.as_str())!=Some(expected_name){return Err(rpc_error("acp_agent_identity_mismatch"));}
    if init.agent_info.as_ref().map(|i|i.version.as_str())!=Some(request.agent_version.as_str()){return Err(rpc_error("acp_agent_version_mismatch"));}
    lock(shared).phase="session";
    let mut new=NewSessionRequest::new(request.project.clone());
    if request.target==AcpTarget::Claude {
        let mut env=serde_json::Map::new();
        for key in ["ANTHROPIC_MODEL","ANTHROPIC_DEFAULT_OPUS_MODEL","ANTHROPIC_DEFAULT_SONNET_MODEL","ANTHROPIC_DEFAULT_HAIKU_MODEL","ANTHROPIC_SMALL_FAST_MODEL"] {env.insert(key.into(),json!(request.model));}
        if let Some(url)=&request.provider_url {env.insert("ANTHROPIC_BASE_URL".into(),json!(url));env.insert("CLAUDE_CODE_USE_BEDROCK".into(),json!("0"));env.insert("CLAUDE_CODE_USE_VERTEX".into(),json!("0"));}
        new=new.meta(json!({"claudeCode":{"emitRawSDKMessages":[{"type":"system"},{"type":"assistant"},{"type":"result"}],"options":{"model":request.model,"effort":request.effort,"tools":[],"disallowedTools":["Agent","Task","Bash","Write","Edit","NotebookEdit"],"allowDangerouslySkipPermissions":false,"settings":{"env":env,"effortLevel":request.effort,"permissions":{"defaultMode":"plan"}}}}}).as_object().unwrap().clone());
    }
    let response=connection.send_request(new).block_task().await?;let session=response.session_id;
    {let mut state=lock(shared);if !state.session_matches(&session.to_string()){return Err(rpc_error("acp_session_identity_changed"));}state.session=Some(session.to_string());}
    let mut options=serde_json::to_value(response.config_options.unwrap_or_default()).map_err(|_|rpc_error("acp_config_invalid"))?;
    lock(shared).phase="mode";options=select(connection,&session,"mode",&request.mode,&options).await?;
    lock(shared).phase="model";let selected=model_choice(request,&options).map_err(rpc_error)?;options=select(connection,&session,"model",&selected,&options).await?;
    let mut pins=BTreeMap::from([("model".to_string(),selected.clone()),("mode".to_string(),request.mode.clone())]);
    if let Some(effort)=&request.effort {
        lock(shared).phase="effort";let id=if request.target==AcpTarget::Codex{"reasoning_effort"}else{"effort"};
        options=select(connection,&session,id,effort,&options).await?;pins.insert(id.into(),effort.clone());
    }
    if !check_options(&options,&pins){return Err(rpc_error("acp_config_not_honored"));}
    {let mut state=lock(shared);state.pins=pins;state.expected_model=Some(request.model.clone());state.pinned=true;state.text.clear();state.phase="prompt";state.prompt_sent=true;}
    let prompt=format!("Read-only finite consultation. Use only the supplied context; do not use tools, delegate, or change models.\n\n{}",request.prompt);
    let (sender,receiver)=tokio::sync::oneshot::channel();
    let terminal_state=shared.clone();
    // This SDK callback holds the incoming dispatch barrier until completion,
    // unlike block_task: later notifications cannot race ahead of terminal state.
    connection.send_request(PromptRequest::new(session,vec![ContentBlock::Text(TextContent::new(prompt))]))
        .on_receiving_result(async move |result| {
            lock(&terminal_state).phase="drain";
            let _=sender.send(result);
            Ok(())
        })?;
    let result=receiver.await.map_err(|_|rpc_error("acp_terminal_missing"))??;
    let stop=serde_json::to_value(result.stop_reason).ok().and_then(|v|v.as_str().map(str::to_string)).unwrap_or_default();
    if stop!="end_turn" {return Err(rpc_error("acp_stop_not_complete"));}
    Ok((selected,stop))
}
