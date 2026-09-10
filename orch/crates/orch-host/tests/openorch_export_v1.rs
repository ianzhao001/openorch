//! B338 immutable export boundary contract until TaskRecorded.
//! Mutations: export the private repository wholesale; include untracked files;
//! follow product symlinks; overwrite a nonempty destination; allow private refs.
use std::path::PathBuf;
use std::process::Command;
fn check(case: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let output = Command::new("python3").arg("-c").arg(contract()).arg(root).arg(case).output().unwrap();
    assert!(output.status.success(), "{case}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
#[test]
fn public_export_contains_only_tracked_product_files_and_public_docs() { check("allowlist"); }
#[test]
fn export_rejects_symlinks_dirty_product_files_and_foreign_destinations() { check("refuse"); }
fn contract() -> &'static str { r####"
import json,os,subprocess,sys,tempfile
from pathlib import Path
SOURCE=Path(sys.argv[1]);CASE=sys.argv[2];PARENT=SOURCE/'orch/target/test-tmp';PARENT.mkdir(parents=True,exist_ok=True)
def run(args,cwd=None,ok=True):
    env=dict(os.environ,GIT_AUTHOR_NAME='OpenOrch fixture',GIT_AUTHOR_EMAIL='fixture@example.invalid',GIT_COMMITTER_NAME='OpenOrch fixture',GIT_COMMITTER_EMAIL='fixture@example.invalid')
    p=subprocess.run([str(x) for x in args],cwd=cwd,capture_output=True,text=True,env=env)
    if ok:assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
    return p
with tempfile.TemporaryDirectory(prefix='b338-',dir=PARENT) as t:
    base=Path(t);repo=base/'source';repo.mkdir();run(['git','init','-q',repo])
    files={
      'orch/Cargo.toml':'[workspace]\nmembers=[]\n',
      'orch/Cargo.lock':'# fixture\n',
      'orch/crates/orch-cli/src/main.rs':'fn main() {}\n',
      'orch/crates/orch-host/src/lib.rs':'// product\n',
      '.githooks/reference-transaction':'#!/bin/sh\nexit 0\n',
      'coordination/scripts/wake-multica.sh':'#!/bin/sh\nexit 0\n',
      'plugins/openorch/.codex-plugin/plugin.json':json.dumps({'name':'openorch','version':'0.1.0-alpha.4'}),
      'plugins/openorch/scripts/openorch.py':'# public helper\n',
      'plugins/openorch/skills/openorch/SKILL.md':'---\nname: openorch\ndescription: explicit harness consultation\n---\n',
      'plugins/openorch/PUBLIC-README.md':'# OpenOrch\nPublic installation instructions.\n',
      'plugins/openorch/PUBLIC-CHANGELOG.md':'# Changelog\nPlugin alpha.4.\n',
      'plugins/openorch/LICENSE':'MIT fixture\n',
      'HARNESS-ROSTER.yaml':'PRIVATE_SENTINEL\n',
      '.orch/harnesses.yaml':'PRIVATE_SENTINEL\n',
      'coordination/rounds/r86/events.jsonl':'PRIVATE_SENTINEL\n',
      'coordination/consultations/private.md':'PRIVATE_SENTINEL\n',
      'README.md':'PRIVATE_SENTINEL\n',
      'HANDOFF.md':'PRIVATE_SENTINEL\n',
      'fixtures/private.txt':'PRIVATE_SENTINEL\n'}
    for name,data in files.items():p=repo/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(data)
    run(['git','add','-A'],cwd=repo);run(['git','commit','-qm','source fixture'],cwd=repo)
    extra=repo/'orch/untracked-private.txt';extra.write_text('PRIVATE_SENTINEL\n')
    def export(dest,ok=True):return run([sys.executable,SOURCE/'plugins/openorch/scripts/export.py','--source',repo,'--output',dest],ok=ok)
    out=base/'public output';export(out)
    assert (out/'orch/crates/orch-cli/src/main.rs').read_text()=='fn main() {}\n'
    assert (out/'plugins/openorch/scripts/openorch.py').is_file()
    assert (out/'README.md').read_text()==files['plugins/openorch/PUBLIC-README.md']
    assert (out/'CHANGELOG.md').read_text()==files['plugins/openorch/PUBLIC-CHANGELOG.md']
    assert (out/'LICENSE').read_text()==files['plugins/openorch/LICENSE']
    assert not (out/'.git').exists() and not (out/'.orch').exists()
    assert not (out/'HARNESS-ROSTER.yaml').exists() and not (out/'HANDOFF.md').exists()
    assert not (out/'coordination/rounds').exists() and not (out/'coordination/consultations').exists()
    assert not (out/'orch/untracked-private.txt').exists()
    for p in out.rglob('*'):
        if p.is_file():assert b'PRIVATE_SENTINEL' not in p.read_bytes(),str(p)
    if CASE=='refuse':
        sentinel=out/'foreign';sentinel.write_text('preserve')
        assert export(out,ok=False).returncode!=0 and sentinel.read_text()=='preserve'
        p=repo/'orch/crates/orch-host/src/lib.rs';p.write_text('dirty product\n')
        assert export(base/'dirty',ok=False).returncode!=0
        run(['git','add',str(p)],cwd=repo);run(['git','commit','-qm','fixture change'],cwd=repo)
        link=repo/'plugins/openorch/leak.txt';link.symlink_to(repo/'HANDOFF.md');run(['git','add',str(link)],cwd=repo);run(['git','commit','-qm','symlink fixture'],cwd=repo)
        assert export(base/'symlink',ok=False).returncode!=0
    elif CASE!='allowlist':raise AssertionError(CASE)
print('contract passed',CASE)
"#### }
