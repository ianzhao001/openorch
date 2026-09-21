//! Actual Rust CLI compatibility entry; Python packaging is only a verified launcher.
use std::process::Command;
#[test]
fn rust_helper_namespace_is_hidden_but_available() {
    let binary=env!("CARGO_BIN_EXE_orch");
    let public=Command::new(binary).arg("--help").output().unwrap();
    assert!(public.status.success());assert!(!String::from_utf8_lossy(&public.stdout).contains("__openorch"));
    let internal=Command::new(binary).args(["__openorch","--help"]).output().unwrap();
    assert!(internal.status.success(),"{}",String::from_utf8_lossy(&internal.stderr));
    let help=String::from_utf8(internal.stdout).unwrap();for action in ["configure","attach","discover","doctor","run"] {assert!(help.contains(action));}
}

mod legacy_compatibility {
//! B336 behavioral contract. Keep source bytes unchanged until TaskRecorded.
//! Negative mutations: overwrite existing config; drop explicit members; invoke
//! a shell for question text; accept an unsupported default; follow a config
//! symlink; skip runtime inventory verification. Every mutation must be caught.

use std::path::PathBuf;
use std::process::Command;

fn run_contract(case: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = Command::new("python3")
        .env("OPENORCH_TEST_CORE", env!("CARGO_BIN_EXE_orch"))
        .arg("-c")
        .arg(python_contract())
        .arg(root.canonicalize().unwrap())
        .arg(case)
        .output()
        .expect("Python 3 contract runner");
    assert!(output.status.success(), "case={case}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

#[test]
fn profile_reuse_preserves_existing_projects_and_local_git_state() { run_contract("profile"); }

#[test]
fn invocation_uses_explicit_members_literal_text_and_actual_project() { run_contract("invoke"); }

#[test]
fn unsafe_config_defaults_and_modified_runtime_fail_without_overwrite() { run_contract("reject"); }

fn python_contract() -> &'static str {
    r####"
import hashlib,json,os,shutil,stat,subprocess,sys,tempfile
from pathlib import Path

SOURCE=Path(sys.argv[1])
CASE=sys.argv[2]
PARENT=SOURCE/'orch/target/test-tmp'
PARENT.mkdir(parents=True,exist_ok=True)

# Packaging facade only: the all-feature test binary may expose selfhost leaves.
# Every semantic operation executes the real Cargo-built Rust CLI unchanged.
TEST_CORE_FACADE=r'''import os,sys
args=sys.argv[1:]
if args==['guide','--check']:
    print('orch guide: OK · commands=6');raise SystemExit(0)
if args==['--version']:
    print('orch 0.1.0');raise SystemExit(0)
core=os.environ['OPENORCH_TEST_CORE']
os.execv(core,[core,*args])
'''

def command(args,cwd=None,ok=True,env=None):
    p=subprocess.run([str(x) for x in args],cwd=cwd,capture_output=True,text=True,env=env)
    if ok:assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
    return p

def project(path):
    path.mkdir(parents=True)
    command(['git','init','-q',path])
    env=dict(os.environ,GIT_AUTHOR_NAME='OpenOrch fixture',GIT_AUTHOR_EMAIL='fixture@example.invalid',GIT_COMMITTER_NAME='OpenOrch fixture',GIT_COMMITTER_EMAIL='fixture@example.invalid')
    command(['git','commit','--allow-empty','-qm','fixture initial commit'],cwd=path,env=env)
    return path

with tempfile.TemporaryDirectory(prefix='b336-',dir=PARENT) as temporary:
    base=Path(temporary)
    package=base/'plugin 包'
    (package/'scripts').mkdir(parents=True)
    helper=package/'scripts/openorch.py'
    shutil.copyfile(SOURCE/'plugins/openorch/scripts/openorch.py',helper)
    shutil.copyfile(SOURCE/'plugins/openorch/scripts/install.py',package/'scripts/install.py')
    runtime=package/'runtime';runtime.mkdir()
    core=runtime/'orch';core.write_text('#!'+sys.executable+'\n'+TEST_CORE_FACADE);core.chmod(0o755)
    inventory={}
    assets=['orch','orch-mcp','orch-acp','scripts/wake-multica.sh','scripts/wake-dsh-stream.sh','scripts/wake-pi-stream.sh','scripts/wake-zcode-stream.sh']
    for name in assets:
        path=runtime/name
        if name!='orch':
            path.parent.mkdir(exist_ok=True);path.write_text('#!/bin/sh\necho \"'+name+' 0.1.0\"\n');path.chmod(0o755)
        data=path.read_bytes();inventory[name]={'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)}
    (runtime/'manifest.json').write_text(json.dumps({'version':1,'files':inventory}))
    config_dir=base/'个人 设置'
    a=project(base/'项目 甲')
    b=project(base/'项目 乙')
    executable=base/'native tool'
    executable.write_text('#!'+sys.executable+'\n'+r'''import sys,json
if 'exec' in sys.argv:
    frames=[{'type':'thread.started','thread_id':'fixture'},{'type':'item.completed','item':{'type':'agent_message','text':'verified fixture answer'}},{'type':'turn.completed','usage':{'input_tokens':1,'output_tokens':1}}]
else:
    frames=[{'type':'step_start','sessionID':'fixture','part':{'type':'step-start','modelID':'fixture-model'}},{'type':'text','part':{'text':'verified fixture answer'}},{'type':'step_finish','part':{'type':'step-finish','reason':'stop'}}]
for frame in frames:print(json.dumps(frame),flush=True)
''');executable.chmod(0o755)
    rows={name:{'driver':driver,'executable':str(executable),'enabled':True,'cwdPolicy':'project-root'} for name,driver in [('alpha','codex'),('beta','opencode')]}
    profile={'version':1,'harnesses':rows,'defaults':{'single':'alpha','fusion':['alpha','beta']}}
    input_file=base/'选择.json';input_file.write_text(json.dumps(profile))
    def helper_call(*args,ok=True):return command([sys.executable,helper,'--config-dir',config_dir,*args],ok=ok)
    helper_call('configure','--project',a,'--input',input_file)
    saved=config_dir/'profile.json'
    assert saved.is_file()
    assert stat.S_IMODE(saved.stat().st_mode)==0o600
    assert json.loads(saved.read_text())['defaults']==profile['defaults']
    helper_call('attach','--project',a)
    target=a/'.orch/harnesses.yaml'
    assert json.loads(target.read_text())=={'version':1,'harnesses':rows}

    if CASE=='profile':
        command(['git','check-ignore','-q','.orch/harnesses.yaml'],cwd=a)
        assert command(['git','status','--porcelain','--untracked-files=no'],cwd=a).stdout==''
        assert not (a/'.gitignore').exists()
        before=target.read_bytes()
        helper_call('attach','--project',a)
        assert target.read_bytes()==before
        linked=base/'linked 作业'
        command(['git','worktree','add','--detach',linked,'HEAD'],cwd=a)
        helper_call('attach','--project',linked)
        assert not (linked/'.orch/harnesses.yaml').exists()
        assert target.read_bytes()==before
        helper_call('attach','--project',b)
        assert (b/'.orch/harnesses.yaml').read_bytes()==before
        custom=json.dumps({'version':1,'harnesses':{'custom':rows['alpha']}},separators=(',',':')).encode()+b'\n  '
        (b/'.orch/harnesses.yaml').write_bytes(custom)
        helper_call('attach','--project',b)
        assert (b/'.orch/harnesses.yaml').read_bytes()==custom
        assert command(['git','status','--porcelain','--untracked-files=no'],cwd=b).stdout==''
        c=project(base/'并发 项目')
        argv=[sys.executable,str(helper),'--config-dir',str(config_dir),'attach','--project',str(c)]
        processes=[subprocess.Popen(argv,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True) for _ in range(2)]
        for process in processes:
            output,error=process.communicate();assert process.returncode==0,(output,error)
        assert json.loads((c/'.orch/harnesses.yaml').read_text())=={'version':1,'harnesses':rows}

    elif CASE=='invoke':
        question=base/'问题 引号.txt'
        marker=base/'must-not-exist'
        text='请保留字面值 $(touch '+str(marker)+'); "quoted" & `not-a-command`\n第二行'
        question.write_text(text)
        outputs=[helper_call('run','--project',a,'--mode','single','--question-file',question),
                 helper_call('run','--project',a,'--mode','fusion','--question-file',question),
                 helper_call('run','--project',a,'--mode','single','--harness','beta','--question-file',question)]
        import re
        ids=[re.search(r'id=([A-Z0-9]+)',x.stdout).group(1) for x in outputs]
        calls=[json.loads((a/'.orch/fusion-runs'/identity/'request.json').read_text()) for identity in ids]
        assert [x['legacy']['aliases'] for x in calls]==[['alpha'],['alpha','beta'],['beta']]
        for identity,call in zip(ids,calls):
            assert call['project']==str(a)
            assert call['request']['question']==text
            assert Path(call['legacy']['questionSource']).is_relative_to(a)
            state=json.loads((a/'.orch/fusion-runs'/identity/'state.json').read_text())
            assert state['phase']=='completed',state
            assert all(member['status']=='verified' for member in state['members'])
            assert (a/'coordination/consultations'/identity/'summary.md').is_file()
        assert not marker.exists()
        assert json.loads(saved.read_text())['defaults']==profile['defaults']

    elif CASE=='reject':
        before=saved.read_bytes()
        bad=json.loads(input_file.read_text())
        bad['harnesses']['blocked']={'driver':'agy','executable':str(executable),'enabled':True,'cwdPolicy':'project-root'}
        bad['defaults']['fusion']=['alpha','blocked'];input_file.write_text(json.dumps(bad))
        assert helper_call('configure','--project',a,'--input',input_file,'--replace-profile',ok=False).returncode!=0
        assert saved.read_bytes()==before
        target.unlink();outside=base/'outside.yaml';outside.write_text('unchanged')
        target.symlink_to(outside)
        assert helper_call('attach','--project',a,ok=False).returncode!=0
        assert outside.read_text()=='unchanged'
        target.unlink();helper_call('attach','--project',a)
        original_dir=a/'.orch';backup_dir=a/'.orch-kept'
        original_dir.rename(backup_dir)
        outside_dir=base/'outside directory';outside_dir.mkdir();(outside_dir/'harnesses.yaml').write_text('directory sentinel')
        original_dir.symlink_to(outside_dir,target_is_directory=True)
        assert helper_call('attach','--project',a,ok=False).returncode!=0
        assert (outside_dir/'harnesses.yaml').read_text()=='directory sentinel'
        original_dir.unlink();backup_dir.rename(original_dir)
        question=base/'q.txt';question.write_text('read only')
        assert helper_call('run','--project',a,'--mode','fusion','--harness','alpha','--harness','alpha','--question-file',question,ok=False).returncode!=0
        for protocol in ['orch-mcp','orch-acp']:
            path=runtime/protocol;original=path.read_bytes();path.write_bytes(original+b'\n# changed bytes, unchanged version output\n')
            assert helper_call('run','--project',a,'--mode','single','--question-file',question,ok=False).returncode!=0
            path.write_bytes(original)
        wrapper=runtime/'scripts/wake-dsh-stream.sh';wrapper.write_text('tampered')
        assert helper_call('run','--project',a,'--mode','single','--question-file',question,ok=False).returncode!=0
        assert saved.read_bytes()==before
    else:raise AssertionError(CASE)
print('contract passed',CASE)
"####
}

}

#[test]
fn python_helper_has_no_profile_interpreter_or_validation_clone() {
    let source=std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../plugins/openorch/scripts/openorch.py");
    let text=std::fs::read_to_string(source).unwrap();
    for retired in ["def profile_shape(","def validate_in_clone(","def configure(","def attach(","def run_invocation("] {assert!(!text.contains(retired),"duplicate helper semantics: {retired}");}
    assert!(text.contains("__openorch"));
}

#[cfg(not(feature="selfhost"))]
#[test]
fn default_helper_cannot_bypass_selfhost_markers_from_a_nested_directory() {
    let root=orch_host::util::test_scratch_dir("default helper gate");
    for args in [vec!["init","-q"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","base"]] {
        assert!(Command::new("git").args(args).current_dir(&root).status().unwrap().success());
    }
    std::fs::create_dir(root.join("coordination")).unwrap();std::fs::write(root.join("coordination/PROJECT-BINDING.yaml"),"malformed: [").unwrap();
    let nested=root.join("nested");std::fs::create_dir(&nested).unwrap();let personal=root.join("personal");
    let output=Command::new(env!("CARGO_BIN_EXE_orch")).arg("--root").arg(nested).args(["__openorch","--config-dir"]).arg(&personal).arg("attach").output().unwrap();
    assert_eq!(output.status.code(),Some(2));assert!(String::from_utf8_lossy(&output.stderr).contains("project contains selfhost state"));assert!(!personal.exists());
}
#[test]
fn helper_rejects_relative_personal_directory_before_discovery() {
    let root=orch_host::util::test_scratch_dir("helper relative config");
    for args in [vec!["init","-q"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","base"]] {
        assert!(Command::new("git").args(args).current_dir(&root).status().unwrap().success());
    }
    let output=Command::new(env!("CARGO_BIN_EXE_orch")).arg("--root").arg(&root).args(["__openorch","--config-dir","relative","discover"]).output().unwrap();
    assert_eq!(output.status.code(),Some(2));assert!(String::from_utf8_lossy(&output.stderr).contains("absolute_normalized_path_required"));assert!(!root.join("relative").exists());
}
