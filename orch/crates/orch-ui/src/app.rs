//! Independent, read-only invocation UI. A phase is a past observation, never a
//! liveness assertion. The controller owns bounded observation reads; the view
//! owns no provider, task-control, recovery, collection or GC capability.
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use orch_host::observation::{
    safe_observation_text, InvocationDetail, InvocationObservation, ObservationReader,
    ProjectObservation,
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    Terminal,
};
use std::{
    io::{self, IsTerminal, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Grouping changes ordering and labels, never the set or identity of calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grouping {
    /// Individual calls, newest captured start first; stable ID breaks ties.
    Calls,
    /// Captured harness identity, preserving repeated calls.
    Harness,
    /// Exact round/task/attempt/head, or an explicit unassociated group.
    Tasks,
}
/// UI effects consumed by the read-only controller; none can control a call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// No IO requested.
    None,
    /// Refresh bounded local facts.
    Refresh,
    /// Revalidate the selected ID's detail.
    Detail(String),
    /// Validate a different project before replacing context.
    SwitchProject(String),
    /// Copy already sanitized locator bytes, never execute them.
    CopyLocator(String),
    /// Restore the terminal and return.
    Quit,
}

/// Local presentation state. Public fields are view data, not runtime authority.
/// Rendering is pure and uses the controller's stored successful-read age.
pub struct AppState {
    /// Last successful bounded read, retained on refresh failures.
    pub snapshot: ProjectObservation,
    /// Stable source-qualified invocation ID, independent of grouping/index.
    pub selected_id: Option<String>,
    /// Current projection order.
    pub grouping: Grouping,
    /// On-demand revalidated detail; cleared by every new project snapshot.
    pub detail: Option<InvocationDetail>,
    /// Literal project input; hotkeys become text while it is active.
    pub project_input: Option<String>,
    /// Sanitized view error; does not change observed call state.
    pub error: Option<String>,
    detail_scroll: usize,
    successful_age_secs: u64,
    ordered: Vec<String>,
}
impl AppState {
    /// Construct a fresh context and select its first stable ordered ID.
    pub fn new(snapshot: ProjectObservation) -> Self {
        let mut s = Self {
            snapshot,
            selected_id: None,
            grouping: Grouping::Calls,
            detail: None,
            project_input: None,
            error: None,
            detail_scroll: 0,
            successful_age_secs: 0,
            ordered: vec![],
        };
        s.reorder();
        s.selected_id = s.ordered_ids().first().cloned();
        s
    }
    /// Return a sanitized exact group key, or None for an unavailable ID.
    pub fn group_key(&self, id: &str) -> Option<String> {
        self.snapshot
            .rows
            .iter()
            .find(|r| r.id == id)
            .map(|r| Self::row_group(r, self.grouping))
    }
    fn row_group(r: &InvocationObservation, grouping: Grouping) -> String {
        safe_observation_text(&match grouping {
            Grouping::Calls => r.id.clone(),
            Grouping::Harness => format!(
                "{} / {}",
                r.alias.as_deref().unwrap_or("unknown"),
                r.driver.as_deref().unwrap_or("unknown")
            ),
            Grouping::Tasks => r
                .task
                .as_ref()
                .map(|t| {
                    format!(
                        "{} / {} / {} / {}",
                        t.round,
                        t.id,
                        t.attempt.as_deref().unwrap_or("unknown"),
                        t.head.as_deref().unwrap_or("unknown")
                    )
                })
                .unwrap_or_else(|| "unassociated".into()),
        })
    }
    fn reorder(&mut self) {
        let mut pairs: Vec<_> = self
            .snapshot
            .rows
            .iter()
            .map(|r| {
                (
                    if self.grouping == Grouping::Calls {
                        (
                            r.started_at.is_some(),
                            r.started_at.clone().unwrap_or_default(),
                        )
                    } else {
                        (true, Self::row_group(r, self.grouping))
                    },
                    r.id.clone(),
                )
            })
            .collect();
        pairs.sort();
        if self.grouping == Grouping::Calls {
            pairs.reverse();
        }
        self.ordered = pairs.into_iter().map(|(_, id)| id).collect();
    }
    fn recount(&mut self) {
        for g in &mut self.snapshot.groups {
            g.phase_counts.clear();
            g.result_counts.clear();
            for r in self
                .snapshot
                .rows
                .iter()
                .filter(|r| r.fusion_id.as_deref() == Some(&g.id))
            {
                *g.phase_counts.entry(r.phase.clone()).or_insert(0) += 1;
                *g.result_counts.entry(r.result.clone()).or_insert(0) += 1;
            }
        }
    }
    /// All calls in the current grouping order; ties use stable ID.
    pub fn ordered_ids(&self) -> Vec<String> {
        self.ordered.clone()
    }
    /// Switch projection without replacing the selected call.
    pub fn set_grouping(&mut self, grouping: Grouping) {
        self.grouping = grouping;
        self.reorder();
    }
    /// Select an existing stable ID, discarding another call's detail.
    pub fn select(&mut self, id: &str) {
        if self.snapshot.rows.iter().any(|r| r.id == id) {
            if self.selected_id.as_deref() != Some(id) {
                self.detail = None;
                self.detail_scroll = 0;
            }
            self.selected_id = Some(id.into());
        }
    }
    /// Replace only successful facts. Old detail cannot survive revalidation.
    pub fn apply_snapshot(&mut self, snapshot: ProjectObservation) {
        self.snapshot = snapshot;
        self.reorder();
        self.detail = None;
        self.detail_scroll = 0;
        self.error = None;
        self.successful_age_secs = 0;
        if !self
            .snapshot
            .rows
            .iter()
            .any(|r| Some(&r.id) == self.selected_id.as_ref())
        {
            self.selected_id = self.ordered_ids().first().cloned();
        }
    }
    /// Accept detail only for the selected ID and immediately synchronize its row.
    /// Invalid/unverified details cannot carry a previously verified answer.
    pub fn apply_detail(&mut self, mut detail: InvocationDetail) {
        if self.selected_id.as_deref() != Some(&detail.row.id) {
            return;
        }
        if detail.row.result != "verified" {
            detail.text = None;
        }
        if let Some(row) = self
            .snapshot
            .rows
            .iter_mut()
            .find(|r| r.id == detail.row.id)
        {
            *row = detail.row.clone();
            self.detail = Some(detail);
            self.detail_scroll = 0;
            self.error = None;
            self.recount();
            self.reorder();
        }
    }
    /// Preserve the last snapshot with an honest sanitized error.
    pub fn mark_error(&mut self, error: &str) {
        self.error = Some(safe_observation_text(error));
    }
    /// The selected safe evidence locator, suitable for display/copy only.
    pub fn safe_locator(&self) -> Option<String> {
        self.snapshot
            .rows
            .iter()
            .find(|r| Some(&r.id) == self.selected_id.as_ref())
            .map(|r| safe_observation_text(&r.locator))
    }
    fn navigate(&mut self, delta: isize) {
        let ids = self.ordered_ids();
        if ids.is_empty() {
            self.selected_id = None;
            return;
        }
        let at = ids
            .iter()
            .position(|id| Some(id) == self.selected_id.as_ref())
            .unwrap_or(0);
        let next = (at as isize + delta).clamp(0, ids.len() as isize - 1) as usize;
        self.select(&ids[next]);
    }
    /// Interpret a press/repeat. `page_rows` is viewport size, never a clock.
    /// Ctrl+C always exits; path-input hotkeys remain literal path characters.
    pub fn key(&mut self, key: KeyEvent, page_rows: usize) -> Effect {
        if key.kind == KeyEventKind::Release {
            return Effect::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Effect::Quit;
        }
        if let Some(input) = self.project_input.as_mut() {
            return match key.code {
                KeyCode::Esc => {
                    self.project_input = None;
                    Effect::None
                }
                KeyCode::Enter => Effect::SwitchProject(input.clone()),
                KeyCode::Backspace => {
                    input.pop();
                    Effect::None
                }
                KeyCode::Char(c)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if input.len() < 4096 {
                        input.push(c);
                    }
                    Effect::None
                }
                _ => Effect::None,
            };
        }
        let page = page_rows.max(1);
        match key.code {
            KeyCode::Char('q') => Effect::Quit,
            KeyCode::Char('r') => Effect::Refresh,
            KeyCode::Char('p') => {
                self.project_input = Some(String::new());
                Effect::None
            }
            KeyCode::Char('1') => {
                self.set_grouping(Grouping::Calls);
                Effect::None
            }
            KeyCode::Char('2') => {
                self.set_grouping(Grouping::Harness);
                Effect::None
            }
            KeyCode::Char('3') => {
                self.set_grouping(Grouping::Tasks);
                Effect::None
            }
            KeyCode::Char('y') => self
                .safe_locator()
                .map(Effect::CopyLocator)
                .unwrap_or(Effect::None),
            KeyCode::Enter => self
                .selected_id
                .clone()
                .map(Effect::Detail)
                .unwrap_or(Effect::None),
            KeyCode::Esc => {
                self.detail = None;
                self.detail_scroll = 0;
                Effect::None
            }
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown => {
                let delta = match key.code {
                    KeyCode::Up => -1,
                    KeyCode::Down => 1,
                    KeyCode::PageUp => -(page as isize),
                    _ => page as isize,
                };
                if self.detail.is_some() {
                    self.detail_scroll = self.detail_scroll.saturating_add_signed(delta);
                } else {
                    self.navigate(delta);
                }
                Effect::None
            }
            _ => Effect::None,
        }
    }
    /// Pure bounded Buffer rendering, including zero-size and Unicode screens.
    /// Each untrusted field is sanitized before layout clipping.
    pub fn render(&self, width: u16, height: u16) -> Buffer {
        let mut b = Buffer::empty(Rect::new(0, 0, width, height));
        if width == 0 || height == 0 {
            return b;
        }
        let mut lines: Vec<(String, Style)> = Vec::new();
        let normal = Style::default();
        let muted = Style::default().fg(Color::DarkGray);
        let warning = Style::default().fg(Color::Yellow);
        let title = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        lines.push((
            format!(
                "Fusion observations | {:?} | {}",
                self.grouping,
                safe_observation_text(&self.snapshot.root)
            ),
            title,
        ));
        lines.push((
            format!(
                "last successful read {} | age {}s | phase = last observed",
                safe_observation_text(&self.snapshot.read_at),
                self.successful_age_secs
            ),
            muted,
        ));
        if let Some(e) = &self.error {
            lines.push((format!("View error: {}", safe_observation_text(e)), warning));
        }
        if self.snapshot.truncated {
            lines.push((
                "Bounded sources clipped; missing facts remain unknown".into(),
                warning,
            ));
        }
        for diag in self.snapshot.diagnostics.iter().take(2) {
            lines.push((safe_observation_text(diag), warning));
        }
        if let Some(input) = &self.project_input {
            lines.push((
                format!(
                    "Project: {} | Enter validates, Esc cancels",
                    safe_observation_text(input)
                ),
                title,
            ));
        }
        let fusion = self
            .selected_id
            .as_ref()
            .and_then(|id| self.snapshot.rows.iter().find(|r| &r.id == id))
            .and_then(|r| r.fusion_id.as_ref())
            .and_then(|id| self.snapshot.groups.iter().find(|g| &g.id == id))
            .or_else(|| self.snapshot.groups.first());
        if let Some(g) = fusion {
            lines.push((
                format!(
                    "Fusion {} | {} | roster {} | phases {} | results {}",
                    safe_observation_text(&g.id),
                    g.total
                        .map(|n| format!("total {n}"))
                        .unwrap_or_else(|| "total unknown".into()),
                    if g.roster_complete {
                        "complete"
                    } else {
                        "partial"
                    },
                    safe_json(&g.phase_counts),
                    safe_json(&g.result_counts)
                ),
                normal,
            ));
        }
        if let Some(d) = &self.detail {
            let r = &d.row;
            let mut fields = vec![
                format!(
                    "Call {} | {} / {}",
                    safe_observation_text(&r.id),
                    safe_option(&r.alias),
                    safe_option(&r.driver)
                ),
                format!("Summary: {}", safe_observation_text(&r.summary)),
                format!(
                    "last observed: {} | result: {}",
                    safe_observation_text(&r.phase),
                    safe_observation_text(&r.result)
                ),
                format!("Native: {}", safe_json(&r.native_status)),
                format!("Task: {}", safe_json(&r.task)),
                format!("Requested: {}", safe_json(&r.requested_tuple)),
                format!("Effective: {}", safe_json(&r.effective_tuple)),
                format!("Parameters: {}", safe_json(&r.parameters)),
                format!("Activity: {}", safe_json(&r.activity)),
                format!(
                    "Started: {} | source time: {} | duration {}",
                    safe_option(&r.started_at),
                    safe_option(&r.source_time),
                    r.duration_secs
                        .map(|v| format!("{v}s"))
                        .unwrap_or_else(|| "unknown".into())
                ),
                format!("HEAD: {}", safe_option(&r.head)),
                format!("Locator: {}", safe_observation_text(&d.locator)),
            ];
            for e in &r.diagnostics {
                fields.push(format!("Diagnostic: {}", safe_observation_text(e)));
            }
            fields.push(format!(
                "Answer: {}{}",
                d.text
                    .as_deref()
                    .map(safe_observation_text)
                    .unwrap_or_else(|| "unavailable / unverified".into()),
                if d.truncated {
                    " [display clipped]"
                } else {
                    ""
                }
            ));
            let wrapped: Vec<_> = fields
                .iter()
                .flat_map(|s| wrap(s, width as usize))
                .collect();
            let room = (height as usize).saturating_sub(lines.len() + 1);
            let start = self
                .detail_scroll
                .min(wrapped.len().saturating_sub(room.max(1)));
            for line in wrapped.into_iter().skip(start).take(room) {
                lines.push((line, normal));
            }
        } else {
            lines.push((
                "Call / captured harness | last observed | result | elapsed | summary".into(),
                muted,
            ));
            let ids = self.ordered_ids();
            let room = (height as usize).saturating_sub(lines.len() + 1);
            let at = ids
                .iter()
                .position(|id| Some(id) == self.selected_id.as_ref())
                .unwrap_or(0);
            let start = at.saturating_sub(room.saturating_sub(1));
            if ids.is_empty() {
                lines.push(("No observable calls in this project".into(), muted));
            }
            for id in ids.into_iter().skip(start).take(room) {
                if let Some(r) = self.snapshot.rows.iter().find(|r| r.id == id) {
                    let selected = Some(&id) == self.selected_id.as_ref();
                    let group = if self.grouping == Grouping::Calls {
                        safe_observation_text(&id)
                    } else {
                        self.group_key(&id).unwrap_or_default()
                    };
                    let line = format!(
                        "{} {} / {} | {} | {} | {} | {}",
                        if selected { ">" } else { " " },
                        group,
                        safe_option(&r.alias),
                        safe_observation_text(&r.phase),
                        safe_observation_text(&r.result),
                        r.duration_secs
                            .map(|v| format!("{v}s"))
                            .unwrap_or_else(|| "unknown".into()),
                        safe_observation_text(&r.summary)
                    );
                    lines.push((
                        line,
                        if selected {
                            normal.bg(Color::DarkGray)
                        } else {
                            normal
                        },
                    ));
                }
            }
        }
        let footer="1/2/3 group  ↑↓ PgUp/PgDn  Enter detail  Esc back  p project  r refresh  y locator  q quit";
        for (y, (line, style)) in lines
            .into_iter()
            .take(height.saturating_sub(1) as usize)
            .enumerate()
        {
            b.set_string(0, y as u16, clip(&line, width as usize), style);
        }
        b.set_string(0, height - 1, clip(footer, width as usize), muted);
        b
    }
}
fn safe_option(v: &Option<String>) -> String {
    v.as_deref()
        .map(safe_observation_text)
        .unwrap_or_else(|| "unknown".into())
}
fn safe_json(v: &impl serde::Serialize) -> String {
    safe_observation_text(&serde_json::to_string(v).unwrap_or_else(|_| "unavailable".into()))
}
fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
fn wrap(s: &str, n: usize) -> Vec<String> {
    let n = n.max(1);
    let line = Line::from(s);
    let mut out = Vec::new();
    let mut current = String::new();
    let mut width = 0;
    for g in line.styled_graphemes(Style::default()) {
        let w = Span::raw(g.symbol).width();
        if w > n {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
            out.push("…".into());
            width = 0;
            continue;
        }
        if width + w > n {
            out.push(std::mem::take(&mut current));
            width = 0;
        }
        current.push_str(g.symbol);
        width += w;
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// Owns one bounded reader and its UI context. Failed switches cannot mix projects.
pub struct ObservationApp {
    /// Current local presentation state.
    pub state: AppState,
    reader: ObservationReader,
    last_success: Instant,
}
impl ObservationApp {
    /// Read an existing project without active-round or configuration requirements.
    pub fn open(root: &Path) -> Result<Self, String> {
        let mut reader =
            ObservationReader::open(root).map_err(|e| safe_observation_text(&e.to_string()))?;
        let snapshot = reader
            .refresh()
            .map_err(|e| safe_observation_text(&e.to_string()))?;
        Ok(Self {
            state: AppState::new(snapshot),
            reader,
            last_success: Instant::now(),
        })
    }
    /// Refresh local facts. Preserve previous rows on read failure, invalidate
    /// stale detail, and revalidate an open detail before showing it again.
    pub fn refresh(&mut self) -> Result<(), String> {
        let old_detail = self.state.detail.as_ref().map(|d| d.row.id.clone());
        let scroll = self.state.detail_scroll;
        match self.reader.refresh() {
            Ok(s) => {
                self.state.apply_snapshot(s);
                self.last_success = Instant::now();
                if old_detail.is_some() && old_detail == self.state.selected_id {
                    self.show_detail()?;
                    if self.state.detail.is_some() {
                        self.state.detail_scroll = scroll;
                    }
                }
                Ok(())
            }
            Err(e) => {
                self.state.detail = None;
                let e = safe_observation_text(&e.to_string());
                self.state.mark_error(&e);
                Err(e)
            }
        }
    }
    /// Revalidate selected detail, synchronize its row, discard old text on error.
    pub fn show_detail(&mut self) -> Result<(), String> {
        self.state.detail = None;
        let Some(id) = self.state.selected_id.clone() else {
            return Ok(());
        };
        match self.reader.detail(&id) {
            Ok(d) => {
                self.state.apply_detail(d);
                Ok(())
            }
            Err(e) => {
                if let Some(r) = self.state.snapshot.rows.iter_mut().find(|r| r.id == id) {
                    r.result = "unverified".into();
                }
                self.state.recount();
                let e = safe_observation_text(&e.to_string());
                self.state.mark_error(&e);
                Err(e)
            }
        }
    }
    /// Validate reader and snapshot first. Failure keeps the old context intact.
    pub fn switch_project(&mut self, root: &Path) -> Result<(), String> {
        match Self::open(root) {
            Ok(new) => {
                *self = new;
                Ok(())
            }
            Err(e) => {
                self.state.mark_error(&e);
                Err(e)
            }
        }
    }
    fn tick_age(&mut self) {
        self.state.successful_age_secs = self.last_success.elapsed().as_secs();
    }
}

struct TerminalGuard;
fn restore_screen(writer: &mut impl Write) {
    let _ = execute!(writer, Show);
    let _ = execute!(writer, LeaveAlternateScreen);
}
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_screen(&mut io::stdout());
        let _ = disable_raw_mode();
    }
}
fn terminal_session<T>(
    body: impl FnOnce(&mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<T>,
) -> io::Result<T> {
    enable_raw_mode()?;
    let _guard = TerminalGuard; // Arm before any fallible screen/backend setup.
    execute!(io::stdout(), EnterAlternateScreen, Hide)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    body(&mut terminal)
}
fn copy_locator_with(executable: &Path, text: &str) -> Result<(), String> {
    let safe = safe_observation_text(text);
    if safe.len() > 64 * 1024 {
        return Err("clipboard locator too large".into());
    }
    let child = Command::new(executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "clipboard unavailable".to_string())?;
    finish_clipboard_child(child, &safe)
}
// Shared custody/IO stage. Tests can hand in a proven-started owned fixture;
// the production caller hands in its child immediately after spawn, so loader
// delays still consume the same two-second deadline rather than receiving grace.
fn finish_clipboard_child(mut child: std::process::Child, safe: &str) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let mut input = child.stdin.take();
    let result = (|| {
        let fd = input
            .as_ref()
            .ok_or("clipboard input unavailable")?
            .as_raw_fd();
        unsafe {
            extern "C" {
                fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            }
            let flags = fcntl(fd, 3);
            let nonblock = if cfg!(target_os = "macos") { 4 } else { 0x800 };
            if flags < 0 || fcntl(fd, 4, flags | nonblock) < 0 {
                return Err("clipboard nonblocking setup failed");
            }
        }
        let end = Instant::now() + Duration::from_secs(2);
        let mut written = 0;
        loop {
            if let Some(stdin) = input.as_mut() {
                match stdin.write(&safe.as_bytes()[written..]) {
                    Ok(0) if written < safe.len() => return Err("clipboard write failed"),
                    Ok(n) => written += n,
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                        ) => {}
                    Err(_) => return Err("clipboard write failed"),
                };
                if written == safe.len() {
                    input.take();
                }
            }
            if let Some(status) = child.try_wait().map_err(|_| "clipboard wait failed")? {
                return if status.success() && written == safe.len() {
                    Ok(())
                } else {
                    Err("clipboard process failed")
                };
            }
            if Instant::now() >= end {
                return Err("clipboard deadline exceeded");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    drop(input);
    if result.is_err() {
        if child
            .try_wait()
            .map_err(|_| "clipboard cleanup status unavailable")?
            .is_none()
        {
            if child.kill().is_err() && child.try_wait().ok().flatten().is_none() {
                return Err("clipboard cleanup failed".into());
            }
        }
        child.wait().map_err(|_| "clipboard reap failed")?;
    }
    result.map_err(str::to_string)
}
fn copy_locator(text: &str) -> Result<(), String> {
    if cfg!(target_os = "macos") {
        copy_locator_with(Path::new("/usr/bin/pbcopy"), text)
    } else {
        Err("clipboard unsupported on this platform".into())
    }
}
struct RefreshDeadline {
    next: Instant,
}
impl RefreshDeadline {
    fn new(now: Instant) -> Self {
        Self {
            next: now + Duration::from_secs(2),
        }
    }
    fn due(&mut self, now: Instant) -> bool {
        if now >= self.next {
            self.next = now + Duration::from_secs(2);
            true
        } else {
            false
        }
    }
}

/// Run a foreground, read-only UI. Both input and output must be TTYs before
/// raw mode. Normal/error/unwind paths restore the terminal; abort/SIGKILL do
/// not run Rust destructors. This function never launches or stops a provider.
pub fn run(root: &Path) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("orch-tui requires TTY stdin and stdout".into());
    }
    let mut app = ObservationApp::open(root)?;
    terminal_session(|terminal| {
        let mut deadline = RefreshDeadline::new(Instant::now());
        loop {
            app.tick_age();
            terminal.draw(|frame| {
                let area = frame.area();
                *frame.buffer_mut() = app.state.render(area.width, area.height);
            })?;
            let wait = deadline
                .next
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(250));
            if event::poll(wait)? {
                if let Event::Key(key) = event::read()? {
                    let page = terminal.size()?.height.saturating_sub(7) as usize;
                    match app.state.key(key, page) {
                        Effect::Quit => break,
                        Effect::Refresh => {
                            let _ = app.refresh();
                        }
                        Effect::Detail(_) => {
                            let _ = app.show_detail();
                        }
                        Effect::SwitchProject(path) => {
                            let _ = app.switch_project(Path::new(&path));
                        }
                        Effect::CopyLocator(text) => {
                            if let Err(e) = copy_locator(&text) {
                                app.state.mark_error(&e);
                            }
                        }
                        Effect::None => {}
                    }
                }
            }
            if deadline.due(Instant::now()) {
                let _ = app.refresh();
            }
        }
        Ok(())
    })
    .map_err(|e| safe_observation_text(&e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Read, os::unix::fs::PermissionsExt, path::PathBuf};

    #[test]
    fn detail_untrusted_summary_is_masked_before_wrapping() {
        let f = Fixture::new();
        f.run();
        let mut app = ObservationApp::open(&f.root).unwrap();
        let mut row = app.state.snapshot.rows[0].clone();
        row.summary = "x=1\nAPI_KEY=abcdef".into();
        app.state.apply_detail(InvocationDetail {
            row,
            text: Some("safe answer".into()),
            locator: "ordinary locator".into(),
            truncated: false,
        });
        let b = app.state.render(200, 30);
        let raw = b
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<Vec<_>>()
            .join("");
        assert!(!raw.contains("abcdef"));
        assert!(raw.contains("[redacted]"));
        assert!(raw.contains("safe answer"));
    }

    #[test]
    fn known_started_call_precedes_legacy_unknown_time() {
        let f = Fixture::new();
        f.run();
        let mut snap = ObservationApp::open(&f.root).unwrap().state.snapshot;
        let mut old = snap.rows[0].clone();
        old.id = "zz-legacy".into();
        old.started_at = None;
        let mut new = old.clone();
        new.id = "aa-current".into();
        new.started_at = Some("2026-09-14T00:00:00Z".into());
        snap.rows = vec![old, new];
        let state = AppState::new(snap);
        assert_eq!(state.selected_id.as_deref(), Some("aa-current"));
        assert_eq!(state.ordered_ids(), vec!["aa-current", "zz-legacy"]);
    }
    #[test]
    fn unicode_wrapping_preserves_graphemes_and_every_character() {
        let value = "甲乙丙丁戊己庚辛壬癸 👩‍💻 e\u{301} 后续中文";
        let lines = wrap(value, 10);
        assert_eq!(lines.concat(), value);
        for l in lines {
            assert!(Span::raw(l).width() <= 10);
        }
    }
    #[test]
    fn absolute_refresh_deadline_accumulates_subsecond_polls() {
        let base = Instant::now();
        let mut d = RefreshDeadline::new(base);
        for ms in [200, 400, 800, 1200, 1600, 1999] {
            assert!(!d.due(base + Duration::from_millis(ms)));
        }
        assert!(d.due(base + Duration::from_secs(2)));
        assert!(!d.due(base + Duration::from_millis(2200)));
        assert!(d.due(base + Duration::from_secs(4)));
    }
    #[test]
    fn cleanup_attempts_leave_after_cursor_write_error() {
        struct Once {
            failed: bool,
            bytes: Vec<u8>,
        }
        impl Write for Once {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                if !self.failed {
                    self.failed = true;
                    Err(io::Error::other("first"))
                } else {
                    self.bytes.extend_from_slice(b);
                    Ok(b.len())
                }
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut w = Once {
            failed: false,
            bytes: vec![],
        };
        restore_screen(&mut w);
        assert!(String::from_utf8_lossy(&w.bytes).contains("\x1b[?1049l"));
    }
    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = orch_host::util::test_scratch_dir("b353-app");
            fs::create_dir_all(root.join(".orch")).unwrap();
            fs::write(
                root.join(".gitignore"),
                ".orch/\ncoordination/\n.cowork-temp/\n",
            )
            .unwrap();
            fs::write(root.join("question"), "Hello 中文").unwrap();
            let exe = root.join("provider");
            fs::write(&exe,"#!/bin/sh\nprintf '%s\\n' '{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"ANSWER\"}'\n").unwrap();
            fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
            fs::write(root.join(".orch/harnesses.yaml"),format!("version: 1\nharnesses:\n  one:\n    driver: claude\n    executable: {}\n    enabled: true\n    cwdPolicy: project-root\n",exe.display())).unwrap();
            for args in [
                vec!["init", "-q"],
                vec!["add", "question"],
                vec![
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "-qm",
                    "base",
                ],
            ] {
                assert!(Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .status()
                    .unwrap()
                    .success());
            }
            Self { root }
        }
        fn run(&self) -> PathBuf {
            orch_host::consult::run_consultation(
                &self.root,
                &orch_host::consult::ConsultArgs {
                    question: "question".into(),
                    harnesses: vec!["one".into()],
                    member_timeout_secs: Some(5),
                    total_wall_secs: Some(10),
                    ..Default::default()
                },
            )
            .unwrap()
            .dir
        }
        fn script(&self, name: &str, body: &str) -> PathBuf {
            let p = self.root.join(name);
            fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
            p
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                let _ = fs::remove_dir_all(&self.root);
            }
        }
    }
    #[test]
    fn actual_detail_invalidation_syncs_counts_and_failure_downgrades_label() {
        let f = Fixture::new();
        let dir = f.run();
        let mut app = ObservationApp::open(&f.root).unwrap();
        app.show_detail().unwrap();
        assert_eq!(
            app.state.detail.as_ref().unwrap().text.as_deref(),
            Some("ANSWER")
        );
        fs::write(dir.join("fusion/0-one.md"), "changed").unwrap();
        app.show_detail().unwrap();
        assert_eq!(app.state.snapshot.rows[0].result, "invalid");
        assert_eq!(
            app.state.snapshot.groups[0].result_counts.get("invalid"),
            Some(&1)
        );
        assert_eq!(
            app.state.snapshot.groups[0].result_counts.get("verified"),
            None
        );
        assert!(app.state.detail.as_ref().unwrap().text.is_none());
        fs::remove_dir_all(dir).unwrap();
        assert!(app.show_detail().is_err());
        assert_eq!(app.state.snapshot.rows[0].result, "unverified");
        assert!(app.state.detail.is_none());
    }
    #[test]
    fn refresh_preserves_detail_scroll_and_root_failure_keeps_old_rows() {
        let f = Fixture::new();
        f.run();
        let mut app = ObservationApp::open(&f.root).unwrap();
        app.show_detail().unwrap();
        app.state.detail_scroll = 7;
        app.refresh().unwrap();
        assert_eq!(app.state.detail_scroll, 7);
        let old = serde_json::to_value(&app.state.snapshot).unwrap();
        let moved = f.root.with_extension("away");
        fs::rename(&f.root, &moved).unwrap();
        let result = app.refresh();
        fs::rename(&moved, &f.root).unwrap();
        assert!(result.is_err());
        assert_eq!(serde_json::to_value(&app.state.snapshot).unwrap(), old);
        assert!(app.state.error.is_some());
        assert!(app.state.detail.is_none());
        app.refresh().unwrap();
        assert!(app.state.error.is_none());
    }
    #[test]
    fn project_success_replaces_reader_detail_and_error() {
        let a = Fixture::new();
        a.run();
        let b = Fixture::new();
        b.run();
        let mut app = ObservationApp::open(&a.root).unwrap();
        app.show_detail().unwrap();
        app.state.mark_error("old error");
        app.state.project_input = Some("old path".into());
        app.switch_project(&b.root).unwrap();
        assert_eq!(app.state.snapshot.root, b.root.display().to_string());
        assert!(app.state.detail.is_none());
        assert!(app.state.error.is_none());
        assert!(app.state.project_input.is_none());
        app.show_detail().unwrap();
        assert_eq!(
            app.state.detail.as_ref().unwrap().text.as_deref(),
            Some("ANSWER")
        );
    }
    #[test]
    fn clipboard_receives_safe_literal_and_reaps_hanging_child() {
        let f = Fixture::new();
        let target = f.root.join("copied");
        let exe = f.script("clipboard", &format!("cat > '{}'", target.display()));
        copy_locator_with(&exe, "/path/$(touch NO_EXEC)").unwrap();
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "/path/$(touch NO_EXEC)"
        );
        assert!(!f.root.join("NO_EXEC").exists());
        copy_locator_with(&exe, "API_KEY=abcdef").unwrap();
        assert_eq!(fs::read_to_string(&target).unwrap(), "[redacted]");
        let fail = f.script("failed-copy", "exit 9");
        assert!(copy_locator_with(&fail, "x").is_err());
        let pid = f.root.join("pid");
        let hang = f.script(
            "hung-copy",
            &format!("echo $$ > '{}'\nexec /bin/sleep 20", pid.display()),
        );
        let mut child = Command::new(&hang)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while !pid.exists() {
            if child.try_wait().unwrap().is_some() || Instant::now() >= ready_deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned clipboard fixture did not enter script before ready deadline");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            fs::read_to_string(&pid).unwrap().trim(),
            child.id().to_string()
        );
        let start = Instant::now();
        assert_eq!(
            finish_clipboard_child(child, &"x".repeat(64 * 1024)),
            Err("clipboard deadline exceeded".into())
        );
        assert!(start.elapsed() < Duration::from_secs(4));
        let pid = fs::read_to_string(pid).unwrap();
        assert!(!Command::new("ps")
            .args(["-p", pid.trim(), "-o", "pid="])
            .output()
            .unwrap()
            .status
            .success());
    }
    #[test]
    fn navigation_refresh_detail_leave_all_project_bytes_and_request_count_unchanged() {
        fn tree(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
            let mut out = std::collections::BTreeMap::new();
            let mut dirs = vec![root.to_path_buf()];
            while let Some(dir) = dirs.pop() {
                for e in fs::read_dir(dir).unwrap() {
                    let e = e.unwrap();
                    let t = e.file_type().unwrap();
                    if t.is_dir() {
                        dirs.push(e.path())
                    } else if t.is_file() {
                        out.insert(e.path(), fs::read(e.path()).unwrap());
                    }
                }
            }
            out
        }
        let f = Fixture::new();
        let exe = f.root.join("provider");
        let original = fs::read_to_string(&exe).unwrap();
        fs::write(
            &exe,
            original.replace(
                "printf",
                &format!("echo call >> '{}'\nprintf", f.root.join("calls").display()),
            ),
        )
        .unwrap();
        f.run();
        let before = tree(&f.root);
        assert_eq!(fs::read_to_string(f.root.join("calls")).unwrap(), "call\n");
        let mut app = ObservationApp::open(&f.root).unwrap();
        for g in [Grouping::Calls, Grouping::Harness, Grouping::Tasks] {
            app.state.set_grouping(g);
            app.refresh().unwrap();
            app.show_detail().unwrap();
            app.state.render(80, 24);
            let _ = app.state.safe_locator();
        }
        drop(app);
        assert_eq!(tree(&f.root), before);
    }
    #[test]
    fn thousands_of_calls_group_and_redraw_without_quadratic_lookup() {
        let f = Fixture::new();
        f.run();
        let app = ObservationApp::open(&f.root).unwrap();
        let mut snap = app.state.snapshot.clone();
        let basis = snap.rows[0].clone();
        snap.rows = (0..3000)
            .map(|n| {
                let mut r = basis.clone();
                r.id = format!("call-{n:05}");
                r.alias = Some(format!("harness-{}", n % 7));
                r
            })
            .collect();
        let start = Instant::now();
        let mut state = AppState::new(snap);
        for g in [Grouping::Calls, Grouping::Harness, Grouping::Tasks] {
            state.set_grouping(g);
            assert_eq!(state.ordered_ids().len(), 3000);
            for _ in 0..10 {
                state.render(100, 24);
            }
        }
        assert!(start.elapsed() < Duration::from_secs(3));
    }
    #[test]
    fn public_tui_docs_describe_real_entry_and_recovery_boundaries() {
        let guide = include_str!("../../../docs/AI-MECHANICAL-GUIDE.md");
        for term in [
            "orch-tui --root",
            "2 seconds",
            "clipboard deadline",
            "Rust unwind",
            "both input/output TTY",
            "last observed",
        ] {
            assert!(guide.contains(term), "missing {term}");
        }
        let source = include_str!("app.rs");
        for term in [
            "Both input and output must be TTYs",
            "abort/SIGKILL",
            "never launches or stops a provider",
        ] {
            assert!(source.contains(term));
        }
    }

    // Test-only owned PTY; the production binary has no fault-injection flag.
    const GUARD_PTY: &str = r#"
import os,sys,pty,termios,fcntl,struct,subprocess,select,time,json,signal
m,s=pty.openpty();before=termios.tcgetattr(s);fcntl.ioctl(s,termios.TIOCSWINSZ,struct.pack('HHHH',24,100,0,0))
os.setsid();signal.signal(signal.SIGHUP,signal.SIG_IGN);fcntl.ioctl(s,termios.TIOCSCTTY,0)
env=dict(os.environ,B353_GUARD_CHILD='1',RUST_TEST_THREADS='4');p=subprocess.Popen([sys.argv[1],'--exact',sys.argv[2],'--nocapture'],stdin=s,stdout=s,stderr=s,env=env)
data=b'';raw=False;sent=False;end=time.monotonic()+15
try:
 while time.monotonic()<end:
  if select.select([m],[],[],0.05)[0]:
   try:data+=os.read(m,65536)
   except OSError:pass
  if b'B353_BODY_ENTERED' in data and not sent:
   flags=termios.tcgetattr(s)[3];raw=not(flags&termios.ICANON) and not(flags&termios.ECHO);os.write(m,b'x');sent=True
  if p.poll() is not None:
   drain_end=time.monotonic()+0.2
   while time.monotonic()<drain_end and select.select([m],[],[],0.02)[0]:
    try:chunk=os.read(m,65536)
    except OSError:break
    if not chunk:break
    data+=chunk
   break
 else:raise AssertionError('owned guard deadline')
 raw_after=termios.tcgetattr(s);pending=getattr(termios,'PENDIN',0)
 assert (raw_after[3]^before[3])&~pending==0
 assert bool(raw_after[3]&termios.ICANON) and bool(raw_after[3]&termios.ECHO)
 fcntl.ioctl(s,termios.FIONREAD,struct.pack('i',0))
 after=termios.tcgetattr(s);print(json.dumps({'raw_seen':raw,'restored':after==before,'raw_after':repr(raw_after),'before':repr(before),'after_query':repr(after),'pending_transition':raw_after[3]^after[3],'exit':p.returncode,'output':data.decode('utf8','replace')}))
 assert raw and after==before and p.returncode==0
 assert b'\x1b[?1049h' in data and b'\x1b[?1049l' in data and b'\x1b[?25h' in data
 assert b'1 passed' in data
finally:
 if p.poll() is None:p.kill();p.wait()
 os.close(m);os.close(s)
"#;
    fn guard_case(unwind: bool) {
        if std::env::var_os("B353_GUARD_CHILD").is_none() {
            let name = if unwind {
                "app::tests::terminal_session_body_unwind_restores"
            } else {
                "app::tests::terminal_session_body_error_restores"
            };
            let out = Command::new("python3")
                .args(["-c", GUARD_PTY])
                .arg(std::env::current_exe().unwrap())
                .arg(name)
                .output()
                .unwrap();
            println!("{}", String::from_utf8_lossy(&out.stdout));
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        assert!(io::stdin().is_terminal() && io::stdout().is_terminal());
        let result = std::panic::catch_unwind(|| {
            terminal_session::<()>(|_| {
                print!("B353_BODY_ENTERED");
                io::stdout().flush()?;
                let mut byte = [0];
                io::stdin().read_exact(&mut byte)?;
                if unwind {
                    panic!("owned test unwind")
                }
                Err(io::Error::other("owned body error"))
            })
        });
        if unwind {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
    }
    #[test]
    fn terminal_session_body_error_restores() {
        guard_case(false);
    }
    #[test]
    fn terminal_session_body_unwind_restores() {
        guard_case(true);
    }
}
