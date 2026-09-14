//! Bounded, read-only observations of local invocation facts.
//!
//! Phase records describe past observations, never liveness. Native termination,
//! answer verification and associated task state are independent dimensions.
//! This module never loads current harness configuration or invokes recovery.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[cfg(target_os = "macos")]
const OPEN_NOFOLLOW: i32 = 0x100 | 0x4;
#[cfg(target_os = "linux")]
const OPEN_NOFOLLOW: i32 = 0x20000 | 0x800;
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
const OPEN_NOFOLLOW: i32 = 0;
unsafe extern "C" {
    fn geteuid() -> u32;
}

const RECORD: usize = 64 * 1024;
const BODY: usize = 1024 * 1024;
const META_BUDGET: usize = 32 * BODY;
const BODY_BUDGET: usize = 8 * BODY;

/// A source-linked task version. Only explicit ledger relationships populate it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskObservation {
    /// Exact ledger round, never inferred from file text.
    pub round: String,
    /// Explicit task identifier.
    pub id: String,
    /// Attempt named by the invocation evidence.
    pub attempt: Option<String>,
    /// Last proven state for this version; missing relationships stay unknown.
    pub state: String,
    /// Immutable invocation revision.
    pub head: Option<String>,
}
/// One stable invocation; duplicate aliases remain distinct rows.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationObservation {
    /// Stable source-qualified identity used for selection and detail.
    pub id: String,
    /// consult, standalone, or selfhost.
    pub source: String,
    /// Captured alias, if known.
    pub alias: Option<String>,
    /// Captured native harness driver.
    pub driver: Option<String>,
    /// Explicit action/purpose, never a guessed task.
    pub purpose: Option<String>,
    /// Safe, bounded source summary.
    pub summary: String,
    /// Requested parameters from this invocation.
    pub requested_tuple: Value,
    /// Effective parameters from this invocation.
    pub effective_tuple: Value,
    /// Captured invocation limits and configuration digest; never current config.
    pub parameters: Value,
    /// Reliable activity offsets captured by the runner, not filesystem timestamps.
    pub activity: Value,
    /// Immutable project revision, if recorded.
    pub head: Option<String>,
    /// Start publication time; unavailable for historical records without it.
    pub started_at: Option<String>,
    /// Embedded time of the latest reliable source, distinct from read time.
    pub source_time: Option<String>,
    /// Last observed phase, not a claim of current liveness.
    pub phase: String,
    /// Recorded elapsed duration; no wall-clock completion inference.
    pub duration_secs: Option<u64>,
    /// none, verified, invalid, failed, unknown or too-large.
    pub result: String,
    /// Independent native terminal facts, if available.
    pub native_status: Value,
    /// Only explicit task association.
    pub task: Option<TaskObservation>,
    /// Related consultation identity.
    pub fusion_id: Option<String>,
    /// Safe repository-relative evidence locator.
    pub locator: String,
    /// Local degradation reasons.
    pub diagnostics: Vec<String>,
}
/// A consultation roster and its independent phase/result counts.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FusionObservation {
    /// Consultation ID.
    pub id: String,
    /// Stable invocation IDs for known members.
    pub members: Vec<String>,
    /// Exact total only when an authoritative full roster was validated.
    pub total: Option<usize>,
    /// Whether all explicit slots are accounted for.
    pub roster_complete: bool,
    /// Counts by last observed phase.
    pub phase_counts: BTreeMap<String, usize>,
    /// Counts by independently verified result classification.
    pub result_counts: BTreeMap<String, usize>,
}
/// One bounded snapshot; omissions always have diagnostics.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectObservation {
    /// Canonical project root, sanitized for display.
    pub root: String,
    /// Reader wall time; never substituted for source time.
    pub read_at: String,
    /// Known invocations in stable source order.
    pub rows: Vec<InvocationObservation>,
    /// Full or partial consultation rosters.
    pub groups: Vec<FusionObservation>,
    /// Safe source-local failures and clipping indications.
    pub diagnostics: Vec<String>,
    /// At least one enumeration or byte budget was clipped.
    pub truncated: bool,
}
/// On-demand, bounded answer text and its current verification result.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InvocationDetail {
    /// Current verified row, never a stale cached success decision.
    pub row: InvocationObservation,
    /// Safe text only after full body verification.
    pub text: Option<String>,
    /// Stable safe evidence locator.
    pub locator: String,
    /// Text display was clipped after sanitization.
    pub truncated: bool,
}

/// Strip executable control characters before display or clipboard use.
/// Both token-boundary and joined views are checked with the existing credential
/// recognizer. A known credential pattern masks the entire field, before clipping.
pub fn safe_observation_text(text: &str) -> String {
    let boundary: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let joined: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut detection = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        if !c.is_control() {
            detection.push(c)
        }
    }
    if crate::redact::has_secret(&boundary)
        || crate::redact::has_secret(&joined)
        || crate::redact::has_secret(&detection)
    {
        "[redacted]".into()
    } else {
        boundary
    }
}
fn safe_value(value: &Value) -> Value {
    match value {
        Value::String(s) => safe_observation_text(s).into(),
        Value::Array(v) => v.iter().map(safe_value).collect(),
        Value::Object(v) => Value::Object(
            v.iter()
                .map(|(k, v)| (safe_observation_text(k), safe_value(v)))
                .collect(),
        ),
        _ => value.clone(),
    }
}
fn string(v: &Value, k: &str) -> Option<String> {
    v.get(k)?.as_str().map(safe_observation_text)
}
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn alias(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
fn ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b"0123456789ABCDEFGHJKMNPQRSTVWXYZ".contains(&b))
        && s.as_bytes()[0] <= b'7'
}
fn hash(b: &[u8]) -> String {
    format!("{:x}", Sha256::digest(b))
}
#[derive(Clone, PartialEq, Eq)]
struct Fingerprint(u64, u64, u64, i64, i64, i64, i64, u32, u32);
fn fingerprint(m: &fs::Metadata) -> Fingerprint {
    Fingerprint(
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
        m.mode(),
        m.uid(),
    )
}
struct Cached {
    stamp: Fingerprint,
    bytes: Arc<Vec<u8>>,
    digest: String,
}
struct Budget {
    metadata: usize,
    bodies: usize,
    clipped: bool,
}
impl Budget {
    fn new() -> Self {
        Self {
            metadata: META_BUDGET,
            bodies: BODY_BUDGET,
            clipped: false,
        }
    }
}

/// Stateful cache of immutable local bytes, with refreshed metadata fingerprints.
/// Opening validates only the existing root and never creates runtime directories.
pub struct ObservationReader {
    root: PathBuf,
    cache: BTreeMap<PathBuf, Cached>,
    snapshot: Option<ProjectObservation>,
    bodies: BTreeMap<String, (PathBuf, String)>,
}
impl ObservationReader {
    /// Open an existing project directory without requiring a round, IR or config.
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root).context("project unavailable")?;
        if !root.is_dir() {
            bail!("project is not a directory")
        };
        Ok(Self {
            root,
            cache: BTreeMap::new(),
            snapshot: None,
            bodies: BTreeMap::new(),
        })
    }
    fn file(&self, path: &Path) -> Result<(File, fs::Metadata)> {
        let rel = path
            .strip_prefix(&self.root)
            .context("source outside project")?;
        let mut p = self.root.clone();
        for part in rel.components() {
            let Component::Normal(part) = part else {
                bail!("invalid source path")
            };
            p.push(part);
            let m = fs::symlink_metadata(&p)?;
            if m.file_type().is_symlink() {
                bail!("symlink source")
            };
            if p != path && !m.is_dir() {
                bail!("non-directory source ancestor")
            }
            if p == path && !m.is_file() {
                bail!("nonregular source")
            }
        }
        let f = OpenOptions::new()
            .read(true)
            .custom_flags(OPEN_NOFOLLOW)
            .open(path)?;
        let m = f.metadata()?;
        if !m.is_file() || m.uid() != unsafe { geteuid() } || m.mode() & 0o022 != 0 {
            bail!("unsafe source")
        };
        if fingerprint(&m) != fingerprint(&fs::symlink_metadata(path)?) {
            bail!("changed source")
        };
        Ok((f, m))
    }
    fn read(
        &mut self,
        path: &Path,
        limit: usize,
        budget: &mut Budget,
        body: bool,
    ) -> Result<Arc<Vec<u8>>> {
        let (mut file, meta) = match self.file(path) {
            Ok(x) => x,
            Err(e) => {
                self.cache.remove(path);
                return Err(e);
            }
        };
        let n = usize::try_from(meta.len())?;
        if n > limit {
            bail!("source too large")
        };
        let remaining = if body {
            &mut budget.bodies
        } else {
            &mut budget.metadata
        };
        if n > *remaining {
            budget.clipped = true;
            bail!("source budget clipped")
        };
        *remaining -= n;
        let stamp = fingerprint(&meta);
        if let Some(c) = self.cache.get(path).filter(|c| c.stamp == stamp) {
            return Ok(c.bytes.clone());
        }
        let mut bytes = Vec::with_capacity(n);
        std::io::Read::by_ref(&mut file)
            .take(n as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() != n
            || fingerprint(&file.metadata()?) != stamp
            || fingerprint(&fs::symlink_metadata(path)?) != stamp
        {
            self.cache.remove(path);
            bail!("source changed while reading")
        };
        let bytes = Arc::new(bytes);
        self.cache.insert(
            path.to_owned(),
            Cached {
                stamp,
                digest: hash(&bytes),
                bytes: bytes.clone(),
            },
        );
        Ok(bytes)
    }
    fn json(&mut self, path: &Path, budget: &mut Budget) -> Result<Value> {
        Ok(serde_json::from_slice(
            &self.read(path, RECORD, budget, false)?,
        )?)
    }
    fn row(&self, id: String, source: &str, locator: &Path) -> InvocationObservation {
        InvocationObservation {
            id,
            source: source.into(),
            alias: None,
            driver: None,
            purpose: None,
            summary: String::new(),
            requested_tuple: Value::Null,
            effective_tuple: Value::Null,
            parameters: Value::Null,
            activity: Value::Null,
            head: None,
            started_at: None,
            source_time: None,
            phase: "unknown".into(),
            duration_secs: None,
            result: "none".into(),
            native_status: Value::Null,
            task: None,
            fusion_id: None,
            locator: safe_observation_text(
                &locator
                    .strip_prefix(&self.root)
                    .unwrap_or(locator)
                    .display()
                    .to_string(),
            ),
            diagnostics: vec![],
        }
    }
    /// Refresh bounded local facts. No provider, status CLI, reconcile or collection is invoked.
    pub fn refresh(&mut self) -> Result<ProjectObservation> {
        if !self.root.is_dir() {
            bail!("project unavailable")
        }
        let mut out = ProjectObservation {
            root: safe_observation_text(&self.root.display().to_string()),
            read_at: crate::util::now_rfc3339(),
            rows: vec![],
            groups: vec![],
            diagnostics: vec![],
            truncated: false,
        };
        self.bodies.clear();
        let mut budget = Budget::new();
        let rounds = self.rounds(&mut budget, &mut out);
        if let Some(round) = rounds.first() {
            self.ledger(round, &mut budget, &mut out);
        }
        self.consultations(&mut budget, &mut out);
        let (controls, diag, clipped) =
            crate::channel::managed::observation_projection(&self.root, &mut budget.metadata);
        out.diagnostics.extend(diag);
        out.truncated |= clipped;
        for v in controls {
            let Some(id) = v["wakeId"].as_str() else {
                continue;
            };
            let mut row = self.row(
                format!("standalone:{id}"),
                "standalone",
                &self.root.join(format!(
                    "coordination/runtime/supervisors/{id}.control.json"
                )),
            );
            row.alias = string(&v, "alias");
            row.driver = string(&v, "driver");
            row.purpose = string(&v, "action");
            row.head = string(&v, "fixedHead");
            row.requested_tuple = safe_value(&v["requestedTuple"]);
            row.effective_tuple = safe_value(&v["effectiveTuple"]);
            row.parameters = safe_value(&v["parameters"]);
            row.started_at = string(&v, "publishedAt");
            row.source_time = row.started_at.clone();
            row.phase = "published".into();
            row.native_status = safe_value(&v["status"]);
            if v["statusSupported"] == false {
                row.diagnostics.push("native status unsupported".into())
            } else if v["status"]["managedScopeTerminated"] == true {
                row.phase = "ended".into()
            };
            out.rows.push(row);
        }
        for round in rounds.iter().skip(1) {
            self.ledger(round, &mut budget, &mut out);
        }
        out.truncated |= budget.clipped;
        if budget.clipped {
            out.diagnostics.push("aggregate read budget clipped".into());
        }
        // A bounded cache never keeps all prior projects/history in memory.
        if self.cache.len() > 8192
            || self.cache.values().map(|c| c.bytes.len()).sum::<usize>() > 64 * BODY
        {
            self.cache.clear();
        }
        self.snapshot = Some(out.clone());
        Ok(out)
    }
    /// Revalidate the selected ID and return only fully verified answer bytes.
    pub fn detail(&mut self, id: &str) -> Result<InvocationDetail> {
        let snap = self.refresh()?;
        let mut row = snap
            .rows
            .into_iter()
            .find(|r| r.id == id)
            .context("invocation no longer available")?;
        let mut text = None;
        let mut truncated = false;
        if row.result == "verified" {
            if let Some((path, expected)) = self.bodies.get(id).cloned() {
                let bytes = self.read(&path, BODY, &mut Budget::new(), true)?;
                if self.cache.get(&path).map(|c| &c.digest) != Some(&expected) {
                    row.result = "invalid".into();
                    return Ok(InvocationDetail {
                        locator: row.locator.clone(),
                        row,
                        text: None,
                        truncated: false,
                    });
                }
                let safe = safe_observation_text(std::str::from_utf8(&bytes)?);
                let mut end = safe.len().min(RECORD);
                while !safe.is_char_boundary(end) {
                    end -= 1
                }
                truncated = end < safe.len();
                text = Some(safe[..end].into());
            }
        }
        Ok(InvocationDetail {
            locator: row.locator.clone(),
            row,
            text,
            truncated,
        })
    }
    fn consultations(&mut self, budget: &mut Budget, out: &mut ProjectObservation) {
        let dir = self.root.join("coordination/consultations");
        if !dir.exists() {
            return;
        }
        if fs::canonicalize(&dir).ok().as_ref() != Some(&dir) {
            out.diagnostics.push("consultation directory unsafe".into());
            return;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            out.diagnostics
                .push("consultation directory unavailable".into());
            return;
        };
        let mut ids = BTreeSet::new();
        for e in entries {
            if let Ok(e) = e {
                if let Some(id) = e.file_name().to_str().filter(|id| ulid(id)) {
                    ids.insert(id.to_owned());
                    if ids.len() > 1024 {
                        ids.pop_first();
                        out.truncated = true;
                    }
                }
            }
        }
        if out.truncated {
            out.diagnostics
                .push("consultation candidates clipped".into());
        }
        for id in ids.into_iter().rev() {
            self.consultation(&dir.join(&id), &id, budget, out);
        }
    }
    fn consultation(
        &mut self,
        dir: &Path,
        id: &str,
        budget: &mut Budget,
        out: &mut ProjectObservation,
    ) {
        let start_path = dir.join("start.json");
        let start_present = fs::symlink_metadata(&start_path).is_ok();
        let start = self.json(&start_path, budget).ok().filter(|v| {
            v["version"] == 1
                && v["consultationId"] == id
                && v["project"].as_str() == self.root.to_str()
                && v["head"].as_str().is_some_and(|s| hex(s, 40))
                && v["members"]
                    .as_array()
                    .is_some_and(|a| !a.is_empty() && a.len() <= 5)
        });
        if start_present && start.is_none() {
            out.diagnostics.push(format!(
                "consultation {id}: start unavailable or unsupported"
            ));
        }
        let mut manifests = BTreeMap::new();
        if let Ok(entries) = fs::read_dir(dir.join("fusion")) {
            for e in entries.flatten() {
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                let Some(stem) = name.strip_suffix(".manifest.json") else {
                    continue;
                };
                let Some((index, a)) = stem.split_once('-') else {
                    continue;
                };
                let Ok(index) = index.parse::<usize>() else {
                    continue;
                };
                if index >= 5 || !alias(a) {
                    continue;
                };
                match self.json(&e.path(), budget) {
                    Ok(v) if v["schemaVersion"] == 3 && v["index"] == index && v["member"] == a => {
                        manifests.insert(index, (a.to_owned(), v, e.path()));
                    }
                    _ => out
                        .diagnostics
                        .push(format!("consultation {id}: member record unavailable")),
                }
            }
        }
        let mut slots = BTreeMap::<usize, (String, Value)>::new();
        let mut complete = false;
        if let Some(v) = &start {
            complete = true;
            for (i, slot) in v["members"].as_array().unwrap().iter().enumerate() {
                let a = slot["alias"].as_str().unwrap_or("");
                if slot["index"] != i
                    || !alias(a)
                    || slot["actionId"] != format!("{id}-{i}-{a}")
                    || slot["facts"]["alias"] != a
                    || slot["facts"]["fixedHead"] != v["head"]
                    || slot["facts"]["cwd"] != v["project"]
                    || slot["facts"]["configDigest"] != v["configDigest"]
                {
                    complete = false;
                    break;
                };
                slots.insert(i, (a.into(), slot["facts"].clone()));
            }
            if !complete {
                slots.clear();
                out.diagnostics
                    .push(format!("consultation {id}: roster identity mismatch"));
            }
        }
        if slots.is_empty() {
            for (i, (a, v, _)) in &manifests {
                slots.insert(*i, (a.clone(), v["channelFacts"].clone()));
            }
            if !start_present {
                if let Ok(meta) = self.json(&dir.join("meta.json"), budget) {
                    if meta["schemaVersion"] == 3 && meta["id"] == id {
                        if let Some(members) = meta["members"].as_array() {
                            complete = !members.is_empty()
                                && members.len() <= 5
                                && members.len() == manifests.len()
                                && meta["membership"]["source"] == "explicit"
                                && meta["membership"]["harnesses"].as_array().is_some_and(|a| {
                                    a.len() == members.len()
                                        && a.iter().enumerate().all(|(i, a)| {
                                            manifests
                                                .get(&i)
                                                .is_some_and(|(alias, _, _)| *a == *alias)
                                        })
                                })
                                && members.iter().enumerate().all(|(i, m)| {
                                    m["index"] == i
                                        && manifests.get(&i).is_some_and(|(a, v, _)| {
                                            m["harness"] == *a && m["status"] == v["status"]
                                        })
                                });
                        }
                    }
                }
            }
        }
        let start_digest = if start.is_some() {
            self.cache.get(&start_path).map(|c| c.digest.clone())
        } else {
            None
        };
        let mut group = FusionObservation {
            id: id.into(),
            members: vec![],
            total: complete.then_some(slots.len()),
            roster_complete: complete,
            phase_counts: BTreeMap::new(),
            result_counts: BTreeMap::new(),
        };
        for (index, (a, facts)) in slots {
            let path = dir.join(format!("fusion/{index}-{a}.manifest.json"));
            let mut row = self.row(format!("consult:{id}:{index}"), "consult", &path);
            row.alias = Some(a.clone());
            row.fusion_id = Some(id.into());
            self.apply_facts(&mut row, &facts);
            row.phase = "prepared".into();
            if complete {
                if let Some(start) = &start {
                    row.started_at = string(start, "startedAt");
                    row.source_time = row.started_at.clone();
                    row.summary = string(start, "summary").unwrap_or_default();
                }
            }
            if let Some(digest) = &start_digest {
                for phase in ["entered", "spawned"] {
                    if let Ok(v) = self.json(&dir.join(format!("{index}.{phase}.json")), budget) {
                        if v["version"] == 1
                            && v["consultationId"] == id
                            && v["index"] == index
                            && v["actionId"] == format!("{id}-{index}-{a}")
                            && v["startDigest"] == *digest
                            && v["phase"] == phase
                        {
                            row.phase = phase.into();
                            row.source_time = string(&v, "observedAt");
                        }
                    }
                }
            }
            if let Some((ma, m, path)) = manifests.get(&index) {
                if *ma == a && Self::facts_match(&facts, &m["channelFacts"]) {
                    self.finish_manifest(&mut row, m, path, budget)
                } else {
                    row.result = "invalid".into();
                    row.diagnostics.push("member identity mismatch".into());
                }
            }
            *group.phase_counts.entry(row.phase.clone()).or_default() += 1;
            *group.result_counts.entry(row.result.clone()).or_default() += 1;
            group.members.push(row.id.clone());
            out.rows.push(row);
        }
        if !group.members.is_empty() {
            out.groups.push(group);
        }
    }
    fn apply_facts(&self, row: &mut InvocationObservation, f: &Value) {
        row.driver = string(f, "driver");
        row.purpose = string(f, "action");
        row.head = string(f, "fixedHead");
        row.requested_tuple = safe_value(&f["requestedTuple"]);
        row.effective_tuple = safe_value(&f["effectiveTuple"]);
        row.parameters = safe_value(
            &json!({"configDigest":f["configDigest"],"limits":f["limits"],"deadlineSecs":f["deadlineSecs"]}),
        );
    }
    fn facts_match(a: &Value, b: &Value) -> bool {
        [
            "alias",
            "driver",
            "action",
            "commandDigest",
            "executableIdentityDigest",
            "fixedHead",
            "cwd",
            "requestDigest",
            "configDigest",
            "attachmentManifestSha256",
            "requestedTuple",
            "effectiveTuple",
        ]
        .iter()
        .all(|k| a[*k] == b[*k])
    }
    fn finish_manifest(
        &mut self,
        row: &mut InvocationObservation,
        m: &Value,
        path: &Path,
        budget: &mut Budget,
    ) {
        let f = &m["channelFacts"];
        row.activity = safe_value(
            &json!({"firstFrameAfterMillis":f["execution"]["firstFrameAfterMillis"],"leaderExitedAfterMillis":f["execution"]["leaderExitedAfterMillis"],"elapsedMillis":f["execution"]["elapsedMillis"]}),
        );
        row.source_time = string(m, "observedAt").or(row.source_time.clone());
        row.duration_secs = m["durationSecs"].as_u64();
        row.native_status = safe_value(&f["terminal"]);
        row.phase = if f["stage"] == "prepare-or-render-rejected"
            || f["stage"] == "preflight-or-execution-rejected"
        {
            "rejected"
        } else if f["execution"]["processGroupTerminated"] == true {
            "ended"
        } else {
            "unknown"
        }
        .into();
        row.result = "failed".into();
        if m["status"] != "ok" {
            return;
        }
        row.result = "invalid".into();
        let Ok(inv) = serde_json::from_value::<crate::consult::ConsultMemberManifestV3>(
            m["invocation"].clone(),
        ) else {
            return;
        };
        let Some(head) = f["fixedHead"].as_str() else {
            return;
        };
        let Some(cwd) = f["cwd"].as_str() else { return };
        let Some(request) = f["requestDigest"].as_str() else {
            return;
        };
        let Some(config) = f["configDigest"].as_str() else {
            return;
        };
        let Some(attachments) = f["attachmentManifestSha256"].as_str() else {
            return;
        };
        if !inv.matches_invocation(head, cwd, request, config, attachments)
            || cwd != self.root.to_string_lossy()
            || m["invocation"]["alias"] != m["member"]
            || f["alias"] != m["member"]
            || f["terminal"]["turnEnded"] != true
            || f["execution"]["rawCaptureStable"] != true
            || f["execution"]["processGroupTerminated"] != true
            || f["execution"]["stdoutEofObserved"] != true
            || f["execution"]["stderrEofObserved"] != true
            || f["execution"]["stdoutOverflow"] != false
            || f["execution"]["stderrOverflow"] != false
            || !f["execution"]["observationErrors"]
                .as_array()
                .is_some_and(|e| e.is_empty())
            || f["terminal"]["status"] != "answered"
            || f["terminal"]["mechanicalTerminalAbsent"] != false
            || f["terminal"]["managedScopeTerminated"] != true
        {
            return;
        }
        let Some(name) = m["artifact"]["path"].as_str() else {
            return;
        };
        let expected = format!("{}-{}.md", m["index"], m["member"].as_str().unwrap_or(""));
        if name != expected {
            return;
        }
        let body = path.parent().unwrap().join(name);
        let bytes = match self.read(&body, BODY, budget, true) {
            Ok(b) => b,
            Err(_) => {
                if fs::symlink_metadata(&body).is_ok_and(|m| m.len() > BODY as u64) {
                    row.result = "too-large".into()
                }
                return;
            }
        };
        let actual = &self.cache[&body].digest;
        if !inv.matches_artifact(actual, bytes.len() as u64)
            || m["artifact"]["sha256"] != *actual
            || m["artifact"]["bytes"] != bytes.len()
            || bytes.is_empty()
            || std::str::from_utf8(&bytes).is_err()
        {
            return;
        }
        if let Some(expected) = f["terminal"]["finalTextSha256"].as_str() {
            if expected != actual {
                return;
            }
        }
        row.result = "verified".into();
        self.bodies.insert(row.id.clone(), (body, actual.clone()));
    }
    fn rounds(&mut self, budget: &mut Budget, out: &mut ProjectObservation) -> Vec<String> {
        fn number(s: &str) -> Option<u64> {
            let n = s.strip_prefix('r')?.parse::<u64>().ok()?;
            (format!("r{n}") == s).then_some(n)
        }
        let path = self.root.join("coordination/rounds");
        let mut rounds = BTreeSet::new();
        if fs::canonicalize(&path).ok().as_ref() == Some(&path) {
            if let Ok(entries) = fs::read_dir(path) {
                for e in entries.flatten() {
                    if let Some(n) = e.file_name().to_str().and_then(number) {
                        rounds.insert(n);
                        if rounds.len() > 8 {
                            rounds.pop_first();
                            out.truncated = true;
                        }
                    }
                }
            }
        }
        let current = self
            .read(
                &self.root.join("coordination/runtime/CURRENT-ROUND"),
                128,
                budget,
                false,
            )
            .ok()
            .and_then(|b| std::str::from_utf8(&b).ok().and_then(|s| number(s.trim())));
        let mut result = Vec::new();
        if let Some(n) = current {
            rounds.remove(&n);
            result.push(format!("r{n}"));
        }
        for n in rounds.into_iter().rev() {
            if result.len() == 8 {
                out.truncated = true;
                break;
            }
            result.push(format!("r{n}"));
        }
        if out.truncated {
            out.diagnostics
                .push("ledger round window clipped to eight including current".into());
        }
        result
    }
    fn ledger(&mut self, round: &str, budget: &mut Budget, out: &mut ProjectObservation) {
        let path = self
            .root
            .join(format!("coordination/rounds/{round}/events.jsonl"));
        if !path.exists() {
            return;
        }
        let read = (|| -> Result<Vec<Value>> {
            let (mut file, m) = self.file(&path)?;
            let n = m.len().min((8 * BODY) as u64) as usize;
            if n > budget.metadata {
                budget.clipped = true;
                bail!("ledger budget")
            };
            budget.metadata -= n;
            let offset = m.len() - n as u64;
            let stamp = fingerprint(&m);
            let bytes = if let Some(c) = self.cache.get(&path).filter(|c| c.stamp == stamp) {
                c.bytes.clone()
            } else {
                file.seek(SeekFrom::Start(offset))?;
                let mut bytes = Vec::with_capacity(n);
                file.take(n as u64).read_to_end(&mut bytes)?;
                if fingerprint(&fs::symlink_metadata(&path)?) != stamp {
                    bail!("ledger changed")
                };
                let bytes = Arc::new(bytes);
                self.cache.insert(
                    path.clone(),
                    Cached {
                        stamp,
                        bytes: bytes.clone(),
                        digest: hash(&bytes),
                    },
                );
                bytes
            };
            let bytes = if offset > 0 {
                out.truncated = true;
                out.diagnostics.push(format!("{round}: tail clipped"));
                let end = bytes
                    .iter()
                    .position(|b| *b == b'\n')
                    .map(|i| i + 1)
                    .unwrap_or(bytes.len());
                &bytes[end..]
            } else {
                &bytes[..]
            };
            let mut events = Vec::new();
            for line in bytes.split(|b| *b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                if line.len() > RECORD {
                    out.truncated = true;
                    continue;
                }
                match serde_json::from_slice::<Value>(line) {
                    Ok(v) if v["round"] == round && v["eventId"].as_str().is_some_and(ulid) => {
                        events.push(v)
                    }
                    _ => {
                        if out.diagnostics.len() < 64 {
                            out.diagnostics
                                .push(format!("{round}: invalid ledger record"));
                        }
                    }
                }
            }
            Ok(events)
        })();
        let mut events = match read {
            Ok(v) => v,
            Err(_) => {
                out.diagnostics.push(format!("{round}: ledger unavailable"));
                return;
            }
        };
        let mut counts = BTreeMap::new();
        for e in &events {
            *counts
                .entry(e["eventId"].as_str().unwrap().to_owned())
                .or_insert(0usize) += 1;
        }
        if counts.values().any(|n| *n > 1) {
            out.diagnostics
                .push(format!("{round}: duplicate event identities omitted"));
            events.retain(|e| counts[e["eventId"].as_str().unwrap()] == 1);
        }
        // Index event identities and per-wake evidence once; avoid scanning the
        // full ledger for each historical invocation on every UI refresh.
        let by_id: BTreeMap<&str, (usize, &Value)> = events
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e["eventId"].as_str().map(|id| (id, (i, e))))
            .collect();
        let mut by_wake: BTreeMap<&str, Vec<(usize, &Value)>> = BTreeMap::new();
        let mut pending: BTreeMap<String, (String, String, bool)> = BTreeMap::new();
        let mut recorded: BTreeMap<(String, String, String), Option<String>> = BTreeMap::new();
        for (i, e) in events.iter().enumerate() {
            if let Some(id) = e["payload"]["wakeId"].as_str() {
                by_wake.entry(id).or_default().push((i, e));
            }
            let Some(task) = e["taskId"].as_str() else {
                continue;
            };
            let p = &e["payload"];
            match e["type"].as_str() {
                Some("MergeStarted") => {
                    pending.remove(task);
                    let (Some(attempt), Some(head)) =
                        (p["attemptId"].as_str(), p["headSha"].as_str())
                    else {
                        continue;
                    };
                    let collect = p["collectCompletedEventId"]
                        .as_str()
                        .and_then(|id| by_id.get(id))
                        .is_some_and(|(j, c)| {
                            *j < i
                                && c["type"] == "ReportCollectCompleted"
                                && c["actor"] == "runtime:orch"
                                && c["taskId"] == task
                                && c["payload"]["attemptId"] == attempt
                                && c["payload"]["branchSha"] == head
                        });
                    let verdict = p["verdictEventId"]
                        .as_str()
                        .and_then(|id| by_id.get(id))
                        .is_some_and(|(j, v)| {
                            *j < i
                                && v["type"] == "VerdictIssued"
                                && v["actor"] == "verifier:root"
                                && v["taskId"] == task
                                && v["payload"]["attemptId"] == attempt
                                && v["payload"]["headSha"] == head
                                && v["payload"]["verdict"] == "PASS"
                        });
                    if e["actor"] == "runtime:orch" && collect && verdict && hex(head, 40) {
                        pending.insert(task.into(), (attempt.into(), head.into(), false));
                    }
                }
                Some("MergeExecuted") => {
                    if let Some((_, _, merged)) = pending.get_mut(task) {
                        *merged = e["actor"] == "reviewer:orch-runtime"
                            && p["mergeSha"].as_str().is_some_and(|s| hex(s, 40));
                    }
                }
                Some("TaskRecorded") => {
                    if let Some((attempt, head, merged)) = pending.remove(task) {
                        if merged
                            && e["actor"] == "runtime:orch"
                            && p["postMergeGates"] == "all-green"
                        {
                            recorded.insert((task.into(), attempt, head), string(e, "ts"));
                        }
                    }
                }
                _ => {}
            }
        }
        for (at, wake) in events
            .iter()
            .enumerate()
            .filter(|(_, e)| e["type"] == "WakeIssued" && e["actor"] == "runtime:orch")
        {
            let p = &wake["payload"];
            let Some(wid) = p["wakeId"].as_str() else {
                continue;
            };
            let Some(task) = wake["taskId"].as_str().filter(|s| alias(s)) else {
                continue;
            };
            if p["method"] != "unified-channel-v1"
                || !p["fixedHead"].as_str().is_some_and(|s| hex(s, 40))
                || p["invocationCwd"].as_str() != self.root.to_str()
            {
                continue;
            }
            if ![
                "configDigest",
                "requestDigest",
                "attachmentManifestSha256",
                "commandDigest",
                "executableIdentityDigest",
            ]
            .iter()
            .all(|k| p[*k].as_str().is_some_and(|s| hex(s, 64)))
            {
                continue;
            }
            let tuple_ok = |v: &Value| {
                v.as_object().is_some_and(|o| {
                    o.len() == 4
                        && ["provider", "model", "effort", "mode"].iter().all(|k| {
                            o.get(*k).is_some_and(|v| {
                                v.is_null() || v.as_str().is_some_and(|s| !s.is_empty())
                            })
                        })
                })
            };
            if !tuple_ok(&p["requestedTuple"]) || !tuple_ok(&p["effectiveTuple"]) {
                continue;
            }
            let Some(a) = p["harness"].as_str().filter(|s| alias(s)) else {
                continue;
            };
            let attempt = &p["attemptId"];
            let head = &p["fixedHead"];
            let mut row = self.row(
                format!("selfhost:{round}:{}", wake["eventId"].as_str().unwrap()),
                "selfhost",
                &path,
            );
            row.alias = Some(a.into());
            row.driver = string(p, "driver");
            row.purpose = string(p, "action");
            row.requested_tuple = safe_value(&p["requestedTuple"]);
            row.effective_tuple = safe_value(&p["effectiveTuple"]);
            row.head = string(p, "fixedHead");
            row.started_at = string(wake, "ts");
            row.source_time = row.started_at.clone();
            row.phase = "published".into();
            let mut state = "invoked";
            let suffix = by_wake
                .get(wid)
                .map(|v| {
                    v.iter()
                        .filter(|(i, _)| *i > at)
                        .map(|(_, e)| *e)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let bound = |e: &Value| {
                e["taskId"] == task && e["payload"]["wakeId"] == wid && e["actor"] == "runtime:orch"
            };
            let request = suffix.iter().find(|e| {
                bound(e)
                    && e["type"] == "ReviewRequested"
                    && e["payload"]["attemptId"] == *attempt
                    && e["payload"]["reviewedHead"] == *head
                    && e["payload"]["harness"] == a
            });
            let terminal = suffix.iter().find(|e| {
                bound(e)
                    && e["type"] == "ManagedWakeTerminated"
                    && e["payload"]["agent"] == a
                    && [
                        "configDigest",
                        "requestDigest",
                        "attachmentManifestSha256",
                        "commandDigest",
                        "executableIdentityDigest",
                        "requestedTuple",
                        "effectiveTuple",
                        "driver",
                        "harness",
                        "observationSource",
                        "invocationCwd",
                        "cwdSelection",
                        "fixedHead",
                    ]
                    .iter()
                    .all(|k| e["payload"]["channelBinding"][*k] == p[*k])
            });
            if let Some(t) = terminal {
                row.source_time = string(t, "ts");
                row.native_status = json!({"turnEnded":t["payload"]["turnEnded"],"managedScopeTerminated":t["payload"]["managedScopeTerminated"],"state":safe_value(&t["payload"]["state"])});
                if t["payload"]["managedScopeTerminated"] == true {
                    row.phase = "ended".into();
                }
            }
            let delivery = match (request, terminal) {
                (Some(req), Some(term)) => suffix.iter().find(|e| {
                    bound(e)
                        && e["type"] == "ReviewDelivered"
                        && e["payload"]["requestEventId"] == req["eventId"]
                        && e["payload"]["terminalEventId"] == term["eventId"]
                        && e["payload"]["attemptId"] == *attempt
                        && e["payload"]["reviewedHead"] == *head
                        && e["payload"]["harness"] == a
                        && term["payload"]["turnEnded"] == true
                        && term["payload"]["managedScopeTerminated"] == true
                        && term["payload"]["state"] == "answered"
                        && term["payload"]["terminalSeen"] == true
                        && term["payload"]["mechanicalTerminalAbsent"] != true
                        && by_id[req["eventId"].as_str().unwrap()].0
                            < by_id[term["eventId"].as_str().unwrap()].0
                        && by_id[term["eventId"].as_str().unwrap()].0
                            < by_id[e["eventId"].as_str().unwrap()].0
                }),
                _ => None,
            };
            if let Some(d) = delivery {
                row.result = "invalid".into();
                if let Some(rel) = d["payload"]["path"].as_str() {
                    let body = self.root.join(rel);
                    if let Ok(bytes) = self.read(&body, BODY, budget, true) {
                        let actual = &self.cache[&body].digest;
                        if d["payload"]["sha256"] == *actual
                            && d["payload"]["bytes"] == bytes.len()
                            && !bytes.is_empty()
                        {
                            row.result = "verified".into();
                            self.bodies.insert(row.id.clone(), (body, actual.clone()));
                            state = "reviewed";
                        }
                    }
                }
                if row.result == "verified" && d["payload"]["verdict"] == "PASS" {
                    let key = (
                        task.to_owned(),
                        attempt.as_str().unwrap_or("").to_owned(),
                        head.as_str().unwrap_or("").to_owned(),
                    );
                    if let Some(time) = recorded.get(&key) {
                        state = "recorded";
                        row.source_time = time.clone();
                    }
                }
            }
            row.task = Some(TaskObservation {
                round: round.into(),
                id: task.into(),
                attempt: string(p, "attemptId"),
                state: state.into(),
                head: row.head.clone(),
            });
            out.rows.push(row);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn project() -> PathBuf {
        crate::util::test_scratch_dir("observation-boundaries")
    }
    fn synthetic_member(root: &Path, n: usize, size: usize) {
        let id = format!("{n:026}");
        let dir = root.join(format!("coordination/consultations/{id}/fusion"));
        fs::create_dir_all(&dir).unwrap();
        let bytes = vec![b'a'; size];
        let digest = hash(&bytes);
        fs::write(dir.join("0-one.md"), bytes).unwrap();
        let inv = crate::consult::ConsultMemberManifestV3::new(
            "one",
            "a".repeat(40),
            root,
            "b".repeat(64),
            "c".repeat(64),
            "d".repeat(64),
            &digest,
            size as u64,
        )
        .unwrap();
        let v = json!({"schemaVersion":3,"index":0,"member":"one","status":"ok","invocation":inv,"artifact":{"path":"0-one.md","sha256":digest,"bytes":size},"channelFacts":{"alias":"one","driver":"claude","action":"consult","fixedHead":"a".repeat(40),"cwd":root,"requestDigest":"b".repeat(64),"configDigest":"c".repeat(64),"attachmentManifestSha256":"d".repeat(64),"execution":{"processGroupTerminated":true,"rawCaptureStable":true,"stdoutEofObserved":true,"stderrEofObserved":true,"stdoutOverflow":false,"stderrOverflow":false,"observationErrors":[]},"terminal":{"status":"answered","turnEnded":true,"managedScopeTerminated":true,"mechanicalTerminalAbsent":false,"finalTextSha256":digest}}});
        fs::write(
            dir.join("0-one.manifest.json"),
            serde_json::to_vec(&v).unwrap(),
        )
        .unwrap();
    }
    #[test]
    fn cache_eviction_preserves_the_current_verified_detail_basis() {
        let root = project();
        synthetic_member(&root, 1, 6);
        let mut reader = ObservationReader::open(&root).unwrap();
        let id = reader.refresh().unwrap().rows[0].id.clone();
        reader.cache.insert(
            root.join("cached-history"),
            Cached {
                stamp: Fingerprint(0, 0, 0, 0, 0, 0, 0, 0, 0),
                bytes: Arc::new(vec![0; 64 * BODY + 1]),
                digest: String::new(),
            },
        );
        let detail = reader.detail(&id).unwrap();
        assert_eq!(detail.row.result, "verified");
        assert_eq!(detail.text.as_deref(), Some("aaaaaa"));
        fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn total_body_budget_never_verifies_unread_prefixes() {
        let root = project();
        for n in 0..12 {
            synthetic_member(&root, n, BODY);
        }
        let mut reader = ObservationReader::open(&root).unwrap();
        let snapshot = reader.refresh().unwrap();
        assert_eq!(snapshot.rows.len(), 12);
        assert_eq!(
            snapshot
                .rows
                .iter()
                .filter(|r| r.result == "verified")
                .count(),
            8
        );
        assert!(snapshot.truncated);
        assert!(!snapshot.diagnostics.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
    fn review_events(root: &Path) -> Vec<Value> {
        let tuple = json!({"provider":null,"model":"fixture","effort":null,"mode":null});
        let binding = json!({"driver":"opencode","harness":"one","fixedHead":"a".repeat(40),"invocationCwd":root,"observationSource":"fixture","cwdSelection":"project-root","configDigest":"a".repeat(64),"requestDigest":"b".repeat(64),"attachmentManifestSha256":"c".repeat(64),"commandDigest":"d".repeat(64),"executableIdentityDigest":"e".repeat(64),"requestedTuple":tuple,"effectiveTuple":tuple});
        let wake = "00000001-1111-4111-8111-111111111111";
        let id = |n| format!("{n:026}");
        let event = |n, kind, payload| json!({"eventId":id(n),"ts":"2026-01-01T00:00:00Z","round":"r1","taskId":"B999","actor":"runtime:orch","type":kind,"payload":payload});
        let mut p = binding.clone();
        p["method"] = "unified-channel-v1".into();
        p["wakeId"] = wake.into();
        p["attemptId"] = "B999-A0001".into();
        p["action"] = "review".into();
        fs::write(root.join("review.md"), "answer").unwrap();
        vec![
            event(1, "WakeIssued", p),
            event(
                2,
                "ReviewRequested",
                json!({"wakeId":wake,"attemptId":"B999-A0001","reviewedHead":"a".repeat(40),"harness":"one"}),
            ),
            event(
                3,
                "ManagedWakeTerminated",
                json!({"wakeId":wake,"agent":"one","state":"answered","turnEnded":true,"terminalSeen":true,"managedScopeTerminated":true,"channelBinding":binding}),
            ),
            event(
                4,
                "ReviewDelivered",
                json!({"wakeId":wake,"attemptId":"B999-A0001","harness":"one","reviewedHead":"a".repeat(40),"requestEventId":id(2),"terminalEventId":id(3),"path":"review.md","sha256":hash(b"answer"),"bytes":6,"verdict":"PASS"}),
            ),
        ]
    }
    #[test]
    fn selfhost_terminal_order_and_duplicate_identity_cannot_be_promoted() {
        let root = project();
        let dir = root.join("coordination/rounds/r1");
        fs::create_dir_all(&dir).unwrap();
        let original = review_events(&root);
        let path = dir.join("events.jsonl");
        let write = |events: &Vec<Value>| {
            fs::write(
                &path,
                events.iter().map(|v| format!("{v}\n")).collect::<String>(),
            )
            .unwrap()
        };
        let mut reader = ObservationReader::open(&root).unwrap();
        write(&original);
        assert_eq!(reader.refresh().unwrap().rows[0].result, "verified");
        for (key, v) in [
            ("state", json!("failed")),
            ("terminalSeen", json!(false)),
            ("mechanicalTerminalAbsent", json!(true)),
        ] {
            let mut events = original.clone();
            events[2]["payload"][key] = v;
            write(&events);
            assert!(
                reader
                    .refresh()
                    .unwrap()
                    .rows
                    .iter()
                    .all(|r| r.result != "verified"),
                "{key}"
            );
        }
        let mut events = original.clone();
        events.swap(1, 3);
        write(&events);
        assert!(
            reader
                .refresh()
                .unwrap()
                .rows
                .iter()
                .all(|r| r.result != "verified")
        );
        let mut events = original.clone();
        events.push(events[2].clone());
        write(&events);
        let s = reader.refresh().unwrap();
        assert!(s.rows.iter().all(|r| r.result != "verified"));
        assert!(!s.diagnostics.is_empty());
        fs::remove_dir_all(root).unwrap();
    }
}
