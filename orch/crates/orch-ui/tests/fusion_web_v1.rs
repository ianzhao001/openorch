//! Real loopback routes exercise guarded role mutations and finite fake-client runs.
use orch_host::native_discovery::DiscoveryContext;
use orch_ui::web::WebServer;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    net::TcpStream,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
struct Fixture {
    root: PathBuf,
    context: DiscoveryContext,
}
impl Fixture {
    fn new() -> Self {
        let root = orch_host::util::test_scratch_dir("fusion web 中文");
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=f@example.invalid",
                "commit",
                "--allow-empty",
                "-qm",
                "base",
            ],
        ] {
            assert!(Command::new("git")
                .args(["-c", "core.fsmonitor=false"])
                .args(args)
                .current_dir(&root)
                .status()
                .unwrap()
                .success());
        }
        let home = root.join("home");
        let bin = root.join("bin");
        fs::create_dir_all(home.join(".claude")).unwrap();
        fs::create_dir(&bin).unwrap();
        fs::write(
            home.join(".claude/settings.json"),
            r#"{"model":"web-native","effortLevel":"off"}"#,
        )
        .unwrap();
        let exe = bin.join("claude");
        fs::write(&exe,"#!/bin/sh\nprintf 'called\\n' >> \"$0.calls\"\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"# Verified fixture answer\"}'\n").unwrap();
        fs::set_permissions(exe, fs::Permissions::from_mode(0o755)).unwrap();
        let context = DiscoveryContext {
            project: root.clone(),
            home,
            search_path: vec![bin, "/usr/bin".into(), "/bin".into()],
            query_timeout_ms: 1000,
            allow_commands: false,
            overrides: BTreeMap::new(),
            include_platform_locations: false,
        };
        Self { root, context }
    }
    fn server(&self) -> WebServer {
        WebServer::start_with_discovery_context(&self.root, 0, self.context.clone()).unwrap()
    }
    fn config(&self) -> Value {
        json!({"revision":0,"roles":[{"id":"a","name":"Alpha <script>","instructions":"Perspective A","harness":"installed:claude","fixed":{}},{"id":"b","name":"Beta","instructions":"Perspective B","harness":"installed:claude","fixed":{}},{"id":"s","name":"Synthesis","instructions":"Synthesize evidence","harness":"installed:claude","fixed":{}}],"combinations":[{"id":"pair","name":"Pair","members":["a","b"],"disabled":[],"synthesizer":"s"}]})
    }
    fn calls(&self) -> usize {
        fs::read_to_string(self.root.join("bin/claude.calls"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
fn api(
    s: &WebServer,
    method: &str,
    path: &str,
    body: Option<Value>,
    auth: bool,
    extra: &str,
) -> (u16, Value) {
    let body = body.map(|v| v.to_string()).unwrap_or_default();
    let mut c = TcpStream::connect(s.address()).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n",
        s.address(),
        body.len()
    );
    if auth {
        req += &format!("X-Orch-Capability: {}\r\n", s.capability());
    }
    if !body.is_empty() {
        req += "Content-Type: application/json\r\n";
    }
    req += extra;
    req += "\r\n";
    req += &body;
    c.write_all(req.as_bytes()).unwrap();
    let mut raw = String::new();
    c.take(8 * 1024 * 1024).read_to_string(&mut raw).unwrap();
    let (h, b) = raw.split_once("\r\n\r\n").unwrap();
    let status = h.split_whitespace().nth(1).unwrap().parse().unwrap();
    (status, serde_json::from_str(b).unwrap())
}
fn url(s: &WebServer, tail: &str) -> String {
    format!("/api/v1/projects/{}/fusion/{tail}", s.initial_project())
}
#[test]
fn role_crud_cas_and_request_guards_use_actual_http_without_native_calls() {
    let f = Fixture::new();
    let s = f.server();
    let cfg = url(&s, "config");
    assert_eq!(
        api(&s, "GET", &cfg, None, true, "").1["data"]["revision"],
        0
    );
    assert!(!f.root.join(".orch/fusion.json").exists());
    let body = json!({"expectedRevision":0,"config":f.config()});
    assert_eq!(api(&s, "POST", &cfg, Some(body.clone()), false, "").0, 403);
    assert_eq!(
        api(
            &s,
            "POST",
            &cfg,
            Some(body.clone()),
            true,
            "Origin: https://evil.invalid\r\n"
        )
        .0,
        403
    );
    assert!(!f.root.join(".orch/fusion.json").exists());
    let mut bad = body.clone();
    bad["config"]["roles"][0]["executable"] = json!("/bin/sh");
    assert_eq!(api(&s, "POST", &cfg, Some(bad), true, "").0, 400);
    let mut secret = body.clone();
    secret["config"]["roles"][0]["instructions"] = json!("Bearer fixture-secret-token");
    assert_eq!(api(&s, "POST", &cfg, Some(secret), true, "").0, 400);
    assert!(!f.root.join(".orch/fusion.json").exists());
    let saved = api(&s, "POST", &cfg, Some(body.clone()), true, "");
    assert_eq!(saved.0, 200);
    assert_eq!(saved.1["data"]["revision"], 1);
    assert_eq!(api(&s, "POST", &cfg, Some(body), true, "").0, 409);
    let mut next = saved.1["data"].clone();
    next["roles"].as_array_mut().unwrap().remove(0);
    next["combinations"][0]["members"] = json!(["b"]);
    assert_eq!(
        api(
            &s,
            "POST",
            &cfg,
            Some(json!({"expectedRevision":1,"config":next})),
            true,
            ""
        )
        .0,
        200
    );
    let request = json!({"requestId":"bad-count","combinationId":"pair","question":"Question"});
    assert_eq!(
        api(&s, "POST", &url(&s, "runs"), Some(request), true, "").0,
        400
    );
    assert_eq!(f.calls(), 0);
    assert!(!f.root.join(".orch/fusion-runs").exists());
}
#[test]
fn actual_http_run_is_idempotent_verified_and_isolated_from_other_projects() {
    let f = Fixture::new();
    let s = f.server();
    assert_eq!(
        api(
            &s,
            "POST",
            &url(&s, "config"),
            Some(json!({"expectedRevision":0,"config":f.config()})),
            true,
            ""
        )
        .0,
        200
    );
    assert_eq!(
        api(&s, "POST", &url(&s, "discovery"), Some(json!({})), true, "").0,
        202
    );
    assert_eq!(f.calls(), 0);
    let q =
        json!({"requestId":"http-run","combinationId":"pair","question":"Assess this fixture."});
    assert_eq!(
        api(&s, "POST", &url(&s, "runs"), Some(q.clone()), false, "").0,
        403
    );
    assert_eq!(
        api(
            &s,
            "POST",
            &url(&s, "runs"),
            Some(q.clone()),
            true,
            "X-Orch-Config-Revision: 0\r\n"
        )
        .0,
        409
    );
    assert_eq!(f.calls(), 0);
    assert_eq!(
        api(
            &s,
            "POST",
            &url(&s, "runs"),
            Some(q.clone()),
            true,
            "X-Orch-Config-Revision: 1\r\n"
        )
        .0,
        202
    );
    let start = Instant::now();
    let result = loop {
        let (code, v) = api(&s, "GET", &url(&s, "runs/http-run"), None, true, "");
        assert_eq!(code, 200);
        if v["data"]["phase"] == "completed" {
            break v;
        }
        assert!(start.elapsed() < Duration::from_secs(15), "{v}");
        std::thread::sleep(Duration::from_millis(30));
    };
    assert_eq!(result["data"]["members"].as_array().unwrap().len(), 2);
    assert_eq!(result["data"]["synthesis"]["answerStatus"], "verified");
    assert!(result["data"]["synthesis"]["answer"]
        .as_str()
        .unwrap()
        .contains("Verified fixture answer"));
    assert_eq!(f.calls(), 3);
    assert_eq!(api(&s, "POST", &url(&s, "runs"), Some(q), true, "").0, 202);
    assert_eq!(f.calls(), 3);
    let other = Fixture::new();
    let registered = api(
        &s,
        "POST",
        "/api/v1/projects",
        Some(json!({"root":other.root})),
        true,
        "",
    );
    let id = registered.1["data"]["id"].as_str().unwrap();
    let path = format!("/api/v1/projects/{id}/fusion/config");
    assert_eq!(
        api(&s, "GET", &path, None, true, "").1["data"]["roles"],
        json!([])
    );
}
