//! Production socket and observation safety contract; immutable until Recorded.
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
fn actual_loopback_and_html_bootstrap() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    assert_eq!(s.address().ip().to_string(), "127.0.0.1");
    assert_ne!(s.address().port(), 0);
    let (code, h, b) = http(&s, "GET", "/", "", &[], false);
    assert_eq!(code, 200);
    assert!(b.contains("orch-capability"));
    assert!(b.contains(s.capability()));
    assert!(h.contains("content-security-policy:"));
    assert!(h.contains("frame-ancestors 'none'"));
    assert!(h.contains("cache-control: no-store"));
    assert!(h.contains("x-content-type-options: nosniff"));
}
#[test]
fn independent_request_guards() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let p = snapshot(&s);
    assert_eq!(http(&s, "GET", &p, "", &[], false).0, 403);
    for pair in [
        ("Host", "evil.example"),
        ("Origin", "https://evil.example"),
        ("Sec-Fetch-Site", "cross-site"),
        ("Sec-Fetch-Site", "same-site"),
    ] {
        let (code, h, _) = http(&s, "GET", &p, "", &[(pair.0, pair.1.into())], true);
        assert_eq!(code, 403, "{}", pair.0);
        assert!(!h.contains("access-control-allow-origin"));
    }
    assert_eq!(http(&s, "OPTIONS", &p, "", &[], true).0, 405);
    assert_eq!(
        http(
            &s,
            "GET",
            &p,
            "",
            &[
                ("Origin", format!("http://{}", s.address())),
                ("Sec-Fetch-Site", "same-origin".into())
            ],
            true
        )
        .0,
        200
    );
}
#[test]
fn project_validation_and_transaction() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    fs::create_dir(f.0.join("sub")).unwrap();
    let before = http(&s, "GET", "/api/v1/projects", "", &[], true).2;
    for p in [f.0.join("sub"), f.0.join("missing"), f.0.join("tracked")] {
        assert_eq!(
            http(
                &s,
                "POST",
                "/api/v1/projects",
                &json!({"root":p}).to_string(),
                &[],
                true
            )
            .0,
            400
        );
    }
    assert_eq!(before, http(&s, "GET", "/api/v1/projects", "", &[], true).2);
    assert_eq!(
        http(
            &s,
            "GET",
            "/api/v1/projects/unknown/snapshot",
            "",
            &[],
            true
        )
        .0,
        404
    );
}

#[test]
fn project_registration_rejects_an_unborn_repository_without_changing_projects() {
    let f = Fixture::new();
    let unborn = f.0.join("unborn");
    fs::create_dir(&unborn).unwrap();
    assert!(Command::new("git").args(["init", "-q"]).arg(&unborn)
        .status().unwrap().success());
    let s = WebServer::start(&f.0, 0).unwrap();
    let before = http(&s, "GET", "/api/v1/projects", "", &[], true).2;
    assert_eq!(http(&s, "POST", "/api/v1/projects", &json!({"root":unborn}).to_string(),
        &[("Origin", format!("http://{}", s.address())), ("Sec-Fetch-Site", "same-origin".into())], true).0, 400);
    assert_eq!(before, http(&s, "GET", "/api/v1/projects", "", &[], true).2);
}

#[test]
fn project_ids_and_generations_are_independent() {
    let a = Fixture::new();
    let b = Fixture::new();
    let s = WebServer::start(&a.0, 0).unwrap();
    let reg = http(
        &s,
        "POST",
        "/api/v1/projects",
        &json!({"root":b.0}).to_string(),
        &[],
        true,
    );
    assert_eq!(reg.0, 200);
    let j: Value = serde_json::from_str(&reg.2).unwrap();
    let id = j["data"]["id"].as_str().unwrap();
    assert_ne!(id, s.initial_project());
    assert_eq!(
        j["data"]["id"],
        serde_json::from_str::<Value>(
            &http(
                &s,
                "POST",
                "/api/v1/projects",
                &json!({"root":b.0}).to_string(),
                &[],
                true
            )
            .2
        )
        .unwrap()["data"]["id"]
    );
    let p = snapshot(&s);
    let x: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
    let other = format!("/api/v1/projects/{id}/snapshot");
    for _ in 0..2 {
        assert_eq!(http(&s, "GET", &other, "", &[], true).0, 200);
    }
    let y: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
    assert_eq!(
        y["snapshotGeneration"].as_u64().unwrap(),
        x["snapshotGeneration"].as_u64().unwrap() + 1
    );
    assert_eq!(x["projectId"], s.initial_project());
    assert!(!x["serverInstanceId"].as_str().unwrap().is_empty());
}
#[test]
fn structural_redaction_and_multiline() {
    let v = sanitize_payload(
        &json!({"safe":"OD-OC/qwen3.8-max","nested":[{"API_KEY":"canary_A","Authorization":"Basic canary_B","cookie":"canary_C","text":"Bearer canary_D"}],"body":"# Title\n\n\tcode","split":"sk-\u{1b}[0mcanary_E"}),
    );
    let s = v.to_string();
    for x in ["canary_A", "canary_B", "canary_C", "canary_D", "canary_E"] {
        assert!(!s.contains(x), "{x}");
    }
    assert_eq!(v["safe"], "OD-OC/qwen3.8-max");
    assert_eq!(v["body"], "# Title\n\n\tcode");
}
#[test]
fn verified_multiline_and_tamper_over_actual_http() {
    let f = Fixture::new();
    let dir = f.consultation();
    let s = WebServer::start(&f.0, 0).unwrap();
    let snap: Value =
        serde_json::from_str(&http(&s, "GET", &snapshot(&s), "", &[], true).2).unwrap();
    let id = snap["data"]["rows"][0]["id"].as_str().unwrap();
    let p = format!("/api/v1/projects/{}/detail?id={id}", s.initial_project());
    let d: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
    assert_eq!(d["data"]["row"]["result"], "verified");
    assert!(d["data"]["text"]
        .as_str()
        .unwrap()
        .contains("\n\n- one\n- two"));
    fs::write(dir.join("fusion/0-one.md"), "tampered").unwrap();
    let d: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
    assert!(d["data"]["text"].is_null());
    assert_ne!(d["data"]["row"]["result"], "verified");
}
#[test]
fn colon_qualified_selfhost_review_reads_over_actual_http() {
    let f = Fixture::new();
    let round = f.0.join("coordination/rounds/r90");
    fs::create_dir_all(&round).unwrap();
    fs::write(f.0.join("review.md"), "answer").unwrap();
    let tuple = json!({"provider":null,"model":"fixture","effort":null,"mode":null});
    let binding = json!({"driver":"smartclaw","harness":"smartclaw","fixedHead":"a".repeat(40),"invocationCwd":f.0,"observationSource":"fixture","cwdSelection":"project-root","configDigest":"a".repeat(64),"requestDigest":"b".repeat(64),"attachmentManifestSha256":"c".repeat(64),"commandDigest":"d".repeat(64),"executableIdentityDigest":"e".repeat(64),"requestedTuple":tuple.clone(),"effectiveTuple":tuple});
    let wake = "00000001-1111-4111-8111-111111111111";
    let wake_event = "01M2JKYB6NPJZWC7Q15EEKZBYJ";
    let requested = "00000000000000000000000002";
    let terminal = "00000000000000000000000003";
    let event = |id: &str, kind: &str, payload: Value| json!({"eventId":id,"ts":"2026-09-15T13:26:16Z","round":"r90","taskId":"B362","actor":"runtime:orch","type":kind,"payload":payload});
    let mut issued = binding.clone();
    issued["method"] = "unified-channel-v1".into();
    issued["wakeId"] = wake.into();
    issued["attemptId"] = "B362-A0001".into();
    issued["action"] = "review".into();
    let events = vec![
        event(wake_event, "WakeIssued", issued),
        event(requested, "ReviewRequested", json!({"wakeId":wake,"attemptId":"B362-A0001","reviewedHead":"a".repeat(40),"harness":"smartclaw"})),
        event(terminal, "ManagedWakeTerminated", json!({"wakeId":wake,"agent":"smartclaw","state":"answered","turnEnded":true,"terminalSeen":true,"managedScopeTerminated":true,"mechanicalTerminalAbsent":false,"channelBinding":binding})),
        event("00000000000000000000000004", "ReviewDelivered", json!({"wakeId":wake,"attemptId":"B362-A0001","harness":"smartclaw","reviewedHead":"a".repeat(40),"requestEventId":requested,"terminalEventId":terminal,"path":"review.md","sha256":"0db52f4076c082518412afd3dd3576e2cb0c63703fd7fed5e23ade60efef31d9","bytes":6,"verdict":"PASS"})),
    ];
    fs::write(round.join("events.jsonl"), events.iter().map(|v| format!("{v}\n")).collect::<String>()).unwrap();
    let s = WebServer::start(&f.0, 0).unwrap();
    let snapshot: Value = serde_json::from_str(&http(&s, "GET", &snapshot(&s), "", &[], true).2).unwrap();
    let expected = format!("selfhost:r90:{wake_event}");
    assert!(snapshot["data"]["rows"].as_array().unwrap().iter().any(|row| row["id"] == expected && row["result"] == "verified"));
    let path = format!("/api/v1/projects/{}/detail?id=selfhost%3Ar90%3A{wake_event}", s.initial_project());
    let (code, _, body) = http(&s, "GET", &path, "", &[], true);
    assert_eq!(code, 200);
    let detail: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(detail["data"]["row"]["id"], expected);
    assert_eq!(detail["data"]["row"]["result"], "verified");
    assert_eq!(detail["data"]["text"], "answer");
}
#[test]
fn refresh_failure_has_no_false_generation() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let p = snapshot(&s);
    let before: Value = serde_json::from_str(&http(&s, "GET", &p, "", &[], true).2).unwrap();
    let moved = f.0.with_extension("moved");
    fs::rename(&f.0, &moved).unwrap();
    let (code, _, body) = http(&s, "GET", &p, "", &[], true);
    fs::rename(&moved, &f.0).unwrap();
    assert_eq!(code, 503);
    let e: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(e["snapshotGeneration"], before["snapshotGeneration"]);
    assert_eq!(e["error"]["code"], "read_failed");
}
#[test]
fn unrelated_round_clipping_is_not_consultation_clipping() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("coordination/consultations")).unwrap();
    for i in 1..=10 {
        fs::create_dir_all(f.0.join(format!("coordination/rounds/r{i}"))).unwrap();
    }
    let mut r = ObservationReader::open(&f.0).unwrap();
    let s = r.refresh().unwrap();
    assert!(s.truncated);
    assert!(!s
        .diagnostics
        .iter()
        .any(|x| x == "consultation candidates clipped"));
}
#[test]
fn service_rejects_occupied_port_and_releases_listener() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let a = s.address();
    assert!(WebServer::start(&f.0, a.port()).is_err());
    drop(s);
    let next = WebServer::start(&f.0, a.port()).unwrap();
    assert_eq!(next.address(), a);
}
#[test]
fn invalid_project_start_and_path_limits() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("empty")).unwrap();
    assert!(WebServer::start(&f.0.join("empty"), 0).is_err());
    let s = WebServer::start(&f.0, 0).unwrap();
    assert_eq!(
        http(
            &s,
            "POST",
            "/api/v1/projects",
            &json!({"root":"x".repeat(9000)}).to_string(),
            &[],
            true
        )
        .0,
        413
    );
}
#[test]
fn whole_api_keeps_project_files_unchanged() {
    fn tree(p: &Path) -> Vec<(PathBuf, Vec<u8>)> {
        let mut out = vec![];
        for e in fs::read_dir(p).unwrap().flatten() {
            if e.file_type().unwrap().is_dir() {
                out.extend(tree(&e.path()));
            } else if e.file_type().unwrap().is_file() {
                out.push((e.path(), fs::read(e.path()).unwrap()));
            }
        }
        out.sort();
        out
    }
    let f = Fixture::new();
    let before = tree(&f.0);
    let s = WebServer::start(&f.0, 0).unwrap();
    http(&s, "GET", &snapshot(&s), "", &[], true);
    http(&s, "GET", "/api/v1/projects", "", &[], true);
    drop(s);
    assert_eq!(before, tree(&f.0));
}
#[test]
fn public_contract_docs() {
    let text = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    for clause in [
        "X-Orch-Capability",
        "snapshotGeneration",
        "same-UID",
        "detail_multiline",
        "orch-web",
    ] {
        assert!(text.contains(clause), "{clause}");
    }
}
