//! End-to-end coverage for the OpenAI-compatible HTTP embedder.
//!
//! A real loopback HTTP server stands in for the embedding backend, so the
//! request shape, auth header, batching, dimension handling, and the
//! `index`/`search semantic`/`doctor` wiring are all exercised over the wire.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DIMENSION: usize = 1024;
const MODEL: &str = "Qwen3-Embedding-0.6B";
const API_KEY: &str = "test-secret-token";

#[derive(Default)]
struct ServerState {
    auth_headers: Mutex<Vec<String>>,
    inputs: Mutex<Vec<String>>,
    requests: AtomicUsize,
}

struct MockEmbeddingServer {
    base_url: String,
    state: Arc<ServerState>,
}

impl MockEmbeddingServer {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let addr = listener.local_addr().expect("read listener address");
        let state = Arc::new(ServerState::default());
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let connection_state = Arc::clone(&thread_state);
                std::thread::spawn(move || handle_connection(stream, connection_state));
            }
        });
        Self {
            base_url: format!("http://{addr}/v1"),
            state,
        }
    }

    fn seen_inputs(&self) -> Vec<String> {
        self.state.inputs.lock().expect("inputs lock").clone()
    }

    fn auth_headers(&self) -> Vec<String> {
        self.state.auth_headers.lock().expect("auth lock").clone()
    }

    fn request_count(&self) -> usize {
        self.state.requests.load(Ordering::SeqCst)
    }
}

fn handle_connection(mut stream: TcpStream, state: Arc<ServerState>) {
    let reader_stream = stream.try_clone().expect("clone stream for reading");
    let mut reader = BufReader::new(reader_stream);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = header.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => state
                    .auth_headers
                    .lock()
                    .expect("auth lock")
                    .push(value.trim().to_string()),
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }

    let request: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let inputs = request
        .get("input")
        .and_then(|value| value.as_array())
        .map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    state.requests.fetch_add(1, Ordering::SeqCst);
    state
        .inputs
        .lock()
        .expect("inputs lock")
        .extend(inputs.iter().cloned());

    let data = inputs
        .iter()
        .enumerate()
        .map(|(index, text)| {
            serde_json::json!({
                "object": "embedding",
                "index": index,
                "embedding": vector_for(text, DIMENSION),
            })
        })
        .collect::<Vec<_>>();
    let response = serde_json::json!({
        "object": "list",
        "data": data,
        "model": request.get("model").cloned().unwrap_or(serde_json::Value::Null),
    });
    let payload = serde_json::to_vec(&response).expect("serialize response");

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&payload);
    let _ = stream.flush();
}

/// Bag-of-hashed-words vector, matching the crate's lexical overlap intuition so
/// a query sharing terms with a passage scores above unrelated passages.
fn vector_for(text: &str, dimension: usize) -> Vec<f32> {
    let mut vector = vec![0.0_f32; dimension];
    for term in text.split_whitespace() {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in term.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        let index = usize::try_from(hash % u64::try_from(dimension).expect("dimension fits u64"))
            .expect("index fits usize");
        vector[index] += 1.0;
    }
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in &mut vector {
            *value /= norm;
        }
    }
    vector
}

fn cli() -> Command {
    Command::cargo_bin("lazykatok").expect("lazykatok binary")
}

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/kakao")
        .join(name)
}

fn configure_http_embedder<'a>(
    command: &'a mut Command,
    server: &MockEmbeddingServer,
) -> &'a mut Command {
    command
        .env("KATOK_EMBEDDER", "openai-compatible")
        .env("KATOK_EMBED_URL", &server.base_url)
        .env("KATOK_EMBED_MODEL", MODEL)
        .env("KATOK_EMBED_DIM", DIMENSION.to_string())
        .env("KATOK_EMBED_API_KEY", API_KEY)
}

fn stdout_json(output: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse stdout as JSON: {error}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn openai_compatible_embedder_indexes_searches_and_reports_doctor() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let data_dir = dir.path().join("data");
    let server = MockEmbeddingServer::start();

    let data_args = |command: &mut Command| {
        command.args(["--data-dir", data_dir.to_str().expect("utf8 data dir")]);
    };

    let mut sync = cli();
    data_args(&mut sync);
    let sync_output = sync
        .args([
            "sync",
            "--source",
            "fixture",
            fixture("replies.jsonl").to_str().expect("utf8 fixture"),
            "--json",
        ])
        .output()
        .expect("run sync");
    assert!(
        sync_output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&sync_output.stderr)
    );

    let mut index = cli();
    data_args(&mut index);
    configure_http_embedder(&mut index, &server);
    let index_output = index.args(["index", "--json"]).output().expect("run index");
    assert!(
        index_output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );
    let index_json = stdout_json(&index_output);
    assert_eq!(index_json["embedder"], format!("openai/{MODEL}"));
    assert_eq!(index_json["embedded_texts"].as_u64(), Some(1));
    assert!(index_json["embedding_calls"].as_u64().unwrap_or(0) >= 1);

    let mut search = cli();
    data_args(&mut search);
    configure_http_embedder(&mut search, &server);
    let search_output = search
        .args(["search", "semantic", "회의 보고서", "--json"])
        .output()
        .expect("run semantic search");
    assert!(
        search_output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&search_output.stderr)
    );
    let hits = stdout_json(&search_output);
    let hits = hits.as_array().expect("search returns an array");
    assert!(!hits.is_empty(), "semantic search returned no hits");
    let child_ids = hits[0]["child_chunk_ids"]
        .as_array()
        .expect("hit carries child chunk ids");
    assert!(
        child_ids
            .iter()
            .any(|id| id.as_str() == Some("chunk_2aeac4db0a04ceb2")),
        "expected the matching parent window, got {hits:?}"
    );

    let mut doctor = cli();
    data_args(&mut doctor);
    configure_http_embedder(&mut doctor, &server);
    let doctor_output = doctor
        .args(["doctor", "--json"])
        .output()
        .expect("run doctor");
    assert!(
        doctor_output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&doctor_output.stderr)
    );
    let doctor_json = stdout_json(&doctor_output);
    assert_eq!(doctor_json["embedder"]["provider"], "openai-compatible");
    assert_eq!(doctor_json["embedder"]["mode"], "openai-compatible");
    assert_eq!(doctor_json["embedder"]["model"], MODEL);
    assert_eq!(
        doctor_json["embedder"]["dimension"].as_u64(),
        Some(DIMENSION as u64)
    );
    assert_eq!(doctor_json["embedder"]["endpoint"], server.base_url);

    assert!(
        server.request_count() >= 1,
        "the loopback embedding server saw no requests"
    );
    for header in server.auth_headers() {
        assert_eq!(header, format!("Bearer {API_KEY}"));
    }
}

#[test]
fn embed_url_alone_selects_the_openai_compatible_provider() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let data_dir = dir.path().join("data");

    let mut doctor = cli();
    doctor
        .env_remove("KATOK_EMBEDDER")
        .env_remove("LAZYKATOK_EMBEDDER")
        .env_remove("KATOK_EMBED_MODEL")
        .env_remove("KATOK_EMBED_DIM")
        .env("KATOK_EMBED_URL", "http://127.0.0.1:9/v1")
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "doctor",
            "--json",
        ]);
    let output = doctor.output().expect("run doctor");
    assert!(
        output.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json = stdout_json(&output);
    assert_eq!(json["embedder"]["provider"], "openai-compatible");
    assert_eq!(json["embedder"]["endpoint"], "http://127.0.0.1:9/v1");
}

#[test]
fn query_and_passage_prefixes_are_sent_to_the_endpoint() {
    let dir = tempfile::tempdir().expect("create tempdir");
    let data_dir = dir.path().join("data");
    let config = dir.path().join("katok.toml");
    std::fs::write(
        &config,
        "passage_prefix = \"passage: \"\nquery_prefix = \"query: \"\n",
    )
    .expect("write config");
    let server = MockEmbeddingServer::start();

    let mut sync = cli();
    let sync_output = sync
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "sync",
            "--source",
            "fixture",
            fixture("replies.jsonl").to_str().expect("utf8 fixture"),
            "--json",
        ])
        .output()
        .expect("run sync");
    assert!(
        sync_output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&sync_output.stderr)
    );

    let mut index = cli();
    configure_http_embedder(&mut index, &server);
    let index_output = index
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "index",
            "--json",
        ])
        .output()
        .expect("run index");
    assert!(
        index_output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&index_output.stderr)
    );

    let mut search = cli();
    configure_http_embedder(&mut search, &server);
    let search_output = search
        .args([
            "--config",
            config.to_str().expect("utf8 config"),
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "search",
            "semantic",
            "회의 보고서",
            "--json",
        ])
        .output()
        .expect("run semantic search");
    assert!(
        search_output.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&search_output.stderr)
    );

    let inputs = server.seen_inputs();
    assert!(
        inputs.iter().any(|input| input.starts_with("passage: ")),
        "no indexed passage carried the passage prefix: {inputs:?}"
    );
    assert!(
        inputs.iter().any(|input| input.starts_with("query: ")),
        "the search query carried no query prefix: {inputs:?}"
    );
}
