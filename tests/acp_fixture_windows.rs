// Standalone synthetic ACP agent; build with rustc, without Cargo dependencies.
use std::io::{self, BufRead, Write};
fn main() {
    for line in io::stdin().lock().lines() {
        let line = line.unwrap();
        let response = if line.contains("\"initialize\"") {
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}"#
        } else if line.contains("\"session/new\"") {
            r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"fixture-session"}}"#
        } else if line.contains("\"session/prompt\"") {
            if std::env::var("INITIAL_AGENT_MODE").as_deref() != Ok("read-only")
                || std::env::vars_os().any(|(k, _)| {
                    k.to_string_lossy().starts_with("SESSION_SCHEDULER_")
                        || k == "CONTROL_PLANE_API_KEY"
                        || k == "OPENAI_ADMIN_KEY"
                })
                || line.contains("\"error\"")
            {
                r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"fixture failure"}}"#
            } else {
                r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#
            }
        } else {
            continue;
        };
        println!("{response}");
        io::stdout().flush().unwrap();
    }
}
