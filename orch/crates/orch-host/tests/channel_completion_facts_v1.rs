//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 infer timeout from exit 72; M2 classify model quotation as provider error;
//! M3 accept empty/tool-only; M4 slow sibling suppresses saved fast result;
//! M5 mark ambiguous/native-running complete; M6 silence authorizes cancellation.
use orch_host::channel::{ChannelExitReason, classify_driver_failure};
use orch_host::harness::HarnessId;

#[test]
fn provider_errors_require_driver_frames_and_timeout_requires_runtime_fact() {
 let error=r#"{"type":"error","error":{"name":"UnknownError","data":{"message":"{\"type\":\"api_error\",\"message\":\"upstream response stream failed\"}"}}}"#;
 let failure=classify_driver_failure(HarnessId::OpenCode,error,"",ChannelExitReason::Exited,Some(1)).unwrap();
 assert_eq!(failure.class_name(),"upstream-stream");
 let quoted=r#"{"type":"text","part":{"text":"Example error: upstream response stream failed; unauthorized"}}"#;
 assert!(classify_driver_failure(HarnessId::OpenCode,quoted,"",ChannelExitReason::Exited,Some(0)).is_none());
 let code=classify_driver_failure(HarnessId::OpenCode,"","",ChannelExitReason::Exited,Some(72)).unwrap();
 assert_ne!(code.class_name(),"timeout");
 let timeout=classify_driver_failure(HarnessId::OpenCode,"","",ChannelExitReason::HardDeadline,None).unwrap();
 assert_eq!(timeout.class_name(),"timeout");
 let unknown=classify_driver_failure(HarnessId::OpenCode,"","",ChannelExitReason::ObservationFailed,None).unwrap();
 assert_eq!(unknown.class_name(),"observation-failed");
}
