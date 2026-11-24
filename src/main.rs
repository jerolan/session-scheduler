mod events;
mod http;
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
fn run_acp(
    sessions: Sessions,
    id: String,
    workspace: PathBuf,
    prompt: String,
    command: PathBuf,
    events: Option<Arc<events::Events>>,
) {
    let mut submitted = false;
    let run = || -> io::Result<()> {
        let mut agent_command = Command::new(command);
        for (key, _) in env::vars_os() {
            if key.to_string_lossy().starts_with("SESSION_SCHEDULER_")
                || key == "CONTROL_PLANE_API_KEY"
                || key == "OPENAI_ADMIN_KEY"
            {
                agent_command.env_remove(key);
            }
        }
        let mut child = Agent(
            agent_command
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
    if let Some(events) = events {
        let record = sessions.lock().unwrap().get(&id).cloned();
        if let Some(record) = record {
            let reason = match (record["status"].as_str(), record["stop_reason"].as_str()) {
                (Some("completed"), _) => Some("end_turn"),
                (Some("cancelled"), _) => Some("cancelled"),
                (Some("failed"), Some("refusal")) => Some("refusal"),
                (Some("failed"), Some("max_tokens")) => Some("max_tokens"),
                (Some("failed"), Some("max_turn_requests")) => Some("max_turn_requests"),
                (Some("failed"), _) => Some("failed"),
                _ => None,
            };
            if let (Some(reason), Some(turn_id)) = (reason, record["turn_id"].as_str()) {
                events.complete(&id, turn_id, reason);
            }
        }
    }
}
type Launch = (
    Sessions,
    String,
    PathBuf,
    String,
    PathBuf,
    Option<Arc<events::Events>>,
);
fn launch(job: Launch) {
    let (shared, id, workspace, prompt, command, events) = job;
    thread::spawn(move || run_acp(shared, id, workspace, prompt, command, events));
}
struct State {
    allowed: Vec<PathBuf>,
    command: Option<PathBuf>,
    sessions: Sessions,
    events: Option<Arc<events::Events>>,
}
impl State {
    fn new(events: Option<Arc<events::Events>>) -> Self {
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
        Self {
            allowed,
            command,
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            events,
        }
    }
    fn dispatch(&self, request: Value) -> (Option<Value>, Option<Launch>) {
        let Some(id) = request.get("id") else {
            return (None, None);
        };
        let method = request["method"].as_str().unwrap_or("");
        if let Some(events) = &self.events {
            if let Some(response) = events.dispatch(method, &request["params"]) {
                return (
                    Some(match response {
                        Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
                        Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
                    }),
                    None,
                );
            }
        }
        let allowed = &self.allowed;
        let command = &self.command;
        let sessions = &self.sessions;
        let mut launch = None;
        let response = match method {
            "initialize" => {
                let version = if request["params"]["protocolVersion"] == "2025-03-26" {
                    "2025-03-26"
                } else {
                    "2025-11-25"
                };
                let capabilities = if self.events.is_some() {
                    json!({"tools":{},"events":{}})
                } else {
                    json!({"tools":{}})
                };
                json!({"protocolVersion":version,"capabilities":capabilities,"serverInfo":{"name":"session-scheduler","version":"0.1.0"}})
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
                            let shared = Arc::clone(sessions);
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
                return (
                    Some(
                        json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
                    ),
                    None,
                );
            }
        };
        let job = launch.map(|(shared, id, workspace, prompt, command)| {
            (shared, id, workspace, prompt, command, self.events.clone())
        });
        (
            Some(json!({"jsonrpc":"2.0","id":id,"result":response})),
            job,
        )
    }
}
fn main() {
    if env::var_os("SESSION_SCHEDULER_HTTP_PORT").is_some() {
        if let Err(error) = http::serve() {
            eprintln!("HTTP server: {error}");
            std::process::exit(1);
        }
        return;
    }
    let state = State::new(None);
    for line in io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let (response, job) = state.dispatch(request);
        if let Some(response) = response {
            if send(&mut io::stdout().lock(), response).is_err() {
                break;
            }
            if let Some(job) = job {
                launch(job);
            }
        }
    }
}
