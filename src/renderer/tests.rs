use std::os::fd::RawFd;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde_json::json;

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
        render_markdown("# Title\n\nA *soft* **bold** and `code` [link](https://example.com).\n");
    assert!(rendered.contains(BOLD));
    assert!(rendered.contains(ITALIC));
    assert!(rendered.contains(DIM));
    assert!(rendered.contains(CYAN));
    assert!(rendered.contains(GREEN));
    assert!(rendered.contains("https://example.com"));
    assert!(rendered.contains(&hyperlink_text("https://example.com", "link")));
    assert!(rendered.ends_with(RESET));
}

#[test]
fn markdown_renderer_handles_lists_and_tables() {
    let rendered =
        render_markdown("- one\n  - two\n\n| Name | Value |\n| --- | --- |\n| a | b |\n");
    assert!(rendered.contains("• one"));
    assert!(rendered.contains("• two"));
    assert!(rendered.contains("| Name | Value |"));
    assert!(rendered.contains("---"));
    assert!(rendered.lines().any(|line| line.starts_with('|')
        && line.ends_with('|')
        && line.contains('a')
        && line.contains('b')));
}

#[test]
fn markdown_renderer_uses_distinct_heading_levels_and_code_styles() {
    let rendered = render_markdown("# Top\n## Mid\n### Low\n\n`inline`\n\n```rust\nblock\n```\n");
    assert!(rendered.contains(&format!("{BOLD}{UNDERLINE}{CYAN}Top")));
    assert!(rendered.contains(&format!("{BOLD}{BLUE}Mid")));
    assert!(rendered.contains(&format!("{BOLD}{GREEN}Low")));
    assert!(rendered.contains(&format!("{GREEN}inline{RESET}")));
    assert!(rendered.contains(&format!("{GREEN}block")));
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
    assert_eq!(line, "✗ exit 7 · 250ms\n");
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
    assert_eq!(line, "\x1b[90m[thought 1.2s, ~5 tokens]\x1b[0m");
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
fn short_reasoning_trace_still_formats_a_thought_line() {
    let line = format_thought_line(Duration::from_millis(300), 3, None, true);
    assert_eq!(line, "\x1b[90m[thought 300ms, ~1 tokens]\x1b[0m\n");
}

#[test]
fn plain_bash_header_keeps_title_and_explicit_risk_prefix() {
    let header = format_bash_header("List files", "printf 'a'\npwd", Some("readonly"), false);
    assert_eq!(header, "# List files\n$ [readonly] printf 'a'…\n");
}

#[test]
fn plain_reasoning_trace_formats_without_ansi() {
    let line = format_thought_line(Duration::from_millis(300), 3, None, false);
    assert_eq!(line, "[thought 300ms, ~1 tokens]\n");
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
fn boundary_blank_lines_are_trimmed_from_bash_output() {
    let snapshot = compute_bash_preview_snapshot("\n\t\none\n\n two\n \n", true);
    assert_eq!(snapshot.head_rendered, "one\n\n two\n");
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
    let raw = capture_renderer_pty_transcript(false, Duration::from_secs(12), None);
    let normalized = visible_terminal_text(&raw);

    assert!(raw.contains(&format!("{DIM}${RESET}")));
    assert!(raw.matches("[… omitted").count() > 1);
    assert!(raw.contains("\x1b[90mline01"));
    assert!(normalized.contains("[thought"));
    assert!(normalized.contains("Stream demo"));
    assert!(normalized.contains("# Stream demo"));
    assert!(normalized.contains("$ i=1…"));
    assert!(!normalized.contains("  while [ $i -le 40 ]; do"));
    assert!(normalized.contains("[… omitted 35 lines, 245 bytes]"));
    assert!(
        normalized.contains("line03\n[… omitted 35 lines, 245 bytes]"),
        "{normalized:?}"
    );
    assert!(
        !normalized.contains("line03\n\n[… omitted 35 lines, 245 bytes]"),
        "{normalized:?}"
    );
    assert!(normalized.contains("line03"));
    assert!(!normalized.contains("line04"));
    assert!(normalized.contains("line39"));
    assert!(normalized.contains("line40"));
    assert!(normalized.contains("\n\n# Stream demo"));
    assert!(!normalized.contains("\n\n\n# Stream demo"));
    assert!(!normalized.ends_with("\n\n"));
    assert!(normalized.starts_with("[thought"));
    assert!(normalized.contains("5 tokens]"));
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

#[test]
fn terminal_bell_is_emitted_after_summary_when_enabled() {
    let raw = capture_renderer_pty_transcript(true, Duration::from_secs(12), None);

    assert!(raw.contains("[mu] tokens: 12 in / 5 out  context: 25%"));
    assert!(raw.ends_with("\r\n\r\n\x07"));
}

#[test]
fn terminal_bell_is_suppressed_below_threshold() {
    let raw = capture_renderer_pty_transcript(true, Duration::from_secs(2), None);

    assert!(raw.contains("[mu] tokens: 12 in / 5 out  context: 25%"));
    assert!(!raw.ends_with("\x07"));
}

#[test]
fn pty_transcript_keeps_one_blank_line_between_thought_and_assistant() {
    let raw = capture_assistant_after_thought_transcript();
    let normalized = strip_ansi(&raw);

    assert!(normalized.contains("[thought"));
    assert!(normalized.contains("5 tokens]"));
    assert!(normalized.contains("\r\n\r\nHello world"));
}

#[test]
fn terminal_summary_leaves_a_blank_line_before_the_next_prompt() {
    let raw = capture_renderer_pty_transcript(false, Duration::from_secs(12), Some("mu> "));
    let normalized = strip_ansi(&raw.replace('\r', ""));

    assert!(normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%\n\nmu> "));
    assert!(!normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%\n\n\nmu> "));
}

#[test]
fn pty_transcript_keeps_one_blank_line_before_followup_thinking() {
    let raw = capture_tool_then_followup_thinking_transcript();
    let normalized = visible_terminal_text(&raw);

    assert!(normalized.contains("exit 0"), "{normalized:?}");
    assert!(
        normalized.contains("✓ exit 0 · 250ms\n\n[thought"),
        "{normalized:?}"
    );
    assert!(
        !normalized.contains("exit 0\n\n\n[thought"),
        "{normalized:?}"
    );
}

#[test]
fn plain_transcript_aligns_tool_blocks_without_live_updates_or_bell() {
    let raw = capture_plain_renderer_transcript(true, Duration::from_secs(12), Some("mu> "));
    let normalized = raw.replace('\r', "");

    assert!(!raw.contains('\x1b'));
    assert_eq!(raw.matches("[thought").count(), 1);
    assert_eq!(raw.matches("[… omitted").count(), 1);
    assert!(
        normalized.contains("# Stream demo\n$ [readonly] i=1…\nline01\nline02\nline03\n"),
        "{normalized:?}"
    );
    assert!(
        normalized.contains("[… omitted 35 lines, 245 bytes]\nline39\nline40\n✓ exit 0 · 250ms\n"),
        "{normalized:?}"
    );
    assert!(!normalized.contains("line04"));
    assert!(normalized.contains("[mu] tokens: 12 in / 5 out  context: 25%\n\nmu> "));
    assert!(!raw.ends_with('\x07'));
}

#[test]
fn plain_transcript_keeps_raw_markdown_and_spacing_after_thought() {
    let raw = capture_plain_markdown_after_thought_transcript();
    let normalized = raw.replace('\r', "");

    assert!(!raw.contains('\x1b'));
    assert!(
        normalized.starts_with("[thought ") && normalized.contains(", 5 tokens]\n\n"),
        "{normalized:?}"
    );
    assert!(
        normalized.ends_with("# Heading\n\n- item\n"),
        "{normalized:?}"
    );
}

fn capture_renderer_pty_transcript(
    bell_enabled: bool,
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

            let mut renderer = Renderer::with_terminal_bell(
                OutputFormat::Terminal,
                bell_enabled.then_some(Duration::from_secs(10)),
            );
            renderer.reasoning_start().unwrap();
            renderer.reasoning_delta("plan").unwrap();
            std::thread::sleep(Duration::from_millis(40));
            renderer.reasoning_end(Some((12, 5))).unwrap();

            renderer.tool_call_composition_start().unwrap();
            std::thread::sleep(Duration::from_millis(20));
            renderer
                .tool_start(
                    None,
                    "bash",
                    &json!({
                        "title": "Stream demo",
                        "risk": "readonly",
                        "script": "i=1\nwhile [ $i -le 40 ]; do\n  printf 'line%02d\\n' \"$i\"\n  i=$((i+1))\n  sleep 0.02\ndone",
                    }),
                )
                .unwrap();
            for idx in 1..=40 {
                renderer
                    .bash_output(None, "bash", &format!("line{idx:02}\n"))
                    .unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            renderer
                .tool_finished(
                    None,
                    "bash",
                    &ToolDisplay::Bash { exit_code: 0 },
                    Duration::from_millis(250),
                )
                .unwrap();
            renderer.finish_turn().unwrap();
            renderer.turn_summary(12, 5, Some(25.0), None).unwrap();
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

fn capture_assistant_after_thought_transcript() -> String {
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
            renderer.assistant_text("Hello world\n").unwrap();
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

fn capture_tool_then_followup_thinking_transcript() -> String {
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
            renderer.tool_call_composition_start().unwrap();
            renderer
                .tool_start(
                    None,
                    "bash",
                    &json!({
                        "title": "Stream demo",
                        "risk": "readonly",
                        "script": "i=1\nwhile [ $i -le 40 ]; do\n  printf 'line%02d\\n' \"$i\"\n  i=$((i+1))\n  sleep 0.02\ndone",
                    }),
                )
                .unwrap();
            for idx in 1..=40 {
                renderer
                    .bash_output(None, "bash", &format!("line{idx:02}\n"))
                    .unwrap();
            }
            renderer
                .tool_finished(
                    None,
                    "bash",
                    &ToolDisplay::Bash { exit_code: 0 },
                    Duration::from_millis(250),
                )
                .unwrap();
            renderer.reasoning_start().unwrap();
            renderer.reasoning_delta("followup").unwrap();
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

fn capture_plain_renderer_transcript(
    bell_enabled: bool,
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

            let mut renderer = Renderer::with_terminal_bell(
                OutputFormat::Plain,
                bell_enabled.then_some(Duration::from_secs(10)),
            );
            renderer.reasoning_start().unwrap();
            renderer.reasoning_delta("plan").unwrap();
            std::thread::sleep(Duration::from_millis(40));
            renderer.reasoning_end(Some((12, 5))).unwrap();

            renderer.tool_call_composition_start().unwrap();
            renderer
                .tool_start(
                    None,
                    "bash",
                    &json!({
                        "title": "Stream demo",
                        "risk": "readonly",
                        "script": "i=1\nwhile [ $i -le 40 ]; do\n  printf 'line%02d\\n' \"$i\"\n  i=$((i+1))\n  sleep 0.02\ndone",
                    }),
                )
                .unwrap();
            for idx in 1..=40 {
                renderer
                    .bash_output(None, "bash", &format!("line{idx:02}\n"))
                    .unwrap();
            }
            renderer
                .tool_finished(
                    None,
                    "bash",
                    &ToolDisplay::Bash { exit_code: 0 },
                    Duration::from_millis(250),
                )
                .unwrap();
            renderer.finish_turn().unwrap();
            renderer.turn_summary(12, 5, Some(25.0), None).unwrap();
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

fn capture_plain_markdown_after_thought_transcript() -> String {
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

            let mut renderer = Renderer::with_format(OutputFormat::Plain);
            renderer.reasoning_start().unwrap();
            renderer.reasoning_delta("plan").unwrap();
            std::thread::sleep(Duration::from_millis(40));
            renderer.reasoning_end(Some((12, 5))).unwrap();
            renderer.assistant_text("# Heading\n\n- item\n").unwrap();
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

fn visible_terminal_text(raw: &str) -> String {
    let mut lines: Vec<Vec<char>> = vec![Vec::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let chars: Vec<char> = raw.chars().collect();
    let mut idx = 0usize;

    while idx < chars.len() {
        match chars[idx] {
            '\x1b' => {
                idx += 1;
                match chars.get(idx) {
                    Some('[') => {
                        idx += 1;
                        let mut seq = String::new();
                        while idx < chars.len() {
                            let ch = chars[idx];
                            seq.push(ch);
                            idx += 1;
                            if ('@'..='~').contains(&ch) {
                                break;
                            }
                        }
                        if seq == "2K" {
                            lines[row].clear();
                            col = 0;
                        }
                    }
                    Some(']') => {
                        idx += 1;
                        let mut escape = false;
                        while idx < chars.len() {
                            let ch = chars[idx];
                            idx += 1;
                            if ch == '\x07' || (escape && ch == '\\') {
                                break;
                            }
                            escape = ch == '\x1b';
                        }
                    }
                    Some(_) => {
                        idx += 1;
                    }
                    None => break,
                }
            }
            '\r' => {
                col = 0;
                idx += 1;
            }
            '\n' => {
                lines.push(Vec::new());
                row += 1;
                col = 0;
                idx += 1;
            }
            ch => {
                if col < lines[row].len() {
                    lines[row][col] = ch;
                } else {
                    while lines[row].len() < col {
                        lines[row].push(' ');
                    }
                    lines[row].push(ch);
                }
                col += 1;
                idx += 1;
            }
        }
    }

    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }

    lines
        .into_iter()
        .map(|line| line.into_iter().collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn pty_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
