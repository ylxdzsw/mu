use crate::config::LimitsConfig;

pub struct TruncationResult {
    pub text: String,
}

pub fn truncate_output(
    output: &str,
    limits: &LimitsConfig,
    spill_prefix: &str,
    state_dir: &std::path::Path,
    use_tail: bool,
) -> anyhow::Result<TruncationResult> {
    let max_line_bytes = limits.max_line_bytes;
    let max_lines = limits.max_lines;
    let max_bytes = limits.max_bytes;

    let lines: Vec<&str> = output.lines().collect();
    let total_lines = lines.len();
    let total_bytes = output.len();

    let within_lines = total_lines <= max_lines;
    let within_bytes = total_bytes <= max_bytes;
    let line_ok = lines.iter().all(|l| l.len() <= max_line_bytes);

    if within_lines && within_bytes && line_ok {
        return Ok(TruncationResult {
            text: output.to_string(),
        });
    }

    crate::paths::ensure_dir(&state_dir.join("truncation"))?;
    let spill_path =
        state_dir
            .join("truncation")
            .join(format!("{}-{}.txt", spill_prefix, uuid::Uuid::new_v4()));
    std::fs::write(&spill_path, output)?;

    let preview = if use_tail {
        build_tail_preview(&lines, max_lines, max_bytes, max_line_bytes)
    } else {
        build_head_preview(&lines, max_lines, max_bytes, max_line_bytes)
    };

    let elided_lines = total_lines.saturating_sub(preview.lines().count());
    let marker = format!(
        "\n[… {elided_lines} lines elided; full output saved to {}; inspect it with `bash` if needed]",
        spill_path.display()
    );

    Ok(TruncationResult {
        text: format!("{preview}{marker}"),
    })
}

fn build_head_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for line in lines {
        if count >= max_lines {
            break;
        }
        let truncated_line = truncate_line(line, max_line_bytes);
        if out.len() + truncated_line.len() + 1 > max_bytes {
            break;
        }
        if count > 0 {
            out.push('\n');
        }
        out.push_str(&truncated_line);
        count += 1;
    }
    out
}

fn build_tail_preview(
    lines: &[&str],
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
) -> String {
    let start = lines.len().saturating_sub(max_lines);
    let mut out = String::new();
    for line in &lines[start..] {
        let truncated_line = truncate_line(line, max_line_bytes);
        if out.len() + truncated_line.len() + 1 > max_bytes {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&truncated_line);
    }
    out
}

fn truncate_line(line: &str, max_bytes: usize) -> String {
    if line.len() <= max_bytes {
        return line.to_string();
    }
    let budget = max_bytes.saturating_sub(3);
    // Find the largest char boundary <= budget so we never split a codepoint.
    let mut end = budget.min(line.len());
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

pub fn prune_truncation_spills(state_dir: &std::path::Path, retention_days: u64) {
    let dir = state_dir.join("truncation");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let cutoff =
        std::time::SystemTime::now() - std::time::Duration::from_secs(retention_days * 24 * 3600);
    for entry in entries.flatten() {
        if let Ok(meta) = entry.metadata() {
            if let Ok(modified) = meta.modified() {
                if modified < cutoff {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_line;

    #[test]
    fn truncate_line_respects_char_boundaries() {
        // A line of multibyte chars whose byte length exceeds the budget.
        // Cutting at an arbitrary byte index would panic; this must not.
        let line = "héllo wörld ".repeat(20); // multibyte é and ö
        let out = truncate_line(&line, 25);
        assert!(out.ends_with('…'));
        // The prefix before the ellipsis must be valid UTF-8 (guaranteed by
        // returning a String) and within the byte budget.
        assert!(out.len() <= 25 + '…'.len_utf8());
    }

    #[test]
    fn truncate_line_short_passthrough() {
        assert_eq!(truncate_line("abc", 100), "abc");
    }
}
