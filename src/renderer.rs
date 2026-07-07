use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::cli::OutputFormat;
use crate::tools::ToolDisplay;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const OSC8_OPEN: &str = "\x1b]8;;";
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";
const OSC8_ST: &str = "\x1b\\";
const RED: &str = "\x1b[91m";
const GREEN: &str = "\x1b[92m";
const YELLOW: &str = "\x1b[93m";
const BLUE: &str = "\x1b[94m";
const CYAN: &str = "\x1b[96m";
const GRAY: &str = "\x1b[90m";
pub(crate) const BASH_COMMAND_PREVIEW_BYTES: usize = 160;
pub(crate) const BASH_TITLE_PREVIEW_BYTES: usize = 120;
const GUARDRAIL_REASON_PREVIEW_BYTES: usize = 180;
const BASH_HEAD_LINE_BUDGET: usize = 3;
const BASH_HEAD_BYTE_BUDGET: usize = 1024;
const BASH_HEAD_LINE_CAP_BYTES: usize = 256;
const BASH_TAIL_LINE_RESERVE: usize = 2;
const BASH_TAIL_FALLBACK_BYTES: usize = 512;
const BASH_TAIL_LINE_CAP_BYTES: usize = 256;
pub(crate) const ELLIPSIS: &str = "…";

pub struct Renderer {
    stdout: io::Stdout,
    stderr: io::Stderr,
    stdout_at_line_start: bool,
    trailing_newlines: usize,
    has_committed_stdout: bool,
    styled: bool,
    markdown: MarkdownStream,
    assistant_block_open: bool,
    live_line: Option<LiveLine>,
    live_line_rendered: bool,
    reasoning: Option<ReasoningState>,
    bash_preview: Option<BashPreviewState>,
    turn_done_bell_min_duration: Option<Duration>,
    final_only: bool,
}

impl Renderer {
    pub fn new() -> Self {
        Self::with_format(OutputFormat::Terminal)
    }

    pub fn with_format(format: OutputFormat) -> Self {
        Self::with_terminal_bell(format, None)
    }

    pub fn with_terminal_bell(
        format: OutputFormat,
        turn_done_bell_min_duration: Option<Duration>,
    ) -> Self {
        let stdout = io::stdout();
        Self {
            styled: format == OutputFormat::Terminal && stdout.is_terminal(),
            stdout,
            stderr: io::stderr(),
            stdout_at_line_start: true,
            trailing_newlines: 0,
            has_committed_stdout: false,
            markdown: MarkdownStream::default(),
            assistant_block_open: false,
            live_line: None,
            live_line_rendered: false,
            reasoning: None,
            bash_preview: None,
            turn_done_bell_min_duration,
            final_only: format == OutputFormat::Final,
        }
    }

    #[cfg(test)]
    pub(crate) fn force_styled_for_test(&mut self) {
        self.styled = true;
    }

    pub fn assistant_text(&mut self, text: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        if !self.styled {
            if !self.assistant_block_open {
                self.ensure_block_separator_if_needed()?;
                self.assistant_block_open = true;
            }
            return self.write_stdout_committed(text);
        }

        let blocks = self.markdown.push(text);
        if blocks.is_empty() {
            return self.render_live_line();
        }

        for block in blocks {
            if !self.assistant_block_open {
                self.ensure_block_separator_if_needed()?;
                self.assistant_block_open = true;
            }
            let rendered = render_markdown(&block);
            self.write_committed(&rendered)?;
        }
        self.render_live_line()
    }

    pub fn assistant_end(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if !self.styled {
            self.assistant_block_open = false;
            return Ok(());
        }
        let Some(block) = self.markdown.finish() else {
            self.assistant_block_open = false;
            return Ok(());
        };
        if self.styled && !self.assistant_block_open {
            self.ensure_block_separator_if_needed()?;
        }
        self.assistant_block_open = false;
        let rendered = render_markdown(&block);
        self.write_committed(&rendered)?;
        self.render_live_line()
    }

    pub fn reasoning_start(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.assistant_block_open = false;
        self.reasoning = Some(ReasoningState {
            started: Instant::now(),
            reasoning_chars: 0,
            committed: false,
        });
        Ok(())
    }

    pub fn reasoning_delta(&mut self, text: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        let Some(reasoning) = self.reasoning.as_mut() else {
            return Ok(());
        };
        reasoning.reasoning_chars = reasoning
            .reasoning_chars
            .saturating_add(text.chars().count());
        if reasoning.committed {
            return Ok(());
        }
        if !self.styled {
            return Ok(());
        }
        self.live_line = Some(LiveLine::Thinking);
        self.render_live_line()
    }

    pub fn thinking_tick(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if !matches!(self.live_line, Some(LiveLine::Thinking)) {
            return Ok(());
        }
        self.render_live_line()
    }

    pub fn reasoning_end(&mut self, usage: Option<(u64, u64)>) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        let Some(reasoning) = self.reasoning.as_mut() else {
            return Ok(());
        };
        if reasoning.committed || reasoning.reasoning_chars == 0 {
            return Ok(());
        }
        let line = format_thought_line(
            reasoning.started.elapsed(),
            reasoning.reasoning_chars,
            usage,
            self.styled,
        );
        reasoning.committed = true;
        self.live_line = None;
        self.ensure_block_separator_if_needed()?;
        self.write_committed(&line)
    }

    pub fn bash_header_start(&mut self, _tool_call_id: Option<&str>) -> io::Result<bool> {
        if self.final_only {
            return Ok(true);
        }
        self.assistant_block_open = false;
        self.reasoning_end(None)?;
        self.live_line = None;
        self.ensure_block_separator_if_needed()?;
        if self.styled {
            self.write_committed(&format!("{GRAY}# {RESET}{BOLD}"))?;
        } else {
            self.write_committed("# ")?;
        }
        Ok(true)
    }

    pub fn bash_header_title_delta(&mut self, text: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        self.write_committed(text)
    }

    pub fn bash_header_title_end(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled {
            self.write_committed(&format!("{RESET}\n"))
        } else {
            self.write_committed("\n")
        }
    }

    pub fn bash_header_script_start(&mut self, risk: Option<&str>) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled {
            self.write_committed(&format!("{DIM}${RESET} {}{BOLD}", bash_risk_color(risk)))
        } else {
            let mut out = String::from("$ ");
            if let Some(risk) = risk {
                out.push_str(&format_risk_label(risk, false));
                out.push(' ');
            }
            self.write_committed(&out)
        }
    }

    pub fn bash_header_script_delta(&mut self, text: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        self.write_committed(text)
    }

    pub fn bash_header_script_end(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled {
            self.write_committed(&format!("{RESET}\n"))
        } else {
            self.write_committed("\n")
        }
    }

    pub fn bash_header_full(
        &mut self,
        tool_call_id: Option<&str>,
        args: &serde_json::Value,
    ) -> io::Result<bool> {
        if self.final_only {
            return Ok(true);
        }
        let title = args
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let script = args
            .get("script")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let risk = args.get("risk").and_then(|value| value.as_str());
        self.bash_header_start(tool_call_id)?;
        self.bash_header_title_delta(&preview_first_line(title, BASH_TITLE_PREVIEW_BYTES))?;
        self.bash_header_title_end()?;
        self.bash_header_script_start(risk)?;
        self.bash_header_script_delta(&preview_first_line(script, BASH_COMMAND_PREVIEW_BYTES))?;
        self.bash_header_script_end()?;
        Ok(true)
    }

    pub fn cancel_live_state(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.clear_live_line()?;
        self.live_line = None;
        self.reasoning = None;
        self.bash_preview = None;
        Ok(())
    }

    /// Completion-only tools are silent here. Bash is the exception because
    /// its live output needs a visible command header.
    pub fn tool_start(
        &mut self,
        _tool_call_id: Option<&str>,
        name: &str,
        args: &serde_json::Value,
        header_already_rendered: bool,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if name != "bash" {
            return Ok(());
        }
        self.assistant_block_open = false;
        self.reasoning_end(None)?;
        self.live_line = None;
        self.bash_preview = Some(BashPreviewState::default());
        if header_already_rendered {
            return Ok(());
        }
        self.bash_header_full(None, args).map(|_| ())
    }

    pub fn bash_output(
        &mut self,
        _tool_call_id: Option<&str>,
        _tool: &str,
        text: &str,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        let sanitized = strip_ansi(text);
        let Some(preview) = self.bash_preview.as_mut() else {
            return if self.styled {
                self.write_committed(&format!("{GRAY}{sanitized}{RESET}"))
            } else {
                self.write_committed(&sanitized)
            };
        };
        preview.raw.push_str(&sanitized);
        let snapshot = compute_bash_preview_snapshot(&preview.raw, false);
        if snapshot.head_rendered.len() > preview.committed_head_len {
            let next = snapshot.head_rendered[preview.committed_head_len..].to_string();
            preview.committed_head_len = snapshot.head_rendered.len();
            if self.styled {
                self.write_committed(&format!("{GRAY}{next}{RESET}"))?;
            } else {
                self.write_committed(&next)?;
            }
        }
        if self.styled {
            self.set_omitted_live_line(snapshot.omitted_lines, snapshot.omitted_bytes)
        } else {
            Ok(())
        }
    }

    pub fn tool_finished(
        &mut self,
        _tool_call_id: Option<&str>,
        _tool: &str,
        display: &ToolDisplay,
        elapsed: Duration,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.finalize_bash_preview()?;
        let text = format_tool(display, elapsed, self.styled);
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_line_start()?;
        self.write_stdout_committed(&terminal_trim_committed_text(&text))
    }

    pub fn tool_failed(
        &mut self,
        _tool_call_id: Option<&str>,
        name: &str,
        error: &str,
        elapsed: Duration,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.finalize_bash_preview()?;
        self.ensure_line_start()?;
        let elapsed = format_duration(elapsed);
        let line = if self.styled {
            format!(
                "{RED}✗ {BOLD}{name} failed{RESET}{RED}: {error}{RESET}{DIM} · {elapsed}{RESET}\n"
            )
        } else {
            format!("✗ {name} failed: {error} · {elapsed}\n")
        };
        self.write_stdout_committed(&terminal_trim_committed_text(&line))
    }

    pub fn guardrail_verdict(
        &mut self,
        allowed: bool,
        risk_level: &str,
        user_auth_level: &str,
        reason: &str,
        _script: &str,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.assistant_block_open = false;
        self.ensure_line_start()?;
        let verdict = if allowed { "allow" } else { "deny" };
        let reason_preview = preview_first_line(reason, GUARDRAIL_REASON_PREVIEW_BYTES);
        let line = if self.styled {
            let (color, verdict) = if allowed {
                (GREEN, "allow")
            } else {
                (RED, "deny")
            };
            format!(
                "{color}[guardrail: {verdict}]{RESET} {DIM}risk={risk_level} auth={user_auth_level} — {reason_preview}{RESET}\n"
            )
        } else {
            format!(
                "[guardrail: {verdict}] risk={risk_level} auth={user_auth_level} — {reason_preview}\n"
            )
        };
        self.write_stdout_committed(&terminal_trim_committed_text(&line))
    }

    pub fn error(&mut self, msg: &str) -> io::Result<()> {
        writeln!(self.stderr, "error: {msg}")?;
        self.stderr.flush()
    }

    pub fn notice(&mut self, msg: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.assistant_block_open = false;
        self.ensure_block_separator_if_needed()?;
        self.write_stdout_committed(&terminal_trim_committed_text(&format!("{msg}\n")))
    }

    pub fn turn_retry(
        &mut self,
        retry_count: u64,
        max_auto_retries: u64,
        reason: &str,
    ) -> io::Result<()> {
        self.notice(&format!(
            "[mu] retrying [{retry_count}/{max_auto_retries}] after {reason}"
        ))
    }

    /// Announce that the turn was interrupted before it finished. This is an
    /// append-only notice: any partial assistant text streamed above was not
    /// persisted to session history (only completed messages are). The session
    /// is now "unclean"; the next prompt continues on top of it, or `mu retry`
    /// resumes it.
    pub fn turn_interrupted(&mut self, reason: &str) -> io::Result<()> {
        self.notice(&format!("[mu] interrupted: {reason}"))?;
        self.notice(
            "[mu] partial output above is not saved to session history; \
             run `mu retry` to resume or just send another prompt",
        )
    }

    /// Ensure stdout ends on a fresh line so the next shell prompt does not
    /// glue onto the final line of assistant output.
    pub fn finish_turn(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.assistant_end()?;
        self.ensure_line_start()
    }

    pub fn turn_summary(
        &mut self,
        input_tokens: u64,
        output_tokens: u64,
        context_pct: Option<f64>,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if !self.stderr.is_terminal() {
            return Ok(());
        }
        write!(
            self.stderr,
            "{}\n\n",
            format_turn_summary(input_tokens, output_tokens, context_pct)
        )?;
        self.stderr.flush()
    }

    pub fn turn_done_bell(&mut self, elapsed: Duration) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        let Some(min_duration) = self.turn_done_bell_min_duration else {
            return Ok(());
        };
        if elapsed < min_duration || !self.styled || !self.stderr.is_terminal() {
            return Ok(());
        }
        self.stderr.write_all(b"\x07")?;
        self.stderr.flush()
    }

    fn ensure_line_start(&mut self) -> io::Result<()> {
        self.clear_live_line()?;
        if self.stdout_at_line_start {
            return Ok(());
        }
        self.write_stdout("\n")
    }

    fn ensure_blank_line_before_next_block(&mut self) -> io::Result<()> {
        self.clear_live_line()?;
        let needed = if self.stdout_at_line_start {
            2usize.saturating_sub(self.trailing_newlines)
        } else {
            2
        };
        if needed == 0 {
            return Ok(());
        }
        self.write_stdout(&"\n".repeat(needed))
    }

    fn ensure_block_separator_if_needed(&mut self) -> io::Result<()> {
        self.clear_live_line()?;
        if !self.has_committed_stdout {
            return Ok(());
        }
        self.ensure_blank_line_before_next_block()
    }

    fn render_live_line(&mut self) -> io::Result<()> {
        let Some(line) = self.format_live_line() else {
            return Ok(());
        };
        if !self.styled {
            return Ok(());
        }
        if self.live_line_rendered {
            self.stdout.write_all(b"\r\x1b[2K")?;
        } else if matches!(self.live_line, Some(LiveLine::Thinking)) {
            self.ensure_block_separator_if_needed()?;
        } else if !self.stdout_at_line_start {
            self.write_stdout("\n")?;
        }
        self.stdout.write_all(line.as_bytes())?;
        self.stdout_at_line_start = false;
        self.live_line_rendered = true;
        self.stdout.flush()
    }

    fn clear_live_line(&mut self) -> io::Result<()> {
        if self.live_line_rendered {
            self.stdout.write_all(b"\r\x1b[2K")?;
            self.stdout_at_line_start = true;
            self.stdout.flush()?;
            self.live_line_rendered = false;
        }
        Ok(())
    }

    fn format_live_line(&self) -> Option<String> {
        match self.live_line {
            Some(LiveLine::Thinking) => {
                let reasoning = self.reasoning.as_ref()?;
                Some(format_thinking_live(
                    reasoning.started.elapsed(),
                    approx_tokens_from_chars(reasoning.reasoning_chars),
                ))
            }
            Some(LiveLine::BashOmitted {
                omitted_lines,
                omitted_bytes,
            }) => Some(format_omitted_line(
                omitted_lines,
                omitted_bytes,
                self.styled,
            )),
            None => None,
        }
    }

    fn set_omitted_live_line(
        &mut self,
        omitted_lines: usize,
        omitted_bytes: usize,
    ) -> io::Result<()> {
        if omitted_lines == 0 && omitted_bytes == 0 {
            if matches!(self.live_line, Some(LiveLine::BashOmitted { .. })) {
                self.clear_live_line()?;
                self.live_line = None;
            }
            return Ok(());
        }
        self.live_line = Some(LiveLine::BashOmitted {
            omitted_lines,
            omitted_bytes,
        });
        self.render_live_line()
    }

    fn finalize_bash_preview(&mut self) -> io::Result<()> {
        let Some(preview) = self.bash_preview.take() else {
            return Ok(());
        };
        let snapshot = compute_bash_preview_snapshot(&preview.raw, true);
        self.clear_live_line()?;
        self.live_line = None;
        if snapshot.head_rendered.len() > preview.committed_head_len {
            let next = snapshot.head_rendered[preview.committed_head_len..].to_string();
            if self.styled {
                self.write_stdout_committed(&format!(
                    "{GRAY}{}{RESET}",
                    terminal_trim_committed_text(&next)
                ))?;
            } else {
                self.write_stdout_committed(&terminal_trim_committed_text(&next))?;
            }
        }
        if snapshot.omitted_lines > 0 || snapshot.omitted_bytes > 0 {
            self.write_stdout_committed(&terminal_trim_committed_text(&format!(
                "{}\n",
                format_omitted_line(snapshot.omitted_lines, snapshot.omitted_bytes, self.styled)
            )))?;
        }
        if !snapshot.tail_rendered.is_empty() {
            if self.styled {
                self.write_stdout_committed(&format!(
                    "{GRAY}{}{RESET}",
                    terminal_trim_committed_text(&snapshot.tail_rendered)
                ))?;
            } else {
                self.write_stdout_committed(&terminal_trim_committed_text(
                    &snapshot.tail_rendered,
                ))?;
            }
        }
        Ok(())
    }

    fn write_committed(&mut self, text: &str) -> io::Result<()> {
        if !self.styled {
            return self.write_stdout_committed(text);
        }
        self.clear_live_line()?;
        self.write_stdout_committed(&terminal_trim_committed_text(text))?;
        self.render_live_line()
    }

    fn write_stdout_committed(&mut self, text: &str) -> io::Result<()> {
        self.write_stdout(text)?;
        if !text.is_empty() {
            self.has_committed_stdout = true;
        }
        Ok(())
    }

    fn write_stdout(&mut self, text: &str) -> io::Result<()> {
        let previous_trailing_newlines = self.trailing_newlines;
        let visible = strip_ansi(text);
        let visible_char_count = visible.chars().count();
        let trailing_in_text = visible.chars().rev().take_while(|ch| *ch == '\n').count();
        self.stdout.write_all(text.as_bytes())?;
        self.stdout_at_line_start = visible.ends_with('\n');
        self.trailing_newlines = if text.is_empty() {
            previous_trailing_newlines
        } else if trailing_in_text == visible_char_count {
            previous_trailing_newlines + trailing_in_text
        } else {
            trailing_in_text
        };
        self.stdout.flush()
    }
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Default)]
struct MarkdownStream {
    pending: String,
}

#[derive(Clone, Copy)]
enum LiveLine {
    Thinking,
    BashOmitted {
        omitted_lines: usize,
        omitted_bytes: usize,
    },
}

struct ReasoningState {
    started: Instant,
    reasoning_chars: usize,
    committed: bool,
}

#[derive(Default)]
struct BashPreviewState {
    raw: String,
    committed_head_len: usize,
}

#[derive(Default, Debug, PartialEq, Eq)]
struct BashPreviewSnapshot {
    head_rendered: String,
    tail_rendered: String,
    omitted_lines: usize,
    omitted_bytes: usize,
}

impl MarkdownStream {
    fn push(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        let stable = stable_markdown_prefix(&self.pending);
        if stable == 0 {
            return Vec::new();
        }
        let rest = self.pending.split_off(stable);
        let block = std::mem::replace(&mut self.pending, rest);
        vec![block]
    }

    fn finish(&mut self) -> Option<String> {
        if self.pending.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.pending))
    }
}

fn stable_markdown_prefix(text: &str) -> usize {
    let mut stable = 0;
    let mut offset = 0;
    let mut fence: Option<(char, usize)> = None;

    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break;
        }
        offset += line.len();
        let trimmed = line.trim();
        let marker = fence_marker(trimmed);

        if let Some((kind, width)) = fence {
            if marker.is_some_and(|(next, count)| next == kind && count >= width) {
                fence = None;
                stable = offset;
            }
            continue;
        }

        if let Some(next) = marker {
            fence = Some(next);
            continue;
        }
        if trimmed.is_empty() || is_single_line_block(trimmed) {
            stable = offset;
        }
    }
    stable
}

fn fence_marker(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = line.chars().take_while(|ch| *ch == first).count();
    (count >= 3).then_some((first, count))
}

fn is_single_line_block(line: &str) -> bool {
    let heading = line
        .strip_prefix('#')
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('#') || rest.starts_with(' '));
    let rule = line.len() >= 3
        && ['-', '*', '_']
            .into_iter()
            .any(|mark| line.chars().all(|ch| ch == mark || ch.is_whitespace()));
    heading || rule
}

#[derive(Clone, Copy)]
enum MdStyle {
    Bold,
    Dim,
    Italic,
    Underline,
    Strike,
    Blue,
    Cyan,
    Green,
}

impl MdStyle {
    fn ansi(self) -> &'static str {
        match self {
            Self::Bold => BOLD,
            Self::Dim => DIM,
            Self::Italic => ITALIC,
            Self::Underline => UNDERLINE,
            Self::Strike => STRIKE,
            Self::Blue => BLUE,
            Self::Cyan => CYAN,
            Self::Green => GREEN,
        }
    }
}

#[derive(Clone, Copy)]
struct ListState {
    next: Option<u64>,
}

struct TableState {
    alignments: Vec<Alignment>,
    header: Vec<String>,
    rows: Vec<Vec<String>>,
    current_row: Vec<String>,
    current_cell: Option<String>,
}

impl TableState {
    fn new(alignments: Vec<Alignment>) -> Self {
        Self {
            alignments,
            header: Vec::new(),
            rows: Vec::new(),
            current_row: Vec::new(),
            current_cell: None,
        }
    }

    fn start_row(&mut self) {
        self.current_row.clear();
    }

    fn start_cell(&mut self) {
        self.current_cell = Some(String::new());
    }

    fn finish_cell(&mut self) {
        let cell = self.current_cell.take().unwrap_or_default();
        self.current_row.push(normalize_table_cell(&cell));
    }

    fn finish_header(&mut self) {
        self.header = std::mem::take(&mut self.current_row);
    }

    fn finish_row(&mut self) {
        self.rows.push(std::mem::take(&mut self.current_row));
    }
}

fn render_markdown(markdown: &str) -> String {
    let options =
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS;
    let parser = Parser::new_ext(markdown, options);
    let mut out = String::new();
    let mut styles: Vec<MdStyle> = Vec::new();
    let mut lists: Vec<ListState> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut in_item = 0usize;
    let mut table_state: Option<TableState> = None;

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    push_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        heading_styles(level),
                    );
                }
                Tag::BlockQuote(_) => {
                    current_render_target(&mut out, &mut table_state).push_str("│ ");
                    push_style(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        MdStyle::Dim,
                    );
                }
                Tag::CodeBlock(_) => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    push_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        code_block_styles(),
                    );
                }
                Tag::List(start) => lists.push(ListState { next: start }),
                Tag::Item => {
                    in_item += 1;
                    current_render_target(&mut out, &mut table_state)
                        .push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                    let marker = lists.last_mut().and_then(|list| {
                        let current = list.next?;
                        list.next = Some(current + 1);
                        Some(format!("{current}. "))
                    });
                    current_render_target(&mut out, &mut table_state)
                        .push_str(marker.as_deref().unwrap_or("• "));
                }
                Tag::Table(alignments) => {
                    table_state = Some(TableState::new(alignments));
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(table) = table_state.as_mut() {
                        table.start_row();
                    }
                }
                Tag::TableCell => {
                    if let Some(table) = table_state.as_mut() {
                        table.start_cell();
                    }
                }
                Tag::Emphasis => push_styles(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                    emphasis_styles(),
                ),
                Tag::Strong => push_styles(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                    strong_styles(),
                ),
                Tag::Strikethrough => push_style(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                    MdStyle::Strike,
                ),
                Tag::Link { dest_url, .. } => {
                    links.push(dest_url.to_string());
                    push_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        link_styles(),
                    );
                    current_render_target(&mut out, &mut table_state)
                        .push_str(&open_hyperlink(&dest_url));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if in_item == 0 {
                        current_render_target(&mut out, &mut table_state).push_str("\n\n");
                    }
                }
                TagEnd::Heading(level) => {
                    pop_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        heading_styles(level).len(),
                    );
                    current_render_target(&mut out, &mut table_state).push_str("\n\n");
                }
                TagEnd::BlockQuote(_) | TagEnd::CodeBlock => {
                    pop_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        code_block_styles().len(),
                    );
                    current_render_target(&mut out, &mut table_state).push_str("\n\n");
                }
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        current_render_target(&mut out, &mut table_state).push('\n');
                    }
                }
                TagEnd::Item => {
                    in_item = in_item.saturating_sub(1);
                    current_render_target(&mut out, &mut table_state).push('\n');
                }
                TagEnd::TableHead => {
                    if let Some(table) = table_state.as_mut() {
                        table.finish_header();
                    }
                }
                TagEnd::TableRow => {
                    if let Some(table) = table_state.as_mut() {
                        table.finish_row();
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = table_state.as_mut() {
                        table.finish_cell();
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = table_state.take() {
                        out.push_str(&render_table(&table));
                    }
                }
                TagEnd::Emphasis => pop_styles(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                    emphasis_styles().len(),
                ),
                TagEnd::Strong => pop_styles(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                    strong_styles().len(),
                ),
                TagEnd::Strikethrough => pop_style(
                    current_render_target(&mut out, &mut table_state),
                    &mut styles,
                ),
                TagEnd::Link => {
                    current_render_target(&mut out, &mut table_state).push_str(OSC8_CLOSE);
                    pop_styles(
                        current_render_target(&mut out, &mut table_state),
                        &mut styles,
                        link_styles().len(),
                    );
                    if let Some(url) = links.pop() {
                        let target = current_render_target(&mut out, &mut table_state);
                        target.push_str(DIM);
                        target.push_str(" (");
                        target.push_str(&hyperlink_text(&url, &url));
                        target.push(')');
                        target.push_str(RESET);
                        for style in &styles {
                            current_render_target(&mut out, &mut table_state)
                                .push_str(style.ansi());
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) => current_render_target(&mut out, &mut table_state).push_str(&text),
            Event::Code(code) => {
                let target = current_render_target(&mut out, &mut table_state);
                for style in inline_code_styles() {
                    target.push_str(style.ansi());
                }
                target.push_str(&code);
                target.push_str(RESET);
                for style in &styles {
                    current_render_target(&mut out, &mut table_state).push_str(style.ansi());
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                current_render_target(&mut out, &mut table_state).push('\n')
            }
            Event::Rule => current_render_target(&mut out, &mut table_state)
                .push_str("────────────────────────────────────────\n\n"),
            Event::TaskListMarker(done) => current_render_target(&mut out, &mut table_state)
                .push_str(if done { "[✓] " } else { "[ ] " }),
            Event::Html(html) | Event::InlineHtml(html) => {
                current_render_target(&mut out, &mut table_state).push_str(&html)
            }
            Event::FootnoteReference(name) => {
                let target = current_render_target(&mut out, &mut table_state);
                target.push('[');
                target.push_str(&name);
                target.push(']');
            }
            _ => {}
        }
    }

    if !styles.is_empty() || !out.ends_with(RESET) {
        out.push_str(RESET);
    }
    out
}

fn current_render_target<'a>(
    out: &'a mut String,
    table_state: &'a mut Option<TableState>,
) -> &'a mut String {
    match table_state {
        Some(table) => match table.current_cell.as_mut() {
            Some(cell) => cell,
            None => out,
        },
        None => out,
    }
}

fn push_style(out: &mut String, styles: &mut Vec<MdStyle>, style: MdStyle) {
    styles.push(style);
    out.push_str(style.ansi());
}

fn pop_style(out: &mut String, styles: &mut Vec<MdStyle>) {
    styles.pop();
    out.push_str(RESET);
    for style in styles.iter() {
        out.push_str(style.ansi());
    }
}

fn push_styles(out: &mut String, styles: &mut Vec<MdStyle>, applied: &[MdStyle]) {
    for style in applied {
        push_style(out, styles, *style);
    }
}

fn pop_styles(out: &mut String, styles: &mut Vec<MdStyle>, count: usize) {
    for _ in 0..count {
        pop_style(out, styles);
    }
}

fn normalize_table_cell(cell: &str) -> String {
    cell.lines()
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn render_table(table: &TableState) -> String {
    let cols = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0))
        .max(table.alignments.len());
    if cols == 0 {
        return String::new();
    }

    let mut widths = vec![3usize; cols];
    for row in std::iter::once(&table.header).chain(table.rows.iter()) {
        for (idx, cell) in row.iter().enumerate() {
            widths[idx] = widths[idx].max(visible_text_width(cell));
        }
    }

    let mut out = String::new();
    out.push_str(&render_table_row(&table.header, &widths, &table.alignments));
    out.push_str(&render_table_separator(&widths, &table.alignments));
    for row in &table.rows {
        out.push_str(&render_table_row(row, &widths, &table.alignments));
    }
    out.push('\n');
    out
}

fn render_table_row(row: &[String], widths: &[usize], alignments: &[Alignment]) -> String {
    let mut out = String::from("|");
    for (idx, width) in widths.iter().copied().enumerate() {
        let cell = row.get(idx).map(String::as_str).unwrap_or("");
        out.push(' ');
        out.push_str(&pad_table_cell(
            cell,
            width,
            alignments.get(idx).copied().unwrap_or(Alignment::None),
        ));
        out.push(' ');
        out.push('|');
    }
    out.push('\n');
    out
}

fn render_table_separator(widths: &[usize], alignments: &[Alignment]) -> String {
    let mut out = String::from("|");
    for (idx, width) in widths.iter().copied().enumerate() {
        let width = width.max(3);
        let segment = match alignments.get(idx).copied().unwrap_or(Alignment::None) {
            Alignment::Left => format!(":{:-<width$}", "", width = width.saturating_sub(1)),
            Alignment::Center => {
                if width <= 2 {
                    ":|".to_string()
                } else {
                    format!(":{:-<inner$}:", "", inner = width.saturating_sub(2))
                }
            }
            Alignment::Right => format!("{:-<width$}:", "", width = width.saturating_sub(1)),
            Alignment::None => format!("{:-<width$}", "", width = width),
        };
        out.push(' ');
        out.push_str(&segment);
        out.push(' ');
        out.push('|');
    }
    out.push('\n');
    out
}

fn pad_table_cell(cell: &str, width: usize, alignment: Alignment) -> String {
    let visible = visible_text_width(cell);
    let padding = width.saturating_sub(visible);
    let (left, right) = match alignment {
        Alignment::Right => (padding, 0),
        Alignment::Center => (padding / 2, padding - (padding / 2)),
        Alignment::Left | Alignment::None => (0, padding),
    };
    format!("{}{}{}", " ".repeat(left), cell, " ".repeat(right))
}

fn visible_text_width(text: &str) -> usize {
    UnicodeWidthStr::width(strip_ansi(text).as_str())
}

fn heading_styles(level: HeadingLevel) -> &'static [MdStyle] {
    match level {
        HeadingLevel::H1 => &[MdStyle::Bold, MdStyle::Underline, MdStyle::Cyan],
        HeadingLevel::H2 => &[MdStyle::Bold, MdStyle::Blue],
        HeadingLevel::H3 => &[MdStyle::Bold, MdStyle::Green],
        HeadingLevel::H4 => &[MdStyle::Underline, MdStyle::Green],
        HeadingLevel::H5 => &[MdStyle::Bold, MdStyle::Dim],
        HeadingLevel::H6 => &[MdStyle::Italic, MdStyle::Dim],
    }
}

fn emphasis_styles() -> &'static [MdStyle] {
    &[MdStyle::Italic, MdStyle::Dim]
}

fn strong_styles() -> &'static [MdStyle] {
    &[MdStyle::Bold]
}

fn link_styles() -> &'static [MdStyle] {
    &[MdStyle::Underline, MdStyle::Blue]
}

fn inline_code_styles() -> &'static [MdStyle] {
    &[MdStyle::Green]
}

fn code_block_styles() -> &'static [MdStyle] {
    inline_code_styles()
}

fn open_hyperlink(url: &str) -> String {
    format!("{OSC8_OPEN}{url}{OSC8_ST}")
}

fn hyperlink_text(url: &str, text: &str) -> String {
    format!("{}{text}{OSC8_CLOSE}", open_hyperlink(url))
}

fn format_tool(display: &ToolDisplay, elapsed: Duration, styled: bool) -> String {
    let elapsed = format_duration(elapsed);
    match display {
        ToolDisplay::None => String::new(),
        ToolDisplay::Bash { exit_code } => {
            if styled {
                let (color, icon) = if *exit_code == 0 {
                    (GREEN, "✓")
                } else {
                    (RED, "✗")
                };
                format!("{color}{icon} exit {exit_code}{RESET}{DIM} · {elapsed}{RESET}\n")
            } else {
                let icon = if *exit_code == 0 { "✓" } else { "✗" };
                format!("{icon} exit {exit_code} · {elapsed}\n")
            }
        }
    }
}

#[cfg(test)]
fn format_bash_header(title: &str, script: &str, risk: Option<&str>, styled: bool) -> String {
    let command = preview_first_line(script, BASH_COMMAND_PREVIEW_BYTES);
    if !styled {
        let mut out = String::new();
        if !title.is_empty() {
            out.push_str("# ");
            out.push_str(title);
            out.push('\n');
        }
        out.push('$');
        out.push(' ');
        if let Some(risk) = risk {
            out.push_str(&format_risk_label(risk, false));
            out.push(' ');
        }
        out.push_str(&command);
        out.push('\n');
        return out;
    }

    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(GRAY);
        out.push_str("# ");
        out.push_str(RESET);
        out.push_str(BOLD);
        out.push_str(title);
        out.push_str(RESET);
        out.push('\n');
    }
    out.push_str(DIM);
    out.push('$');
    out.push_str(RESET);
    out.push(' ');
    out.push_str(bash_risk_color(risk));
    out.push_str(BOLD);
    out.push_str(&command);
    out.push_str(RESET);
    out.push('\n');
    out
}

fn format_risk_label(risk: &str, styled: bool) -> String {
    if !styled {
        return format!("[{risk}]");
    }
    match risk {
        "readonly" => format!("{CYAN}[{risk}]{RESET}"),
        "reversible" => format!("{YELLOW}[{risk}]{RESET}"),
        "destructive" => format!("{RED}[{risk}]{RESET}"),
        _ => format!("{DIM}[{risk}]{RESET}"),
    }
}

fn bash_risk_color(risk: Option<&str>) -> &'static str {
    match risk {
        Some("readonly") => CYAN,
        Some("reversible") => YELLOW,
        Some("destructive") => RED,
        _ => DIM,
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        return format!("{}ms", duration.as_millis());
    }
    format!("{:.1}s", duration.as_secs_f64())
}

fn format_thinking_live(elapsed: Duration, output_tokens: u64) -> String {
    format!(
        "{GRAY}[thought {}, ~{} tokens]{RESET}",
        format_duration(elapsed),
        output_tokens
    )
}

fn format_thought_line(
    elapsed: Duration,
    reasoning_chars: usize,
    usage: Option<(u64, u64)>,
    styled: bool,
) -> String {
    let elapsed = format_duration(elapsed);
    let tokens = usage
        .map(|(_, completion_tokens)| completion_tokens.to_string())
        .unwrap_or_else(|| format!("~{}", approx_tokens_from_chars(reasoning_chars)));
    if styled {
        format!("{GRAY}[thought {elapsed}, {tokens} tokens]{RESET}\n")
    } else {
        format!("[thought {elapsed}, {tokens} tokens]\n")
    }
}

fn format_omitted_line(omitted_lines: usize, omitted_bytes: usize, styled: bool) -> String {
    if styled {
        format!("{GRAY}[… omitted {omitted_lines} lines, {omitted_bytes} bytes]{RESET}")
    } else {
        format!("[… omitted {omitted_lines} lines, {omitted_bytes} bytes]")
    }
}

fn approx_tokens_from_chars(chars: usize) -> u64 {
    (chars as u64).div_ceil(4)
}

fn terminal_trim_committed_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut pending = String::new();
    for ch in text.chars() {
        if ch == '\n' {
            while pending.ends_with(' ') || pending.ends_with('\t') {
                pending.pop();
            }
            out.push_str(&pending);
            out.push('\n');
            pending.clear();
        } else {
            pending.push(ch);
        }
    }
    out.push_str(&pending);
    out
}

fn preview_first_line(text: &str, max_bytes: usize) -> String {
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let mut preview = truncate_prefix(first, max_bytes, false).rendered;
    if preview.is_empty() {
        return preview;
    }
    if lines.next().is_some() || first.len() > preview.len() {
        append_ellipsis_in_place(&mut preview, max_bytes);
    }
    preview
}

fn compute_bash_preview_snapshot(text: &str, finalizing: bool) -> BashPreviewSnapshot {
    let mut complete_lines = Vec::new();
    let mut trailing = String::new();
    for part in text.split_inclusive('\n') {
        if part.ends_with('\n') {
            complete_lines.push(trim_complete_terminal_line(part));
        } else {
            trailing.push_str(part);
        }
    }

    while complete_lines
        .first()
        .is_some_and(|line| is_blank_output_line(line))
    {
        complete_lines.remove(0);
    }

    let trimmed_trailing = normalize_trailing_fragment(&trailing, finalizing);
    if trimmed_trailing.is_empty() {
        while complete_lines
            .last()
            .is_some_and(|line| is_blank_output_line(line))
        {
            complete_lines.pop();
        }
    }

    let mut head_rendered = String::new();
    let mut head_count = 0usize;
    let mut head_bytes = 0usize;
    let mut hidden_head_bytes = 0usize;
    for line in &complete_lines {
        if head_count >= BASH_HEAD_LINE_BUDGET {
            break;
        }
        let preview = truncate_prefix(line, BASH_HEAD_LINE_CAP_BYTES, true);
        let next_bytes = head_bytes + preview.rendered.len();
        if next_bytes > BASH_HEAD_BYTE_BUDGET {
            break;
        }
        head_bytes = next_bytes;
        hidden_head_bytes += line.len().saturating_sub(preview.raw_kept_bytes);
        head_rendered.push_str(&preview.rendered);
        head_count += 1;
    }

    let tail_start = if finalizing {
        complete_lines
            .len()
            .saturating_sub(BASH_TAIL_LINE_RESERVE)
            .max(head_count)
    } else {
        complete_lines.len()
    };
    let omitted_complete = &complete_lines[head_count..tail_start];
    let omitted_lines = omitted_complete.len();
    let omitted_line_bytes = omitted_complete
        .iter()
        .map(|line| line.len())
        .sum::<usize>();
    let reserved_complete = &complete_lines[tail_start..];

    let mut head_fragment_kept = 0usize;
    if finalizing && head_count == 0 && !trimmed_trailing.is_empty() {
        let preview = truncate_prefix(&trimmed_trailing, BASH_HEAD_LINE_CAP_BYTES, false);
        head_fragment_kept = preview.raw_kept_bytes;
        head_rendered.push_str(&preview.rendered);
    }
    let fallback_reserved =
        if !trimmed_trailing.is_empty() && head_fragment_kept < trimmed_trailing.len() {
            trim_to_last_bytes(&trimmed_trailing, BASH_TAIL_FALLBACK_BYTES)
        } else {
            String::new()
        };

    if !finalizing {
        return BashPreviewSnapshot {
            head_rendered,
            tail_rendered: String::new(),
            omitted_lines,
            omitted_bytes: omitted_line_bytes + hidden_head_bytes + trimmed_trailing.len(),
        };
    }

    let mut tail_segments = Vec::new();
    let mut hidden_tail_bytes = 0usize;
    for line in reserved_complete {
        let (rendered, kept) = cap_tail_line(line, true);
        hidden_tail_bytes += line.len().saturating_sub(kept);
        tail_segments.push(rendered);
    }
    if !fallback_reserved.is_empty() {
        let (rendered, kept) = cap_tail_line(&fallback_reserved, false);
        let overlap = head_fragment_kept
            .saturating_add(kept)
            .saturating_sub(trimmed_trailing.len());
        hidden_tail_bytes += trimmed_trailing
            .len()
            .saturating_sub(head_fragment_kept)
            .saturating_sub(kept.saturating_sub(overlap));
        tail_segments.push(rendered);
    }
    BashPreviewSnapshot {
        head_rendered,
        tail_rendered: tail_segments.concat(),
        omitted_lines,
        omitted_bytes: omitted_line_bytes + hidden_head_bytes + hidden_tail_bytes,
    }
}

fn trim_complete_terminal_line(line: &str) -> String {
    let line = line.strip_suffix('\n').unwrap_or(line);
    let trimmed = line.trim_end_matches([' ', '\t']);
    format!("{trimmed}\n")
}

fn normalize_trailing_fragment(fragment: &str, finalizing: bool) -> String {
    if fragment.trim().is_empty() {
        return String::new();
    }
    if finalizing {
        return trim_final_tail_fragment(fragment);
    }
    fragment.to_string()
}

fn trim_final_tail_fragment(fragment: &str) -> String {
    fragment.trim_end().to_string()
}

fn is_blank_output_line(line: &str) -> bool {
    line.trim().is_empty()
}

fn trim_to_last_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let start = text
        .char_indices()
        .find(|(idx, _)| text.len() - idx <= max_bytes)
        .map(|(idx, _)| idx)
        .unwrap_or(text.len());
    text[start..].to_string()
}

fn cap_tail_line(line: &str, complete_line: bool) -> (String, usize) {
    let newline = complete_line && line.ends_with('\n');
    let body = if newline {
        line.strip_suffix('\n').unwrap_or(line)
    } else {
        line
    };
    let body_bytes = body.len();
    if body_bytes <= BASH_TAIL_LINE_CAP_BYTES {
        let mut out = body.to_string();
        if newline {
            out.push('\n');
        }
        return (out, body_bytes + usize::from(newline));
    }
    let kept = BASH_TAIL_LINE_CAP_BYTES
        .saturating_sub(ELLIPSIS.len())
        .saturating_sub(usize::from(newline));
    let suffix = trim_to_last_bytes(body, kept);
    let mut out = String::from(ELLIPSIS);
    out.push_str(&suffix);
    if newline {
        out.push('\n');
    }
    (out, suffix.len() + usize::from(newline))
}

fn truncate_prefix(text: &str, max_bytes: usize, preserve_newline: bool) -> LinePreview {
    let newline = preserve_newline && text.ends_with('\n');
    let body = if newline {
        text.strip_suffix('\n').unwrap_or(text)
    } else {
        text
    };
    if body.len() <= max_bytes {
        let mut rendered = body.to_string();
        if newline {
            rendered.push('\n');
        }
        return LinePreview {
            rendered,
            raw_kept_bytes: body.len() + usize::from(newline),
        };
    }

    let kept = max_bytes.saturating_sub(ELLIPSIS.len());
    let prefix = trim_to_first_bytes(body, kept);
    let mut rendered = prefix.clone();
    rendered.push_str(ELLIPSIS);
    if newline {
        rendered.push('\n');
    }
    LinePreview {
        rendered,
        raw_kept_bytes: prefix.len() + usize::from(newline),
    }
}

fn trim_to_first_bytes(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .take_while(|(idx, ch)| idx + ch.len_utf8() <= max_bytes)
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())
        .unwrap_or(0);
    text[..end].to_string()
}

fn append_ellipsis_in_place(text: &mut String, max_bytes: usize) {
    if text.is_empty() {
        return;
    }
    if text.ends_with(ELLIPSIS) {
        return;
    }
    let budget = max_bytes.saturating_sub(ELLIPSIS.len());
    if text.len() > budget {
        *text = trim_to_first_bytes(text, budget);
    }
    text.push_str(ELLIPSIS);
}

fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\x1b' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                let mut escape = false;
                for next in chars.by_ref() {
                    if next == '\x07' || (escape && next == '\\') {
                        break;
                    }
                    escape = next == '\x1b';
                }
            }
            Some(_) | None => {}
        }
    }
    out
}

fn format_turn_summary(input_tokens: u64, output_tokens: u64, context_pct: Option<f64>) -> String {
    let ctx = context_pct
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "?".into());
    format!("[mu] tokens: {input_tokens} in / {output_tokens} out  context: {ctx}")
}

struct LinePreview {
    rendered: String,
    raw_kept_bytes: usize,
}

#[cfg(test)]
mod tests {
    use std::os::fd::RawFd;
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    use serde_json::json;

    use super::*;

    #[test]
    fn markdown_renderer_handles_lists_and_tables() {
        let rendered =
            render_markdown("- one\n  - two\n\n| Name | Value |\n| --- | --- |\n| a | b |\n");
        let plain = strip_ansi(&rendered);
        assert!(rendered.contains("• one"));
        assert!(rendered.contains("• two"));
        assert!(plain.contains("| Name | Value |"));
        assert_table_grid_aligned(&plain);
    }

    #[test]
    fn markdown_renderer_aligns_wide_table_cells() {
        let rendered =
            render_markdown("| Name | Value |\n| --- | ---: |\n| 字 | 10 |\n| ascii | 2 |\n");
        assert_table_grid_aligned(&strip_ansi(&rendered));
    }

    fn assert_table_grid_aligned(rendered: &str) {
        let table_lines = rendered
            .lines()
            .filter(|line| line.starts_with('|') && line.ends_with('|'))
            .collect::<Vec<_>>();
        assert!(table_lines.len() >= 3, "{rendered:?}");

        let first_bar_columns = bar_columns(table_lines[0]);
        let first_width = UnicodeWidthStr::width(table_lines[0]);
        for line in table_lines {
            assert_eq!(bar_columns(line), first_bar_columns, "{rendered:?}");
            assert_eq!(UnicodeWidthStr::width(line), first_width, "{rendered:?}");
        }
    }

    fn bar_columns(line: &str) -> Vec<usize> {
        let mut columns = Vec::new();
        let mut width = 0;
        for ch in line.chars() {
            if ch == '|' {
                columns.push(width);
            }
            width += UnicodeWidthChar::width(ch).unwrap_or(0);
        }
        columns
    }

    #[test]
    fn bash_header_renders_title_and_risk_colored_script_without_risk_label() {
        let header = format_bash_header("List files", "printf 'a'\npwd", Some("readonly"), true);
        assert!(header.starts_with(&format!("{GRAY}# {RESET}{BOLD}List files{RESET}\n")));
        assert!(header.contains(&format!("{DIM}${RESET} {CYAN}{BOLD}printf 'a'…{RESET}\n")));
        assert!(!header.contains("  pwd\n"));
        assert!(!header.contains("[readonly]"));
    }

    #[test]
    fn bash_start_can_reuse_streamed_header_without_duplication() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let args = json!({
            "title": "List files",
            "risk": "readonly",
            "script": "printf 'a'\npwd",
        });

        assert!(renderer.bash_header_full(Some("call_1"), &args).unwrap());
        renderer
            .tool_start(Some("call_1"), "bash", &args, true)
            .unwrap();
        renderer.bash_output(Some("call_1"), "bash", "a\n").unwrap();
        renderer
            .tool_finished(
                Some("call_1"),
                "bash",
                &ToolDisplay::Bash { exit_code: 0 },
                Duration::from_millis(1),
            )
            .unwrap();
    }

    #[test]
    fn terminal_trimming_only_removes_committed_line_suffixes() {
        assert_eq!(
            terminal_trim_committed_text("a  \n b\t\t\nc  "),
            "a\n b\nc  "
        );
        assert_eq!(trim_final_tail_fragment("tail \t\n\n"), "tail");
    }

    #[test]
    fn bash_preview_does_not_duplicate_short_unterminated_output_on_finish() {
        let snapshot = compute_bash_preview_snapshot("first", true);

        assert_eq!(snapshot.head_rendered, "first");
        assert_eq!(snapshot.tail_rendered, "");
        assert_eq!(snapshot.omitted_lines, 0);
        assert_eq!(snapshot.omitted_bytes, 0);
    }

    #[test]
    fn plain_reasoning_commits_summary_after_reasoning_finishes() {
        let raw = capture_plain_reasoning_transcript();
        let normalized = strip_ansi(&raw.replace('\r', ""));

        assert!(normalized.starts_with("[thought "));
        assert!(normalized.contains(", ~2 tokens]\n"));
    }

    #[test]
    fn terminal_summary_leaves_a_blank_line_before_the_next_prompt() {
        let raw = capture_renderer_pty_transcript(Duration::from_secs(12), Some("mu> "));
        let normalized = strip_ansi(&raw.replace('\r', ""));

        assert!(normalized.contains(
            "$ printf 'line01\\nline02\\nline03\\n'\n[guardrail: allow] risk=low auth=explicit"
        ));
        assert!(normalized.contains("reason is acceptable\nline01\n"));
        assert!(!normalized.contains("reason is acceptable\n\nline01\n"));
        assert!(normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%\n\nmu> "));
        assert!(!normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%\n\n\nmu> "));
    }

    fn capture_renderer_pty_transcript(
        turn_elapsed: Duration,
        trailing_prompt: Option<&str>,
    ) -> String {
        let _guard = pty_test_lock().lock().unwrap();
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                0
            );
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::close(master);
                assert_eq!(libc::dup2(slave, libc::STDOUT_FILENO), libc::STDOUT_FILENO);
                assert_eq!(libc::dup2(slave, libc::STDERR_FILENO), libc::STDERR_FILENO);
                if slave > libc::STDERR_FILENO {
                    libc::close(slave);
                }

                let mut renderer = Renderer::with_format(OutputFormat::Terminal);
                renderer.reasoning_start().unwrap();
                renderer.reasoning_delta("plan").unwrap();
                std::thread::sleep(Duration::from_millis(40));
                renderer.reasoning_end(Some((12, 5))).unwrap();
                renderer
                    .tool_start(
                        None,
                        "bash",
                        &json!({
                            "title": "Stream demo",
                            "risk": "readonly",
                            "script": "printf 'line01\\nline02\\nline03\\n'",
                        }),
                        false,
                    )
                    .unwrap();
                renderer
                    .guardrail_verdict(
                        true,
                        "low",
                        "explicit",
                        "reason is acceptable",
                        "printf 'line01\\nline02\\nline03\\n'",
                    )
                    .unwrap();
                renderer
                    .bash_output(None, "bash", "line01\nline02\nline03\n")
                    .unwrap();
                renderer
                    .tool_finished(
                        None,
                        "bash",
                        &ToolDisplay::Bash { exit_code: 0 },
                        Duration::from_millis(250),
                    )
                    .unwrap();
                renderer.finish_turn().unwrap();
                renderer.turn_summary(12, 5, Some(25.0)).unwrap();
                renderer.turn_done_bell(turn_elapsed).unwrap();
                if let Some(prompt) = trailing_prompt {
                    let bytes = prompt.as_bytes();
                    assert_eq!(
                        libc::write(libc::STDOUT_FILENO, bytes.as_ptr().cast(), bytes.len()),
                        bytes.len() as isize
                    );
                }
                libc::_exit(0);
            }

            libc::close(slave);
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            libc::close(master);
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            String::from_utf8_lossy(&out).into_owned()
        }
    }

    fn capture_plain_reasoning_transcript() -> String {
        let _guard = pty_test_lock().lock().unwrap();
        let mut master: RawFd = -1;
        let mut slave: RawFd = -1;
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                0
            );
            let pid = libc::fork();
            assert!(pid >= 0);
            if pid == 0 {
                libc::close(master);
                assert_eq!(libc::dup2(slave, libc::STDOUT_FILENO), libc::STDOUT_FILENO);
                if slave > libc::STDERR_FILENO {
                    libc::close(slave);
                }

                let mut renderer = Renderer::with_format(OutputFormat::Plain);
                renderer.reasoning_start().unwrap();
                renderer.reasoning_delta("reason").unwrap();
                renderer.reasoning_end(None).unwrap();
                libc::_exit(0);
            }

            libc::close(slave);
            let mut out = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = libc::read(master, buf.as_mut_ptr().cast(), buf.len());
                if n <= 0 {
                    break;
                }
                out.extend_from_slice(&buf[..n as usize]);
            }
            libc::close(master);
            let mut status = 0;
            assert_eq!(libc::waitpid(pid, &mut status, 0), pid);
            assert!(libc::WIFEXITED(status));
            assert_eq!(libc::WEXITSTATUS(status), 0);
            String::from_utf8_lossy(&out).into_owned()
        }
    }

    fn pty_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }
}
