//! Role editing invariants executed by the shipped browser model, with native IDs opaque.
use std::{path::Path, process::Command};
fn js(code: &str) {
    let src=format!("import assert from 'node:assert/strict';import {{pathToFileURL}} from 'node:url';const m=await import(pathToFileURL(process.argv[1]));{code}");
    let o = Command::new("node")
        .args(["--input-type=module", "--eval", &src])
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/web_assets/model.mjs"))
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
}
#[test]
fn deleting_roles_repairs_all_references_and_order_changes_are_immutable() {
    js(
        r#"const c={roles:[{id:'a'},{id:'b'},{id:'s'}],combinations:[{id:'pair',members:['a','b'],disabled:['a'],synthesizer:'a'}]};const moved=m.fusionMoveMember(c,'pair','b',-1);assert.deepEqual(moved.combinations[0].members,['b','a']);assert.deepEqual(c.combinations[0].members,['a','b']);assert.deepEqual(m.fusionMoveMember(c,'pair','a',-1),c);const next=m.fusionRemoveRole(c,'a');assert.deepEqual(next.roles.map(r=>r.id),['b','s']);assert.deepEqual(next.combinations[0],{id:'pair',members:['b'],disabled:[],synthesizer:null});"#,
    );
}
#[test]
fn readiness_respects_native_follow_fixed_opaque_ids_and_missing_managed_pins() {
    js(
        r#"const c={roles:['a','b','s'].map(id=>({id,name:id,harness:'installed:pi',instructions:'',fixed:{}})),combinations:[{id:'pair',members:['a','b'],disabled:[],synthesizer:'s'}]};const row={id:'installed:pi',driver:'pi',enabled:true,availability:'supported',native:{current:{provider:'future-provider',model:'future/model',effort:'future-effort'}}};assert.equal(m.fusionReady(c,'pair',[row]),null);assert.equal(m.fusionTuple(c.roles[0],row).model,'future/model');c.roles[0].fixed.model='unknown/next';assert.equal(m.fusionTuple(c.roles[0],row).model,'unknown/next');delete row.native.current.effort;assert.equal(m.fusionReady(c,'pair',[row]),'requiredNativePin');row.native.current.effort='off';c.combinations[0].disabled=['b'];assert.equal(m.fusionReady(c,'pair',[row]),'memberCount');c.roles[0].instructions='字'.repeat(6000);assert.equal(m.fusionDraftError(c),'roleIncomplete');"#,
    );
}

#[test]
fn fusion_documentation_explains_execution_and_recovery_boundaries() {
    let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
    for text in [
        "X-Orch-Config-Revision",
        "Zero verified answers skips synthesis",
        "There is no scheduler, automatic retry",
        "without its original engine is HOLD",
        "not an OS sandbox",
        "only the exact owned unchanged private settings file is removed",
    ] {
        assert!(guide.contains(text), "missing semantic guidance: {text}");
    }
}

#[test]
fn fusion_requests_reject_cross_project_and_restarted_server_responses() {
    js(r#"const fs=await import('node:fs');const text=fs.readFileSync(new URL('./app.mjs',pathToFileURL(process.argv[1])),'utf8');const source=text.slice(text.indexOf('async function fusionRequest('),text.indexOf('const fusionPost ='));assert(source.includes('async function fusionRequest'));const build=new Function('request',source+';return fusionRequest;');const f={server:'server-current',project:'project-current'};for(const envelope of [{serverInstanceId:'old',projectId:f.project,data:{wrong:true}},{serverInstanceId:f.server,projectId:'other',data:{wrong:true}}])await assert.rejects(build(async()=>envelope)(f,'config'),/stale_project/);assert.deepEqual(await build(async()=>({serverInstanceId:f.server,projectId:f.project,data:{revision:3}}))(f,'config'),{revision:3});"#);
}

#[test]
fn failed_run_revalidation_withholds_previous_answers_without_clobbering_new_selection() {
    js(r#"const fs=await import('node:fs');const text=fs.readFileSync(new URL('./app.mjs',pathToFileURL(process.argv[1])),'utf8');const source=text.slice(text.indexOf('async function fusionReadRun('),text.indexOf('async function fusionSave('));const build=new Function('fusionRequest','fusionCurrent','view','renderFusionResults','renderFusionControls',source+';return fusionReadRun;');const prior=()=>({phase:'consulting',members:[{answer:'old member',answerStatus:'verified'}],synthesis:{answer:'old synthesis',answerStatus:'verified'}});const f={runId:'a',runSeq:0,readRequest:false,run:prior()};await build(async()=>{throw Error('fusion_unavailable')},()=>false,'fusion',()=>{},()=>{})(f);assert.equal(f.run.members[0].answer,null);assert.equal(f.run.synthesis.answer,null);assert.equal(f.run.members[0].answerStatus,'invalid');assert.equal(f.runError,'fusion_unavailable');assert.equal(f.readRequest,false);let reject;const pending=build(()=>new Promise((_,r)=>{reject=r}),()=>false,'fusion',()=>{},()=>{})(f);f.runId='b';f.run=prior();reject(Error('old response'));await pending;assert.equal(f.run.members[0].answer,'old member');"#);
}
