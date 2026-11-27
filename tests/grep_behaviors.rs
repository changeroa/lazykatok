use assert_cmd::Command;
use lazykatok::{
    archive::Archive,
    chunking::rebuild_chunks,
    grep::{grep_archive, GrepOptions},
    types::RawMessage,
    Error,
};
use tempfile::TempDir;

fn synthetic_message(
    chat_id: &str,
    chat_name: &str,
    message_id: &str,
    sender_nickname: &str,
    timestamp: &str,
    text: &str,
) -> RawMessage {
    RawMessage {
        account_hash: "acct-synth".to_string(),
        chat_id: chat_id.to_string(),
        chat_name: chat_name.to_string(),
        chat_type: "group".to_string(),
        message_id: message_id.to_string(),
        sender_id: format!("sender-{sender_nickname}"),
        sender_nickname: sender_nickname.to_string(),
        timestamp: timestamp.parse().expect("valid rfc3339 timestamp"),
        text: text.to_string(),
        message_type: "text".to_string(),
        reply_to_message_id: None,
    }
}

fn seed_archive(dir: &TempDir) -> Archive {
    let archive_path = dir.path().join("archive.sqlite3");
    let archive = Archive::open(&archive_path).expect("open archive");
    let messages = vec![
        synthetic_message(
            "chat-ops",
            "Ops Sync",
            "m-1",
            "Alex",
            "2026-06-01T09:00:00Z",
            "line 0 intro\nerror_code=ERR_4201 on node-a\nline 2 followup",
        ),
        synthetic_message(
            "chat-ops",
            "Ops Sync",
            "m-2",
            "Bora",
            "2026-06-01T12:00:00Z",
            "배포 코드 확인: ERR_4202 그리고 100%_raw_literal",
        ),
        synthetic_message(
            "chat-design",
            "Design Review",
            "m-3",
            "Cathy",
            "2026-06-02T10:00:00Z",
            "lowercase err_4201 mentioned in design note",
        ),
    ];
    archive.sync_messages(&messages).expect("sync messages");
    rebuild_chunks(&archive).expect("rebuild chunks");
    archive
}

#[test]
fn grep_archive_literal_case_and_context_lines() {
    let dir = TempDir::new().expect("tempdir");
    let archive = seed_archive(&dir);

    let exact = grep_archive(
        &archive,
        "ERR_4201",
        &GrepOptions {
            literal: true,
            ignore_case: false,
            context: 1,
            ..Default::default()
        },
    )
    .expect("literal exact grep");
    assert_eq!(exact.len(), 1);
    let hit = &exact[0];
    assert_eq!(hit.ranker, "grep");
    assert_eq!(hit.unit, "micro_chunk");
    assert_eq!(hit.chat_id, "chat-ops");
    assert_eq!(hit.sender_nickname, "Alex");
    assert_eq!(hit.line_number, 2);
    assert_eq!(hit.matched_line, "error_code=ERR_4201 on node-a");
    assert_eq!(hit.context_before, vec!["line 0 intro".to_string()]);
    assert_eq!(hit.context_after, vec!["line 2 followup".to_string()]);
    assert!(hit.snippet.contains("line 0 intro"));
    assert!(hit.snippet.contains("line 2 followup"));

    let folded = grep_archive(
        &archive,
        "ERR_4201",
        &GrepOptions {
            literal: true,
            ignore_case: true,
            ..Default::default()
        },
    )
    .expect("case-insensitive grep");
    assert_eq!(folded.len(), 2);
    // Ordered newest first.
    assert_eq!(folded[0].chat_id, "chat-design");
    assert_eq!(folded[1].chat_id, "chat-ops");
}

#[test]
fn grep_archive_regex_sql_like_escaping_and_filters() {
    let dir = TempDir::new().expect("tempdir");
    let archive = seed_archive(&dir);

    let regex_hits = grep_archive(
        &archive,
        r"ERR_420[12]",
        &GrepOptions {
            literal: false,
            ..Default::default()
        },
    )
    .expect("regex grep");
    assert_eq!(regex_hits.len(), 2);

    // SQLite LIKE wildcards (% and _) must be treated literally when --literal is set.
    let wildcard_literal = grep_archive(
        &archive,
        "100%_raw",
        &GrepOptions {
            literal: true,
            ..Default::default()
        },
    )
    .expect("literal wildcard grep");
    assert_eq!(wildcard_literal.len(), 1);
    assert_eq!(wildcard_literal[0].sender_nickname, "Bora");

    // Non-matching wildcard literal must not falsely match arbitrary characters.
    let no_false_match = grep_archive(
        &archive,
        "100%_not_there",
        &GrepOptions {
            literal: true,
            ..Default::default()
        },
    )
    .expect("non-matching wildcard grep");
    assert!(no_false_match.is_empty());

    // Chat, sender, and time filters compose cleanly.
    let filtered = grep_archive(
        &archive,
        "ERR_420",
        &GrepOptions {
            literal: true,
            chat: Some("chat-ops".to_string()),
            sender: Some("bora".to_string()),
            since: Some("2026-06-01T11:00:00Z".to_string()),
            until: Some("2026-06-01T13:00:00Z".to_string()),
            ..Default::default()
        },
    )
    .expect("filtered grep");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].sender_nickname, "Bora");

    // Validation errors on empty pattern and malformed regex.
    assert!(matches!(
        grep_archive(&archive, "", &GrepOptions::default()),
        Err(Error::EmptyQuery)
    ));
    assert!(matches!(
        grep_archive(&archive, "(unclosed", &GrepOptions::default()),
        Err(Error::InvalidRegex(_))
    ));
}

#[test]
fn cli_grep_and_search_grep_emit_structured_json() {
    let dir = TempDir::new().expect("tempdir");
    let _archive = seed_archive(&dir);

    let direct_out = Command::cargo_bin("lazykatok")
        .expect("lazykatok binary")
        .args([
            "--data-dir",
            dir.path().to_str().expect("utf8 path"),
            "grep",
            "ERR_420[12]",
            "--json",
            "--limit",
            "5",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let direct_hits: serde_json::Value =
        serde_json::from_slice(&direct_out).expect("parse direct grep json");
    let direct_arr = direct_hits.as_array().expect("json array");
    assert_eq!(direct_arr.len(), 2);
    assert_eq!(direct_arr[0]["ranker"], "grep");
    assert_eq!(direct_arr[0]["chat_id"], "chat-ops");

    let sub_out = Command::cargo_bin("lazykatok")
        .expect("lazykatok binary")
        .args([
            "--data-dir",
            dir.path().to_str().expect("utf8 path"),
            "search",
            "grep",
            "ERR_4201",
            "-F",
            "-i",
            "-C",
            "1",
            "--chat-name",
            "ops",
            "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let sub_hits: serde_json::Value =
        serde_json::from_slice(&sub_out).expect("parse search grep json");
    let sub_arr = sub_hits.as_array().expect("json array");
    assert_eq!(sub_arr.len(), 1);
    assert_eq!(sub_arr[0]["line_number"], 2);
    assert_eq!(sub_arr[0]["context_before"][0], "line 0 intro");

    // Invalid regex produces structured JSON error with invalid_regex code.
    let err_out = Command::cargo_bin("lazykatok")
        .expect("lazykatok binary")
        .args([
            "--data-dir",
            dir.path().to_str().expect("utf8 path"),
            "grep",
            "[unclosed",
            "--json",
        ])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();
    let err_json: serde_json::Value =
        serde_json::from_slice(&err_out).expect("parse error envelope");
    assert_eq!(err_json["ok"], false);
    assert_eq!(err_json["error"]["code"], "invalid_regex");
}
