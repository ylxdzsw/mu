use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};

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

pub struct Renderer {
    stdout: io::Stdout,
    stderr: io::Stderr,
    stdout_at_line_start: bool,
    styled: bool,
    markdown: MarkdownStream,
}

impl Renderer {
    pub fn new() -> Self {
        let stdout = io::stdout();
        Self {
            styled: stdout.is_terminal(),
            stdout,
            stderr: io::stderr(),
            stdout_at_line_start: true,
            markdown: MarkdownStream::default(),
        }
    }

    pub fn assistant_text(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        if !self.styled {
            return self.write_stdout(text);
        }

        for block in self.markdown.push(text) {
            let rendered = render_markdown(&block);
            self.write_stdout(&rendered)?;
        }
        Ok(())
    }

    pub fn assistant_end(&mut self) -> io::Result<()> {
        let Some(block) = self.markdown.finish() else {
            return Ok(());
        };
        let rendered = render_markdown(&block);
        self.write_stdout(&rendered)
    }

    /// Completion-only tools are silent here. Bash is the exception because
    /// its live output needs a visible command header.
    pub fn tool_start(&mut self, name: &str, args: &serde_json::Value) -> io::Result<()> {
        if name != "bash" {
            return Ok(());
        }
        self.ensure_line_start()?;
        let command = args
            .get("title")
            .and_then(|value| value.as_str())
            .or_else(|| args.get("script").and_then(|value| value.as_str()))
            .unwrap_or_default();
        let risk = args
            .get("risk")
            .and_then(|value| value.as_str())
            .map(|risk| format_risk_label(risk, self.styled));
        let line = if self.styled {
            match risk {
                Some(risk) => format!("{CYAN}${RESET} {risk} {BOLD}{command}{RESET}\n"),
                None => format!("{CYAN}${RESET} {BOLD}{command}{RESET}\n"),
            }
        } else {
            match risk {
                Some(risk) => format!("$ {risk} {command}\n"),
                None => format!("$ {command}\n"),
            }
        };
        self.write_stdout(&line)
    }

    pub fn bash_output(&mut self, text: &str) -> io::Result<()> {
        if text.is_empty() {
            return Ok(());
        }
        self.write_stdout(&strip_ansi(text))
    }

    pub fn tool_finished(&mut self, display: &ToolDisplay, elapsed: Duration) -> io::Result<()> {
        let text = format_tool(display, elapsed, self.styled);
        if text.is_empty() {
            return Ok(());
        }
        self.ensure_line_start()?;
        self.write_stdout(&text)
    }

    pub fn tool_failed(&mut self, name: &str, error: &str, elapsed: Duration) -> io::Result<()> {
        self.ensure_line_start()?;
        let elapsed = format_duration(elapsed);
        let line = if self.styled {
            format!(
                "{RED}✗ {BOLD}{name} failed{RESET}{RED}: {error}{RESET}{DIM} · {elapsed}{RESET}\n"
            )
        } else {
            format!("[error] {name} failed: {error} ({elapsed})\n")
        };
        self.write_stdout(&line)
    }

    pub fn error(&mut self, msg: &str) -> io::Result<()> {
        writeln!(self.stderr, "error: {msg}")?;
        self.stderr.flush()
    }

    pub fn notice(&mut self, msg: &str) -> io::Result<()> {
        self.ensure_line_start()?;
        self.write_stdout(&format!("{msg}\n"))
    }

    /// Ensure stdout ends on a fresh line so the next shell prompt does not
    /// glue onto the final line of assistant output.
    pub fn finish_turn(&mut self) -> io::Result<()> {
        self.assistant_end()?;
        self.ensure_line_start()
    }

    pub fn turn_summary(
        &mut self,
        prompt_tokens: u64,
        completion_tokens: u64,
        context_pct: Option<f64>,
        cost: Option<f64>,
    ) -> io::Result<()> {
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
        if self.stdout_at_line_start {
            return Ok(());
        }
        self.write_stdout("\n")
    }

    fn write_stdout(&mut self, text: &str) -> io::Result<()> {
        self.stdout.write_all(text.as_bytes())?;
        self.stdout_at_line_start = text.ends_with('\n');
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

fn format_duration(duration: Duration) -> String {
    if duration.as_secs() == 0 {
        return format!("{}ms", duration.as_millis());
    }
    format!("{:.1}s", duration.as_secs_f64())
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
}
