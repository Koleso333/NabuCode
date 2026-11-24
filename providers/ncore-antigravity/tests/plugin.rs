use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::StreamExt;
use ncore::rt::{BoxFuture, header};
use ncore::{
    CancelToken, Core, HttpRequest, HttpResponse, InboundRequest, PassthroughBody,
    PassthroughOptions, Transport, TransportError,
};
use ncore_antigravity::{Account, Antigravity};
use serde_json::Value;

/// Fake Google: token endpoint plus Cloud Code that rate-limits the first account.
#[derive(Default)]
struct FakeGoogle {
    seen: Mutex<Vec<HttpRequest>>,
}

fn reply(status: u16, body: String) -> HttpResponse {
    let chunks: Vec<_> = body
        .into_bytes()
        .chunks(9)
        .map(|c| Ok(Bytes::copy_from_slice(c)))
        .collect();
    HttpResponse {
        status,
        headers: vec![],
        body: Box::pin(futures_util::stream::iter(chunks)),
    }
}

impl Transport for FakeGoogle {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        self.seen.lock().unwrap().push(req.clone());
        Box::pin(async move {
            if req.url == "https://oauth2.googleapis.com/token" {
                let body = String::from_utf8(req.body.to_vec()).unwrap();
                let who = if body.contains("refresh_token=r1") {
                    "a1"
                } else {
                    "a2"
                };
                return Ok(reply(
                    200,
                    format!(r#"{{"access_token":"tok-{who}","expires_in":3599}}"#),
                ));
            }
            let auth = header(&req.headers, "authorization").unwrap().to_owned();
            if auth == "Bearer tok-a1" {
                return Ok(reply(
                    429,
                    r#"{"error":{"code":429,"message":"quota","status":"RESOURCE_EXHAUSTED"}}"#
                        .into(),
                ));
            }
            let inner = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":1},"modelVersion":"gemini-x","responseId":"r"}"#;
            if req.url.ends_with("?alt=sse") {
                Ok(reply(
                    200,
                    format!("data: {{\"response\": {inner}, \"traceId\": \"t\"}}\r\n\r\n"),
                ))
            } else {
                Ok(reply(
                    200,
                    format!(r#"{{"response":{inner},"traceId":"t"}}"#),
                ))
            }
        })
    }
}

fn accounts() -> Vec<Account> {
    ["a1", "a2"]
        .iter()
        .enumerate()
        .map(|(i, e)| Account {
            email: e.to_string(),
            refresh_token: format!("r{}|p|managed-{e}", i + 1),
            project_id: None,
            enabled: true,
        })
        .collect()
}

fn inbound(method: &str) -> InboundRequest {
    let query = if method == "streamGenerateContent" {
        "?alt=sse&key=client"
    } else {
        ""
    };
    InboundRequest {
        method: "POST".into(),
        path_and_query: format!("/v1beta/models/gemini-3.1-pro-low:{method}{query}"),
        headers: vec![
            ("x-goog-api-key".into(), "client".into()),
            ("user-agent".into(), "client-ua".into()),
        ],
        body: Bytes::from_static(br#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#),
    }
}

#[tokio::test]
async fn stream_fails_over_and_is_unwrapped() {
    let google = Arc::new(FakeGoogle::default());
    let plugin = Arc::new(Antigravity::new(accounts()));
    let core = Core::builder(google.clone()).build();
    let ep = Antigravity::endpoint().with_plugin(plugin);
    let resp = core
        .passthrough(
            &ep,
            inbound("streamGenerateContent"),
            PassthroughOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status, 200);
    let PassthroughBody::Stream(mut s) = resp.body else {
        panic!("expected stream")
    };
    let mut out = Vec::new();
    while let Some(c) = s.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("data: {\"candidates\""), "{text}");
    assert!(text.ends_with("}\r\n\r\n"));
    let obs = s.observation();
    assert_eq!(obs.model.as_deref(), Some("gemini-x"));
    assert_eq!(obs.usage.input_total, Some(5));

    let seen = google.seen.lock().unwrap();
    let urls: Vec<_> = seen.iter().map(|r| r.url.as_str()).collect();
    assert_eq!(
        urls,
        [
            "https://oauth2.googleapis.com/token",
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse",
            "https://oauth2.googleapis.com/token",
            "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse",
        ]
    );
    let last = &seen[3];
    assert_eq!(header(&last.headers, "x-goog-api-key"), None);
    assert!(
        header(&last.headers, "user-agent")
            .unwrap()
            .starts_with("antigravity/")
    );
    let env: Value = serde_json::from_slice(&last.body).unwrap();
    assert_eq!(env["project"], "managed-a2");
    assert_eq!(env["model"], "gemini-3.1-pro-low");
    assert_eq!(env["request"]["contents"][0]["parts"][0]["text"], "hello");
    assert_eq!(env["request"]["sessionId"].as_str().unwrap().len(), 32);
}

#[tokio::test]
async fn full_body_is_unwrapped_and_token_cached() {
    let google = Arc::new(FakeGoogle::default());
    let plugin = Arc::new(Antigravity::new(accounts()));
    let core = Core::builder(google.clone()).build();
    let ep = Antigravity::endpoint().with_plugin(plugin);
    for _ in 0..2 {
        let resp = core
            .passthrough(
                &ep,
                inbound("generateContent"),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let PassthroughBody::Full { bytes, observation } = resp.body else {
            panic!("expected full body")
        };
        assert!(bytes.starts_with(br#"{"candidates""#));
        assert_eq!(observation.stop_reason.as_deref(), Some("STOP"));
    }
    let seen = google.seen.lock().unwrap();
    let refreshes = seen.iter().filter(|r| r.url.contains("oauth2")).count();
    assert_eq!(refreshes, 2, "one refresh per account, then cached");
}

#[tokio::test]
#[ignore = "uses real accounts from ~/.reproxy/antigravity-accounts.json"]
async fn live() {
    let path = std::env::var("ANTIGRAVITY_ACCOUNTS").unwrap_or_else(|_| {
        format!(
            "{}/.reproxy/antigravity-accounts.json",
            std::env::var("HOME").unwrap()
        )
    });
    let accounts = Account::parse_file(&std::fs::read(path).unwrap()).unwrap();
    let plugin = Arc::new(Antigravity::new(accounts));
    let core = ncore_tokio::core(None);
    let ep = Antigravity::endpoint().with_plugin(plugin);
    let models = std::env::var("ANTIGRAVITY_MODELS")
        .unwrap_or_else(|_| "gemini-3-flash,claude-sonnet-4-6".into());
    for model in models.split(',') {
        for method in ["streamGenerateContent", "generateContent"] {
            let query = if method == "streamGenerateContent" {
                "?alt=sse"
            } else {
                ""
            };
            let req = InboundRequest {
                method: "POST".into(),
                path_and_query: format!("/v1beta/models/{model}:{method}{query}"),
                headers: vec![("content-type".into(), "application/json".into())],
                body: Bytes::from_static(
                    br#"{"contents":[{"role":"user","parts":[{"text":"Reply with one word: pong"}]}],"generationConfig":{"maxOutputTokens":2048}}"#,
                ),
            };
            let resp = core
                .passthrough(&ep, req, PassthroughOptions::default(), &CancelToken::new())
                .await
                .unwrap();
            let status = resp.status;
            let (text, obs) = match resp.body {
                PassthroughBody::Stream(mut s) => {
                    let mut out = Vec::new();
                    while let Some(c) = s.next().await {
                        out.extend_from_slice(&c.unwrap());
                    }
                    (String::from_utf8_lossy(&out).into_owned(), s.observation())
                }
                PassthroughBody::Full { bytes, observation } => {
                    (String::from_utf8_lossy(&bytes).into_owned(), observation)
                }
            };
            let docs: Vec<Value> = match serde_json::from_str::<Value>(&text) {
                Ok(v) => vec![v],
                Err(_) => text
                    .lines()
                    .filter_map(|l| l.strip_prefix("data: "))
                    .filter_map(|l| serde_json::from_str(l).ok())
                    .collect(),
            };
            let answer: String = docs
                .into_iter()
                .flat_map(|v| {
                    v["candidates"][0]["content"]["parts"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                })
                .filter(|p| p["thought"].as_bool() != Some(true))
                .filter_map(|p| p["text"].as_str().map(str::to_owned))
                .collect();
            println!(
                "{model} {method}: {status} answer={answer:?} model={:?} stop={:?} usage={:?} error={:?}",
                obs.model,
                obs.stop_reason,
                obs.usage,
                obs.error.as_ref().map(|e| &e.message)
            );
            assert_eq!(status, 200, "{text}");
            assert!(!answer.is_empty(), "{text}");
        }
    }
}

#[tokio::test]
#[ignore = "uses real accounts from ~/.reproxy/antigravity-accounts.json"]
async fn live_models() {
    let path = format!(
        "{}/.reproxy/antigravity-accounts.json",
        std::env::var("HOME").unwrap()
    );
    let accounts = Account::parse_file(&std::fs::read(path).unwrap()).unwrap();
    let transport = ncore_tokio::HyperTransport::new();
    let endpoints = ncore_antigravity::Config::default().endpoints;
    for (i, a) in accounts.iter().enumerate() {
        match ncore_antigravity::quota::fetch_models(&transport, a, &endpoints).await {
            Ok(models) => {
                let list: Vec<String> = models
                    .iter()
                    .map(|m| format!("{}={:?}", m.model, m.remaining))
                    .collect();
                println!("account #{i}: {}", list.join(" "));
            }
            Err(e) => println!("account #{i}: error {e}"),
        }
    }
}

#[tokio::test]
#[ignore = "uses real accounts from ~/.reproxy/antigravity-accounts.json"]
async fn live_tools() {
    let path = format!(
        "{}/.reproxy/antigravity-accounts.json",
        std::env::var("HOME").unwrap()
    );
    let accounts = Account::parse_file(&std::fs::read(path).unwrap()).unwrap();
    let core = ncore_tokio::core(None);
    let ep = Antigravity::endpoint().with_plugin(Arc::new(Antigravity::new(accounts)));
    let tools = r#"[{"functionDeclarations":[{"name":"get_weather","description":"Weather for a city","parametersJsonSchema":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object","additionalProperties":false,"properties":{"city":{"type":"string","minLength":1},"unit":{"type":["string","null"],"enum":["c","f"]}},"required":["city","unit"]}}]}]"#;
    for model in ["gemini-3-flash", "claude-sonnet-4-6"] {
        let ask = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"What is the weather in Paris? Use the tool."}}]}}],"tools":{tools}}}"#
        );
        let call = run(&core, &ep, model, ask).await;
        let fc = call
            .iter()
            .find_map(|p| p.get("functionCall").cloned())
            .unwrap_or_else(|| panic!("{model}: no function call in {call:?}"));
        println!("{model}: call {fc} parts {call:?}");
        assert_eq!(fc["name"], "get_weather");
        let model_turn = Value::Array(call.clone());
        let follow = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"What is the weather in Paris? Use the tool."}}]}},{{"role":"model","parts":{model_turn}}},{{"role":"user","parts":[{{"functionResponse":{{"name":"get_weather","id":{id},"response":{{"result":"18C, sunny"}}}}}}]}}],"tools":{tools}}}"#,
            id = fc.get("id").cloned().unwrap_or(Value::Null)
        );
        let parts = run(&core, &ep, model, follow).await;
        let text: String = parts
            .iter()
            .filter(|p| p["thought"].as_bool() != Some(true))
            .filter_map(|p| p["text"].as_str())
            .collect();
        println!("{model}: answer {text:?}");
        assert!(text.contains("18"), "{model}: {parts:?}");
    }
}

// Streams one request and returns all response parts merged in order.
async fn run(core: &Core, ep: &ncore::Endpoint, model: &str, body: String) -> Vec<Value> {
    let req = InboundRequest {
        method: "POST".into(),
        path_and_query: format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"),
        headers: vec![],
        body: Bytes::from(body),
    };
    let resp = core
        .passthrough(ep, req, PassthroughOptions::default(), &CancelToken::new())
        .await
        .unwrap();
    let status = resp.status;
    let mut s = match resp.body {
        PassthroughBody::Stream(s) => s,
        PassthroughBody::Full { bytes, .. } => {
            panic!(
                "{model}: status {status} {}",
                String::from_utf8_lossy(&bytes)
            )
        }
    };
    let mut out = Vec::new();
    while let Some(c) = s.next().await {
        out.extend_from_slice(&c.unwrap());
    }
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .flat_map(|v| {
            v["candidates"][0]["content"]["parts"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

#[tokio::test]
async fn cooling_accounts_are_skipped_without_repeated_quota_requests() {
    let google = Arc::new(FakeGoogle::default());
    let core = Core::builder(google.clone()).build();
    let endpoint = Antigravity::endpoint().with_plugin(Arc::new(Antigravity::new(accounts())));
    for _ in 0..3 {
        let response = core
            .passthrough(
                &endpoint,
                inbound("generateContent"),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 200);
    }
    let seen = google.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|r| header(&r.headers, "authorization") == Some("Bearer tok-a1"))
            .count(),
        1
    );
    assert_eq!(
        seen.iter()
            .filter(|r| header(&r.headers, "authorization") == Some("Bearer tok-a2"))
            .count(),
        3
    );
}

#[tokio::test]
async fn exhausted_pool_returns_retry_after_without_another_network_request() {
    let google = Arc::new(FakeGoogle::default());
    let core = Core::builder(google.clone()).build();
    let endpoint = Antigravity::endpoint()
        .with_plugin(Arc::new(Antigravity::new(vec![accounts()[0].clone()])));
    for _ in 0..2 {
        let response = core
            .passthrough(
                &endpoint,
                inbound("generateContent"),
                PassthroughOptions::default(),
                &CancelToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.status, 429);
    }
    assert_eq!(google.seen.lock().unwrap().len(), 2);
}
