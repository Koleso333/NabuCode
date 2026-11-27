use std::sync::{Arc, Mutex};
use std::time::Duration;

use nabu_protocol::{
    AccountStatus, Event, Message, ModelRef, Part, Prompt, PromptPart, Reply, Request,
    SessionStatus,
};
use ncore::core::ir;
use ncore::rt::{BoxFuture, boxed, header};
use ncore::{CancelToken, HttpRequest, HttpResponse, Transport, TransportError};
use ncore_tokio::HyperTransport;
use serde_json::{Value, json};

use crate::Server;

const ARGON: &str = "gemini-4-argon";
const CARBON: &str = "gemini-4-carbon";
const FLASH: &str = "gemini-3.5-flash-lite";
const LIMITED: &str = "gemini-3.8-flash";
const NEON: &str = "gemini-4-neon";
const FLASH_25: &str = "gemini-2.5-flash";

#[derive(Default)]
struct GeminiTransport {
    generations: Mutex<Vec<(String, String)>>,
}

impl Transport for GeminiTransport {
    fn send(&self, request: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        boxed(async move {
            let key = header(&request.headers, "x-goog-api-key").unwrap();
            let (status, body) = if request.method == "GET" {
                assert_eq!(
                    request.url,
                    "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1000"
                );
                let models = [ARGON, CARBON, FLASH, LIMITED, NEON, FLASH_25].map(|model| {
                    json!({
                        "name": format!("models/{model}"),
                        "supportedGenerationMethods": ["generateContent"],
                        "thinking": true,
                    })
                });
                (200, json!({"models": models}).to_string())
            } else {
                let model = request
                    .url
                    .strip_prefix("https://generativelanguage.googleapis.com/v1beta/models/")
                    .unwrap()
                    .strip_suffix(":streamGenerateContent?alt=sse")
                    .unwrap();
                self.generations
                    .lock()
                    .unwrap()
                    .push((model.into(), key.into()));
                match (model, key) {
                    (ARGON, "test-first") => (
                        429,
                        json!({"error": {
                            "status": "RESOURCE_EXHAUSTED",
                            "message": "Quota exceeded for metric: generate_content_free_tier_requests, limit: 0, model: gemini-4-argon",
                            "details": [{"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "20895s"}],
                        }}).to_string(),
                    ),
                    (CARBON, _) => (403, json!({"error": {
                        "status": "PERMISSION_DENIED", "message": "This project does not have access to gemini-4-carbon",
                    }}).to_string()),
                    (LIMITED, "test-first") => (429, json!({"error": {
                        "status": "RESOURCE_EXHAUSTED", "message": "Requests per day quota exceeded",
                        "details": [
                            {"@type": "type.googleapis.com/google.rpc.QuotaFailure", "violations": [{"quotaValue": "50"}]},
                            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": "120.5s"},
                        ],
                    }}).to_string()),
                    _ => {
                        let chunk = json!({"candidates": [{
                            "content": {"role": "model", "parts": [{"text": "OK"}]},
                            "finishReason": "STOP",
                        }]});
                        (200, format!("data: {chunk}\n\n"))
                    }
                }
            };
            Ok(HttpResponse {
                status,
                headers: vec![(
                    "content-type".into(),
                    if status == 200 && request.method == "POST" {
                        "text/event-stream"
                    } else {
                        "application/json"
                    }
                    .into(),
                )],
                body: Box::pin(futures_util::stream::once(async move { Ok(body.into()) })),
            })
        })
    }
}

#[tokio::test]
async fn gemini_effort_is_listed_per_model_and_sent_in_the_native_request() {
    let mut random = [0; 8];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!(
        "nabu-gemini-effort-{:x}",
        u64::from_ne_bytes(random)
    ));
    let server = Server::with_transport(
        &root,
        Arc::new(HyperTransport::new()),
        Arc::new(GeminiTransport::default()),
    );
    add_key(&server, "test-effort", 1).await;
    let provider = server.gemini.provider().unwrap();
    let efforts = |id: &str| {
        &provider
            .models
            .iter()
            .find(|model| model.id == id)
            .unwrap()
            .efforts
    };
    assert_eq!(efforts(FLASH), &["minimal", "low", "medium", "high"]);
    assert_eq!(efforts(LIMITED), &["low", "medium", "high"]);
    assert_eq!(efforts(NEON), &["low", "medium", "high", "xhigh"]);
    assert_eq!(efforts(FLASH_25), &["low", "medium", "high"]);

    for (id, variant, expected) in [
        (
            FLASH,
            Some("minimal"),
            json!({"thinkingLevel": "MINIMAL", "includeThoughts": true}),
        ),
        (
            LIMITED,
            Some("medium"),
            json!({"thinkingLevel": "MEDIUM", "includeThoughts": true}),
        ),
        (
            NEON,
            Some("xhigh"),
            json!({"thinkingLevel": "XHIGH", "includeThoughts": true}),
        ),
        (
            FLASH_25,
            Some("medium"),
            json!({"thinkingBudget": 4096, "includeThoughts": true}),
        ),
        (LIMITED, Some("minimal"), Value::Null),
        (FLASH, None, Value::Null),
    ] {
        let model = ModelRef {
            provider_id: "gemini".into(),
            model_id: id.into(),
            variant: variant.map(str::to_owned),
            fast: false,
        };
        let mut request = ir::Request {
            model: model.model_id.clone(),
            messages: vec![ir::Message::new(
                ir::Role::User,
                vec![ir::Block::text("Reply OK")],
            )],
            stream: true,
            ..Default::default()
        };
        let route = server
            .models
            .route("test-session", &model, &mut request)
            .unwrap();
        let generation = server
            .models
            .core
            .generate(
                &route.endpoint,
                &request,
                &route.options,
                &CancelToken::new(),
            )
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&generation.request).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"], expected,
            "{id} at {variant:?}"
        );
    }
    std::fs::remove_dir_all(root).unwrap();
}

async fn add_key(server: &Server, key: &str, total: usize) {
    let mut events = server.subscribe();
    server
        .handle(Request::GeminiAdd {
            keys: vec![key.into()],
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Event::GeminiAccounts { accounts } = events.recv().await.unwrap()
                && accounts.len() == total
                && accounts
                    .iter()
                    .all(|account| account.status == AccountStatus::Work)
            {
                break;
            }
        }
    })
    .await
    .unwrap();
}

async fn prompt(
    server: &Server,
    session: &str,
    model: &str,
    turn: usize,
) -> Result<String, String> {
    let mut events = server.subscribe();
    server
        .handle(Request::SessionPrompt(Prompt {
            session_id: session.into(),
            message_id: format!("msg_gemini_{turn:02}"),
            agent: "build".into(),
            model: ModelRef {
                provider_id: "gemini".into(),
                model_id: model.into(),
                variant: None,
                fast: false,
            },
            parts: vec![PromptPart::Text {
                id: format!("prt_gemini_{turn:02}"),
                text: "Reply OK".into(),
            }],
        }))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match events.recv().await.unwrap() {
                Event::SessionStatus {
                    status: SessionStatus::Idle,
                    ..
                } => break,
                Event::SessionStatus {
                    status: SessionStatus::Retry { message, .. },
                    ..
                } => {
                    panic!("unexpected repeated request: {message}")
                }
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let messages = server.sessions.messages(session).unwrap();
    let response = messages.last().unwrap();
    let Message::Assistant(assistant) = &response.info else {
        panic!("missing assistant message")
    };
    if let Some(error) = &assistant.error {
        return Err(error.data.message.clone());
    }
    Ok(response
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text(part) => Some(part.text.as_str()),
            _ => None,
        })
        .collect())
}

#[tokio::test]
async fn gemini_refusals_keep_the_reason_and_only_block_the_affected_key_and_model() {
    let mut random = [0; 8];
    getrandom::fill(&mut random).unwrap();
    let root = std::env::temp_dir().join(format!("nabu-gemini-{:x}", u64::from_ne_bytes(random)));
    let transport = Arc::new(GeminiTransport::default());
    let server = Server::with_transport(&root, Arc::new(HyperTransport::new()), transport.clone());
    add_key(&server, "test-first", 1).await;
    let Reply::Session(session) = server
        .handle(Request::SessionCreate {
            directory: root.to_string_lossy().into_owned(),
            temporary: false,
            tools: None,
            chrome: None,
        })
        .await
        .unwrap()
    else {
        panic!("missing session")
    };

    let argon_error = prompt(&server, &session.id, ARGON, 1).await.unwrap_err();
    assert!(argon_error.contains("Model unavailable on the current plan (quota is 0) (HTTP 429)"));
    assert!(argon_error.contains("Quota exceeded for metric"));
    assert_eq!(
        prompt(&server, &session.id, ARGON, 2).await.unwrap_err(),
        argon_error
    );
    assert_eq!(prompt(&server, &session.id, FLASH, 3).await.unwrap(), "OK");

    let permission_error = prompt(&server, &session.id, CARBON, 4).await.unwrap_err();
    assert!(permission_error.contains("This project does not have access to gemini-4-carbon"));
    let quota_error = prompt(&server, &session.id, LIMITED, 5).await.unwrap_err();
    assert!(quota_error.contains("Model quota exhausted (HTTP 429)"));
    assert!(quota_error.contains("Requests per day quota exceeded"));
    assert_eq!(
        prompt(&server, &session.id, LIMITED, 6).await.unwrap_err(),
        quota_error
    );

    add_key(&server, "test-second", 2).await;
    server.gemini.check_all().await;
    assert_eq!(
        prompt(&server, &session.id, LIMITED, 7).await.unwrap(),
        "OK"
    );
    server.handle(Request::GeminiRemoveAll).await.unwrap();
    add_key(&server, "test-first", 1).await;
    assert_eq!(prompt(&server, &session.id, FLASH, 8).await.unwrap(), "OK");
    add_key(&server, "test-second", 2).await;
    assert_eq!(prompt(&server, &session.id, ARGON, 9).await.unwrap(), "OK");
    assert_eq!(
        *transport.generations.lock().unwrap(),
        [
            (ARGON.into(), "test-first".into()),
            (FLASH.into(), "test-first".into()),
            (CARBON.into(), "test-first".into()),
            (LIMITED.into(), "test-first".into()),
            (LIMITED.into(), "test-second".into()),
            (FLASH.into(), "test-first".into()),
            (ARGON.into(), "test-first".into()),
            (ARGON.into(), "test-second".into()),
        ]
    );
    std::fs::remove_dir_all(root).unwrap();
}
