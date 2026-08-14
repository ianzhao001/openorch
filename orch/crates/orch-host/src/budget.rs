//! 轮级预算执行门（design/07 §3）：派发前汇总已花，阈值落账，100% 时暂停。

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use fd_lock::RwLock;
use orch_core::{read_ledger, EventRecord};
use serde::{Deserialize, Serialize};

use crate::ledger;

const MODEL_WAKE_LOCK_REL: &str = "coordination/runtime/locks/model-wake.lock";
const MODEL_WAKE_RESERVATIONS_REL: &str = "coordination/runtime/locks/model-wake-reservations";
const MODEL_WAKE_RESERVATION_TTL_SECS: u64 = 35 * 60;

thread_local! {
    static CURRENT_MODEL_WAKE_RESERVATION: RefCell<Option<String>> = const { RefCell::new(None) };
}

#[derive(Debug, Default)]
pub struct ModelWakePermit {
    reservation_id: Option<String>,
    root: Option<PathBuf>,
    committed: bool,
}

impl ModelWakePermit {
    pub fn reservation_id(&self) -> Option<&str> {
        self.reservation_id.as_deref()
    }

    /// 启动事实已 durable：保留 reservation，交给下一次持 model-wake 锁的账本快照去重清理。
    pub fn commit(&mut self) {
        if let Some(reservation_id) = self.reservation_id() {
            CURRENT_MODEL_WAKE_RESERVATION.with(|current| {
                if current.borrow().as_deref() == Some(reservation_id) {
                    current.borrow_mut().take();
                }
            });
        }
        self.committed = true;
    }
}

impl Drop for ModelWakePermit {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let (Some(root), Some(reservation_id)) =
            (self.root.as_ref(), self.reservation_id.as_deref())
        else {
            return;
        };
        CURRENT_MODEL_WAKE_RESERVATION.with(|current| {
            if current.borrow().as_deref() == Some(reservation_id) {
                current.borrow_mut().take();
            }
        });
        match fs::remove_file(reservation_path(root, reservation_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => eprintln!(
                "orch budget: 回滚未兑现 reservation {} 失败: {}",
                reservation_id, error
            ),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ModelWakeReservation {
    reservation_id: String,
    round: String,
    owner_pid: u32,
    created_at: String,
    #[serde(default)]
    wake_id: Option<String>,
    #[serde(default)]
    epoch_key: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RoundBudget {
    #[serde(rename = "maxUsd", default)]
    pub max_usd: Option<f64>,
    #[serde(rename = "wallMinutes", default)]
    pub wall_minutes: Option<u64>,
    #[serde(rename = "maxModelWakes", default)]
    pub max_model_wakes: Option<u64>,
}

#[derive(Default, Deserialize)]
struct ModeConfig {
    #[serde(default)]
    budgets: Budgets,
}

#[derive(Default, Deserialize)]
struct Budgets {
    #[serde(default)]
    round: RoundBudget,
}

pub fn parse_mode_config(yaml: &str) -> Result<RoundBudget> {
    let mode: ModeConfig =
        serde_yaml::from_str(yaml).context("解析 ModeConfig budgets.round 失败")?;
    Ok(mode.budgets.round)
}

pub fn threshold(spent: f64, max: f64) -> Option<u8> {
    let ratio = spent / max;
    if ratio >= 1.0 {
        Some(100)
    } else if ratio >= 0.8 {
        Some(80)
    } else if ratio >= 0.5 {
        Some(50)
    } else {
        None
    }
}

fn highest_dimension(
    b: &RoundBudget,
    spent_usd: f64,
    spent_wall_mins: u64,
    spent_wakes: u64,
) -> Option<(&'static str, u8)> {
    let candidates = [
        b.max_usd
            .and_then(|max| threshold(spent_usd, max))
            .map(|pct| ("usd", pct)),
        b.wall_minutes
            .and_then(|max| threshold(spent_wall_mins as f64, max as f64))
            .map(|pct| ("wall", pct)),
        b.max_model_wakes
            .and_then(|max| threshold(spent_wakes as f64, max as f64))
            .map(|pct| ("wake", pct)),
    ];
    let mut highest = None;
    for candidate in candidates.into_iter().flatten() {
        if highest.is_none_or(|(_, pct)| candidate.1 > pct) {
            highest = Some(candidate);
        }
    }
    highest
}

pub fn worst_threshold(
    b: &RoundBudget,
    spent_usd: f64,
    spent_wall_mins: u64,
    spent_wakes: u64,
) -> Option<u8> {
    highest_dimension(b, spent_usd, spent_wall_mins, spent_wakes).map(|(_, pct)| pct)
}

pub fn load_round_budget(root: &Path) -> Result<Option<RoundBudget>> {
    let modes_dir = root.join("coordination/modes");
    let entries = match fs::read_dir(&modes_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取 modes 目录失败: {}", modes_dir.display()))
        }
    };
    let mut yaml_paths = Vec::<PathBuf>::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("读取 modes 目录项失败: {}", modes_dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "yaml") {
            yaml_paths.push(path);
        }
    }
    yaml_paths.sort();
    let Some(path) = yaml_paths.first() else {
        return Ok(None);
    };
    let yaml = fs::read_to_string(path)
        .with_context(|| format!("读取 ModeConfig 失败: {}", path.display()))?;
    parse_mode_config(&yaml).map(Some)
}

/// 模型启动计数的兼容口径：既有执行者意图事件 + planner 的实际 InjectionIssued。
/// 一条批量 InjectionIssued 无论携带多少 reasons 都只计一次。
pub fn count_model_wakes(events: &[EventRecord]) -> u64 {
    events
        .iter()
        .filter(|event| {
            matches!(
                event.kind.as_str(),
                "DispatchIssued"
                    | "NudgeIssued"
                    | "ResumeIssued"
                    | "InjectionIssued"
                    | "WakeIssued"
            )
        })
        .count() as u64
}

fn whole_minutes_between(start: &SystemTime, end: &SystemTime) -> u64 {
    end.duration_since(*start).unwrap_or_default().as_secs() / 60
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveWallBreakdown {
    pub implementation_mins: u64,
    pub review_mins: u64,
}

impl ActiveWallBreakdown {
    pub fn total_mins(self) -> u64 {
        self.implementation_mins.saturating_add(self.review_mins)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ImplementationKey {
    LegacyTask(String),
    Modern { task: String, attempt: String },
}

impl ImplementationKey {
    fn belongs_to(&self, task: &str) -> bool {
        match self {
            Self::LegacyTask(key_task) | Self::Modern { task: key_task, .. } => key_task == task,
        }
    }

    fn is_other_modern_attempt(&self, task: &str, attempt: &str) -> bool {
        matches!(
            self,
            Self::Modern {
                task: key_task,
                attempt: key_attempt,
            } if key_task == task && key_attempt != attempt
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ReviewKey {
    task: String,
    attempt: String,
    role: String,
    agent: String,
}

fn nonempty_payload_string<'a>(event: &'a EventRecord, key: &str) -> Option<&'a str> {
    event
        .payload
        .as_ref()
        .and_then(|payload| payload.get(key))
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
}

fn review_key(event: &EventRecord, task: &str) -> Option<ReviewKey> {
    Some(ReviewKey {
        task: task.to_string(),
        attempt: nonempty_payload_string(event, "attemptId")?.to_string(),
        role: nonempty_payload_string(event, "role")?.to_string(),
        agent: nonempty_payload_string(event, "agent")?.to_string(),
    })
}

fn close_implementation(
    opened: &mut BTreeMap<ImplementationKey, SystemTime>,
    key: &ImplementationKey,
    at: &SystemTime,
    total: &mut u64,
) {
    if let Some(start) = opened.remove(key) {
        *total = total.saturating_add(whole_minutes_between(&start, at));
    }
}

fn close_review(
    opened: &mut BTreeMap<ReviewKey, SystemTime>,
    completed: &mut BTreeSet<ReviewKey>,
    key: &ReviewKey,
    at: &SystemTime,
    total: &mut u64,
) {
    if let Some(start) = opened.remove(key) {
        *total = total.saturating_add(whole_minutes_between(&start, at));
    }
    completed.insert(key.clone());
}

fn close_reviews_matching(
    opened: &mut BTreeMap<ReviewKey, SystemTime>,
    completed: &mut BTreeSet<ReviewKey>,
    at: &SystemTime,
    total: &mut u64,
    mut matches: impl FnMut(&ReviewKey) -> bool,
) {
    let keys = opened
        .keys()
        .filter(|key| matches(key))
        .cloned()
        .collect::<Vec<_>>();
    for key in keys {
        close_review(opened, completed, &key, at, total);
    }
}

/// 按账本顺序累加 implementation 与 exact review 的 active wall 分钟。
///
/// 时间仅取自事件与显式 `now_rfc3339`。窗口按完整 stage identity 独立求和，
/// 因此并行 task/reviewer 各自消耗 model-minutes，阶段之间没有 active owner 的空档不计。
pub fn active_stage_wall_mins(events: &[EventRecord], now_rfc3339: &str) -> ActiveWallBreakdown {
    let now = humantime::parse_rfc3339(now_rfc3339).ok();
    let mut implementation_open = BTreeMap::<ImplementationKey, SystemTime>::new();
    let mut review_open = BTreeMap::<ReviewKey, SystemTime>::new();
    let mut completed_reviews = BTreeSet::<ReviewKey>::new();
    let mut completed_review_attempts = BTreeSet::<(String, String)>::new();
    let mut recorded_tasks = BTreeSet::<String>::new();
    let mut implementation_mins = 0_u64;
    let mut review_mins = 0_u64;

    for event in events {
        let Some(task) = event.task_id.as_deref().filter(|task| !task.is_empty()) else {
            continue;
        };
        let Ok(at) = humantime::parse_rfc3339(&event.ts) else {
            continue;
        };
        let attempt = nonempty_payload_string(event, "attemptId");

        match event.kind.as_str() {
            "DispatchIssued" | "NudgeIssued" | "ResumeIssued" => {
                if recorded_tasks.contains(task) {
                    continue;
                }
                let key = if let Some(attempt) = attempt {
                    if event.kind == "DispatchIssued" {
                        let stale = implementation_open
                            .keys()
                            .filter(|key| key.is_other_modern_attempt(task, attempt))
                            .cloned()
                            .collect::<Vec<_>>();
                        for key in stale {
                            close_implementation(
                                &mut implementation_open,
                                &key,
                                &at,
                                &mut implementation_mins,
                            );
                        }
                    }
                    ImplementationKey::Modern {
                        task: task.to_string(),
                        attempt: attempt.to_string(),
                    }
                } else {
                    ImplementationKey::LegacyTask(task.to_string())
                };
                implementation_open.entry(key).or_insert(at);
            }
            "ReportObserved" => {
                if let Some(attempt) = attempt {
                    close_implementation(
                        &mut implementation_open,
                        &ImplementationKey::Modern {
                            task: task.to_string(),
                            attempt: attempt.to_string(),
                        },
                        &at,
                        &mut implementation_mins,
                    );
                }
            }
            "AttemptBlocked" | "AttemptCrashed" | "AttemptTimedOut" | "AttemptFailed" => {
                if let Some(attempt) = attempt {
                    close_implementation(
                        &mut implementation_open,
                        &ImplementationKey::Modern {
                            task: task.to_string(),
                            attempt: attempt.to_string(),
                        },
                        &at,
                        &mut implementation_mins,
                    );
                    close_reviews_matching(
                        &mut review_open,
                        &mut completed_reviews,
                        &at,
                        &mut review_mins,
                        |key| key.task == task && key.attempt == attempt,
                    );
                } else {
                    close_implementation(
                        &mut implementation_open,
                        &ImplementationKey::LegacyTask(task.to_string()),
                        &at,
                        &mut implementation_mins,
                    );
                }
            }
            "ReviewRequested" => {
                let Some(key) = review_key(event, task) else {
                    continue;
                };
                if recorded_tasks.contains(task)
                    || completed_reviews.contains(&key)
                    || completed_review_attempts.contains(&(key.task.clone(), key.attempt.clone()))
                {
                    continue;
                }
                review_open.entry(key).or_insert(at);
            }
            "ReviewDelivered" => {
                let Some(key) = review_key(event, task) else {
                    continue;
                };
                close_review(
                    &mut review_open,
                    &mut completed_reviews,
                    &key,
                    &at,
                    &mut review_mins,
                );
            }
            "VerdictIssued" => {
                let Some(attempt) = attempt else {
                    continue;
                };
                close_implementation(
                    &mut implementation_open,
                    &ImplementationKey::Modern {
                        task: task.to_string(),
                        attempt: attempt.to_string(),
                    },
                    &at,
                    &mut implementation_mins,
                );
                close_reviews_matching(
                    &mut review_open,
                    &mut completed_reviews,
                    &at,
                    &mut review_mins,
                    |key| key.task == task && key.attempt == attempt,
                );
                completed_review_attempts.insert((task.to_string(), attempt.to_string()));
            }
            "TaskRecorded" => {
                let implementation_keys = implementation_open
                    .keys()
                    .filter(|key| key.belongs_to(task))
                    .cloned()
                    .collect::<Vec<_>>();
                for key in implementation_keys {
                    close_implementation(
                        &mut implementation_open,
                        &key,
                        &at,
                        &mut implementation_mins,
                    );
                }
                close_reviews_matching(
                    &mut review_open,
                    &mut completed_reviews,
                    &at,
                    &mut review_mins,
                    |key| key.task == task,
                );
                recorded_tasks.insert(task.to_string());
            }
            _ => {}
        }
    }

    if let Some(now) = now {
        for start in implementation_open.values() {
            implementation_mins =
                implementation_mins.saturating_add(whole_minutes_between(start, &now));
        }
        for start in review_open.values() {
            review_mins = review_mins.saturating_add(whole_minutes_between(start, &now));
        }
    }

    ActiveWallBreakdown {
        implementation_mins,
        review_mins,
    }
}

/// 兼容入口：预算门与既有调用方统一消费 stage-aware breakdown 的 conservative sum。
pub fn active_attempt_wall_mins(events: &[EventRecord], now_rfc3339: &str) -> u64 {
    active_stage_wall_mins(events, now_rfc3339).total_mins()
}

fn reservation_dir(root: &Path) -> PathBuf {
    root.join(MODEL_WAKE_RESERVATIONS_REL)
}

fn reservation_path(root: &Path, reservation_id: &str) -> PathBuf {
    reservation_dir(root).join(format!("{reservation_id}.json"))
}

fn reservation_id_from_event(event: &EventRecord) -> Option<&str> {
    event
        .extra
        .get("modelWakeReservationId")
        .and_then(|value| value.as_str())
}

fn reservation_age(created_at: &str) -> Option<Duration> {
    let created = humantime::parse_rfc3339(created_at).ok()?;
    SystemTime::now().duration_since(created).ok()
}

fn active_model_wake_reservations(
    root: &Path,
    round: &str,
    all_events: &[EventRecord],
    epoch: &[EventRecord],
    epoch_key: &str,
) -> Result<u64> {
    let fulfilled = epoch
        .iter()
        .filter_map(reservation_id_from_event)
        .collect::<HashSet<_>>();
    let epoch_boundary = all_events
        .iter()
        .rfind(|event| event.kind == "RoundClosed")
        .and_then(|event| humantime::parse_rfc3339(&event.ts).ok());
    let dir = reservation_dir(root);
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("读取模型唤醒 reservation 目录失败: {}", dir.display()))
        }
    };
    let mut active = 0_u64;
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        let reservation = match serde_json::from_str::<ModelWakeReservation>(&text) {
            Ok(reservation) => reservation,
            Err(_) => {
                let old = fs::metadata(&path)
                    .and_then(|metadata| metadata.modified())
                    .ok()
                    .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                    .is_some_and(|age| age >= Duration::from_secs(MODEL_WAKE_RESERVATION_TTL_SECS));
                if old {
                    fs::remove_file(&path).ok();
                } else {
                    // 无法解析的近期 reservation 也保守占一个名额，不能借半写绕过预算。
                    active = active.saturating_add(1);
                }
                continue;
            }
        };
        if fulfilled.contains(reservation.reservation_id.as_str()) {
            fs::remove_file(&path).ok();
            continue;
        }
        if reservation.wake_id.is_none() && !process_alive(reservation.owner_pid) {
            // CLI dispatch/runtask 在 DispatchIssued 前失败时不会启动模型；进程退出即证明
            // 这份未绑定 reservation 可安全回收，不必楔住最后名额 35 分钟。
            fs::remove_file(&path).ok();
            continue;
        }
        let created = humantime::parse_rfc3339(&reservation.created_at).ok();
        let before_epoch = match reservation.epoch_key.as_deref() {
            Some(reservation_epoch) => reservation_epoch != epoch_key,
            None => epoch_boundary
                .zip(created)
                .is_some_and(|(boundary, created)| created <= boundary),
        };
        let expired = reservation_age(&reservation.created_at)
            .is_some_and(|age| age >= Duration::from_secs(MODEL_WAKE_RESERVATION_TTL_SECS));
        if before_epoch || expired {
            fs::remove_file(&path).ok();
            continue;
        }
        if reservation.round == round {
            active = active.saturating_add(1);
        }
    }
    Ok(active)
}

fn create_model_wake_reservation(
    root: &Path,
    round: &str,
    epoch_key: &str,
) -> Result<ModelWakePermit> {
    let reservation_id = ulid::Ulid::new().to_string();
    let reservation = ModelWakeReservation {
        reservation_id: reservation_id.clone(),
        round: round.to_string(),
        owner_pid: std::process::id(),
        created_at: humantime::format_rfc3339_seconds(SystemTime::now()).to_string(),
        wake_id: None,
        epoch_key: Some(epoch_key.to_string()),
    };
    let dir = reservation_dir(root);
    fs::create_dir_all(&dir)?;
    let candidate = dir.join(format!(
        ".candidate-{}-{}.json",
        std::process::id(),
        ulid::Ulid::new()
    ));
    let final_path = reservation_path(root, &reservation_id);
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&candidate)?;
    serde_json::to_writer_pretty(&mut file, &reservation)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&candidate, &final_path).with_context(|| {
        format!(
            "原子发布模型唤醒 reservation 失败: {} → {}",
            candidate.display(),
            final_path.display()
        )
    })?;
    CURRENT_MODEL_WAKE_RESERVATION.with(|current| {
        *current.borrow_mut() = Some(reservation_id.clone());
    });
    Ok(ModelWakePermit {
        reservation_id: Some(reservation_id),
        root: Some(root.to_path_buf()),
        committed: false,
    })
}

pub(crate) fn attach_current_model_wake_reservation(
    kind: &str,
    extra: &mut serde_json::Map<String, serde_json::Value>,
) {
    if !matches!(
        kind,
        "DispatchIssued" | "NudgeIssued" | "ResumeIssued" | "InjectionIssued" | "WakeIssued"
    ) {
        return;
    }
    let reservation = CURRENT_MODEL_WAKE_RESERVATION.with(|current| current.borrow_mut().take());
    if let Some(reservation_id) = reservation {
        extra.insert(
            "modelWakeReservationId".to_string(),
            serde_json::Value::String(reservation_id),
        );
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn bind_model_wake_reservation(
    root: &Path,
    permit: &ModelWakePermit,
    wake_id: &str,
) -> Result<()> {
    let Some(reservation_id) = permit.reservation_id() else {
        return Ok(());
    };
    let path = reservation_path(root, reservation_id);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("读取待绑定模型唤醒 reservation 失败: {}", path.display()))?;
    let mut reservation: ModelWakeReservation = serde_json::from_str(&text)?;
    reservation.wake_id = Some(wake_id.to_string());
    let candidate = path.with_extension(format!("json.bind-{}", ulid::Ulid::new()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&candidate)?;
    serde_json::to_writer_pretty(&mut file, &reservation)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);
    fs::rename(&candidate, &path).with_context(|| {
        format!(
            "原子绑定模型唤醒 reservation 失败: {} → {}",
            candidate.display(),
            path.display()
        )
    })
}

pub fn cancel_model_wake_reservation(root: &Path, permit: &mut ModelWakePermit) -> Result<()> {
    let Some(reservation_id) = permit.reservation_id() else {
        return Ok(());
    };
    CURRENT_MODEL_WAKE_RESERVATION.with(|current| {
        if current.borrow().as_deref() == Some(reservation_id) {
            current.borrow_mut().take();
        }
    });
    let outcome = match fs::remove_file(reservation_path(root, reservation_id)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("取消模型唤醒 reservation 失败"),
    };
    if outcome.is_ok() {
        permit.committed = true;
    }
    outcome
}

pub fn forget_current_model_wake_reservation(permit: &mut ModelWakePermit) {
    permit.commit();
}

/// 在 spawn **之前**判断是否仍有一个名额。max-1 放行最后一次，达到 max 后阻断。
pub fn model_wake_permitted(spent: u64, max: Option<u64>) -> bool {
    max.is_none_or(|limit| spent < limit)
}

/// 返回本轮预算 epoch。
///
/// 关闭轮收到下一轮 NewInstruction 时 CURRENT-ROUND 仍指向旧轮；最后一条 RoundClosed
/// 之前的 USD/wall/wake/阈值都不能永久卡死 bootstrap，因此只看其后的事件。
fn budget_epoch(events: &[EventRecord]) -> (&[EventRecord], Option<&str>) {
    if let Some(closed_index) = events.iter().rposition(|event| event.kind == "RoundClosed") {
        let epoch = &events[closed_index + 1..];
        // 空闲等待不应消耗下一轮 wall 预算；第一个 post-close 事件出现后才起算。
        return (epoch, epoch.first().map(|event| event.ts.as_str()));
    }
    let started = events
        .iter()
        .find(|event| event.kind == "RoundOpened")
        .map(|event| event.ts.as_str());
    (events, started)
}

fn budget_epoch_key(events: &[EventRecord]) -> String {
    events
        .iter()
        .rfind(|event| event.kind == "RoundClosed")
        .or_else(|| events.iter().find(|event| event.kind == "RoundOpened"))
        .map(|event| event.event_id.clone())
        .unwrap_or_else(|| "unanchored".to_string())
}

fn spent_usd(events: &[EventRecord]) -> f64 {
    events
        .iter()
        .filter(|event| event.kind == "VerdictIssued")
        .filter_map(|event| {
            event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("costUsd"))
                .and_then(|value| value.as_f64())
        })
        .sum()
}

pub fn check_before_model_wake(root: &Path, round: &str) -> Result<ModelWakePermit> {
    CURRENT_MODEL_WAKE_RESERVATION.with(|current| {
        current.borrow_mut().take();
    });
    let Some(budget) = load_round_budget(root)? else {
        return Ok(ModelWakePermit::default());
    };
    let lock_path = root.join(MODEL_WAKE_LOCK_REL);
    if let Some(parent) = lock_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let lock_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)?;
    let mut lock = RwLock::new(lock_file);
    let _guard = lock
        .write()
        .context("获取模型唤醒预算 reservation 锁失败")?;

    let ledger_path = root.join(format!("coordination/rounds/{round}/events.jsonl"));
    let ledger_read = read_ledger(&ledger_path)
        .with_context(|| format!("读取预算账本失败: {}", ledger_path.display()))?;
    let (epoch, _) = budget_epoch(&ledger_read.events);
    let epoch_key = budget_epoch_key(&ledger_read.events);
    let spent_usd = spent_usd(epoch);
    let now_rfc3339 = humantime::format_rfc3339(SystemTime::now()).to_string();
    let spent_wall_mins = active_attempt_wall_mins(epoch, &now_rfc3339);
    let spent_wakes = count_model_wakes(epoch).saturating_add(active_model_wake_reservations(
        root,
        round,
        &ledger_read.events,
        epoch,
        &epoch_key,
    )?);

    let highest = if !model_wake_permitted(spent_wakes, budget.max_model_wakes) {
        Some(("wake", 100))
    } else {
        highest_dimension(&budget, spent_usd, spent_wall_mins, spent_wakes)
    };
    let Some((kind, pct)) = highest else {
        return create_model_wake_reservation(root, round, &epoch_key);
    };
    let already_recorded = epoch.iter().any(|event| {
        event.kind == "BudgetThresholdCrossed"
            && event
                .payload
                .as_ref()
                .and_then(|payload| payload.get("pct"))
                .and_then(|value| value.as_u64())
                == Some(pct as u64)
    });
    if !already_recorded {
        ledger::append(
            root,
            round,
            &[ledger::event(
                "BudgetThresholdCrossed",
                "runtime:orch",
                None,
                Some(round),
                serde_json::json!({
                    "kind": kind,
                    "pct": pct,
                    "spent": {
                        "usd": spent_usd,
                        "wallMins": spent_wall_mins,
                        "wakes": spent_wakes,
                    }
                }),
            )],
        )?;
    }

    if pct == 100 {
        bail!("预算耗尽（{pct}%）——暂停派发等用户裁决");
    }
    if pct == 80 {
        eprintln!("警告：轮预算已达到 {pct}%（{kind}）——继续派发");
    }
    create_model_wake_reservation(root, round, &epoch_key)
}

pub fn check_before_dispatch(root: &Path, round: &str) -> Result<()> {
    let mut permit = check_before_model_wake(root, round)?;
    cancel_model_wake_reservation(root, &mut permit)
}

/// 唤醒预算摘要（r30/B57）：把 spent/max 折叠成人可读、机器可比的余量结构。
/// 语义与 `model_wake_permitted`（spent < max 放行、spent >= max 阻断）一致：
/// - remaining = max.map(|m| m.saturating_sub(spent))（无上限 ⇒ None）；
/// - exhausted = max 存在且 spent >= max；
/// - 无上限（max=None）永不 exhausted、remaining=None。
#[derive(Debug, PartialEq)]
pub struct WakeBudgetSummary {
    pub spent: u64,
    pub max: Option<u64>,
    pub remaining: Option<u64>,
    pub exhausted: bool,
}

pub fn wake_budget_summary(spent: u64, max: Option<u64>) -> WakeBudgetSummary {
    let remaining = max.map(|m| m.saturating_sub(spent));
    let exhausted = max.is_some_and(|m| spent >= m);
    WakeBudgetSummary {
        spent,
        max,
        remaining,
        exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stage_event(kind: &str, ts: &str, task: &str, payload: serde_json::Value) -> EventRecord {
        EventRecord {
            event_id: format!("{task}-{kind}-{ts}"),
            ts: ts.to_string(),
            actor: "runtime:test".to_string(),
            kind: kind.to_string(),
            task_id: Some(task.to_string()),
            round: Some("rT".to_string()),
            payload: Some(payload),
            extra: serde_json::Map::new(),
        }
    }

    fn attempt_payload(attempt_id: &str) -> serde_json::Value {
        serde_json::json!({"attemptId": attempt_id})
    }

    fn review_payload(attempt_id: &str, role: &str, agent: &str) -> serde_json::Value {
        serde_json::json!({
            "attemptId": attempt_id,
            "role": role,
            "agent": agent,
        })
    }

    #[test]
    fn worst_threshold_uses_highest_configured_dimension() {
        let budget = RoundBudget {
            max_usd: Some(10.0),
            wall_minutes: Some(100),
            max_model_wakes: Some(10),
        };
        assert_eq!(worst_threshold(&budget, 6.0, 85, 10), Some(100));
    }

    #[test]
    fn missing_round_budget_parses_as_all_none() {
        let budget = parse_mode_config("preset: relay\nunknown: tolerated\n").unwrap();
        assert!(budget.max_usd.is_none());
        assert!(budget.wall_minutes.is_none());
        assert!(budget.max_model_wakes.is_none());
    }

    #[test]
    fn batch_injection_counts_once_and_limit_is_pre_spawn() {
        let events = vec![
            crate::ledger::event(
                "DispatchIssued",
                "runtime:orch",
                Some("B1"),
                Some("rT"),
                serde_json::json!({}),
            ),
            crate::ledger::event(
                "InjectionIssued",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({"reasons": ["a", "b", "c", "d"]}),
            ),
        ];
        assert_eq!(count_model_wakes(&events), 2);
        assert!(model_wake_permitted(1, Some(2)));
        assert!(!model_wake_permitted(2, Some(2)));
    }

    #[test]
    fn closed_round_starts_a_fresh_budget_epoch() {
        let events = vec![
            crate::ledger::event(
                "RoundOpened",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({}),
            ),
            crate::ledger::event(
                "InjectionIssued",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({"reasons": ["all_recorded"]}),
            ),
            crate::ledger::event(
                "RoundClosed",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({}),
            ),
            crate::ledger::event(
                "BudgetThresholdCrossed",
                "runtime:orch",
                None,
                Some("rT"),
                serde_json::json!({"pct": 100}),
            ),
        ];
        let (epoch, _) = budget_epoch(&events);
        assert_eq!(count_model_wakes(epoch), 0);
        assert!(epoch.iter().all(|event| event.kind != "RoundClosed"));
    }

    #[test]
    fn wall_budget_gate_ignores_idle_time_before_first_dispatch() {
        let root =
            std::env::temp_dir().join(format!("orch-budget-active-wall-{}", ulid::Ulid::new()));
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        fs::write(
            root.join("coordination/modes/relay.yaml"),
            "budgets:\n  round:\n    wallMinutes: 1\n",
        )
        .unwrap();
        let opened = EventRecord {
            event_id: "opened".to_string(),
            ts: "2020-01-01T00:00:00Z".to_string(),
            actor: "runtime:orch".to_string(),
            kind: "RoundOpened".to_string(),
            task_id: None,
            round: Some("rT".to_string()),
            payload: None,
            extra: serde_json::Map::new(),
        };
        fs::write(
            root.join("coordination/rounds/rT/events.jsonl"),
            format!("{}\n", serde_json::to_string(&opened).unwrap()),
        )
        .unwrap();

        let permit = check_before_model_wake(&root, "rT")
            .expect("没有 dispatch 的空闲 epoch 不应耗尽 wall 预算");
        assert!(permit.reservation_id().is_some());
        drop(permit);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_wall_handles_redispatch_timeout_nonterminal_and_bad_ts() {
        let event = |kind: &str, ts: &str, task: &str| EventRecord {
            event_id: format!("{task}-{kind}-{ts}"),
            ts: ts.to_string(),
            actor: "runtime:orch".to_string(),
            kind: kind.to_string(),
            task_id: Some(task.to_string()),
            round: Some("rT".to_string()),
            payload: None,
            extra: serde_json::Map::new(),
        };
        let events = vec![
            event("DispatchIssued", "bad-ts", "B2"),
            event("DispatchIssued", "2026-07-24T11:00:00Z", "B1"),
            event("DispatchIssued", "2026-07-24T11:00:00Z", "B2"),
            event("DispatchIssued", "2026-07-24T11:00:00Z", "B3"),
            event("TaskRecorded", "2026-07-24T11:02:00Z", "B2"),
            event("ReportObserved", "2026-07-24T11:05:00Z", "B1"),
            event("AttemptCrashed", "bad-ts", "B3"),
            event("DispatchIssued", "2026-07-24T11:10:00Z", "B1"),
            event("AttemptTimedOut", "2026-07-24T11:25:00Z", "B1"),
        ];
        // B1=10+15，B2=2；B3 的坏终态被跳过，仍 in-flight 到 11:30=30。
        assert_eq!(
            active_attempt_wall_mins(&events, "2026-07-24T11:30:00Z"),
            57
        );
    }

    #[test]
    fn stage_wall_skips_bad_timestamps_and_invalid_now_keeps_closed_windows() {
        let events = vec![
            stage_event(
                "DispatchIssued",
                "bad-open",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T11:00:00Z",
                "B2",
                attempt_payload("B2-A0001"),
            ),
            stage_event(
                "AttemptTimedOut",
                "bad-close",
                "B2",
                attempt_payload("B2-A0001"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T12:00:00Z",
                "B3",
                attempt_payload("B3-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T11:50:00Z",
                "B3",
                attempt_payload("B3-A0001"),
            ),
        ];

        assert_eq!(
            active_stage_wall_mins(&[], "not-now"),
            ActiveWallBreakdown {
                implementation_mins: 0,
                review_mins: 0,
            }
        );
        assert_eq!(
            active_stage_wall_mins(&events, "not-now"),
            ActiveWallBreakdown {
                implementation_mins: 10,
                review_mins: 0,
            },
            "invalid now 只能保留已由合法事件闭合的窗口"
        );
        assert_eq!(
            active_stage_wall_mins(&events, "2026-07-30T12:00:00Z"),
            ActiveWallBreakdown {
                implementation_mins: 70,
                review_mins: 0,
            },
            "坏 close 不得关窗；end-before-start 必须 saturating 为零"
        );
    }

    #[test]
    fn attempt_failure_and_late_old_attempt_events_do_not_touch_redispatch() {
        let events = vec![
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:01:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "AttemptFailed",
                "2026-07-30T10:05:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0002"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:11:00Z",
                "B1",
                review_payload("B1-A0002", "primary", "executor-opencode"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:15:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "VerdictIssued",
                "2026-07-30T10:20:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:30:00Z",
                "B1",
                attempt_payload("B1-A0002"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:31:00Z",
                "B1",
                review_payload("B1-A0002", "primary", "executor-opencode"),
            ),
        ];

        assert_eq!(
            active_stage_wall_mins(&events, "2026-07-30T12:00:00Z"),
            ActiveWallBreakdown {
                implementation_mins: 25,
                review_mins: 24,
            }
        );
    }

    #[test]
    fn verdict_and_task_recorded_are_exact_and_task_wide_fallbacks() {
        let events = vec![
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:00:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:02:00Z",
                "B1",
                review_payload("B1-A0001", "secondary", "executor-opencode"),
            ),
            stage_event(
                "VerdictIssued",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:20:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T11:00:00Z",
                "B2",
                serde_json::json!({}),
            ),
            stage_event(
                "NudgeIssued",
                "2026-07-30T11:01:00Z",
                "B2",
                attempt_payload("B2-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T11:02:00Z",
                "B2",
                review_payload("B2-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "TaskRecorded",
                "2026-07-30T11:06:00Z",
                "B2",
                serde_json::json!({}),
            ),
        ];

        assert_eq!(
            active_stage_wall_mins(&events, "2026-07-30T12:00:00Z"),
            ActiveWallBreakdown {
                implementation_mins: 21,
                review_mins: 22,
            },
            "Verdict closes one attempt; TaskRecorded closes every remaining task window"
        );
    }

    #[test]
    fn blocked_attempt_can_reopen_via_both_nudge_and_resume() {
        let events = vec![
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "AttemptBlocked",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "NudgeIssued",
                "2026-07-30T10:20:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "AttemptBlocked",
                "2026-07-30T10:25:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ResumeIssued",
                "2026-07-30T10:40:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:47:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
        ];

        assert_eq!(
            active_stage_wall_mins(&events, "2026-07-30T12:00:00Z"),
            ActiveWallBreakdown {
                implementation_mins: 22,
                review_mins: 0,
            }
        );
    }

    #[test]
    fn review_delivery_is_exact_permanent_and_first_open_wins() {
        let events = vec![
            stage_event(
                "ReviewDelivered",
                "2026-07-30T09:55:00Z",
                "B1",
                review_payload("B1-A0000", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:00:00Z",
                "B1",
                review_payload("B1-A0000", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:00:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:05:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:06:00Z",
                "B1",
                review_payload("B1-A0001", "secondary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:07:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-opencode"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:08:00Z",
                "B1",
                review_payload("B1-A0002", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:10:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:15:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:20:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
        ];

        assert_eq!(
            active_stage_wall_mins(&events, "2026-07-30T11:00:00Z"),
            ActiveWallBreakdown {
                implementation_mins: 0,
                review_mins: 10,
            }
        );
    }

    #[test]
    fn parallel_tasks_and_reviewers_sum_model_minutes_instead_of_union() {
        let events = vec![
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B2",
                attempt_payload("B2-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:15:00Z",
                "B2",
                attempt_payload("B2-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:20:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:20:00Z",
                "B1",
                review_payload("B1-A0001", "secondary", "executor-opencode"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:25:00Z",
                "B2",
                review_payload("B2-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:30:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:35:00Z",
                "B2",
                review_payload("B2-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:40:00Z",
                "B1",
                review_payload("B1-A0001", "secondary", "executor-opencode"),
            ),
        ];

        let breakdown = active_stage_wall_mins(&events, "2026-07-30T12:00:00Z");
        assert_eq!(
            breakdown,
            ActiveWallBreakdown {
                implementation_mins: 25,
                review_mins: 40,
            }
        );
        assert_eq!(breakdown.total_mins(), 65);
        assert_eq!(
            active_attempt_wall_mins(&events, "2026-07-30T12:00:00Z"),
            breakdown.total_mins(),
            "兼容入口必须只委托 breakdown conservative sum"
        );
    }

    #[test]
    fn model_wake_gate_consumes_stage_breakdown_total() {
        let root =
            std::env::temp_dir().join(format!("orch-budget-stage-gate-{}", ulid::Ulid::new()));
        fs::create_dir_all(root.join("coordination/modes")).unwrap();
        fs::create_dir_all(root.join("coordination/rounds/rT")).unwrap();
        fs::write(
            root.join("coordination/modes/relay.yaml"),
            "budgets:\n  round:\n    wallMinutes: 40\n",
        )
        .unwrap();

        let mut opened = stage_event(
            "RoundOpened",
            "2026-07-30T09:00:00Z",
            "",
            serde_json::json!({}),
        );
        opened.task_id = None;
        let events = vec![
            opened,
            stage_event(
                "DispatchIssued",
                "2026-07-30T10:00:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReportObserved",
                "2026-07-30T10:10:00Z",
                "B1",
                attempt_payload("B1-A0001"),
            ),
            stage_event(
                "ReviewRequested",
                "2026-07-30T10:20:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
            stage_event(
                "ReviewDelivered",
                "2026-07-30T10:55:00Z",
                "B1",
                review_payload("B1-A0001", "primary", "executor-claw"),
            ),
        ];
        let ledger = events
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(root.join("coordination/rounds/rT/events.jsonl"), ledger).unwrap();

        let error = check_before_model_wake(&root, "rT")
            .expect_err("implementation=10 + review=35 必须触发 wall 100%");
        assert!(error.to_string().contains("预算耗尽（100%）"));
        let measured = read_ledger(&root.join("coordination/rounds/rT/events.jsonl")).unwrap();
        let crossed = measured
            .events
            .iter()
            .find(|event| event.kind == "BudgetThresholdCrossed")
            .expect("真实 gate 必须落 wall threshold 事实");
        assert_eq!(
            crossed
                .payload
                .as_ref()
                .and_then(|payload| payload.pointer("/spent/wallMins"))
                .and_then(|value| value.as_u64()),
            Some(45)
        );

        fs::remove_dir_all(root).unwrap();
    }
}
