//! B362 frozen answer-reader state contract.
//!
//! The snapshot poll and detail reader have independent request ownership.
//! Background refresh preserves a still-verified answer, while selection or
//! verification changes settle the reader without stale text or endless loading.
use std::{path::Path, process::Command};

fn js(program: &str) {
    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web_assets");
    let source = format!(
        "import assert from 'node:assert/strict'; import {{pathToFileURL}} from 'node:url'; const m=await import(pathToFileURL(process.argv[1]+'/model.mjs')); {program}"
    );
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

fn initial() -> &'static str {
    r#"let s=m.initialState('server','project');s={...s,generation:1,snapshot:{rows:[{id:'answer',result:'verified'}],groups:[]},selected:'answer',answer:'verified body'};"#
}

#[test]
fn background_refresh_does_not_clear_a_verified_answer() {
    js(&(initial().to_owned()
        + r#"s=m.beginRefresh(s);assert.equal(s.answer,'verified body');assert.equal(s.selected,'answer');"#));
}

#[test]
fn snapshot_and_detail_requests_have_independent_tokens() {
    js(&(initial().to_owned()
        + r#"s=m.beginDetail(s,'answer');const detail=s.detailSeq;assert.equal(typeof detail,'number');s=m.beginRefresh(s);assert.equal(s.detailSeq,detail);assert.equal(typeof s.snapshotSeq,'number');"#));
}

#[test]
fn a_refresh_interleaving_does_not_discard_the_current_detail() {
    js(&(initial().to_owned()
        + r#"s=m.beginDetail(s,'answer');const detail=s.detailSeq;s=m.beginRefresh(s);s=m.applySnapshot(s,{serverInstanceId:'server',projectId:'project',snapshotGeneration:2,data:{rows:[{id:'answer',result:'verified'}],groups:[]}},s.epoch,s.snapshotSeq);s=m.applyDetail(s,{serverInstanceId:'server',projectId:'project',snapshotGeneration:3,data:{row:{id:'answer',result:'verified'},text:'new body'}},s.epoch,detail,'answer');assert.equal(s.answer,'new body');assert.equal(s.detailPending,false);"#));
}

#[test]
fn a_new_snapshot_preserves_only_a_still_verified_selection() {
    js(&(initial().to_owned()
        + r#"s=m.beginRefresh(s);s=m.applySnapshot(s,{serverInstanceId:'server',projectId:'project',snapshotGeneration:2,data:{rows:[{id:'answer',result:'verified'}],groups:[]}},s.epoch,s.snapshotSeq);assert.equal(s.answer,'verified body');s=m.beginRefresh(s);s=m.applySnapshot(s,{serverInstanceId:'server',projectId:'project',snapshotGeneration:3,data:{rows:[{id:'answer',result:'invalid'}],groups:[]}},s.epoch,s.snapshotSeq);assert.equal(s.answer,null);assert.equal(s.detailError,'answer_changed');"#));
}

#[test]
fn a_current_detail_error_always_settles_loading() {
    js(&(initial().to_owned()
        + r#"s=m.beginDetail(s,'answer');const detail=s.detailSeq;s=m.failDetail(s,'busy',s.epoch,detail,'answer');assert.equal(s.answer,null);assert.equal(s.detailPending,false);assert.equal(s.detailError,'busy');"#));
}

#[test]
fn changing_selection_invalidates_only_the_old_detail() {
    js(&(initial().to_owned()
        + r#"s=m.beginDetail(s,'answer');const detail=s.detailSeq;s=m.select(s,'other');const after=m.applyDetail(s,{serverInstanceId:'server',projectId:'project',snapshotGeneration:2,data:{row:{id:'answer',result:'verified'},text:'stale'}},s.epoch,detail,'answer');assert.equal(after.selected,'other');assert.equal(after.answer,null);assert.equal(after.detailPending,false);"#));
}
