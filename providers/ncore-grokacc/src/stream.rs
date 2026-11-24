use std::collections::VecDeque;

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use ncore::core::{ProviderError, SseParser};
use ncore::rt::BoxStream;
use ncore::{Error, Format, HttpResponse, TransportError};
use serde_json::Value;

use crate::api::{LIMIT, fail};

fn terminal(data: &Value) -> bool {
    matches!(
        data["type"].as_str(),
        Some("response.completed" | "response.incomplete" | "response.failed" | "error")
    )
}

struct StreamState<F> {
    body: BoxStream<'static, Result<Bytes, TransportError>>,
    parser: SseParser,
    ready: VecDeque<Result<Bytes, TransportError>>,
    pending: usize,
    done: bool,
    observe: F,
}

pub(crate) async fn adapt(
    mut response: HttpResponse,
    streaming: bool,
    observe: impl FnOnce(&Value) + Send + 'static,
) -> Result<HttpResponse, Error> {
    if response.header("content-type").is_some_and(|header| {
        !header
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("text/event-stream")
    }) {
        return Err(fail("Grok returned a non-SSE inference response"));
    }
    let state = StreamState {
        body: response.body,
        parser: SseParser::new(),
        ready: VecDeque::new(),
        pending: 0,
        done: false,
        observe: Some(observe),
    };
    response.body = Box::pin(stream::unfold(state, |mut state| async move {
        while state.ready.is_empty() && !state.done {
            match state.body.next().await {
                Some(Ok(chunk)) => {
                    for part in chunk.split_inclusive(|b| matches!(b, b'\n' | b'\r')) {
                        state.pending = state.pending.saturating_add(part.len());
                        if state.pending > LIMIT {
                            state.ready.push_back(Err(TransportError::new(
                                "Grok SSE event exceeds size limit",
                            )));
                            state.done = true;
                            break;
                        }
                        for event in state.parser.feed(part) {
                            state.pending = state.pending.saturating_sub(event.raw.len());
                            if let Ok(value) = serde_json::from_str::<Value>(&event.data)
                                && terminal(&value)
                            {
                                if let Some(observe) = state.observe.take() {
                                    observe(&value);
                                }
                                state.done = true;
                            }
                            state.ready.push_back(Ok(event.raw));
                            if state.done {
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
                        "Grok stream ended without a terminal event",
                    )));
                    state.done = true;
                }
            }
            if state.done {
                state.body = Box::pin(stream::empty());
            }
        }
        state.ready.pop_front().map(|chunk| (chunk, state))
    }));
    if streaming {
        if response.header("content-type").is_none() {
            response
                .headers
                .push(("content-type".into(), "text/event-stream".into()));
        }
        return Ok(response);
    }

    let mut parser = SseParser::new();
    while let Some(chunk) = response.body.next().await {
        for event in parser.feed(&chunk?) {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            if !terminal(&value) {
                continue;
            }
            let body = &value["response"];
            if matches!(value["type"].as_str(), Some("response.failed" | "error")) {
                let data = if value["type"] == "error" {
                    value
                } else {
                    serde_json::json!({"error": body["error"]})
                };
                return Err(Error::Provider(ProviderError::parse(
                    Format::OpenAiResponses,
                    None,
                    Bytes::from(data.to_string()),
                )));
            }
            if !body.is_object() {
                return Err(fail("Grok terminal event has no response"));
            }
            let bytes = Bytes::from(body.to_string());
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
            response.body = Box::pin(stream::once(async { Ok(bytes) }));
            return Ok(response);
        }
    }
    Err(fail("Grok stream has no terminal response"))
}
