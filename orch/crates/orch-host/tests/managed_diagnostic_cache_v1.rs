//! r85 B333: managed local diagnostics preserve a stable receipt and reclaim their private cache.
use orch_host::buildcache::{run_managed_diagnostic, DiagnosticRequest};
#[test]
fn managed_diagnostic_api_is_an_explicit_foreground_entry() {
 let root=orch_host::util::test_scratch_dir("b333-missing-root");
 let request=DiagnosticRequest { cwd:root.clone(), executable:std::path::PathBuf::from("relative-program"), args:vec![], purpose:"reject unsafe executable".into() };
 assert!(run_managed_diagnostic(&root,&request).is_err());
 assert!(!root.join("coordination/runtime/diagnostic-cache").exists());
}
