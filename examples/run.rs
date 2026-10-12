//! Run one prompt with any supported CLI.
//! Usage: cargo run --example run -- <claude|codex|devin|opencode> [model]

use std::time::Duration;

use agent_core::{AgentCli, Provider, RunRequest};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let provider = match args.next().as_deref() {
        Some("claude") => Provider::Claude,
        Some("codex") => Provider::Codex,
        Some("devin") => Provider::Devin,
        Some("opencode") => Provider::OpenCodeV2,
        _ => {
            eprintln!("Usage: cargo run --example run -- <claude|codex|devin|opencode> [model]");
            std::process::exit(2);
        }
    };
    let model = args.next();
    if args.next().is_some() {
        eprintln!("Expected at most one model argument");
        std::process::exit(2);
    }

    let agent = AgentCli::discover(provider)?;
    let result = agent
        .run(RunRequest {
            prompt: "Summarize this repository in three sentences.".into(),
            cwd: std::env::current_dir()?,
            model,
            timeout: Duration::from_secs(300),
            env_remove: vec![],
            json_schema: None,
            ..RunRequest::default()
        })
        .await?;

    println!("{}", result.text);
    if let Some(usage) = result.usage {
        eprintln!("Tokens: {usage:?}");
    }
    Ok(())
}
