//! Durable request diagnostics contain categories and fingerprints, never bodies
//! or arbitrary error strings (which can embed credentials, URLs and prompts).
use std::error::Error;
use std::future::Future;
use std::sync::Arc;

use parking_lot::Mutex;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub struct Failure {
    pub kind: &'static str,
    pub causes: Vec<&'static str>,
}

impl Failure {
    pub fn classified(kind: &'static str) -> Self {
        Self { kind, causes: vec![] }
    }

    pub fn http(kind: &'static str, error: &reqwest::Error) -> Self {
        let kind = if error.is_timeout() { "upstream_timeout" } else { kind };
        Self { kind, causes: cause_chain(error) }
    }
}

pub fn cause_chain(error: &(dyn Error + 'static)) -> Vec<&'static str> {
    let mut next = Some(error);
    let mut causes = Vec::new();
    while let Some(error) = next.filter(|_| causes.len() < 8) {
        let category = if let Some(error) = error.downcast_ref::<reqwest::Error>() {
            if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connect"
            } else if error.is_decode() {
                "body_decode"
            } else if error.is_body() {
                "body_read"
            } else if error.is_redirect() {
                "redirect"
            } else if error.is_builder() {
                "request_build"
            } else if error.is_status() {
                "http_status"
            } else {
                "http_request"
            }
        } else if let Some(error) = error.downcast_ref::<std::io::Error>() {
            match error.kind() {
                std::io::ErrorKind::TimedOut => "timeout",
                std::io::ErrorKind::ConnectionReset => "connection_reset",
                std::io::ErrorKind::ConnectionAborted => "connection_aborted",
                std::io::ErrorKind::ConnectionRefused => "connection_refused",
                std::io::ErrorKind::BrokenPipe => "broken_pipe",
                std::io::ErrorKind::UnexpectedEof => "unexpected_eof",
                std::io::ErrorKind::InvalidData => "invalid_data",
                _ => "io_error",
            }
        } else if error.downcast_ref::<rustls::Error>().is_some() {
            "tls_error"
        } else if let Some(error) = error.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
            use tokio_tungstenite::tungstenite::Error as WsError;
            match error {
                WsError::Io(_) => "websocket_io",
                WsError::Protocol(_) => "websocket_protocol",
                WsError::Http(_) | WsError::HttpFormat(_) => "websocket_handshake",
                WsError::ConnectionClosed | WsError::AlreadyClosed => "connection_closed",
                WsError::Tls(_) => "tls_error",
                WsError::Capacity(_) | WsError::WriteBufferFull(_) => "websocket_capacity",
                WsError::Utf8(_) => "invalid_utf8",
                WsError::Url(_) => "websocket_url",
                WsError::AttackAttempt => "websocket_protocol",
            }
        } else {
            // Unknown library errors have no stable public types. Match a small
            // vocabulary while retaining none of their potentially sensitive text.
            let text = error.to_string().to_ascii_lowercase();
            [
                ("connection reset", "connection_reset"),
                ("incomplete message", "incomplete_message"),
                ("unexpected eof", "unexpected_eof"),
                ("broken pipe", "broken_pipe"),
                ("timed out", "timeout"),
                ("connection closed", "connection_closed"),
                ("connection refused", "connection_refused"),
                ("certificate", "tls_certificate"),
                ("dns", "dns_error"),
                ("http2", "http2_error"),
                ("http/2", "http2_error"),
                ("decode", "decode_error"),
            ]
            .into_iter()
            .find_map(|(needle, category)| text.contains(needle).then_some(category))
            .unwrap_or("unclassified_cause")
        };
        causes.push(category);
        next = error.source();
    }
    causes
}

pub fn failure_kind(status: u16, message: Option<&str>) -> Option<&'static str> {
    if status < 400 {
        return None;
    }
    if status == 499 {
        return Some("downstream_disconnect");
    }
    let message = message.unwrap_or_default();
    Some(if message == "upstream ended before completing the response" {
        "upstream_incomplete"
    } else if message.starts_with("upstream stream error:") || message.starts_with("upstream response read failed:") {
        "upstream_body_read"
    } else if message.starts_with("upstream connection failed:") {
        "upstream_connect"
    } else if message == "upstream returned an invalid JSON response" {
        "upstream_invalid_response"
    } else if message.starts_with("token refresh failed:") {
        "credential_refresh"
    } else {
        match status {
            401 | 403 => "authentication_or_permission",
            429 => "rate_limit_or_quota",
            500..=599 => "upstream_error",
            _ => "request_rejected",
        }
    })
}

pub fn fingerprint(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.is_empty()).map(|value| hex::encode(&Sha256::digest(value.as_bytes())[..16]))
}

pub fn model_label(model: &str) -> &str {
    if model.len() <= 128 && model.bytes().all(|b| b.is_ascii_alphanumeric() || b"-._/".contains(&b)) {
        model
    } else {
        "redacted"
    }
}

pub fn record(
    log: &crate::state::RequestLog,
    capture: Option<&crate::usage::types::Observation>,
    failure: Option<&Failure>,
) {
    let outcome = if log.status == 499 {
        "cancelled"
    } else if log.status < 400 {
        "success"
    } else {
        "failed"
    };
    let account = fingerprint(Some(&log.account_id));
    let session = fingerprint(log.session_id.as_deref());
    let upstream_request = fingerprint(capture.and_then(|o| o.provider_request_id.as_deref()));
    let response = fingerprint(capture.and_then(|o| o.response_id.as_deref()));
    let logical_request = fingerprint(capture.and_then(|o| o.logical_request_id.as_deref()));
    let causes = failure.map(|f| f.causes.join(" > ")).unwrap_or_default();
    macro_rules! emit {
        ($level:expr) => {
            tracing::event!(target: "fusebox::request", $level,
                request_id = log.id, logical_request = ?logical_request, started_at = %log.ts,
                outcome, status = log.status, failure_kind = log.failure_kind.unwrap_or("none"),
                error_causes = causes, transport = log.transport, stream = log.stream,
                client = log.client, client_app = log.client_app.unwrap_or("unknown"),
                provider = log.provider, model = model_label(&log.model),
                account = ?account, session = ?session,
                upstream_request = ?upstream_request, upstream_response = ?response,
                attempts = log.attempts, latency_ms = log.latency_ms, ttft_ms = ?log.ttft_ms,
                input_tokens = log.input_tokens, output_tokens = log.output_tokens, cached_tokens = log.cache_tokens,
                usage_completeness = log.usage_completeness,
                reported_input_tokens = ?capture.and_then(|o| o.tokens.input),
                reported_output_tokens = ?capture.and_then(|o| o.tokens.output),
                reported_cache_read_tokens = ?capture.and_then(|o| o.tokens.cache_read),
                reported_cache_write_tokens = ?capture.and_then(|o| o.tokens.cache_write),
                reported_input_total = ?capture.and_then(|o| o.numeric_metadata.get("input_total").copied()),
                session_source = log.session_source.unwrap_or("none"), routing_strategy = ?log.routing_strategy,
                routing_reason = log.routing_reason.unwrap_or("unassigned"), routing_warning = log.routing_warning.unwrap_or("none"),
                "request completed");
        };
    }
    if log.status >= 500 {
        emit!(tracing::Level::WARN);
    } else {
        emit!(tracing::Level::INFO);
    }
}

#[derive(Clone, Default)]
pub struct Downstream(Arc<Mutex<Option<&'static str>>>);

impl Downstream {
    pub fn set(&self, kind: &'static str) {
        *self.0.lock() = Some(kind);
    }
    pub fn kind(&self) -> &'static str {
        self.0.lock().unwrap_or("downstream_disconnect")
    }
    pub fn current() -> Self {
        DOWNSTREAM.try_with(Clone::clone).unwrap_or_default()
    }
}

tokio::task_local! { static DOWNSTREAM: Downstream; }

pub async fn downstream_scope<T>(state: Downstream, future: impl Future<Output = T>) -> T {
    DOWNSTREAM.scope(state, future).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_causes_and_identifiers_never_contain_arbitrary_error_text() {
        let secret = "Bearer sk-private https://example.test/?token=secret private-prompt@example.test";
        let error = std::io::Error::new(std::io::ErrorKind::ConnectionReset, secret);
        assert_eq!(cause_chain(&error)[0], "connection_reset");
        let rendered = format!("{:?} {:?}", cause_chain(&error), fingerprint(Some(secret)));
        for fragment in ["Bearer", "sk-private", "https", "token=", "private-prompt"] {
            assert!(!rendered.contains(fragment));
        }
        assert_eq!(model_label(secret), "redacted");
        assert_eq!(model_label("gpt-6.1-sol"), "gpt-6.1-sol");
    }
}
