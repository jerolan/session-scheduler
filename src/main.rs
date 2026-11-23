use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
};
use uuid::Uuid;

struct Agent(Child);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
type Sessions = Arc<Mutex<BTreeMap<String, Value>>>;
fn result(value: Value, error: bool) -> Value {
    json!({"content":[{"type":"text", "text":value.to_string()}], "isError":error})
}
fn update(sessions: &Sessions, id: &str, f: impl FnOnce(&mut Value)) {
    if let Some(session) = sessions.lock().unwrap().get_mut(id) {
        f(session);
    }
}
fn send(writer: &mut impl Write, value: Value) -> io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
}
fn run_acp(sessions: Sessions, id: String, workspace: PathBuf, prompt: String, command: PathBuf) {
    let mut submitted = false;
    let run = || -> io::Result<()> {
        let mut child = Agent(
            Command::new(command)
                .current_dir(&workspace)
                .env("INITIAL_AGENT_MODE", "read-only")
                .env_remove("CODEX_CONFIG")
                .env_remove("APP_SERVER_LOGS")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        );
        let mut input = child
            .0
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing stdin"))?;
        let output = child
            .0
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        send(
            &mut input,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"session-scheduler","version":"0.1.0"}}}),
        )?;
        let mut phase = 1;
        let mut acp_session_id: Option<String> = None;
        for line in BufReader::new(output).lines() {
            let line = line?;
            let message: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if method == "session/request_permission" {
                    if let Some(request_id) = message.get("id") {
                        send(
                            &mut input,
                            json!({"jsonrpc":"2.0","id":request_id,"result":{"outcome":{"outcome":"cancelled"}}}),
                        )?;
                    }
                } else if method == "session/update" {
                    if message["params"]["sessionId"].as_str() != acp_session_id.as_deref()
                        || acp_session_id.is_none()
                    {
                        continue;
                    }
                    let u = &message["params"]["update"];
                    if u["sessionUpdate"] == "agent_message_chunk" && u["content"]["type"] == "text"
                    {
                        if let Some(text) = u["content"]["text"].as_str() {
                            update(&sessions, &id, |s| {
                                let old = s["result"].as_str().unwrap_or("");
                                s["result"] = json!(format!("{old}{text}"));
                                let history = s["history"].as_array_mut().unwrap();
                                if let Some(last) =
                                    history.last_mut().filter(|entry| entry["role"] == "agent")
                                {
                                    let old = last["text"].as_str().unwrap_or("");
                                    last["text"] = json!(format!("{old}{text}"));
                                } else {
                                    history.push(json!({"role":"agent", "text":text}));
                                }
                            });
                        }
                    } else if matches!(
                        u["sessionUpdate"].as_str(),
                        Some("tool_call" | "tool_call_update")
                    ) {
                        if let (Some(tool_id), Some(status)) =
                            (u["toolCallId"].as_str(), u["status"].as_str())
                        {
                            if matches!(status, "pending" | "in_progress" | "completed" | "failed")
                            {
                                update(&sessions, &id, |s| {
                                    s["history"].as_array_mut().unwrap().push(json!({"type":"tool_call", "tool_call_id":tool_id, "status":status}))
                                });
                            }
                        }
                    }
                } else if let Some(request_id) = message.get("id") {
                    send(
                        &mut input,
                        json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Method not supported"}}),
                    )?;
                }
                continue;
            }
            if message["id"].as_u64() != Some(phase) {
                continue;
            }
            if message.get("error").is_some() {
                submitted = false; // A matching response confirms the request failed.
                return Err(io::Error::other("ACP request failed"));
            }
            match phase {
                1 => {
                    send(
                        &mut input,
                        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":workspace,"mcpServers":[]}}),
                    )?;
                    phase = 2;
                }
                2 => {
                    let acp_id = message["result"]["sessionId"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("missing ACP session"))?;
                    update(&sessions, &id, |s| s["acp_session_id"] = json!(acp_id));
                    acp_session_id = Some(acp_id.to_owned());
                    // The running turn is already visible before sending the prompt.
                    submitted = true;
                    send(
                        &mut input,
                        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":acp_id,"prompt":[{"type":"text","text":prompt}]}}),
                    )?;
                    phase = 3;
                }
                3 => {
                    let reason = message["result"]["stopReason"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("missing stop reason"))?;
                    update(&sessions, &id, |s| {
                        s["status"] = json!(match reason {
                            "end_turn" => "completed",
                            "cancelled" => "cancelled",
                            _ => "failed",
                        });
                        s["stop_reason"] = json!(reason);
                    });
                    return Ok(());
                }
                _ => unreachable!(),
            }
        }
        Err(io::Error::other("ACP connection closed"))
    };
    if run().is_err() {
        update(&sessions, &id, |s| {
            s["status"] = json!(if submitted { "unknown" } else { "failed" });
        });
    }
}
fn main() {
    let allowed: Vec<PathBuf> = env::var_os("SESSION_SCHEDULER_WORKSPACES")
        .map(|v| {
            env::split_paths(&v)
                .filter_map(|p| p.canonicalize().ok())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    let command = env::var_os("SESSION_SCHEDULER_ACP_COMMAND")
        .map(PathBuf::from)
        .and_then(|p| p.canonicalize().ok());
    let sessions: Sessions = Arc::new(Mutex::new(BTreeMap::new()));
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = request.get("id") else {
            continue;
        };
        let method = request["method"].as_str().unwrap_or("");
        let mut launch = None;
        let response = match method {
            "initialize" => {
                let version = if request["params"]["protocolVersion"] == "2025-03-26" {
                    "2025-03-26"
                } else {
                    "2025-11-25"
                };
                json!({"protocolVersion":version,"capabilities":{"tools":{}},"serverInfo":{"name":"session-scheduler","version":"0.1.0"}})
            }
            "ping" => json!({}),
            "tools/list" => {
                json!({"tools":[{"name":"delegate","description":"Delegate work to Codex","inputSchema":{"type":"object","properties":{"workspace":{"type":"string"},"prompt":{"type":"string"}},"required":["workspace","prompt"]}},{"name":"list_sessions","description":"List sessions","inputSchema":{"type":"object","properties":{}}},{"name":"inspect_session","description":"Inspect a session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}]})
            }
            "tools/call" => {
                let args = &request["params"]["arguments"];
                match request["params"]["name"].as_str().unwrap_or("") {
                    "list_sessions" => result(
                        json!(sessions
                            .lock()
                            .unwrap()
                            .values()
                            .map(|s| json!({"session_id":s["session_id"],"turn_id":s["turn_id"],"workspace":s["workspace"],"status":s["status"]}))
                            .collect::<Vec<_>>()),
                        false,
                    ),
                    "inspect_session" => match args["session_id"]
                        .as_str()
                        .and_then(|id| sessions.lock().unwrap().get(id).cloned())
                    {
                        Some(s) => result(s, false),
                        None => result(json!("Session not found"), true),
                    },
                    "delegate" => {
                        let path = args["workspace"]
                            .as_str()
                            .and_then(|p| PathBuf::from(p).canonicalize().ok());
                        if !path.as_ref().is_some_and(|p| allowed.contains(p)) {
                            result(json!("Workspace is not allowed"), true)
                        } else if let (Some(command), Some(prompt)) = (
                            command.clone(),
                            args["prompt"].as_str().filter(|p| !p.is_empty()),
                        ) {
                            let workspace = path.unwrap();
                            let sid = Uuid::new_v4().to_string();
                            let tid = Uuid::new_v4().to_string();
                            let record = json!({"session_id":sid,"turn_id":tid,"workspace":workspace,"status":"running","result":"","history":[{"role":"user","text":prompt}],"acp_session_id":null,"stop_reason":null});
                            sessions.lock().unwrap().insert(sid.clone(), record);
                            let shared = Arc::clone(&sessions);
                            let worker_id = sid.clone();
                            let prompt = prompt.to_owned();
                            launch = Some((shared, worker_id, workspace, prompt, command));
                            result(json!({"session_id":sid,"turn_id":tid}), false)
                        } else {
                            result(json!("Adapter or prompt unavailable"), true)
                        }
                    }
                    _ => result(json!("Unknown tool"), true),
                }
            }
            _ => {
                let response = json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}});
                println!("{response}");
                let _ = io::stdout().flush();
                continue;
            }
        };
        let mut stdout = io::stdout().lock();
        if send(
            &mut stdout,
            json!({"jsonrpc":"2.0","id":id,"result":response}),
        )
        .is_err()
        {
            break;
        }
        if let Some((shared, worker_id, workspace, prompt, command)) = launch {
            thread::spawn(move || run_acp(shared, worker_id, workspace, prompt, command));
        }
    }
}
