//! Request diagnostics remain useful when durable analytics is disabled and the
//! dashboard ring has evicted the original request.
use super::*;
use crate::config::{Config, KeyEntry};
use parking_lot::Mutex;
use std::io::Write;

// Tracing's callsite interest cache is process-wide. Exercise emitted records
// in a fresh test process with a global subscriber, as the service initializes
// logging, so unrelated parallel tests cannot disable a request callsite.
fn run_in_isolated_process(test: &str) -> bool {
    let name = format!("proxy::diagnostic_tests::{test}");
    const CHILD: &str = "FUSEBOX_DIAGNOSTIC_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name.as_str()) {
        return false;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env(CHILD, &name)
        .output()
        .expect("start isolated diagnostics test");
    assert!(
        output.status.success(),
        "isolated diagnostics failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    true
}

#[derive(Clone, Default)]
struct LogBuffer(Arc<Mutex<Vec<u8>>>);
impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl LogBuffer {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().clone()).unwrap()
    }
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        let writer = self.clone();
        tracing_subscriber::fmt().with_ansi(false).without_time().with_writer(move || writer.clone()).finish()
    }
}

fn app() -> Arc<App> {
    let app = App::new(
        Config {
            auth_dir: "/nonexistent".into(),
            codex_api_key: vec![KeyEntry { api_key: "mock-only".into(), ..Default::default() }],
            ..Default::default()
        },
        "/nonexistent/config.yaml".into(),
    );
    assert!(app.usage.is_none());
    app
}
fn tracker(app: &Arc<App>) -> Tracker {
    let mut tracker = Tracker::new(app, Format::Responses, true, "http", "gpt-6.1-sol");
    tracker.attempt(&app.pool.all()[0]);
    tracker
}

#[tokio::test]
async fn absent_and_explicit_zero_usage_remain_distinct_without_analytics() {
    let app = app();
    let mut missing = tracker(&app);
    missing.observe_wire(&json!({"usage":{}}));
    missing.finish(200, &Usage::default(), None);
    let mut zero = tracker(&app);
    zero.observe_wire(&json!({"usage":{"input_tokens":0,"output_tokens":0,
        "input_tokens_details":{"cached_tokens":0,"cache_creation_tokens":0}}}));
    zero.finish(200, &Usage::default(), None);
    let mut cancelled = tracker(&app);
    cancelled.observe_wire(&json!({"usage":{"output_tokens":4}}));
    cancelled.stream_usage.output = 4;
    drop(cancelled);
    drop(tracker(&app));
    let logs = app.stats.recent.lock();
    assert_eq!(logs[0].usage_completeness, "missing");
    assert_eq!(logs[1].usage_completeness, "complete");
    assert_eq!(logs[2].usage_completeness, "partial");
    assert_eq!(logs[2].output_tokens, 4);
    assert_eq!(logs[3].usage_completeness, "missing");
    for log in [&logs[2], &logs[3]] {
        assert_eq!(log.status, 499);
        assert_eq!(log.failure_kind, Some("downstream_disconnect"));
    }
}

#[tokio::test]
async fn fallback_keeps_diagnostic_snapshot_without_becoming_logical_final() {
    let app = app();
    let mut tracker = tracker(&app);
    let tap = tracker.usage_tap().unwrap();
    tracker.observe_wire(&json!({"response":{"id":"provider-response", "usage":{"output_tokens":3}}}));
    tracker.diagnostic_failure(crate::diagnostics::Failure::classified("rate_limit_or_quota"));
    tracker.finish_fallback(429, &Usage { output: 3, ..Default::default() }, Some("quota exhausted".into()));
    let log = app.stats.recent.lock()[0].clone();
    assert_eq!(log.usage_completeness, "partial");
    assert_eq!(log.failure_kind, Some("rate_limit_or_quota"));
    assert!(!tracker.logical_final);
    assert_eq!(tap.snapshot(Some(429), None, tracker.logical_final).logical_success, None);
    assert!(tracker.usage_tap().is_none());
}

#[tokio::test]
async fn journal_keeps_sanitized_request_details_after_ring_eviction() {
    if run_in_isolated_process("journal_keeps_sanitized_request_details_after_ring_eviction") {
        return;
    }
    let app = app();
    let buffer = LogBuffer::default();
    tracing::subscriber::set_global_default(buffer.subscriber()).unwrap();
    let secret = "private-prompt Bearer sk-token https://private.test/?key=secret";
    let mut first = tracker(&app);
    first.log.account = secret.into();
    first.log.account_id = secret.into();
    first.log.session_id = Some(secret.into());
    first.log.model = secret.into();
    first.first_token();
    first.observe_wire(&json!({"response":{"id":secret,"usage":{"output_tokens":7}}}));
    first.diagnostic_failure(crate::diagnostics::Failure {
        kind: "upstream_body_read",
        causes: vec!["body_decode", "incomplete_message"],
    });
    first.finish(502, &Usage { output: 7, ..Default::default() }, Some(secret.into()));
    for _ in 0..300 {
        tracker(&app).finish(200, &Usage::default(), None);
    }
    assert_eq!(app.stats.recent.lock().len(), 300);
    assert_eq!(app.stats.recent.lock()[0].id, 2);
    let journal = buffer.text();
    assert_eq!(journal.matches("request completed").count(), 301);
    let recorded_ids: Vec<u64> = journal
        .lines()
        .filter(|line| line.contains("request completed"))
        .map(|line| line.split("request_id=").nth(1).unwrap().split_whitespace().next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(recorded_ids, (1..=301).collect::<Vec<_>>());
    let failed = journal.lines().find(|line| line.contains("upstream_body_read")).unwrap();
    for field in [
        "request_id=1",
        "status=502",
        "transport=\"http\"",
        "attempts=1",
        "ttft_ms=Some(",
        "usage_completeness=\"partial\"",
        "body_decode > incomplete_message",
        "output_tokens=7",
        "reported_input_tokens=None",
        "reported_output_tokens=Some(7)",
    ] {
        assert!(failed.contains(field), "missing {field}: {failed}");
    }
    for fragment in ["private-prompt", "Bearer", "sk-token", "https://", "key=secret", "\u{1b}["] {
        assert!(!journal.contains(fragment), "journal exposed {fragment}");
    }
}

#[tokio::test]
async fn broken_http_body_records_sanitized_source_chain() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    if run_in_isolated_process("broken_http_body_records_sanitized_source_chain") {
        return;
    }
    let app = app();
    let buffer = LogBuffer::default();
    tracing::subscriber::set_global_default(buffer.subscriber()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let peer = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        assert!(socket.read(&mut request).await.unwrap() > 0);
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\n\r\ndata: {}\n\n",
            )
            .await
            .unwrap();
        socket.shutdown().await.unwrap();
    });
    let response = reqwest::get(format!("http://{address}/private?token=secret")).await.unwrap();
    let mut stream = passthrough_stream(response, Format::Responses, tracker(&app), false);
    while stream.next().await.is_some() {}
    peer.await.unwrap();
    let logs = app.stats.recent.lock();
    assert_eq!(logs[0].status, 502);
    assert_eq!(logs[0].failure_kind, Some("upstream_body_read"));
    assert_eq!(logs[0].usage_completeness, "missing");
    let journal = buffer.text();
    assert!(journal.contains("body_decode"), "{journal}");
    assert!(journal.contains("unexpected_eof"), "{journal}");
    assert!(!journal.contains("token=secret"));
}
