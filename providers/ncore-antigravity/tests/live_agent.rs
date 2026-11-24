//! Agent loop against Antigravity and a real MCP filesystem server.
//! Run with `MCP_BIN=<dir with mcp-server-filesystem> cargo test -p ncore-antigravity --test live_agent -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use ncore::core::ir::{Block, Message, Request, Role, StreamEvent};
use ncore::{Agent, LoopEvent, RetryPolicy, RunOptions, RunStop};
use ncore_antigravity::{Account, Antigravity};
use ncore_mcp::{McpClient, ResultLimits, Runtime, ServerConfig, Toolbox};
use ncore_tokio::{TokioClock, TokioLauncher, TokioSpawner};

fn antigravity() -> Antigravity {
    let path = format!(
        "{}/.reproxy/antigravity-accounts.json",
        std::env::var("HOME").unwrap()
    );
    Antigravity::new(Account::parse_file(&std::fs::read(path).unwrap()).unwrap())
}

async fn filesystem(root: &str) -> McpClient {
    let bin = format!(
        "{}/mcp-server-filesystem",
        std::env::var("MCP_BIN").unwrap()
    );
    let mut rt = Runtime::new(Arc::new(TokioClock), Arc::new(TokioSpawner));
    rt.launcher = Some(Arc::new(TokioLauncher));
    McpClient::connect(ServerConfig::stdio("fs", bin, &[root]), rt)
        .await
        .unwrap()
}

fn request(model: &str, root: &str, task: &str) -> Request {
    Request {
        model: model.into(),
        system: vec![Block::text(format!(
            "You work with files under {root}. Use the tools; always pass absolute paths."
        ))],
        messages: vec![Message::new(Role::User, vec![Block::text(task)])],
        max_tokens: Some(4096),
        stream: true,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "talks to Antigravity with the user's accounts and needs MCP_BIN"]
async fn live_multi_step_task() {
    let dir = std::env::temp_dir().join(format!("ncore-agent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "17").unwrap();
    std::fs::write(dir.join("b.txt"), "25").unwrap();
    let root = std::fs::canonicalize(&dir)
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let client = filesystem(&root).await;
    let mut tb = Toolbox::new(ResultLimits::default());
    tb.add("fs", Arc::new(client.clone())).unwrap();
    assert!(tb.refresh().await.failed.is_empty());

    let core = ncore_tokio::core(Some(RetryPolicy::default()));
    let plugin = Arc::new(antigravity());
    let ep = Antigravity::endpoint().with_plugin(plugin);
    let agent = Agent::new(&core, &ep, Arc::new(TokioClock)).tools(&tb);
    let models = std::env::var("NABU_LIVE_MODELS")
        .unwrap_or_else(|_| "gemini-3-flash,claude-sonnet-4-6".into());

    for model in models.split(',') {
        let _ = std::fs::remove_file(dir.join("sum.txt"));
        println!("=== {model}");
        let task = "Read a.txt and b.txt, write their sum as plain digits into sum.txt, \
                    then read sum.txt back to check it and answer with the number.";
        let mut run = agent.run(request(model, &root, task), RunOptions::default());
        let mut out = None;
        while let Some(e) = run.next().await {
            match e {
                LoopEvent::ToolStart { step, call } => {
                    println!("  step {step} call {} {}", call.name, call.args)
                }
                LoopEvent::ToolEnd { result, .. } if result.is_error => println!(
                    "  error {}",
                    ncore::core::ir::plain_text(&result.content)
                ),
                LoopEvent::StreamRestart { error, .. } => println!("  restart: {error}"),
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
            "  stop {:?} steps {} usage {:?}\n  answer {answer:?}",
            out.stop, out.steps, out.usage
        );
        assert!(matches!(out.stop, RunStop::Finished(_)), "{:?}", out.stop);
        assert_eq!(
            std::fs::read_to_string(dir.join("sum.txt")).unwrap().trim(),
            "42"
        );
        assert!(answer.contains("42"));
        assert!(out.steps >= 2);

        // Cancel as soon as the model starts its first tool call, then continue from the history.
        let opts = RunOptions::default();
        let cancel = opts.cancel.clone();
        let mut history = request(
            model,
            &root,
            "List the directory, then tell me how many files it has.",
        );
        let mut run = agent.run(history.clone(), opts);
        let mut cut = None;
        while let Some(e) = run.next().await {
            match e {
                LoopEvent::Stream {
                    event: StreamEvent::BlockStart { .. },
                    step: 1,
                } => cancel.cancel(),
                LoopEvent::Done(o) => cut = Some(o),
                _ => {}
            }
        }
        let cut = cut.unwrap();
        println!(
            "  cancelled: stop {:?}, {} messages kept, partial {}",
            cut.stop,
            cut.messages.len(),
            cut.partial.is_some()
        );
        assert!(matches!(cut.stop, RunStop::Cancelled));
        history.messages.extend(cut.messages);
        let resumed = tokio::time::timeout(
            Duration::from_secs(180),
            agent.run(history, RunOptions::default()).complete(),
        )
        .await
        .unwrap();
        println!(
            "  resumed: stop {:?} answer {:?}",
            resumed.stop,
            resumed
                .messages
                .last()
                .map(|m| ncore::core::ir::plain_text(&m.content))
        );
        assert!(
            matches!(resumed.stop, RunStop::Finished(_)),
            "{:?}",
            resumed.stop
        );
    }
    client.close().await;
    std::fs::remove_dir_all(&dir).unwrap();
}
