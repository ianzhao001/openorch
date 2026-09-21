//! Supervised one-shot bridge: a digest-bound request file in, one safe envelope out.
use anyhow::{bail,Context,Result};
use orch_core::acp::{AcpRequest,AcpEnvelope,AcpFailure};
use sha2::{Digest,Sha256};
use std::{fs::{self,OpenOptions},io::{Read,Write},os::unix::fs::{MetadataExt,OpenOptionsExt},path::{Path,PathBuf},process::ExitCode,time::Duration};
const LIMIT:usize=2*1024*1024;
#[cfg(target_os="linux")]
const NO_FOLLOW:i32=0x20000;
#[cfg(not(target_os="linux"))]
const NO_FOLLOW:i32=0x100;
fn read_request(path:&Path,expected:&str)->Result<AcpRequest> {
    if !path.is_absolute() || expected.len()!=64 || !expected.bytes().all(|b|b.is_ascii_hexdigit()) {bail!("invalid_bridge_arguments");}
    let mut prefix=PathBuf::new();
    for component in path.components() {
        if matches!(component,std::path::Component::CurDir|std::path::Component::ParentDir){bail!("unsafe_request_path");}
        prefix.push(component.as_os_str());if fs::symlink_metadata(&prefix)?.file_type().is_symlink(){bail!("unsafe_request_path");}
    }
    let before=fs::symlink_metadata(path)?;
    if !before.is_file() || before.len()>LIMIT as u64 {bail!("invalid_request_file");}
    let file=OpenOptions::new().read(true).custom_flags(NO_FOLLOW).open(path)?;let opened=file.metadata()?;
    if opened.dev()!=before.dev() || opened.ino()!=before.ino(){bail!("request_file_changed");}
    let mut bytes=Vec::new();file.take((LIMIT+1) as u64).read_to_end(&mut bytes)?;
    if bytes.len()>LIMIT || hex::encode(Sha256::digest(&bytes))!=expected {bail!("request_digest_changed");}
    let after=fs::symlink_metadata(path)?;
    if after.file_type().is_symlink() || after.dev()!=opened.dev() || after.ino()!=opened.ino() || after.len()!=opened.len() || after.mtime()!=opened.mtime() || after.mtime_nsec()!=opened.mtime_nsec(){bail!("request_file_changed");}
    let request:AcpRequest=serde_json::from_slice(&bytes).context("invalid_bridge_request")?;
    request.validate().map_err(anyhow::Error::msg)?;Ok(request)
}
fn run()->Result<bool> {
    let mut args=std::env::args_os().skip(1);let mut path=None;let mut digest=None;
    while let Some(arg)=args.next() {
        if arg=="--request-file" && path.is_none(){path=Some(PathBuf::from(args.next().context("request file missing")?));}
        else if arg=="--sha256" && digest.is_none(){digest=Some(args.next().context("digest missing")?.into_string().map_err(|_|anyhow::anyhow!("invalid digest"))?);}
        else if arg=="--version" {println!("orch-acp {}",env!("CARGO_PKG_VERSION"));return Ok(true);}
        else if arg=="--help" {println!("orch-acp --request-file <absolute private JSON> --sha256 <file-byte SHA256>\nInternal finite ACP v1 bridge; use orch consult or orch-mcp for supervised operation.");return Ok(true);}
        else {bail!("unsupported_bridge_argument");}
    }
    let request=read_request(&path.context("request file required")?,&digest.context("digest required")?)?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(orch_acp::client::consult_verified(request.clone()));
    runtime.shutdown_timeout(Duration::from_secs(2));
    let success=result.is_ok();
    let envelope=match result {Ok(answer)=>AcpEnvelope::Completed{answer},Err(failure)=>AcpEnvelope::Rejected{failure}};
    let mut bytes=serde_json::to_vec(&envelope)?;
    if bytes.len()+1>LIMIT {
        bytes=serde_json::to_vec(&AcpEnvelope::Rejected{failure:AcpFailure{version:1,id:request.id,request_digest:request.request_digest,code:"acp_output_too_large".into(),prompt_sent:true,evidence:serde_json::json!({})}})?;
        bytes.push(b'\n');std::io::stdout().lock().write_all(&bytes)?;return Ok(false);
    }
    bytes.push(b'\n');std::io::stdout().lock().write_all(&bytes)?;Ok(success)
}
fn main()->ExitCode {
    match run() {Ok(true)=>ExitCode::SUCCESS,Ok(false)=>ExitCode::from(2),Err(_)=>{eprintln!("orch-acp: request or bridge startup rejected");ExitCode::from(2)}}
}
