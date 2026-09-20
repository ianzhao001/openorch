//! Frozen r90/B356 frontend contract; successor to the immutable B355 fixture.
//!
//! Required negative mutations: grouping round/task, fusion membership, global
//! window-before-priority, invalid-answer priority, strict RFC3339 fallback,
//! filter intersection, every response-identity guard, answer revalidation,
//! cookie preference validation, URL allowlist, asset byte/MIME binding, CLI
//! argument/lifecycle handling, and owned shutdown. Restore every mutant exactly.
use orch_ui::web::{run_web_cli, WebServer};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
fn js(program: &str) {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web_assets");
    let source=format!("import assert from 'node:assert/strict'; import {{pathToFileURL}} from 'node:url'; const m=await import(pathToFileURL(process.argv[1]+'/model.mjs')); {program}");
    let out = Command::new("node")
        .args(["--input-type=module", "--eval", &source])
        .arg(assets)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
fn app_js(program: &str) {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web_assets");
    let source = format!("import assert from 'node:assert/strict';import fs from 'node:fs';const app=fs.readFileSync(process.argv[1]+'/app.mjs','utf8');const fragment=app.slice(app.indexOf('async function request('),app.indexOf('function formatTime')).replace('export function createDetailDeadline','function createDetailDeadline');const build=fetch=>new Function('fetch','Headers','setTimeout','clearTimeout',`const capability='capability';${{fragment}};return {{request,createDetailDeadline}};`)(fetch,Headers,setTimeout,clearTimeout);const make=fetch=>build(fetch).request;{program}");
    let out = Command::new("node")
        .args(["--input-type=module", "--eval", &source])
        .arg(assets)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
const ROWS: &str = r#"const row=(id,over={})=>({id,source:'consult',summary:'',phase:'prepared',result:'none',alias:'one',driver:'claude',source_time:'2026-09-15T10:00:00Z',...over});"#;
#[test]
fn explicit_tasks_keep_round_and_versions() {
    js(&(ROWS.to_owned()
        + r#"const task=(round,attempt,head)=>({round,id:'B1',attempt,head,state:'invoked'}); const r=[row('a',{task:task('r1','A1','h1')}),row('b',{task:task('r1','A2','h2')}),row('c',{task:task('r2','A1','h1')})]; const p=m.project(r,[],{}); assert.equal(p.cards.length,2); const card=p.cards.find(c=>c.rows.length===2); assert.equal(card.versions.length,2);"#));
}
#[test]
fn fusion_and_unassociated_rows_are_honest() {
    js(&(ROWS.to_owned()
        + r#"const r=[row('a',{fusion_id:'F'}),row('b',{fusion_id:'F',alias:'two'}),row('c',{alias:null,driver:null})];const p=m.project(r,[{id:'F',members:['a','b'],total:null,roster_complete:false}],{});assert.equal(p.cards.length,2);const f=p.cards.find(c=>c.kind==='fusion');assert.equal(f.rows.length,2);assert.equal(f.group.total,null);assert.equal(f.group.roster_complete,false);assert.equal(m.project(r,[],{view:'members'}).cards.length,3);"#));
}
#[test]
fn filter_then_window_then_failure_priority() {
    js(&(ROWS.to_owned()
        + r#"const task=(i)=>({round:'r1',id:'B'+i,attempt:'A1',head:'h'+i,state:'invoked'});const r=Array.from({length:36},(_,i)=>row(String(i).padStart(2,'0'),{task:i%2?task(i):null,fusion_id:i%2?null:'F'+i,source_time:new Date(Date.UTC(2026,8,15,10,i)).toISOString(),result:i===0?'failed':i===20?'invalid':i===32?'failed':i===33?'none':i===34?'unknown':i===35?'too-large':'verified'}));let p=m.project(r,[],{limit:30});assert.equal(p.cards.length,30);assert.equal(p.cards[0].rows[0].id,'32');assert.equal(p.cards[1].rows[0].id,'20');assert.equal(p.hiddenFailures,1);assert.equal(p.outsideWindow,6);assert(!p.cards.some(c=>c.rows[0].id==='00'));assert.equal(p.sections.tasks.length+p.sections.unassociated.length,30);p=m.project(r,[],{limit:30,result:'failed'});assert.deepEqual(p.cards.map(c=>c.rows[0].id),['32','00']);assert.equal(m.project(r,[],{limit:30,offset:30}).cards.length,6);"#));
}
#[test]
fn historical_capture_gap_stays_in_invalid_filter_without_failure_priority() {
    js(&(ROWS.to_owned()
        + r#"const historical=row('historical',{result:'invalid',parameters:{channelDiagnostic:{code:'capture_evidence_missing'}}});const failed=row('failed',{result:'failed',source_time:'2026-09-15T09:00:00Z'});let p=m.project([historical,failed],[],{});assert.equal(m.historicalUnverified(historical),true);assert.equal(p.cards[0].rows[0].id,'failed');assert.equal(p.cards.find(c=>c.rows[0].id==='historical').failed,false);p=m.project([historical,failed],[],{result:'invalid'});assert.deepEqual(p.cards.map(c=>c.rows[0].id),['historical']);assert.equal(p.hiddenFailures,0);"#));
}
#[test]
fn timestamps_offsets_unknown_and_stable_ties() {
    js(&(ROWS.to_owned()
        + r#"const t={round:'r1',id:'B1',attempt:'A1',head:'h1',state:'invoked'};const r=[row('z',{source_time:'2026-09-15 10:00:00',started_at:null}),row('b',{source_time:'2026-09-15T12:00:00+02:00'}),row('a',{source_time:'2026-09-15T10:00:00Z'}),row('fallback',{source_time:null,started_at:'2026-09-15T11:00:00Z'}),row('task-old',{task:t,source_time:'2026-09-15T09:00:00Z'}),row('task-new',{task:t,source_time:'2026-09-15T12:00:00Z'})];const p=m.project(r,[],{});assert.equal(p.cards[0].rows[0].id,'task-new');assert.equal(p.cards[0].time,Date.parse('2026-09-15T12:00:00Z'));assert.equal(m.stamp(r[0]),null);assert.equal(m.stamp(r[3]),Date.parse('2026-09-15T11:00:00Z'));assert.deepEqual(p.cards.slice(-3).map(c=>c.rows[0].id),['a','b','z']);"#));
}
#[test]
fn missing_answer_is_not_failure_and_mixed_state_not_maximum() {
    js(&(ROWS.to_owned()
        + r#"const t={round:'r1',id:'B1',attempt:'A1',head:'h1',state:'recorded'};const r=[row('a',{task:t}),row('b',{task:{...t,attempt:'A2',head:'h2',state:'invoked'},source_time:'2026-09-15T11:00:00Z'})];const c=m.project(r,[],{}).cards[0];assert.equal(c.taskState,'invoked');assert.equal(c.mixed,true);assert.equal(c.failed,false);assert.equal(c.summary,null);"#));
}
#[test]
fn filters_are_intersections() {
    js(&(ROWS.to_owned()
        + r#"const task={round:'r1',id:'B7',attempt:'A1',head:'h1',state:'recorded'};const r=[row('a',{summary:'HELLO',result:'verified',purpose:'review',task}),row('b',{summary:'hello',alias:'two',result:'failed',purpose:'consult'}),row('c',{summary:'other',result:'unknown'})];assert.equal(m.project(r,[],{query:'hello',member:'one / claude',result:'verified',task:'B7',taskState:'recorded',purpose:'review',time:'all'}).cards.length,1);assert.equal(m.project(r,[],{query:'hello',member:'one / claude',result:'failed'}).cards.length,0);assert.equal(m.project(r,[],{result:'unknown'}).cards[0].rows[0].id,'c');assert.equal(m.project([row('u',{source_time:'invalid'})],[],{time:'24h',now:Date.parse('2026-09-15T12:00:00Z')}).cards.length,0);"#));
}
#[test]
fn stale_project_request_and_generation_are_rejected() {
    js(
        r#"let s=m.initialState('S','P');s=m.beginRefresh(s);const e={serverInstanceId:'S',projectId:'P',snapshotGeneration:4,data:{rows:[{id:'a',result:'verified'}]}};s=m.applySnapshot(s,e,s.epoch,s.lastSeq);assert.equal(s.generation,4);assert.equal(s.snapshot.rows[0].id,'a');const same=m.applySnapshot(s,e,s.epoch,s.lastSeq);assert.deepEqual(same,s);for(const bad of [{...e,projectId:'Q'},{...e,serverInstanceId:'old'},{...e,snapshotGeneration:3}])assert.deepEqual(m.applySnapshot(s,bad,s.epoch,s.lastSeq),s);assert.deepEqual(m.applySnapshot(s,e,s.epoch-1,s.lastSeq),s);assert.deepEqual(m.applySnapshot(s,e,s.epoch,s.lastSeq-1),s);s=m.select(s,'a');s=m.beginDetail(s,'a');assert.equal(s.answer,null);s=m.applyDetail(s,{...e,snapshotGeneration:5,data:{row:{id:'a',result:'verified'},text:'ok'}},s.epoch,s.detailSeq,'a');assert.equal(s.answer,'ok');const changed=m.select(s,'b');assert.equal(m.applyDetail(changed,{...e,snapshotGeneration:6,data:{row:{id:'a',result:'verified'},text:'wrong'}},changed.epoch,changed.detailSeq,'a').answer,null);const pending=m.beginRefresh(s);const failed=m.failRefresh(pending,'read_failed',1234);assert.equal(failed.snapshot.rows[0].id,'a');assert.equal(failed.refreshError,'read_failed');assert.equal(failed.lastSuccessAt,1234);assert.equal(failed.answer,'ok');"#,
    );
}
#[test]
fn request_preserves_error_codes_and_rejects_invalid_json() {
    app_js(
        r#"const envelope={serverInstanceId:'S',projectId:'P',snapshotGeneration:1,error:{code:'busy'}};await assert.rejects(make(async()=>({ok:false,status:503,json:async()=>envelope}))('/x'),error=>error.message==='busy'&&error.httpStatus===503&&error.envelope===envelope);for(const body of [null,[],{}, {serverInstanceId:'S',snapshotGeneration:1}]){await assert.rejects(make(async()=>({ok:false,status:503,json:async()=>body}))('/x'),error=>error.message==='invalid_response');await assert.rejects(make(async()=>({ok:true,status:200,json:async()=>body}))('/x'),error=>error.message==='invalid_response');}await assert.rejects(make(async()=>({ok:true,status:200,json:async()=>{throw Error('bad json')}}))('/x'),error=>error.message==='invalid_response'&&error.httpStatus===200);let seen;const controller=new AbortController();const value=await make(async(path,init)=>{seen=init;return{ok:true,status:200,json:async()=>({serverInstanceId:'S',projectId:'P',snapshotGeneration:1,data:'ok'})}})('/x',{signal:controller.signal});assert.equal(value.data,'ok');assert.equal(seen.signal,controller.signal);assert.equal(seen.headers.get('X-Orch-Capability'),'capability');"#,
    );
}
#[test]
fn current_malformed_detail_settles_but_superseded_detail_is_silent() {
    js(
        r#"let s=m.initialState('S','P');s={...s,snapshot:{rows:[{id:'a',result:'verified'}],groups:[]},selected:'a'};s=m.beginDetail(s,'a');const seq=s.detailSeq;const malformed=m.applyDetail(s,{serverInstanceId:'S',projectId:'P',snapshotGeneration:1,data:{}},s.epoch,seq,'a');assert.equal(malformed.detailPending,false);assert.equal(malformed.detailError,'invalid_response');const changed=m.select(s,'b');assert.deepEqual(m.applyDetail(changed,{serverInstanceId:'S',projectId:'P',snapshotGeneration:1,data:{}},changed.epoch,seq,'a'),changed);"#,
    );
}
#[test]
fn detail_deadline_uses_exact_bound_and_supports_cancellation() {
    app_js(
        r#"const api=build(async()=>{});let callback,scheduled,cancelled,aborted=0;const controller={abort(){aborted+=1}};const deadline=api.createDetailDeadline(controller,{schedule(fn,ms){callback=fn;scheduled=ms;return 41},cancel(id){cancelled=id}});assert.equal(scheduled,12000);assert.equal(deadline.timedOut,false);callback();assert.equal(deadline.timedOut,true);assert.equal(aborted,1);deadline.cancel();assert.equal(cancelled,41);deadline.cancel();assert.equal(cancelled,41);"#,
    );
}
#[test]
fn theme_language_cookie_and_storage_failure() {
    js(
        r#"assert.deepEqual(m.preferences('','zh-CN'),{theme:'system',lang:'zh'});assert.deepEqual(m.preferences('orch_theme=light; orch_lang=en','zh-CN'),{theme:'light',lang:'en'});assert.deepEqual(m.preferences('orch_theme=evil; orch_lang=xx','fr'),{theme:'system',lang:'en'});const writes=[];assert(m.writePreference({set cookie(v){writes.push(v)}},'theme','dark'));assert(writes[0].includes('orch_theme=dark'));assert(writes[0].includes('SameSite=Strict'));assert(!m.writePreference({set cookie(v){throw Error('blocked')}},'lang','zh'));"#,
    );
}
#[test]
fn link_protocol_allowlist() {
    js(
        r#"for(const s of ['javascript:alert(1)','data:text/html,hi','file:///etc/passwd','java\nscript:alert(1)','java%0ascript:alert(1)','&#x6a;avascript:alert(1)','//evil.test','/api/v1/projects','ftp://example.com']) assert.equal(m.safeHref(s),null,s);assert.equal(m.safeHref('https://example.com/a'),'https://example.com/a');assert.equal(m.safeHref('http://example.com/a'),'http://example.com/a');"#,
    );
}
#[test]
fn real_cli_help_and_argument_errors() {
    assert_eq!(run_web_cli(&["--help".into()]), 0);
    assert_eq!(run_web_cli(&["--bad-option".into()]), 2);
    assert_eq!(run_web_cli(&["--port".into(), "-1".into()]), 2);
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = orch_host::util::test_scratch_dir("web-ui 中文 space");
        fs::write(p.join("tracked"), "fixture").unwrap();
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
            let o = Command::new("git")
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "commit.gpgSign=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-C",
                ])
                .arg(&p)
                .args(args)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        }
        Self(p)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn get(addr: std::net::SocketAddr, path: &str) -> String {
    let mut c = TcpStream::connect(addr).unwrap();
    c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    c.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n").as_bytes(),
    )
    .unwrap();
    let mut s = String::new();
    c.take(1024 * 1024).read_to_string(&mut s).unwrap();
    s
}
fn split_http(raw: &str) -> (&str, &str) {
    raw.split_once("\r\n\r\n").expect("complete HTTP response")
}
#[test]
fn actual_service_delivers_fixed_ui_assets() {
    let f = Fixture::new();
    let s = WebServer::start(&f.0, 0).unwrap();
    let html = get(s.address(), "/");
    assert!(html.starts_with("HTTP/1.1 200"));
    for id in ["project-select", "search", "cards", "drawer", "reader"] {
        assert!(html.contains(&format!("id=\"{id}\"")), "{id}");
    }
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web_assets");
    for (asset, file, mime) in [
        ("/assets/model.mjs", "model.mjs", "text/javascript; charset=utf-8"),
        ("/assets/app.mjs", "app.mjs", "text/javascript; charset=utf-8"),
        ("/assets/style.css", "style.css", "text/css; charset=utf-8"),
        ("/assets/marked.mjs", "marked.mjs", "text/javascript; charset=utf-8"),
    ] {
        let raw = get(s.address(), asset);
        assert!(raw.starts_with("HTTP/1.1 200"), "{asset}");
        let (headers, body) = split_http(&raw);
        assert!(headers.to_lowercase().contains("x-content-type-options: nosniff"));
        assert!(headers.to_lowercase().contains(&format!("content-type: {mime}")));
        assert_eq!(body.as_bytes(), fs::read(assets.join(file)).unwrap());
    }
    assert!(get(s.address(), "/assets/no-such-file").starts_with("HTTP/1.1 404"));
    assert!(get(s.address(), "/assets/../Cargo.toml").starts_with("HTTP/1.1 404"));
}
#[test]
fn actual_bin_reports_url_and_exits_on_owned_sigint() {
    struct Owned(std::process::Child);
    impl Drop for Owned {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let f = Fixture::new();
    let mut child = Owned(
        Command::new(env!("CARGO_BIN_EXE_orch-web"))
            .current_dir(&f.0)
            .args(["--no-open", "--root"])
            .arg(&f.0)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    let t = std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line);
        let _ = tx.send(result.map(|_| line));
    });
    let line = rx
        .recv_timeout(std::time::Duration::from_secs(15))
        .expect("bin startup ready line")
        .unwrap();
    t.join().unwrap();
    let url = line
        .trim()
        .strip_prefix("OpenOrch Web: ")
        .expect("documented ready URL");
    let addr = url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .unwrap()
        .parse()
        .unwrap();
    assert!(get(addr, "/").starts_with("HTTP/1.1 200"));
    assert!(Command::new("/bin/kill")
        .args(["-INT", &child.0.id().to_string()])
        .status()
        .unwrap()
        .success());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "owned SIGINT shutdown"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
#[ignore = "explicit real-Chrome acceptance; run for B356 required evidence"]
fn actual_chrome_dom_network_theme_locale_and_viewport_matrix() {
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/browser/web_browser_acceptance.mjs");
    let out = Command::new("node")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_orch-web"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    for proof in [
        "matrix=light/en/wide,dark/zh/medium,system/zh/narrow",
        "externalRequests=0",
        "keyboard=pass",
        "markdownDom=pass",
        "overflow=pass",
        "readonly=pass",
        "readerPolling=delayed-detail-three-refreshes-pass",
        "diagnostics=historical-neutral-language-width-filter-pass",
    ] {
        assert!(text.contains(proof), "missing {proof}: {text}");
    }
}
