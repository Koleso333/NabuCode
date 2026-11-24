use std::collections::VecDeque;

use bytes::{Bytes, BytesMut};
use futures_util::{StreamExt, stream};
use ncore::core::{Format, ProviderError, SseEvent, SseParser};
use ncore::rt::BoxStream;
use ncore::{Error, HttpResponse, TransportError};
use serde_json::Value;

use crate::api::{MAX_BODY, fail};

fn terminal(event: &SseEvent) -> bool {
    serde_json::from_str::<Value>(&event.data)
        .ok()
        .and_then(|v| v["type"].as_str().map(str::to_owned))
        .is_some_and(|ty| {
            matches!(
                ty.as_str(),
                "response.completed" | "response.incomplete" | "response.failed" | "error"
            )
        })
}

struct State {
    body: BoxStream<'static, Result<Bytes, TransportError>>,
    parser: SseParser,
    line: BytesMut,
    ready: VecDeque<Result<Bytes, TransportError>>,
    pending: usize,
    done: bool,
}

/// Stops at a complete terminal SSE block even when upstream keeps HTTP alive.
fn bounded(
    body: BoxStream<'static, Result<Bytes, TransportError>>,
) -> BoxStream<'static, Result<Bytes, TransportError>> {
    let state = State {
        body,
        parser: SseParser::new(),
        line: BytesMut::new(),
        ready: VecDeque::new(),
        pending: 0,
        done: false,
    };
    Box::pin(stream::unfold(state, |mut state| async move {
        loop {
            if let Some(chunk) = state.ready.pop_front() {
                return Some((chunk, state));
            }
            if state.done {
                return None;
            }
            match state.body.next().await {
                Some(Ok(chunk)) => {
                    // Feed complete lines to avoid rescanning a growing JSON line on every network chunk.
                    for part in chunk.split_inclusive(|b| matches!(b, b'\r' | b'\n')) {
                        state.pending = state.pending.saturating_add(part.len());
                        if state.pending > MAX_BODY {
                            state.ready.push_back(Err(TransportError::new(
                                "Codex SSE event exceeds size limit",
                            )));
                            state.done = true;
                            break;
                        }
                        state.line.extend_from_slice(part);
                        if !part.last().is_some_and(|b| matches!(b, b'\r' | b'\n')) {
                            continue;
                        }
                        for event in state.parser.feed(&state.line.split()) {
                            state.pending = state.pending.saturating_sub(event.raw.len());
                            let end = terminal(&event);
                            state.ready.push_back(Ok(event.raw));
                            if end {
                                state.done = true;
                                break;
                            }
                        }
                        if state.done {
                            break;
                        }
                    }
                }
                Some(Err(error)) => {
                    state.ready.push_back(Err(error));
                    state.done = true;
                }
                None => {
                    state.ready.push_back(Err(TransportError::new(
                        "Codex stream ended before a terminal event",
                    )));
                    state.done = true;
                }
            }
            if state.done {
                state.body = Box::pin(stream::empty());
            }
        }
    }))
}

pub(crate) async fn adapt(
    mut response: HttpResponse,
    streaming: bool,
) -> Result<HttpResponse, Error> {
    if !(200..300).contains(&response.status) {
        return Ok(response);
    }
    if response.header("content-type").is_some_and(|v| {
        v.split(';')
            .next()
            .is_none_or(|v| !v.trim().eq_ignore_ascii_case("text/event-stream"))
    }) {
        return Err(fail("Codex returned a non-SSE inference response"));
    }
    if response.header("content-type").is_none() {
        response
            .headers
            .push(("content-type".into(), "text/event-stream".into()));
    }
    response.body = bounded(response.body);
    if streaming {
        return Ok(response);
    }
    let mut parser = SseParser::new();
    while let Some(chunk) = response.body.next().await {
        for event in parser.feed(&chunk.map_err(Error::from)?) {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            if !terminal(&event) {
                continue;
            }
            if value["type"] == "response.failed" || value["type"] == "error" {
                let error = if value["type"] == "error" {
                    value
                } else {
                    serde_json::json!({"error":value["response"]["error"]})
                };
                return Err(Error::Provider(ProviderError::parse(
                    Format::OpenAiResponses,
                    None,
                    Bytes::from(error.to_string()),
                )));
            }
            if !value["response"].is_object() {
                return Err(fail("Codex terminal event has no response"));
            }
            let bytes = Bytes::from(value["response"].to_string());
            response.headers.retain(|(k, _)| {
                ![
                    "content-type",
                    "content-length",
                    "content-encoding",
                    "transfer-encoding",
                ]
                .iter()
                .any(|name| k.eq_ignore_ascii_case(name))
            });
            response
                .headers
                .push(("content-type".into(), "application/json".into()));
            response.body = Box::pin(stream::once(async move { Ok(bytes) }));
            return Ok(response);
        }
    }
    Err(fail("Codex stream has no terminal response"))
}
