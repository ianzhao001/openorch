//! Consult-only rendering and typed evidence adaptation. Protocol lives in orch-acp.
use super::*;
use orch_core::acp::{AcpEnvelope,AcpRequest,AcpTarget};
use std::os::unix::fs::DirBuilderExt;
fn directory(path:&Path)->Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {Ok(())=>{},Err(e) if e.kind()==std::io::ErrorKind::AlreadyExists=>{},Err(e)=>return Err(e.into())}
    let m=fs::symlink_metadata(path)?;if !m.is_dir() || m.file_type().is_symlink(){bail!("unsafe ACP private directory");}Ok(())
}
fn bridge_path(context:&InvocationContextV1)->Result<PathBuf> {
    let directory=context.orch_executable.parent().context("runtime directory absent")?;
    let mut choices=vec![directory.join("orch-acp")];
    if directory.file_name().is_some_and(|n|n=="deps") {if let Some(parent)=directory.parent(){choices.push(parent.join("orch-acp"));}}
    for path in choices {if path.is_file(){capture_executable_identity(&path)?;return Ok(path);}}
    bail!("orch-acp sibling runtime unavailable; build or install the complete runtime")
}
pub(super) fn render(prepared:PreparedInvocation,context:InvocationContextV1,native:Option<&crate::native_discovery::DiscoveryContext>,private:Option<&Path>)->Result<RenderedInvocationV1> {
    if prepared.requested().action!=InvocationAction::Consult {bail!("ACP is Consult-only");}
    let mut profile=prepared.acp.clone().context("ACP profile absent")?;
    let owned_context;
    let native=match native {Some(n)=>n,None=>{owned_context=crate::native_discovery::DiscoveryContext::system(prepared.project_root())?;&owned_context}};
    if profile.native_route.is_none() && prepared.driver()!=HarnessId::OpenCode && prepared.effective().provider.is_some() {
        let mut metadata=native.clone();metadata.allow_commands=false;
        let scan=crate::native_discovery::discover_with_context(&metadata)?;
        profile.native_route=scan.harnesses.iter().find(|h|h.alias.as_deref()==Some(prepared.alias())).and_then(|h|h.native.provider_route.clone());
    }
    let target=match prepared.driver(){HarnessId::OpenCode=>AcpTarget::OpenCode,HarnessId::Codex=>AcpTarget::Codex,HarnessId::Claude=>AcpTarget::Claude,_=>bail!("ACP driver unsupported")};
    let route=profile.native_route.as_ref();
    if target!=AcpTarget::OpenCode && prepared.effective().provider.as_deref().is_some_and(|provider|route.is_none_or(|r|r.provider!=provider)){bail!("acp_provider_route_unverified");}
    let private=match private {Some(p)=>p.to_path_buf(),None=>{
        let state=prepared.project_root().join(".orch");directory(&state)?;let base=state.join("acp-private");directory(&base)?;
        let dir=base.join(hex::encode(Sha256::digest(context.action_id.as_bytes())));directory(&dir)?;dir
    }};
    require_canonical_exact_directory(&private,"ACP private directory")?;
    let config_key=match target {AcpTarget::Claude=>"CLAUDE_CONFIG_DIR",AcpTarget::Codex=>"CODEX_HOME",AcpTarget::OpenCode=>"XDG_CONFIG_HOME"};
    let credential_env=profile.credential_env.clone().or_else(||route.and_then(|r|r.credential_env.clone()));
    let request=AcpRequest{version:1,id:context.action_id.clone(),request_digest:prepared.request_digest.clone(),target,project:prepared.cwd.clone(),executable:prepared.executable.clone(),adapter:profile.adapter.clone(),node:profile.node.clone(),agent_version:profile.agent_version.clone(),provider:prepared.effective.provider.clone(),provider_url:route.map(|r|r.url.clone()),provider_env:credential_env.clone(),model:prepared.effective.model.clone().context("ACP model missing")?,effort:prepared.effective.effort.clone(),mode:prepared.effective.mode.clone().context("ACP mode missing")?,home:native.home.clone(),config_dir:native.overrides.get(config_key).map(PathBuf::from),isolation_dir:private.clone(),prompt:prepared.prompt.clone(),timeout_secs:context.deadline_secs};
    request.validate().map_err(anyhow::Error::msg)?;
    let mut env=controlled_operational_environment(prepared.driver())?;
    env.insert("HOME".into(),path_text(&native.home,"native home")?);
    env.insert("PATH".into(),std::env::join_paths(&native.search_path)?.into_string().map_err(|_|anyhow::anyhow!("native PATH is not UTF8"))?);
    if let Some(config)=native.overrides.get(config_key){env.insert(config_key.into(),config.clone());}
    if let Some(key)=credential_env {
        if !crate::harness_config::safe_credential_reference(&key){bail!("invalid ACP credential environment reference");}
        let value=std::env::var(&key).ok().filter(|v|!v.is_empty()).context("ACP credential environment reference unavailable")?;
        env.insert(key,value);
    }
    let mut extra_assets=Vec::new();
    if let Some(adapter)=&profile.adapter {extra_assets.push(capture_file_identity(adapter,profile.node.is_none())?);}
    if let Some(node)=&profile.node {extra_assets.push(capture_executable_identity(node)?);}
    let bytes=serde_json::to_vec(&request)?;if bytes.len()>2*1024*1024 {bail!("ACP request file too large");}
    let payload_sha=hex::encode(Sha256::digest(&bytes));let path=private.join("acp-request.json");
    let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    file.write_all(&bytes)?;file.sync_all()?;drop(file);
    extra_assets.push(capture_file_identity(&path,false)?);
    let program=bridge_path(&context)?;let program_identity=capture_executable_identity(&program)?;
    let argv=vec![path_text(&program,"ACP bridge")?,"--request-file".into(),path_text(&path,"ACP request")?,"--sha256".into(),payload_sha];
    let base=rendered_command_digest(&prepared,&context,&argv,&env,&program_identity,None);
    let mut digest=Sha256::new();digest.update(base.as_bytes());for asset in &extra_assets {digest.update(asset.sha256().as_bytes());}
    Ok(RenderedInvocationV1{acp_request:Some(request),extra_assets,prepared,context,argv,env,program_identity,wrapper_identity:None,command_digest:hex::encode(digest.finalize())})
}
/// Decode one complete private envelope; never fall back to native transcript extraction.
pub(crate) fn envelope(bytes:&[u8],request:&AcpRequest)->Result<AcpEnvelope> {
    if bytes.len()>2*1024*1024 {bail!("ACP envelope too large");}
    let envelope:AcpEnvelope=serde_json::from_slice(bytes).context("invalid single ACP envelope")?;
    match &envelope {
        AcpEnvelope::Completed{answer}=>answer.validate(request).map_err(anyhow::Error::msg)?,
        AcpEnvelope::Rejected{failure}=>{
            if failure.version!=1 || failure.id!=request.id || failure.request_digest!=request.request_digest || failure.code.is_empty() || failure.code.len()>128 || !failure.code.bytes().all(|b|b.is_ascii_lowercase()||b.is_ascii_digit()||b==b'_'){bail!("ACP rejection identity invalid");}
        }
    }
    Ok(envelope)
}
