use std::io::{self, IsTerminal, Write};
use std::time::{Duration, Instant};

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

use crate::cli::OutputFormat;
use crate::tools::ToolDisplay;

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const ITALIC: &str = "\x1b[3m";
const UNDERLINE: &str = "\x1b[4m";
const STRIKE: &str = "\x1b[9m";
const RED: &str = "\x1b[91m";
const GREEN: &str = "\x1b[92m";
const YELLOW: &str = "\x1b[93m";
const CYAN: &str = "\x1b[96m";
const GRAY: &str = "\x1b[90m";
const BASH_COMMAND_PREVIEW_BYTES: usize = 160;
const BASH_HEAD_LINE_BUDGET: usize = 3;
const BASH_HEAD_BYTE_BUDGET: usize = 1024;
const BASH_HEAD_LINE_CAP_BYTES: usize = 256;
const BASH_TAIL_LINE_RESERVE: usize = 2;
const BASH_TAIL_FALLBACK_BYTES: usize = 512;
const BASH_TAIL_LINE_CAP_BYTES: usize = 256;
const ELLIPSIS: &str = "…";

pub struct Renderer {
    stdout: io::Stdout,
    stderr: io::Stderr,
    stdout_at_line_start: bool,
    styled: bool,
    format: OutputFormat,
    markdown: MarkdownStream,
    live_line: Option<LiveLine>,
    live_line_rendered: bool,
    reasoning: Option<ReasoningState>,
    bash_preview: Option<BashPreviewState>,
}

impl Renderer {
    pub fn new() -> Self {
        Self::with_format(OutputFormat::Terminal)
    }

    pub fn with_format(format: OutputFormat) -> Self {
        let stdout = io::stdout();
        Self {
            styled: format == OutputFormat::Terminal && stdout.is_terminal(),
            stdout,
            stderr: io::stderr(),
            stdout_at_line_start: true,
            format,
            markdown: MarkdownStream::default(),
            live_line: None,
            live_line_rendered: false,
            reasoning: None,
            bash_preview: None,
        }
    }

    pub fn assistant_text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.format == OutputFormat::Json {
            return self.write_json("assistant_delta", serde_json::json!({ "text": text }));
        }
        if !self.styled {
            return self.write_stdout(text);
        }

        let blocks = self.markdown.push(text);
        if blocks.is_empty() {
            return self.render_live_line();
        }

        for block in blocks {
            let rendered = render_markdown(&block);
            self.write_committed(&rendered)?;
        }
        self.render_live_line()
    }

    pub fn assistant_end(&mut self) -> io::Result<()> {
        let Some(block) = self.markdown.finish() else {
            return Ok(());
        };
        let rendered = render_markdown(&block);
        self.write_committed(&rendered)?;
        self.render_live_line()
    }

    pub fn reasoning_start(&mut self) -> io::Result<()> {
        if !self.styled {
            return Ok(());
        }
        self.reasoning = Some(ReasoningState {
            started: Instant::now(),
            reasoning_chars: 0,
            committed: false,
        });
        Ok(())
    }

    pub fn reasoning_delta(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() || !self.styled {
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
        self.live_line = Some(LiveLine::Thinking);
        self.render_live_line()
    }

    pub fn thinking_tick(&mut self) -> io::Result<()> {
        if !matches!(self.live_line, Some(LiveLine::Thinking)) {
            return Ok(());
        }
        self.render_live_line()
    }

    pub fn reasoning_end(&mut self, usage: Option<(u64, u64)>) -> io::Result<()> {
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
        );
        reasoning.committed = true;
        self.live_line = None;
        self.write_committed(&line)
    }

    pub fn tool_call_composition_start(&mut self) -> io::Result<()> {
        if !self.styled {
            return Ok(());
        }
        self.live_line = Some(LiveLine::ToolComposition);
        self.render_live_line()
    }

    pub fn cancel_live_state(&mut self) -> io::Result<()> {
        self.clear_live_line()?;
        self.live_line = None;
        self.reasoning = None;
        self.bash_preview = None;
        Ok(())
    }

    /// Completion-only tools are silent here. Bash is the exception because
    /// its live output needs a visible command header.
    pub fn tool_start(&mut self, name: &str, args: &serde_json::Value) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json(
                "tool_start",
                serde_json::json!({ "tool": name, "args": args }),
            );
        }
        if name != "bash" {
            return Ok(());
        }
        if self.styled {
            self.reasoning_end(None)?;
        }
        self.ensure_line_start()?;
        let title = args
            .get("title")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let script = args
            .get("script")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        let risk = args.get("risk").and_then(|value| value.as_str());
        let line = format_bash_header(title, script, risk, self.styled);
        if self.styled {
            self.bash_preview = Some(BashPreviewState::default());
        }
        self.write_committed(&line)
    }

    pub fn bash_output(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if self.format == OutputFormat::Json {
            return self.write_json(
                "tool_output",
                serde_json::json!({ "text": strip_ansi(text) }),
            );
        }
        let sanitized = strip_ansi(text);
        if !self.styled {
            return self.write_stdout(&sanitized);
        }
        let Some(preview) = self.bash_preview.as_mut() else {
            return self.write_committed(&format!("{GRAY}{sanitized}{RESET}"));
        };
        preview.raw.push_str(&sanitized);
        let snapshot = compute_bash_preview_snapshot(&preview.raw, false);
        if snapshot.head_rendered.len() > preview.committed_head_len {
            let next = snapshot.head_rendered[preview.committed_head_len..].to_string();
            preview.committed_head_len = snapshot.head_rendered.len();
            self.write_committed(&format!("{GRAY}{next}{RESET}"))?;
        }
        self.set_omitted_live_line(snapshot.omitted_lines, snapshot.omitted_bytes)
    }

    pub fn tool_finished(&mut self, display: &ToolDisplay, elapsed: Duration) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json(
                "tool_finish",
                serde_json::json!({
                    "display": tool_display_json(display),
                    "elapsed_ms": elapsed.as_millis()
                }),
            );
        }
        self.finalize_bash_preview()?;
        let text = format_tool(display, elapsed, self.styled);
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_line_start()?;
        self.write_stdout(&terminal_trim_committed_text(&text))
    }

    pub fn tool_failed(&mut self, name: &str, error: &str, elapsed: Duration) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json(
                "tool_error",
                serde_json::json!({
                    "tool": name,
                    "error": error,
                    "elapsed_ms": elapsed.as_millis()
                }),
            );
        }
        self.finalize_bash_preview()?;
        self.ensure_line_start()?;
        let elapsed = format_duration(elapsed);
        let line = if self.styled {
            format!(
                "{RED}✗ {BOLD}{name} failed{RESET}{RED}: {error}{RESET}{DIM} · {elapsed}{RESET}\n"
            )
        } else {
            format!("[error] {name} failed: {error} ({elapsed})\n")
        };
        self.write_stdout(&terminal_trim_committed_text(&line))
    }

    pub fn guardrail_verdict(
        &mut self,
        allowed: bool,
        risk_level: &str,
        user_auth_level: &str,
        reason: &str,
        script: &str,
    ) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json(
                "guardrail",
                serde_json::json!({
                    "allowed": allowed,
                    "risk_level": risk_level,
                    "user_auth_level": user_auth_level,
                    "reason": reason,
                    "script": script
                }),
            );
        }
        self.ensure_line_start()?;
        let verdict = if allowed { "allow" } else { "deny" };
        let script_preview = script.lines().next().unwrap_or(script);
        let script_preview = if script_preview.len() > 120 {
            format!("{}…", &script_preview[..117])
        } else {
            script_preview.to_string()
        };
        let line = if self.styled {
            let (color, verdict) = if allowed {
                (GREEN, "allow")
            } else {
                (RED, "deny")
            };
            format!(
                "{color}[guardrail: {verdict}]{RESET} {DIM}risk={risk_level} auth={user_auth_level} — {reason}{RESET}\n{DIM}  {script_preview}{RESET}\n"
            )
        } else {
            format!(
                "[guardrail: {verdict}] risk={risk_level} auth={user_auth_level} — {reason}\n  {script_preview}\n"
            )
        };
        self.write_stdout(&terminal_trim_committed_text(&line))
    }

    pub fn error(&mut self, msg: &str) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json("error", serde_json::json!({ "message": msg }));
        }
        writeln!(self.stderr, "error: {msg}")?;
        self.stderr.flush()
    }

    pub fn notice(&mut self, msg: &str) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json("notice", serde_json::json!({ "message": msg }));
        }
        self.ensure_line_start()?;
        self.write_stdout(&terminal_trim_committed_text(&format!("{msg}\n")))
    }

    /// Ensure stdout ends on a fresh line so the next shell prompt does not
    /// glue onto the final line of assistant output.
    pub fn finish_turn(&mut self) -> io::Result<()> {
        self.assistant_end()?;
        if self.format == OutputFormat::Json {
            return Ok(());
        }
        self.ensure_line_start()
    }

    pub fn turn_summary(
        &mut self,
        prompt_tokens: u64,
        completion_tokens: u64,
        context_pct: Option<f64>,
        cost: Option<f64>,
    ) -> io::Result<()> {
        if self.format == OutputFormat::Json {
            return self.write_json(
                "turn_summary",
                serde_json::json!({
                    "prompt_tokens": prompt_tokens,
                    "completion_tokens": completion_tokens,
                    "context_pct": context_pct,
                    "cost": cost
                }),
            );
        }
        if !self.stderr.is_terminal() {
            return Ok(());
        }
        writeln!(
            self.stderr,
            "{}",
            format_turn_summary(prompt_tokens, completion_tokens, context_pct, cost)
        )?;
        self.stderr.flush()
    }

    fn ensure_line_start(&mut self) -> io::Result<()> {
        self.clear_live_line()?;
        if self.stdout_at_line_start {
            return Ok(());
        }
        self.write_stdout("\n")
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
            Some(LiveLine::ToolComposition) => Some(format!("{CYAN}${RESET}")),
            Some(LiveLine::BashOmitted {
                omitted_lines,
                omitted_bytes,
            }) => Some(format_omitted_line(omitted_lines, omitted_bytes)),
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
            self.write_stdout(&format!(
                "{GRAY}{}{RESET}",
                terminal_trim_committed_text(&next)
            ))?;
        }
        if snapshot.omitted_lines > 0 || snapshot.omitted_bytes > 0 {
            self.write_stdout(&terminal_trim_committed_text(&format!(
                "{}\n",
                format_omitted_line(snapshot.omitted_lines, snapshot.omitted_bytes)
            )))?;
        }
        if !snapshot.tail_rendered.is_empty() {
            self.write_stdout(&format!(
                "{GRAY}{}{RESET}",
                terminal_trim_committed_text(&snapshot.tail_rendered)
            ))?;
        }
        Ok(())
    }

    fn write_committed(&mut self, text: &str) -> io::Result<()> {
        if !self.styled {
            return self.write_stdout(text);
        }
        self.clear_live_line()?;
        self.write_stdout(&terminal_trim_committed_text(text))?;
        self.render_live_line()
    }

    fn write_stdout(&mut self, text: &str) -> io::Result<()> {
        self.stdout.write_all(text.as_bytes())?;
        self.stdout_at_line_start = text.ends_with('\n');
        self.stdout.flush()
    }

    fn write_json(&mut self, event: &str, payload: serde_json::Value) -> io::Result<()> {
        let line = serde_json::json!({
            "event": event,
            "payload": payload,
        });
        writeln!(self.stdout, "{line}")?;
        self.stdout_at_line_start = true;
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
    ToolComposition,
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
    Cyan,
}

impl MdStyle {
    fn ansi(self) -> &'static str {
        match self {
            Self::Bold => BOLD,
            Self::Dim => DIM,
            Self::Italic => ITALIC,
            Self::Underline => UNDERLINE,
            Self::Strike => STRIKE,
            Self::Cyan => CYAN,
        }
    }
}

#[derive(Clone, Copy)]
struct ListState {
    next: Option<u64>,
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
    let mut table_cell = 0usize;

    let push_style = |out: &mut String, styles: &mut Vec<MdStyle>, style| {
        styles.push(style);
        out.push_str(style.ansi());
    };
    let pop_style = |out: &mut String, styles: &mut Vec<MdStyle>| {
        styles.pop();
        out.push_str(RESET);
        for style in styles.iter() {
            out.push_str(style.ansi());
        }
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    push_style(&mut out, &mut styles, MdStyle::Bold);
                    push_style(&mut out, &mut styles, MdStyle::Cyan);
                }
                Tag::BlockQuote(_) => {
                    out.push_str("│ ");
                    push_style(&mut out, &mut styles, MdStyle::Dim);
                }
                Tag::CodeBlock(_) => {
                    if !out.ends_with('\n') && !out.is_empty() {
                        out.push('\n');
                    }
                    push_style(&mut out, &mut styles, MdStyle::Cyan);
                }
                Tag::List(start) => lists.push(ListState { next: start }),
                Tag::Item => {
                    in_item += 1;
                    out.push_str(&"  ".repeat(lists.len().saturating_sub(1)));
                    let marker = lists.last_mut().and_then(|list| {
                        let current = list.next?;
                        list.next = Some(current + 1);
                        Some(format!("{current}. "))
                    });
                    out.push_str(marker.as_deref().unwrap_or("• "));
                }
                Tag::Table(_) => {}
                Tag::TableHead | Tag::TableRow => {
                    table_cell = 0;
                    out.push_str("| ");
                }
                Tag::TableCell => {
                    if table_cell > 0 {
                        out.push_str(" | ");
                    }
                    table_cell += 1;
                }
                Tag::Emphasis => push_style(&mut out, &mut styles, MdStyle::Italic),
                Tag::Strong => push_style(&mut out, &mut styles, MdStyle::Bold),
                Tag::Strikethrough => push_style(&mut out, &mut styles, MdStyle::Strike),
                Tag::Link { dest_url, .. } => {
                    links.push(dest_url.to_string());
                    push_style(&mut out, &mut styles, MdStyle::Underline);
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    if in_item == 0 {
                        out.push_str("\n\n");
                    }
                }
                TagEnd::Heading(_) => {
                    pop_style(&mut out, &mut styles);
                    pop_style(&mut out, &mut styles);
                    out.push_str("\n\n");
                }
                TagEnd::BlockQuote(_) | TagEnd::CodeBlock => {
                    pop_style(&mut out, &mut styles);
                    out.push_str("\n\n");
                }
                TagEnd::List(_) => {
                    lists.pop();
                    if lists.is_empty() {
                        out.push('\n');
                    }
                }
                TagEnd::Item => {
                    in_item = in_item.saturating_sub(1);
                    out.push('\n');
                }
                TagEnd::TableHead | TagEnd::TableRow => out.push_str(" |\n"),
                TagEnd::Table => out.push('\n'),
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    pop_style(&mut out, &mut styles)
                }
                TagEnd::Link => {
                    pop_style(&mut out, &mut styles);
                    if let Some(url) = links.pop() {
                        out.push_str(DIM);
                        out.push_str(" (");
                        out.push_str(&url);
                        out.push(')');
                        out.push_str(RESET);
                        for style in &styles {
                            out.push_str(style.ansi());
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) => out.push_str(&text),
            Event::Code(code) => {
                out.push_str(CYAN);
                out.push_str(&code);
                out.push_str(RESET);
                for style in &styles {
                    out.push_str(style.ansi());
                }
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            Event::Rule => out.push_str("────────────────────────────────────────\n\n"),
            Event::TaskListMarker(done) => out.push_str(if done { "[✓] " } else { "[ ] " }),
            Event::Html(html) | Event::InlineHtml(html) => out.push_str(&html),
            Event::FootnoteReference(name) => {
                out.push('[');
                out.push_str(&name);
                out.push(']');
            }
            _ => {}
        }
    }

    if !styles.is_empty() || !out.ends_with(RESET) {
        out.push_str(RESET);
    }
    out
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
                format!("[bash] exit {exit_code} ({elapsed})\n")
            }
        }
    }
}

fn tool_display_json(display: &ToolDisplay) -> serde_json::Value {
    match display {
        ToolDisplay::None => serde_json::json!({"kind": "none"}),
        ToolDisplay::Bash { exit_code } => {
            serde_json::json!({"kind": "bash", "exit_code": exit_code})
        }
    }
}

fn format_bash_header(title: &str, script: &str, risk: Option<&str>, styled: bool) -> String {
    let command = preview_first_line(script, BASH_COMMAND_PREVIEW_BYTES);
    if !styled {
        return match risk {
            Some(risk) => format!("$ {} {command}\n", format_risk_label(risk, false)),
            None => format!("$ {command}\n"),
        };
    }

    let mut out = String::new();
    if !title.is_empty() {
        out.push_str(BOLD);
        out.push_str(title);
        out.push_str(RESET);
        out.push('\n');
    }
    out.push_str(bash_risk_color(risk));
    out.push_str("$ ");
    out.push_str(&command);
    out.push('\n');
    out.push_str(RESET);
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
        "{GRAY}thinking {}  out ~{} tok{RESET}",
        format_duration(elapsed),
        output_tokens
    )
}

fn format_thought_line(
    elapsed: Duration,
    reasoning_chars: usize,
    usage: Option<(u64, u64)>,
) -> String {
    let elapsed = format_duration(elapsed);
    match usage {
        Some((prompt_tokens, completion_tokens)) => format!(
            "{GRAY}thought {elapsed}  in {prompt_tokens} tok  out {completion_tokens} tok{RESET}\n"
        ),
        None => format!(
            "{GRAY}thought {elapsed}  out ~{} tok{RESET}\n",
            approx_tokens_from_chars(reasoning_chars)
        ),
    }
}

fn format_omitted_line(omitted_lines: usize, omitted_bytes: usize) -> String {
    format!("{GRAY}[… omitted {omitted_lines} lines, {omitted_bytes} bytes]{RESET}")
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

    let trimmed_trailing = if finalizing {
        trim_final_tail_fragment(&trailing)
    } else {
        trailing.clone()
    };
    let mut head_fragment_kept = 0usize;
    if finalizing && head_count == 0 && !trimmed_trailing.is_empty() {
        let preview = truncate_prefix(&trimmed_trailing, BASH_HEAD_LINE_CAP_BYTES, false);
        head_fragment_kept = preview.raw_kept_bytes;
        head_rendered.push_str(&preview.rendered);
    }
    let fallback_reserved = if !trimmed_trailing.is_empty() {
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

fn trim_final_tail_fragment(fragment: &str) -> String {
    fragment.trim_end().to_string()
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

fn format_turn_summary(
    prompt_tokens: u64,
    completion_tokens: u64,
    context_pct: Option<f64>,
    cost: Option<f64>,
) -> String {
    let ctx = context_pct
        .map(|p| format!("{p:.0}%"))
        .unwrap_or_else(|| "?".into());
    let mut summary =
        format!("[mu] tokens: {prompt_tokens} in / {completion_tokens} out  context: {ctx}");
    if let Some(cost) = cost {
        summary.push_str(&format!("  cost: ${cost:.4}"));
    }
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::fd::RawFd;

    #[test]
    fn markdown_stream_holds_incomplete_block() {
        let mut stream = MarkdownStream::default();
        assert!(stream.push("Hello **wor").is_empty());
        let blocks = stream.push("ld**\n\nNext");
        assert_eq!(blocks, vec!["Hello **world**\n\n"]);
        assert_eq!(stream.finish().as_deref(), Some("Next"));
    }

    #[test]
    fn markdown_stream_holds_open_fence() {
        let mut stream = MarkdownStream::default();
        assert!(stream.push("```rust\nfn main() {}\n").is_empty());
        assert_eq!(stream.push("```\n"), vec!["```rust\nfn main() {}\n```\n"]);
    }

    #[test]
    fn markdown_renderer_styles_common_constructs() {
        let rendered =
            render_markdown("# Title\n\nA **bold** and `code` [link](https://example.com).\n");
        assert!(rendered.contains(BOLD));
        assert!(rendered.contains(CYAN));
        assert!(rendered.contains("https://example.com"));
        assert!(rendered.ends_with(RESET));
    }

    #[test]
    fn markdown_renderer_handles_lists_and_tables() {
        let rendered =
            render_markdown("- one\n  - two\n\n| Name | Value |\n| --- | --- |\n| a | b |\n");
        assert!(rendered.contains("• one"));
        assert!(rendered.contains("• two"));
        assert!(rendered.contains("| Name | Value |"));
        assert!(rendered.contains("| a | b |"));
    }

    #[test]
    fn strips_terminal_escape_sequences_from_bash() {
        assert_eq!(strip_ansi("a\x1b[31mred\x1b[0m b"), "ared b");
        assert_eq!(
            strip_ansi("before\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07after"),
            "beforelinkafter"
        );
    }

    #[test]
    fn non_tty_tool_format_is_plain_and_compact() {
        let line = format_tool(
            &ToolDisplay::Bash { exit_code: 7 },
            Duration::from_millis(250),
            false,
        );
        assert_eq!(line, "[bash] exit 7 (250ms)\n");
        assert!(!line.contains('\x1b'));
    }

    #[test]
    fn tty_bash_format_contains_exit_status_and_color() {
        let line = format_tool(
            &ToolDisplay::Bash { exit_code: 0 },
            Duration::from_secs(1),
            true,
        );
        assert!(line.contains("exit 0"));
        assert!(line.contains(GREEN));
    }

    #[test]
    fn thinking_live_line_is_gray_and_uses_approx_output_tokens() {
        let line = format_thinking_live(Duration::from_millis(1250), approx_tokens_from_chars(17));
        assert_eq!(line, "\x1b[90mthinking 1.2s  out ~5 tok\x1b[0m");
    }

    #[test]
    fn bash_header_renders_title_and_risk_colored_script_without_risk_label() {
        let header = format_bash_header("List files", "printf 'a'\npwd", Some("readonly"), true);
        assert!(header.starts_with(&format!("{BOLD}List files{RESET}\n")));
        assert!(header.contains(&format!("{CYAN}$ printf 'a'…\n{RESET}")));
        assert!(!header.contains("  pwd\n"));
        assert!(!header.contains("[readonly]"));
    }

    #[test]
    fn short_reasoning_trace_still_formats_a_thought_line() {
        let line = format_thought_line(Duration::from_millis(300), 3, None);
        assert!(line.contains("thought 300ms"));
        assert!(line.contains("out ~1 tok"));
    }

    #[test]
    fn short_bash_output_has_no_omitted_counter() {
        let snapshot = compute_bash_preview_snapshot("one\n two \nthree\n", true);
        assert_eq!(snapshot.head_rendered, "one\n two\nthree\n");
        assert_eq!(snapshot.tail_rendered, "");
        assert_eq!(snapshot.omitted_lines, 0);
        assert_eq!(snapshot.omitted_bytes, 0);
    }

    #[test]
    fn long_bash_output_preserves_head_and_tail_without_duplication() {
        let text = (1..=40)
            .map(|idx| format!("line{idx:02}\n"))
            .collect::<String>();
        let streaming = compute_bash_preview_snapshot(&text, false);
        let final_snapshot = compute_bash_preview_snapshot(&text, true);

        assert!(streaming.head_rendered.contains("line01\n"));
        assert!(streaming.head_rendered.contains("line03\n"));
        assert!(!streaming.head_rendered.contains("line04\n"));
        assert_eq!(streaming.omitted_lines, 37);
        assert!(streaming.omitted_bytes > 0);

        assert_eq!(final_snapshot.omitted_lines, 35);
        assert!(final_snapshot.tail_rendered.contains("line39\n"));
        assert!(final_snapshot.tail_rendered.contains("line40\n"));
        assert!(!final_snapshot.tail_rendered.contains("line03\n"));
    }

    #[test]
    fn huge_single_line_output_uses_byte_tail_fallback() {
        let text = "x".repeat(900);
        let snapshot = compute_bash_preview_snapshot(&text, true);
        assert_eq!(snapshot.head_rendered.len(), BASH_HEAD_LINE_CAP_BYTES);
        assert_eq!(snapshot.omitted_lines, 0);
        assert_eq!(snapshot.omitted_bytes, 394);
        assert_eq!(snapshot.tail_rendered.len(), BASH_TAIL_LINE_CAP_BYTES);
        assert!(snapshot.head_rendered.ends_with(ELLIPSIS));
        assert!(snapshot.tail_rendered.starts_with('…'));
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
    fn pty_transcript_shows_live_placeholder_and_omission_updates() {
        let raw = capture_renderer_pty_transcript();
        let normalized = strip_ansi(&raw.replace('\r', ""))
            .lines()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(raw.contains("\x1b[96m$\x1b[0m"));
        assert!(raw.matches("[… omitted").count() > 1);
        assert!(raw.contains("\x1b[90mline01"));
        assert!(normalized.contains("thought"));
        assert!(normalized.contains("Stream demo"));
        assert!(normalized.contains("$ i=1…"));
        assert!(!normalized.contains("  while [ $i -le 40 ]; do"));
        assert!(normalized.contains("[… omitted 35 lines, 245 bytes]"));
        assert!(normalized.contains("line03"));
        assert!(!normalized.contains("line04"));
        assert!(normalized.contains("line39"));
        assert!(normalized.contains("line40"));
        assert!(!normalized.contains("\n$\n"));
        assert!(!normalized.ends_with("\n\n"));
    }

    #[test]
    fn omits_cost_when_not_available() {
        let summary = format_turn_summary(12, 5, Some(25.0), None);
        assert_eq!(summary, "[mu] tokens: 12 in / 5 out  context: 25%");
    }

    #[test]
    fn includes_cost_when_available() {
        let summary = format_turn_summary(12, 5, None, Some(0.0034));
        assert_eq!(
            summary,
            "[mu] tokens: 12 in / 5 out  context: ?  cost: $0.0034"
        );
    }

    fn capture_renderer_pty_transcript() -> String {
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

                let mut renderer = Renderer::new();
                renderer.reasoning_start().unwrap();
                renderer.reasoning_delta("plan").unwrap();
                std::thread::sleep(Duration::from_millis(40));
                renderer.reasoning_end(Some((12, 5))).unwrap();

                renderer.tool_call_composition_start().unwrap();
                std::thread::sleep(Duration::from_millis(20));
                renderer
                    .tool_start(
                        "bash",
                        &json!({
                            "title": "Stream demo",
                            "risk": "readonly",
                            "script": "i=1\nwhile [ $i -le 40 ]; do\n  printf 'line%02d\\n' \"$i\"\n  i=$((i+1))\n  sleep 0.02\ndone",
                        }),
                    )
                    .unwrap();
                for idx in 1..=40 {
                    renderer.bash_output(&format!("line{idx:02}\n")).unwrap();
                    std::thread::sleep(Duration::from_millis(5));
                }
                renderer
                    .tool_finished(
                        &ToolDisplay::Bash { exit_code: 0 },
                        Duration::from_millis(250),
                    )
                    .unwrap();
                renderer.finish_turn().unwrap();
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
}

struct LinePreview {
    rendered: String,
    raw_kept_bytes: usize,
}
