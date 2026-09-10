//! B337 immutable package/install behavior contract until TaskRecorded.
//! Mutations: omit/tamper a runtime resource; omit DSH bundle declaration;
//! resolve skills from source instead of installed package; overwrite foreign
//! installation; delete payload on one-host uninstall; reset user configuration.
use std::path::PathBuf;
use std::process::Command;

fn run_case(case: &str) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").canonicalize().unwrap();
    let output = Command::new("python3").arg("-c").arg(contract()).arg(root).arg(case).output().unwrap();
    assert!(output.status.success(), "{case}\n{}\n{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}
#[test]
fn packaged_resources_and_both_native_manifests_are_complete() { run_case("package"); }
#[test]
fn native_registration_is_idempotent_and_uninstall_preserves_other_host_payload() { run_case("install"); }
#[test]
fn installed_esm_provider_uses_its_own_skill_directory() { run_case("loader"); }

fn contract() -> &'static str { r####"
import hashlib,json,os,shutil,subprocess,sys,tempfile
from pathlib import Path
SOURCE=Path(sys.argv[1]);CASE=sys.argv[2]
PARENT=SOURCE/'orch/target/test-tmp';PARENT.mkdir(parents=True,exist_ok=True)
def run(args,ok=True):
    p=subprocess.run([str(x) for x in args],capture_output=True,text=True)
    if ok:assert p.returncode==0,(args,p.returncode,p.stdout,p.stderr)
    return p
HOST=r'''import json,sys
from pathlib import Path
base=Path(__file__).parent;file=base/'state.json';s=json.loads(file.read_text()) if file.exists() else {'marketplaces':[],'installed':[],'dsh':{}}
a=sys.argv[1:];kind=Path(__file__).name
with (base/'calls.jsonl').open('a') as f:f.write(json.dumps({'host':kind,'args':a})+'\n')
if kind=='codex':
    if a[:3]==['plugin','marketplace','list']:print(json.dumps({'marketplaces':s['marketplaces']}))
    elif a[:3]==['plugin','marketplace','add']:
        root=Path(a[3]);meta=json.loads((root/'.agents/plugins/marketplace.json').read_text());s['marketplaces']=[x for x in s['marketplaces'] if x['name']!=meta['name']]+[{'name':meta['name'],'root':str(root),'marketplaceSource':{'sourceType':'local','source':str(root)}}];print('{}')
    elif a[:3]==['plugin','marketplace','remove']:
        s['marketplaces']=[x for x in s['marketplaces'] if x['name']!=a[3]];print('{}')
    elif a[:2]==['plugin','list']:print(json.dumps({'installed':s['installed'],'available':[]}))
    elif a[:2]==['plugin','add']:
        name,market=a[2].split('@');m=next(x for x in s['marketplaces'] if x['name']==market);p=Path(m['root'])/'plugins'/name;v=json.loads((p/'.codex-plugin/plugin.json').read_text());s['installed']=[x for x in s['installed'] if x['pluginId']!=a[2]]+[{'pluginId':a[2],'name':name,'marketplaceName':market,'version':v['version'],'installed':True,'enabled':True,'source':{'source':'local','path':str(p)}}];print('{}')
    elif a[:2]==['plugin','remove']:s['installed']=[x for x in s['installed'] if x['pluginId']!=a[2]];print('{}')
    else:raise SystemExit('unexpected codex command '+str(a))
else:
    assert a[:3]==['plugin','--profile','web'],a
    profile=base/'web-profile';profile.mkdir(exist_ok=True)
    if a[3]=='add':
        p=Path(a[4]);v=json.loads((p/'package.json').read_text());s['dsh'][v['name']]={'from':v['name'],'version':v['version'],'path':str(p)};print('added')
    elif a[3] in ('remove','uninstall'):s['dsh'].pop(a[4],None);print('removed')
    elif a[3]=='list':print(json.dumps([{'name':'dsh-profile-web','path':str(profile),'private':True,'dependencies':s['dsh']}]))
    else:raise SystemExit('unexpected dsh command '+str(a))
    (profile/'package.json').write_text(json.dumps({'name':'dsh-profile-web','dependencies':{k:'file:'+v['path'] for k,v in s['dsh'].items()},'dsh':{'profile':{'bundles':['@deepseek-ai/dsh-base','@deepseek-ai/dsh-web-app',*s['dsh']]}}}))
file.write_text(json.dumps(s))
'''
with tempfile.TemporaryDirectory(prefix='b337-',dir=PARENT) as t:
    base=Path(t);runtime=base/'runtime input';runtime.mkdir()
    core=runtime/'orch';core.write_text('#!/bin/sh\nif [ "$1" = "--version" ]; then echo "orch 0.1.0"; else echo "orch guide: OK · commands=6 sections=10 invariants=4 wakeActions=4 dispositions=4"; fi\n');core.chmod(0o755)
    for name in ['wake-multica.sh','wake-dsh-stream.sh','wake-pi-stream.sh','wake-zcode-stream.sh']:
        p=runtime/'scripts'/name;p.parent.mkdir(exist_ok=True);p.write_text('#!/bin/sh\nexit 0\n');p.chmod(0o755)
    (runtime/'LICENSE').write_text('MIT fixture license\n')
    bundle=base/'bundle 包'
    run([sys.executable,SOURCE/'plugins/openorch/scripts/package.py','--runtime-dir',runtime,'--output',bundle,'--version','0.1.0-alpha.4'])
    plugin=bundle/'plugins/openorch'
    manifest=json.loads((plugin/'.codex-plugin/plugin.json').read_text())
    dsh=json.loads((plugin/'package.json').read_text())
    assert manifest['name']=='openorch' and manifest['version']=='0.1.0-alpha.4'
    assert dsh['name']=='openorch' and dsh['version']==manifest['version']
    assert (plugin/dsh['dsh']['bundle']['patch']).is_file()
    skill=plugin/'skills/openorch/SKILL.md';assert skill.is_file()
    inventory=json.loads((plugin/'runtime/manifest.json').read_text())
    expected={'orch','scripts/wake-multica.sh','scripts/wake-dsh-stream.sh','scripts/wake-pi-stream.sh','scripts/wake-zcode-stream.sh'}
    assert set(inventory['files'])==expected
    for name,item in inventory['files'].items():
        data=(plugin/'runtime'/name).read_bytes();assert item=={'sha256':hashlib.sha256(data).hexdigest(),'bytes':len(data)}
    catalog=json.loads((bundle/'.agents/plugins/marketplace.json').read_text())
    assert catalog['name']=='openorch' and len(catalog['plugins'])==1
    assert catalog['plugins'][0]['name']=='openorch' and catalog['plugins'][0]['source']['path']=='./plugins/openorch'
    assert catalog['plugins'][0]['policy']=={'installation':'AVAILABLE','authentication':'ON_INSTALL'}

    if CASE=='package':
        bad=base/'bad-runtime';shutil.copytree(runtime,bad);(bad/'scripts/wake-pi-stream.sh').unlink()
        assert run([sys.executable,SOURCE/'plugins/openorch/scripts/package.py','--runtime-dir',bad,'--output',base/'bad-bundle','--version','0.1.0-alpha.4'],ok=False).returncode!=0
        linked=base/'linked-runtime';shutil.copytree(runtime,linked);p=linked/'scripts/wake-dsh-stream.sh';p.unlink();p.symlink_to(runtime/'scripts/wake-dsh-stream.sh')
        assert run([sys.executable,SOURCE/'plugins/openorch/scripts/package.py','--runtime-dir',linked,'--output',base/'linked-bundle','--version','0.1.0-alpha.4'],ok=False).returncode!=0
        assert (bundle/'install.py').is_file()
    elif CASE=='install':
        bins=base/'native bins';bins.mkdir()
        for name in ['codex','dsh']:
            p=bins/name;p.write_text('#!'+sys.executable+'\n'+HOST);p.chmod(0o755)
        prefix=base/'install 前缀';prefix.mkdir();sentinel=prefix/'personal-sentinel';sentinel.write_text('keep personal choices')
        args=[sys.executable,bundle/'install.py','--bundle',bundle,'--prefix',prefix,'--host','both','--codex-bin',bins/'codex','--dsh-bin',bins/'dsh']
        run(args);run(args)
        state=json.loads((bins/'state.json').read_text())
        assert len([x for x in state['installed'] if x['pluginId']=='openorch@openorch'])==1
        assert 'openorch' in state['dsh']
        used=Path(state['dsh']['openorch']['path']);assert used.is_relative_to(prefix) and (used/'runtime/orch').is_file()
        run([sys.executable,bundle/'install.py','--bundle',bundle,'--prefix',prefix,'--host','codex','--codex-bin',bins/'codex','--uninstall'])
        state=json.loads((bins/'state.json').read_text());assert not state['installed'] and 'openorch' in state['dsh']
        assert (used/'runtime/orch').is_file() and sentinel.read_text()=='keep personal choices'
        good_state=(bins/'state.json').read_bytes()
        foreign=base/'foreign';foreign.mkdir()
        state['marketplaces']=[{'name':'openorch','root':str(foreign)}];(bins/'state.json').write_text(json.dumps(state))
        before=(bins/'state.json').read_bytes()
        assert run(args,ok=False).returncode!=0
        assert (bins/'state.json').read_bytes()==before
        (bins/'state.json').write_bytes(good_state)
        damaged=used/'runtime/scripts/wake-dsh-stream.sh';damaged.write_text('tampered')
        assert run(args,ok=False).returncode!=0
    elif CASE=='loader':
        fixture=plugin/'node_modules/@deepseek-ai/dsh-skill-filesystem';fixture.mkdir(parents=True)
        (fixture/'package.json').write_text(json.dumps({'name':'@deepseek-ai/dsh-skill-filesystem','type':'module','exports':'./index.mjs'}))
        (fixture/'index.mjs').write_text('export function apply(ctx, config) { ctx.captured = config; }\n')
        script=base/'check-loader.mjs'
        script.write_text("import fs from 'node:fs';import {pathToFileURL} from 'node:url';const p=process.argv[2];const m=await import(pathToFileURL(p+'/dsh-entry.mjs'));const ctx={plugin(plugin,config){plugin.apply(this,config)}};await m.apply(ctx);const c=ctx.captured;if(!c || c.includeDefaultRoots!==false || c.providerName!=='openorch')throw new Error('provider isolation');if(c.customSkillDirs.length!==1 || c.customSkillDirs[0]!==p+'/skills' || !fs.existsSync(c.customSkillDirs[0]+'/openorch/SKILL.md'))throw new Error('installed resource location');console.log('installed provider path verified');")
        run(['node',script,plugin])
    else:raise AssertionError(CASE)
print('contract passed',CASE)
"#### }
