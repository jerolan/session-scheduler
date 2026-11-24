// Standalone synthetic ACP agent; build with rustc, without Cargo dependencies.
use std::io::{self, BufRead, Write};
fn main() {
    let mut lines = io::stdin().lock().lines();
    while let Some(Ok(line)) = lines.next() {
        let response = if line.contains("\"initialize\"") {
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}"#
        } else if line.contains("\"session/new\"") {
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fixture-session"}}"#
        } else if line.contains("\"session/prompt\"") {
            if std::env::var("INITIAL_AGENT_MODE").as_deref() != Ok("read-only")
                || std::env::vars_os().any(|(k, _)| {
                    let key = k.to_string_lossy().to_ascii_uppercase();
                    key.starts_with("SESSION_SCHEDULER_")
                        || key.starts_with("CONTROL_PLANE_")
                        || key == "OPENAI_ADMIN_KEY"
                })
                || line.contains("\"error\"")
            {
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"fixture failure"}}"#
            } else {
                if line.contains("\"permission\"") {
                    let request = r#"{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{"sessionId":"fixture-session","toolCall":{"toolCallId":"probe"},"options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}"#;
                    println!("{request}");
                    io::stdout().flush().unwrap();
                    if !lines.next().is_some_and(|reply| {
                        reply.is_ok_and(|reply| reply.contains("\"cancelled\""))
                    }) {
                        std::process::exit(1);
                    }
                }
                r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#
            }
        } else {
            continue;
        };
        println!("{response}");
        io::stdout().flush().unwrap();
    }
}
