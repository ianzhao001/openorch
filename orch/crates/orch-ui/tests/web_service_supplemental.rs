//! Supplemental actual HTTP response and bounded shutdown tests.
use orch_host::observation::ObservationReader;
use orch_ui::web::{sanitize_payload, WebServer};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::Command,
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = orch_host::util::test_scratch_dir("web 中文 space");
        fs::write(root.join("tracked"), "unchanged").unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", "tracked"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "base",
            ],
        ] {
            let out = Command::new("git")
                .arg("-c")
                .arg("core.fsmonitor=false")
                .arg("-c")
                .arg("commit.gpgSign=false")
                .arg("-c")
                .arg("core.hooksPath=/dev/null")
                .arg("-C")
                .arg(&root)
                .args(args)
                .env_remove("GIT_CONFIG_PARAMETERS")
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Self(root)
    }
    fn consultation(&self) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        fs::create_dir(self.0.join(".orch")).unwrap();
        fs::write(
            self.0.join(".gitignore"),
            ".orch/\ncoordination/\n.cowork-temp/\n",
        )
        .unwrap();
        fs::write(self.0.join("question"), "fixture").unwrap();
        let exe = self.0.join("provider");
        fs::write(&exe,"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"# Title\\n\\n- one\\n- two\\n\\n```rust\\nlet x = 1;\\n```\"}'\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(self.0.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  one:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",exe.display())).unwrap();
        // Absolute input and target cwd owned by the production API; no process-global chdir.
        orch_host::consult::run_consultation(
            &self.0,
            &orch_host::consult::ConsultArgs {
                question: self.0.join("question"),
                harnesses: vec!["one".into()],
                member_timeout_secs: Some(5),
                total_wall_secs: Some(10),
                ..Default::default()
            },
        )
        .unwrap()
        .dir
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn http(
    s: &WebServer,
    method: &str,
    path: &str,
    body: &str,
    headers: &[(&str, String)],
    auth: bool,
) -> (u16, String, String) {
    let mut c = TcpStream::connect(s.address()).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    c.set_write_timeout(Some(std::time::Duration::from_secs(10)))
        .unwrap();
    let host = headers
        .iter()
        .find(|(k, _)| *k == "Host")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| s.address().to_string());
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    if auth {
        req += &format!("X-Orch-Capability: {}\r\n", s.capability());
    }
    if !body.is_empty() {
        req += "Content-Type: application/json\r\n";
    }
    for (k, v) in headers {
        if *k != "Host" {
            req += &format!("{k}: {v}\r\n");
        }
    }
    req += "\r\n";
    req += body;
    c.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    c.take(8 * 1024 * 1024).read_to_string(&mut raw).unwrap();
    let (h, b) = raw.split_once("\r\n\r\n").unwrap();
    (
        h.split_whitespace().nth(1).unwrap().parse().unwrap(),
        h.to_lowercase(),
        b.to_string(),
    )
}
fn snapshot(s: &WebServer) -> String {
    format!("/api/v1/projects/{}/snapshot", s.initial_project())
}

#[test]
fn response_bytes_are_redacted_and_detail_readonly() {
    let f = Fixture::new();
    let dir = f.consultation();
    let manifest = dir.join("fusion/0-one.manifest.json");
    let mut v: Value = serde_json::from_slice(&fs::read(&manifest).unwrap()).unwrap();
    v["channelFacts"]["terminal"]["API_KEY"] = "response_canary_A".into();
    v["channelFacts"]["terminal"]["headers"] =
        json!({"authorization":"Basic response_canary_B","safe":"visible_marker"});
    fs::write(&manifest, serde_json::to_vec(&v).unwrap()).unwrap();
    let before = tree(&f.0);
    let s = WebServer::start(&f.0, 0).unwrap();
    let reg = http(
        &s,
        "POST",
        "/api/v1/projects",
        &json!({"root":f.0}).to_string(),
        &[],
        true,
    );
    assert_eq!(reg.0, 200);
    let raw = http(&s, "GET", &snapshot(&s), "", &[], true).2;
    assert!(!raw.contains("response_canary_A"));
    assert!(!raw.contains("response_canary_B"));
    assert!(raw.contains("visible_marker"));
    let snap: Value = serde_json::from_str(&raw).unwrap();
    let id = snap["data"]["rows"][0]["id"].as_str().unwrap();
    let p = format!("/api/v1/projects/{}/detail?id={id}", s.initial_project());
    let (code, _, body) = http(&s, "GET", &p, "", &[], true);
    assert_eq!(code, 200);
    assert!(!body.contains("response_canary_"));
    drop(s);
    assert_eq!(before, tree(&f.0));
}
fn tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut out = vec![];
    for e in fs::read_dir(root).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(tree(&e.path()));
        } else if e.file_type().unwrap().is_file() {
            out.push((e.path(), fs::read(e.path()).unwrap()));
        }
    }
    out.sort();
    out
}
#[test]
fn missing_and_unsafe_answer_never_reuses_verified_text() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::new();
    let dir = f.consultation();
    let body = dir.join("fusion/0-one.md");
    let original = fs::read(&body).unwrap();
    let s = WebServer::start(&f.0, 0).unwrap();
    let snap: Value =
        serde_json::from_str(&http(&s, "GET", &snapshot(&s), "", &[], true).2).unwrap();
    let id = snap["data"]["rows"][0]["id"].as_str().unwrap();
    let p = format!("/api/v1/projects/{}/detail?id={id}", s.initial_project());
    for mode in 0..3 {
        fs::write(&body, &original).unwrap();
        fs::set_permissions(&body, fs::Permissions::from_mode(0o600)).unwrap();
        let d: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
        assert!(d["data"]["text"].is_string());
        match mode {
            0 => fs::remove_file(&body).unwrap(),
            1 => fs::set_permissions(&body, fs::Permissions::from_mode(0o666)).unwrap(),
            _ => fs::write(&body, vec![b'x'; 1024 * 1024 + 1]).unwrap(),
        };
        let d: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
        assert!(d["data"]["text"].is_null());
    }
}
#[test]
fn slow_connection_cannot_hold_drop_forever() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let mut c = TcpStream::connect(s.address()).unwrap();
    c.write_all(format!("POST /api/v1/projects HTTP/1.1\r\nHost: {}\r\nX-Orch-Capability: {}\r\nContent-Type: application/json\r\nContent-Length: 8000\r\n\r\n{{",s.address(),s.capability()).as_bytes()).unwrap();
    let start = std::time::Instant::now();
    drop(s);
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    drop(c);
}
#[test]
fn lookup_never_serves_arbitrary_files() {
    let f = Fixture::new();
    fs::write(f.0.join("secretfile"), "never_serve_me").unwrap();
    let s = WebServer::start(&f.0, 0).unwrap();
    for path in [
        "/secretfile",
        "/assets/../../secretfile",
        "/api/v1/projects/../snapshot",
    ] {
        let (code, _, body) = http(&s, "GET", path, "", &[], true);
        assert_ne!(code, 200);
        assert!(!body.contains("never_serve_me"));
    }
    let p = format!(
        "/api/v1/projects/{}/detail?id=../../secretfile",
        s.initial_project()
    );
    assert!(!http(&s, "GET", &p, "", &[], true)
        .2
        .contains("never_serve_me"));
}
#[test]
fn metadata_controls_ansi_and_legacy_detail_compatibility() {
    let v = sanitize_payload(
        &json!({"x":"ok\u{1b}[31m red\u{1b}[0m\n\tline","PROXY-AUTHORIZATION":"do_not_expose"}),
    );
    assert_eq!(v["x"], "ok red\n\tline");
    assert!(!v.to_string().contains("do_not_expose"));
    let f = Fixture::new();
    f.consultation();
    let mut r = ObservationReader::open(&f.0).unwrap();
    let id = r.refresh().unwrap().rows[0].id.clone();
    assert!(!r.detail(&id).unwrap().text.unwrap().contains('\n'));
    assert!(r
        .detail_multiline(&id)
        .unwrap()
        .text
        .unwrap()
        .contains('\n'));
}

#[test]
fn public_rustdoc_describes_service_and_safe_observation_contracts() {
    for (source, symbols) in [
        (
            include_str!("../src/web.rs"),
            vec![
                "pub struct WebServer",
                "pub fn start",
                "pub fn address",
                "pub fn capability",
                "pub fn initial_project",
                "pub fn sanitize_payload",
            ],
        ),
        (
            include_str!("../../orch-host/src/observation.rs"),
            vec![
                "pub fn safe_observation_multiline",
                "pub fn safe_observation_value",
                "pub fn detail_multiline",
            ],
        ),
    ] {
        for symbol in symbols {
            let before = source[..source.find(symbol).unwrap()].trim_end();
            let docs: String = before
                .lines()
                .rev()
                .take_while(|l| l.trim_start().starts_with("///"))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(docs.len() > 40, "substantive rustdoc missing for {symbol}");
        }
    }
}

#[test]
fn errors_redact_route_ids_and_classify_missing_calls() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let (code, _, body) = http(
        &s,
        "GET",
        "/api/v1/projects/Bearer%20route_secret_canary/snapshot",
        "",
        &[],
        true,
    );
    assert_eq!(code, 404);
    assert!(!body.contains("route_secret_canary"));
    let path = format!(
        "/api/v1/projects/{}/detail?id=consult:missing:0",
        s.initial_project()
    );
    let (code, _, body) = http(&s, "GET", &path, "", &[], true);
    assert_eq!(code, 404);
    let value: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(value["error"]["code"], "not_found");
    assert!(value["data"].is_null());
}
