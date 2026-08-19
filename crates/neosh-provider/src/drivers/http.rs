//! Shared HTTP plumbing for the network drivers.
//!
//! SSE framing is implemented here rather than pulled from a crate because the requirement is
//! small and precise, and because the failure mode of getting it subtly wrong — a truncated final
//! event, a dropped multi-line payload — looks like a model bug rather than a parsing bug.

use bytes::Bytes;
use futures::StreamExt;
use reqwest::Response;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::ProviderError;

/// Decode an SSE body into its `data:` payloads.
///
/// Handles multi-line `data:` accumulation, ignores comments and other fields, and terminates on
/// the `[DONE]` sentinel that the OpenAI-shaped APIs use.
pub fn sse_data(
    resp: Response,
    cancel: CancellationToken,
) -> mpsc::Receiver<Result<String, ProviderError>> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(async move {
        let mut body = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut data = String::new();

        loop {
            let chunk: Option<Result<Bytes, reqwest::Error>> = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                c = body.next() => c,
            };
            let Some(chunk) = chunk else { break };
            match chunk {
                Ok(bytes) => buf.extend_from_slice(&bytes),
                Err(e) => {
                    let _ = tx.send(Err(ProviderError::Transport(e.to_string()))).await;
                    return;
                }
            }

            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=nl).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end_matches(['\r', '\n']);

                if line.is_empty() {
                    // Blank line dispatches the accumulated event.
                    if !data.is_empty() {
                        let payload = std::mem::take(&mut data);
                        if payload.trim() == "[DONE]" {
                            return;
                        }
                        if tx.send(Ok(payload)).await.is_err() {
                            return;
                        }
                    }
                    continue;
                }
                if let Some(rest) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
                }
                // `event:`, `id:`, `retry:` and comment lines carry nothing we need.
            }
        }

        // A body that ends without a trailing blank line still has a final event.
        if !data.is_empty() && data.trim() != "[DONE]" {
            let _ = tx.send(Ok(data)).await;
        }
    });
    rx
}

/// Turn a non-2xx response into an error carrying the body, which is where every provider puts the
/// actual reason.
pub async fn error_for_status(resp: Response) -> Result<Response, ProviderError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message").or(e.get("type")))
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.chars().take(400).collect());
    Err(ProviderError::BadResponse(format!("HTTP {status}: {detail}")))
}

/// Whether a status is worth retrying. Used by the agent layer's backoff.
pub fn retryable(status: u16) -> bool {
    matches!(status, 408 | 409 | 425 | 429) || (500..600).contains(&status)
}

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        // Long, because a high-effort turn legitimately takes minutes. Cancellation is the
        // mechanism for giving up early, not a short timeout.
        .timeout(std::time::Duration::from_secs(60 * 30))
        .connect_timeout(std::time::Duration::from_secs(20))
        .user_agent(concat!("neosh/", env!("CARGO_PKG_VERSION")))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn retryable_covers_overload_and_rate_limit_but_not_bad_request() {
        assert!(super::retryable(429));
        assert!(super::retryable(529));
        assert!(super::retryable(503));
        assert!(!super::retryable(400));
        assert!(!super::retryable(401));
        assert!(!super::retryable(404));
    }
}
