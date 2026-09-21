//! Data-only contract between the shared host and its finite ACP bridge.
#![deny(missing_docs)]
use serde::{Deserialize,Serialize};
use std::path::PathBuf;
/// Supported native adapter families; this does not certify a particular model.
#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="lowercase")]
pub enum AcpTarget {
    /// OpenCode's built-in ACP command.
    OpenCode,
    /// Official Codex ACP with an explicitly selected native Codex executable.
    Codex,
    /// Official Claude Agent ACP with an explicitly selected native CLI.
    Claude,
}
impl AcpTarget {
    /// The only session mode this finite consultation client permits.
    pub fn readonly_mode(self)->&'static str {match self {Self::Codex=>"read-only",_=>"plan"}}
}
/// Immutable host-produced invocation. Credentials and arbitrary environment maps
/// are deliberately absent; native authentication remains out of band.
#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AcpRequest {
    /// Private bridge contract revision, currently one.
    pub version:u32,
    /// Exact action identity selected by the host.
    pub id:String,
    /// Digest of the host's already prepared request.
    pub request_digest:String,
    /// Code-owned native startup and parameter mapping.
    pub target:AcpTarget,
    /// Exact canonical consultation worktree.
    pub project:PathBuf,
    /// Native client executable from captured local configuration.
    pub executable:PathBuf,
    /// Optional adapter entry; OpenCode uses its native executable directly.
    pub adapter:Option<PathBuf>,
    /// Optional explicit Node executable for a JavaScript adapter.
    pub node:Option<PathBuf>,
    /// Exact agentInfo version required before session creation.
    pub agent_version:String,
    /// Requested provider identity; never a command or endpoint override.
    pub provider:Option<String>,
    /// Captured native provider endpoint; credentials and query strings are excluded.
    #[serde(default)]
    pub provider_url:Option<String>,
    /// Optional native credential environment reference, never its value.
    #[serde(default)]
    pub provider_env:Option<String>,
    /// Exact requested native model, independent of any picker alias.
    pub model:String,
    /// Requested effort, when explicitly pinned.
    pub effort:Option<String>,
    /// Required readonly session mode, not a general execution mode.
    pub mode:String,
    /// Captured native home used by the client to read its own secure state.
    pub home:PathBuf,
    /// Optional captured native configuration directory.
    pub config_dir:Option<PathBuf>,
    /// Host-owned private directory for nonsecret launch overlays.
    pub isolation_dir:PathBuf,
    /// Complete captured question/context, not a path to reread later.
    pub prompt:String,
    /// Finite native deadline; the outer channel retains process-group ownership.
    pub timeout_secs:u64,
}
impl AcpRequest {
    /// Validate the private protocol shape before spawning any native process.
    pub fn validate(&self)->Result<(),String> {
        let clean=|s:&str|!s.trim().is_empty() && s.len()<=512 && !s.chars().any(char::is_control);
        if self.version!=1 || !clean(&self.id) || !clean(&self.agent_version) || !clean(&self.model)
            || self.provider.as_deref().is_some_and(|s|!clean(s)) || self.effort.as_deref().is_some_and(|s|!clean(s))
            || self.request_digest.len()!=64 || !self.request_digest.bytes().all(|b|b.is_ascii_hexdigit())
            || self.prompt.trim().is_empty() || self.prompt.len()>1024*1024 || self.timeout_secs==0 || self.timeout_secs>7200 {
            return Err("invalid_acp_request".into());
        }
        if self.provider_url.as_deref().is_some_and(|url|!(url.starts_with("https://")||url.starts_with("http://")) || url.len()>2048 || url.contains(['@','?','#']) || url.chars().any(char::is_control)) {return Err("invalid_acp_provider_route".into());}
        if self.provider_env.as_deref().is_some_and(|key|key.len()>128 || !key.bytes().all(|b|b.is_ascii_uppercase()||b.is_ascii_digit()||b==b'_') || !(key.ends_with("API_KEY")||key.ends_with("AUTH_TOKEN")||key.ends_with("ACCESS_TOKEN"))) {return Err("invalid_acp_credential_reference".into());}
        if self.target!=AcpTarget::OpenCode && self.provider.is_some() && self.provider_url.is_none() {return Err("acp_provider_route_unverified".into());}
        if self.mode!=self.target.readonly_mode() {return Err("acp_readonly_mode_required".into());}
        for p in [&self.project,&self.executable,&self.home,&self.isolation_dir].into_iter().chain(self.adapter.iter()).chain(self.node.iter()).chain(self.config_dir.iter()) {
            if !p.is_absolute() || p.components().any(|c|matches!(c,std::path::Component::ParentDir|std::path::Component::CurDir)) {return Err("invalid_acp_path".into());}
        }
        if self.target!=AcpTarget::OpenCode && self.adapter.is_none() {return Err("acp_adapter_required".into());}
        if self.target==AcpTarget::OpenCode && (self.adapter.is_some() || self.node.is_some()) {return Err("acp_opencode_requires_builtin_entry".into());}
        Ok(())
    }
}
/// A completed native answer, still subject to the outer host's process-group and
/// artifact identity checks. Tool output is never placed in `text`.
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AcpAnswer {
    /// Private bridge contract revision.
    pub version:u32,
    /// Echoed exact action identity.
    pub id:String,
    /// Echoed prepared request digest.
    pub request_digest:String,
    /// Verified adapter family.
    pub target:AcpTarget,
    /// Captured requested provider.
    pub provider:Option<String>,
    /// Verified requested model, after explicit alias resolution if required.
    pub model:String,
    /// Verified requested effort.
    pub effort:Option<String>,
    /// Verified readonly mode.
    pub mode:String,
    /// Observed matching native adapter version.
    pub agent_version:String,
    /// Native selected model option, retained separately from its resolved name.
    pub selected_model:String,
    /// Final agent text after the last tool event.
    pub text:String,
    /// Native PromptResponse stop reason, required to be end_turn.
    pub stop_reason:String,
    /// Actual direct adapter exit code, required to be zero.
    pub native_exit_code:i32,
    /// SDK reader observed actual stdout EOF.
    pub stdout_eof:bool,
    /// Independent stderr reader observed actual EOF.
    pub stderr_eof:bool,
    /// Bounded nonsecret configuration and lifecycle observations.
    pub evidence:serde_json::Value,
}
impl AcpAnswer {
    /// Reject mismatched identities, unproven pins or incomplete native termination.
    pub fn validate(&self,request:&AcpRequest)->Result<(),String> {
        if self.version!=1 || self.id!=request.id || self.request_digest!=request.request_digest
            || self.target!=request.target || self.provider!=request.provider || self.model!=request.model
            || self.effort!=request.effort || self.mode!=request.mode || self.agent_version!=request.agent_version
            || self.stop_reason!="end_turn" || self.native_exit_code!=0 || !self.stdout_eof || !self.stderr_eof
            || self.text.trim().is_empty() || self.text.len()>512*1024 {return Err("acp_answer_not_verified".into());}
        Ok(())
    }
}
/// A safe failure report with no raw SDK error, prompt or credential contents.
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub struct AcpFailure {
    /// Private bridge contract revision.
    pub version:u32,
    /// Action identity, when request validation completed.
    pub id:String,
    /// Prepared request digest.
    pub request_digest:String,
    /// Stable failure classification.
    pub code:String,
    /// Whether a prompt request was attempted; false is explicit pre-inference refusal.
    pub prompt_sent:bool,
    /// Bounded nonsecret observations needed to diagnose the refusal.
    pub evidence:serde_json::Value,
}
impl std::fmt::Display for AcpFailure {fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {write!(f,"{}",self.code)}}
impl std::error::Error for AcpFailure {}
/// The bridge emits exactly one of these envelopes on stdout.
#[derive(Debug,Clone,Serialize,Deserialize)]
#[serde(tag="status",rename_all="snake_case",deny_unknown_fields)]
pub enum AcpEnvelope {
    /// Completed native answer awaiting outer group/artifact verification.
    Completed { /// Bound answer and native evidence.
        answer:AcpAnswer },
    /// Refusal or failure; never treated as an answer or automatically retried.
    Rejected { /// Safe failure and whether inference was attempted.
        failure:AcpFailure },
}
