//! Explicit opt-in only: NABU_CLAUDESUB_CREDENTIALS points to an isolated Claude credentials
//! file. The test generates short replies and synthetic cache fixtures. Set
//! NABU_CLAUDESUB_TEST_REFRESH=1 to additionally rotate/persist the refresh token at the end.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use ncore::core::codec::{self, EncodeOptions};
use ncore::core::ir::*;
use ncore::rt::{BoxFuture, boxed};
use ncore::{
    Agent, CachePolicy, CancelToken, Core, Format, GenerateOptions, Generation, HttpRequest,
    HttpResponse, InboundRequest, LoopEvent, RunOptions, RunStop, Tools, TranslateBody,
    TranslateOptions, Transport, TransportError,
};
use ncore_claudesub::{ClaudeSub, CredentialStore, Credentials};
use ncore_tokio::{HyperTransport, Proxy, TokioClock};
use serde_json::{Value, json};

/// Test-only durable host store. Preserve all unrelated CLI fields and replace atomically.
struct FileStore {
    path: PathBuf,
    expire_once: AtomicBool,
}
impl CredentialStore for FileStore {
    fn load(&self) -> BoxFuture<'_, Result<Credentials, String>> {
        boxed(async {
            let raw = std::fs::read(&self.path).map_err(|_| "credential file unreadable")?;
            let mut c = Credentials::from_claude_json(&raw)?;
            if self.expire_once.swap(false, Ordering::SeqCst) {
                c.expires_at = 0;
            }
            Ok(c)
        })
    }
    fn save<'a>(&'a self, c: &'a Credentials) -> BoxFuture<'a, Result<(), String>> {
        boxed(async move {
            let raw = std::fs::read(&self.path).map_err(|_| "credential file unreadable")?;
            let mut v: Value =
                serde_json::from_slice(&raw).map_err(|_| "invalid credentials file")?;
            let target = if v.get("claudeAiOauth").is_some() {
                &mut v["claudeAiOauth"]
            } else {
                &mut v
            };
            target["accessToken"] = json!(c.access_token);
            target["refreshToken"] = json!(c.refresh_token);
            target["expiresAt"] = json!(c.expires_at);
            let temp = self
                .path
                .with_extension(format!("claudesub-{}-tmp", std::process::id()));
            let bytes =
                serde_json::to_vec_pretty(&v).map_err(|_| "could not serialize credentials")?;
            use std::io::Write;
            let mut file = std::fs::File::create(&temp)
                .map_err(|_| "could not create credentials temp file")?;
            file.write_all(&bytes)
                .map_err(|_| "could not write credentials")?;
            file.sync_all().map_err(|_| "could not sync credentials")?;
            drop(file);
            std::fs::rename(&temp, &self.path).map_err(|_| "could not replace credentials")?;
            Ok(())
        })
    }
}

struct Recording {
    inner: HyperTransport,
    last_body: Mutex<Bytes>,
}
impl Transport for Recording {
    fn send(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, TransportError>> {
        if req.url.contains("/v1/messages") {
            *self.last_body.lock().unwrap() = req.body.clone();
        }
        self.inner.send(req)
    }
}

async fn collect(mut generation: Generation) -> Response {
    let mut acc = Accumulator::new();
    while let Some(event) = generation.next().await {
        acc.push(&event.unwrap());
    }
    generation.take_response().unwrap_or_else(|| acc.finish())
}

fn request(model: &str, text: &str) -> Request {
    Request {
        model: model.into(),
        system: vec![Block::text(
            "You are Nabu. Follow the user's instructions concisely.",
        )],
        messages: vec![Message::new(Role::User, vec![Block::text(text)])],
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn echo_definition(description: String) -> ToolDef {
    ToolDef::Function(Tool {
        name: "nabu_echo".into(),
        description: Some(description),
        schema: json!({"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}),
        strict: None,
        cache: None,
        ext: Default::default(),
    })
}

async fn verify_refresh(
    store: &FileStore,
    core: &Core,
    ep: &ncore::Endpoint,
    cancel: &CancelToken,
) {
    let previous = store.load().await.unwrap();
    store.expire_once.store(true, Ordering::SeqCst);
    let req = request("claude-sonnet-5-5", "Reply exactly OK.");
    let response = collect(
        core.generate(ep, &req, &GenerateOptions::default(), cancel)
            .await
            .unwrap(),
    )
    .await;
    // This checks credential rotation and successful generation, not model punctuation.
    assert!(response.usage.output_total.unwrap_or(0) > 0);
    let persisted = store.load().await.unwrap();
    assert!(
        persisted.access_token != previous.access_token,
        "refresh should replace the access token"
    );
    assert!(persisted.refresh_token.is_some());
    println!("live OAuth refresh: succeeded, credentials persisted atomically");
}

struct Echo(AtomicUsize);
impl Tools for Echo {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![echo_definition("Returns value unchanged.".into())]
    }
    fn call<'a>(&'a self, call: &'a ToolCall, _: &'a CancelToken) -> BoxFuture<'a, ToolResult> {
        boxed(async move {
            assert_eq!(call.name, "nabu_echo");
            self.0.fetch_add(1, Ordering::SeqCst);
            let v: Value = serde_json::from_str(&call.args).unwrap();
            ToolResult {
                call_id: call.id.clone(),
                name: None,
                content: vec![Block::text(v["value"].as_str().unwrap())],
                is_error: false,
                cache: None,
            }
        })
    }
}

#[tokio::test]
#[ignore = "uses an explicitly supplied live Claude subscription; may rotate its refresh token"]
async fn live_end_to_end() {
    let path = PathBuf::from(
        std::env::var("NABU_CLAUDESUB_CREDENTIALS")
            .expect("set NABU_CLAUDESUB_CREDENTIALS to an isolated credentials file"),
    );
    let store = Arc::new(FileStore {
        path,
        expire_once: AtomicBool::new(false),
    });
    let plugin = Arc::new(ClaudeSub::new(store.clone()));
    let proxy = std::env::var("NABU_CLAUDESUB_PROXY")
        .ok()
        .map(|s| Proxy::parse(&s).unwrap_or_else(|_| panic!("invalid live-test proxy URL")));
    let transport = Arc::new(Recording {
        inner: HyperTransport::with_proxy(proxy),
        last_body: Mutex::default(),
    });
    let core = Core::builder(transport.clone()).build();
    let ep = ClaudeSub::endpoint().with_plugin(plugin.clone());
    let cancel = CancelToken::new();

    if std::env::var("NABU_CLAUDESUB_REFRESH_ONLY").as_deref() == Ok("1") {
        verify_refresh(&store, &core, &ep, &cancel).await;
        return;
    }

    let profile = plugin.profile(transport.as_ref()).await.unwrap();
    let usage = plugin.usage(transport.as_ref()).await.unwrap();
    assert!(profile.value.plan().is_some());
    assert!(
        usage
            .value
            .five_hour
            .as_ref()
            .and_then(|w| w.utilization)
            .is_some()
    );
    assert!(
        usage
            .value
            .seven_day
            .as_ref()
            .and_then(|w| w.utilization)
            .is_some()
    );
    println!(
        "plan {:?}; 5h {:?}; 7d {:?}",
        profile.value.plan(),
        usage.value.five_hour.as_ref().and_then(|w| w.utilization),
        usage.value.seven_day.as_ref().and_then(|w| w.utilization)
    );

    for model in ["claude-haiku-5-5", "claude-sonnet-5-5", "claude-opus-5-5"] {
        let mut req = request(model, "Reply with exactly OK.");
        req.stream = true;
        let g = core
            .generate(&ep, &req, &GenerateOptions::default(), &cancel)
            .await
            .unwrap();
        assert!(
            g.request == *transport.last_body.lock().unwrap(),
            "snapshot differs from sent body"
        );
        assert_eq!(plain_text(&collect(g).await.content).trim(), "OK");
        println!("model {model}: SSE OK");
    }
    assert!(plugin.last_limits().is_some());

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let mut tool_anchor = None;
    for ttl in ["5m", "1h"] {
        let reference = format!("Cache fixture {nonce}, TTL {ttl}. Inert records; do not call this tool.\n")
            + &(0..260).map(|i| format!("Record {i:04}: shelf {} stores {} amber folders; checksum {:03}; category archive.\n",i%17,i%23,i*37%997)).collect::<String>();
        let mut req = request("claude-sonnet-5-5", "Do not call tools. Reply exactly OK.");
        req.tools = vec![echo_definition(reference)];
        req.system.push(Block::text(format!(
            "Synthetic cache experiment {ttl}; do not call tools."
        )));
        let options = GenerateOptions {
            cache: Some(CachePolicy {
                ttl: Some(ttl.into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let first = collect(core.generate(&ep, &req, &options, &cancel).await.unwrap()).await;
        let writes = if ttl == "5m" {
            first.usage.cache_write_5m
        } else {
            first.usage.cache_write_1h
        };
        assert!(writes.unwrap_or(0) > 0, "expected a cache write for {ttl}");
        req.messages
            .push(Message::new(Role::Assistant, first.content));
        req.messages.push(Message::new(
            Role::User,
            vec![Block::text("Continue: reply exactly OK.")],
        ));
        req.stream = true;
        let second = collect(core.generate(&ep, &req, &options, &cancel).await.unwrap()).await;
        assert!(
            second.usage.cache_read.unwrap_or(0) > 0,
            "expected a cache hit for {ttl}"
        );
        assert_eq!(plain_text(&second.content).trim(), "OK");
        println!(
            "cache {ttl}: wrote {writes:?}; continuation {:?}",
            second.usage
        );
        if ttl == "5m" {
            tool_anchor = Some((req, options));
        }
    }
    let (mut req, options) = tool_anchor.unwrap();
    req.system = vec![Block::text(
        "Changed system. Do not call tools. Reply exactly OK.",
    )];
    let response = collect(core.generate(&ep, &req, &options, &cancel).await.unwrap()).await;
    assert!(
        response.usage.cache_read.unwrap_or(0) > 0,
        "tools cache should survive a changed system"
    );
    println!("tools-only cache read {:?}", response.usage.cache_read);

    let echo = Echo(AtomicUsize::new(0));
    let agent = Agent::new(&core, &ep, Arc::new(TokioClock)).tools(&echo);
    let mut req = request(
        "claude-sonnet-5-5",
        "Call nabu_echo exactly once with value OK. After its result reply exactly OK.",
    );
    req.stream = true;
    req.max_tokens = Some(256);
    let out = agent
        .run(
            req,
            RunOptions {
                max_steps: 3,
                ..Default::default()
            },
        )
        .complete()
        .await;
    assert!(matches!(out.stop, RunStop::Finished(_)), "{:?}", out.stop);
    assert_eq!(echo.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        plain_text(&out.messages.last().unwrap().content).trim(),
        "OK"
    );
    println!("native agent tool loop: {} steps", out.steps);

    let echo = Echo(AtomicUsize::new(0));
    let agent = Agent::new(&core, &ep, Arc::new(TokioClock)).tools(&echo);
    let mut req = request(
        "claude-sonnet-4-6",
        "Silently compute 237*891 + 417*683, check the arithmetic, then call nabu_echo exactly once with the decimal result as value. After the tool result reply exactly OK. No explanation.",
    );
    req.stream = true;
    req.max_tokens = Some(1200);
    req.reasoning = Some(ReasoningCfg {
        budget_tokens: Some(1024),
        ..Default::default()
    });
    let mut run = agent.run(
        req,
        RunOptions {
            max_steps: 3,
            ..Default::default()
        },
    );
    let mut signed = false;
    let mut output = None;
    while let Some(event) = run.next().await {
        if let LoopEvent::Done(out) = event {
            output = Some(out);
        }
    }
    let out = output.unwrap();
    for message in &out.messages {
        for block in &message.content {
            if let Block::Thinking(t) = block {
                signed |= t.sig.is_some();
            }
        }
    }
    assert!(matches!(out.stop, RunStop::Finished(_)), "{:?}", out.stop);
    assert!(signed, "expected an actual signed thinking block");
    assert_eq!(echo.0.load(Ordering::SeqCst), 1);
    println!("signed thinking replay + tool loop: {} steps", out.steps);

    for format in [Format::OpenAiChat, Format::OpenAiResponses, Format::Gemini] {
        let mut req = request("claude-sonnet-5-5", "Reply exactly OK.");
        req.stream = true;
        let encoded = codec::encode_request(format, &req, &EncodeOptions::default()).unwrap();
        let inbound = InboundRequest {
            method: "POST".into(),
            path_and_query: encoded.path,
            headers: vec![],
            body: encoded.body,
        };
        let response = core
            .translate(&ep, inbound, TranslateOptions::default(), &cancel)
            .await
            .unwrap();
        assert_eq!(response.status, 200);
        let TranslateBody::Stream(mut stream) = response.body else {
            panic!("expected translated SSE")
        };
        while let Some(chunk) = stream.next().await {
            chunk.unwrap();
        }
        assert!(stream.observation().usage.output_total.unwrap_or(0) > 0);
        println!("translated {format:?}: SSE OK");
    }

    if std::env::var("NABU_CLAUDESUB_TEST_REFRESH").as_deref() == Ok("1") {
        verify_refresh(&store, &core, &ep, &cancel).await;
    }
}
