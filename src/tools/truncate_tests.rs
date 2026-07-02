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
