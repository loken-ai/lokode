//! Headless smoke test: drive the verve agent loop against a real
//! backend (default 127.0.0.1:11435) with an auto-approve stub UI.
//!
//! Usage:
//!   cargo run -p verve --example smoke -- \
//!     --model qwen3-coder:latest \
//!     --root /tmp/verve-sandbox \
//!     --prompt "list the files in src/ and tell me what's there"

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use verve::agent::agent_loop::{Agent, AgentConfig, AgentEvent};
use verve::agent::session::Session;
use verve::backend::client::BackendClient;
use verve::sandbox::approval::{self, ApprovalDecision, SessionApprovals};
use verve::tools::{
    bash::Bash,
    fs_read::{Glob, ListDir, ReadFile},
    fs_write::{ApplyPatch, EditFile, WriteFile},
    lsp::{LspDiagnostics, LspHover, LspPool},
    registry::{Registry, ToolCtx},
    search::Grep,
    tests::RunTests,
    web_fetch::WebFetch,
};

#[derive(Parser, Debug)]
struct Cli {
    #[arg(short, long)]
    model: String,
    #[arg(short, long, default_value = ".")]
    root: PathBuf,
    #[arg(short = 'u', long, default_value = "http://127.0.0.1:11435")]
    backend_url: String,
    #[arg(short, long)]
    prompt: String,
    #[arg(long, default_value = "8")]
    max_steps: usize,

    /// Context window the backend offers, in tokens.
    #[arg(long, default_value = "8192")]
    context_tokens: usize,

    /// Tokens held back from the window for the answer.
    #[arg(long, default_value = "4096")]
    max_answer_tokens: usize,
    /// Approve mutating tools too (default: approve only read-only).
    #[arg(long)]
    approve_writes: bool,
    /// Disable Layer 1 grammar to exercise the Layer 2/3 repair pipeline.
    #[arg(long)]
    no_grammar: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn,verve=info"))
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let root = cli.root.canonicalize()?;

    let (approval_tx, mut approval_rx) = approval::channel();
    let session_approvals = Arc::new(SessionApprovals::new());
    // Nothing cancels this run, but the turn still needs a token to carry.
    let cancel = CancellationToken::new();

    let mut reg = Registry::new();
    reg.register(ReadFile);
    reg.register(ListDir);
    reg.register(Glob);
    reg.register(Grep);
    reg.register(WriteFile);
    reg.register(EditFile);
    reg.register(ApplyPatch);
    reg.register(Bash);
    reg.register(RunTests);
    reg.register(WebFetch);
    let pool = LspPool::new(root.clone());
    reg.register(LspDiagnostics { pool: pool.clone() });
    reg.register(LspHover { pool });
    let registry = Arc::new(reg);

    let ctx = ToolCtx {
        root: root.clone(),
        approval: approval_tx.clone(),
        session_approvals,
        max_output_chars: verve::agent::session::tool_output_chars(
            cli.context_tokens,
            cli.max_answer_tokens,
        ),
        cancel: cancel.clone(),
        http: verve::tools::registry::tool_http_client()?,
        allow_hosts: Arc::new(Default::default()),
    };

    let mut agent = Agent {
        backend: BackendClient::new(cli.backend_url),
        registry: registry.clone(),
        session: Session::new(Agent::build_system_prompt(&registry), cli.context_tokens, cli.max_answer_tokens),
        cfg: AgentConfig {
            model: cli.model,
            grammar_enabled: !cli.no_grammar,
            max_steps: cli.max_steps,
            max_answer_tokens: cli.max_answer_tokens,
        },
        ctx,
    };

    // Auto-approve stub: read-only tools never prompt (handled inside each
    // tool), so anything coming through this channel is mutating.
    let approve_writes = cli.approve_writes;
    tokio::spawn(async move {
        while let Some(prompt) = approval_rx.recv().await {
            let decision = if approve_writes {
                ApprovalDecision::Always
            } else {
                ApprovalDecision::Deny
            };
            eprintln!(
                "[smoke] approval-stub: {} for tool '{}'  (summary: {})",
                if approve_writes { "ALLOW" } else { "DENY" },
                prompt.req.tool,
                prompt.req.summary
            );
            let _ = prompt.reply.send(decision);
        }
    });

    let (evt_tx, mut evt_rx) = mpsc::channel::<AgentEvent>(128);
    let driver = tokio::spawn(async move {
        agent.run_user_turn(cli.prompt, cancel, evt_tx).await
    });

    let mut streaming = String::new();
    while let Some(evt) = evt_rx.recv().await {
        match evt {
            AgentEvent::AssistantDelta(s) => streaming.push_str(&s),
            AgentEvent::AssistantText(s) => {
                if !streaming.is_empty() {
                    println!("\n[stream-raw]\n{streaming}\n");
                    streaming.clear();
                }
                println!("[assistant]\n{s}\n");
            }
            AgentEvent::ToolCall { name, args_pretty } => {
                println!("[→ tool] {name}\n{args_pretty}\n");
            }
            AgentEvent::ToolResult { name, output } => {
                let tag = if output.is_error { "ERROR" } else { "ok" };
                println!("[← {name} {tag}]\n{}\n", output.content);
            }
            AgentEvent::Notice(s) => println!("[notice] {s}"),
            AgentEvent::TurnDone => {
                println!("[turn-done]");
                break;
            }
        }
    }
    driver.await??;
    Ok(())
}
