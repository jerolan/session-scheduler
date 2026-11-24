// Standalone synthetic ACP agent; build with rustc, without Cargo dependencies.
use std::io::{self, BufRead, Write};
fn main() {
    let mut lines = io::stdin().lock().lines();
    let session_id = format!("fixture-{}", std::process::id());
    let mut remembered = false;
    while let Some(Ok(line)) = lines.next() {
        let response = if line.contains("\"initialize\"") {
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{},"authMethods":[]}}"#
        } else if line.contains("\"session/new\"") {
            remembered = false;
            println!(r#"{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"{session_id}"}}}}"#);
            io::stdout().flush().unwrap();
            continue;
        } else if line.contains("\"session/prompt\"") {
            let id = line
                .split_once(r#""id":"#)
                .and_then(|(_, tail)| tail.split([',', '}']).next())
                .and_then(|id| id.trim().parse::<u64>().ok())
                .expect("numeric ACP request ID");
            if !line.contains(&format!(r#""sessionId":"{session_id}""#)) {
                std::process::exit(1);
            }
            let outcome = if std::env::var("INITIAL_AGENT_MODE").as_deref() != Ok("read-only")
                || std::env::vars_os().any(|(k, _)| {
                    let key = k.to_string_lossy().to_ascii_uppercase();
                    key.starts_with("SESSION_SCHEDULER_")
                        || key.starts_with("CONTROL_PLANE_")
                        || key == "OPENAI_ADMIN_KEY"
                })
                || line.contains("\"error\"")
            {
                r#""error":{"code":-32603,"message":"fixture failure"}"#
            } else {
                if line.contains("\"permission\"") {
                    println!(
                        r#"{{"jsonrpc":"2.0","id":99,"method":"session/request_permission","params":{{"sessionId":"{session_id}","toolCall":{{"toolCallId":"probe"}},"options":[{{"optionId":"allow","name":"Allow","kind":"allow_once"}}]}}}}"#
                    );
                    io::stdout().flush().unwrap();
                    if !lines.next().is_some_and(|reply| {
                        reply.is_ok_and(|reply| reply.contains("\"cancelled\""))
                    }) {
                        std::process::exit(1);
                    }
                }
                let reply = if line.contains(r#""Remember blue plum""#) {
                    remembered = true;
                    Some("Remembered.")
                } else if line.contains(r#""What did I ask you to remember?""#) && remembered {
                    Some("blue plum")
                } else {
                    None
                };
                if let Some(text) = reply {
                    println!(
                        r#"{{"jsonrpc":"2.0","method":"session/update","params":{{"sessionId":"{session_id}","update":{{"sessionUpdate":"agent_message_chunk","content":{{"type":"text","text":"{text}"}}}}}}}}"#
                    );
                }
                r#""result":{"stopReason":"end_turn"}"#
            };
            println!(r#"{{"jsonrpc":"2.0","id":{id},{outcome}}}"#);
            io::stdout().flush().unwrap();
            continue;
        } else {
            continue;
        };
        println!("{response}");
        io::stdout().flush().unwrap();
    }
}
