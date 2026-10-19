use assert_cmd::Command;
use serde_json::Value;
use std::path::Path;

fn fixture_path() -> String {
    format!(
        "{}/tests/fixtures/kakao/hybrid.jsonl",
        env!("CARGO_MANIFEST_DIR")
    )
}

fn lazykatok(data_dir: &Path) -> Command {
    let mut command = Command::cargo_bin("lazykatok").expect("lazykatok binary");
    command
        .env("KATOK_EMBEDDER", "local-test")
        .args(["--data-dir", data_dir.to_str().expect("utf8 path")]);
    command
}

fn synced_data_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create tempdir");
    lazykatok(dir.path())
        .args(["sync", "--source", "fixture", &fixture_path(), "--json"])
        .assert()
        .success();
    dir
}

fn indexed_data_dir() -> tempfile::TempDir {
    let dir = synced_data_dir();
    lazykatok(dir.path())
        .args(["index", "--json"])
        .assert()
        .success();
    dir
}

fn search(data_dir: &Path, mode: &str, query: &str, extra: &[&str]) -> Vec<Value> {
    let output = lazykatok(data_dir)
        .args(["search", mode, query, "--json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice::<Vec<Value>>(&output).expect("search emits a JSON array")
}

fn ids(hits: &[Value]) -> Vec<String> {
    hits.iter()
        .map(|hit| hit["chunk_id"].as_str().expect("chunk_id").to_string())
        .collect()
}

#[test]
fn hybrid_returns_parent_windows_that_contain_the_bm25_chunks() {
    let dir = indexed_data_dir();

    let bm25 = search(dir.path(), "bm25", "rollback", &[]);
    let hybrid = search(dir.path(), "hybrid", "rollback", &[]);

    assert!(bm25.iter().all(|hit| hit["unit"] == "micro_chunk"));
    assert!(hybrid.iter().all(|hit| hit["unit"] == "parent_window"));
    assert!(hybrid.iter().all(|hit| hit["ranker"] == "hybrid"));
    for chunk in ids(&bm25) {
        assert!(
            hybrid.iter().any(|window| window["child_chunk_ids"]
                .as_array()
                .expect("child_chunk_ids")
                .iter()
                .any(|child| child == chunk.as_str())),
            "no hybrid window contains bm25 chunk {chunk}"
        );
    }
}

#[test]
fn hybrid_credits_a_window_for_the_bm25_rank_of_its_chunk() {
    let dir = indexed_data_dir();

    let semantic = search(dir.path(), "semantic", "rollback", &[]);
    let hybrid = search(dir.path(), "hybrid", "rollback", &[]);

    // Both windows are found by both rankings at swapped ranks, so they tie; the tie
    // goes to the BM25 order. Semantic alone ranks them the other way round.
    let bm25_first_window = {
        let bm25 = search(dir.path(), "bm25", "rollback", &[]);
        let first_chunk = bm25[0]["chunk_id"].as_str().expect("chunk_id").to_string();
        semantic
            .iter()
            .find(|window| {
                window["child_chunk_ids"]
                    .as_array()
                    .expect("child_chunk_ids")
                    .iter()
                    .any(|child| child == first_chunk.as_str())
            })
            .expect("semantic returns the window of the best bm25 chunk")["chunk_id"]
            .clone()
    };
    assert_ne!(semantic[0]["chunk_id"], bm25_first_window);
    assert_eq!(hybrid[0]["chunk_id"], bm25_first_window);
    assert_eq!(hybrid.len(), 2);
}

#[test]
fn hybrid_honors_the_limit() {
    let dir = indexed_data_dir();

    let hybrid = search(dir.path(), "hybrid", "rollback", &["--limit", "1"]);

    assert_eq!(hybrid.len(), 1);
    assert_eq!(hybrid[0]["rank"], 1);
}

#[test]
fn hybrid_without_a_semantic_index_fails_instead_of_returning_keyword_only_hits() {
    let dir = synced_data_dir();

    let output = lazykatok(dir.path())
        .args(["search", "hybrid", "rollback", "--json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let error: Value = serde_json::from_slice(&output).expect("error payload is JSON");
    assert_eq!(error["ok"], false);
    assert_eq!(error["error"]["code"], "semantic_index_missing");
}

#[test]
fn hybrid_rejects_an_empty_query() {
    let dir = indexed_data_dir();

    let output = lazykatok(dir.path())
        .args(["search", "hybrid", "   ", "--json"])
        .assert()
        .failure()
        .get_output()
        .stdout
        .clone();

    let error: Value = serde_json::from_slice(&output).expect("error payload is JSON");
    assert_eq!(error["error"]["code"], "empty_query");
}
