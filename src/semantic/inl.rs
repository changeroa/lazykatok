//! Discovery of a running `im-not-llm` (`inl`) daemon.
//!
//! `inl` owns the per-user model runtime. It writes `host`/`port` files under
//! `<cache root>/serve/<instance>` and reports what it serves over
//! `GET /health`. The `inl` embedder provider reads those files, asks the server
//! itself what it serves (never trusting the directory name), and embeds through
//! that daemon's OpenAI-compatible route. Nothing here starts a daemon: a
//! missing one becomes an actionable error.

use crate::{config::KatokConfig, Error, Result};
use serde::Deserialize;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

/// The multimodal `inl` model whose 768-dimensional text vectors the `inl`
/// provider accepts without extra configuration.
pub(crate) const INL_MULTIMODAL_MODEL: &str = "google/embeddinggemma-2";

/// `inl` binds only loopback and has no authentication, so a health answer must
/// come from this machine. Two seconds is the same budget `inl` uses itself.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

/// Root of `inl`'s discovery files: `<cache root>/serve`.
///
/// Mirrors `inl_core::device::cache_root` — `$HOME/.cache/im-not-llm` on Unix,
/// `%LOCALAPPDATA%\im-not-llm` on Windows. `INL_CACHE_DIR` is deliberately not
/// consulted: `inl` uses it only for the model weights, never for the serve
/// directory.
pub(crate) fn serve_root() -> PathBuf {
    #[cfg(windows)]
    let root = PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string()));
    #[cfg(not(windows))]
    let root =
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(".cache");
    root.join("im-not-llm").join("serve")
}

/// The subset of `GET /health` this provider needs.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InlHealth {
    pub(crate) status: String,
    #[serde(default = "dense_kind")]
    pub(crate) kind: String,
    pub(crate) model_id: String,
    pub(crate) dimension: usize,
    #[serde(default)]
    pub(crate) host: Option<String>,
    #[serde(default)]
    pub(crate) port: Option<u16>,
    /// Modalities the engine accepts, as `text`/`image`/`video`/`audio`.
    #[serde(default)]
    pub(crate) modalities: Vec<String>,
    #[serde(default)]
    pub(crate) instance: Option<String>,
    /// The loopback address that actually answered `/health`. Not part of the
    /// body: it is recorded by the probe, so the endpoint this provider dials is
    /// always the address it verified rather than a URL the body merely claims.
    #[serde(skip)]
    pub(crate) address: Option<SocketAddr>,
}

fn dense_kind() -> String {
    "dense".to_string()
}

impl InlHealth {
    fn is_ok(&self) -> bool {
        self.status == "ok"
    }

    /// Only a dense or multimodal server yields a single vector per text;
    /// sparse servers answer with `{index, value}` rows and late-interaction
    /// servers with per-token vectors, neither of which this provider reads.
    fn serves_dense_text(&self) -> bool {
        match self.kind.as_str() {
            "dense" => true,
            "multimodal" => self.modalities.iter().any(|modality| modality == "text"),
            _ => false,
        }
    }

    /// The server serves the model this provider wants: either the multimodal
    /// `embeddinggemma-2` it knows, or the model the caller configured.
    fn serves_wanted_model(&self, config: &KatokConfig) -> bool {
        let configured = config.embedder_model.trim();
        self.model_id.eq_ignore_ascii_case(INL_MULTIMODAL_MODEL)
            || (!configured.is_empty() && self.model_id.eq_ignore_ascii_case(configured))
    }

    /// A ready, loopback-reachable dense-text server whose model and dimension
    /// line up with the configured vector store.
    fn matches(&self, config: &KatokConfig) -> bool {
        self.is_ok()
            && self.serves_dense_text()
            && self.dimension == usize::from(config.vector_dimension)
            && self.serves_wanted_model(config)
    }

    /// Loopback base URL of the daemon.
    ///
    /// The address that answered `/health` is authoritative; the health body's
    /// own `host`/`port` is only a fallback (hand-built health), and it is still
    /// gated on a loopback host so embeddings can never leave this machine.
    pub(crate) fn base_url(&self) -> Option<String> {
        if let Some(address) = self.address {
            return Some(format!("http://{address}"));
        }
        let host = self.host.as_deref().unwrap_or("127.0.0.1");
        let port = self.port?;
        Some(format!(
            "http://{}",
            SocketAddr::new(loopback_ip(host)?, port)
        ))
    }

    /// One line describing a server discovery saw but did not accept.
    fn describe(&self) -> String {
        let status = if self.is_ok() {
            String::new()
        } else {
            format!(" status={}", self.status)
        };
        format!(
            "{} kind={} model={} dimension={}{status}",
            self.instance.as_deref().unwrap_or("?"),
            self.kind,
            self.model_id,
            self.dimension,
        )
    }
}

/// Find the daemon to embed through, or explain how to start one.
///
/// Candidates are discovered from `serve_root`'s `host`/`port` files, but the
/// answering server decides whether it matches: a stale directory whose port
/// nothing listens on is skipped, as is a non-loopback host, a `loading`
/// server, a mismatched kind, model, or dimension.
pub(crate) fn discover(serve_root: &Path, config: &KatokConfig) -> Result<InlHealth> {
    let mut directories = std::fs::read_dir(serve_root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    directories.sort();
    let mut candidates = Vec::new();
    let mut observed = Vec::new();
    for directory in directories {
        if !directory.is_dir() {
            continue;
        }
        let Some(address) = read_endpoint(&directory) else {
            continue;
        };
        let Some(health) = fetch_health(address) else {
            continue;
        };
        if health.matches(config) {
            candidates.push((address, health));
        } else {
            observed.push(health.describe());
        }
    }
    candidates.sort_by_key(|(address, _)| *address);
    candidates
        .into_iter()
        .map(|(_, health)| health)
        .next()
        .ok_or_else(|| missing_daemon_error(serve_root, config, &observed))
}

/// Read the loopback `host`/`port` a state directory records, if both are valid.
fn read_endpoint(directory: &Path) -> Option<SocketAddr> {
    let port: u16 = std::fs::read_to_string(directory.join("port"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let host = std::fs::read_to_string(directory.join("host"))
        .map(|host| host.trim().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string());
    Some(SocketAddr::new(loopback_ip(&host)?, port))
}

/// Accept only loopback hosts, matching `inl`'s own `parse_loopback_host`.
fn loopback_ip(host: &str) -> Option<IpAddr> {
    if host.eq_ignore_ascii_case("localhost") {
        return Some(IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    host.parse::<IpAddr>().ok().filter(IpAddr::is_loopback)
}

fn fetch_health(address: SocketAddr) -> Option<InlHealth> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(HEALTH_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(&format!("http://{address}/health")).call().ok()?;
    let mut health: InlHealth = response.body_mut().read_json().ok()?;
    health.address = Some(address);
    Some(health)
}

fn missing_daemon_error(serve_root: &Path, config: &KatokConfig, observed: &[String]) -> Error {
    let mut message = format!(
        "no running im-not-llm daemon matches the `inl` embedder under {} \
         (need a dense or multimodal server of `{}` or `{}` with {} dimensions): \
         start one with `inl serve --multimodal`",
        serve_root.display(),
        INL_MULTIMODAL_MODEL,
        config.embedder_model.trim(),
        config.vector_dimension,
    );
    if !observed.is_empty() {
        message.push_str("; saw ");
        message.push_str(&observed.join(", "));
    }
    Error::Embedding(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    fn health_json(
        status: &str,
        kind: &str,
        model: &str,
        dimension: usize,
        modalities: &[&str],
        port: u16,
    ) -> String {
        serde_json::json!({
            "status": status,
            "kind": kind,
            "model_id": model,
            "dimension": dimension,
            "modalities": modalities,
            "instance": format!("{kind}-{dimension}"),
            "host": "127.0.0.1",
            "port": port,
        })
        .to_string()
    }

    /// Serve a fixed JSON body for every request; the returned counter lets a
    /// test prove discovery talked to the server at all. The closure receives
    /// the bound port so a body can carry a real health answer's host/port.
    fn serve_health(body: impl FnOnce(u16) -> String) -> (u16, Arc<Mutex<usize>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
        let port = listener.local_addr().expect("listener address").port();
        let body = body(port);
        let hits = Arc::new(Mutex::new(0usize));
        let counted = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let reader = BufReader::new(stream.try_clone().expect("clone stream"));
                for line in reader.lines() {
                    match line {
                        Ok(line) if line.trim_end().is_empty() => break,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                }
                *counted.lock().expect("hit counter") += 1;
                let payload = body.as_bytes();
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(payload);
                let _ = stream.flush();
            }
        });
        (port, hits)
    }

    fn write_instance(serve_root: &Path, name: &str, host: &str, port: u16) {
        let directory = serve_root.join(name);
        std::fs::create_dir_all(&directory).expect("create instance directory");
        std::fs::write(directory.join("host"), format!("{host}\n")).expect("write host");
        std::fs::write(directory.join("port"), format!("{port}\n")).expect("write port");
    }

    fn config_for(model: &str, dimension: u16) -> KatokConfig {
        KatokConfig {
            embedder_model: model.to_string(),
            vector_dimension: dimension,
            ..KatokConfig::default()
        }
    }

    #[test]
    fn loopback_ip_accepts_only_loopback_hosts() {
        assert!(loopback_ip("127.0.0.1").is_some());
        assert!(loopback_ip("127.0.0.2").is_some());
        assert!(loopback_ip("::1").is_some());
        assert!(loopback_ip("LOCALHOST").is_some());
        assert!(loopback_ip("10.0.0.1").is_none());
        assert!(loopback_ip("0.0.0.0").is_none());
        assert!(loopback_ip("example.com").is_none());
    }

    #[test]
    fn base_url_ignores_a_non_loopback_health_body() {
        // A hand-built health body (no verified address) must not become an
        // endpoint embeddings are sent to when it claims a remote host.
        let mut health: InlHealth = serde_json::from_str(
            r#"{"status":"ok","kind":"multimodal","model_id":"google/embeddinggemma-2","dimension":768,"host":"10.0.0.1","port":80,"modalities":["text"]}"#,
        )
        .expect("parse health");
        assert!(health.address.is_none());
        assert_eq!(health.base_url(), None);

        health.address = Some("127.0.0.1:9".parse().expect("address"));
        assert_eq!(health.base_url().as_deref(), Some("http://127.0.0.1:9"));
    }

    #[test]
    fn discover_accepts_a_healthy_multimodal_embeddinggemma_daemon() {
        let home = tempfile::tempdir().expect("serve root");
        let (port, hits) = serve_health(|port| {
            health_json(
                "ok",
                "multimodal",
                INL_MULTIMODAL_MODEL,
                768,
                &["text", "image"],
                port,
            )
        });
        write_instance(home.path(), "multimodal-metal-f16", "127.0.0.1", port);

        let health = discover(home.path(), &KatokConfig::default()).expect("discover daemon");
        assert_eq!(health.model_id, INL_MULTIMODAL_MODEL);
        assert_eq!(health.dimension, 768);
        assert_eq!(
            health.base_url().expect("loopback endpoint"),
            format!("http://127.0.0.1:{port}")
        );
        assert_eq!(*hits.lock().expect("hit counter"), 1);
    }

    #[test]
    fn discover_prefers_the_answer_over_stale_and_mismatched_directories() {
        let home = tempfile::tempdir().expect("serve root");
        // A stale directory: the port file names nothing that listens.
        write_instance(home.path(), "stale", "127.0.0.1", 9);
        // A non-loopback host is skipped without a connection attempt.
        write_instance(home.path(), "remote", "10.0.0.1", 80);
        // A loading server is not ready.
        let (loading_port, _) = serve_health(|port| {
            health_json(
                "loading",
                "multimodal",
                INL_MULTIMODAL_MODEL,
                768,
                &["text"],
                port,
            )
        });
        write_instance(home.path(), "loading", "127.0.0.1", loading_port);
        // A sparse server produces no dense vector.
        let (sparse_port, _) = serve_health(|port| {
            health_json(
                "ok",
                "sparse",
                "nlpai-lab/KURE-Sparse-nano",
                36282,
                &["text"],
                port,
            )
        });
        write_instance(home.path(), "sparse", "127.0.0.1", sparse_port);
        // The one acceptable server.
        let (ok_port, _) = serve_health(|port| {
            health_json(
                "ok",
                "multimodal",
                INL_MULTIMODAL_MODEL,
                768,
                &["text"],
                port,
            )
        });
        write_instance(home.path(), "healthy", "127.0.0.1", ok_port);

        let health = discover(home.path(), &KatokConfig::default()).expect("discover daemon");
        assert_eq!(
            health.base_url().expect("loopback endpoint"),
            format!("http://127.0.0.1:{ok_port}")
        );
    }

    #[test]
    fn discover_reports_when_only_mismatched_servers_answer() {
        let home = tempfile::tempdir().expect("serve root");
        let (port, _) = serve_health(|port| {
            health_json(
                "ok",
                "multimodal",
                INL_MULTIMODAL_MODEL,
                512,
                &["text"],
                port,
            )
        });
        write_instance(home.path(), "wrong-dimension", "127.0.0.1", port);

        let error = discover(home.path(), &KatokConfig::default()).expect_err("no match");
        let message = error.to_string();
        assert!(message.contains("inl serve --multimodal"), "{message}");
        assert!(message.contains("dimension=512"), "{message}");
    }

    #[test]
    fn discover_matches_a_configured_dense_model() {
        let home = tempfile::tempdir().expect("serve root");
        let (port, _) = serve_health(|port| {
            health_json(
                "ok",
                "dense",
                "Qwen/Qwen3-Embedding-0.6B",
                1024,
                &["text"],
                port,
            )
        });
        write_instance(home.path(), "dense-qwen", "127.0.0.1", port);

        let config = config_for("Qwen/Qwen3-Embedding-0.6B", 1024);
        let health = discover(home.path(), &config).expect("discover daemon");
        assert_eq!(health.model_id, "Qwen/Qwen3-Embedding-0.6B");
    }

    #[test]
    fn discover_explains_a_missing_serve_root() {
        let home = tempfile::tempdir().expect("serve root");
        let error =
            discover(&home.path().join("absent"), &KatokConfig::default()).expect_err("no daemon");
        assert!(error.to_string().contains("inl serve --multimodal"));
    }
}
