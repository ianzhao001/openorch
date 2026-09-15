//! orch-ui: 只读 TUI 面板（r45/B103）。
//!
//! 纪律：
//! - `render` 是纯函数，不碰终端、不做 IO；只往 `ratatui::buffer::Buffer` 里写。
//! - **纯只读铁律**：`key_bindings` 返回的每个动作 `is_read_only()` 必须为真，
//!   面板内不得有任何会触发 orch 写操作（dispatch/verify/merge/close/nudge/retry）的入口。
//! - 活动流必须 redact 后再上屏（纵深防御：即便上游漏网也不得把密文画到屏幕）。
//! - 极窄/极矮尺寸必须截断而不 panic；`buf.area` 的宽高必须与请求一致。
//! - `critical` 告警必须与 `info` 视觉可区分（样式不同，不能只靠排序）。
//! - `source == Frontend`（前端自算）或快照陈旧时必须显式标注。
//! - **未知时间 fail-closed**：`generated_at` 解析失败或系统时间不可用时必须标「时间未知」，
//!   绝不把「不知道」折叠成「新鲜」（只读面板最危险的说谎形态）。
//! - **应用层薄壳**（本卡 §2）：刷新调度（fs 事件闹钟 + 2s 定时重扫兜底）、告警去抖、
//!   `copy_to_clipboard`（`pbcopy` 只复制不执行）。`orch tui` CLI 入口仍归 planner 合并后接线。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

use orch_host::redact;
use orch_host::snapshot::{Alert, OrchSnapshot, Severity, SnapshotSource};

/// Independent invocation-observation application; legacy snapshot assets remain available.
pub mod app;

/// Local Web observation service, isolated from default CLI dependencies.
pub mod web;

/// 只读动作集合：TUI 面板能触发的动作。**全部只读**——`is_read_only()` 恒真。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// 手动刷新快照（读 events/heartbeats，不改任何状态）。
    Refresh,
    /// 复制当前选中告警的 `suggestedCommand` 到剪贴板（只读：shell-out `pbcopy`，不执行该命令）。
    CopyCommand,
    /// 退出 TUI。
    Quit,
}

impl Action {
    /// 铁律：所有 TUI 动作必须只读。返回 `true`——任何写动作都不得出现在 `key_bindings`。
    pub fn is_read_only(&self) -> bool {
        match self {
            Action::Refresh | Action::CopyCommand | Action::Quit => true,
        }
    }
}

/// 按键绑定：`(键, 动作)`。全部只读。
pub fn key_bindings() -> Vec<(char, Action)> {
    vec![
        ('r', Action::Refresh),
        ('y', Action::CopyCommand),
        ('q', Action::Quit),
    ]
}

/// 陈旧阈值（秒）：`generatedAt` 比当前时间老于此值即标「陈旧」。
/// 测试快照用 `2026-07-26T00:00:00Z`，本机现在远大于此 → 必然陈旧。
/// 但本函数是纯渲染，不做时间判断——只要 `source==Frontend` 或快照的
/// `generated_at` 解析为「过去超过阈值」就标陈旧。种子快照固定为 2026-07-26，
/// 在任何晚于此的真实运行里都算陈旧；为了契约可重复，陈旧判断同时纳入
/// `source==Frontend`（一定标）与「generated_at 解析失败或远早于 now」。
const STALE_THRESHOLD_SECS: u64 = 300;

/// 把 ISO8601 时间字符串解析为相对 Unix 秒；失败返回 None。
fn parse_iso8601_to_epoch(s: &str) -> Option<u64> {
    // 接受 `YYYY-MM-DDTHH:MM:SSZ`。不依赖 chrono（不新增依赖）。
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let bytes = s.as_bytes();
    let ok_digit = |b: u8| b.is_ascii_digit();
    // 年 4 / 月 2 / 日 2 / 时 2 / 分 2 / 秒 2
    if !(ok_digit(bytes[0]) && ok_digit(bytes[1]) && ok_digit(bytes[2]) && ok_digit(bytes[3])) {
        return None;
    }
    if bytes[4] != b'-' {
        return None;
    }
    if !(ok_digit(bytes[5]) && ok_digit(bytes[6])) {
        return None;
    }
    if bytes[7] != b'-' {
        return None;
    }
    if !(ok_digit(bytes[8]) && ok_digit(bytes[9])) {
        return None;
    }
    if bytes[10] != b'T' {
        return None;
    }
    if !(ok_digit(bytes[11]) && ok_digit(bytes[12])) {
        return None;
    }
    if bytes[13] != b':' {
        return None;
    }
    if !(ok_digit(bytes[14]) && ok_digit(bytes[15])) {
        return None;
    }
    if bytes[16] != b':' {
        return None;
    }
    if !(ok_digit(bytes[17]) && ok_digit(bytes[18])) {
        return None;
    }
    let year: u64 = s[0..4].parse().ok()?;
    let month: u64 = s[5..7].parse().ok()?;
    let day: u64 = s[8..10].parse().ok()?;
    let hour: u64 = s[11..13].parse().ok()?;
    let minute: u64 = s[14..16].parse().ok()?;
    let second: u64 = s[17..19].parse().ok()?;
    // 朴素公历→Unix 秒（忽略闰秒；只用于「陈旧」判断，误差可接受）。
    let days = days_since_epoch(year, month, day)?;
    Some(days * 86400 + hour * 3600 + minute * 60 + second)
}

/// 朴素公历→Unix 天。算法：Howard Hinnant 的 `days_from_civil`。
fn days_since_epoch(y: u64, m: u64, d: u64) -> Option<u64> {
    if !(1..=12).contains(&m) {
        return None;
    }
    if !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = y / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146097 + doe - 719468)
}

/// now() 的 Unix 秒。系统时间不可用时返回 None（→ 视为未知时间，fail-closed 标注）。
fn now_epoch() -> Option<u64> {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// 陈旧/未知时间的标注形态。**fail-closed 铁律**：
/// 「不知道」必须折叠成「可疑/未知」而非「新鲜」——只读面板不得把来源不明的
/// 快照无标记地显示成正常当前真值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleMark {
    /// 已解析且未超阈值：确为新鲜，无需标注。
    Fresh,
    /// `source==Frontend`（前端自算，投影非真值）——一律标注。
    Frontend,
    /// 已解析且超阈值——陈旧。
    Stale,
    /// **未知**：`generated_at` 解析失败或系统时间不可用。必须标注，不得显示为正常快照。
    Unknown,
}

impl StaleMark {
    /// 渲染用的屏幕标注字符串（空串表示不标注）。
    pub fn label(self) -> &'static str {
        match self {
            StaleMark::Fresh => "",
            StaleMark::Frontend => "[frontend 自算]",
            StaleMark::Stale => "[陈旧]",
            StaleMark::Unknown => "[时间未知]",
        }
    }
}

/// 判定快照需要何种标注。**fail-closed**：无法判定新鲜度时返回 `Unknown`，
/// 绝不把「不知道」折叠成「新鲜」。
pub fn needs_stale_mark(snap: &OrchSnapshot) -> StaleMark {
    if matches!(snap.source, SnapshotSource::Frontend) {
        return StaleMark::Frontend;
    }
    let Some(gen) = parse_iso8601_to_epoch(&snap.generated_at) else {
        return StaleMark::Unknown;
    };
    let Some(now) = now_epoch() else {
        return StaleMark::Unknown;
    };
    if now.saturating_sub(gen) > STALE_THRESHOLD_SECS {
        StaleMark::Stale
    } else {
        StaleMark::Fresh
    }
}

/// 把字符串按字符数截断到 `max_chars`，保证不溢出给定宽度（按字符计）。
/// 注意：这是按字符数截断，不是显示宽度；但 `set_string` 内部会再做裁剪，
/// 我们这里额外截断是为了在极窄（如 width=1）下也绝不出错。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    s.chars().take(max_chars).collect()
}

/// 把 `line` 以 `style` 写到 `buf` 的第 `y` 行，从 `x0` 开始，
/// 但严格不超出 `buf.area.width`：先按可用列数截断字符串，再写。
/// 越界（y>=height 或 x0>=width）时安全地什么都不做。
fn write_line(buf: &mut Buffer, x0: u16, y: u16, line: &str, style: Style) {
    let area = buf.area;
    if y >= area.height {
        return;
    }
    if x0 >= area.width {
        return;
    }
    let avail = (area.width - x0) as usize;
    let truncated = truncate_chars(line, avail);
    // set_string 内部还会按区域裁剪，这里先截断是为了防御性，避免在
    // 极窄尺寸下任何越界路径触发 panic。
    buf.set_string(x0, y, truncated, style);
}

/// severity→(前缀标签, 样式)。critical 必须与 info 视觉可区分。
fn severity_style(sev: Severity) -> ( &'static str, Style) {
    match sev {
        Severity::Critical => (
            "D",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        Severity::Warn => (
            "W",
            Style::default().fg(Color::Yellow),
        ),
        Severity::Info => (
            "i",
            Style::default().fg(Color::DarkGray),
        ),
    }
}

/// `render(&OrchSnapshot, width, height) -> Buffer`——纯函数，不碰终端、不做 IO。
///
/// 五个区：agents / tasks / round / alerts / activity。
/// - critical 与 info 必须样式不同（critical=Red+BOLD，info=DarkGray）。
/// - `source==Frontend` 或陈旧时显式标注。
/// - 活动流必须 redact 后再上屏。
/// - 极窄/极矮尺寸截断而不 panic；`buf.area` 与请求一致。
pub fn render(snap: &OrchSnapshot, width: u16, height: u16) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);

    let mut y: u16 = 0;
    let max_y = height;

    // ── 顶栏：round + source/陈旧标记 ──
    if max_y > 0 {
        // round_id 本身已含 `r` 前缀（如 "r45"），不要再额外补 `r`，否则会出现 `orch rr45`。
        let mark = needs_stale_mark(snap);
        let round_line = format!(
            "orch {}  src={:?}  {}",
            snap.round.round_id,
            snap.source,
            mark.label()
        );
        write_line(&mut buf, 0, y, &round_line, Style::default());
        y = y.saturating_add(1);
    }

    // ── alerts（分级；critical 必须与 info 视觉不同）──
    if y < max_y {
        write_line(&mut buf, 0, y, "alerts:", Style::default().add_modifier(Modifier::BOLD));
        y = y.saturating_add(1);
    }
    for a in &snap.alerts {
        if y >= max_y {
            break;
        }
        let (prefix, style) = severity_style(a.severity);
        // 行内容：前缀字符 + 空格 + subject/message。前缀字符保证首个可被
        // 种子按 `cell.symbol().starts_with(needle)` 找到并取到对应 severity 的样式。
        let content = format!("{prefix} {} {}", a.subject, a.message);
        write_line(&mut buf, 0, y, &content, style);
        y = y.saturating_add(1);
    }

    // ── activity（redact 后再上屏）──
    if y < max_y {
        write_line(&mut buf, 0, y, "activity:", Style::default().add_modifier(Modifier::BOLD));
        y = y.saturating_add(1);
    }
    for line in &snap.activity {
        if y >= max_y {
            break;
        }
        // 纵深防御：即便上游漏网也把密文 redact 掉。
        let safe_summary = redact::redact_full(&line.summary);
        let safe_agent = redact::redact_full(&line.agent);
        let safe_kind = redact::redact_full(&line.kind);
        let row = format!("{} {} {} {}", line.ts, safe_agent, safe_kind, safe_summary);
        write_line(&mut buf, 0, y, &row, Style::default());
        y = y.saturating_add(1);
    }

    // ── agents（空闲/忙碌/假死一览）──
    if y < max_y {
        write_line(&mut buf, 0, y, "agents:", Style::default().add_modifier(Modifier::BOLD));
        y = y.saturating_add(1);
    }
    for ag in &snap.agents {
        if y >= max_y {
            break;
        }
        let row = format!(
            "{:?} {} task={:?} model={:?}",
            ag.state, ag.agent_id, ag.current_task, ag.model_declared
        );
        write_line(&mut buf, 0, y, &row, Style::default());
        y = y.saturating_add(1);
    }

    // ── tasks（8 态 + 门 + 成本）──
    if y < max_y {
        write_line(&mut buf, 0, y, "tasks:", Style::default().add_modifier(Modifier::BOLD));
        y = y.saturating_add(1);
    }
    for t in &snap.tasks {
        if y >= max_y {
            break;
        }
        let row = format!(
            "{} {} evts={} g={}/{} cost={:?}",
            t.task_id, t.state, t.event_count, t.gates_green, t.gates_red, t.cost_usd
        );
        write_line(&mut buf, 0, y, &row, Style::default());
        y = y.saturating_add(1);
    }

    // ── round 预算三维 ──
    if y < max_y {
        let row = format!(
            "round evts={} closed={} usd={:?}/{:?} wall={:?}/{:?} wakes={:?}/{:?}",
            snap.round.events,
            snap.round.closed,
            snap.round.budget.usd.spent,
            snap.round.budget.usd.max,
            snap.round.budget.wall_minutes.spent,
            snap.round.budget.wall_minutes.max,
            snap.round.budget.model_wakes.spent,
            snap.round.budget.model_wakes.max,
        );
        write_line(&mut buf, 0, y, &row, Style::default());
        // last section: no further `y` read after this — saturating_add elided to avoid unused_assignments warning
    }

    // 确保 buf.area 与请求一致（Buffer::empty 已保证，这里只做断言式自检）。
    debug_assert_eq!(buf.area.width, width);
    debug_assert_eq!(buf.area.height, height);

    // 抑制未使用的 Line 引入（保留以备后续 bin 接线，但当前纯函数路径不用）。
    let _ = Line::from("");

    buf
}

// ════════════════════════════════════════════════════════════════════════
// 应用层薄壳（任务卡 §2，本轮定向修复补齐）
//
// 纪律：这些是**可测的 pub 函数/结构**，不要求真起终端。`orch tui` 的 CLI 入口
// 仍归 planner 合并后接线（不在本卡 writeSet 内的 orch-cli）。
// ════════════════════════════════════════════════════════════════════════

/// 刷新触发源：fs 事件闹钟 + 2s 定时重扫兜底的合成结果。
///
/// 沿用 `orch_host::run_await` 的约定：**事件只是闹钟，扫描才是保证**。
/// 即便 fs 事件丢失或节流，2s 定时器也会兜底重扫——保证面板最终一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshTrigger {
    /// fs 事件（FSEvents 监听 events.jsonl/heartbeats/logs）触发的闹钟。
    FsEvent,
    /// 2s 定时器触发的兜底重扫。
    Timer,
    /// 用户按 `r` 手动刷新。
    Manual,
}

impl RefreshTrigger {
    /// 不论触发源如何，**都要执行一次扫描**（事件只是闹钟，扫描是保证）。
    /// 返回 true——调用方据此决定是否真的重读快照。
    pub fn should_scan(self) -> bool {
        // 三种触发源一律触发扫描：这是 fail-safe 设计。
        matches!(self, RefreshTrigger::FsEvent | RefreshTrigger::Timer | RefreshTrigger::Manual)
    }
}

/// 刷新调度器状态：合成 fs 事件闹钟 + 2s 定时兜底重扫 + 手动刷新。
///
/// `next_refresh_due`：相对于某基准时刻，下一次应当扫描的时间点（秒）。
/// 设计为可单测的纯函数式决策——不持有真实定时器，由调用方（planner 接的 bin）轮询。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSchedule {
    /// 2s 定时重扫兜底间隔（秒）。
    pub timer_interval_secs: u64,
    /// 自上一次扫描起，距离下次定时兜底重扫的秒数。
    pub secs_since_last_scan: u64,
    /// 是否有未消费的 fs 事件闹钟。
    pub pending_fs_alarm: bool,
    /// 是否有未消费的手动刷新请求。
    pub pending_manual: bool,
}

impl RefreshSchedule {
    /// 构造一个默认调度状态（2s 兜底、无积压闹钟）。
    pub fn new() -> Self {
        RefreshSchedule {
            timer_interval_secs: 2,
            secs_since_last_scan: 0,
            pending_fs_alarm: false,
            pending_manual: false,
        }
    }

    /// 判定此刻是否应当扫描。**fail-safe**：任一触发源就绪即扫。
    /// - fs 事件闹钟就绪 → 扫
    /// - 手动刷新就绪 → 扫
    /// - 距上次扫描已过 `timer_interval_secs` → 扫（兜底，即便没事件）
    pub fn trigger(&self) -> Option<RefreshTrigger> {
        if self.pending_manual {
            return Some(RefreshTrigger::Manual);
        }
        if self.pending_fs_alarm {
            return Some(RefreshTrigger::FsEvent);
        }
        if self.secs_since_last_scan >= self.timer_interval_secs {
            return Some(RefreshTrigger::Timer);
        }
        None
    }

    /// 记录一次 fs 事件闹钟到达。
    pub fn note_fs_alarm(&mut self) {
        self.pending_fs_alarm = true;
    }

    /// 记录一次手动刷新请求。
    pub fn note_manual(&mut self) {
        self.pending_manual = true;
    }

    /// 记录时间流逝（秒）。不会让 `secs_since_last_scan` 溢出。
    pub fn tick(&mut self, secs: u64) {
        self.secs_since_last_scan = self.secs_since_last_scan.saturating_add(secs);
    }

    /// 标记一次扫描已完成：清空闹钟与手动请求，重置计时。
    pub fn mark_scanned(&mut self) {
        self.pending_fs_alarm = false;
        self.pending_manual = false;
        self.secs_since_last_scan = 0;
    }
}

impl Default for RefreshSchedule {
    fn default() -> Self {
        Self::new()
    }
}

/// 告警去抖键：同一告警身份。相同身份在冷却窗内不重复通知。
///
/// 「身份」= `(kind, subject, severity)`：只有这三者全同才算「同一告警重复」。
/// 不同 severity 的同 subject 视为不同告警（Critical 升级应当重新通知）。
///
/// 三个字段独立存储为结构化三元组，**绝不**用分隔符拼接成单一字符串：
/// `kind`/`subject` 可能含任意字符（包括分隔符 `|`），拼接会令不同三元组碰撞，
/// 导致本应通知的新告警被误判为冷却窗内的重复而被静默抑制（复审 P1-1）。
/// 结构化字段直接由 `derive(Hash, Eq)` 按三元组逐字段比较，无歧义。
///
/// 注：`Severity` 来自 `orch-host`（冻结，未实现 `Hash`），故存其判别值
/// `severity_tag: &'static str`（每个变体映射到唯一静态字面量）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AlertIdentity {
    kind: String,
    subject: String,
    severity_tag: &'static str,
}

impl AlertIdentity {
    /// 从 `Alert` 抽取去抖身份。
    pub fn of(alert: &Alert) -> Self {
        AlertIdentity {
            kind: alert.kind.clone(),
            subject: alert.subject.clone(),
            severity_tag: severity_tag(alert.severity),
        }
    }

    /// 用于断言/诊断：返回 `(kind, subject, severity_tag)` 三元组，无歧义。
    pub fn as_tuple(&self) -> (&str, &str, &'static str) {
        (&self.kind, &self.subject, self.severity_tag)
    }
}

/// `Severity` → 唯一静态判别标签（`orch-host::Severity` 未实现 `Hash`，冻结不可改）。
/// 每个变体映射到互不相等的字面量，供 `AlertIdentity` 直接做 `Hash`/`Eq` 的第三字段。
fn severity_tag(sev: Severity) -> &'static str {
    match sev {
        Severity::Critical => "Critical",
        Severity::Warn => "Warn",
        Severity::Info => "Info",
    }
}

/// 告警通知去抖器：同一 `AlertIdentity` 在冷却窗内不重复触发 `notify_macos`。
///
/// 设计为可单测的纯函数式决策——不真调 `notify_macos`，由调用方在 `should_notify`
/// 返回 true 时自行 shell-out。
#[derive(Debug, Clone)]
pub struct AlertDebouncer {
    /// 冷却窗（秒）。
    pub cooldown_secs: u64,
    /// 已通知过的告警身份 → 上次通知时的「时间戳」（秒，外部基准）。
    notified: std::collections::HashMap<AlertIdentity, u64>,
}

impl AlertDebouncer {
    /// 构造一个空去抖器，冷却窗默认 300s（5 分钟）。
    pub fn new() -> Self {
        AlertDebouncer {
            cooldown_secs: 300,
            notified: std::collections::HashMap::new(),
        }
    }

    /// 构造指定冷却窗的去抖器。
    pub fn with_cooldown(cooldown_secs: u64) -> Self {
        AlertDebouncer {
            cooldown_secs,
            notified: std::collections::HashMap::new(),
        }
    }

    /// 判定该告警此刻是否应当通知。
    ///
    /// - 从未通知过 → 应当通知
    /// - 上次通知距今超过冷却窗 → 应当通知
    /// - 在冷却窗内已通知过 → 不重复通知（去抖）
    ///
    /// `now_secs`：外部时间基准（调用方提供，便于单测；生产用 `SystemTime::now`）。
    /// 返回 true 时**同时记录**本次通知（调用方据此 shell-out `notify_macos`）。
    pub fn should_notify(&mut self, alert: &Alert, now_secs: u64) -> bool {
        let id = AlertIdentity::of(alert);
        if let Some(&last) = self.notified.get(&id) {
            if now_secs.saturating_sub(last) < self.cooldown_secs {
                return false;
            }
        }
        self.notified.insert(id, now_secs);
        true
    }

    /// 清空去抖历史（用于轮次切换等需要重新通知的场景）。
    pub fn reset(&mut self) {
        self.notified.clear();
    }
}

impl Default for AlertDebouncer {
    fn default() -> Self {
        Self::new()
    }
}

/// 剪贴板写入：走 `pbcopy` shell-out，**只复制不执行**。
///
/// `text` 被原样写入剪贴板，**绝不**作为 shell 命令执行——这是只读面板的铁律：
/// `suggestedCommand` 只复制到剪贴板供用户自行决定是否粘贴执行。
///
/// 返回 `Ok(())` 当 `pbcopy` 成功；返回 `Err` 当进程启动失败或非零退出
/// （调用方 best-effort，失败不阻断）。
pub fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("pbcopy spawn failed: {e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    let status = child
        .wait()
        .map_err(|e| format!("pbcopy wait failed: {e}"))?;
    if !status.success() {
        return Err(format!("pbcopy exit: {status}"));
    }
    Ok(())
}

/// 断言式自检：`Action` 枚举里**不存在**任何能触发 orch 写命令的变体。
///
/// 写命令 = dispatch / verify / merge / close / nudge / retry。
/// 这是只读铁律的静态表达——通过遍历所有 `key_bindings()` 返回的按键，
/// 确认没有写动作潜入。测试覆盖此函数。
pub fn assert_no_write_actions_in_bindings(bindings: &[(char, Action)]) -> bool {
    bindings.iter().all(|(_, a)| a.is_read_only())
}

// 防御：测试里禁止用 std::env::temp_dir()（指向 /tmp，会被 Tier F 沙箱 auto-reject）。
// 本 crate 的测试一律用 CARGO_MANIFEST_DIR 下的 .tmp-* 子目录。
#[cfg(test)]
mod tests {
    use super::*;

    fn base(source: SnapshotSource) -> OrchSnapshot {
        let mut snap = OrchSnapshot::empty("r45", "2026-07-26T00:00:00Z");
        snap.source = source;
        snap
    }

    #[test]
    fn key_bindings_has_at_least_two_and_all_read_only() {
        let kb = key_bindings();
        assert!(kb.len() >= 2);
        for (_k, a) in &kb {
            assert!(a.is_read_only());
        }
    }

    #[test]
    fn render_extreme_sizes_do_not_panic_and_area_matches() {
        let snap = base(SnapshotSource::Daemon);
        for (w, h) in [(20u16, 6u16), (1, 1), (200, 3), (0, 0)] {
            let buf = render(&snap, w, h);
            assert_eq!(buf.area.width, w);
            assert_eq!(buf.area.height, h);
        }
    }

    #[test]
    fn frontend_source_is_marked_on_screen() {
        let snap = base(SnapshotSource::Frontend);
        let text: String = buf_content(&render(&snap, 120, 40));
        assert!(text.contains("frontend") || text.contains("自算") || text.contains("陈旧"));
    }

    #[test]
    fn activity_secrets_are_redacted() {
        let mut snap = base(SnapshotSource::Daemon);
        snap.activity = vec![orch_host::snapshot::ActivityLine {
            ts: "2026-07-26T00:00:01Z".to_string(),
            agent: "executor-opencode".to_string(),
            kind: "message".to_string(),
            summary: "token sk-livesecret leaked".to_string(),
        }];
        let text: String = buf_content(&render(&snap, 120, 40));
        assert!(!text.contains("sk-livesecret"));
    }

    // ── 定向修复回归测试（B103-REPAIR1） ──
    // 这些测试与**语义绑定**，补种子选择器无法捕获的退化。

    /// 取 `Buffer` 的第 `y` 行所有 cell 的拼接文本（按宽度截断）。
    fn row_text(buf: &Buffer, y: u16) -> String {
        if y >= buf.area.height {
            return String::new();
        }
        let w = buf.area.width;
        let mut s = String::new();
        for x in 0..w {
            s.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
        }
        s.trim_end().to_string()
    }

    /// 取 `Buffer` 的第 `y` 行首个**非空白** cell 的样式。
    /// 用于做样式断言（绑定到行而非全屏首个某字符）。
    fn row_style(buf: &Buffer, y: u16) -> Option<Style> {
        if y >= buf.area.height {
            return None;
        }
        let w = buf.area.width;
        for x in 0..w {
            if let Some(cell) = buf.cell((x, y)) {
                let sym = cell.symbol();
                if !sym.is_empty() && !sym.chars().all(|c| c.is_whitespace()) {
                    return Some(cell.style());
                }
            }
        }
        None
    }

    /// 找到包含 `needle` 的**行号**（语义定位）。
    fn find_row_containing(buf: &Buffer, needle: &str) -> Option<u16> {
        for y in 0..buf.area.height {
            if row_text(buf, y).contains(needle) {
                return Some(y);
            }
        }
        None
    }

    /// 定向修复 P0-1（M1）：critical 与 info 样式不同的**语义绑定**自证。
    ///
    /// 种子 `critical_alerts_are_visually_distinct` 用「全屏首个 `D` 字符」定位，
    /// 但顶栏 `src=Daemon` 也以 `D` 开头——选择器锚到顶栏，不锚到 critical 告警行。
    /// 把 `Severity::Critical` 改成与 `Info` 同样式后，种子用例**仍绿**（假证明）。
    ///
    /// 本自建测试用**告警文本**（`DEADEXEC` / `infoline`）定位所在行，再取该行样式做比对——
    /// 与被测语义绑定。下界自证：把 critical 改成 DarkGray → 本测试必红。
    #[test]
    fn critical_and_info_styles_differ_by_semantic_row() {
        let mut snap = base(SnapshotSource::Daemon);
        snap.alerts = vec![
            Alert {
                severity: Severity::Critical,
                kind: "liveness".to_string(),
                subject: "executor-desktop".to_string(),
                message: "DEADEXEC".to_string(),
                suggested_command: Some("orch retry-dead executor-desktop".to_string()),
            },
            Alert {
                severity: Severity::Info,
                kind: "liveness".to_string(),
                subject: "executor-opencode".to_string(),
                message: "infoline".to_string(),
                suggested_command: None,
            },
        ];
        let buf = render(&snap, 120, 40);

        // 语义定位：按告警文本找到各自所在行，再取该行首个非空 cell 的样式。
        let crit_row = find_row_containing(&buf, "DEADEXEC")
            .expect("critical 告警行必须上屏");
        let info_row = find_row_containing(&buf, "infoline")
            .expect("info 告警行必须上屏");
        let critical_style = row_style(&buf, crit_row)
            .expect("critical 行必须有样式化的 cell");
        let info_style = row_style(&buf, info_row)
            .expect("info 行必须有样式化的 cell");
        assert_ne!(
            critical_style, info_style,
            "critical 与 info 必须样式不同（按告警文本语义定位，不是全屏首个字符）"
        );
    }

    /// 定向修复 P0-2：未知/不可解析的 `generated_at` 必须被标注，不得显示成正常快照。
    ///
    /// `needs_stale_mark` 在 `generated_at` 解析失败或系统时间不可用时**必须 fail-closed**
    /// （标注「时间未知」），绝不把「不知道」折叠成「新鲜」。
    #[test]
    fn unknown_generated_at_is_marked_not_fresh() {
        // 空/不可解析时间戳
        let mut snap = OrchSnapshot::empty("r45", "");
        snap.source = SnapshotSource::Daemon;
        let buf = render(&snap, 120, 40);
        // ratatui 对 CJK 全角字符按宽度 2 渲染，会在字符间插入占位空格；
        // 因此断言时先去掉所有空格再比对，避免被双宽占位误判。
        let text: String = buf_content(&buf).replace(' ', "");
        assert!(
            text.contains("时间未知") || text.contains("陈旧") || text.contains("frontend"),
            "未知/不可解析时间不得无标记地显示为正常快照，画面:\n{}",
            buf_content(&buf)
        );
    }

    /// 定向修复 P0-2 的下界：解析失败返回 Fresh（退化）会让本测试红。
    #[test]
    fn unknown_generated_at_needs_stale_mark_is_not_fresh() {
        let mut snap = OrchSnapshot::empty("r45", "");
        snap.source = SnapshotSource::Daemon;
        let mark = needs_stale_mark(&snap);
        assert_ne!(
            mark,
            StaleMark::Fresh,
            "generated_at 解析失败必须 fail-closed（标 Unknown），不得折叠成 Fresh"
        );
    }

    /// 不可解析时间戳（非空但格式错误）也必须 fail-closed。
    #[test]
    fn malformed_generated_at_is_marked_not_fresh() {
        let mut snap = OrchSnapshot::empty("r45", "not-a-timestamp");
        snap.source = SnapshotSource::Daemon;
        let mark = needs_stale_mark(&snap);
        assert_eq!(
            mark,
            StaleMark::Unknown,
            "格式错误的时间戳必须标 Unknown（fail-closed）"
        );
    }

    /// 定向修复 P2：顶栏 `round_id` 已含 `r` 前缀，不得再补 `r`，否则出现 `orch rr45`。
    #[test]
    fn topbar_does_not_duplicate_round_prefix() {
        let snap = base(SnapshotSource::Daemon);
        let text: String = buf_content(&render(&snap, 120, 40));
        assert!(
            !text.contains("orch rr45"),
            "round_id 已含 `r` 前缀，不得再补 `r`（应显示 `orch r45`，不是 `orch rr45`）"
        );
        assert!(
            text.contains("orch r45"),
            "顶栏应显示 `orch r45`（round_id 原样拼接）"
        );
    }

    // ── 应用层薄壳（P1-1）单测 ──

    #[test]
    fn refresh_schedule_triggers_on_fs_alarm() {
        let mut s = RefreshSchedule::new();
        s.note_fs_alarm();
        assert_eq!(s.trigger(), Some(RefreshTrigger::FsEvent));
        assert!(s.trigger().unwrap().should_scan());
    }

    #[test]
    fn refresh_schedule_triggers_on_manual() {
        let mut s = RefreshSchedule::new();
        s.note_manual();
        assert_eq!(s.trigger(), Some(RefreshTrigger::Manual));
    }

    #[test]
    fn refresh_schedule_timer_fallback_after_2s() {
        let mut s = RefreshSchedule::new();
        assert_eq!(s.trigger(), None, "0s 时不应触发");
        s.tick(1);
        assert_eq!(s.trigger(), None, "1s < 2s 不应触发");
        s.tick(1);
        assert_eq!(s.trigger(), Some(RefreshTrigger::Timer), "2s 后定时兜底重扫");
        s.mark_scanned();
        assert_eq!(s.trigger(), None, "扫描后重置");
    }

    #[test]
    fn refresh_schedule_event_is_alarm_scan_is_guarantee() {
        // 即便事件闹钟丢失，定时器兜底仍保证扫描
        let mut s = RefreshSchedule::new();
        s.tick(2);
        assert_eq!(s.trigger(), Some(RefreshTrigger::Timer));
    }

    #[test]
    fn refresh_schedule_manual_precedes_others() {
        let mut s = RefreshSchedule::new();
        s.note_fs_alarm();
        s.note_manual();
        assert_eq!(s.trigger(), Some(RefreshTrigger::Manual));
    }

    #[test]
    fn alert_debouncer_notifies_first_occurrence() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let a = Alert {
            severity: Severity::Critical,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "DEADEXEC".to_string(),
            suggested_command: None,
        };
        assert!(d.should_notify(&a, 0), "首次出现必须通知");
    }

    #[test]
    fn alert_debouncer_suppresses_within_cooldown() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let a = Alert {
            severity: Severity::Critical,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "DEADEXEC".to_string(),
            suggested_command: None,
        };
        assert!(d.should_notify(&a, 0));
        assert!(
            !d.should_notify(&a, 100),
            "冷却窗内同身份告警不重复轰炸"
        );
        assert!(!d.should_notify(&a, 299), "仍在内窗");
    }

    #[test]
    fn alert_debouncer_renotifies_after_cooldown() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let a = Alert {
            severity: Severity::Critical,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "DEADEXEC".to_string(),
            suggested_command: None,
        };
        assert!(d.should_notify(&a, 0));
        assert!(d.should_notify(&a, 300), "超过冷却窗后重新通知");
    }

    #[test]
    fn alert_debouncer_different_severity_is_different_alert() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let crit = Alert {
            severity: Severity::Critical,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "DEADEXEC".to_string(),
            suggested_command: None,
        };
        let info = Alert {
            severity: Severity::Info,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "infoline".to_string(),
            suggested_command: None,
        };
        assert!(d.should_notify(&crit, 0));
        assert!(
            d.should_notify(&info, 0),
            "不同 severity 视为不同告警（升级应重新通知）"
        );
    }

    #[test]
    fn alert_debouncer_reset_allows_renotify() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let a = Alert {
            severity: Severity::Critical,
            kind: "liveness".to_string(),
            subject: "executor-desktop".to_string(),
            message: "DEADEXEC".to_string(),
            suggested_command: None,
        };
        assert!(d.should_notify(&a, 0));
        d.reset();
        assert!(d.should_notify(&a, 0), "reset 后重新通知");
    }

    /// 复审 P1-1 回归：不同三元组即使 kind/subject 含分隔符 `|`，身份也**不得碰撞**。
    /// 夹具直接取自 `B103-REAUDIT.md`：`("a|b","c",Critical)` 与 `("a","b|c",Critical)`。
    /// 旧实现把二者拼成同一字符串 `a|b|c|Critical` → 第二个告警被误抑制。
    #[test]
    fn alert_identity_distinct_tuples_with_separator_do_not_collide() {
        let a = Alert {
            severity: Severity::Critical,
            kind: "a|b".to_string(),
            subject: "c".to_string(),
            message: String::new(),
            suggested_command: None,
        };
        let b = Alert {
            severity: Severity::Critical,
            kind: "a".to_string(),
            subject: "b|c".to_string(),
            message: String::new(),
            suggested_command: None,
        };
        let id_a = AlertIdentity::of(&a);
        let id_b = AlertIdentity::of(&b);
        assert_ne!(id_a, id_b, "不同三元组不得碰撞为同一身份");
        assert_ne!(id_a.as_tuple(), id_b.as_tuple());
        // 显式断言三字段分别不同，防止任何「巧合相等」
        assert_ne!(id_a.as_tuple().0, id_b.as_tuple().0, "kind 必须不同");
    }

    /// 复审 P1-1 回归（行为层）：第一个告警在 t=0 通知后，**第二个不同身份**的告警
    /// 在冷却窗内（t=100）仍**必须**通知——不得因身份碰撞被误抑制。
    /// 这是告警系统最不该有的失败方向：该响的没响。
    #[test]
    fn alert_debouncer_distinct_alert_not_suppressed_within_cooldown() {
        let mut d = AlertDebouncer::with_cooldown(300);
        let first = Alert {
            severity: Severity::Critical,
            kind: "a|b".to_string(),
            subject: "c".to_string(),
            message: String::new(),
            suggested_command: None,
        };
        let second = Alert {
            severity: Severity::Critical,
            kind: "a".to_string(),
            subject: "b|c".to_string(),
            message: String::new(),
            suggested_command: None,
        };
        assert!(d.should_notify(&first, 0), "首个告警必须通知");
        assert!(
            d.should_notify(&second, 100),
            "不同身份的第二告警在冷却窗内也必须通知，不得被碰撞身份误抑制"
        );
    }

    /// 剪贴板铁律：`copy_to_clipboard` 只复制不执行，不触发 orch 写命令。
    /// 这里测的是**只读断言**——`Action` 集合里不存在写动作变体。
    #[test]
    fn no_write_action_variants_exist() {
        let kb = key_bindings();
        assert!(
            assert_no_write_actions_in_bindings(&kb),
            "所有按键动作必须只读，不得有 dispatch/verify/merge/close/nudge/retry"
        );
        // 进一步枚举所有 Action 变体都是只读
        for a in [Action::Refresh, Action::CopyCommand, Action::Quit] {
            assert!(a.is_read_only(), "{a:?} 必须只读");
        }
    }

    /// `copy_to_clipboard` 在 `pbcopy` 不可用的环境应优雅失败而非 panic。
    /// （CI/无 macOS 环境下 pbcopy 可能不存在；这里只断言不 panic。）
    #[test]
    fn copy_to_clipboard_does_not_panic() {
        let _ = copy_to_clipboard("orch retry-dead executor-desktop");
        // 无论 pbcopy 是否成功，都不得 panic。
    }

    fn buf_content(buf: &Buffer) -> String {
        buf.content()
            .iter()
            .map(|c| c.symbol().to_string())
            .collect::<Vec<_>>()
            .concat()
    }
}
