//! Local stdio server. Standard output is reserved for SDK protocol messages.
use anyhow::{bail, Context, Result};
use orch_host::fusion_run::FusionEngine;
use orch_mcp::service::Gateway;
use std::{io, path::PathBuf, time::Duration};
use orch_core::protocol_io::{BoundedReader,BoundedWriter};
const MAX_FRAME: usize=2*1024*1024;
async fn signal()->io::Result<()> {
    let mut terminate=tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! { result=tokio::signal::ctrl_c()=>result, _=terminate.recv()=>Ok(()) }
}
async fn serve(gateway:Gateway)->Result<()> {
    let transport=(BoundedReader::new(tokio::io::stdin(),MAX_FRAME),BoundedWriter::new(tokio::io::stdout(),MAX_FRAME));
    let result=async {
        let running=tokio::select! {
            result=rmcp::serve_server(gateway.clone(),transport)=>result.context("MCP initialization failed")?,
            result=signal()=>{result?;return Ok(());}
        };
        let cancel=running.cancellation_token();
        tokio::select! {
            result=running.waiting()=>{result.context("MCP service failed")?;},
            result=signal()=>{cancel.cancel();result?;}
        }
        Ok(())
    }.await;
    gateway.shutdown().await?;
    result
}
fn run()->Result<()> {
    let mut roots=Vec::new();let mut projects_file=None;let mut args=std::env::args_os().skip(1);
    while let Some(arg)=args.next() {
        if arg=="--project" {roots.push(PathBuf::from(args.next().context("--project requires a directory")?));}
        else if arg=="--projects-file" && projects_file.is_none() {projects_file=Some(PathBuf::from(args.next().context("--projects-file requires a registry")?));}
        else if arg=="--help" {println!("orch-mcp [--project <authorized Git worktree> ... | --projects-file <private registry>]\nNo arguments: use OPENORCH_CONFIG_DIR or ~/.config/openorch/projects.json after explicit helper attach. The working directory never grants access.\nLocal MCP stdio: list_harnesses, consult, get_run, read_answer. No background daemon.");return Ok(());}
        else if arg=="--version" {println!("orch-mcp {}",env!("CARGO_PKG_VERSION"));return Ok(());}
        else {bail!("unsupported argument; use --help");}
    }
    if !roots.is_empty() && projects_file.is_some() {bail!("explicit_projects_and_registry_are_mutually_exclusive");}
    if roots.is_empty() {
        roots=match projects_file {
            Some(path)=>orch_host::openorch_helper::registered_projects_file(&path)?,
            None=>orch_host::openorch_helper::registered_projects(&orch_host::openorch_helper::default_config_dir()?)?,
        };
    }
    let gateway=Gateway::with_engine(roots,FusionEngine::new())?;
    let runtime=tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
    let result=runtime.block_on(serve(gateway));
    // Tokio's stdin reader is not cancellable. All owned core jobs were drained
    // above; do not keep a closed service alive solely for a blocked stdin read.
    runtime.shutdown_timeout(Duration::from_secs(2));result
}
fn main() {
    if run().is_err() { eprintln!("orch-mcp: startup or transport failed; use --help and check authorized project configuration");std::process::exit(2); }
}

#[cfg(test)]
mod output_bound_tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    #[test]
    fn oversized_output_is_rejected_before_any_prefix_is_written() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut writer=BoundedWriter::new(Vec::<u8>::new(),MAX_FRAME);
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();writer.flush().await.unwrap();
            let original=writer.get_ref().clone();assert!(writer.write_all(&vec![b' ';MAX_FRAME+1]).await.is_err());
            assert_eq!(writer.get_ref(),&original);
        });
    }
}
