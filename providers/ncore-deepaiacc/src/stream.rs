use bytes::Bytes;
use futures_util::StreamExt;
use ncore::core::{
    SseParser, codec,
    ir::{Accumulator, StreamEvent},
};
use ncore::{Error, Format, HttpResponse};
use serde_json::Value;

use crate::{api, usage};

pub(crate) async fn collect(
    format: Format,
    mut response: HttpResponse,
) -> Result<HttpResponse, Error> {
    let mut parser = SseParser::new();
    let mut decoder = codec::stream_decoder(format);
    let mut accumulator = Accumulator::new();
    let mut native_usage = Value::Null;
    let mut terminal = None;
    let mut size = 0usize;
    while let Some(chunk) = response.body.next().await {
        let chunk = chunk.map_err(Error::from)?;
        size = size.saturating_add(chunk.len());
        if size > 32 * 1024 * 1024 {
            return Err(api::fail("DeepAI stream exceeds aggregation limit"));
        }
        for mut event in parser.feed(&chunk) {
            let mut bytes = Bytes::copy_from_slice(event.data.as_bytes());
            api::normalize_error(&mut bytes, None);
            if let Some(sample) = usage::sample(&bytes) {
                usage::merge(&mut native_usage, &sample.usage);
            }
            if format == Format::OpenAiResponses && event.has_data() {
                let value: Value = serde_json::from_slice(&bytes)
                    .map_err(|_| api::fail("invalid DeepAI Responses event"))?;
                if matches!(
                    value["type"].as_str(),
                    Some("response.completed" | "response.incomplete")
                ) {
                    let response = &value["response"];
                    let expected = if value["type"] == "response.completed" {
                        "completed"
                    } else {
                        "incomplete"
                    };
                    if !response.is_object()
                        || !response["output"].is_array()
                        || response["status"] != expected
                    {
                        return Err(api::fail("invalid terminal DeepAI response"));
                    }
                    if !response["error"].is_null() {
                        return Err(Error::Provider(api::provider_error(None, response)));
                    }
                    terminal = Some(response.clone());
                }
            }
            if bytes.as_ref() != event.data.as_bytes() {
                event.set_data(
                    std::str::from_utf8(&bytes)
                        .map_err(|_| api::fail("invalid DeepAI stream encoding"))?,
                );
            }
            for item in decoder.feed(&event).map_err(|e| api::fail(e.0))? {
                if let StreamEvent::Error(error) = item {
                    return Err(Error::Provider(error));
                }
                accumulator.push(&item);
            }
        }
    }
    if !parser.finish().is_empty() {
        return Err(api::fail("DeepAI stream ended inside an event"));
    }
    if !accumulator.is_done() || accumulator.response().stop.is_none() {
        return Err(api::fail(
            "DeepAI stream ended without a completed response",
        ));
    }
    let mut value = if format == Format::OpenAiResponses {
        terminal.ok_or_else(|| api::fail("DeepAI stream omitted its terminal response"))?
    } else {
        let (bytes, _) = codec::encode_response(format, &accumulator.finish());
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| api::fail("invalid collected DeepAI response"))?
    };
    if native_usage.is_object() {
        value["usage"] = native_usage;
    } else {
        value.as_object_mut().unwrap().remove("usage");
    }
    let bytes = Bytes::from(value.to_string());
    response.headers.retain(|(name, _)| {
        !matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "content-length" | "content-encoding"
        )
    });
    response
        .headers
        .push(("content-type".into(), "application/json".into()));
    response.body = Box::pin(futures_util::stream::once(async move { Ok(bytes) }));
    Ok(response)
}
