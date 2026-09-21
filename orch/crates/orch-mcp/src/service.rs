//! Four bounded MCP tools over the shared consultation lifecycle.
use anyhow::{bail, Context, Result};
use orch_host::{consult::{consultation_admitted, GateDecision}, fusion_roles::FusionRole,
    fusion_run::{ConsultRequest, FusionEngine, RunView}};
use rmcp::{model::*, service::RequestContext, ErrorData, RoleServer, ServerHandler};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::BTreeMap, fs, os::unix::fs::MetadataExt, path::PathBuf,
    sync::{Arc, atomic::{AtomicBool, Ordering}}, time::{Duration, Instant}};
use tokio::sync::Semaphore;
const MAX_IN_FLIGHT: u32 = 8;
const MAX_TOOL_RESPONSE: usize = 256 * 1024;
type Cancelled = Arc<dyn Fn() -> bool + Send + Sync>;
#[derive(Clone)]
struct Project { path: PathBuf, device: u64, inode: u64 }
struct State { projects: BTreeMap<String, Project>, engine: FusionEngine,
    permits: Arc<Semaphore>, closing: AtomicBool }
/// A local host-owned project allowlist and one shared finite consultation engine.
/// Remote tool arguments cannot supply environment variables or executable paths.
#[derive(Clone)]
pub struct Gateway { state: Arc<State> }
#[derive(Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct ProjectArgs { project: String }
#[derive(Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct ConsultArgs { project: String, request_key: String, question: String,
    members: Vec<FusionRole>, #[serde(default)] attachments: Vec<PathBuf> }
#[derive(Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct StatusArgs { project: String, run_id: String, #[serde(default)] wait_ms: u64 }
#[derive(Deserialize)]
#[serde(rename_all="camelCase", deny_unknown_fields)]
struct AnswerArgs { project: String, run_id: String, member_id: String,
    #[serde(default)] offset: usize, #[serde(default="page_limit")] limit: usize,
    sha256: Option<String> }
fn page_limit()->usize { 16384 }
impl Gateway {
    /// Bind exact canonical worktree directories at startup, preserving sibling privacy.
    /// The injected engine is host-owned and supports isolated local qualification.
    pub fn with_engine(roots: Vec<PathBuf>, engine: FusionEngine) -> Result<Self> {
        if roots.is_empty() || roots.len()>32 { bail!("invalid_project_allowlist"); }
        let mut projects=BTreeMap::new();
        for root in roots {
            let path=fs::canonicalize(root).context("project_unavailable")?;
            let meta=fs::metadata(&path)?;
            if !meta.is_dir() { bail!("project_not_directory"); }
            // Validate Git authority without broadening permission to sibling worktrees.
            orch_host::fusion_roles::project_root(&path)?;
            let key=path.to_str().context("project_path_not_utf8")?.to_string();
            if projects.insert(key, Project { path, device:meta.dev(), inode:meta.ino() }).is_some() {
                bail!("duplicate_project");
            }
        }
        Ok(Self { state: Arc::new(State { projects, engine,
            permits:Arc::new(Semaphore::new(MAX_IN_FLIGHT as usize)), closing:AtomicBool::new(false) }) })
    }
    fn project(&self, key:&str)->Result<PathBuf> {
        let project=if key=="." && self.state.projects.len()==1 {
            self.state.projects.values().next().unwrap()
        } else { self.state.projects.get(key).context("project_not_allowed")? };
        let actual=fs::canonicalize(&project.path).context("project_identity_changed")?;
        let meta=fs::metadata(&actual)?;
        if actual!=project.path || meta.dev()!=project.device || meta.ino()!=project.inode {
            bail!("project_identity_changed");
        }
        Ok(project.path.clone())
    }
    fn status(&self,root:&std::path::Path,id:&str)->Result<RunView> {
        let view=self.state.engine.read_status(root,id)?;
        if view.project!=root { bail!("run_project_mismatch"); }
        Ok(view)
    }
    /// Return only the four supported tools and their closed argument schemas.
    pub fn tools(&self)->Vec<Tool> {
        let project=json!({"type":"string","description":"Exact startup-authorized canonical project path; '.' only when one project is configured."});
        let text=json!({"type":"string"});
        let fixed=json!({"type":"object","additionalProperties":false,"properties":{
            "provider":{"type":["string","null"]},"model":{"type":["string","null"]},
            "effort":{"type":["string","null"]},"mode":{"type":["string","null"]}}});
        let member=json!({"type":"object","additionalProperties":false,"required":["id","name","instructions","harness","fixed"],
            "properties":{"id":text,"name":text,"instructions":text,"harness":text,"fixed":fixed}});
        [
          ("list_harnesses","List local member capabilities without invoking models.",json!({"project":project}),vec!["project"]),
          ("consult","Reserve an explicit finite consultation. Reuse the same request key only with identical inputs; no automatic synthesis.",json!({"project":project,"requestKey":text,"question":text,"members":{"type":"array","minItems":1,"maxItems":5,"items":member},"attachments":{"type":"array","maxItems":16,"items":text}}),vec!["project","requestKey","question","members"]),
          ("get_run","Read compact state; optionally wait up to 30000ms. HOLD is never automatically restarted.",json!({"project":project,"runId":text,"waitMs":{"type":"integer","minimum":0,"maximum":30000}}),vec!["project","runId"]),
          ("read_answer","Read a verified UTF-8 page. Every continuation requires the previous whole-answer SHA256.",json!({"project":project,"runId":text,"memberId":text,"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":65536},"sha256":text}),vec!["project","runId","memberId"])
        ].into_iter().map(|(name,description,properties,required)|{
            let schema=json!({"type":"object","additionalProperties":false,"properties":properties,"required":required});
            let mut tool=Tool::new(name,description,schema.as_object().unwrap().clone());
            tool.annotations=Some(ToolAnnotations::default().read_only(name!="consult").destructive(false).idempotent(true).open_world(name=="consult"));tool
        }).collect()
    }
    /// Execute a typed, bounded tool request. Admission survives client cancellation:
    /// a permit stays with its blocking operation until reservation or observation ends.
    pub async fn invoke(&self,name:&str,args:Value)->Result<Value> {
        self.invoke_with_cancel(name,args,Arc::new(||false)).await
    }
    async fn invoke_with_cancel(&self,name:&str,args:Value,cancelled:Cancelled)->Result<Value> {
        if self.state.closing.load(Ordering::Acquire) { bail!("service_closing"); }
        let permit=self.state.permits.clone().try_acquire_owned().context("service_busy")?;
        let this=self.clone();let name=name.to_string();
        tokio::task::spawn_blocking(move || {
            let _permit=permit;
            if this.state.closing.load(Ordering::Acquire) { bail!("service_closing"); }
            let value=this.invoke_sync(&name,args,cancelled)?;
            if serde_json::to_vec(&value)?.len()>MAX_TOOL_RESPONSE { bail!("response_too_large"); }
            Ok(value)
        }).await.context("service_worker_failed")?
    }
    fn invoke_sync(&self,name:&str,args:Value,cancelled:Cancelled)->Result<Value> {
        match name {
            "list_harnesses"=>{
                let p:ProjectArgs=serde_json::from_value(args).context("invalid_arguments")?;
                let root=self.project(&p.project)?;
                let gate=consultation_admitted(&root)?;
                let refusal=match gate { GateDecision::Admit{..}=>None,GateDecision::Refuse{reason}=>Some(reason) };
                let snapshot=self.state.engine.discovery_without_commands(&root)?;
                let members=snapshot.harnesses.into_iter().map(|h|json!({"id":h.id,"driver":h.driver,"backend":h.backend,
                    "enabled":h.enabled,"availability":h.availability,"reason":h.reason,"configured":h.configured,
                    "native":{"current":h.native.current,"models":h.native.models,"status":h.native.status},
                    "consult":{"negotiationRequired":h.backend=="acp","available":refusal.is_none() && h.enabled && h.availability=="supported","reason":refusal}})).collect::<Vec<_>>();
                Ok(json!({"harnesses":members,"projectReason":refusal}))
            }
            "consult"=>{
                let p:ConsultArgs=serde_json::from_value(args).context("invalid_arguments")?;
                let root=self.project(&p.project)?;
                let request=ConsultRequest { request_id:p.request_key,question:p.question,members:p.members,attachments:p.attachments };
                request.validate()?;
                let view=self.state.engine.start_consult(&root,request)?;
                if view.project!=root { bail!("run_project_mismatch"); }
                Ok(metadata(view))
            }
            "get_run"=>{
                let p:StatusArgs=serde_json::from_value(args).context("invalid_arguments")?;
                if p.wait_ms>30000 { bail!("invalid_wait_bound"); }
                let root=self.project(&p.project)?;let deadline=Instant::now()+Duration::from_millis(p.wait_ms);
                loop {
                    let view=self.status(&root,&p.run_id)?;
                    if !matches!(view.phase.as_str(),"preparing"|"consulting"|"synthesizing") || Instant::now()>=deadline || cancelled() || self.state.closing.load(Ordering::Acquire) { return Ok(metadata(view)); }
                    std::thread::sleep(Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())));
                }
            }
            "read_answer"=>{
                let p:AnswerArgs=serde_json::from_value(args).context("invalid_arguments")?;
                let root=self.project(&p.project)?;
                self.status(&root,&p.run_id)?;
                Ok(serde_json::to_value(self.state.engine.read_answer_page(&root,&p.run_id,&p.member_id,p.offset,p.limit,p.sha256.as_deref())?)?)
            }
            _=>bail!("unknown_tool"),
        }
    }
    /// Stop admission and drain admitted operations and finite core jobs before exit.
    /// Call while the async runtime remains alive; this never rewrites a HOLD as success.
    pub async fn shutdown(&self)->Result<()> {
        self.state.closing.store(true,Ordering::Release);
        let permits=self.state.permits.clone().acquire_many_owned(MAX_IN_FLIGHT).await?;
        let this=self.clone();
        tokio::task::spawn_blocking(move || { let _permits=permits;this.state.engine.drain_jobs(); }).await?;
        Ok(())
    }
}
fn metadata(view:RunView)->Value {
    let members=view.members.into_iter().map(|m|json!({"id":m.role_id,"name":m.name,"harness":m.harness,"requested":m.tuple,"status":m.status,"reason":m.reason,"answerStatus":m.answer_status})).collect::<Vec<_>>();
    json!({"id":view.id,"phase":view.phase,"createdAt":view.created_at,"head":view.head,"members":members,"reason":view.reason})
}
impl ServerHandler for Gateway {
    fn get_info(&self)->ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("orch-mcp",env!("CARGO_PKG_VERSION")))
            .with_instructions("Explicit finite consultations only. Choose members yourself. Read verified answers separately and synthesize in the host. No automatic retries or fallback.")
    }
    fn get_tool(&self,name:&str)->Option<Tool> { self.tools().into_iter().find(|t|t.name==name) }
    async fn list_tools(&self,_:Option<PaginatedRequestParams>,_:RequestContext<RoleServer>)->Result<ListToolsResult,ErrorData> {
        Ok(ListToolsResult { tools:self.tools(), ..Default::default() })
    }
    async fn call_tool(&self,request:CallToolRequestParams,context:RequestContext<RoleServer>)->Result<CallToolResponse,ErrorData> {
        let args=Value::Object(request.arguments.unwrap_or_default());
        let ct=context.ct;
        let result=match self.invoke_with_cancel(&request.name,args,Arc::new(move || ct.is_cancelled())).await {
            Ok(value)=>CallToolResult::structured(value),
            // Native stderr, paths and credentials never become protocol errors.
            Err(error)=>{
                let label=error.to_string();
                let code=if ["project_not_allowed","run_project_mismatch","invalid_wait_bound","service_closing","service_busy","unknown_tool","invalid_arguments","request_id_conflict","answer_digest_changed","invalid_answer_page","project_identity_changed","response_too_large"].contains(&label.as_str()) { label.as_str() } else { "consultation_rejected" };
                CallToolResult::structured_error(json!({"error":{"code":code,"message":code}}))
            }
        };Ok(result.into())
    }
}
