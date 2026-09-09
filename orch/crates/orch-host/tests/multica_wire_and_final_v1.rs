//! Actual socket bridge regression: deliberately reproduce the peer's per-chunk
//! UTF-8 decoder, and compare every returned raw byte without textual trimming.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

fn exercise(terminal_newline: bool) {
    let root = orch_host::util::test_scratch_dir("b327-wire");
    fs::create_dir_all(&root).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).arg(&root).status().unwrap().success());
    let session = "orch-wake-b327-wire-fixture";
    let prompt = "known: 知🧭 e\u{301}\nquoted \"text\" and \\ slash\r\n";
    let mut raw = format!("\n{{\"type\":\"text\",\"sessionId\":\"{session}\",\"text\":\"过程知\"}}\n{{\"payloads\":[{{\"text\":\"raw final 🧭\"}}],\"meta\":{{\"agentMeta\":{{\"sessionId\":\"{session}\"}}}}}}").into_bytes();
    if terminal_newline { raw.extend_from_slice(b"\n\n"); }
    let expected_raw = raw.clone();
    fs::write(root.join("response.bin"), raw).unwrap();
    // Relative addresses in child-only cwd avoid macOS's sockaddr_un length
    // limit in long review/trial worktrees without changing the test process cwd.
    let server = r#"
import json,pathlib,socket
s=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM); s.settimeout(5)
s.bind('bridge.sock'); s.listen(1); pathlib.Path('ready').write_text('ready')
c,_=s.accept(); c.settimeout(5); wire=b''; decoded=''
while not wire.endswith(b'\n'):
    b=c.recv(1)
    if not b: raise RuntimeError('request ended before newline')
    wire+=b; decoded+=b.decode('utf8','replace')
pathlib.Path('request.json').write_text(json.dumps({'wireAscii':wire.isascii(),'request':json.loads(decoded)}),encoding='utf8')
for b in pathlib.Path('response.bin').read_bytes(): c.sendall(bytes([b]))
c.close(); s.close()
"#;
    let mut peer = Command::new("/usr/bin/python3").args(["-I", "-c", server])
        .current_dir(&root).spawn().unwrap();
    let ready_until = Instant::now() + Duration::from_secs(5);
    while !root.join("ready").exists() && Instant::now() < ready_until {
        assert!(peer.try_wait().unwrap().is_none(), "fixture server exited before ready");
        std::thread::sleep(Duration::from_millis(10));
    }
    if !root.join("ready").exists() {
        let _ = peer.kill(); let _ = peer.wait();
        panic!("fixture server did not become ready");
    }
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).ancestors().nth(3).unwrap()
        .join("coordination/scripts/wake-multica.sh");
    let output = Command::new("/bin/sh").arg(source).arg(prompt).current_dir(&root)
        .env("ORCH_MULTICA_SOCK", "bridge.sock").env("ORCH_MULTICA_CWD", &root)
        .env("ORCH_MULTICA_SESSION", session).env("ORCH_MULTICA_TIMEOUT", "5")
        .output().unwrap();
    assert!(peer.wait().unwrap().success());
    let observed: serde_json::Value = serde_json::from_slice(&fs::read(root.join("request.json")).unwrap()).unwrap();
    let request = &observed["request"];
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(observed["wireAscii"], true, "wire JSON must survive a peer that decodes every byte separately");
    assert_eq!(request["prompt"], prompt);
    assert_eq!(request["cwd"], root.to_str().unwrap());
    assert_eq!(request["sessionId"], session);
    assert_eq!(output.stdout, expected_raw, "raw stream was altered or cut");
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn ascii_request_and_chunked_raw_output_are_lossless_with_a_tail_frame() { exercise(false); }

#[test]
fn raw_blank_lines_and_terminal_newlines_are_preserved() { exercise(true); }
