use std::time::{SystemTime, UNIX_EPOCH};

use super::*;

fn temp_file_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("mu-{name}-{nanos}.tmp"))
}

#[test]
fn load_prompt_file_preserves_body() {
    let path = temp_file_path("prompt");
    std::fs::write(&path, "hello\nworld\n").unwrap();
    let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(prompt, "hello\nworld");
}

#[test]
fn load_prompt_file_trims_shebang_line() {
    let path = temp_file_path("shebang");
    std::fs::write(&path, "#!/usr/bin/env -S mu --output plain\nhello\n").unwrap();
    let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(prompt, "hello");
}

#[test]
fn load_prompt_file_trims_crlf_shebang_line() {
    let path = temp_file_path("shebang-crlf");
    std::fs::write(&path, "#!/usr/bin/env -S mu\r\nhello\r\n").unwrap();
    let prompt = load_prompt(PromptSource::File(path.clone())).unwrap();
    std::fs::remove_file(path).unwrap();
    assert_eq!(prompt, "hello");
}

#[test]
fn load_prompt_file_rejects_shebang_only() {
    let path = temp_file_path("shebang-only");
    std::fs::write(&path, "#!/usr/bin/env -S mu --output plain\n").unwrap();
    let err = load_prompt(PromptSource::File(path.clone())).unwrap_err();
    std::fs::remove_file(path).unwrap();
    assert_eq!(err.to_string(), "empty prompt");
}

#[test]
fn normalize_prompt_keeps_stdin_shebang_text() {
    let prompt = normalize_prompt("#!/usr/bin/env -S mu --output plain\nhello\n", false).unwrap();
    assert_eq!(prompt, "#!/usr/bin/env -S mu --output plain\nhello");
}

#[test]
fn normalize_prompt_trims_file_shebang_text() {
    let prompt = normalize_prompt("#!/usr/bin/env -S mu --output plain\nhello\n", true).unwrap();
    assert_eq!(prompt, "hello");
}

#[test]
fn load_prompt_file_reports_utf8_errors_with_path() {
    let path = temp_file_path("invalid-utf8");
    std::fs::write(&path, [0xff, 0xfe, 0xfd]).unwrap();
    let err = load_prompt(PromptSource::File(path.clone())).unwrap_err();
    std::fs::remove_file(&path).unwrap();
    assert!(err.to_string().contains("reading prompt file"));
    assert!(err.to_string().contains(path.to_string_lossy().as_ref()));
}
