use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use ncore::core::ir::{
    Accumulator, Block, Message, ReasoningCfg, Request, Response, Role, StopReason, Tool,
    ToolChoice, ToolDef, ToolResult,
};
use ncore::{
    CancelToken, Core, Endpoint, Format, GenerateOptions, InboundRequest, TranslateBody,
    TranslateOptions,
};
use ncore_tokio::{HyperTransport, Proxy};
use ncore_zcodeacc::{ClientInfo, Credentials, MemoryStore, ZCodeAcc};
use serde_json::{Value, json};

fn setup() -> (Arc<HyperTransport>, Core, Endpoint, Arc<ZCodeAcc>) {
    let path = std::env::var("NABU_ZCODEACC_CREDENTIALS").expect("set NABU_ZCODEACC_CREDENTIALS");
    let raw = std::fs::read(path).expect("could not read supplied credentials");
    let cipher = std::env::var("NABU_ZCODEACC_CIPHER_SECRET").ok();
    let credentials = serde_json::from_slice::<Credentials>(&raw)
        .or_else(|_| Credentials::from_zcode_json(&raw, cipher.as_deref()))
        .expect("invalid credentials");
    let client_path = std::env::var("NABU_ZCODEACC_CLIENT")
        .expect("set NABU_ZCODEACC_CLIENT to host client metadata JSON");
    let client: ClientInfo = serde_json::from_slice(&std::fs::read(client_path).unwrap())
        .expect("invalid client metadata");
    let proxy = std::env::var("NABU_ZCODEACC_PROXY")
        .ok()
        .map(|s| Proxy::parse(&s).unwrap());
    let net = Arc::new(HyperTransport::with_proxy(proxy));
    let plugin = Arc::new(ZCodeAcc::new(
        Arc::new(MemoryStore::new(credentials)),
        client,
    ));
    let endpoint = ZCodeAcc::endpoint().with_plugin(plugin.clone());
    (net.clone(), Core::builder(net).build(), endpoint, plugin)
}

fn request() -> Request {
    Request {
        model: std::env::var("NABU_ZCODEACC_MODEL").unwrap_or_else(|_| "glm-5.3-flash".into()),
        system: vec![
            Block::text("You are a connectivity test in a graphical application."),
            Block::text("Reply only with NABU_OK."),
        ],
        messages: vec![Message::new(
            Role::User,
            vec![Block::text("Reply with NABU_OK.")],
        )],
        max_tokens: Some(2048),
        reasoning: Some(ReasoningCfg {
            effort: Some("low".into()),
            budget_tokens: Some(1024),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn generate(core: &Core, endpoint: &Endpoint, request: &Request) -> Response {
    let mut generation = core
        .generate(
            endpoint,
            request,
            &GenerateOptions::default(),
            &CancelToken::new(),
        )
        .await
        .unwrap_or_else(|error| panic!("generation failed: {error}"));
    assert_eq!(generation.status, 200);
    let sent: Value = serde_json::from_slice(&generation.request).unwrap();
    assert_eq!(
        sent["system"][0]["text"],
        "You are ZCode, an interactive agent"
    );
    let mut acc = Accumulator::new();
    while let Some(event) = generation.next().await {
        acc.push(&event.unwrap_or_else(|error| panic!("stream failed: {error}")));
    }
    assert!(acc.error().is_none(), "provider reported a stream error");
    if request.stream {
        assert!(acc.is_done(), "stream did not complete");
    }
    generation.take_response().unwrap_or_else(|| acc.finish())
}

fn text(response: &Response) -> String {
    response
        .content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires explicit ZCode credentials and generates model tokens"]
async fn live_metadata_and_generation() {
    let (net, core, endpoint, plugin) = setup();
    tokio::time::timeout(Duration::from_secs(240), async {
        let usage = plugin
            .usage(&*net)
            .await
            .unwrap_or_else(|e| panic!("usage failed: {e}"));
        assert!(!usage.stale);
        let ids = plugin.models(&*net).await.unwrap();
        println!("Account model ids: {ids:?}");
        let config = plugin.client_config(&*net).await.unwrap();
        println!(
            "Captcha skip_model_request: {:?}",
            config.pointer("/configs/captcha/skip_model_request")
        );
        let modes = match std::env::var("NABU_ZCODEACC_STREAM").as_deref() {
            Ok("true") => vec![true],
            Ok("false") => vec![false],
            _ => vec![false, true],
        };
        for streaming in modes {
            let mut request = request();
            request.stream = streaming;
            assert!(ids.iter().any(|id| id.eq_ignore_ascii_case(&request.model)));
            let response = generate(&core, &endpoint, &request).await;
            assert_eq!(response.stop, Some(StopReason::EndTurn));
            assert_eq!(text(&response).trim(), "NABU_OK");
            println!(
                "{} streaming={streaming}: NABU_OK; usage={:?}",
                request.model, response.usage
            );
        }
    })
    .await
    .expect("live check timed out");
}

#[tokio::test]
#[ignore = "requires explicit ZCode credentials and generates model tokens"]
async fn live_translation_and_tool_round_trip() {
    let (_, core, endpoint, _) = setup();
    tokio::time::timeout(Duration::from_secs(300), async {
        let model = request().model;
        let clients = [
            (Format::OpenAiChat, "/v1/chat/completions".to_owned(), json!({"model":model,"max_completion_tokens":2048,"reasoning_effort":"low","stream":true,
                "messages":[{"role":"system","content":"Reply only with NABU_OK."},{"role":"user","content":"Reply with NABU_OK."}]})),
            (Format::OpenAiResponses, "/v1/responses".into(), json!({"model":model,"max_output_tokens":2048,"reasoning":{"effort":"low"},"stream":true,
                "instructions":"Reply only with NABU_OK.","input":"Reply with NABU_OK."})),
            (Format::Gemini, format!("/v1beta/models/{model}:streamGenerateContent?alt=sse"), json!({"generationConfig":{"maxOutputTokens":2048},
                "systemInstruction":{"parts":[{"text":"Reply only with NABU_OK."}]},
                "contents":[{"role":"user","parts":[{"text":"Reply with NABU_OK."}]}]})),
        ];
        for (format, path, body) in clients {
            let result = core.translate(&endpoint, InboundRequest {method:"POST".into(),path_and_query:path.clone(),headers:vec![],body:Bytes::from(body.to_string())},
                TranslateOptions {strict_stream:true,..Default::default()}, &CancelToken::new()).await.unwrap_or_else(|e| panic!("translate failed: {e}"));
            assert_eq!(result.status,200);
            let TranslateBody::Stream(mut stream) = result.body else {panic!("expected stream")};
            let mut bytes = Vec::new();
            while let Some(chunk) = stream.next().await {bytes.extend_from_slice(&chunk.unwrap_or_else(|e| panic!("translate stream failed: {e}")));}
            let mut parser = ncore::core::SseParser::new();
            let mut decoder = ncore::core::codec::stream_decoder(format);
            let mut acc = Accumulator::new();
            for event in parser.feed(&bytes) {
                for decoded in decoder.feed(&event).unwrap() { acc.push(&decoded); }
            }
            for decoded in decoder.finish() { acc.push(&decoded); }
            assert!(acc.error().is_none(), "translated provider error: {:?}",acc.error());
            let response = acc.finish();
            assert_eq!(text(&response).trim(),"NABU_OK", "unexpected translated output: {response:?}");
            println!("Translated SSE succeeded: {path}");
        }
        let mut request = request();
        request.system = vec![Block::text("Call nabu_echo once with text NABU_TOOL_OK, then reply exactly with the tool result.")];
        request.messages = vec![Message::new(Role::User,vec![Block::text("Call nabu_echo with NABU_TOOL_OK and return its result.")])];
        request.tools = vec![ToolDef::Function(Tool {name:"nabu_echo".into(),description:Some("Return the text unchanged.".into()),
            schema:json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}),
            strict:None,cache:None,ext:Default::default()})];
        request.tool_choice = Some(ToolChoice::Tool("nabu_echo".into()));
        request.stream = true;
        let first = generate(&core,&endpoint,&request).await;
        assert_eq!(first.stop,Some(StopReason::ToolUse));
        let call = first.content.iter().find_map(|block| match block {Block::ToolCall(call) => Some(call.clone()),_=>None}).expect("missing tool call");
        assert_eq!(call.name,"nabu_echo");
        assert_eq!(serde_json::from_str::<Value>(&call.args).unwrap()["text"],"NABU_TOOL_OK");
        request.messages.push(Message::new(Role::Assistant,first.content));
        request.messages.push(Message::new(Role::User,vec![Block::ToolResult(ToolResult {call_id:call.id,name:Some(call.name),
            content:vec![Block::text("NABU_TOOL_OK")],is_error:false,cache:None})]));
        request.tool_choice = Some(ToolChoice::Auto);
        let second = generate(&core,&endpoint,&request).await;
        assert_eq!(second.stop,Some(StopReason::EndTurn));
        assert_eq!(text(&second).trim(),"NABU_TOOL_OK");
        println!("Streaming tool round trip succeeded");
    }).await.expect("live check timed out");
}
