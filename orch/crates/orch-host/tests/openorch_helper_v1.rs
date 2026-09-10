//! B336 behavioral contract. Keep source bytes unchanged until TaskRecorded.
//! Negative mutations: overwrite existing config; drop explicit members; invoke
//! a shell for question text; accept an unsupported default; follow a config
//! symlink; skip runtime inventory verification. Every mutation must be caught.

use std::path::PathBuf;
use std::process::Command;

fn run_contract(case: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = Command::new("python3")
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

FAKE_CORE=r'''import json,os,subprocess,sys
from pathlib import Path
args=sys.argv[1:]
package=Path(__file__).resolve().parents[1]
root=Path(args[args.index('--root')+1]) if '--root' in args else Path.cwd()
tail=args[:]
if '--root' in tail:
    i=tail.index('--root'); del tail[i:i+2]
record={'args':args,'cwd':os.getcwd()}
if tail and tail[0]=='consult':
    record['questionPath']=tail[1]
    record['question']=Path(tail[1]).read_text()
with (package/'calls.jsonl').open('a') as log:log.write(json.dumps(record)+'\n')
if tail==['guide','--check']:
    print('orch guide: OK · commands=6 sections=10 invariants=4 wakeActions=4 dispositions=4');sys.exit(0)
if tail==['--version']:
    print('orch 0.1.0');sys.exit(0)
if tail and tail[0]=='consult':
    print('orch consult · id=01R86FIXTURE000000000000000 fusion=2/2')
    print('summary='+str(root/'coordination/consultations/01R86FIXTURE000000000000000/summary.md'));sys.exit(0)
common=subprocess.check_output(['git','rev-parse','--git-common-dir'],cwd=root,text=True).strip()
common=Path(common) if Path(common).is_absolute() else root/common
config=common.resolve().parent/'.orch/harnesses.yaml'
try:
    obj=json.loads(config.read_text())
    assert type(obj.get('version')) is int and obj['version']==1
    assert set(obj)=={'version','harnesses'}
    for name,row in obj['harnesses'].items():
        assert set(row)<= {'driver','executable','enabled','cwdPolicy','defaults','execute','review','consult'}
        assert Path(row['executable']).is_absolute() and Path(row['executable']).is_file()
except Exception as error:
    print('invalid native configuration: '+str(error),file=sys.stderr);sys.exit(2)
if tail==['harness','lint']:
    print('orch harness lint · ok');sys.exit(0)
if tail[:2]==['harness','list']:
    print('alias\tdriver\tstatus\treason')
    for name,row in obj['harnesses'].items():
        supported=row['driver'] in ('codex','opencode','smartclaw','claude','cursor','mimo','codebuddy')
        print(name+'\t'+row['driver']+'\t'+('supported' if supported else 'unsupported')+'\t'+('—' if supported else 'driver does not support consult'))
    sys.exit(0)
if tail==['doctor']:
    print('orch doctor: standalone configuration checked');sys.exit(0)
print('unexpected core arguments',args,file=sys.stderr);sys.exit(2)
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
    runtime=package/'runtime';runtime.mkdir()
    core=runtime/'orch';core.write_text('#!'+sys.executable+'\n'+FAKE_CORE);core.chmod(0o755)
    inventory={}
    assets=['orch','scripts/wake-multica.sh','scripts/wake-dsh-stream.sh','scripts/wake-pi-stream.sh','scripts/wake-zcode-stream.sh']
    for name in assets:
        path=runtime/name
        if name!='orch':path.parent.mkdir(exist_ok=True);path.write_text('#!/bin/sh\nexit 0\n');path.chmod(0o755)
        data=path.read_bytes();inventory[name]={'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)}
    (runtime/'manifest.json').write_text(json.dumps({'version':1,'files':inventory}))
    config_dir=base/'个人 设置'
    a=project(base/'项目 甲')
    b=project(base/'项目 乙')
    executable=base/'native tool';executable.write_text('#!/bin/sh\nexit 0\n');executable.chmod(0o755)
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
        helper_call('run','--project',a,'--mode','single','--question-file',question)
        helper_call('run','--project',a,'--mode','fusion','--question-file',question)
        helper_call('run','--project',a,'--mode','single','--harness','beta','--question-file',question)
        calls=[json.loads(x) for x in (package/'calls.jsonl').read_text().splitlines()]
        calls=[x for x in calls if 'consult' in x['args']]
        assert len(calls)==3,calls
        def members(call):return [call['args'][i+1] for i,v in enumerate(call['args']) if v=='--harness']
        assert [members(x) for x in calls]==[['alpha'],['alpha','beta'],['beta']]
        for call in calls:
            assert call['args'][call['args'].index('--root')+1]==str(a)
            assert call['cwd']==str(a)
            assert call['question']==text
            assert Path(call['questionPath']).is_relative_to(a)
        assert not marker.exists()
        assert json.loads(saved.read_text())['defaults']==profile['defaults']

    elif CASE=='reject':
        before=saved.read_bytes()
        bad=json.loads(input_file.read_text())
        bad['harnesses']['blocked']={'driver':'dsh','executable':str(executable),'enabled':True,'cwdPolicy':'project-root'}
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
        wrapper=runtime/'scripts/wake-dsh-stream.sh';wrapper.write_text('tampered')
        assert helper_call('run','--project',a,'--mode','single','--question-file',question,ok=False).returncode!=0
        assert saved.read_bytes()==before
    else:raise AssertionError(CASE)
print('contract passed',CASE)
"####
}
