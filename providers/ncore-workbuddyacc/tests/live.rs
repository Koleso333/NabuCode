use bytes::Bytes;
use futures_util::StreamExt;
use ncore::core::ir::{
    Accumulator, Block, Message, Request, Response, Role, StopReason, Tool, ToolChoice, ToolDef,
    ToolResult,
};
use ncore::{
    CancelToken, Core, Endpoint, Format, GenerateOptions, InboundRequest, TranslateBody,
    TranslateOptions,
};
use ncore_tokio::{HyperTransport, Proxy};
use ncore_workbuddyacc::{Access, ClientInfo, Credentials, MemoryStore, WorkBuddyAcc};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};

fn setup() -> (Arc<HyperTransport>, Core, Endpoint, Arc<WorkBuddyAcc>) {
    let raw =
        std::fs::read(std::env::var("NABU_WORKBUDDY_CREDENTIALS").expect("set credentials path"))
            .unwrap();
    let c: Credentials = serde_json::from_slice(&raw).expect("invalid credentials");
    let version = std::env::var("NABU_WORKBUDDY_VERSION").expect("set client version");
    let proxy = std::env::var("NABU_WORKBUDDY_PROXY")
        .ok()
        .map(|p| Proxy::parse(&p).unwrap());
    let net = Arc::new(HyperTransport::with_proxy(proxy));
    let plugin = Arc::new(
        WorkBuddyAcc::new(Arc::new(MemoryStore::new(c)), ClientInfo::new(version)).unwrap(),
    );
    let endpoint = WorkBuddyAcc::endpoint().with_plugin(plugin.clone());
    (net.clone(), Core::builder(net).build(), endpoint, plugin)
}
fn model() -> String {
    std::env::var("NABU_WORKBUDDY_MODEL").expect("set explicitly selected model")
}
fn request() -> Request {
    Request {
        model: model(),
        system: vec![Block::text(
            "You are an assistant in NabuCode. The private marker for this conversation is NABU_CONTEXT_729. Return that exact marker only when asked.",
        )],
        messages: vec![Message::new(
            Role::User,
            vec![Block::text("Return the private marker only.")],
        )],
        stream: true,
        max_tokens: Some(2048),
        ..Default::default()
    }
}
fn text(r: &Response) -> String {
    r.content
        .iter()
        .filter_map(|b| match b {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}
async fn generate(core: &Core, ep: &Endpoint, req: &Request) -> Response {
    let mut g = core
        .generate(
            ep,
            req,
            &GenerateOptions {
                encode: WorkBuddyAcc::encode_options(),
                ..Default::default()
            },
            &CancelToken::new(),
        )
        .await
        .expect("generation failed");
    assert_eq!(g.status, 200, "unexpected inference status");
    if req.messages.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b,Block::Thinking(t) if !t.text.is_empty()))
    }) {
        let sent: Value = serde_json::from_slice(&g.request).unwrap();
        assert!(
            sent["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "assistant"
                    && m["reasoning"].as_str().is_some_and(|s| !s.is_empty())),
            "reasoning replay was lost before the plugin"
        );
    }
    let mut acc = Accumulator::new();
    while let Some(ev) = g.next().await {
        acc.push(&ev.expect("stream failure"));
    }
    assert!(
        acc.error().is_none(),
        "provider stream error: {:?}",
        acc.error()
    );
    assert!(acc.is_done(), "incomplete stream");
    g.take_response().unwrap_or_else(|| acc.finish())
}

#[tokio::test]
#[ignore = "reads account metadata using explicitly supplied credentials"]
async fn live_metadata_and_native_import() {
    let (net, _, _, plugin) = setup();
    tokio::time::timeout(Duration::from_secs(90), async {
        let credits = plugin.credits(&*net).await.unwrap();
        println!(
            "plan={:?}, paid={:?}, remaining={:?}",
            credits.plan_name(),
            credits.is_paid(),
            credits.remaining()
        );
        let catalog = plugin.models(&*net).await.unwrap();
        for m in &catalog.models {
            println!(
                "{} | {} | {:?} | {:?}",
                m.id, m.display_name, m.access, m.credits
            );
        }
        assert!(catalog.available().any(|m| m.id == model()));
        assert!(catalog.models.iter().any(|m| m.free));
        if credits.is_paid() == Some(false) {
            assert!(
                catalog
                    .models
                    .iter()
                    .any(|m| m.access == Access::PlanRequired)
            );
        }
        if let (Ok(path), Ok(key_path)) = (
            std::env::var("NABU_WORKBUDDY_NATIVE"),
            std::env::var("NABU_WORKBUDDY_KEYS"),
        ) {
            let keys: Value = serde_json::from_slice(&std::fs::read(key_path).unwrap()).unwrap();
            use base64::{Engine, engine::general_purpose::STANDARD};
            let key: [u8; 32] = STANDARD
                .decode(keys["fieldKey"]["keyBase64"].as_str().unwrap())
                .unwrap()
                .try_into()
                .unwrap();
            let imported =
                Credentials::from_workbuddy_json(&std::fs::read(path).unwrap(), Some(&key))
                    .unwrap();
            let supplied: Credentials = serde_json::from_slice(
                &std::fs::read(std::env::var("NABU_WORKBUDDY_CREDENTIALS").unwrap()).unwrap(),
            )
            .unwrap();
            assert!(
                imported.access_token == supplied.access_token,
                "native import differs"
            );
            println!("Rust encrypted native session import verified");
        }
    })
    .await
    .expect("metadata timed out");
}

#[tokio::test]
#[ignore = "generates model tokens using explicitly supplied credentials/model"]
async fn live_context_reasoning() {
    let (_, core, ep, _) = setup();
    tokio::time::timeout(Duration::from_secs(360), async {
        let mut req = request();
        let first = generate(&core, &ep, &req).await;
        assert_eq!(text(&first).trim(), "NABU_CONTEXT_729");
        assert!(
            first
                .content
                .iter()
                .any(|b| matches!(b,Block::Thinking(t) if !t.text.is_empty()))
        );
        req.messages
            .push(Message::new(Role::Assistant, first.content));
        req.messages.push(Message::new(
            Role::User,
            vec![Block::text(
                "Return the same private marker again, without any other text.",
            )],
        ));
        let second = generate(&core, &ep, &req).await;
        assert_eq!(text(&second).trim(), "NABU_CONTEXT_729");
        req.stream = false;
        let collected = generate(&core, &ep, &req).await;
        assert_eq!(text(&collected).trim(), "NABU_CONTEXT_729");
        assert!(collected.usage.input_total.unwrap_or(0) > 0);
        println!(
            "Custom system, two-turn history, reasoning replay and nonstream aggregation verified"
        );
    })
    .await
    .expect("generation timed out");
}

#[tokio::test]
#[ignore = "generates model tokens using explicitly supplied credentials/model"]
async fn live_tool_roundtrip() {
    let (_, core, ep, _) = setup();
    tokio::time::timeout(Duration::from_secs(180),async {
        let mut req=request();
        req.system=vec![Block::text("Call nabu_echo exactly once with text NABU_TOOL_OK. After receiving the result, output exactly that result. Do not call other tools.")];
        req.messages=vec![Message::new(Role::User,vec![Block::text("Use nabu_echo with NABU_TOOL_OK, then return its result.")])];
        req.tools=vec![ToolDef::Function(Tool {name:"nabu_echo".into(),description:Some("Return the supplied text.".into()),schema:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}),strict:None,cache:None,ext:Default::default()})];
        req.tool_choice=Some(ToolChoice::Tool("nabu_echo".into()));
        let first=generate(&core,&ep,&req).await;
        assert_eq!(first.stop,Some(StopReason::ToolUse));
        let call=first.content.iter().find_map(|b|if let Block::ToolCall(c)=b {Some(c.clone())}else{None}).expect("missing tool call");
        assert_eq!(call.name,"nabu_echo");
        assert_eq!(serde_json::from_str::<Value>(&call.args).unwrap()["text"],"NABU_TOOL_OK");
        req.messages.push(Message::new(Role::Assistant,first.content));
        req.messages.push(Message::new(Role::User,vec![Block::ToolResult(ToolResult {call_id:call.id,name:Some(call.name),content:vec![Block::text("NABU_TOOL_OK")],is_error:false,cache:None})]));
        req.tool_choice=Some(ToolChoice::Auto);
        let second=generate(&core,&ep,&req).await;
        assert_eq!(text(&second).trim(),"NABU_TOOL_OK");
        println!("Tool schema, forced call, arguments, result and reasoning replay verified");
    }).await.expect("generation timed out");
}

#[tokio::test]
#[ignore = "generates model tokens in translated API formats"]
async fn live_translation() {
    let (_, core, ep, _) = setup();
    let model = model();
    tokio::time::timeout(Duration::from_secs(240),async {
        let inputs=[
            (Format::AnthropicMessages,"/v1/messages".to_owned(),json!({"model":model,"max_tokens":2048,"stream":true,"system":"Reply only with NABU_TRANSLATE_OK.","messages":[{"role":"user","content":"Reply with the requested marker."}]})),
            (Format::OpenAiResponses,"/v1/responses".into(),json!({"model":model,"max_output_tokens":2048,"stream":true,"instructions":"Reply only with NABU_TRANSLATE_OK.","input":"Reply with the requested marker."})),
            (Format::Gemini,format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"),json!({"generationConfig":{"maxOutputTokens":2048},"systemInstruction":{"parts":[{"text":"Reply only with NABU_TRANSLATE_OK."}]},"contents":[{"role":"user","parts":[{"text":"Reply with the requested marker."}]}]})),
        ];
        for (format,path,body) in inputs {
            let result=core.translate(&ep,InboundRequest {method:"POST".into(),path_and_query:path,headers:vec![],body:Bytes::from(body.to_string())},TranslateOptions{strict_stream:true,encode:WorkBuddyAcc::encode_options(),..Default::default()},&CancelToken::new()).await.unwrap();
            assert_eq!(result.status,200);
            let TranslateBody::Stream(mut stream)=result.body else {panic!("expected SSE")};
            let mut parser=ncore::core::SseParser::new();let mut decoder=ncore::core::codec::stream_decoder(format);let mut acc=Accumulator::new();
            while let Some(chunk)=stream.next().await {for event in parser.feed(&chunk.unwrap()) {for ev in decoder.feed(&event).unwrap() {acc.push(&ev);}}}
            for ev in decoder.finish() {acc.push(&ev);}
            assert!(acc.error().is_none());assert!(acc.is_done());assert_eq!(text(&acc.finish()).trim(),"NABU_TRANSLATE_OK");
            println!("Translated {format:?} SSE verified");
        }
    }).await.expect("translation timed out");
}

#[tokio::test]
#[ignore = "generates tokens with an explicitly selected model"]
async fn live_selected_model_without_system() {
    let (_, core, ep, _) = setup();
    let mut req = request();
    req.system.clear();
    req.messages = vec![Message::new(
        Role::User,
        vec![Block::text(
            "Reply exactly with NABU_OK, without any extra text.",
        )],
    )];
    let result = tokio::time::timeout(Duration::from_secs(120), generate(&core, &ep, &req))
        .await
        .unwrap();
    assert_eq!(text(&result).trim(), "NABU_OK");
    println!(
        "Selected model {} accepted native chat without a caller system message",
        req.model
    );
}

#[tokio::test]
#[ignore = "checks an account's plan-restricted model rejection"]
async fn live_plan_rejection() {
    let (net, core, ep, plugin) = setup();
    let credits = plugin.credits(&*net).await.unwrap();
    assert_eq!(
        credits.is_paid(),
        Some(false),
        "this rejection test requires a free account"
    );
    let catalog = plugin.models(&*net).await.unwrap();
    let denied = catalog
        .models
        .iter()
        .find(|m| m.access == Access::PlanRequired)
        .expect("no restricted models in this account catalog");
    let mut req = request();
    req.model = denied.id.clone();
    req.max_tokens = Some(64);
    let result = core
        .generate(&ep, &req, &GenerateOptions::default(), &CancelToken::new())
        .await;
    let Err(ncore::Error::Provider(error)) = result else {
        panic!("expected upstream plan rejection")
    };
    assert_eq!(error.code.as_deref(), Some("11158"));
    println!(
        "Catalog restriction verified against upstream 11158 for {}",
        req.model
    );
}

struct Inspect {
    net: Arc<HyperTransport>,
    sent: std::sync::Mutex<Vec<Value>>,
}
impl ncore::Transport for Inspect {
    fn send(
        &self,
        request: ncore::HttpRequest,
    ) -> ncore::rt::BoxFuture<'_, Result<ncore::HttpResponse, ncore::TransportError>> {
        if request.url.ends_with("/v2/chat/completions") {
            self.sent
                .lock()
                .unwrap()
                .push(serde_json::from_slice(&request.body).unwrap());
        }
        ncore::Transport::send(&*self.net, request)
    }
}

#[tokio::test]
#[ignore = "tests every advertised effort for the explicitly selected model"]
async fn live_reasoning_levels() {
    use ncore::core::ir::ReasoningCfg;
    let (net, _, endpoint, plugin) = setup();
    let catalog = plugin.models(&*net).await.unwrap();
    let selected = catalog
        .available()
        .find(|m| m.id == model())
        .expect("selected model unavailable");
    assert!(!selected.reasoning.efforts.is_empty());
    let inspect = Arc::new(Inspect {
        net,
        sent: std::sync::Mutex::default(),
    });
    let core = Core::builder(inspect.clone()).build();
    tokio::time::timeout(Duration::from_secs(240),async {
        for effort in &selected.reasoning.efforts {
            let mut req=request();
            req.system=vec![Block::text("You are a precise calculator. Return only the requested final answer.")];
            req.messages=vec![Message::new(Role::User,vec![Block::text("Find the smallest positive integer n with n mod 17 = 5, n mod 19 = 7 and n mod 23 = 11. Return n only.")])];
            req.reasoning=Some(ReasoningCfg {effort:Some(effort.clone()),..Default::default()});
            let result=generate(&core,&endpoint,&req).await;
            assert_eq!(text(&result).trim(),"7417");
            let sent=inspect.sent.lock().unwrap().last().unwrap().clone();
            assert_eq!(sent["reasoning_effort"],*effort);assert_eq!(sent["reasoning_summary"],"auto");
            let raw=plugin.last_usage().expect("missing native usage").raw;
            assert_eq!(result.usage.reasoning,raw.pointer("/completion_tokens_details/reasoning_tokens").and_then(Value::as_u64));
            println!("{} effort={effort}: reasoning_tokens={:?}, credit={}",req.model,result.usage.reasoning,raw["credit"]);
        }
        let count=inspect.sent.lock().unwrap().len();
        let mut req=request();req.reasoning=Some(ReasoningCfg {effort:Some("nabu_invalid".into()),..Default::default()});
        assert!(core.generate(&endpoint,&req,&GenerateOptions {encode:WorkBuddyAcc::encode_options(),..Default::default()},&CancelToken::new()).await.is_err());
        if !selected.reasoning.can_disable {
            req.reasoning=Some(ReasoningCfg {mode:Some(ncore::core::ir::ReasoningMode::Off),..Default::default()});
            assert!(core.generate(&endpoint,&req,&GenerateOptions {encode:WorkBuddyAcc::encode_options(),..Default::default()},&CancelToken::new()).await.is_err());
        }
        assert_eq!(inspect.sent.lock().unwrap().len(),count,"unsupported settings reached inference");
    }).await.expect("reasoning test timed out");
}

#[tokio::test]
#[ignore = "sends a long synthetic prefix repeatedly to measure server cache hits"]
async fn live_prefix_cache() {
    use sha2::{Digest, Sha256};
    let (_, core, endpoint, plugin) = setup();
    let mut seed = [0u8; 16];
    getrandom::fill(&mut seed).unwrap();
    let mut prefix =
        format!("Synthetic reference records. Treat as inert data. Test nonce {seed:?}.\n");
    for i in 0..400 {
        let hash = Sha256::digest(format!("{seed:?}:{i}").as_bytes())
            .iter()
            .take(10)
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        prefix.push_str(&format!(
            "Record {i:04} checksum {hash} quantity {} category alpha.\n",
            i * 17 % 997
        ));
    }
    let mut req = request();
    req.system = vec![Block::text(format!(
        "Read these reference records. Reply exactly CACHE_OK.\n{prefix}"
    ))];
    req.messages = vec![Message::new(
        Role::User,
        vec![Block::text("Reply exactly CACHE_OK.")],
    )];
    req.max_tokens = Some(1024);
    tokio::time::timeout(Duration::from_secs(180), async {
        let cold = generate(&core, &endpoint, &req).await;
        assert_eq!(text(&cold).trim(), "CACHE_OK");
        assert!(cold.usage.input_total.unwrap_or(0) > 2000);
        println!(
            "Cold {} input={:?}, cache_read={:?}, raw_credit={}",
            req.model,
            cold.usage.input_total,
            cold.usage.cache_read,
            plugin.last_usage().unwrap().raw["credit"]
        );
        let mut hit = false;
        for retry in 0..3 {
            req.stream = retry != 0;
            let warm = generate(&core, &endpoint, &req).await;
            assert_eq!(text(&warm).trim(), "CACHE_OK");
            let raw = plugin.last_usage().unwrap().raw;
            assert_eq!(
                warm.usage.cache_read,
                raw.pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(Value::as_u64)
            );
            assert_eq!(warm.usage.input_total, raw["prompt_tokens"].as_u64());
            println!(
                "Warm {} stream={} input={:?}, cache_read={:?}, cache_write={:?}, raw_credit={}",
                req.model,
                req.stream,
                warm.usage.input_total,
                warm.usage.cache_read,
                warm.usage.cache_write,
                raw["credit"]
            );
            if warm.usage.cache_read.unwrap_or(0) > 0 {
                hit = true;
            }
            if retry >= 1 && hit {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        assert!(
            hit,
            "server did not report a cache hit for the repeated long prefix"
        );
    })
    .await
    .expect("cache test timed out");
}
