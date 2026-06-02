//! End-to-end integration test: real `llama-server` → `InferenceRouter`
//! → `LlamaCppSynthesizer` → `SummaryBundle`.
//!
//! Runtime-gated: the test compiles whenever the `http-client`
//! feature is enabled but becomes a no-op unless **both** of these
//! env vars are set:
//!
//! * `LLAMA_SERVER_BINARY` — absolute path to the `llama-server`
//!   executable (the `kennguy3n/llama.cpp@prism` build).
//! * `LLAMA_SERVER_MODEL` — absolute path to a GGUF model file the
//!   binary should load. Any tiny chat-tuned model works; the test
//!   asks the SLM to emit a fixed JSON shape constrained by GBNF.
//!
//! Optional knobs:
//!
//! * `LLAMA_SERVER_PORT` — TCP port for the spawned server.
//!   Defaults to a random ephemeral port via `0.0.0.0:0` discovery.
//! * `LLAMA_SERVER_READY_TIMEOUT_SECS` — how long to wait for the
//!   server's `/health` to return 200. Defaults to 90s.
//! * `LLAMA_SERVER_REQUEST_TIMEOUT_SECS` — `/completion` request
//!   timeout used by the `HttpLlamaServerClient`. Defaults to 180s.
//!
//! Wire-up rationale lives in the body comments — short version:
//! we own the process so the test is hermetic, and we kill it on
//! every exit path so a failing assertion doesn't strand a
//! background `llama-server`.

#![cfg(feature = "http-client")]

use std::io::{BufRead, BufReader, Read};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use evidence_store::ScopeId;
use inference_router::adapter::InferenceAdapter;
use inference_router::adapters::llama_cpp::LlamaServerClient;
use inference_router::{
    DeviceTier, FallbackAdapter, HttpLlamaServerClient, InferenceRouter, LlamaCppAdapter,
    RouterConfig,
};
use synthesis_pipeline::{
    LlamaCppSynthesizer, SummaryBundle, SynthesisInputs, SynthesisPipeline, SynthesisWindow,
};

const READY_TIMEOUT_SECS_DEFAULT: u64 = 90;
const REQUEST_TIMEOUT_SECS_DEFAULT: u64 = 180;

/// Owns the spawned `llama-server` so `Drop` always kills it,
/// even if the test panics partway through.
struct LlamaServerGuard {
    child: Child,
    server_url: String,
}

impl LlamaServerGuard {
    fn server_url(&self) -> &str {
        &self.server_url
    }
}

impl Drop for LlamaServerGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn synthesizes_summary_bundle_against_real_llama_server() {
    let Some((binary, model)) = read_required_env() else {
        eprintln!("skipping: set LLAMA_SERVER_BINARY and LLAMA_SERVER_MODEL to run the real \
             llama-server integration test"
        );
        return;
    };

    let port = parse_env_u16("LLAMA_SERVER_PORT").unwrap_or_else(pick_ephemeral_port);
    let ready_timeout = Duration::from_secs(parse_env_u64("LLAMA_SERVER_READY_TIMEOUT_SECS").unwrap_or(READY_TIMEOUT_SECS_DEFAULT),
    );
    let request_timeout = Duration::from_secs(parse_env_u64("LLAMA_SERVER_REQUEST_TIMEOUT_SECS").unwrap_or(REQUEST_TIMEOUT_SECS_DEFAULT),
    );

    let guard = spawn_llama_server(&binary, &model, port, ready_timeout);

    let http = HttpLlamaServerClient::with_timeout(guard.server_url(), request_timeout)
        .expect("http client build");
    assert!(http.ping(),
        "spawned llama-server should be reachable at {}",
        guard.server_url()
    );

    let cfg = RouterConfig::default().with_device_tier(DeviceTier::High);
    let llama = Box::new(LlamaCppAdapter::new(cfg.clone(), Box::new(http)));
    let fallback = Box::new(FallbackAdapter::default());
    let adapters: Vec<Box<dyn InferenceAdapter>> = vec![llama, fallback];
    let router = Arc::new(InferenceRouter::new(cfg, adapters));
    router.bootstrap();

    let synth = LlamaCppSynthesizer::new(router);
    let scope = ScopeId::new_v4();
    let now = Utc::now();
    let window =
        SynthesisWindow::new(scope, now - ChronoDuration::hours(1), now).expect("valid window");

    let inputs = SynthesisInputs {
        recap_seed: "vendor selection meeting".into(),
        observations: Vec::new(),
    };

    let object = synth
        .synthesize(&window, &inputs)
        .expect("synth via real llama-server");
    assert_eq!(object.window_id, window.id);

    // The grammar guarantees the bytes parse as SummaryBundle —
    // if this assertion fires the GBNF or the prompt drifted.
    let bundle: SummaryBundle =
        serde_json::from_slice(&object.payload).expect("payload should parse as SummaryBundle");
    assert!(!bundle.recap.trim().is_empty(),
        "real SLM should produce a non-empty recap, got `{}`",
        bundle.recap
    );
}

fn read_required_env() -> Option<(String, String)> {
    let bin = std::env::var("LLAMA_SERVER_BINARY").ok()?;
    let model = std::env::var("LLAMA_SERVER_MODEL").ok()?;
    if bin.trim().is_empty() || model.trim().is_empty() {
        return None;
    }
    Some((bin, model))
}

fn parse_env_u16(name: &str) -> Option<u16> {
    std::env::var(name).ok()?.parse().ok()
}

fn parse_env_u64(name: &str) -> Option<u64> {
    std::env::var(name).ok()?.parse().ok()
}

/// Bind to `127.0.0.1:0`, read back the port the OS assigned, and
/// drop the listener so `llama-server` can grab it. There is a
/// small TOCTOU window where another process could claim the port,
/// but it's negligible for the dev-loop integration test.
fn pick_ephemeral_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let port = l.local_addr().expect("local_addr").port();
    drop(l);
    port
}

fn spawn_llama_server(binary: &str,
    model: &str,
    port: u16,
    ready_timeout: Duration,
) -> LlamaServerGuard {
    let mut child = Command::new(binary)
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "-m",
            model,
            // Small context + cap predict so smoke runs stay quick
            // on CPU-only test hosts. The synthesis test only needs
            // enough room for one `SummaryBundle` JSON.
            "-c",
            "1024",
            "-n",
            "256",
            "--log-disable",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn llama-server");

    // Drain stdout / stderr so a chatty server can't deadlock on a
    // full pipe buffer while we poll /health.
    if let Some(stdout) = child.stdout.take() {
        drain_pipe("llama-server stdout", stdout);
    }
    if let Some(stderr) = child.stderr.take() {
        drain_pipe("llama-server stderr", stderr);
    }

    let server_url = format!("http://127.0.0.1:{port}");
    let guard = LlamaServerGuard { child, server_url };
    wait_for_ready(guard.server_url(), ready_timeout);
    guard
}

fn wait_for_ready(server_url: &str, timeout: Duration) {
    // Use the production `HttpLlamaServerClient::ping()` so this
    // helper exercises the same code path the synthesiser will hit
    // at runtime. Short per-attempt timeout keeps the loop snappy
    // while model load takes its sweet time.
    let probe = HttpLlamaServerClient::with_timeout(server_url, Duration::from_secs(2))
        .expect("probe client build");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if probe.ping() {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    panic!("llama-server at {server_url} did not become ready within {timeout:?}");
}

fn drain_pipe<R: Read + Send + 'static>(prefix: &'static str, r: R) {
    std::thread::spawn(move || {
        let reader = BufReader::new(r);
        for line in reader.lines().map_while(std::result::Result::ok) {
            eprintln!("[{prefix}] {line}");
        }
    });
}
