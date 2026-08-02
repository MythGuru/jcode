//! Tokenizer tests. The tokenizer's job is to make quoting and chaining
//! non-bypasses, so those are the cases that matter.

use super::*;

fn texts(command: &str) -> Vec<String> {
    tokenize(command).into_iter().map(|t| t.text).collect()
}

#[test]
fn quoting_does_not_hide_a_target() {
    // All three must yield the same target word.
    for command in [r#"rm -rf $HOME"#, r#"rm -rf "$HOME""#, r#"rm -rf '$HOME'"#] {
        assert_eq!(texts(command), vec!["rm", "-rf", "$HOME"], "{command}");
    }
}

#[test]
fn backslash_escapes_are_resolved() {
    assert_eq!(
        texts(r"rm -rf /home/my\ files"),
        vec!["rm", "-rf", "/home/my files"]
    );
}

#[test]
fn chained_commands_split_into_separate_segments() {
    let segments = split_segments("echo hi && rm -rf /tmp/a; rm -rf /tmp/b");
    assert_eq!(segments.len(), 3, "{segments:#?}");
    assert_eq!(segments[1][0].text, "rm");
    assert_eq!(segments[2][0].text, "rm");
}

#[test]
fn piped_commands_are_separate_segments() {
    let segments = split_segments("find . -type f | xargs rm -rf");
    assert_eq!(segments.len(), 2);
    assert_eq!(segments[1][0].text, "xargs");
}

#[test]
fn truncating_redirect_target_is_marked_but_append_is_not() {
    let clobber = tokenize("echo x > important.conf");
    assert!(
        clobber
            .iter()
            .any(|t| t.is_truncating_redirect_target && t.text == "important.conf")
    );

    let append = tokenize("echo x >> log.txt");
    assert!(
        !append.iter().any(|t| t.is_truncating_redirect_target),
        "append does not truncate and must not be flagged"
    );
}

#[test]
fn recursive_flag_detected_in_bundles_and_long_form() {
    let cases = [
        ("-rf", true),
        ("-fr", true),
        ("-R", true),
        ("--recursive", true),
        ("-f", false),
        ("--force", false),
        ("-i", false),
    ];
    for (flag, expected) in cases {
        let token = Token::word(flag);
        assert_eq!(token.is_recursive_flag(), expected, "{flag}");
    }
}

#[test]
fn basename_strips_the_path_so_absolute_invocations_match() {
    assert_eq!(Token::word("/bin/rm").basename(), "rm");
    assert_eq!(Token::word("/usr/bin/shred").basename(), "shred");
}

#[test]
fn tokenizing_is_total_and_never_panics() {
    // Malformed input must degrade gracefully rather than crash the tool call.
    for command in [
        "",
        "   ",
        "'",
        "\"",
        "\\",
        ">",
        "&&",
        "rm -rf \"unterminated",
    ] {
        let _ = split_segments(command);
    }
}

#[test]
fn windows_switches_are_flags_not_targets() {
    for flag in ["/s", "/q", "/b", "/n", "/i", "/c:pattern", "/S"] {
        assert!(Token::word(flag).is_flag(), "{flag} should be a flag");
    }
    // Unix absolute paths must never be mistaken for switches.
    for path in ["/etc", "/home/u", "/usr/bin", "/tmp/x"] {
        assert!(!Token::word(path).is_flag(), "{path} is a path, not a flag");
    }
}

#[test]
fn windows_backslash_paths_survive_tokenization() {
    assert_eq!(
        texts(r"type C:\Users\micha\notes.md"),
        vec!["type", r"C:\Users\micha\notes.md"]
    );
}

#[test]
fn fd_number_redirects_leave_no_stray_operand() {
    // `2>nul` and `2>/dev/null` redirect stderr; the "2" is part of the
    // operator, not a file operand.
    let tokens = tokenize("cargo build 2>nul");
    let words: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
    assert!(!words.contains(&"2"), "stray fd digit in {words:?}");
    assert!(
        tokens
            .iter()
            .any(|t| t.is_truncating_redirect_target && t.text == "nul")
    );
}
