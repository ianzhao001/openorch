//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 hide/rename writer; M2 retain dead module; M3 remove manual safety;
//! M4 recreate queue/policy/daemon. Real WAL/sites/seal regression is additionally required.

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf()
}

#[test]
fn automatic_commands_cannot_parse_even_when_hidden() {
 for command in ["run","step","serve","run-wave","schedule","nudge","resume","retry-dead","run-task"] {
  let out=std::process::Command::new(env!("CARGO_BIN_EXE_orch")).args([command,"--help"]).output().unwrap();
  assert!(!out.status.success(),"retired command still parses: {command}");
 }
}
#[test]
fn automatic_modules_are_physically_absent() {
 for name in ["serve.rs","runloop.rs","runtask.rs","wave.rs"] {
  assert!(!root().join("orch/crates/orch-host/src").join(name).exists(),"{name}");
 }
 assert!(root().join("orch/crates/orch-host/src/wake/resume_dispatch.rs").exists());
}
