//! Prompt cache over a multi-step run against Antigravity.
//! Run with `cargo test -p ncore-antigravity --test live_cache -- --ignored --nocapture`.

use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use ncore::core::ir::{Block, Message, Request, Role, Tool, ToolCall, ToolDef, ToolResult};
use ncore::rt::BoxFuture;
use ncore::{
    Agent, CacheTracker, CancelToken, LoopEvent, RetryPolicy, RunOptions, RunStop, Tools,
};
use ncore_antigravity::{Account, Antigravity};
use ncore_tokio::TokioClock;
use serde_json::json;

fn antigravity() -> Antigravity {
    let path = format!(
        "{}/.reproxy/antigravity-accounts.json",
        std::env::var("HOME").unwrap()
    );
    Antigravity::new(Account::parse_file(&std::fs::read(path).unwrap()).unwrap())
}

/// Hands out one code word per call.
struct Words;

impl Tools for Words {
    fn definitions(&self) -> Vec<ToolDef> {
        vec![ToolDef::Function(Tool {
            name: "next_word".into(),
            description: Some("Returns the code word for step n.".into()),
            schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}),
            strict: None,
            cache: None,
            ext: Default::default(),
        })]
    }

    fn call<'a>(&'a self, call: &'a ToolCall, _: &'a CancelToken) -> BoxFuture<'a, ToolResult> {
        Box::pin(async move {
            let n = serde_json::from_str::<serde_json::Value>(&call.args)
                .ok()
                .and_then(|v| v["n"].as_u64())
                .unwrap_or(0);
            let word = ["amber", "birch", "cobalt", "dune", "ember"]
                .get(n.saturating_sub(1) as usize)
                .copied()
                .unwrap_or("none");
            ToolResult {
                call_id: call.id.clone(),
                name: None,
                content: vec![Block::text(word)],
                is_error: false,
                cache: None,
            }
        })
    }
}

fn system() -> String {
    // Well past every provider's minimum cacheable prefix.
    let mut s = String::from("You are a careful assistant. Reference facts follow.\n");
    for i in 0..400 {
        s.push_str(&format!(
            "Fact {i}: the archive shelf {i} holds {} folders labelled with the colour {}.\n",
            i * 7 % 23,
            ["red", "green", "blue", "grey"][i % 4]
        ));
    }
    s
}

#[tokio::test]
#[ignore = "talks to Antigravity with the user's accounts"]
async fn live_cache_grows_over_steps() {
    let core = ncore_tokio::core(Some(RetryPolicy::default()));
    let ep = Antigravity::endpoint().with_plugin(Arc::new(antigravity()));
    let models = std::env::var("NABU_LIVE_MODELS")
        .unwrap_or_else(|_| "gemini-3-flash,claude-sonnet-4-6".into());
    for model in models.split(',') {
        println!("=== {model}");
        let tracker = Arc::new(Mutex::new(CacheTracker::new()));
        let agent = Agent::new(&core, &ep, Arc::new(TokioClock))
            .tools(&Words)
            .tracker(tracker);
        let req = Request {
            model: model.into(),
            system: vec![Block::text(system())],
            messages: vec![Message::new(
                Role::User,
                vec![Block::text(
                    "Call next_word with n=1, then n=2, then n=3, then n=4, one call per turn, \
                     waiting for each result. Then answer with the four words joined by dashes.",
                )],
            )],
            max_tokens: Some(2048),
            stream: true,
            ..Default::default()
        };
        let mut run = agent.run(req, RunOptions::default());
        let mut breaks = Vec::new();
        let mut reads = Vec::new();
        let mut out = None;
        while let Some(e) = run.next().await {
            match e {
                LoopEvent::StepEnd { step, usage, .. } => {
                    println!(
                        "  step {step}: input {:?} cache_read {:?} cache_write {:?}",
                        usage.input_total, usage.cache_read, usage.cache_write
                    );
                    reads.push(usage.cache_read.unwrap_or(0));
                }
                LoopEvent::CacheBreak { step, info } => {
                    println!("  step {step}: break {info:?}");
                    breaks.push(info);
                }
                LoopEvent::CacheMiss { step, info } => println!("  step {step}: miss {info:?}"),
                LoopEvent::Done(o) => out = Some(o),
                _ => {}
            }
        }
        let out = out.unwrap();
        let answer = out
            .messages
            .last()
            .map(|m| ncore::core::ir::plain_text(&m.content))
            .unwrap_or_default();
        println!(
            "  stop {:?} steps {} answer {answer:?}",
            out.stop, out.steps
        );
        assert!(matches!(out.stop, RunStop::Finished(_)), "{:?}", out.stop);
        assert!(breaks.is_empty(), "the loop only appends: {breaks:?}");
        assert!(out.steps >= 3);
        // Later steps should read the prefix the earlier ones wrote.
        assert!(
            reads[1..].iter().any(|&r| r > 0),
            "no cache reads: {reads:?}"
        );
    }
}
