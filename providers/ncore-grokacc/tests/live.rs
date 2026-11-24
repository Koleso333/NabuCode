use std::sync::Arc;

use futures_util::StreamExt;
use ncore::core::ir::{
    Accumulator, Block, Message, Request, Role, Tool, ToolChoice, ToolDef, ToolResult, plain_text,
};
use ncore::{CancelToken, Core, GenerateOptions};
use ncore_grokacc::{Credentials, GrokAcc, MemoryStore};
use ncore_tokio::HyperTransport;
use serde_json::json;

#[tokio::test]
#[ignore = "requires a fresh isolated Grok OAuth session and generates tokens"]
async fn catalog_billing_and_tool_roundtrip() {
    let path = std::env::var("NABU_GROKACC_CREDENTIALS").unwrap();
    let raw = std::fs::read(path).unwrap();
    let mut credentials = Credentials::from_grok_json(&raw).unwrap();
    credentials.refresh_token = None;
    let plugin = Arc::new(GrokAcc::new(Arc::new(MemoryStore::new(credentials))));
    let net = Arc::new(HyperTransport::new());
    let profile = plugin.profile(&*net).await.unwrap();
    let billing = plugin.billing(&*net).await.unwrap();
    let model = plugin
        .models(&*net)
        .await
        .unwrap()
        .into_iter()
        .find(|model| model.id == "grok-4.7")
        .unwrap();
    assert_eq!(model.context_window, 256_000);
    println!(
        "plan={:?}, reset={:?}, model={}",
        profile.subscription_tier,
        billing.resets_at(),
        model.id
    );

    let endpoint = GrokAcc::endpoint().with_plugin(plugin.clone());
    let core = Core::builder(net).build();
    let options = GenerateOptions {
        headers: vec![GrokAcc::cache_key(format!(
            "nabu-live-{}",
            std::process::id()
        ))],
        ..Default::default()
    };
    let cancel = CancelToken::new();
    let mut request = Request {
        model: model.id,
        messages: vec![Message::new(
            Role::User,
            vec![Block::text(
                "Use the temperature tool to read chamber alpha and tell me the temperature in Celsius.",
            )],
        )],
        tools: vec![ToolDef::Function(Tool {
            name: "read_fixture_temperature".into(),
            description: Some("Read the temperature in a synthetic test chamber.".into()),
            schema: json!({"type":"object","properties":{"chamber":{"type":"string"}},"required":["chamber"],"additionalProperties":false}),
            strict: Some(true),
            cache: None,
            ext: Default::default(),
        })],
        tool_choice: Some(ToolChoice::Tool("read_fixture_temperature".into())),
        stream: true,
        ..Default::default()
    };
    let mut generation = core
        .generate(&endpoint, &request, &options, &cancel)
        .await
        .unwrap();
    let mut acc = Accumulator::new();
    while let Some(event) = generation.next().await {
        acc.push(&event.unwrap());
    }
    let first = generation.take_response().unwrap_or_else(|| acc.finish());
    let call = first
        .content
        .iter()
        .find_map(|block| match block {
            Block::ToolCall(call) => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(call.name, "read_fixture_temperature");
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&call.args).unwrap(),
        json!({"chamber":"alpha"})
    );
    let result = Block::ToolResult(ToolResult {
        call_id: call.id.clone(),
        name: Some(call.name.clone()),
        content: vec![Block::text("{\"celsius\":37.4}")],
        is_error: false,
        cache: None,
    });
    request
        .messages
        .push(Message::new(Role::Assistant, first.content));
    request
        .messages
        .push(Message::new(Role::User, vec![result]));
    request.tool_choice = Some(ToolChoice::None);
    request.stream = false;
    let mut generation = core
        .generate(&endpoint, &request, &options, &cancel)
        .await
        .unwrap();
    let response = generation.take_response().expect("complete response");
    assert!(plain_text(&response.content).contains("37.4"));
    assert!(response.usage.cache_read.unwrap_or(0) > 0);
    assert!(plugin.free_quota().used_tokens > 0);
    println!("tool round trip and cached usage confirmed");
}
