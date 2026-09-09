//! r84 contract. Relocate byte-for-byte; do not change until this task is Recorded.
//! Evidence also requires real production entry, full signed gates and each named negative mutation.
//! M1 default leaks task leaf; M2 selfhost lacks a leaf; M3 hidden old command;
//! M4 standalone requires IR/binding; M5 UI no longer compiles; M6 guide validates only one tree.
fn names(text:&str)->Vec<String> {
 let mut scan=false;let mut out=Vec::new();
 for l in text.lines(){if l=="Commands:"{scan=true;continue;}if scan&&(l=="Options:"||l.starts_with("Usage:")){break;}
 if scan {if let Some(x)=l.split_whitespace().next(){out.push(x.into());}}}
 out.sort();out
}
fn help(args:&[&str])->String{
 let o=std::process::Command::new(env!("CARGO_BIN_EXE_orch")).args(args).output().unwrap();
 assert!(o.status.success(),"{}",String::from_utf8_lossy(&o.stderr));String::from_utf8(o.stdout).unwrap()
}
#[test]
fn compiled_product_and_selfhost_command_trees_are_exact(){
 let expected=if cfg!(feature="selfhost"){vec!["await-report","check","consult","cost","current","dispatch","doctor","guide","harness","ledger","plan","review","round","seal","sites","snapshot","stall-check","status","verdict","wake"]}else{vec!["consult","doctor","guide","harness","wake"]};
 assert_eq!(names(&help(&["--help"])),expected);
 assert_eq!(names(&help(&["harness","--help"])),["lint","list"]);
 if cfg!(feature="selfhost"){
  assert_eq!(names(&help(&["review","--help"])),["deliver"]);
  assert_eq!(names(&help(&["round","--help"])),["close","open","seed-verified","sign-off"]);
  assert_eq!(names(&help(&["ledger","--help"])),["recover"]);
  assert_eq!(names(&help(&["sites","--help"])),["cache","gc","sweep-scratch","sweep-targets","sweep-trial-cache"]);
  assert_eq!(names(&help(&["sites","cache","--help"])),["run","status","sweep"]);
 }
 for cmd in ["init","bind","agent","runtime-policy","schema","run-task","verify","merge","record","retry-dead","bootstrap","nudge","handshake","resume","schedule","approve","run","step","serve","mcp","session","inbox","run-wave","help"]{
  assert!(!std::process::Command::new(env!("CARGO_BIN_EXE_orch")).args([cmd,"--help"]).output().unwrap().status.success(),"{cmd}");
 }
 assert!(!help(&["--help"]).contains("__wake-supervise"));
 assert!(help(&["guide","--check"]).contains(if cfg!(feature="selfhost"){"commands=30"}else{"commands=6"}));
}
