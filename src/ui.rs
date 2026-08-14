//! wrap pre-entry surface.
//!
//! Thesis: calibrated, threshold, quiet; never dashboard-like.
//! Signature: a layer rail that fills as snapshots land, then collapses
//! into one host → vm crossing.
//!
//! Primary: current layer or the crossing.
//! Supporting: completed marks, session kind.
//! Ambient: missing secrets, leftover sandboxes.
//!
//! Scrollback CLI + one bounded five-line live region during setup. Never alternate screen.

use std::{
    collections::VecDeque,
    env,
    io::{self, IsTerminal, Write},
    sync::Once,
    time::{Duration, Instant},
};

const PRIMARY: u32 = 0xeceff1; // porcelain
const MUTED: u32 = 0x78909c; // slate
const ACCENT: u32 = 0x4fc3f7; // cyan
const VM: u32 = 0xc4a7e7; // lavender identity
const OK: u32 = 0x81c784; // sage
const WARN: u32 = 0xf78c6c; // coral warning
const DANGER: u32 = 0xef5350; // red danger

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const SPIN_DELAY: Duration = Duration::from_millis(150);
const SPIN_EVERY: Duration = Duration::from_millis(90);
const HISTORY_CAP: usize = 48;
const LIVE_TAIL_ROWS: usize = 4;
const LIVE_ROWS: usize = LIVE_TAIL_ROWS + 1;
const MIN_WIDTH: usize = 20;
const DEFAULT_WIDTH: usize = 80;

const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const RESET: &str = "\x1b[0m";
const CLEAR_LINE: &str = "\r\x1b[2K";
const CURSOR_UP_LIVE: &str = "\x1b[4A";
const CURSOR_DOWN: &str = "\x1b[1B";

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub color: bool,
}

impl Theme {
    pub fn detect() -> Self {
        Self {
            color: color_enabled(),
        }
    }

    pub fn paint(self, hex: u32, text: &str) -> String {
        if !self.color || text.is_empty() {
            return text.to_string();
        }
        let r = (hex >> 16) & 0xff;
        let g = (hex >> 8) & 0xff;
        let b = hex & 0xff;
        format!("\x1b[38;2;{r};{g};{b}m{text}{RESET}")
    }

    pub fn primary(self, text: &str) -> String {
        self.paint(PRIMARY, text)
    }

    pub fn muted(self, text: &str) -> String {
        self.paint(MUTED, text)
    }

    pub fn accent(self, text: &str) -> String {
        self.paint(ACCENT, text)
    }

    pub fn vm(self, text: &str) -> String {
        self.paint(VM, text)
    }

    pub fn ok(self, text: &str) -> String {
        self.paint(OK, text)
    }

    pub fn warn(self, text: &str) -> String {
        self.paint(WARN, text)
    }

    pub fn danger(self, text: &str) -> String {
        self.paint(DANGER, text)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrossingKind {
    New,
    Reused,
    Reset,
}

impl CrossingKind {
    fn label(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reused => "reused",
            Self::Reset => "reset",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerEnd {
    Reused,
    Snapped,
}

pub struct Ui {
    theme: Theme,
    interactive: bool,
}

impl Ui {
    pub fn stderr() -> Self {
        install_cursor_restore();
        Self {
            theme: Theme::detect(),
            interactive: io::stderr().is_terminal(),
        }
    }

    pub fn setting_up_base(&self) {
        self.hide_cursor();
        let _ = writeln!(io::stderr(), "{}", format_setup(self.theme, "base"));
    }

    pub fn setting_up_project(&self) {
        self.hide_cursor();
        let _ = writeln!(io::stderr(), "{}", format_setup(self.theme, "project"));
    }

    fn hide_cursor(&self) {
        if self.interactive {
            let _ = write!(io::stderr(), "{HIDE_CURSOR}");
            let _ = io::stderr().flush();
        }
    }

    pub fn rebuild(&self, layers: usize) {
        let _ = writeln!(io::stderr(), "{}", format_rebuild(self.theme, layers));
    }

    pub fn layer_reused(&self, id: &str) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_layer_end(self.theme, id, LayerEnd::Reused)
        );
    }

    pub fn start_layer(&self, id: &str) -> Live {
        self.start_task(id)
    }

    pub fn start_task(&self, id: &str) -> Live {
        Live::new(self.theme, self.interactive, id)
    }

    pub fn leftover(&self, name: &str, err: &dyn std::fmt::Display) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_leftover(self.theme, name, &err.to_string())
        );
    }

    pub fn crossing(
        &self,
        host: &str,
        workspace: &str,
        kind: CrossingKind,
        cpus: u8,
        memory_mib: u32,
        memory_max_mib: u32,
    ) {
        let line = format_crossing(
            self.theme,
            stderr_width(),
            host,
            workspace,
            kind,
            cpus,
            memory_mib,
            memory_max_mib,
        );
        let _ = writeln!(io::stderr(), "{line}");
    }

    pub fn secret_access(&self, secrets: &[(&str, &[String])]) {
        let _ = writeln!(
            io::stderr(),
            "no secrets are exposed to the VM directly, but it can use the following pseudo tokens with the following hosts:"
        );
        if secrets.is_empty() {
            let _ = writeln!(io::stderr(), "  (none)");
            return;
        }
        for (env, hosts) in secrets.iter().take(4) {
            let hosts = hosts
                .iter()
                .map(|host| sanitize(host))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                io::stderr(),
                "  {}: {}",
                self.theme.muted(&hosts),
                self.theme.primary(&sanitize(env))
            );
        }
        if secrets.len() > 4 {
            let _ = writeln!(
                io::stderr(),
                "  {}",
                self.theme
                    .muted(&format!("[... and {} more]", secrets.len() - 4))
            );
        }
    }

    pub fn attached(&self) {
        let _ = writeln!(io::stderr(), "  {}", self.theme.ok("fully attached"));
    }

    pub fn stop_failed(&self, err: &dyn std::fmt::Display) {
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_stop_failed(self.theme, &err.to_string())
        );
    }

    pub fn fatal(&self, err: &dyn std::fmt::Display) {
        restore_terminal();
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_fatal(self.theme, &err.to_string())
        );
    }
}

pub struct Live {
    theme: Theme,
    interactive: bool,
    layer: String,
    phase: &'static str,
    spinner_i: usize,
    started: Instant,
    last_draw: Instant,
    lines: LineBuf,
    carry: Vec<u8>,
    cursor_hidden: bool,
    finished: bool,
    rendered: bool,
}

impl Live {
    fn new(theme: Theme, interactive: bool, id: &str) -> Self {
        let now = Instant::now();
        Self {
            theme,
            interactive,
            layer: sanitize(id),
            phase: "",
            spinner_i: 0,
            started: now,
            last_draw: now,
            lines: LineBuf::new(),
            carry: Vec::new(),
            cursor_hidden: false,
            finished: false,
            rendered: false,
        }
    }

    pub fn feed_stdout(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.feed(chunk, Stream::Stdout)
    }

    pub fn feed_stderr(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.feed(chunk, Stream::Stderr)
    }

    fn feed(&mut self, chunk: &[u8], stream: Stream) -> io::Result<()> {
        if !self.interactive {
            match stream {
                Stream::Stdout => {
                    io::stdout().write_all(chunk)?;
                    io::stdout().flush()?;
                }
                Stream::Stderr => {
                    io::stderr().write_all(chunk)?;
                    io::stderr().flush()?;
                }
            }
            return Ok(());
        }
        self.carry.extend_from_slice(chunk);
        let valid = match std::str::from_utf8(&self.carry) {
            Ok(_) => self.carry.len(),
            Err(err) => err.valid_up_to(),
        };
        if valid == 0 {
            return Ok(());
        }
        let text = String::from_utf8(self.carry.drain(..valid).collect()).unwrap_or_default();
        self.lines.push(&text);
        self.draw(false)?;
        Ok(())
    }

    pub fn tick(&mut self) -> io::Result<()> {
        if !self.interactive || self.finished {
            return Ok(());
        }
        if self.last_draw.elapsed() < SPIN_EVERY {
            return Ok(());
        }
        if self.started.elapsed() >= SPIN_DELAY {
            self.spinner_i = self.spinner_i.wrapping_add(1);
        }
        self.draw(true)
    }

    pub fn phase(&mut self, phase: &'static str) -> io::Result<()> {
        self.phase = phase;
        self.draw(true)
    }

    pub fn succeed(&mut self) {
        if self.finished {
            return;
        }
        self.clear_live();
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_layer_end(self.theme, &self.layer, LayerEnd::Snapped)
        );
        self.finish();
    }

    pub fn done(&mut self) {
        if self.finished {
            return;
        }
        self.clear_live();
        self.finish();
    }

    pub fn fail(&mut self, detail: &str) {
        if self.finished {
            return;
        }
        self.clear_live();
        let _ = writeln!(
            io::stderr(),
            "{}",
            format_layer_fail(self.theme, &self.layer, detail)
        );
        for line in self.lines.history() {
            let _ = writeln!(io::stderr(), "    {}", self.theme.muted(line));
        }
        self.finish();
    }

    pub fn interrupt(&mut self) {
        self.fail("interrupted");
    }

    fn draw(&mut self, force: bool) -> io::Result<()> {
        if !self.interactive || self.finished {
            return Ok(());
        }
        let spinning = self.started.elapsed() >= SPIN_DELAY;
        if !force && !spinning && self.phase.is_empty() && self.lines.snippet().is_empty() {
            return Ok(());
        }
        if spinning && !self.cursor_hidden {
            io::stderr().write_all(HIDE_CURSOR.as_bytes())?;
            self.cursor_hidden = true;
        }
        let spin = if spinning {
            SPINNER[self.spinner_i % SPINNER.len()]
        } else {
            ' '
        };
        let region = format_live_region(
            self.theme,
            stderr_width(),
            &self.layer,
            spin,
            self.phase,
            &self.lines,
        );
        let mut frame = String::with_capacity(region.len() + 64);
        if self.rendered {
            frame.push('\r');
            frame.push_str(CURSOR_UP_LIVE);
        }
        for (index, line) in region.split('\n').enumerate() {
            frame.push_str(CLEAR_LINE);
            frame.push_str(line);
            if index + 1 < LIVE_ROWS {
                if self.rendered {
                    frame.push_str(CURSOR_DOWN);
                } else {
                    frame.push('\n');
                }
            }
        }
        io::stderr().write_all(frame.as_bytes())?;
        io::stderr().flush()?;
        self.rendered = true;
        self.last_draw = Instant::now();
        Ok(())
    }

    fn clear_live(&mut self) {
        if !self.interactive || !self.rendered {
            return;
        }
        let mut frame = String::from("\r");
        frame.push_str(CURSOR_UP_LIVE);
        for row in 0..LIVE_ROWS {
            frame.push_str("\x1b[2K");
            if row + 1 < LIVE_ROWS {
                frame.push_str(CURSOR_DOWN);
            }
        }
        frame.push('\r');
        frame.push_str(CURSOR_UP_LIVE);
        let _ = io::stderr().write_all(frame.as_bytes());
        let _ = io::stderr().flush();
        self.rendered = false;
    }

    fn finish(&mut self) {
        self.finished = true;
        self.show_cursor();
    }

    fn show_cursor(&mut self) {
        if self.cursor_hidden {
            restore_terminal();
            self.cursor_hidden = false;
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if !self.finished {
            self.clear_live();
        }
        self.show_cursor();
    }
}

#[derive(Clone, Copy)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub struct LineBuf {
    partial: String,
    history: VecDeque<String>,
}

impl LineBuf {
    pub fn new() -> Self {
        Self {
            partial: String::new(),
            history: VecDeque::new(),
        }
    }

    pub fn push(&mut self, text: &str) {
        for ch in text.chars() {
            match ch {
                '\n' => self.commit(),
                '\r' => self.partial.clear(),
                c if c.is_control() && c != '\u{1b}' => {}
                c => self.partial.push(c),
            }
        }
    }

    fn commit(&mut self) {
        let line = sanitize(&std::mem::take(&mut self.partial));
        if line.is_empty() {
            return;
        }
        if self.history.len() == HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(line);
    }

    pub fn snippet(&self) -> String {
        let current = sanitize(&self.partial);
        if !current.is_empty() {
            return current;
        }
        self.history.back().cloned().unwrap_or_default()
    }

    pub fn history(&self) -> impl Iterator<Item = &str> {
        self.history.iter().map(String::as_str)
    }
}

pub fn format_crossing(
    theme: Theme,
    width: usize,
    host: &str,
    workspace: &str,
    kind: CrossingKind,
    cpus: u8,
    memory_mib: u32,
    memory_max_mib: u32,
) -> String {
    let host = sanitize(host);
    let workspace = sanitize(workspace);
    let kind = kind.label();
    let resources = format_resources(cpus, memory_mib, memory_max_mib);
    let width = width.max(MIN_WIDTH);

    let base_width = |h: &str, w: &str| display_width(h) + 6 + display_width(w);
    let suffix_width = |text: &str| 5 + display_width(text);
    let base = base_width(&host, &workspace);
    if base <= width {
        let with_kind = base + suffix_width(kind);
        let with_resources = base + suffix_width(&resources);
        if with_kind + suffix_width(&resources) <= width {
            return render_crossing(theme, &host, &workspace, Some(kind), Some(&resources));
        }
        if with_resources <= width {
            return render_crossing(theme, &host, &workspace, None, Some(&resources));
        }
        if with_kind <= width {
            return render_crossing(theme, &host, &workspace, Some(kind), None);
        }
        return render_crossing(theme, &host, &workspace, None, None);
    }

    let budget = width.saturating_sub(base_width("", "").max(6));
    let host_budget = (budget / 3).max(2);
    let workspace_budget = budget.saturating_sub(host_budget).max(2);
    render_crossing(
        theme,
        &truncate_cells(&host, host_budget),
        &truncate_cells(&workspace, workspace_budget),
        None,
        None,
    )
}

fn render_crossing(
    theme: Theme,
    host: &str,
    workspace: &str,
    kind: Option<&str>,
    resources: Option<&str>,
) -> String {
    let mut line = format!(
        "{} {} {}{}",
        theme.accent(host),
        theme.muted("→"),
        theme.vm("vm:"),
        theme.primary(workspace)
    );
    if let Some(kind) = kind {
        line.push_str(&format!("  {}  {}", theme.muted("·"), theme.muted(kind)));
    }
    if let Some(resources) = resources {
        line.push_str(&format!(
            "  {}  {}",
            theme.muted("·"),
            theme.primary(resources)
        ));
    }
    line
}

fn format_resources(cpus: u8, memory_mib: u32, memory_max_mib: u32) -> String {
    let memory = if memory_mib == memory_max_mib {
        format_memory(memory_mib)
    } else if memory_mib % 1024 == 0 && memory_max_mib % 1024 == 0 {
        format!("{}–{} GiB", memory_mib / 1024, memory_max_mib / 1024)
    } else {
        format!("{memory_mib}–{memory_max_mib} MiB")
    };
    format!("{cpus} CPU · {memory}")
}

fn format_memory(memory_mib: u32) -> String {
    if memory_mib % 1024 == 0 {
        format!("{} GiB", memory_mib / 1024)
    } else {
        format!("{memory_mib} MiB")
    }
}

pub fn format_layer_end(theme: Theme, id: &str, end: LayerEnd) -> String {
    let id = sanitize(id);
    match end {
        LayerEnd::Reused => format!("  {}  {}", theme.muted("·"), theme.muted(&id)),
        LayerEnd::Snapped => format!("  {}  {}", theme.ok("●"), theme.primary(&id)),
    }
}

pub fn format_layer_fail(theme: Theme, id: &str, detail: &str) -> String {
    let id = sanitize(id);
    let detail = sanitize(detail);
    format!(
        "  {}  {}  {}  {}",
        theme.danger("×"),
        theme.primary(&id),
        theme.muted("·"),
        theme.danger(&detail)
    )
}

pub fn format_live_line(theme: Theme, width: usize, id: &str, spin: char, phase: &str) -> String {
    let id = sanitize(id);
    let phase = sanitize(phase);
    let width = width.max(MIN_WIDTH);
    let spin_s = spin.to_string();
    let mark = if spin == ' ' {
        theme.muted(&spin_s)
    } else {
        theme.vm(&spin_s)
    };

    let prefix_plain = format!("  {spin} {id}");
    let prefix_w = display_width(&prefix_plain);
    if prefix_w >= width {
        return format!(
            "  {} {}",
            mark,
            theme.primary(&truncate_cells(&id, width.saturating_sub(4)))
        );
    }

    if phase.is_empty() {
        return format!("  {} {}", mark, theme.primary(&id));
    }

    let room = width.saturating_sub(prefix_w + 2);
    if room < 2 {
        return format!("  {} {}", mark, theme.primary(&id));
    }
    let clip = truncate_cells(&phase, room);
    format!("  {} {}  {}", mark, theme.primary(&id), theme.muted(&clip))
}

fn format_live_region(
    theme: Theme,
    width: usize,
    id: &str,
    spin: char,
    phase: &str,
    lines: &LineBuf,
) -> String {
    let mut out = format_live_line(theme, width, id, spin, phase);
    let partial = sanitize(&lines.partial);
    let history_limit = LIVE_TAIL_ROWS - usize::from(!partial.is_empty());
    let history_start = lines.history.len().saturating_sub(history_limit);
    let mut tail_rows = 0;
    for line in lines.history.iter().skip(history_start) {
        out.push('\n');
        out.push_str(&format_live_tail(theme, width, line));
        tail_rows += 1;
    }
    if !partial.is_empty() {
        out.push('\n');
        out.push_str(&format_live_tail(theme, width, &partial));
        tail_rows += 1;
    }
    while tail_rows < LIVE_TAIL_ROWS {
        out.push('\n');
        tail_rows += 1;
    }
    out
}

fn format_live_tail(theme: Theme, width: usize, line: &str) -> String {
    let line = truncate_cells(line, width.max(MIN_WIDTH).saturating_sub(4));
    format!("    {}", theme.muted(&line))
}

pub fn format_setup(theme: Theme, target: &str) -> String {
    theme.primary(&format!("setting up {} vm", sanitize(target)))
}

pub fn format_rebuild(theme: Theme, layers: usize) -> String {
    format!(
        "  {}  {}",
        theme.warn("rebuild"),
        theme.muted(&format!("{layers} layers"))
    )
}

pub fn format_leftover(theme: Theme, name: &str, err: &str) -> String {
    format!(
        "  {}  {}  {}  {}",
        theme.muted("leftover"),
        theme.primary(&sanitize(name)),
        theme.muted("·"),
        theme.muted(&sanitize(err))
    )
}

pub fn format_stop_failed(theme: Theme, err: &str) -> String {
    format!(
        "  {}  {}  {}",
        theme.danger("stop failed"),
        theme.muted("·"),
        theme.muted(&sanitize(err))
    )
}

pub fn format_fatal(theme: Theme, err: &str) -> String {
    format!(
        "{}  {}",
        theme.danger("wrap"),
        theme.primary(&sanitize(err))
    )
}

pub fn color_enabled() -> bool {
    if env_flag_set("NO_COLOR") || env_is("CLICOLOR", "0") {
        return false;
    }
    if env_is("CLICOLOR_FORCE", "1") {
        return true;
    }
    io::stderr().is_terminal()
}

fn env_flag_set(key: &str) -> bool {
    env::var_os(key).is_some_and(|v| !v.is_empty())
}

fn env_is(key: &str, want: &str) -> bool {
    env::var(key).ok().as_deref() == Some(want)
}

pub fn stderr_width() -> usize {
    tty_cols()
        .or_else(|| env::var("COLUMNS").ok()?.parse().ok())
        .filter(|&n| n >= MIN_WIDTH)
        .unwrap_or(DEFAULT_WIDTH)
}

fn tty_cols() -> Option<usize> {
    #[repr(C)]
    struct WinSize {
        row: u16,
        col: u16,
        x: u16,
        y: u16,
    }

    #[cfg(unix)]
    {
        unsafe extern "C" {
            fn ioctl(fd: i32, op: std::os::raw::c_ulong, ws: *mut WinSize) -> i32;
        }
        const TIOCGWINSZ: std::os::raw::c_ulong = 0x5413;
        let mut ws = WinSize {
            row: 0,
            col: 0,
            x: 0,
            y: 0,
        };
        let ok = unsafe { ioctl(2, TIOCGWINSZ, &mut ws) == 0 && ws.col > 0 };
        if ok { Some(ws.col as usize) } else { None }
    }
    #[cfg(not(unix))]
    {
        let _ = WinSize {
            row: 0,
            col: 0,
            x: 0,
            y: 0,
        };
        None
    }
}

pub fn display_width(s: &str) -> usize {
    s.chars().map(char_width).sum()
}

fn char_width(c: char) -> usize {
    if c == '\0' || c.is_control() {
        0
    } else if is_wide(c) {
        2
    } else {
        1
    }
}

fn is_wide(c: char) -> bool {
    matches!(
        c as u32,
        0x1100..=0x115F
            | 0x2329
            | 0x232A
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE10..=0xFE19
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x1F900..=0x1F9FF
    )
}

pub fn truncate_cells(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if display_width(s) <= max {
        return s.to_string();
    }
    if max == 1 {
        return "…".into();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = char_width(c);
        if w + cw > max - 1 {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for x in chars.by_ref() {
                    if x.is_ascii_alphabetic() || x == '~' {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(x) = chars.next() {
                    if x == '\u{7}' {
                        break;
                    }
                    if x == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

pub fn sanitize(s: &str) -> String {
    strip_ansi(s).chars().filter(|c| !c.is_control()).collect()
}

fn install_cursor_restore() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            prev(info);
        }));
    });
}

pub fn restore_terminal() {
    let _ = write!(io::stderr(), "{SHOW_CURSOR}{RESET}");
    let _ = io::stderr().flush();
}

pub fn spin_period() -> Duration {
    SPIN_EVERY
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> Theme {
        Theme { color: false }
    }

    fn color() -> Theme {
        Theme { color: true }
    }

    #[test]
    fn theme_tokens_are_distinct() {
        let t = color();
        let primary = t.primary("x");
        let danger = t.danger("x");
        let accent = t.accent("x");
        let ok = t.ok("x");
        let warn = t.warn("x");
        let vm = t.vm("x");
        assert!(primary.contains("38;2;236;239;241m"));
        assert!(danger.contains("38;2;239;83;80m"));
        assert!(accent.contains("38;2;79;195;247m"));
        assert!(ok.contains("38;2;129;199;132m"));
        assert!(warn.contains("38;2;247;140;108m"));
        assert!(vm.contains("38;2;196;167;231m"));
        assert_ne!(primary, danger);
        assert_ne!(accent, vm);
        assert_ne!(ok, warn);
        assert!(t.paint(PRIMARY, "").is_empty());
        assert_eq!(plain().danger("x"), "x");
    }

    #[test]
    fn crossing_states_and_widths() {
        let t = plain();
        let wide = format_crossing(
            t,
            80,
            "tobi-xe",
            "2026-08-13-smolvm",
            CrossingKind::Reused,
            4,
            4096,
            8192,
        );
        assert_eq!(
            wide,
            "tobi-xe → vm:2026-08-13-smolvm  ·  reused  ·  4 CPU · 4–8 GiB"
        );
        assert!(
            format_crossing(t, 80, "h", "w", CrossingKind::New, 2, 4096, 4096)
                .contains("new  ·  2 CPU · 4 GiB")
        );
        assert!(
            format_crossing(t, 80, "h", "w", CrossingKind::Reset, 8, 6144, 8192)
                .ends_with("8 CPU · 6–8 GiB")
        );

        let narrow = format_crossing(
            t,
            20,
            "very-long-hostname",
            "very-long-workspace",
            CrossingKind::Reused,
            4,
            4096,
            8192,
        );
        assert!(display_width(&narrow) <= 20);
        assert!(!narrow.contains("reused"));
        assert!(narrow.contains('…'));
    }

    #[test]
    fn layer_marks_differ_without_color() {
        let t = plain();
        assert_eq!(
            format_layer_end(t, "packages", LayerEnd::Reused),
            "  ·  packages"
        );
        assert_eq!(
            format_layer_end(t, "packages", LayerEnd::Snapped),
            "  ●  packages"
        );
        assert_eq!(
            format_layer_fail(t, "languages", "exit 1"),
            "  ×  languages  ·  exit 1"
        );
        assert_eq!(
            format_layer_fail(t, "languages", "interrupted"),
            "  ×  languages  ·  interrupted"
        );
    }

    #[test]
    fn live_line_aligns_spinner_and_truncates_phase() {
        let t = plain();
        let line = format_live_line(t, 28, "languages", '⠋', "pacman -S gcc clang make cmake");
        assert!(line.starts_with("  ⠋ languages  "));
        assert!(line.contains('…'));
        assert!(display_width(&line) <= 28);
        assert_eq!(format_live_line(t, 80, "tools", ' ', ""), "    tools");
        assert_eq!(line.chars().position(|c| c == '⠋'), Some(2));
        assert_eq!(
            format_layer_end(t, "languages", LayerEnd::Reused)
                .chars()
                .position(|c| c == '·'),
            Some(2)
        );
    }

    #[test]
    fn live_region_streams_the_latest_four_lines() {
        let t = plain();
        let mut lines = LineBuf::new();
        lines.push("line-1\nline-2\nline-3\nline-4\nline-5\npartial\x1b[31m");
        let region = format_live_region(t, 80, "system", '⠋', "running setup", &lines);
        assert_eq!(
            region.lines().collect::<Vec<_>>(),
            [
                "  ⠋ system  running setup",
                "    line-3",
                "    line-4",
                "    line-5",
                "    partial",
            ]
        );
        assert!(!region.contains('\u{1b}'));
    }

    #[test]
    fn status_lines_cover_remaining_states() {
        let t = plain();
        assert_eq!(format_rebuild(t, 4), "  rebuild  4 layers");
        assert_eq!(format_setup(t, "base"), "setting up base vm");
        assert_eq!(format_setup(t, "project"), "setting up project vm");
        assert_eq!(
            format_leftover(t, "wrap-build-languages", "busy"),
            "  leftover  wrap-build-languages  ·  busy"
        );
        assert_eq!(
            format_stop_failed(t, "timeout"),
            "  stop failed  ·  timeout"
        );
        assert_eq!(
            format_fatal(t, "layer languages failed"),
            "wrap  layer languages failed"
        );
    }

    #[test]
    fn color_is_optional_decoration() {
        let p = plain();
        let c = color();
        let cases = [
            format_crossing(p, 80, "host", "ws", CrossingKind::New, 4, 4096, 8192),
            format_layer_end(p, "packages", LayerEnd::Snapped),
            format_layer_fail(p, "languages", "exit 1"),
            format_live_line(p, 80, "tools", '⠋', "mise use rust"),
            format_setup(p, "base"),
            format_rebuild(p, 4),
            format_fatal(p, "interrupted"),
        ];
        let colored = [
            format_crossing(c, 80, "host", "ws", CrossingKind::New, 4, 4096, 8192),
            format_layer_end(c, "packages", LayerEnd::Snapped),
            format_layer_fail(c, "languages", "exit 1"),
            format_live_line(c, 80, "tools", '⠋', "mise use rust"),
            format_setup(c, "base"),
            format_rebuild(c, 4),
            format_fatal(c, "interrupted"),
        ];
        for (plain_line, color_line) in cases.iter().zip(colored.iter()) {
            assert_eq!(plain_line, &strip_ansi(color_line));
            assert!(!plain_line.contains('\u{1b}'));
            assert!(color_line.contains("\x1b[38;2;"));
        }
    }

    #[test]
    fn line_buf_handles_cr_and_ansi() {
        let mut buf = LineBuf::new();
        buf.push("wrap: layer packages\n");
        buf.push("downloading\r");
        buf.push("\x1b[32m100%\x1b[0m\n");
        buf.push("partial");
        assert_eq!(buf.snippet(), "partial");
        let hist: Vec<_> = buf.history().collect();
        assert_eq!(hist, ["wrap: layer packages", "100%"]);
    }

    #[test]
    fn sanitize_strips_controls_and_csi() {
        assert_eq!(sanitize("a\x1b[31mb\u{7}c"), "abc");
        assert_eq!(display_width("日本語"), 6);
        assert_eq!(truncate_cells("日本語", 5), "日本…");
        assert_eq!(truncate_cells("abc", 10), "abc");
    }

    #[test]
    fn history_is_bounded() {
        let mut buf = LineBuf::new();
        for i in 0..80 {
            buf.push(&format!("line-{i}\n"));
        }
        assert_eq!(buf.history().count(), HISTORY_CAP);
        assert_eq!(buf.history().next(), Some("line-32"));
    }
}
