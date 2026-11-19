//! End-to-end coverage for the `inl` embedder provider.
//!
//! A loopback server stands in for an `im-not-llm` daemon: it answers the
//! discovery `GET /health` and the OpenAI `POST /v1/embeddings` route, so the
//! discovery rules, the wire shape, and the vector identity are all exercised
//! over the wire through the `lazykatok` CLI. A real `inl serve --multimodal`
//! daemon is exercised manually against a fixture archive.

use assert_cmd::Command;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const DIMENSION: usize = 768;
const MODEL: &str = "google/embeddinggemma-2";

#[derive(Default)]
struct DaemonState {
    health_requests: AtomicUsize,
    embedding_requests: AtomicUsize,
    models: Mutex<Vec<String>>,
    input_types: Mutex<Vec<String>>,
    inputs: Mutex<Vec<String>>,
}

#[derive(Clone)]
struct HealthSpec {
    status: &'static str,
    kind: &'static str,
    model: String,
    dimension: usize,
    modalities: Vec<String>,
}

impl HealthSpec {
    fn embeddinggemma() -> Self {
        Self {
            status: "ok",
            kind: "multimodal",
            model: MODEL.to_string(),
            dimension: DIMENSION,
            modalities: vec!["text".to_string(), "image".to_string()],
        }
    }
}

struct FakeDaemon {
    port: u16,
    state: Arc<DaemonState>,
}

impl FakeDaemon {
    fn start(health: HealthSpec) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().expect("read listener address").port();
        let state = Arc::new(DaemonState::default());
        let thread_state = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let connection_state = Arc::clone(&thread_state);
                let connection_health = health.clone();
                std::thread::spawn(move || {
                    handle_connection(stream, connection_state, connection_health, port);
                });
            }
        });
        Self { port, state }
    }

    fn health_requests(&self) -> usize {
        self.state.health_requests.load(Ordering::SeqCst)
    }

    fn embedding_requests(&self) -> usize {
        self.state.embedding_requests.load(Ordering::SeqCst)
    }

    fn models(&self) -> Vec<String> {
        self.state.models.lock().expect("models lock").clone()
    }

    fn input_types(&self) -> Vec<String> {
        self.state
            .input_types
            .lock()
            .expect("input types lock")
            .clone()
    }
}

fn handle_connection(
    mut stream: TcpStream,
    state: Arc<DaemonState>,
    health: HealthSpec,
    port: u16,
) {
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
        if header.trim_end().is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim_end().split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();
    if path.starts_with("/health") {
        state.health_requests.fetch_add(1, Ordering::SeqCst);
        let payload = serde_json::json!({
            "status": health.status,
            "kind": health.kind,
            "model_id": health.model,
            "dimension": health.dimension,
            "dtype": "f16",
            "device": "metal",
            "attention": "auto",
            "max_length": 2048,
            "modalities": health.modalities,
            "max_inputs": 32,
            "instance": "inl-test",
            "pid": std::process::id(),
            "host": "127.0.0.1",
            "port": port,
            "url": format!("http://127.0.0.1:{port}"),
            "version": "0.3.3",
            "requests_total": state.embedding_requests.load(Ordering::SeqCst),
            "loaded": true,
        });
        respond(
            &mut stream,
            200,
            &serde_json::to_vec(&payload).expect("health json"),
        );
        return;
    }
    if path != "/v1/embeddings" {
        respond(&mut stream, 404, b"{}");
        return;
    }

    let request: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let model = request
        .get("model")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
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

    state.embedding_requests.fetch_add(1, Ordering::SeqCst);
    state
        .models
        .lock()
        .expect("models lock")
        .push(model.clone());
    if let Some(input_type) = request.get("input_type").and_then(|value| value.as_str()) {
        state
            .input_types
            .lock()
            .expect("input types lock")
            .push(input_type.to_string());
    }
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
    let payload = serde_json::json!({
        "object": "list",
        "data": data,
        "model": model,
    });
    respond(
        &mut stream,
        200,
        &serde_json::to_vec(&payload).expect("embeddings json"),
    );
}

fn respond(stream: &mut TcpStream, status: u16, payload: &[u8]) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(payload);
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

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/kakao")
        .join(name)
}

/// Write the discovery files `inl` leaves in `<cache root>/serve/<instance>`.
fn write_serve_instance(home: &std::path::Path, host: &str, port: u16) {
    let directory = home.join(".cache/im-not-llm/serve/inl-test");
    std::fs::create_dir_all(&directory).expect("create serve instance directory");
    std::fs::write(directory.join("host"), format!("{host}\n")).expect("write host");
    std::fs::write(directory.join("port"), format!("{port}\n")).expect("write port");
}

fn sync_fixture(data_dir: &std::path::Path) {
    let output = cli()
        .args([
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
        output.status.success(),
        "sync failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn index_json(
    home: &std::path::Path,
    data_dir: &std::path::Path,
    provider: &str,
) -> serde_json::Value {
    let output = cli()
        .env("HOME", home)
        .env("KATOK_EMBEDDER", provider)
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "index",
            "--json",
        ])
        .output()
        .expect("run index");
    assert!(
        output.status.success(),
        "index failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "parse index stdout: {error}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

#[test]
fn inl_provider_indexes_and_searches_through_a_discovered_daemon() {
    let home = tempfile::tempdir().expect("create home");
    let data_dir = home.path().join("data");
    let daemon = FakeDaemon::start(HealthSpec::embeddinggemma());
    write_serve_instance(home.path(), "127.0.0.1", daemon.port);

    sync_fixture(&data_dir);

    let index = index_json(home.path(), &data_dir, "inl");
    assert_eq!(index["embedder"], format!("inl/{MODEL}"));
    assert_eq!(index["embedded_texts"].as_u64(), Some(1));
    assert!(index["embedding_calls"].as_u64().unwrap_or(0) >= 1);
    assert!(
        daemon.health_requests() >= 1,
        "discovery never asked the daemon for its health"
    );
    assert_eq!(daemon.embedding_requests(), 1);
    assert_eq!(daemon.models(), vec![MODEL.to_string()]);
    assert!(
        daemon.input_types().contains(&"passage".to_string()),
        "indexing should label passages: {:?}",
        daemon.input_types()
    );

    let search = cli()
        .env("HOME", home.path())
        .env("KATOK_EMBEDDER", "inl")
        .args([
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
        search.status.success(),
        "search failed: {}",
        String::from_utf8_lossy(&search.stderr)
    );
    let hits: serde_json::Value = serde_json::from_slice(&search.stdout).expect("parse hits");
    let hits = hits.as_array().expect("search returns an array");
    assert!(!hits.is_empty(), "semantic search returned no hits");
    assert!(
        hits[0]["child_chunk_ids"]
            .as_array()
            .expect("hit carries child chunk ids")
            .iter()
            .any(|id| id.as_str() == Some("chunk_2aeac4db0a04ceb2")),
        "expected the matching parent window, got {hits:?}"
    );
    assert!(
        daemon.input_types().contains(&"query".to_string()),
        "search should label the query: {:?}",
        daemon.input_types()
    );

    let doctor = cli()
        .env("HOME", home.path())
        .env("KATOK_EMBEDDER", "inl")
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "doctor",
            "--json",
        ])
        .output()
        .expect("run doctor");
    assert!(
        doctor.status.success(),
        "doctor failed: {}",
        String::from_utf8_lossy(&doctor.stderr)
    );
    let doctor: serde_json::Value = serde_json::from_slice(&doctor.stdout).expect("parse doctor");
    assert_eq!(doctor["embedder"]["provider"], "inl");
    assert_eq!(doctor["embedder"]["mode"], "inl");
    assert_eq!(doctor["embedder"]["dimension"].as_u64(), Some(768));
}

#[test]
fn switching_from_local_vectors_to_inl_reeembeds() {
    let home = tempfile::tempdir().expect("create home");
    let data_dir = home.path().join("data");
    sync_fixture(&data_dir);

    let local = index_json(home.path(), &data_dir, "local-test");
    assert_eq!(local["embedder"], "embeddinggemma/local-test");
    assert_eq!(local["embedded_texts"].as_u64(), Some(1));

    // Same daemon-visible model, but a distinct recorded id, so the local
    // vectors are not reused: a switch re-embeds instead of silently mixing.
    let daemon = FakeDaemon::start(HealthSpec::embeddinggemma());
    write_serve_instance(home.path(), "127.0.0.1", daemon.port);
    let inl = index_json(home.path(), &data_dir, "inl");
    assert_eq!(inl["embedder"], format!("inl/{MODEL}"));
    assert_eq!(inl["embedded_texts"].as_u64(), Some(1));
    assert_eq!(inl["reused_vectors"].as_u64(), Some(0));
    assert_eq!(daemon.embedding_requests(), 1);
}

#[test]
fn inl_provider_without_a_daemon_reports_how_to_start_one() {
    let home = tempfile::tempdir().expect("create home");
    let data_dir = home.path().join("data");
    sync_fixture(&data_dir);

    let output = cli()
        .env("HOME", home.path())
        .env("KATOK_EMBEDDER", "inl")
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "index",
        ])
        .output()
        .expect("run index");
    assert!(
        !output.status.success(),
        "index should fail without a daemon"
    );
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("inl serve --multimodal"), "{message}");
}

#[test]
fn inl_provider_rejects_a_daemon_of_the_wrong_dimension() {
    let home = tempfile::tempdir().expect("create home");
    let data_dir = home.path().join("data");
    sync_fixture(&data_dir);

    let mut health = HealthSpec::embeddinggemma();
    health.dimension = 512;
    let daemon = FakeDaemon::start(health);
    write_serve_instance(home.path(), "127.0.0.1", daemon.port);

    let output = cli()
        .env("HOME", home.path())
        .env("KATOK_EMBEDDER", "inl")
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "index",
        ])
        .output()
        .expect("run index");
    assert!(!output.status.success(), "index should reject the daemon");
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(message.contains("inl serve --multimodal"), "{message}");
    assert!(message.contains("dimension=512"), "{message}");
    assert_eq!(daemon.embedding_requests(), 0);
}

#[test]
fn inl_provider_ignores_a_non_loopback_instance_directory() {
    let home = tempfile::tempdir().expect("create home");
    let data_dir = home.path().join("data");
    sync_fixture(&data_dir);

    let daemon = FakeDaemon::start(HealthSpec::embeddinggemma());
    // A state directory that records a non-loopback host must not be dialled.
    write_serve_instance(home.path(), "10.0.0.1", daemon.port);

    let output = cli()
        .env("HOME", home.path())
        .env("KATOK_EMBEDDER", "inl")
        .args([
            "--data-dir",
            data_dir.to_str().expect("utf8 data dir"),
            "index",
        ])
        .output()
        .expect("run index");
    assert!(
        !output.status.success(),
        "index should fail without a daemon"
    );
    assert_eq!(daemon.health_requests(), 0);
}
