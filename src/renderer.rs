use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::cli::OutputFormat;
use crate::provider::ReasoningVisibility;
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
const CODE: &str = "\x1b[38;5;215m";
pub(crate) const BASH_COMMAND_PREVIEW_BYTES: usize = 160;
pub(crate) const BASH_TITLE_PREVIEW_BYTES: usize = 120;
const GUARDRAIL_REASON_PREVIEW_BYTES: usize = 180;
const REASONING_TITLE_MAX_WIDTH: usize = 80;
const MAX_TABLE_COLUMN_WIDTH: usize = 80;
const BASH_HEAD_LINE_BUDGET: usize = 3;
const BASH_HEAD_BYTE_BUDGET: usize = 1024;
const BASH_HEAD_LINE_CAP_BYTES: usize = 120;
const BASH_TAIL_LINE_RESERVE: usize = 2;
const BASH_TAIL_FALLBACK_BYTES: usize = 512;
const BASH_TAIL_LINE_CAP_BYTES: usize = 120;
pub(crate) const ELLIPSIS: &str = "…";

pub struct Renderer {
    stdout: Box<dyn Write + Send>,
    stderr: Box<dyn Write + Send>,
    stderr_is_terminal: bool,
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

#[cfg(test)]
#[derive(Clone, Default)]
struct SharedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl SharedOutput {
    fn write_raw(&self, text: &str) {
        self.0.lock().unwrap().extend_from_slice(text.as_bytes());
    }

    fn transcript(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[cfg(test)]
impl Write for SharedOutput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
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
        let styled = format == OutputFormat::Terminal && stdout.is_terminal();
        let stderr = io::stderr();
        let stderr_is_terminal = stderr.is_terminal();
        Self {
            stdout: Box::new(stdout),
            stderr: Box::new(stderr),
            stderr_is_terminal,
            stdout_at_line_start: true,
            trailing_newlines: 0,
            has_committed_stdout: false,
            styled,
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

    #[cfg(test)]
    fn with_test_output(
        format: OutputFormat,
        stdout_is_terminal: bool,
        stderr_is_terminal: bool,
        turn_done_bell_min_duration: Option<Duration>,
    ) -> (Self, SharedOutput, SharedOutput) {
        let stdout = SharedOutput::default();
        let stderr = SharedOutput::default();
        (
            Self {
                stdout: Box::new(stdout.clone()),
                stderr: Box::new(stderr.clone()),
                stderr_is_terminal,
                stdout_at_line_start: true,
                trailing_newlines: 0,
                has_committed_stdout: false,
                styled: format == OutputFormat::Terminal && stdout_is_terminal,
                markdown: MarkdownStream::default(),
                assistant_block_open: false,
                live_line: None,
                live_line_rendered: false,
                reasoning: None,
                bash_preview: None,
                turn_done_bell_min_duration,
                final_only: format == OutputFormat::Final,
            },
            stdout,
            stderr,
        )
    }

    #[cfg(test)]
    fn with_test_shared_output(
        format: OutputFormat,
        output_is_terminal: bool,
        turn_done_bell_min_duration: Option<Duration>,
    ) -> (Self, SharedOutput) {
        let output = SharedOutput::default();
        (
            Self {
                stdout: Box::new(output.clone()),
                stderr: Box::new(output.clone()),
                stderr_is_terminal: output_is_terminal,
                stdout_at_line_start: true,
                trailing_newlines: 0,
                has_committed_stdout: false,
                styled: format == OutputFormat::Terminal && output_is_terminal,
                markdown: MarkdownStream::default(),
                assistant_block_open: false,
                live_line: None,
                live_line_rendered: false,
                reasoning: None,
                bash_preview: None,
                turn_done_bell_min_duration,
                final_only: format == OutputFormat::Final,
            },
            output,
        )
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
        let table_live = self.markdown.table_live();
        self.sync_table_live_line(table_live)?;
        if blocks.is_empty() {
            return self.render_live_line();
        }

        for block in blocks {
            if !self.assistant_block_open && rendered_block_is_blank(&block) {
                continue;
            }
            if !self.assistant_block_open {
                self.ensure_block_separator_if_needed()?;
                self.assistant_block_open = true;
            }
            self.write_committed(&block)?;
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
        let blocks = self.markdown.finish();
        let table_live = self.markdown.table_live();
        self.sync_table_live_line(table_live)?;
        if blocks.is_empty() {
            self.assistant_block_open = false;
            return Ok(());
        }
        for rendered in blocks {
            if !self.assistant_block_open && rendered_block_is_blank(&rendered) {
                continue;
            }
            if !self.assistant_block_open {
                self.ensure_block_separator_if_needed()?;
                self.assistant_block_open = true;
            }
            self.write_committed(&rendered)?;
        }
        self.assistant_block_open = false;
        self.render_live_line()
    }

    pub fn reasoning_start(&mut self, visibility: ReasoningVisibility) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.assistant_block_open = false;
        self.reasoning = Some(ReasoningState {
            started: Instant::now(),
            visibility,
            reasoning_chars: 0,
            summary: String::new(),
            title: None,
            committed: false,
        });
        if self.styled {
            self.live_line = Some(LiveLine::Thinking);
            self.render_live_line()?;
        }
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

    pub fn reasoning_summary_delta(&mut self, text: &str) -> io::Result<()> {
        if self.final_only || text.is_empty() {
            return Ok(());
        }
        let Some(reasoning) = self.reasoning.as_mut() else {
            return Ok(());
        };
        if reasoning.committed || reasoning.visibility != ReasoningVisibility::Opaque {
            return Ok(());
        }
        reasoning.summary.push_str(text);
        if reasoning.title.is_none() {
            reasoning.title = extract_reasoning_summary_title(&reasoning.summary, false);
        }
        if self.styled {
            self.live_line = Some(LiveLine::Thinking);
            self.render_live_line()?;
        }
        Ok(())
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
        if reasoning.committed {
            return Ok(());
        }
        if reasoning.title.is_none() {
            reasoning.title = extract_reasoning_summary_title(&reasoning.summary, true);
        }
        let tokens = match reasoning.visibility {
            ReasoningVisibility::StreamedTrace => Some(
                usage
                    .map(|(_, completion_tokens)| completion_tokens.to_string())
                    .unwrap_or_else(|| {
                        format!("~{}", approx_tokens_from_chars(reasoning.reasoning_chars))
                    }),
            ),
            ReasoningVisibility::Opaque => None,
        };
        let line = format_thought_line(
            reasoning.started.elapsed(),
            tokens,
            reasoning.title.as_deref(),
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
        if self.styled {
            self.live_line = Some(LiveLine::ToolComposition);
            self.render_live_line()?;
        }
        Ok(true)
    }

    pub fn bash_header_title_start(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        self.clear_live_line()?;
        self.live_line = None;
        self.ensure_block_separator_if_needed()?;
        if self.styled {
            self.write_committed(&format!("{BOLD}# "))
        } else {
            self.write_committed("# ")
        }
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

    pub fn bash_header_command_start(&mut self, risk: Option<&str>) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled {
            self.write_committed(&format!("{}{BOLD}$ ", bash_risk_color(risk)))
        } else {
            let mut out = String::from("$ ");
            if let Some(risk) = risk {
                out.push_str(&format_risk_label(risk, false));
                out.push(' ');
            }
            self.write_committed(&out)
        }
    }

    pub fn bash_header_command_delta(&mut self, text: &str) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if text.is_empty() {
            return Ok(());
        }
        self.write_committed(text)
    }

    pub fn bash_header_command_end(&mut self) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled {
            self.write_committed(&format!("{RESET}\n"))
        } else {
            self.write_committed("\n")
        }
    }

    pub fn bash_header_stdin_summary(&mut self, bytes: usize, complete: bool) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if self.styled && !complete {
            self.live_line = Some(LiveLine::BashStdin { bytes });
            return self.render_live_line();
        }
        if !complete {
            return Ok(());
        }
        if self.styled {
            self.clear_live_line()?;
            self.live_line = None;
        }
        self.write_stdout_committed(&format_stdin_summary_line(bytes, self.styled))
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
        let command = args
            .get("command")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let risk = args.get("risk").and_then(|value| value.as_str());
        self.bash_header_start(tool_call_id)?;
        self.bash_header_title_start()?;
        self.bash_header_title_delta(&preview_first_line(title, BASH_TITLE_PREVIEW_BYTES))?;
        self.bash_header_title_end()?;
        self.bash_header_command_start(risk)?;
        self.bash_header_command_delta(&preview_first_line(command, BASH_COMMAND_PREVIEW_BYTES))?;
        self.bash_header_command_end()?;
        if let Some(cwd) = args.get("cwd").and_then(|value| value.as_str()) {
            self.bash_header_cwd_line(cwd)?;
        }
        if let Some(stdin) = args.get("stdin").and_then(|value| value.as_str()) {
            self.bash_header_stdin_summary(stdin.len(), true)?;
        }
        Ok(true)
    }

    pub fn bash_header_cwd_line(&mut self, raw_cwd: &str) -> io::Result<()> {
        if self.final_only || !should_render_bash_cwd(raw_cwd) {
            return Ok(());
        }
        self.write_stdout_committed(&format_cwd_line(raw_cwd, self.styled))
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
        _command: &str,
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
        cache_read_input_tokens: u64,
        cache_write_input_tokens: Option<u64>,
        output_tokens: u64,
        context_pct: Option<f64>,
        elapsed: Duration,
    ) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        if !self.stderr_is_terminal {
            return Ok(());
        }
        if self.has_committed_stdout {
            self.stderr.write_all(b"\n")?;
        }
        let summary = format_turn_summary(
            input_tokens,
            cache_read_input_tokens,
            cache_write_input_tokens,
            output_tokens,
            context_pct,
            elapsed,
        );
        if self.styled {
            write!(self.stderr, "{GRAY}{summary}{RESET}\n\n")?;
        } else {
            write!(self.stderr, "{summary}\n\n")?;
        }
        self.stderr.flush()
    }

    pub fn turn_done_bell(&mut self, elapsed: Duration) -> io::Result<()> {
        if self.final_only {
            return Ok(());
        }
        let Some(min_duration) = self.turn_done_bell_min_duration else {
            return Ok(());
        };
        if elapsed < min_duration || !self.styled || !self.stderr_is_terminal {
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
        } else if matches!(
            self.live_line,
            Some(LiveLine::Thinking | LiveLine::ToolComposition)
        ) {
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
                    (reasoning.visibility == ReasoningVisibility::StreamedTrace)
                        .then(|| approx_tokens_from_chars(reasoning.reasoning_chars)),
                    reasoning.title.as_deref(),
                ))
            }
            Some(LiveLine::ToolComposition) => Some(format!("{GRAY}[preparing toolcall]{RESET}")),
            Some(LiveLine::TableBuffering { chars }) => Some(format_table_live(
                approx_tokens_from_chars(chars),
                self.styled,
            )),
            Some(LiveLine::BashOmitted {
                omitted_lines,
                omitted_bytes,
            }) => Some(format_omitted_line(
                omitted_lines,
                omitted_bytes,
                self.styled,
            )),
            Some(LiveLine::BashStdin { bytes }) => Some(format_stdin_summary(bytes, self.styled)),
            None => None,
        }
    }

    fn sync_table_live_line(&mut self, live: Option<TableBufferLive>) -> io::Result<()> {
        match live {
            Some(live) => {
                self.live_line = Some(LiveLine::TableBuffering { chars: live.chars });
                Ok(())
            }
            None => {
                if matches!(self.live_line, Some(LiveLine::TableBuffering { .. })) {
                    self.clear_live_line()?;
                    self.live_line = None;
                }
                Ok(())
            }
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
    pending_line: String,
    pending_block_separator: bool,
    line_stream: Option<LineStreamState>,
    line_prefix: Option<LineStreamPrefix>,
    inline_stream: InlineStream,
    code_fence: Option<FenceState>,
    table_candidate: Option<String>,
    table_buffer: Option<String>,
}

#[derive(Clone, Copy)]
struct FenceState {
    kind: char,
    width: usize,
    has_content: bool,
}

#[derive(Clone, Copy)]
enum LineStreamState {
    Prose,
    Heading { level: HeadingLevel },
    List,
    Quote,
}

#[derive(Default)]
struct LineStreamPrefix {
    raw: String,
    rendered: String,
    emitted: bool,
}

#[derive(Default)]
struct InlineStream {
    pending: String,
    base_styles: Vec<MdStyle>,
}

#[derive(Clone, Copy)]
struct TableBufferLive {
    chars: usize,
}

#[derive(Clone, Copy)]
enum LiveLine {
    Thinking,
    ToolComposition,
    TableBuffering {
        chars: usize,
    },
    BashOmitted {
        omitted_lines: usize,
        omitted_bytes: usize,
    },
    BashStdin {
        bytes: usize,
    },
}

struct ReasoningState {
    started: Instant,
    visibility: ReasoningVisibility,
    reasoning_chars: usize,
    summary: String,
    title: Option<String>,
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
        let mut out = Vec::new();
        self.pending_line.push_str(text);
        while let Some(newline) = self.pending_line.find('\n') {
            let line = self.pending_line[..=newline].to_string();
            self.pending_line.replace_range(..=newline, "");
            self.push_complete_line(&line, &mut out);
        }
        self.push_partial_line(&mut out);
        out
    }

    fn finish(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        if self.line_stream.is_some() {
            if !self.pending_line.is_empty() {
                let text = std::mem::take(&mut self.pending_line);
                self.inline_stream.push(&text, &mut out);
            }
            self.finish_line_stream(&mut out);
        } else if !self.pending_line.is_empty() {
            let line = std::mem::take(&mut self.pending_line);
            self.push_line(&line, false, &mut out);
        }
        self.flush_table_candidate(&mut out);
        self.flush_table_buffer(&mut out);
        self.flush_pending_block_separator(&mut out);
        if self.code_fence.take().is_some() {
            out.push(RESET.to_string());
        }
        out
    }

    fn table_live(&self) -> Option<TableBufferLive> {
        self.table_buffer.as_ref().map(|table| TableBufferLive {
            chars: table.chars().count(),
        })
    }

    fn push_complete_line(&mut self, line: &str, out: &mut Vec<String>) {
        if self.line_stream.is_some() {
            let (body, _) = split_line_ending(line);
            if !body.is_empty() {
                self.push_line_stream_text(body, out);
            }
            self.finish_line_stream(out);
            return;
        }
        self.push_line(line, true, out);
    }

    fn push_partial_line(&mut self, out: &mut Vec<String>) {
        if self.pending_line.is_empty()
            || self.code_fence.is_some()
            || self.table_candidate.is_some()
            || self.table_buffer.is_some()
        {
            return;
        }

        if self.line_stream.is_some() {
            let text = std::mem::take(&mut self.pending_line);
            self.push_line_stream_text(&text, out);
            return;
        }

        if !self.pending_line.trim().is_empty() {
            self.flush_pending_block_separator(out);
        }

        let Some((state, prefix_len, rendered_prefix)) =
            classify_streaming_line(&self.pending_line)
        else {
            return;
        };
        self.inline_stream.set_base(line_stream_base_styles(state));
        self.line_stream = Some(state);
        self.line_prefix = Some(LineStreamPrefix {
            raw: self.pending_line[..prefix_len].to_string(),
            rendered: rendered_prefix,
            emitted: false,
        });
        let rest = self.pending_line[prefix_len..].to_string();
        self.pending_line.clear();
        if !rest.is_empty() {
            self.push_line_stream_text(&rest, out);
        }
    }

    fn push_line_stream_text(&mut self, text: &str, out: &mut Vec<String>) {
        let mut rendered = Vec::new();
        self.inline_stream.push(text, &mut rendered);
        if rendered.is_empty() {
            return;
        }
        self.emit_rendered_line_prefix(out);
        out.extend(rendered);
    }

    fn emit_rendered_line_prefix(&mut self, out: &mut Vec<String>) {
        let Some(prefix) = self.line_prefix.as_mut() else {
            return;
        };
        if prefix.emitted {
            return;
        }
        if !prefix.rendered.is_empty() {
            out.push(prefix.rendered.clone());
        }
        prefix.emitted = true;
    }

    fn finish_line_stream(&mut self, out: &mut Vec<String>) {
        let Some(state) = self.line_stream.take() else {
            return;
        };
        let mut rendered = Vec::new();
        self.inline_stream.finish(&mut rendered);
        let raw_fallback = self
            .line_prefix
            .as_ref()
            .is_some_and(|prefix| !prefix.emitted);
        if raw_fallback {
            if let Some(prefix) = self.line_prefix.take() {
                out.push(prefix.raw);
            }
            out.extend(rendered);
            out.push("\n".to_string());
            return;
        }
        self.line_prefix = None;
        out.extend(rendered);
        if matches!(
            state,
            LineStreamState::Heading { .. } | LineStreamState::Quote
        ) {
            out.push(RESET.to_string());
        }
        if matches!(state, LineStreamState::Heading { .. }) {
            out.push("\n".to_string());
            self.pending_block_separator = true;
        } else {
            out.push("\n".to_string());
        }
    }

    fn push_line(&mut self, line: &str, complete: bool, out: &mut Vec<String>) {
        self.resolve_pending_block_separator(line, out);
        if let Some(fence) = self.code_fence {
            if complete && is_closing_fence(line, fence) {
                self.code_fence = None;
                out.push(RESET.to_string());
                if !fence.has_content {
                    out.push("\n".to_string());
                }
            } else {
                self.code_fence.as_mut().unwrap().has_content = true;
                out.push(line.to_string());
            }
            return;
        }

        if let Some(fence) = opening_fence(line) {
            self.flush_table_candidate(out);
            self.flush_table_buffer(out);
            self.code_fence = Some(fence);
            out.push(
                code_block_styles()
                    .iter()
                    .map(|style| style.ansi())
                    .collect::<String>(),
            );
            return;
        }

        if self.table_buffer.is_some() {
            if complete && is_table_row_like(line) {
                self.table_buffer.as_mut().unwrap().push_str(line);
                return;
            }
            self.flush_table_buffer(out);
            self.push_line(line, complete, out);
            return;
        }

        if let Some(candidate) = self.table_candidate.take() {
            if complete && is_table_delimiter_line(line) {
                let mut table = candidate;
                table.push_str(line);
                self.table_buffer = Some(table);
                return;
            }
            self.push_non_table_line(&candidate, true, out);
            self.push_line(line, complete, out);
            return;
        }

        if complete && is_table_candidate_line(line) {
            self.table_candidate = Some(line.to_string());
            return;
        }

        self.push_non_table_line(line, complete, out);
    }

    fn push_non_table_line(&mut self, line: &str, complete: bool, out: &mut Vec<String>) {
        if line.trim().is_empty() {
            out.push(line.to_string());
            return;
        }
        if parse_heading_line(line).is_some() {
            if let Some(rendered) = render_heading_line(line) {
                out.push(rendered);
                self.pending_block_separator = true;
            } else {
                out.push(line.to_string());
            }
            return;
        }
        if is_single_line_block(line.trim()) {
            out.push(render_markdown(line));
            return;
        }
        if let Some(rendered) = render_list_line(line, complete) {
            out.push(rendered);
            return;
        }
        if let Some(rendered) = render_block_quote_line(line, complete) {
            out.push(rendered);
            return;
        }
        out.push(render_inline_or_raw_line(line, complete));
    }

    fn flush_table_candidate(&mut self, out: &mut Vec<String>) {
        if let Some(candidate) = self.table_candidate.take() {
            self.push_non_table_line(&candidate, true, out);
        }
    }

    fn flush_table_buffer(&mut self, out: &mut Vec<String>) {
        if let Some(table) = self.table_buffer.take() {
            out.push(render_markdown(&table));
        }
    }

    fn resolve_pending_block_separator(&mut self, line: &str, out: &mut Vec<String>) {
        if !self.pending_block_separator {
            return;
        }
        self.pending_block_separator = false;
        if !line.trim().is_empty() {
            out.push("\n".to_string());
        }
    }

    fn flush_pending_block_separator(&mut self, out: &mut Vec<String>) {
        if self.pending_block_separator {
            self.pending_block_separator = false;
            out.push("\n".to_string());
        }
    }
}

fn fence_marker(line: &str) -> Option<FenceState> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = line.chars().take_while(|ch| *ch == first).count();
    (count >= 3).then_some(FenceState {
        kind: first,
        width: count,
        has_content: false,
    })
}

fn opening_fence(line: &str) -> Option<FenceState> {
    let trimmed = line.trim_start();
    let leading = line.len().saturating_sub(trimmed.len());
    if leading > 3 {
        return None;
    }
    fence_marker(trimmed.trim_end()).map(|fence| FenceState {
        has_content: false,
        ..fence
    })
}

fn is_closing_fence(line: &str, fence: FenceState) -> bool {
    let trimmed = line.trim();
    let Some(closing) = closing_fence_marker(trimmed) else {
        return false;
    };
    closing.kind == fence.kind && closing.width >= fence.width
}

fn closing_fence_marker(line: &str) -> Option<FenceState> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = line.chars().take_while(|ch| *ch == first).count();
    if count < 3 || !line[count..].trim().is_empty() {
        return None;
    }
    Some(FenceState {
        kind: first,
        width: count,
        has_content: false,
    })
}

fn is_table_row_like(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.contains('|') && !trimmed.is_empty()
}

fn is_table_candidate_line(line: &str) -> bool {
    if !is_table_row_like(line) {
        return false;
    }
    let (body, _) = split_line_ending(line);
    let trimmed = body.trim_start_matches(' ');
    !is_single_line_block(trimmed)
        && parse_list_marker(trimmed).is_none()
        && !trimmed.starts_with('>')
}

fn is_table_delimiter_line(line: &str) -> bool {
    let trimmed = line.trim().trim_matches('|').trim();
    if trimmed.is_empty() {
        return false;
    }
    trimmed.split('|').all(|cell| {
        let cell = cell.trim();
        let hyphens = cell.chars().filter(|ch| *ch == '-').count();
        hyphens >= 3
            && cell
                .chars()
                .all(|ch| ch == '-' || ch == ':' || ch.is_whitespace())
    })
}

fn classify_streaming_line(line: &str) -> Option<(LineStreamState, usize, String)> {
    let trimmed_start = line.trim_start_matches(' ');
    let leading = line.len().saturating_sub(trimmed_start.len());

    if line.trim().is_empty() || (leading > 0 && trimmed_start.is_empty()) {
        return None;
    }
    if leading <= 3 && starts_possible_fence(trimmed_start) {
        return None;
    }
    if leading <= 3 && trimmed_start.starts_with('|') {
        return None;
    }

    if let Some((level, prefix_len)) = parse_streaming_heading_prefix(line) {
        let mut prefix = String::new();
        for style in heading_styles(level) {
            prefix.push_str(style.ansi());
        }
        return Some((LineStreamState::Heading { level }, prefix_len, prefix));
    }

    if let Some((depth, prefix_len)) = parse_streaming_quote_prefix(line) {
        let mut prefix = String::new();
        prefix.push_str(DIM);
        prefix.push_str(&"│ ".repeat(depth));
        return Some((LineStreamState::Quote, prefix_len, prefix));
    }

    if let Some((marker, prefix_len)) = parse_streaming_list_prefix(line) {
        return Some((LineStreamState::List, prefix_len, marker));
    }

    if is_ambiguous_line_prefix(line) {
        return None;
    }

    Some((LineStreamState::Prose, 0, String::new()))
}

fn line_stream_base_styles(state: LineStreamState) -> &'static [MdStyle] {
    match state {
        LineStreamState::Prose | LineStreamState::List => &[],
        LineStreamState::Heading { level } => heading_styles(level),
        LineStreamState::Quote => &[MdStyle::Dim],
    }
}

fn starts_possible_fence(line: &str) -> bool {
    let Some(first) = line.chars().next() else {
        return false;
    };
    if first != '`' && first != '~' {
        return false;
    }
    let count = line.chars().take_while(|ch| *ch == first).count();
    count >= 3 || count == line.chars().count()
}

fn parse_streaming_heading_prefix(line: &str) -> Option<(HeadingLevel, usize)> {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len().saturating_sub(trimmed.len());
    if leading > 3 {
        return None;
    }
    let width = trimmed.chars().take_while(|ch| *ch == '#').count();
    if width == 0 || width > 6 {
        return None;
    }
    let after_hashes = &trimmed[width..];
    let whitespace = after_hashes
        .char_indices()
        .take_while(|(_, ch)| ch.is_whitespace())
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())?;
    let level = match width {
        1 => HeadingLevel::H1,
        2 => HeadingLevel::H2,
        3 => HeadingLevel::H3,
        4 => HeadingLevel::H4,
        5 => HeadingLevel::H5,
        6 => HeadingLevel::H6,
        _ => return None,
    };
    Some((level, leading + width + whitespace))
}

fn parse_streaming_quote_prefix(line: &str) -> Option<(usize, usize)> {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len().saturating_sub(trimmed.len());
    if leading > 3 || !trimmed.starts_with('>') {
        return None;
    }
    let marker_len = trimmed.chars().take_while(|ch| *ch == '>').count();
    let rest = &trimmed[marker_len..];
    if rest.is_empty() {
        return None;
    }
    let space_len = rest
        .chars()
        .next()
        .filter(|ch| ch.is_whitespace())
        .map(char::len_utf8)
        .unwrap_or(0);
    Some((marker_len, leading + marker_len + space_len))
}

fn parse_streaming_list_prefix(line: &str) -> Option<(String, usize)> {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len().saturating_sub(trimmed.len());
    let (marker, rest, marker_len) = parse_list_marker_with_len(trimmed)?;
    if is_partial_task_marker(rest) {
        return None;
    }

    let mut rendered = String::new();
    rendered.push_str(&"  ".repeat(leading / 2));
    rendered.push_str(&marker);
    let mut consumed = leading + marker_len;
    if let Some((task, task_len)) = parse_task_marker_len(rest) {
        rendered.push_str(task);
        consumed += task_len;
    }
    Some((rendered, consumed))
}

fn is_ambiguous_line_prefix(line: &str) -> bool {
    let trimmed = line.trim_start_matches(' ');
    let leading = line.len().saturating_sub(trimmed.len());
    if leading > 3 {
        return false;
    }
    if trimmed.chars().all(|ch| ch == '#') && trimmed.chars().count() <= 6 {
        return true;
    }
    if matches!(trimmed, "-" | "+" | "*" | ">" | ">>" | ">>>") {
        return true;
    }
    if trimmed.len() >= 2
        && ['-', '*', '_']
            .into_iter()
            .any(|mark| trimmed.chars().all(|ch| ch == mark || ch.is_whitespace()))
    {
        return true;
    }
    if trimmed.chars().all(|ch| ch.is_ascii_digit())
        || trimmed
            .strip_suffix(['.', ')'])
            .is_some_and(|prefix| prefix.chars().all(|ch| ch.is_ascii_digit()))
    {
        return true;
    }
    false
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

fn render_heading_line(line: &str) -> Option<String> {
    let (level, content) = parse_heading_line(line)?;
    let rendered = render_inline_markdown(content)?;
    let mut out = String::new();
    let mut styles = Vec::new();
    push_styles(&mut out, &mut styles, heading_styles(level));
    out.push_str(&rendered);
    out.push_str(RESET);
    out.push('\n');
    Some(out)
}

fn parse_heading_line(line: &str) -> Option<(HeadingLevel, &str)> {
    let (body, _) = split_line_ending(line);
    let trimmed = body.trim_start_matches(' ');
    let leading = body.len().saturating_sub(trimmed.len());
    if leading > 3 {
        return None;
    }
    let width = trimmed.chars().take_while(|ch| *ch == '#').count();
    if width == 0 || width > 6 {
        return None;
    }
    let rest = &trimmed[width..];
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return None;
    }
    let content = rest.trim();
    let level = match width {
        1 => HeadingLevel::H1,
        2 => HeadingLevel::H2,
        3 => HeadingLevel::H3,
        4 => HeadingLevel::H4,
        5 => HeadingLevel::H5,
        6 => HeadingLevel::H6,
        _ => return None,
    };
    Some((level, content))
}

fn split_line_ending(line: &str) -> (&str, &str) {
    line.strip_suffix('\n')
        .map(|body| (body, "\n"))
        .unwrap_or((line, ""))
}

fn render_inline_or_raw_line(line: &str, complete: bool) -> String {
    let (body, ending) = split_line_ending(line);
    render_inline_markdown(body)
        .map(|rendered| format!("{rendered}{ending}"))
        .unwrap_or_else(|| {
            if complete {
                line.to_string()
            } else {
                body.to_string()
            }
        })
}

fn render_list_line(line: &str, complete: bool) -> Option<String> {
    let (body, ending) = split_line_ending(line);
    let trimmed = body.trim_start_matches(' ');
    let indent = body.len().saturating_sub(trimmed.len());
    let (marker, rest) = parse_list_marker(trimmed)?;
    let (task, rest) = parse_task_marker(rest);
    let rendered = render_inline_markdown(rest)?;

    let mut out = String::new();
    out.push_str(&"  ".repeat(indent / 2));
    out.push_str(&marker);
    if let Some(task) = task {
        out.push_str(task);
    }
    out.push_str(&rendered);
    if complete {
        out.push_str(ending);
    }
    Some(out)
}

fn parse_list_marker(line: &str) -> Option<(String, &str)> {
    parse_list_marker_with_len(line).map(|(marker, rest, _)| (marker, rest))
}

fn parse_list_marker_with_len(line: &str) -> Option<(String, &str, usize)> {
    let mut chars = line.char_indices();
    let (_, first) = chars.next()?;
    if matches!(first, '-' | '+' | '*') {
        let (idx, next) = chars.next()?;
        if next.is_whitespace() {
            let marker_len = idx + next.len_utf8();
            let rest = &line[marker_len..];
            let trimmed = rest.trim_start();
            return Some((
                "• ".to_string(),
                trimmed,
                marker_len + rest.len().saturating_sub(trimmed.len()),
            ));
        }
        return None;
    }

    let digit_end = line
        .char_indices()
        .take_while(|(_, ch)| ch.is_ascii_digit())
        .last()
        .map(|(idx, ch)| idx + ch.len_utf8())?;
    if digit_end == 0 {
        return None;
    }
    let mut rest = line[digit_end..].char_indices();
    let (_, delimiter) = rest.next()?;
    if delimiter != '.' && delimiter != ')' {
        return None;
    }
    let (space_idx, space) = rest.next()?;
    if !space.is_whitespace() {
        return None;
    }
    let number = &line[..digit_end];
    let marker_len = digit_end + space_idx + space.len_utf8();
    let rest = &line[marker_len..];
    let trimmed = rest.trim_start();
    Some((
        format!("{number}. "),
        trimmed,
        marker_len + rest.len().saturating_sub(trimmed.len()),
    ))
}

fn parse_task_marker(text: &str) -> (Option<&'static str>, &str) {
    if let Some((rendered, len)) = parse_task_marker_len(text) {
        return (Some(rendered), &text[len..]);
    }
    (None, text)
}

fn parse_task_marker_len(text: &str) -> Option<(&'static str, usize)> {
    for (raw, rendered) in [("[ ] ", "[ ] "), ("[x] ", "[✓] "), ("[X] ", "[✓] ")] {
        if text.starts_with(raw) {
            return Some((rendered, raw.len()));
        }
    }
    None
}

fn is_partial_task_marker(text: &str) -> bool {
    !text.is_empty()
        && ["[ ] ", "[x] ", "[X] "]
            .into_iter()
            .any(|marker| marker.starts_with(text))
}

fn render_block_quote_line(line: &str, complete: bool) -> Option<String> {
    let (body, ending) = split_line_ending(line);
    let mut rest = body.trim_start_matches(' ');
    let leading = body.len().saturating_sub(rest.len());
    if leading > 3 {
        return None;
    }

    let mut depth = 0usize;
    while let Some(after_marker) = rest.strip_prefix('>') {
        depth += 1;
        rest = after_marker.strip_prefix(' ').unwrap_or(after_marker);
    }
    if depth == 0 {
        return None;
    }

    let rendered = render_inline_markdown(rest)?;
    let mut out = String::new();
    out.push_str(DIM);
    out.push_str(&"│ ".repeat(depth));
    out.push_str(&rendered);
    out.push_str(RESET);
    if complete {
        out.push_str(ending);
    }
    Some(out)
}

fn render_inline_markdown(markdown: &str) -> Option<String> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(markdown, options);
    let mut out = String::new();
    let mut styles: Vec<MdStyle> = Vec::new();
    let mut links: Vec<String> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {}
                Tag::Emphasis => push_styles(&mut out, &mut styles, emphasis_styles()),
                Tag::Strong => push_styles(&mut out, &mut styles, strong_styles()),
                Tag::Strikethrough => push_style(&mut out, &mut styles, MdStyle::Strike),
                Tag::Link { dest_url, .. } => {
                    links.push(dest_url.to_string());
                    push_styles(&mut out, &mut styles, link_styles());
                    out.push_str(&open_hyperlink(&dest_url));
                }
                _ => return None,
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {}
                TagEnd::Emphasis => pop_styles(&mut out, &mut styles, emphasis_styles().len()),
                TagEnd::Strong => pop_styles(&mut out, &mut styles, strong_styles().len()),
                TagEnd::Strikethrough => pop_style(&mut out, &mut styles),
                TagEnd::Link => {
                    out.push_str(OSC8_CLOSE);
                    pop_styles(&mut out, &mut styles, link_styles().len());
                    let url = links.pop()?;
                    out.push_str(DIM);
                    out.push_str(" (");
                    out.push_str(&hyperlink_text(&url, &url));
                    out.push(')');
                    out.push_str(RESET);
                    for style in &styles {
                        out.push_str(style.ansi());
                    }
                }
                _ => return None,
            },
            Event::Text(text) => out.push_str(&text),
            Event::Code(code) => {
                for style in inline_code_styles() {
                    out.push_str(style.ansi());
                }
                out.push_str(&code);
                out.push_str(RESET);
                for style in &styles {
                    out.push_str(style.ansi());
                }
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            _ => return None,
        }
    }

    if styles.is_empty() && links.is_empty() {
        Some(out)
    } else {
        None
    }
}

impl InlineStream {
    fn set_base(&mut self, styles: &'static [MdStyle]) {
        self.base_styles.clear();
        self.base_styles.extend_from_slice(styles);
    }

    fn push(&mut self, text: &str, out: &mut Vec<String>) {
        self.pending.push_str(text);
        self.flush_available(out);
    }

    fn finish(&mut self, out: &mut Vec<String>) {
        if !self.pending.is_empty() {
            out.push(std::mem::take(&mut self.pending));
        }
        self.base_styles.clear();
    }

    fn flush_available(&mut self, out: &mut Vec<String>) {
        loop {
            let Some(marker) = earliest_inline_marker(&self.pending) else {
                if !self.pending.is_empty() {
                    out.push(std::mem::take(&mut self.pending));
                }
                return;
            };
            if marker > 0 {
                out.push(self.pending[..marker].to_string());
                self.pending.replace_range(..marker, "");
            }
            let Some((rendered, consumed)) = self.render_span_at_start() else {
                return;
            };
            out.push(rendered);
            self.pending.replace_range(..consumed, "");
        }
    }

    fn render_span_at_start(&self) -> Option<(String, usize)> {
        if self.pending.starts_with("![") {
            return None;
        }
        if self.pending.starts_with("**") {
            return self.render_delimited("**", strong_styles());
        }
        if self.pending.starts_with("__") {
            return self.render_delimited("__", strong_styles());
        }
        if self.pending.starts_with("~~") {
            return self.render_delimited("~~", &[MdStyle::Strike]);
        }
        if self.pending.starts_with('*') {
            return self.render_delimited("*", emphasis_styles());
        }
        if self.pending.starts_with('_') {
            return self.render_delimited("_", emphasis_styles());
        }
        if self.pending.starts_with('`') {
            return self.render_delimited("`", inline_code_styles());
        }
        if self.pending.starts_with('[') {
            return self.render_link();
        }
        None
    }

    fn render_delimited(&self, delimiter: &str, styles: &[MdStyle]) -> Option<(String, usize)> {
        let rest = &self.pending[delimiter.len()..];
        let end = rest.find(delimiter)?;
        let body = &rest[..end];
        let mut out = String::new();
        for style in styles {
            out.push_str(style.ansi());
        }
        out.push_str(body);
        out.push_str(RESET);
        self.reapply_base(&mut out);
        Some((out, delimiter.len() + end + delimiter.len()))
    }

    fn render_link(&self) -> Option<(String, usize)> {
        let label_end = self.pending.find("](")?;
        let url_start = label_end + 2;
        let url_end = self.pending[url_start..].find(')')? + url_start;
        let label = &self.pending[1..label_end];
        let url = &self.pending[url_start..url_end];

        let mut out = String::new();
        for style in link_styles() {
            out.push_str(style.ansi());
        }
        out.push_str(&open_hyperlink(url));
        out.push_str(label);
        out.push_str(OSC8_CLOSE);
        out.push_str(RESET);
        self.reapply_base(&mut out);
        out.push_str(DIM);
        out.push_str(" (");
        out.push_str(&hyperlink_text(url, url));
        out.push(')');
        out.push_str(RESET);
        self.reapply_base(&mut out);
        Some((out, url_end + 1))
    }

    fn reapply_base(&self, out: &mut String) {
        for style in &self.base_styles {
            out.push_str(style.ansi());
        }
    }
}

fn earliest_inline_marker(text: &str) -> Option<usize> {
    text.char_indices().find_map(|(idx, ch)| match ch {
        '*' | '_' | '`' | '[' => Some(idx),
        '!' if text[idx..].starts_with("![") => Some(idx),
        '~' if text[idx..].starts_with("~~") => Some(idx),
        _ => None,
    })
}

#[derive(Clone, Copy)]
enum MdStyle {
    Bold,
    Dim,
    Italic,
    Underline,
    Strike,
    Blue,
    Code,
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
            Self::Code => CODE,
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
                Tag::List(start) => {
                    if in_item > 0 {
                        let target = current_render_target(&mut out, &mut table_state);
                        if !target.ends_with('\n') {
                            target.push('\n');
                        }
                    }
                    lists.push(ListState { next: start });
                }
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
                    let target = current_render_target(&mut out, &mut table_state);
                    if !target.ends_with('\n') {
                        target.push('\n');
                    }
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
    for width in &mut widths {
        *width = (*width).min(MAX_TABLE_COLUMN_WIDTH);
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
    let cells = widths
        .iter()
        .copied()
        .enumerate()
        .map(|(idx, width)| wrap_table_cell(row.get(idx).map(String::as_str).unwrap_or(""), width))
        .collect::<Vec<_>>();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    let mut out = String::new();

    for line_idx in 0..height {
        out.push('|');
        for (idx, width) in widths.iter().copied().enumerate() {
            let cell = cells[idx].get(line_idx).map(String::as_str).unwrap_or("");
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
    }
    out
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TableTextStyle {
    sgr: Vec<String>,
    hyperlink: Option<String>,
}

#[derive(Clone, Debug)]
struct StyledGrapheme {
    text: String,
    width: usize,
    whitespace: bool,
    style: TableTextStyle,
    controls: String,
}

fn wrap_table_cell(cell: &str, width: usize) -> Vec<String> {
    if visible_text_width(cell) <= width {
        return vec![cell.to_string()];
    }

    let units = styled_graphemes(cell);
    wrap_styled_graphemes(&units, width)
        .into_iter()
        .map(|line| render_styled_graphemes(&line))
        .collect()
}

fn styled_graphemes(input: &str) -> Vec<StyledGrapheme> {
    let mut units = Vec::new();
    let mut style = TableTextStyle::default();
    let mut controls = String::new();
    let mut offset = 0;

    while offset < input.len() {
        if input.as_bytes()[offset] == b'\x1b' {
            let end = ansi_sequence_end(input, offset);
            let sequence = &input[offset..end];
            if sequence.starts_with("\x1b[") && sequence.ends_with('m') {
                if sequence == RESET || sequence == "\x1b[m" {
                    style.sgr.clear();
                } else {
                    style.sgr.push(sequence.to_string());
                }
            } else if sequence.starts_with(OSC8_OPEN) {
                if sequence == OSC8_CLOSE {
                    style.hyperlink = None;
                } else {
                    style.hyperlink = Some(sequence.to_string());
                }
            } else {
                controls.push_str(sequence);
            }
            offset = end;
            continue;
        }

        let next_escape = input[offset..]
            .find('\x1b')
            .map(|relative| offset + relative)
            .unwrap_or(input.len());
        for grapheme in input[offset..next_escape].graphemes(true) {
            units.push(StyledGrapheme {
                text: grapheme.to_string(),
                width: UnicodeWidthStr::width(grapheme),
                whitespace: grapheme.chars().all(char::is_whitespace),
                style: style.clone(),
                controls: std::mem::take(&mut controls),
            });
        }
        offset = next_escape;
    }

    if !controls.is_empty()
        && let Some(last) = units.last_mut()
    {
        last.controls.push_str(&controls);
    }
    units
}

fn ansi_sequence_end(input: &str, start: usize) -> usize {
    let bytes = input.as_bytes();
    match bytes.get(start + 1) {
        Some(b'[') => bytes[start + 2..]
            .iter()
            .position(|byte| (b'@'..=b'~').contains(byte))
            .map(|relative| start + 3 + relative)
            .unwrap_or(input.len()),
        Some(b']') => {
            let mut idx = start + 2;
            while idx < bytes.len() {
                if bytes[idx] == b'\x07' {
                    return idx + 1;
                }
                if bytes[idx] == b'\x1b' && bytes.get(idx + 1) == Some(&b'\\') {
                    return idx + 2;
                }
                idx += 1;
            }
            input.len()
        }
        Some(_) => (start + 2).min(input.len()),
        None => input.len(),
    }
}

fn wrap_styled_graphemes(units: &[StyledGrapheme], width: usize) -> Vec<Vec<StyledGrapheme>> {
    let mut lines = Vec::new();
    let mut line = Vec::new();
    let mut pending_whitespace = Vec::new();
    let mut idx = 0;

    while idx < units.len() {
        if units[idx].whitespace {
            pending_whitespace.push(units[idx].clone());
            idx += 1;
            continue;
        }

        let word_start = idx;
        while idx < units.len() && !units[idx].whitespace {
            idx += 1;
        }
        let word = &units[word_start..idx];
        let line_width = styled_graphemes_width(&line);
        let whitespace_width = styled_graphemes_width(&pending_whitespace);
        let word_width = styled_graphemes_width(word);

        if !line.is_empty() && line_width + whitespace_width + word_width <= width {
            line.append(&mut pending_whitespace);
            line.extend_from_slice(word);
            continue;
        }

        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        pending_whitespace.clear();

        if word_width <= width {
            line.extend_from_slice(word);
            continue;
        }

        for unit in word {
            if !line.is_empty() && styled_graphemes_width(&line) + unit.width > width {
                lines.push(std::mem::take(&mut line));
            }
            line.push(unit.clone());
        }
    }

    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(Vec::new());
    }
    lines
}

fn styled_graphemes_width(units: &[StyledGrapheme]) -> usize {
    units.iter().map(|unit| unit.width).sum()
}

fn render_styled_graphemes(units: &[StyledGrapheme]) -> String {
    let mut out = String::new();
    let mut active = TableTextStyle::default();

    for unit in units {
        if unit.style != active {
            if active.hyperlink.is_some() {
                out.push_str(OSC8_CLOSE);
            }
            if active.sgr != unit.style.sgr {
                if !active.sgr.is_empty() {
                    out.push_str(RESET);
                }
                for sgr in &unit.style.sgr {
                    out.push_str(sgr);
                }
            }
            if let Some(hyperlink) = &unit.style.hyperlink {
                out.push_str(hyperlink);
            }
            active = unit.style.clone();
        }
        out.push_str(&unit.controls);
        out.push_str(&unit.text);
    }

    if active.hyperlink.is_some() {
        out.push_str(OSC8_CLOSE);
    }
    if !active.sgr.is_empty() {
        out.push_str(RESET);
    }
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
        HeadingLevel::H1 => &[MdStyle::Bold, MdStyle::Underline],
        HeadingLevel::H2 => &[MdStyle::Bold],
        HeadingLevel::H3 => &[MdStyle::Bold],
        HeadingLevel::H4 => &[MdStyle::Underline],
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
    &[MdStyle::Code]
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
fn format_bash_header(title: &str, command: &str, risk: Option<&str>, styled: bool) -> String {
    let command = preview_first_line(command, BASH_COMMAND_PREVIEW_BYTES);
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
        out.push_str(BOLD);
        out.push_str("# ");
        out.push_str(title);
        out.push_str(RESET);
        out.push('\n');
    }
    out.push_str(bash_risk_color(risk));
    out.push_str(BOLD);
    out.push('$');
    out.push(' ');
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

fn format_thinking_live(
    elapsed: Duration,
    output_tokens: Option<u64>,
    title: Option<&str>,
) -> String {
    format_thought(
        elapsed,
        output_tokens.map(|tokens| format!("~{tokens}")),
        title,
        true,
    )
}

fn format_table_live(output_tokens: u64, styled: bool) -> String {
    if styled {
        format!("{GRAY}[table ~{output_tokens} tokens]{RESET}")
    } else {
        format!("[table ~{output_tokens} tokens]")
    }
}

fn format_stdin_summary(bytes: usize, styled: bool) -> String {
    let suffix = if bytes == 1 { "byte" } else { "bytes" };
    if styled {
        format!("{BLUE}< [stdin {bytes} {suffix}]{RESET}")
    } else {
        format!("< [stdin {bytes} {suffix}]")
    }
}

fn format_stdin_summary_line(bytes: usize, styled: bool) -> String {
    format!("{}\n", format_stdin_summary(bytes, styled))
}

fn format_cwd_line(raw_cwd: &str, styled: bool) -> String {
    if styled {
        format!("{DIM}@{RESET} {GRAY}{raw_cwd}{RESET}\n")
    } else {
        format!("@ {raw_cwd}\n")
    }
}

fn should_render_bash_cwd(raw_cwd: &str) -> bool {
    let pwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let raw_path = PathBuf::from(raw_cwd);
    let resolved = if raw_path.is_absolute() {
        raw_path
    } else {
        pwd.join(raw_path)
    };
    lexical_normalize_path(&resolved) != lexical_normalize_path(&pwd)
}

fn lexical_normalize_path(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push(component.as_os_str());
                }
            }
            Component::Normal(_) | Component::RootDir | Component::Prefix(_) => {
                normalized.push(component.as_os_str());
            }
        }
    }
    normalized
}

fn format_thought_line(
    elapsed: Duration,
    output_tokens: Option<String>,
    title: Option<&str>,
    styled: bool,
) -> String {
    format!(
        "{}\n",
        format_thought(elapsed, output_tokens, title, styled,)
    )
}

fn format_thought(
    elapsed: Duration,
    output_tokens: Option<String>,
    title: Option<&str>,
    styled: bool,
) -> String {
    let elapsed = format_duration(elapsed);
    let tokens = output_tokens
        .map(|tokens| format!(", {tokens} tokens"))
        .unwrap_or_default();
    let title = title.map(|title| format!(" {title}")).unwrap_or_default();
    if styled {
        format!("{GRAY}[thought {elapsed}{tokens}]{title}{RESET}")
    } else {
        format!("[thought {elapsed}{tokens}]{title}")
    }
}

fn extract_reasoning_summary_title(summary: &str, finalized: bool) -> Option<String> {
    let mut lines = summary.split_inclusive('\n');
    while let Some(raw_line) = lines.next() {
        let terminated = raw_line.ends_with('\n');
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        if !terminated && !finalized {
            return None;
        }
        return parse_reasoning_summary_title(line)
            .map(|title| truncate_visible_text(&title, REASONING_TITLE_MAX_WIDTH));
    }
    None
}

fn parse_reasoning_summary_title(line: &str) -> Option<String> {
    let title = if line.starts_with("**") && line.ends_with("**") && line.len() > 4 {
        let inner = &line[2..line.len() - 2];
        (!inner.contains("**")).then_some(inner)
    } else {
        let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
        if !(1..=6).contains(&hashes) || line.as_bytes().get(hashes) != Some(&b' ') {
            return None;
        }
        let mut inner = line[hashes + 1..].trim();
        if let Some(without_hashes) = inner.trim_end_matches('#').strip_suffix(' ') {
            inner = without_hashes.trim_end();
        }
        Some(inner)
    }?;
    let normalized = title.split_whitespace().collect::<Vec<_>>().join(" ");
    (!normalized.is_empty()).then_some(normalized)
}

fn truncate_visible_text(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let budget = max_width.saturating_sub(UnicodeWidthStr::width(ELLIPSIS));
    let mut out = String::new();
    let mut width = 0;
    for grapheme in text.graphemes(true) {
        let next = UnicodeWidthStr::width(grapheme);
        if width + next > budget {
            break;
        }
        out.push_str(grapheme);
        width += next;
    }
    out.push_str(ELLIPSIS);
    out
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

    let mut tail_start = if finalizing {
        complete_lines
            .len()
            .saturating_sub(BASH_TAIL_LINE_RESERVE)
            .max(head_count)
    } else {
        complete_lines.len()
    };
    // Do not put a marker between the head and tail for a single complete line:
    // rendering that line is clearer than saying it was omitted.
    if finalizing && tail_start.saturating_sub(head_count) == 1 {
        tail_start = head_count;
    }
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

fn rendered_block_is_blank(input: &str) -> bool {
    strip_ansi(input).trim().is_empty()
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

fn format_turn_summary(
    input_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: Option<u64>,
    output_tokens: u64,
    context_pct: Option<f64>,
    elapsed: Duration,
) -> String {
    let ctx = context_pct
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "?".into());
    let mut cache = Vec::new();
    if cache_read_input_tokens > 0 {
        cache.push(format!(
            "+{} cache read",
            format_number(cache_read_input_tokens)
        ));
    }
    if let Some(cache_write_input_tokens) = cache_write_input_tokens {
        cache.push(format!(
            "+{} cache write",
            format_number(cache_write_input_tokens)
        ));
    }
    let cache = if cache.is_empty() {
        String::new()
    } else {
        format!(" ({})", cache.join(", "))
    };
    format!(
        "[mu] tokens: {} in{cache} / {} out  context: {ctx}  time: {}",
        format_number(input_tokens),
        format_number(output_tokens),
        format_duration(elapsed),
    )
}

fn format_number(number: u64) -> String {
    let digits = number.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(byte as char);
    }
    grouped
}

struct LinePreview {
    rendered: String,
    raw_kept_bytes: usize,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use unicode_width::UnicodeWidthChar;

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
    fn markdown_renderer_keeps_nested_list_items_on_separate_lines() {
        let rendered = render_markdown("- parent\n  - child\n");
        let plain = strip_ansi(&rendered);

        assert!(plain.contains("• parent\n  • child\n"), "{plain:?}");
        assert!(!plain.contains("• parent  • child"), "{plain:?}");
    }

    #[test]
    fn markdown_renderer_aligns_wide_table_cells() {
        let rendered =
            render_markdown("| Name | Value |\n| --- | ---: |\n| 字 | 10 |\n| ascii | 2 |\n");
        assert_table_grid_aligned(&strip_ansi(&rendered));
    }

    #[test]
    fn markdown_renderer_caps_table_columns_at_eighty_cells() {
        let eighty = "a".repeat(MAX_TABLE_COLUMN_WIDTH);
        let eighty_one = "b".repeat(MAX_TABLE_COLUMN_WIDTH + 1);
        let rendered = render_markdown(&format!(
            "| Text | Side |\n| --- | --- |\n| {eighty} | x |\n| {eighty_one} | y |\n"
        ));
        let plain = strip_ansi(&rendered);
        let table_lines = plain
            .lines()
            .filter(|line| line.starts_with('|') && line.ends_with('|'))
            .collect::<Vec<_>>();

        assert_eq!(bar_columns(table_lines[0])[1], MAX_TABLE_COLUMN_WIDTH + 3);
        assert_eq!(table_lines.len(), 5, "{plain:?}");
        assert!(table_lines[2].contains(&eighty), "{plain:?}");
        assert!(table_lines[3].contains(&"b".repeat(80)), "{plain:?}");
        assert!(table_lines[4].contains("b"), "{plain:?}");
        assert_table_grid_aligned(&plain);
    }

    #[test]
    fn table_rows_wrap_words_and_pad_shorter_cells() {
        let rendered = render_table_row(
            &["alpha beta gamma".into(), "x".into()],
            &[10, 3],
            &[Alignment::Left, Alignment::Left],
        );

        assert_eq!(
            strip_ansi(&rendered),
            "| alpha beta | x   |\n| gamma      |     |\n"
        );
    }

    #[test]
    fn wrapped_table_fragments_keep_column_alignment() {
        let row = vec!["alpha beta gamma".into()];

        assert_eq!(
            strip_ansi(&render_table_row(&row, &[10], &[Alignment::Right])),
            "| alpha beta |\n|      gamma |\n"
        );
        assert_eq!(
            strip_ansi(&render_table_row(&row, &[10], &[Alignment::Center])),
            "| alpha beta |\n|   gamma    |\n"
        );
    }

    #[test]
    fn table_cell_wrapping_keeps_unicode_graphemes_intact() {
        let wide = wrap_table_cell("界界界", 4)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();
        let emoji = wrap_table_cell("👩‍💻👩‍💻👩‍💻", 4)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();
        let combining = wrap_table_cell("e\u{301}e\u{301}e\u{301}", 2)
            .into_iter()
            .map(|line| strip_ansi(&line))
            .collect::<Vec<_>>();

        assert_eq!(wide, ["界界", "界"]);
        assert_eq!(emoji, ["👩‍💻👩‍💻", "👩‍💻"]);
        assert_eq!(combining, ["e\u{301}e\u{301}", "e\u{301}"]);
        for line in wide.iter().chain(&emoji).chain(&combining) {
            assert!(UnicodeWidthStr::width(line.as_str()) <= 4, "{line:?}");
        }
    }

    #[test]
    fn wrapped_table_cells_close_and_reopen_styles_and_links() {
        let styled = format!("{BOLD}abcdefgh{RESET}");
        let styled_lines = wrap_table_cell(&styled, 4);
        assert_eq!(styled_lines.len(), 2);
        for line in &styled_lines {
            assert!(line.starts_with(BOLD), "{line:?}");
            assert!(line.ends_with(RESET), "{line:?}");
            assert_eq!(visible_text_width(line), 4);
        }

        let open = open_hyperlink("https://example.com");
        let linked = format!("{open}abcdefgh{OSC8_CLOSE}");
        let linked_lines = wrap_table_cell(&linked, 4);
        assert_eq!(linked_lines.len(), 2);
        for line in &linked_lines {
            assert!(line.starts_with(&open), "{line:?}");
            assert!(line.ends_with(OSC8_CLOSE), "{line:?}");
            assert_eq!(visible_text_width(line), 4);
        }
    }

    #[test]
    fn short_styled_table_cells_are_not_rewritten() {
        let cell = format!("{BOLD}short{RESET}");
        assert_eq!(wrap_table_cell(&cell, 10), [cell]);
    }

    #[test]
    fn markdown_stream_streams_list_items_line_by_line() {
        let mut stream = MarkdownStream::default();

        let first = stream.push("- one\n").concat();
        assert_eq!(strip_ansi(&first), "• one\n");
        let second = stream.push("  - two\n").concat();
        assert_eq!(strip_ansi(&second), "  • two\n");
        let task = stream.push("- [x] done\n").concat();
        assert_eq!(strip_ansi(&task), "• [✓] done\n");
        let pipe = stream.push("- a | b\n").concat();
        assert_eq!(strip_ansi(&pipe), "• a | b\n");
    }

    #[test]
    fn markdown_stream_streams_prose_list_heading_and_quote_before_newline() {
        let mut stream = MarkdownStream::default();

        assert_eq!(strip_ansi(&stream.push("hello").concat()), "hello");
        assert_eq!(stream.push("\n").concat(), "\n");

        assert_eq!(stream.push("- ").concat(), "");
        assert_eq!(strip_ansi(&stream.push("item").concat()), "• item");
        assert_eq!(stream.push("\n").concat(), "\n");

        let heading_start = stream.push("## ").concat();
        assert_eq!(heading_start, "");
        let heading_title = stream.push("Title").concat();
        assert!(heading_title.contains(BOLD), "{heading_title:?}");
        assert_eq!(strip_ansi(&heading_title), "Title");
        assert_eq!(strip_ansi(&stream.push("\n").concat()), "\n");

        assert_eq!(strip_ansi(&stream.push("> quoted").concat()), "\n│ quoted");
        assert_eq!(strip_ansi(&stream.push("\n").concat()), "\n");
    }

    #[test]
    fn markdown_stream_waits_for_stable_heading_depth() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("##").concat(), "");
        let rendered = stream.push("# Title").concat();
        assert_eq!(strip_ansi(&rendered), "Title");
        assert!(rendered.contains(BOLD), "{rendered:?}");
    }

    #[test]
    fn markdown_stream_renders_heading_closing_hashes_literally() {
        let mut stream = MarkdownStream::default();

        let rendered = [
            stream.push("# title #\n").concat(),
            stream.finish().concat(),
        ]
        .concat();
        assert_eq!(strip_ansi(&rendered), "title #\n\n");
    }

    #[test]
    fn markdown_stream_keeps_exactly_one_empty_line_after_headings() {
        let mut stream = MarkdownStream::default();

        let rendered = [
            stream.push("## Heading\n").concat(),
            stream.push("\n").concat(),
            stream.push("body\n").concat(),
        ]
        .concat();
        assert_eq!(strip_ansi(&rendered), "Heading\n\nbody\n");

        let mut no_source_blank = MarkdownStream::default();
        let rendered = [
            no_source_blank.push("## Heading\n").concat(),
            no_source_blank.push("body\n").concat(),
        ]
        .concat();
        assert_eq!(strip_ansi(&rendered), "Heading\n\nbody\n");
    }

    #[test]
    fn markdown_stream_streams_fenced_code_without_fence_markers() {
        let mut stream = MarkdownStream::default();

        let open = stream.push("```sh\n").concat();
        assert!(open.contains(CODE), "{open:?}");
        assert!(!open.contains("```"), "{open:?}");
        let body = stream.push("echo hi\n").concat();
        assert_eq!(strip_ansi(&body), "echo hi\n");
        let close = stream.push("```\n").concat();
        assert_eq!(strip_ansi(&close), "");
        assert!(!close.contains("```"), "{close:?}");
    }

    #[test]
    fn markdown_stream_keeps_exactly_one_empty_line_after_fenced_code() {
        let mut stream = MarkdownStream::default();

        let rendered = [
            stream.push("```sh\n").concat(),
            stream.push("echo hi\n").concat(),
            stream.push("```\n").concat(),
            stream.push("\n").concat(),
            stream.push("body\n").concat(),
        ]
        .concat();
        assert_eq!(strip_ansi(&rendered), "echo hi\n\nbody\n");
    }

    #[test]
    fn markdown_stream_buffers_tables_until_table_ends() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("| Name | Value |\n").concat(), "");
        assert!(stream.table_live().is_none());
        assert_eq!(stream.push("| --- | ---: |\n").concat(), "");
        let live = stream.table_live().expect("confirmed table should be live");
        assert!(approx_tokens_from_chars(live.chars) > 0);
        assert_eq!(stream.push("| a | 1 |\n").concat(), "");
        assert!(stream.table_live().unwrap().chars > live.chars);
        let rendered = stream.push("\n").concat();
        assert!(stream.table_live().is_none());
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("| Name | Value |"), "{plain:?}");
        assert!(plain.contains("| a    |     1 |"), "{plain:?}");
        assert_table_grid_aligned(&plain);
    }

    #[test]
    fn markdown_stream_wraps_long_table_headers_and_cells_on_commit() {
        let mut stream = MarkdownStream::default();
        let header = "heading ".repeat(12);
        let body = "x".repeat(MAX_TABLE_COLUMN_WIDTH + 1);

        assert!(stream.push(&format!("| {header} | Side |\n")).is_empty());
        assert!(stream.push("| --- | --- |\n").is_empty());
        assert!(stream.push(&format!("| {body} | y |\n")).is_empty());
        assert!(stream.table_live().is_some());
        let rendered = stream.push("\n").concat();
        assert!(stream.table_live().is_none());

        let plain = strip_ansi(&rendered);
        let table_lines = plain
            .lines()
            .filter(|line| line.starts_with('|') && line.ends_with('|'))
            .collect::<Vec<_>>();
        let separator = table_lines
            .iter()
            .position(|line| line.contains("---"))
            .expect("table separator missing");
        assert_eq!(separator, 2, "{plain:?}");
        assert_eq!(table_lines.len(), 5, "{plain:?}");
        assert_table_grid_aligned(&plain);
    }

    #[test]
    fn markdown_stream_releases_non_table_pipe_lines_as_raw_markdown() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("a | b\n").concat(), "");
        let rendered = stream.push("next\n").concat();
        assert_eq!(strip_ansi(&rendered), "a | b\nnext\n");
    }

    #[test]
    fn markdown_stream_buffers_inline_links_until_line_is_complete() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("[docs](").concat(), "");
        let rendered = stream.push("https://example.com)\n").concat();
        assert!(rendered.contains(&open_hyperlink("https://example.com")));
        assert!(rendered.contains("docs"));
        assert!(rendered.contains("https://example.com"));
    }

    #[test]
    fn markdown_stream_outputs_unsupported_inline_markdown_raw() {
        let mut stream = MarkdownStream::default();

        let rendered = stream.push("![alt](image.png)\n").concat();
        assert_eq!(rendered, "![alt](image.png)\n");
        let heading = stream.push("# ![alt](image.png)\n").concat();
        assert_eq!(heading, "# ![alt](image.png)\n");
    }

    #[test]
    fn markdown_stream_keeps_chunked_unsupported_image_lines_raw() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("# ").concat(), "");
        let heading = stream.push("![alt](image.png)\n").concat();
        assert_eq!(heading, "# ![alt](image.png)\n");

        assert_eq!(stream.push("- ").concat(), "");
        let list = stream.push("![alt](image.png)\n").concat();
        assert_eq!(list, "- ![alt](image.png)\n");
    }

    #[test]
    fn markdown_stream_does_not_buffer_plain_exclamation_points() {
        let mut stream = MarkdownStream::default();

        assert_eq!(stream.push("Done!").concat(), "Done!");
        assert_eq!(stream.push(" Next").concat(), " Next");
        assert_eq!(stream.push("\n").concat(), "\n");
    }

    #[test]
    fn markdown_stream_keeps_single_tildes_literal_but_styles_double_tilde_strike() {
        let mut stream = MarkdownStream::default();

        let single = stream.push("about ~2 tokens\n").concat();
        assert_eq!(strip_ansi(&single), "about ~2 tokens\n");
        assert!(!single.contains(STRIKE), "{single:?}");

        let double = stream.push("keep ~~struck~~ text\n").concat();
        assert_eq!(strip_ansi(&double), "keep struck text\n");
        assert!(double.contains(STRIKE), "{double:?}");
    }

    #[test]
    fn markdown_stream_only_closes_fences_with_plain_closing_markers() {
        let mut stream = MarkdownStream::default();

        let rendered = [
            stream.push("```rust\n").concat(),
            stream.push("```not-a-close\n").concat(),
            stream.push("```\n").concat(),
        ]
        .concat();
        let plain = strip_ansi(&rendered);
        assert!(plain.contains("```not-a-close\n"), "{plain:?}");
        assert!(!plain.contains("```rust"), "{plain:?}");
    }

    #[test]
    fn terminal_markdown_streaming_commits_only_stable_constructs() {
        let raw = capture_markdown_stream_transcript();
        let normalized = strip_ansi(&raw.replace('\r', ""));

        assert!(
            normalized.contains("• one\n<after-list>\n"),
            "{normalized:?}"
        );
        assert!(
            normalized.contains("echo hi\n<after-code-body>\n"),
            "{normalized:?}"
        );
        assert!(!normalized.contains("```"), "{normalized:?}");

        let after_table_row = normalized
            .find("<after-table-row>")
            .expect("table-row marker missing");
        let table = normalized.find("| Name | Value |").expect("table missing");
        let after_table_flush = normalized
            .find("<after-table-flush>")
            .expect("table-flush marker missing");
        assert!(after_table_row < table, "{normalized:?}");
        assert!(table < after_table_flush, "{normalized:?}");
    }

    #[test]
    fn table_buffer_indicator_is_live_only_until_table_finishes() {
        let mut stream = MarkdownStream::default();
        let mut renderer = Renderer::with_format(OutputFormat::Terminal);
        renderer.force_styled_for_test();

        assert_eq!(stream.push("| Name | Value |\n").concat(), "");
        renderer.sync_table_live_line(stream.table_live()).unwrap();
        assert!(renderer.format_live_line().is_none());

        assert_eq!(stream.push("| --- | ---: |\n").concat(), "");
        renderer.sync_table_live_line(stream.table_live()).unwrap();
        let indicator = renderer
            .format_live_line()
            .expect("table indicator should be live");
        assert!(indicator.contains("[table ~"), "{indicator:?}");
        assert!(indicator.contains("tokens"), "{indicator:?}");

        let rendered = stream.push("\n").concat();
        renderer.sync_table_live_line(stream.table_live()).unwrap();
        assert!(renderer.format_live_line().is_none());
        assert!(strip_ansi(&rendered).contains("| Name | Value |"));
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

    fn capture_markdown_stream_transcript() -> String {
        let (mut renderer, output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);

        renderer.assistant_text("- one\n").unwrap();
        output.write_raw("<after-list>\n");
        renderer.assistant_text("```sh\n").unwrap();
        renderer.assistant_text("echo hi\n").unwrap();
        output.write_raw("<after-code-body>\n");
        renderer.assistant_text("```\n").unwrap();
        renderer.assistant_text("| Name | Value |\n").unwrap();
        renderer.assistant_text("| --- | ---: |\n").unwrap();
        renderer.assistant_text("| a | 1 |\n").unwrap();
        output.write_raw("<after-table-row>\n");
        renderer.assistant_text("\n").unwrap();
        output.write_raw("<after-table-flush>\n");

        output.transcript()
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
        assert!(header.starts_with(&format!("{BOLD}# List files{RESET}\n")));
        assert!(header.contains(&format!("{CYAN}{BOLD}$ printf 'a'…{RESET}\n")));
        assert!(!header.contains("  pwd\n"));
        assert!(!header.contains("[readonly]"));
    }

    #[test]
    fn tool_composition_indicator_is_replaced_before_title_commits() {
        let (mut renderer, output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);

        renderer.bash_header_start(Some("call_1")).unwrap();
        assert_eq!(
            output.transcript(),
            format!("{GRAY}[preparing toolcall]{RESET}")
        );
        assert!(matches!(
            renderer.live_line,
            Some(LiveLine::ToolComposition)
        ));

        renderer.bash_header_title_start().unwrap();
        renderer.bash_header_title_delta("Inspect").unwrap();
        renderer.bash_header_title_end().unwrap();

        assert!(renderer.live_line.is_none());
        assert_eq!(
            output.transcript(),
            format!("{GRAY}[preparing toolcall]{RESET}\r\x1b[2K{BOLD}# Inspect{RESET}\n")
        );
    }

    #[test]
    fn plain_tool_header_omits_composition_indicator() {
        let (mut renderer, output) =
            Renderer::with_test_shared_output(OutputFormat::Plain, false, None);

        renderer.bash_header_start(Some("call_1")).unwrap();
        assert_eq!(output.transcript(), "");

        renderer.bash_header_title_start().unwrap();
        renderer.bash_header_title_delta("Inspect").unwrap();
        renderer.bash_header_title_end().unwrap();
        renderer
            .bash_header_command_start(Some("readonly"))
            .unwrap();
        renderer.bash_header_command_delta("pwd").unwrap();
        renderer.bash_header_command_end().unwrap();

        assert_eq!(output.transcript(), "# Inspect\n$ [readonly] pwd\n");
    }

    #[test]
    fn stdin_summary_uses_bracketed_byte_count() {
        assert_eq!(format_stdin_summary_line(0, false), "< [stdin 0 bytes]\n");
        assert_eq!(format_stdin_summary_line(1, false), "< [stdin 1 byte]\n");
        assert_eq!(
            format_stdin_summary_line(12, true),
            format!("{BLUE}< [stdin 12 bytes]{RESET}\n")
        );
    }

    #[test]
    fn cwd_line_preserves_raw_cwd_text() {
        assert_eq!(format_cwd_line("../other", false), "@ ../other\n");
        assert_eq!(
            format_cwd_line("../other", true),
            format!("{DIM}@{RESET} {GRAY}../other{RESET}\n")
        );
    }

    #[test]
    fn cwd_line_only_renders_when_resolved_cwd_differs_from_pwd() {
        let pwd = std::env::current_dir().unwrap();
        assert!(!should_render_bash_cwd("."));
        assert!(!should_render_bash_cwd(&pwd.display().to_string()));
        assert!(should_render_bash_cwd("__mu_cwd_display_probe__"));
    }

    #[test]
    fn bash_start_can_reuse_streamed_header_without_duplication() {
        let mut renderer = Renderer::with_format(OutputFormat::Plain);
        let args = json!({
            "title": "List files",
            "risk": "readonly",
            "command": "printf 'a'\npwd",
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
    fn bash_preview_keeps_a_single_middle_line() {
        let snapshot = compute_bash_preview_snapshot("one\ntwo\nthree\nfour\nfive\nsix\n", true);

        assert_eq!(snapshot.head_rendered, "one\ntwo\nthree\n");
        assert_eq!(snapshot.tail_rendered, "four\nfive\nsix\n");
        assert_eq!(snapshot.omitted_lines, 0);
        assert_eq!(snapshot.omitted_bytes, 0);
    }

    #[test]
    fn bash_preview_still_omits_multiple_middle_lines() {
        let snapshot =
            compute_bash_preview_snapshot("one\ntwo\nthree\nfour\nfive\nsix\nseven\n", true);

        assert_eq!(snapshot.head_rendered, "one\ntwo\nthree\n");
        assert_eq!(snapshot.tail_rendered, "six\nseven\n");
        assert_eq!(snapshot.omitted_lines, 2);
        assert_eq!(snapshot.omitted_bytes, "four\n".len() + "five\n".len());
    }

    #[test]
    fn bash_preview_caps_a_single_middle_line_without_omitting_it() {
        let middle = "m".repeat(BASH_TAIL_LINE_CAP_BYTES + 80);
        let raw = format!("one\ntwo\nthree\n{middle}\nfive\nsix\n");

        let snapshot = compute_bash_preview_snapshot(&raw, true);

        assert_eq!(snapshot.omitted_lines, 0);
        assert_eq!(
            snapshot.omitted_bytes,
            middle.len() + 1 - (BASH_TAIL_LINE_CAP_BYTES - ELLIPSIS.len())
        );
        assert!(snapshot.tail_rendered.starts_with(ELLIPSIS));
    }

    #[test]
    fn bash_preview_caps_head_and_tail_output_lines() {
        let head = "h".repeat(BASH_HEAD_LINE_CAP_BYTES + 80);
        let tail_a = "t".repeat(BASH_TAIL_LINE_CAP_BYTES + 80);
        let tail_b = "u".repeat(BASH_TAIL_LINE_CAP_BYTES + 80);
        let raw =
            format!("{head}\nhead two\nhead three\nomitted one\nomitted two\n{tail_a}\n{tail_b}\n");

        let snapshot = compute_bash_preview_snapshot(&raw, true);

        let rendered_head = snapshot.head_rendered.lines().next().unwrap();
        let rendered_tail = snapshot.tail_rendered.lines().next().unwrap();
        assert_eq!(rendered_head.len(), BASH_HEAD_LINE_CAP_BYTES);
        assert!(rendered_head.ends_with(ELLIPSIS));
        assert!(rendered_tail.starts_with(ELLIPSIS));
        assert!(rendered_tail.len() < BASH_TAIL_LINE_CAP_BYTES);
    }

    #[test]
    fn plain_reasoning_commits_summary_after_reasoning_finishes() {
        let raw = capture_plain_reasoning_transcript();
        let normalized = strip_ansi(&raw.replace('\r', ""));

        assert!(normalized.starts_with("[thought "));
        assert!(normalized.contains(", ~2 tokens]\n"));
    }

    #[test]
    fn streamed_trace_commits_even_without_reasoning_text_and_keeps_tokens() {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Plain, false, false, None);

        renderer
            .reasoning_start(ReasoningVisibility::StreamedTrace)
            .unwrap();
        renderer.reasoning_end(None).unwrap();

        let transcript = output.transcript();
        assert!(transcript.starts_with("[thought "), "{transcript:?}");
        assert!(transcript.contains(", ~0 tokens]\n"), "{transcript:?}");
    }

    #[test]
    fn opaque_reasoning_commits_duration_and_conservative_summary_title_without_tokens() {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Plain, false, false, None);

        renderer
            .reasoning_start(ReasoningVisibility::Opaque)
            .unwrap();
        renderer.reasoning_summary_delta("**Inspecting").unwrap();
        renderer
            .reasoning_summary_delta(" renderer state**\n\nDetails")
            .unwrap();
        renderer.reasoning_end(Some((20, 7))).unwrap();

        let transcript = output.transcript();
        assert!(
            transcript.contains("] Inspecting renderer state\n"),
            "{transcript:?}"
        );
        assert!(!transcript.contains("token"), "{transcript:?}");
    }

    #[test]
    fn opaque_reasoning_without_summary_commits_timer_only() {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Plain, false, false, None);

        renderer
            .reasoning_start(ReasoningVisibility::Opaque)
            .unwrap();
        renderer.reasoning_end(Some((20, 7))).unwrap();

        let transcript = output.transcript();
        assert!(transcript.starts_with("[thought "), "{transcript:?}");
        assert!(transcript.ends_with("]\n"), "{transcript:?}");
        assert!(!transcript.contains("token"), "{transcript:?}");
    }

    #[test]
    fn multiple_opaque_reasoning_items_commit_separate_titles() {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Plain, false, false, None);

        for title in ["First pass", "Second pass"] {
            renderer
                .reasoning_start(ReasoningVisibility::Opaque)
                .unwrap();
            renderer
                .reasoning_summary_delta(&format!("**{title}**\n"))
                .unwrap();
            renderer.reasoning_end(None).unwrap();
        }

        let transcript = output.transcript();
        assert_eq!(transcript.matches("[thought ").count(), 2, "{transcript:?}");
        assert!(transcript.contains("] First pass\n"), "{transcript:?}");
        assert!(transcript.contains("] Second pass\n"), "{transcript:?}");
    }

    #[test]
    fn final_output_suppresses_opaque_reasoning_and_summary() {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Final, false, false, None);

        renderer
            .reasoning_start(ReasoningVisibility::Opaque)
            .unwrap();
        renderer
            .reasoning_summary_delta("**Hidden title**\n")
            .unwrap();
        renderer.reasoning_end(None).unwrap();

        assert!(output.transcript().is_empty());
    }

    #[test]
    fn terminal_reasoning_title_updates_the_live_line() {
        let (mut renderer, _output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);

        renderer
            .reasoning_start(ReasoningVisibility::Opaque)
            .unwrap();
        renderer
            .reasoning_summary_delta("## Inspecting renderer state\n")
            .unwrap();

        let live = strip_ansi(&renderer.format_live_line().unwrap());
        assert!(live.ends_with("] Inspecting renderer state"), "{live:?}");
        assert!(!live.contains("token"), "{live:?}");
    }

    #[test]
    fn summary_title_extraction_rejects_prose_and_accepts_only_title_lines() {
        assert_eq!(
            extract_reasoning_summary_title("**Inspecting   renderer**\n\nDetails", false),
            Some("Inspecting renderer".into())
        );
        assert_eq!(
            extract_reasoning_summary_title("\n### Checking tests ###\n", false),
            Some("Checking tests".into())
        );
        assert_eq!(
            extract_reasoning_summary_title("I am inspecting the renderer.\n", false),
            None
        );
        assert_eq!(
            extract_reasoning_summary_title("**Incomplete title**", false),
            None
        );
        assert_eq!(
            extract_reasoning_summary_title("**Complete at end**", true),
            Some("Complete at end".into())
        );
    }

    #[test]
    fn summary_title_is_capped_by_visible_terminal_width() {
        let summary = format!("**{}**\n", "界".repeat(50));
        let title = extract_reasoning_summary_title(&summary, false).unwrap();

        assert!(UnicodeWidthStr::width(title.as_str()) <= REASONING_TITLE_MAX_WIDTH);
        assert!(title.ends_with(ELLIPSIS));
    }

    #[test]
    fn terminal_ignores_empty_assistant_blocks_before_first_visible_block() {
        let (mut renderer, output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);

        renderer.assistant_text("\n\n").unwrap();
        renderer.assistant_end().unwrap();
        renderer
            .reasoning_start(ReasoningVisibility::StreamedTrace)
            .unwrap();
        renderer.reasoning_delta("plan").unwrap();
        renderer.reasoning_end(Some((12, 5))).unwrap();

        let normalized = strip_ansi(&output.transcript().replace('\r', ""));
        assert!(normalized.starts_with("[thought "), "{normalized:?}");
    }

    #[test]
    fn first_visible_renderer_blocks_have_no_leading_separator() {
        let (mut assistant, assistant_output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);
        assistant.assistant_text("Hello.\n").unwrap();
        assistant.assistant_end().unwrap();
        assert!(
            strip_ansi(&assistant_output.transcript()).starts_with("Hello."),
            "{:?}",
            assistant_output.transcript()
        );

        let (mut tool, tool_output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);
        tool.bash_header_start(None).unwrap();
        tool.bash_header_title_start().unwrap();
        tool.bash_header_title_delta("Inspect").unwrap();
        tool.bash_header_title_end().unwrap();
        assert!(
            tool_output
                .transcript()
                .starts_with(&format!("{GRAY}[preparing toolcall]{RESET}")),
            "{:?}",
            tool_output.transcript()
        );
        assert!(
            tool_output
                .transcript()
                .contains(&format!("\r\x1b[2K{BOLD}# Inspect{RESET}\n")),
            "{:?}",
            tool_output.transcript()
        );

        let (mut plain, plain_output) =
            Renderer::with_test_shared_output(OutputFormat::Plain, false, None);
        plain.assistant_text("Hello.\n").unwrap();
        plain.assistant_end().unwrap();
        assert_eq!(plain_output.transcript(), "Hello.\n");
    }

    #[test]
    fn terminal_summary_leaves_a_blank_line_before_the_next_prompt() {
        let raw = capture_renderer_transcript(Duration::from_secs(12), Some("mu> "));
        let normalized = strip_ansi(&raw.replace('\r', ""));

        assert!(raw.contains(&format!(
            "{GRAY}[mu] tokens: 12 in / 5 out  context: 25%  time: 12.0s{RESET}"
        )));

        assert!(normalized.contains(
            "$ printf 'line01\\nline02\\nline03\\n'\n[guardrail: allow] risk=low auth=explicit"
        ));
        assert!(normalized.contains("reason is acceptable\nline01\n"));
        assert!(!normalized.contains("reason is acceptable\n\nline01\n"));
        assert!(
            normalized.contains(
                ", 5 tokens]\n\n[preparing toolcall]# Stream demo\n$ printf 'line01\\nline02\\nline03\\n'"
            ),
            "{normalized:?}"
        );
        assert!(raw.contains(&format!(
            "{GRAY}[preparing toolcall]{RESET}\r\x1b[2K{BOLD}# Stream demo{RESET}\n"
        )));
        assert!(normalized.contains(
            "✓ exit 0 · 250ms\n\nDone.\n\n[mu] tokens: 12 in / 5 out  context: 25%  time: 12.0s\n\nmu> "
        ));
        assert!(
            !normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%  time: 12.0s\n\n\nmu> ")
        );
    }

    #[test]
    fn turn_summary_shows_reported_cache_usage_without_a_total() {
        assert_eq!(
            format_turn_summary(
                1_234,
                567,
                Some(89),
                12_345,
                Some(12.0),
                Duration::from_millis(4200),
            ),
            "[mu] tokens: 1,234 in (+567 cache read, +89 cache write) / 12,345 out  context: 12%  time: 4.2s"
        );
        assert_eq!(
            format_turn_summary(600, 500, None, 456, Some(12.0), Duration::from_millis(932),),
            "[mu] tokens: 600 in (+500 cache read) / 456 out  context: 12%  time: 932ms"
        );
        assert_eq!(
            format_turn_summary(600, 0, None, 456, Some(12.0), Duration::from_millis(1100),),
            "[mu] tokens: 600 in / 456 out  context: 12%  time: 1.1s"
        );
    }

    fn capture_renderer_transcript(
        turn_elapsed: Duration,
        trailing_prompt: Option<&str>,
    ) -> String {
        let (mut renderer, output) =
            Renderer::with_test_shared_output(OutputFormat::Terminal, true, None);

        renderer
            .reasoning_start(ReasoningVisibility::StreamedTrace)
            .unwrap();
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
                    "command": "printf 'line01\\nline02\\nline03\\n'",
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
        renderer.assistant_text("Done.\n").unwrap();
        renderer.assistant_end().unwrap();
        renderer.finish_turn().unwrap();
        renderer
            .turn_summary(12, 0, None, 5, Some(25.0), turn_elapsed)
            .unwrap();
        renderer.turn_done_bell(turn_elapsed).unwrap();
        if let Some(prompt) = trailing_prompt {
            output.write_raw(prompt);
        }
        output.transcript()
    }

    fn capture_plain_reasoning_transcript() -> String {
        let (mut renderer, output, _stderr) =
            Renderer::with_test_output(OutputFormat::Plain, false, false, None);

        renderer
            .reasoning_start(ReasoningVisibility::StreamedTrace)
            .unwrap();
        renderer.reasoning_delta("reason").unwrap();
        renderer.reasoning_end(None).unwrap();

        output.transcript()
    }
}
