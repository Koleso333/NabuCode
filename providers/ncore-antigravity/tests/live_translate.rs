use std::sync::Arc;

use futures_util::StreamExt;
use ncore::core::codec::{self, EncodeOptions};
use ncore::core::ir::*;
use ncore::core::{Format, SseParser};
use ncore::{CancelToken, Core, Endpoint, InboundRequest, TranslateBody, TranslateOptions};
use ncore_antigravity::{Account, Antigravity};
use serde_json::json;

const CLIENTS: [Format; 4] = [
    Format::AnthropicMessages,
    Format::OpenAiChat,
    Format::OpenAiResponses,
    Format::Gemini,
];

struct Provider {
    name: String,
    endpoint: Endpoint,
    model: String,
}

// `NABU_LIVE_<FMT>` = "base_url|key|model"; Antigravity comes from the ReProxy accounts file.
fn providers() -> Vec<Provider> {
    let mut out = Vec::new();
    for (var, format) in [
        ("NABU_LIVE_ANTHROPIC", Format::AnthropicMessages),
        ("NABU_LIVE_CHAT", Format::OpenAiChat),
        ("NABU_LIVE_RESPONSES", Format::OpenAiResponses),
    ] {
        let Ok(spec) = std::env::var(var) else {
            continue;
        };
        let mut it = spec.splitn(3, '|');
        let (url, key, model) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let auth = match format {
            Format::AnthropicMessages => ncore::Auth::Header {
                name: "x-api-key".into(),
                value: key.into(),
            },
            _ => ncore::Auth::Bearer(key.into()),
        };
        out.push(Provider {
            name: format!("{format:?}:{model}"),
            endpoint: Endpoint::new(format, url).with_auth(auth),
            model: model.into(),
        });
    }
    let path = format!(
        "{}/.reproxy/antigravity-accounts.json",
        std::env::var("HOME").unwrap()
    );
    if let Ok(raw) = std::fs::read(path) {
        let plugin = Arc::new(Antigravity::new(Account::parse_file(&raw).unwrap()));
        for model in ["gemini-3-flash", "claude-sonnet-4-6"] {
            out.push(Provider {
                name: format!("Antigravity:{model}"),
                endpoint: Antigravity::endpoint().with_plugin(plugin.clone()),
                model: model.into(),
            });
        }
    }
    if let Ok(only) = std::env::var("NABU_LIVE_ONLY") {
        out.retain(|p| p.name.contains(&only));
    }
    out
}

fn weather_tool() -> ToolDef {
    ToolDef::Function(Tool {
        name: "get_weather".into(),
        description: Some("Current weather for a city".into()),
        schema: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        strict: None,
        cache: None,
        ext: Ext::default(),
    })
}

fn inbound(client: Format, ir: &Request) -> InboundRequest {
    let e = codec::encode_request(
        client,
        ir,
        &EncodeOptions {
            default_max_tokens: Some(2048),
            ..Default::default()
        },
    )
    .unwrap();
    InboundRequest {
        method: "POST".into(),
        path_and_query: e.path,
        headers: vec![("content-type".into(), "application/json".into())],
        body: e.body,
    }
}

// Sends an IR request as `client` would, and decodes the answer with the client's own codec.
async fn ask(
    core: &Core,
    p: &Provider,
    client: Format,
    ir: &Request,
) -> Result<(Response, Vec<Loss>), String> {
    let opts = TranslateOptions {
        model: Some(p.model.clone()),
        response_model: Some("client-model".into()),
        encode: EncodeOptions {
            default_max_tokens: Some(2048),
            ..Default::default()
        },
        ..Default::default()
    };
    let resp = core
        .translate(&p.endpoint, inbound(client, ir), opts, &CancelToken::new())
        .await
        .map_err(|e| e.to_string())?;
    let status = resp.status;
    match resp.body {
        TranslateBody::Full { bytes, .. } => {
            if status >= 400 {
                return Err(format!("{status}: {}", String::from_utf8_lossy(&bytes)));
            }
            let r = codec::decode_response(client, &bytes)
                .map_err(|e| format!("{e}: {}", String::from_utf8_lossy(&bytes)))?;
            Ok((r, resp.losses))
        }
        TranslateBody::Stream(mut s) => {
            let mut bytes = Vec::new();
            while let Some(c) = s.next().await {
                bytes.extend_from_slice(&c.map_err(|e| e.to_string())?);
            }
            let mut parser = SseParser::new();
            let mut dec = codec::stream_decoder(client);
            let mut acc = Accumulator::new();
            for ev in parser.feed(&bytes) {
                for e in dec.feed(&ev).map_err(|e| e.to_string())? {
                    acc.push(&e);
                }
            }
            for e in dec.finish() {
                acc.push(&e);
            }
            if let Some(e) = acc.error() {
                return Err(format!("stream error: {}", e.message));
            }
            Ok((acc.finish(), resp.losses))
        }
    }
}

fn text_of(r: &Response) -> String {
    plain_text(&r.content)
}

#[tokio::test]
#[ignore = "talks to real providers with the user's keys"]
async fn live_matrix() {
    let core = ncore_tokio::core(None);
    let mut failures = Vec::new();
    for p in providers() {
        for client in CLIENTS {
            for stream in [false, true] {
                let ir = Request {
                    model: "client-model".into(),
                    system: vec![Block::text("Answer with exactly one lowercase word.")],
                    messages: vec![Message::new(Role::User, vec![Block::text("Say pong.")])],
                    max_tokens: Some(2048),
                    stream,
                    ..Request::default()
                };
                let label = format!("{client:?} -> {} stream={stream}", p.name);
                match ask(&core, &p, client, &ir).await {
                    Ok((r, _)) => {
                        let t = text_of(&r);
                        println!(
                            "{label}: {t:?} stop={:?} usage_in={:?} out={:?} model={:?}",
                            r.stop, r.usage.input_total, r.usage.output_total, r.model
                        );
                        if !t.to_lowercase().contains("pong")
                            || r.model.as_deref() != Some("client-model")
                        {
                            failures.push(format!("{label}: unexpected {t:?} model={:?}", r.model));
                        }
                    }
                    Err(e) => {
                        println!("{label}: ERROR {e}");
                        failures.push(format!("{label}: {e}"));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}

#[tokio::test]
#[ignore = "talks to real providers with the user's keys"]
async fn live_tool_round_trip() {
    let core = ncore_tokio::core(None);
    let mut failures = Vec::new();
    for p in providers() {
        for client in CLIENTS {
            let label = format!("{client:?} -> {}", p.name);
            let question = Message::new(
                Role::User,
                vec![Block::text(
                    "What is the weather in Paris? Use the tool, then answer in one short sentence.",
                )],
            );
            let mut ir = Request {
                model: "client-model".into(),
                messages: vec![question],
                tools: vec![weather_tool()],
                max_tokens: Some(2048),
                stream: true,
                ..Request::default()
            };
            let first = match ask(&core, &p, client, &ir).await {
                Ok((r, _)) => r,
                Err(e) => {
                    println!("{label}: ERROR {e}");
                    failures.push(format!("{label} call: {e}"));
                    continue;
                }
            };
            let calls: Vec<ToolCall> = first
                .content
                .iter()
                .filter_map(|b| match b {
                    Block::ToolCall(c) => Some(c.clone()),
                    _ => None,
                })
                .collect();
            if calls.is_empty() || first.stop != Some(StopReason::ToolUse) {
                failures.push(format!(
                    "{label}: no tool call, stop={:?} text={:?}",
                    first.stop,
                    text_of(&first)
                ));
                continue;
            }
            ir.messages
                .push(Message::new(Role::Assistant, first.content.clone()));
            ir.messages.push(Message::new(
                Role::User,
                calls
                    .iter()
                    .map(|c| {
                        Block::ToolResult(ToolResult {
                            call_id: c.id.clone(),
                            name: None,
                            content: vec![Block::text("18C and sunny")],
                            is_error: false,
                            cache: None,
                        })
                    })
                    .collect(),
            ));
            ir.stream = false;
            match ask(&core, &p, client, &ir).await {
                Ok((r, losses)) => {
                    let t = text_of(&r);
                    println!(
                        "{label}: call {}({}) -> {t:?} losses={:?}",
                        calls[0].name,
                        calls[0].args,
                        losses.iter().map(|l| &l.path).collect::<Vec<_>>()
                    );
                    if !t.contains("18") {
                        failures.push(format!("{label}: answer {t:?}"));
                    }
                }
                Err(e) => {
                    println!("{label}: ERROR {e}");
                    failures.push(format!("{label} answer: {e}"));
                }
            }
        }
    }
    assert!(failures.is_empty(), "failures:\n{}", failures.join("\n"));
}
