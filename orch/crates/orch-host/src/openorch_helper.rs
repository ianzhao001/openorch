//! Personal setup and compatibility helpers over the existing Rust configuration core.
//! No provider is started here. Consultation preparation returns ordinary ConsultArgs.
#![deny(missing_docs)]
use anyhow::{bail,Context,Result};
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use std::{collections::BTreeSet,fs::{self,File,OpenOptions},io::{Read,Write},os::unix::fs::{DirBuilderExt,MetadataExt,OpenOptionsExt},path::{Component,Path,PathBuf},process::Command};
use crate::harness_config::{self,HarnessAction,HarnessConfigSnapshot};
const LIMIT:usize=2*1024*1024;
const EXCLUDES:[&str;3]=["/.orch/","/coordination/consultations/","/.cowork-temp/channel-capture/"];
#[cfg(target_os="linux")]
const SAFE_OPEN:i32=0x20000|0x800;
#[cfg(not(target_os="linux"))]
const SAFE_OPEN:i32=0x100|0x4;
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {single:String,fusion:Vec<String>}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct Profile {version:u32,harnesses:Value,defaults:Defaults}
#[derive(Serialize,Deserialize,Default)]
#[serde(deny_unknown_fields)]
struct Registry {version:u32,projects:Vec<RegisteredProject>}
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisteredProject {path:PathBuf,device:u64,inode:u64}
fn safe_path(path:&Path,directory:bool)->Result<()> {
    if !path.is_absolute() || path.components().any(|c|matches!(c,Component::ParentDir|Component::CurDir)) {bail!("absolute_normalized_path_required");}
    let mut prefix=PathBuf::new();
    for part in path.components() {
        prefix.push(part.as_os_str());
        match fs::symlink_metadata(&prefix) {
            Ok(m)=>{if m.file_type().is_symlink() || if prefix!=path||directory {!m.is_dir()} else {!m.is_file()} {bail!("unsafe_helper_path: {}",prefix.display());}},
            Err(e) if e.kind()==std::io::ErrorKind::NotFound=>{},Err(e)=>return Err(e.into()),
        }
    }
    Ok(())
}
fn make_dir(path:&Path)->Result<()> {safe_path(path,true)?;fs::DirBuilder::new().recursive(true).mode(0o700).create(path)?;safe_path(path,true)}
fn same_file(a:&fs::Metadata,b:&fs::Metadata)->bool {
    a.dev()==b.dev()&&a.ino()==b.ino()&&a.len()==b.len()&&a.mtime()==b.mtime()&&a.mtime_nsec()==b.mtime_nsec()&&a.ctime()==b.ctime()&&a.ctime_nsec()==b.ctime_nsec()
}
fn read_regular(path:&Path)->Result<Vec<u8>> {
    safe_path(path,false)?;let before=fs::symlink_metadata(path)?;
    if !before.is_file()||before.len()>LIMIT as u64 {bail!("invalid_or_oversized_helper_file");}
    let mut file=OpenOptions::new().read(true).custom_flags(SAFE_OPEN).open(path)?;let opened=file.metadata()?;
    if !same_file(&before,&opened){bail!("helper_file_changed");}
    let mut bytes=Vec::new();(&mut file).take(LIMIT as u64+1).read_to_end(&mut bytes)?;
    let after=fs::symlink_metadata(path)?;
    if bytes.len()>LIMIT||bytes.len() as u64!=opened.len()||!same_file(&opened,&file.metadata()?)||!same_file(&opened,&after)||after.file_type().is_symlink(){bail!("helper_file_changed_or_oversized");}
    Ok(bytes)
}
fn encoded(value:&impl Serialize)->Result<Vec<u8>> {let mut b=serde_json::to_vec_pretty(value)?;b.push(b'\n');if b.len()>LIMIT{bail!("helper_file_too_large");}Ok(b)}
fn atomic_bytes(path:&Path,bytes:&[u8],replace:bool)->Result<()> {
    if bytes.len()>LIMIT{bail!("helper_file_too_large");}
    let parent=path.parent().context("missing_parent")?;make_dir(parent)?;safe_path(path,false)?;
    let temporary=parent.join(format!(".openorch-{}.tmp",ulid::Ulid::new()));
    let mut file=OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(SAFE_OPEN).open(&temporary)?;
    let result=(||->Result<()> {file.write_all(bytes)?;file.sync_all()?;safe_path(path,false)?;
        if replace {fs::rename(&temporary,path)?;} else {fs::hard_link(&temporary,path)?;}
        File::open(parent)?.sync_all()?;Ok(())})();
    drop(file);if temporary.exists(){fs::remove_file(&temporary)?;}result
}
#[cfg(test)]
static PROFILE_LATE_EDITS:std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<PathBuf,Vec<u8>>>>=std::sync::OnceLock::new();
#[cfg(test)]
fn inject_profile_edit(path:&Path)->Result<()> {
    let bytes=PROFILE_LATE_EDITS.get_or_init(Default::default).lock().unwrap().remove(path);
    if let Some(bytes)=bytes{fs::write(path,bytes)?;}Ok(())
}
#[cfg(test)]
static PROFILE_POST_EDITS:std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<PathBuf,Vec<u8>>>>=std::sync::OnceLock::new();
struct ExpectedProfile { metadata:fs::Metadata, bytes:Vec<u8> }
fn exchange_files(left:&Path,right:&Path)->Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let left=std::ffi::CString::new(left.as_os_str().as_bytes())?;
    let right=std::ffi::CString::new(right.as_os_str().as_bytes())?;
    #[cfg(target_os="macos")]
    let code={
        unsafe extern "C" {fn renamex_np(from:*const std::os::raw::c_char,to:*const std::os::raw::c_char,flags:u32)->i32;}
        // Darwin sys/stdio.h: RENAME_SWAP exchanges both names atomically.
        unsafe {renamex_np(left.as_ptr(),right.as_ptr(),2)}
    };
    #[cfg(target_os="linux")]
    let code={
        unsafe extern "C" {fn renameat2(from_dir:i32,from:*const std::os::raw::c_char,to_dir:i32,to:*const std::os::raw::c_char,flags:u32)->i32;}
        unsafe {renameat2(-100,left.as_ptr(),-100,right.as_ptr(),2)}
    };
    #[cfg(any(target_os="macos",target_os="linux"))]
    {if code==0 {Ok(())} else {Err(std::io::Error::last_os_error().into())}}
    #[cfg(not(any(target_os="macos",target_os="linux")))]
    {let _=(left,right);bail!("atomic_profile_exchange_unavailable");}
}
fn publish_profile(path:&Path,bytes:&[u8],expected:Option<&ExpectedProfile>)->Result<Option<PathBuf>> {
    let Some(old)=expected else {atomic_bytes(path,bytes,false)?;return Ok(None);};
    let parent=path.parent().context("missing_profile_parent")?;
    let backups=parent.join("backups");make_dir(&backups)?;
    let retained=backups.join(format!(".profile-exchange-{}.json",ulid::Ulid::new()));
    // Before exchange this is a prepared candidate, afterwards the actual old
    // inode. Never unlink it after exchange, including on sync/readback errors.
    atomic_bytes(&retained,bytes,false).context("profile candidate preparation failed; original was not exchanged")?;
    let mut exchanged=false;
    let result=(||->Result<Option<PathBuf>> {
        safe_path(path,false)?;
        if !same_file(&old.metadata,&fs::symlink_metadata(path)?) || read_regular(path)?!=old.bytes {bail!("profile_changed_since_backup");}
        #[cfg(test)]
        inject_profile_edit(path)?;
        exchange_files(&retained,path)?;exchanged=true;
        #[cfg(test)]
        if let Some(bytes)=PROFILE_POST_EDITS.get_or_init(Default::default).lock().unwrap().remove(path) {fs::write(path,bytes)?;}

        File::open(parent)?.sync_all()?;File::open(&backups)?.sync_all()?;
        if read_regular(&retained)?!=old.bytes {bail!("concurrent_profile_edit_during_exchange; the requested profile was published but the actual overwritten bytes are retained");}
        if read_regular(path)?!=bytes {bail!("profile_changed_after_exchange; current user bytes were retained without rollback");}
        Ok(Some(retained.clone()))
    })();
    result.with_context(||if exchanged {format!("Profile replacement not confirmed; actual overwritten file retained at {}",retained.display())}
        else {format!("Profile was not exchanged; prepared candidate retained at {}",retained.display())})
}
fn with_lock<T>(path:&Path,body:impl FnOnce()->Result<T>)->Result<T> {
    make_dir(path.parent().context("missing_lock_parent")?)?;safe_path(path,false)?;
    let file=OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).custom_flags(SAFE_OPEN).open(path)?;
    if !file.metadata()?.is_file(){bail!("invalid_helper_lock");}
    let mut lock=fd_lock::RwLock::new(file);let _guard=lock.write()?;body()
}
fn git(root:&Path,args:&[&str])->Result<String> {
    let output=Command::new("git").args(["-c","core.fsmonitor=false"]).args(args).current_dir(root).output()?;
    if !output.status.success(){bail!("project_git_inspection_failed");}
    Ok(String::from_utf8(output.stdout)?.trim_end_matches(['\r','\n']).to_string())
}
/// Resolve the actual non-bare Git worktree and require its existing commit.
/// This is read-only and may run before ordinary CLI project-mode/staleness guards.
pub fn canonical_project(root:&Path)->Result<PathBuf> {
    crate::gitx::canonical_committed_worktree(root)
}
/// Resolve only the local startup selector; no MCP request can change this directory.
pub fn default_config_dir()->Result<PathBuf> {
    let dir=if let Some(value)=std::env::var_os("OPENORCH_CONFIG_DIR") {PathBuf::from(value)}
        else {PathBuf::from(std::env::var_os("HOME").context("HOME_unavailable")?).join(".config/openorch")};
    safe_path(&dir,true)?;Ok(dir)
}
/// Validate an explicit personal directory or resolve the one shared startup default.
pub fn resolve_config_dir(explicit:Option<PathBuf>)->Result<PathBuf> {
    match explicit {Some(path)=>{safe_path(&path,true)?;Ok(path)},None=>default_config_dir()}
}
fn profile_shape(bytes:&[u8])->Result<Profile> {
    let text=std::str::from_utf8(bytes).context("profile_not_utf8")?;
    if text.lines().any(crate::redact::has_secret){bail!("profile_contains_secret_use_credential_environment_reference");}
    let profile:Profile=serde_json::from_slice(bytes).context("invalid_personal_profile")?;
    let rows=profile.harnesses.as_object().context("harnesses_must_be_object")?;
    if profile.version!=1||rows.is_empty(){bail!("invalid_personal_profile_version_or_members");}
    crate::consult::ExplicitConsultMembers::new([profile.defaults.single.clone()])?;
    if !rows.contains_key(&profile.defaults.single){bail!("single_default_missing");}
    if !profile.defaults.fusion.is_empty() {
        if !(2..=5).contains(&profile.defaults.fusion.len()){bail!("fusion_default_requires_two_to_five_members");}
        crate::consult::ExplicitConsultMembers::new(profile.defaults.fusion.clone())?;
        if profile.defaults.fusion.iter().any(|a|!rows.contains_key(a)){bail!("fusion_default_missing");}
    }
    Ok(profile)
}
fn load_profile(dir:&Path)->Result<Profile> {profile_shape(&read_regular(&dir.join("profile.json"))?)}
fn config_value(profile:&Profile)->Value {json!({"version":1,"harnesses":profile.harnesses})}
fn validate_profile(profile:&Profile,source:&Path)->Result<HarnessConfigSnapshot> {
    let snapshot=harness_config::parse_harness_config_snapshot(source,&serde_json::to_string(&config_value(profile))?)?;
    snapshot.lint_without_tokens()?;
    for alias in std::iter::once(&profile.defaults.single).chain(profile.defaults.fusion.iter()) {snapshot.resolve(alias,HarnessAction::Consult)?;}
    Ok(snapshot)
}
/// Validate explicit personal choices using the single core parser and publish privately.
/// Explicit replacement preserves and verifies exact previous bytes before publication.
pub fn configure_profile(root:&Path,dir:&Path,input:&Path,replace:bool)->Result<Value> {
    let project=canonical_project(root)?;safe_path(dir,true)?;let profile=profile_shape(&read_regular(input)?)?;
    validate_profile(&profile,&project.join(".orch/harnesses.yaml"))?;
    with_lock(&dir.join(".profile.lock"),|| {
        let target=dir.join("profile.json");safe_path(&target,false)?;let mut backup=None;let mut expected=None;
        if target.exists() {
            if !replace {bail!("personal_profile_exists_use_replace_profile");}
            let metadata=fs::symlink_metadata(&target)?;let original=read_regular(&target)?;let path=dir.join("backups").join(format!("profile-{}.json",ulid::Ulid::new()));
            atomic_bytes(&path,&original,false)?;if read_regular(&path)?!=original{bail!("profile_backup_verification_failed");}backup=Some(path);expected=Some(ExpectedProfile{metadata,bytes:original});
        }
        let displaced=publish_profile(&target,&encoded(&profile)?,expected.as_ref()).with_context(||format!("Profile publication not confirmed at {}; inspect before retrying; snapshot backup={}",target.display(),backup.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"none (first profile)".into())))?;
        Ok(json!({"saved":target,"defaults":profile.defaults,"backup":displaced.as_ref().or(backup.as_ref()),"snapshotBackup":backup,"projectConfigurationChanged":false,"loginVerified":false}))
    })
}
fn add_excludes(owner:&Path)->Result<()> {
    let value=PathBuf::from(git(owner,&["rev-parse","--git-path","info/exclude"])?);let path=if value.is_absolute(){value}else{owner.join(value)};
    make_dir(path.parent().context("missing_exclude_parent")?)?;safe_path(&path,false)?;
    let file=OpenOptions::new().read(true).append(true).create(true).mode(0o600).custom_flags(SAFE_OPEN).open(&path)?;
    if !file.metadata()?.is_file()||file.metadata()?.len()>LIMIT as u64{bail!("invalid_exclude_file");}
    let mut lock=fd_lock::RwLock::new(file);let mut guard=lock.write()?;let mut bytes=Vec::new();(&mut *guard).take(LIMIT as u64+1).read_to_end(&mut bytes)?;
    if bytes.len()>LIMIT{bail!("exclude_file_too_large");}let text=std::str::from_utf8(&bytes)?;
    let missing=EXCLUDES.iter().filter(|line|!text.lines().any(|existing|existing==**line)).collect::<Vec<_>>();
    if !missing.is_empty(){let mut suffix=String::new();if !bytes.is_empty()&&!bytes.ends_with(b"\n"){suffix.push('\n');}for line in missing{suffix.push_str(line);suffix.push('\n');}guard.write_all(suffix.as_bytes())?;guard.sync_all()?;}Ok(())
}
fn read_registry(path:&Path)->Result<Registry> {
    let registry:Registry=serde_json::from_slice(&read_regular(path)?).context("invalid_project_registry")?;
    if registry.version!=1||registry.projects.len()>32{bail!("invalid_project_registry");}
    let mut seen=BTreeSet::new();for item in &registry.projects {if !item.path.is_absolute()||!seen.insert(item.path.clone()){bail!("invalid_project_registry_entry");}}
    Ok(registry)
}
fn register_project(project:&Path,dir:&Path)->Result<()> {
    with_lock(&dir.join(".projects.lock"),|| {
        let path=dir.join("projects.json");safe_path(&path,false)?;
        let mut registry=if path.exists(){read_registry(&path)?}else{Registry{version:1,projects:vec![]}};
        let metadata=fs::metadata(project)?;let row=RegisteredProject{path:project.to_path_buf(),device:metadata.dev(),inode:metadata.ino()};
        registry.projects.retain(|p|p.path!=project);registry.projects.push(row);registry.projects.sort_by(|a,b|a.path.cmp(&b.path));
        if registry.projects.len()>32{bail!("project_registry_full");}atomic_bytes(&path,&encoded(&registry)?,true)
    })
}
/// Create only missing shared project configuration; register the exact requested worktree.
/// Existing configuration bytes are validated and never replaced or reformatted.
pub fn attach_project(root:&Path,dir:&Path)->Result<Value> {
    let project=canonical_project(root)?;safe_path(dir,true)?;let owner=crate::fusion_roles::project_root(&project)?;let state=owner.join(".orch");make_dir(&state)?;
    let target=state.join("harnesses.yaml");let created=with_lock(&state.join(".openorch-setup.lock"),|| {
        safe_path(&target,false)?;
        if target.exists(){harness_config::load_harness_config_snapshot(&project)?.lint_without_tokens()?;return Ok(false);}
        let profile=load_profile(dir)?;validate_profile(&profile,&target)?;add_excludes(&owner)?;
        atomic_bytes(&target,&encoded(&config_value(&profile))?,false).context("Project config publication not confirmed; generated local ignore entries may already be present; existing target bytes were not replaced")?;harness_config::load_harness_config_snapshot(&project)?.lint_without_tokens()?;Ok(true)
    })?;
    register_project(&project,dir)?;Ok(json!({"created":created,"config":target,"project":project,"projectsFile":dir.join("projects.json")}))
}
/// Read an explicit bounded registry and reject changed worktree identities or symlinks.
pub fn registered_projects_file(path:&Path)->Result<Vec<PathBuf>> {
    let registry=read_registry(path)?;if registry.projects.is_empty(){bail!("no_registered_projects_run_attach");}
    let mut projects=Vec::new();for row in registry.projects {
        safe_path(&row.path,true)?;let actual=canonical_project(&row.path)?;let metadata=fs::metadata(&actual)?;
        if actual!=row.path||metadata.dev()!=row.device||metadata.ino()!=row.inode{bail!("project_registration_changed_reattach_required");}projects.push(actual);
    }Ok(projects)
}
/// Read the selected personal registry without creating it or broadening its authority.
pub fn registered_projects(dir:&Path)->Result<Vec<PathBuf>> {registered_projects_file(&dir.join("projects.json"))}
/// Resolve saved defaults or an explicit invocation override without modifying either.
pub fn select_members(dir:&Path,mode:&str,overrides:&[String])->Result<Vec<String>> {
    let members=if overrides.is_empty(){let profile=load_profile(dir)?;match mode{"single"=>vec![profile.defaults.single],"fusion"=>profile.defaults.fusion,_=>bail!("invalid_consultation_mode")}}else{overrides.to_vec()};
    match mode {"single" if members.len()==1=>{},"fusion" if (2..=5).contains(&members.len())=>{},_=>bail!("single_requires_one_fusion_requires_two_to_five")}
    crate::consult::ExplicitConsultMembers::new(members.clone())?;Ok(members)
}
fn discovery_rows(snapshot:&HarnessConfigSnapshot)->Vec<Value> {
    snapshot.discover_for_action(HarnessAction::Consult).into_iter().map(|r|json!({"alias":r.alias(),"driver":r.driver(),"status":r.availability().label(),"reason":r.availability().reason().unwrap_or("—")})).collect()
}
/// Discover installed executable candidates using the shared native locator, without inference.
pub fn discover_clients(root:&Path)->Result<Value> {
    let project=canonical_project(root)?;let mut context=crate::native_discovery::DiscoveryContext::system(&project)?;context.allow_commands=false;
    let found=crate::native_discovery::installed_executables(&context).into_iter().map(|(name,path)|(name.clone(),json!({"driver":name,"executable":path,"enabled":true,"cwdPolicy":"project-root"}))).collect::<serde_json::Map<_,_>>();
    let candidates=if found.is_empty(){vec![]}else{let snapshot=harness_config::parse_harness_config_snapshot(&project.join(".orch/discovery.json"),&serde_json::to_string(&json!({"version":1,"harnesses":found}))?)?;discovery_rows(&snapshot).into_iter().map(|mut r|{r["executable"]=found[r["alias"].as_str().unwrap()]["executable"].clone();r}).collect()};
    Ok(json!({"candidates":candidates,"configurationExample":found,"loginVerified":false,"modelsInvoked":0,"message":"Installed capability does not prove login or model availability; no automatic substitution."}))
}
/// Inspect project configuration and optional saved defaults without invoking any model.
pub fn doctor(root:&Path,dir:&Path)->Result<Value> {
    let project=canonical_project(root)?;let snapshot=harness_config::load_harness_config_snapshot(&project)?;snapshot.lint_without_tokens()?;
    let profile=dir.join("profile.json");safe_path(&profile,false)?;let defaults=if profile.exists(){serde_json::to_value(load_profile(dir)?.defaults)?}else{Value::Null};
    Ok(json!({"project":project,"config":snapshot.source_path(),"doctor":"Git and local harness configuration checked","harnesses":discovery_rows(&snapshot),"defaults":defaults,"loginVerified":false,"modelsInvoked":0}))
}
/// Stage a literal private question and return the ordinary shared CLI consultation arguments.
/// Locks are released before the caller executes the consultation; no provider starts here.
pub fn prepare_consultation(root:&Path,dir:&Path,mode:&str,question:&Path,overrides:&[String],attachments:&[PathBuf])->Result<crate::consult::ConsultArgs> {
    let project=canonical_project(root)?;let members=select_members(dir,mode,overrides)?;let bytes=read_regular(question)?;
    let text=std::str::from_utf8(&bytes).context("question_not_utf8")?;if text.trim().is_empty(){bail!("empty_question");}crate::consult::validate_question_before_reservation(text)?;
    attach_project(&project,dir)?;let requests=project.join(".orch/openorch-requests");make_dir(&requests)?;let path=requests.join(format!("{}.md",ulid::Ulid::new()));atomic_bytes(&path,&bytes,false)?;
    Ok(crate::consult::ConsultArgs{question:path,harnesses:members,attachments:attachments.iter().map(|p|if p.is_absolute(){p.clone()}else{project.join(p)}).collect(),member_timeout_secs:None,total_wall_secs:None})
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn profile_changed_after_backup_is_not_overwritten() {
        let root=crate::util::test_scratch_dir("profile compare before publish");let path=root.join("profile.json");fs::write(&path,b"old snapshot").unwrap();
        let expected=ExpectedProfile{metadata:fs::metadata(&path).unwrap(),bytes:read_regular(&path).unwrap()};
        fs::write(&path,b"new user edit").unwrap();
        assert!(publish_profile(&path,b"helper update",Some(&expected)).is_err());
        assert_eq!(fs::read(&path).unwrap(),b"new user edit");
    }
    #[test]
    fn missing_at_backup_does_not_authorize_overwriting_a_new_profile() {
        let root=crate::util::test_scratch_dir("profile appeared before publish");let path=root.join("profile.json");
        fs::write(&path,b"concurrently created user profile").unwrap();
        assert!(publish_profile(&path,b"helper update",None).is_err());
        assert_eq!(fs::read(&path).unwrap(),b"concurrently created user profile");
    }
    #[test]
    fn post_publication_error_names_the_retained_backup() {
        use std::os::unix::fs::PermissionsExt;
        let root=crate::util::test_scratch_dir("profile directory sync failure");
        for args in [vec!["init","-q"],vec!["-c","user.name=Fixture","-c","user.email=fixture@example.invalid","-c","core.hooksPath=/dev/null","-c","commit.gpgSign=false","commit","--allow-empty","-qm","base"]] {
            assert!(Command::new("git").args(args).current_dir(&root).status().unwrap().success());
        }
        let native=root.join("native");fs::write(&native,"#!/bin/sh\nexit 0\n").unwrap();fs::set_permissions(&native,fs::Permissions::from_mode(0o755)).unwrap();
        let mut profile=json!({"version":1,"harnesses":{"one":{"driver":"claude","executable":native,"enabled":true,"cwdPolicy":"project-root"}},"defaults":{"single":"one","fusion":[]}});
        let input=root.join("input.json");fs::write(&input,encoded(&profile).unwrap()).unwrap();let dir=root.join("personal");configure_profile(&root,&dir,&input,false).unwrap();let old=read_regular(&dir.join("profile.json")).unwrap();
        profile["harnesses"]["one"]["defaults"]=json!({"model":"fixture-change"});fs::write(&input,encoded(&profile).unwrap()).unwrap();
        fs::set_permissions(&dir,fs::Permissions::from_mode(0o300)).unwrap();
        if File::open(&dir).is_ok() {fs::set_permissions(&dir,fs::Permissions::from_mode(0o700)).unwrap();eprintln!("directory read permission is not enforced for this identity");return;}
        let result=configure_profile(&root,&dir,&input,true);fs::set_permissions(&dir,fs::Permissions::from_mode(0o700)).unwrap();
        let error=result.unwrap_err();assert!(error.to_string().contains("backup"),"{error:#}");
        assert_eq!(serde_json::from_slice::<Value>(&read_regular(&dir.join("profile.json")).unwrap()).unwrap()["harnesses"]["one"]["defaults"]["model"],"fixture-change");
        let backups=fs::read_dir(dir.join("backups")).unwrap().map(|p|p.unwrap().path()).collect::<Vec<_>>();assert!(backups.iter().any(|p|read_regular(p).unwrap()==old));
    }

    #[test]
    fn edit_after_last_check_is_retained_as_the_actual_overwritten_backup() {
        let root=crate::util::test_scratch_dir("profile late edit exchange");let path=root.join("profile.json");fs::write(&path,b"old snapshot").unwrap();
        let expected=ExpectedProfile{metadata:fs::metadata(&path).unwrap(),bytes:read_regular(&path).unwrap()};
        PROFILE_LATE_EDITS.get_or_init(Default::default).lock().unwrap().insert(path.clone(),b"post-check user edit".to_vec());
        assert!(publish_profile(&path,b"helper update",Some(&expected)).is_err());
        assert_eq!(fs::read(&path).unwrap(),b"helper update");
        let backups=fs::read_dir(root.join("backups")).unwrap().map(|p|p.unwrap().path()).collect::<Vec<_>>();
        assert!(backups.iter().any(|p|read_regular(p).unwrap()==b"post-check user edit"));
    }

    #[test]
    fn target_edited_after_exchange_is_not_confirmed_as_requested_profile() {
        let root=crate::util::test_scratch_dir("profile post exchange edit");let path=root.join("profile.json");fs::write(&path,b"old snapshot").unwrap();
        let expected=ExpectedProfile{metadata:fs::metadata(&path).unwrap(),bytes:read_regular(&path).unwrap()};
        PROFILE_POST_EDITS.get_or_init(Default::default).lock().unwrap().insert(path.clone(),b"user edit after exchange".to_vec());
        assert!(publish_profile(&path,b"helper update",Some(&expected)).is_err());
        assert_eq!(fs::read(&path).unwrap(),b"user edit after exchange");
        let backups=fs::read_dir(root.join("backups")).unwrap().map(|p|p.unwrap().path()).collect::<Vec<_>>();assert!(backups.iter().any(|p|read_regular(p).unwrap()==b"old snapshot"));
    }

}
