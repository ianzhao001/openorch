//! B353 immutable replacement seed: grouping, identity, invalidation, safe rendering, input,
//! real independent binary and non-TTY boundary. Mutate each behavior separately:
//! alias dedup, task-key truncation, index selection, retain invalid detail,
//! failed-switch replacement, unknown-total ratio, raw terminal/clipboard fields,
//! input-mode hotkeys, zero-height arithmetic, hidden refresh error, TTY bypass.
use orch_host::observation::{FusionObservation, InvocationDetail, InvocationObservation, ProjectObservation, TaskObservation};
use orch_ui::app::{AppState, Effect, Grouping, ObservationApp};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::{collections::BTreeMap, path::{Path,PathBuf}, process::Command, fs, os::unix::fs::PermissionsExt};

fn row(id: &str) -> InvocationObservation {
    InvocationObservation { id:id.into(), source:"consult".into(), alias:Some("same".into()), driver:Some("pi".into()), purpose:Some("consult".into()), summary:"中文状态".into(), requested_tuple:serde_json::json!({"model":"captured"}), effective_tuple:serde_json::json!({}), parameters:serde_json::json!({}), activity:serde_json::json!({}), head:None, started_at:None, source_time:None, phase:"spawned".into(), duration_secs:None, result:"none".into(), native_status:serde_json::json!({}), task:None, fusion_id:Some("wave".into()), locator:format!("/project/{id}"), diagnostics:vec![] }
}
fn snapshot(rows: Vec<InvocationObservation>) -> ProjectObservation {
    let members=rows.iter().map(|r|r.id.clone()).collect();let mut phases=BTreeMap::new();let mut results=BTreeMap::new();for r in &rows{*phases.entry(r.phase.clone()).or_insert(0)+=1;*results.entry(r.result.clone()).or_insert(0)+=1;}
    ProjectObservation {root:"/project".into(),read_at:"2026-09-14T00:00:00Z".into(),rows,groups:vec![FusionObservation{id:"wave".into(),members,total:None,roster_complete:false,phase_counts:phases,result_counts:results}],diagnostics:vec![],truncated:false}
}
fn text(s:&AppState,w:u16,h:u16)->String { let b=s.render(w,h); b.content.iter().map(|c|c.symbol()).collect::<Vec<_>>().join("") }
fn key(s:&mut AppState,c:KeyCode)->Effect { s.key(KeyEvent::new(c,KeyModifiers::NONE),10) }

#[test] fn three_groupings_preserve_every_invocation_and_stable_id() {
    let mut s=AppState::new(snapshot(vec![row("b"),row("a")]));s.select("b");
    for g in [Grouping::Calls,Grouping::Harness,Grouping::Tasks] {s.set_grouping(g);let mut ids=s.ordered_ids();ids.sort();assert_eq!(ids,vec!["a","b"]);assert_eq!(s.selected_id.as_deref(),Some("b"));}
    s.apply_snapshot(snapshot(vec![row("a"),row("b")]));assert_eq!(s.selected_id.as_deref(),Some("b"));
}
#[test] fn task_group_keys_keep_round_attempt_and_head() {
    let mut a=row("a");a.task=Some(TaskObservation{round:"r89".into(),id:"B351".into(),attempt:Some("A1".into()),state:"pending".into(),head:Some("a".repeat(40))});let mut b=a.clone();b.id="b".into();b.task.as_mut().unwrap().head=Some("b".repeat(40));let mut c=a.clone();c.id="c".into();c.task.as_mut().unwrap().round="r90".into();let mut d=a.clone();d.id="d".into();d.task.as_mut().unwrap().attempt=Some("A2".into());
    let mut s=AppState::new(snapshot(vec![a,b,c,d,row("e")]));s.set_grouping(Grouping::Tasks);assert_ne!(s.group_key("a"),s.group_key("b"));assert_ne!(s.group_key("a"),s.group_key("c"));assert_ne!(s.group_key("a"),s.group_key("d"));let k=s.group_key("a").unwrap();for v in ["r89","B351","A1",&"a".repeat(40)]{assert!(k.contains(v));}assert!(s.group_key("e").unwrap().contains("unassociated"));
}
#[test] fn deleted_selection_and_empty_navigation_are_safe() {
    let mut s=AppState::new(snapshot(vec![row("a"),row("b")]));s.select("b");s.apply_snapshot(snapshot(vec![row("a")]));assert_eq!(s.selected_id.as_deref(),Some("a"));s.apply_snapshot(snapshot(vec![]));assert_eq!(s.selected_id,None);assert_eq!(key(&mut s,KeyCode::Enter),Effect::None);assert_eq!(key(&mut s,KeyCode::Char('y')),Effect::None);for c in [KeyCode::Down,KeyCode::Up,KeyCode::PageDown,KeyCode::PageUp]{key(&mut s,c);}
}
#[test] fn detail_revalidation_replaces_row_and_removes_verified_text() {
    let mut a=row("a");a.result="verified".into();let mut s=AppState::new(snapshot(vec![a.clone()]));s.apply_detail(InvocationDetail{row:a.clone(),text:Some("old answer".into()),locator:"/project/a".into(),truncated:false});a.result="invalid".into();s.apply_detail(InvocationDetail{row:a,text:None,locator:"/project/a".into(),truncated:false});assert!(!text(&s,100,20).contains("old answer"));assert_eq!(s.snapshot.rows[0].result,"invalid");assert!(s.detail.as_ref().map_or(true,|d|d.text.is_none()));
}
#[test] fn refresh_invalidates_detail_even_if_id_remains() {
    let mut a=row("a");a.result="verified".into();let mut s=AppState::new(snapshot(vec![a.clone()]));s.apply_detail(InvocationDetail{row:a,text:Some("stale answer".into()),locator:"x".into(),truncated:false});s.apply_snapshot(snapshot(vec![row("a")]));assert!(s.detail.is_none());assert!(!text(&s,100,20).contains("stale answer"));
}
#[test] fn refresh_error_preserves_snapshot_and_reports_source_age() {
    let mut s=AppState::new(snapshot(vec![row("a")]));s.mark_error("read failed");assert_eq!(s.snapshot.rows.len(),1);let t=text(&s,140,25);assert!(t.contains("read failed"));assert!(t.contains("last successful"));assert!(t.contains("2026-09-14"));
}
#[test] fn unknown_fusion_total_and_native_result_are_independent() {
    let mut a=row("a");a.phase="ended".into();a.result="invalid".into();a.native_status=serde_json::json!({"ended":true});let s=AppState::new(snapshot(vec![a,row("b")]));let t=text(&s,180,25);for v in ["total unknown","ended","invalid","spawned"]{assert!(t.contains(v),"{v}: {t}");}assert!(!t.contains("100%"));assert!(t.contains("last observed"));
}
#[test] fn every_untrusted_render_and_clipboard_field_is_safe_before_clipping() {
    let mut a=row("a");a.summary="x=1\nAPI_KEY=abcdef".into();a.locator="/path/sk-\u{1b}[31mSECRET".into();a.requested_tuple=serde_json::json!({"model":"Bearer se\u{1b}[31mcret"});let mut s=AppState::new(snapshot(vec![a]));s.mark_error("API_KE\u{7}Y=abcdef");let t=text(&s,200,30);for v in ["abcdef","SECRET","Bearer","\u{1b}","\u{7}"]{assert!(!t.contains(v));}assert_eq!(s.safe_locator(),Some("[redacted]".into()));assert_eq!(key(&mut s,KeyCode::Char('y')),Effect::CopyLocator("[redacted]".into()));let mut clean=AppState::new(snapshot(vec![row("safe")]));clean.apply_detail(InvocationDetail{row:row("safe"),text:Some("普通详情".into()),locator:"/project/safe".into(),truncated:false});let visible=visible_text(&clean.render(200,30));for v in ["中文状态","captured","/project/safe"]{assert!(visible.contains(v),"{v}: {visible}");}
}
#[test] fn project_input_treats_hotkeys_as_text_and_cancel_preserves_context() {
    let mut s=AppState::new(snapshot(vec![row("a")]));assert_eq!(key(&mut s,KeyCode::Char('p')),Effect::None);for c in ['q','y','r','中',' ']{assert_eq!(key(&mut s,KeyCode::Char(c)),Effect::None);}assert_eq!(s.project_input.as_deref(),Some("qyr中 "));assert_eq!(key(&mut s,KeyCode::Enter),Effect::SwitchProject("qyr中 ".into()));key(&mut s,KeyCode::Char('p'));key(&mut s,KeyCode::Esc);assert!(s.project_input.is_none());assert_eq!(s.snapshot.root,"/project");assert_eq!(s.key(KeyEvent::new(KeyCode::Char('c'),KeyModifiers::CONTROL),10),Effect::Quit);
}
#[test] fn narrow_unicode_screens_and_paging_do_not_panic() {
    let mut s=AppState::new(snapshot((0..80).map(|n|row(&format!("中文{n}"))).collect()));for w in [0,1,2,10,80]{for h in [0,1,2,8]{let b=s.render(w,h);assert_eq!(b.area.width,w);assert_eq!(b.area.height,h);key(&mut s,KeyCode::PageDown);key(&mut s,KeyCode::PageUp);}}
}
#[test] fn real_reader_switch_failure_preserves_old_project() {
    let f=Fixture::new();f.run();let mut app=ObservationApp::open(&f.root).unwrap();assert_eq!(app.state.snapshot.rows.len(),1);assert_eq!(app.state.snapshot.rows[0].result,"verified");app.show_detail().unwrap();let before=serde_json::to_value(&app.state.snapshot).unwrap();let root=app.state.snapshot.root.clone();assert!(app.switch_project(Path::new("/definitely-missing-b351-project")).is_err());assert_eq!(app.state.snapshot.root,root);assert!(app.state.error.is_some());assert_eq!(serde_json::to_value(&app.state.snapshot).unwrap(),before);assert_eq!(app.state.detail.as_ref().unwrap().text.as_deref(),Some("ANSWER"));
}
#[test] fn real_independent_binary_help_and_non_tty_boundary() {
    let bin=env!("CARGO_BIN_EXE_orch-tui");let help=Command::new(bin).arg("--help").output().unwrap();assert!(help.status.success());let t=String::from_utf8_lossy(&help.stdout);assert!(t.contains("--root"));assert!(t.contains("1/2/3"));let out=Command::new(bin).output().unwrap();assert!(!out.status.success());assert!(!out.stdout.windows(4).any(|w|w==b"\x1b[?1"));let bad=Command::new(bin).arg("--bad").output().unwrap();assert_eq!(bad.status.code(),Some(2));
}

struct Fixture{root:PathBuf}
impl Fixture{
 fn new()->Self{let root=orch_host::util::test_scratch_dir("b351-ui");fs::create_dir_all(root.join(".orch")).unwrap();fs::write(root.join(".gitignore"),".orch/\ncoordination/\n.cowork-temp/\n").unwrap();fs::write(root.join("question"),"Hello 中文").unwrap();let exe=root.join("provider");fs::write(&exe,"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ANSWER\"}'\n").unwrap();fs::set_permissions(&exe,fs::Permissions::from_mode(0o755)).unwrap();fs::write(root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  one:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",exe.display())).unwrap();for args in [vec!["init","-q"],vec!["add","question"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","commit","-qm","base"]]{assert!(Command::new("git").arg("-C").arg(&root).args(args).status().unwrap().success())}Self{root}}
 fn run(&self)->PathBuf{orch_host::consult::run_consultation(&self.root,&orch_host::consult::ConsultArgs{question:"question".into(),harnesses:vec!["one".into()],member_timeout_secs:Some(5),total_wall_secs:Some(10),..Default::default()}).unwrap().dir}
}
impl Drop for Fixture{fn drop(&mut self){if !std::thread::panicking(){let _=fs::remove_dir_all(&self.root);}}}

// Preserve raw-cell scans for secrets; only reconstruct display text for positive checks.
fn visible_text(b:&ratatui::buffer::Buffer)->String{
 use ratatui::buffer::CellWidth;
 if b.area.width==0{return String::new();}let mut out=String::new();
 for row in b.content.chunks(b.area.width as usize){let mut x=0;while x<row.len(){out.push_str(row[x].symbol());x+=usize::from(row[x].cell_width()).max(1);}out.push('\n');}out
}
