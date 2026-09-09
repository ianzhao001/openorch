//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 remove UI; M2 weaken read-only UI/pins; M3 omit product or UI from shipping gate;
//! M4 resurrect legacy instructions; M5 drift skill copies. Execute the shipping gate as required evidence.

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap().to_path_buf()
}

#[test]
fn shipping_gate_covers_retained_ui_and_product_isolation(){
 let r=root();
 assert!(r.join("orch/crates/orch-ui/Cargo.toml").is_file());
 assert!(r.join("orch/crates/orch-host/tests/ui_crate_contract.rs").is_file());
 let script=std::fs::read_to_string(r.join("orch/scripts/check-feature-surfaces.sh")).expect("shipped dual-product/UI verification gate");
 for contract in ["--locked","--manifest-path","--no-default-features","selfhost","orch-ui","guide","--check"]{assert!(script.contains(contract),"gate omits {contract}");}
 let ui=std::fs::read_to_string(r.join("orch/crates/orch-ui/src/lib.rs")).unwrap();
 assert!(ui.contains("is_read_only"));
 let readme=std::fs::read_to_string(r.join("README.md")).unwrap();
 assert!(readme.contains("orch-ui 保留"));
 assert!(readme.contains("--no-default-features"));
}
