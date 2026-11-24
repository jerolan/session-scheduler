mod events;
mod http;
mod state_files;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    io::{self, BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc, Mutex, Weak},
    thread,
    time::Duration,
};
use uuid::Uuid;

#[derive(Default)]
struct ChildRegistry {
    stopping: bool,
    children: BTreeMap<String, Arc<Mutex<Child>>>,
}
type Children = Arc<Mutex<ChildRegistry>>;
struct Agent(Arc<Mutex<Child>>);
impl Drop for Agent {
    fn drop(&mut self) {
        let mut child = self.0.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
    }
}
#[derive(Clone, Copy)]
enum AgentKind {
    Codex,
    OpenCode,
}
impl AgentKind {
    fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::OpenCode => "opencode",
        }
    }
}

struct Adapter {
    kind: AgentKind,
    executable: PathBuf,
}
type Sessions = Arc<Mutex<BTreeMap<String, Value>>>;
struct PendingCancel {
    acp_id: String,
    writer: Arc<Mutex<ChildStdin>>,
    sent: bool,
}
type CancelSlot = Arc<Mutex<Option<PendingCancel>>>;
type CancelWriters = Arc<Mutex<BTreeMap<String, CancelSlot>>>;
fn schedule_cancel(slot: CancelSlot, sessions: Sessions, id: String, turn_id: String) {
    thread::spawn(move || {
        let writer = {
            let guard = slot.lock().unwrap();
            guard.as_ref().map(|pending| pending.writer.clone())
        };
        let Some(writer) = writer else {
            return;
        };
        // Serialize with subsequent prompts, but do not hold state locks over I/O.
        let mut writer = writer.lock().unwrap();
        let acp_id = {
            let mut guard = slot.lock().unwrap();
            let records = sessions.lock().unwrap();
            let current = records
                .get(&id)
                .is_some_and(|s| s["turn_id"] == turn_id && s["status"] == "cancelling");
            match guard.as_mut() {
                Some(pending) if current && !pending.sent => {
                    pending.sent = true;
                    Some(pending.acp_id.clone())
                }
                _ => None,
            }
        };
        if let Some(acp_id) = acp_id {
            // A failed cancel write does not disprove an already-buffered prompt response.
            // Keep reading until that response or EOF determines the turn's outcome.
            let _ = send(
                &mut *writer,
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":acp_id}}),
            );
        }
    });
}
fn register_cancel(
    slot: &CancelSlot,
    sessions: &Sessions,
    id: &str,
    turn_id: &str,
    acp_id: &str,
    input: &Arc<Mutex<ChildStdin>>,
) {
    let cancelling = {
        let mut guard = slot.lock().unwrap();
        *guard = Some(PendingCancel {
            acp_id: acp_id.to_owned(),
            writer: input.clone(),
            sent: false,
        });
        sessions
            .lock()
            .unwrap()
            .get(id)
            .is_some_and(|s| s["turn_id"] == turn_id && s["status"] == "cancelling")
    };
    if cancelling {
        schedule_cancel(
            slot.clone(),
            sessions.clone(),
            id.to_owned(),
            turn_id.to_owned(),
        );
    }
}
type Workers = Arc<Mutex<BTreeMap<String, mpsc::Sender<Turn>>>>;
struct Turn {
    id: String,
    prompt: String,
}

fn result(value: Value, error: bool) -> Value {
    json!({"content":[{"type":"text", "text":value.to_string()}], "isError":error})
}
fn update(sessions: &Sessions, id: &str, f: impl FnOnce(&mut Value)) {
    if let Some(session) = sessions.lock().unwrap().get_mut(id) {
        f(session);
    }
}
fn finish_turn(
    sessions: &Sessions,
    events: Option<&events::Events>,
    id: &str,
    turn_id: &str,
    status: &'static str,
    stop_reason: Option<&str>,
    event_reason: &str,
) {
    if events.is_some() {
        update(sessions, id, |record| {
            record["status"] = json!("completion_pending")
        });
    }
    let publish = {
        let sessions = sessions.clone();
        let id = id.to_owned();
        let turn_id = turn_id.to_owned();
        let stop_reason = json!(stop_reason);
        move || {
            update(&sessions, &id, |record| {
                if record["turn_id"] == turn_id {
                    record["status"] = json!(status);
                    record["stop_reason"] = stop_reason;
                }
            });
        }
    };
    if let Some(events) = events {
        if let Err(error) = events.complete(id, turn_id, event_reason, publish) {
            eprintln!("could not persist completion of turn {turn_id}: {error}");
        }
    } else {
        publish();
    }
}

fn send(writer: &mut impl Write, value: Value) -> io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
}
struct AcpJob {
    sessions: Sessions,
    id: String,
    workspace: PathBuf,
    first: Turn,
    receiver: mpsc::Receiver<Turn>,
    workers: Weak<Mutex<BTreeMap<String, mpsc::Sender<Turn>>>>,
    children: Weak<Mutex<ChildRegistry>>,
    adapter: Adapter,
    events: Option<Arc<events::Events>>,
    cancel_writers: CancelWriters,
}

fn run_acp(job: AcpJob) {
    let AcpJob {
        sessions,
        id,
        workspace,
        first,
        receiver,
        workers,
        children,
        adapter,
        events,
        cancel_writers,
    } = job;
    let cancel_slot = cancel_writers.lock().unwrap().get(&id).unwrap().clone();
    let mut submitted = false;
    let mut turn = first;
    let run = || -> io::Result<()> {
        let mut agent_command = Command::new(adapter.executable);
        match adapter.kind {
            AgentKind::Codex => agent_command.env("INITIAL_AGENT_MODE", "read-only"),
            AgentKind::OpenCode => agent_command.arg("acp"),
        };
        for (key, _) in env::vars_os() {
            // Windows environment keys are case-insensitive.
            let name = key.to_string_lossy().to_ascii_uppercase();
            if name.starts_with("SESSION_SCHEDULER_")
                || name.starts_with("CONTROL_PLANE_")
                || name == "OPENAI_ADMIN_KEY"
            {
                agent_command.env_remove(key);
            }
        }
        let registry = children
            .upgrade()
            .ok_or_else(|| io::Error::other("scheduler stopped"))?;
        let mut registry = registry.lock().unwrap();
        if registry.stopping {
            return Err(io::Error::other("scheduler stopped"));
        }
        let child = Agent(Arc::new(Mutex::new(
            agent_command
                .current_dir(&workspace)
                .env_remove("CODEX_CONFIG")
                .env_remove("APP_SERVER_LOGS")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()?,
        )));
        registry.children.insert(id.clone(), child.0.clone());
        drop(registry);
        let input = child
            .0
            .lock()
            .unwrap()
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing stdin"))?;
        let input = Arc::new(Mutex::new(input));
        let output = child
            .0
            .lock()
            .unwrap()
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        send(
            &mut *input.lock().unwrap(),
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{},"clientInfo":{"name":"session-scheduler","version":"0.1.0"}}}),
        )?;
        let mut phase = 1;
        let mut request_id = 1;
        let mut acp_session_id: Option<String> = None;
        for line in BufReader::new(output).lines() {
            let line = line?;
            let message: Value = serde_json::from_str(&line).map_err(io::Error::other)?;
            if let Some(method) = message.get("method").and_then(Value::as_str) {
                if method == "session/request_permission" {
                    if let Some(request_id) = message.get("id") {
                        send(
                            &mut *input.lock().unwrap(),
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
                        &mut *input.lock().unwrap(),
                        json!({"jsonrpc":"2.0","id":request_id,"error":{"code":-32601,"message":"Method not supported"}}),
                    )?;
                }
                continue;
            }
            if message["id"].as_u64() != Some(request_id) {
                continue;
            }
            if message.get("error").is_some() {
                submitted = false; // A matching response confirms the request failed.
                return Err(io::Error::other("ACP request failed"));
            }
            match phase {
                1 => {
                    send(
                        &mut *input.lock().unwrap(),
                        json!({"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":workspace,"mcpServers":[]}}),
                    )?;
                    phase = 2;
                    request_id = 2;
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
                        &mut *input.lock().unwrap(),
                        json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":acp_id,"prompt":[{"type":"text","text":turn.prompt}]}}),
                    )?;
                    register_cancel(&cancel_slot, &sessions, &id, &turn.id, acp_id, &input);
                    phase = 3;
                    request_id = 3;
                }
                3 => {
                    let reason = message["result"]["stopReason"]
                        .as_str()
                        .ok_or_else(|| io::Error::other("missing stop reason"))?;
                    let status = match reason {
                        "end_turn" => "completed",
                        "cancelled" => "cancelled",
                        _ => "failed",
                    };
                    let event_reason = match reason {
                        "end_turn" | "cancelled" | "refusal" | "max_tokens"
                        | "max_turn_requests" => reason,
                        _ => "failed",
                    };
                    finish_turn(
                        &sessions,
                        events.as_deref(),
                        &id,
                        &turn.id,
                        status,
                        Some(reason),
                        event_reason,
                    );
                    *cancel_slot.lock().unwrap() = None;
                    turn = loop {
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(turn) => break turn,
                            Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                        let alive = matches!(child.0.lock().unwrap().try_wait(), Ok(None));
                        if alive {
                            continue;
                        }
                        let records = sessions.lock().unwrap();
                        // A follow-up reserves the running turn before queuing it.
                        if records.get(&id).is_some_and(|record| {
                            matches!(record["status"].as_str(), Some("running" | "cancelling"))
                        }) {
                            continue;
                        }
                        if let Some(workers) = workers.upgrade() {
                            workers.lock().unwrap().remove(&id);
                        }
                        return Ok(());
                    };
                    submitted = false;
                    if child.0.lock().unwrap().try_wait()?.is_some() {
                        return Err(io::Error::other("ACP process exited"));
                    }
                    submitted = true;
                    request_id += 1;
                    send(
                        &mut *input.lock().unwrap(),
                        json!({"jsonrpc":"2.0","id":request_id,"method":"session/prompt","params":{"sessionId":acp_session_id,"prompt":[{"type":"text","text":turn.prompt}]}}),
                    )?;
                    register_cancel(
                        &cancel_slot,
                        &sessions,
                        &id,
                        &turn.id,
                        acp_session_id.as_ref().unwrap(),
                        &input,
                    );
                }
                _ => unreachable!(),
            }
        }
        Err(io::Error::other("ACP connection closed"))
    };
    let outcome = run();
    if let Some(children) = children.upgrade() {
        children.lock().unwrap().children.remove(&id);
    }
    cancel_writers.lock().unwrap().remove(&id);
    if outcome.is_err() {
        *cancel_slot.lock().unwrap() = None;
        let mut records = sessions.lock().unwrap();
        if let Some(workers) = workers.upgrade() {
            workers.lock().unwrap().remove(&id);
        }
        if submitted {
            if let Some(record) = records.get_mut(&id) {
                record["status"] = json!("unknown");
            }
        } else {
            drop(records);
            finish_turn(
                &sessions,
                events.as_deref(),
                &id,
                &turn.id,
                "failed",
                None,
                "failed",
            );
        }
    }
}
struct Launch {
    action: Option<Action>,
    sessions: Sessions,
    id: String,
    turn_id: String,
    previous: Option<Value>,
    events: Option<Arc<events::Events>>,
    cancel_writers: CancelWriters,
}
enum Action {
    Start {
        workspace: PathBuf,
        turn: Turn,
        receiver: mpsc::Receiver<Turn>,
        workers: Workers,
        children: Children,
        adapter: Adapter,
    },
    Follow(mpsc::Sender<Turn>, Turn),
}
impl Drop for Launch {
    fn drop(&mut self) {
        if self.action.is_some() {
            let mut sessions = self.sessions.lock().unwrap();
            if let Some(record) = sessions.get_mut(&self.id) {
                if record["turn_id"] == self.turn_id {
                    if let Some(previous) = self.previous.take() {
                        *record = previous;
                    } else {
                        sessions.remove(&self.id);
                        if let Some(Action::Start { workers, .. }) = &self.action {
                            workers.lock().unwrap().remove(&self.id);
                            self.cancel_writers.lock().unwrap().remove(&self.id);
                        }
                    }
                }
            }
        }
    }
}
fn launch(mut job: Launch) {
    match job.action.take().unwrap() {
        Action::Start {
            workspace,
            turn,
            receiver,
            workers,
            children,
            adapter,
        } => {
            let acp = AcpJob {
                sessions: job.sessions.clone(),
                id: job.id.clone(),
                workspace,
                first: turn,
                receiver,
                workers: Arc::downgrade(&workers),
                children: Arc::downgrade(&children),
                adapter,
                events: job.events.clone(),
                cancel_writers: job.cancel_writers.clone(),
            };
            thread::spawn(move || run_acp(acp));
        }
        Action::Follow(sender, turn) => {
            if sender.send(turn).is_err() {
                finish_turn(
                    &job.sessions,
                    job.events.as_deref(),
                    &job.id,
                    &job.turn_id,
                    "failed",
                    None,
                    "failed",
                );
            }
        }
    }
}
struct State {
    allowed: Vec<PathBuf>,
    codex_command: Option<PathBuf>,
    opencode_command: Option<PathBuf>,
    sessions: Sessions,
    workers: Workers,
    children: Children,
    events: Option<Arc<events::Events>>,
    cancel_writers: CancelWriters,
}
impl Drop for State {
    fn drop(&mut self) {
        let mut registry = self.children.lock().unwrap();
        registry.stopping = true;
        for child in registry.children.values() {
            let _ = child.lock().unwrap().kill();
        }
    }
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
        let executable =
            |name| env::var_os(name).and_then(|p| PathBuf::from(p).canonicalize().ok());
        Self {
            allowed,
            codex_command: executable("SESSION_SCHEDULER_ACP_COMMAND"),
            opencode_command: executable("SESSION_SCHEDULER_OPENCODE_COMMAND"),
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            workers: Arc::new(Mutex::new(BTreeMap::new())),
            children: Arc::new(Mutex::new(ChildRegistry::default())),
            cancel_writers: Arc::new(Mutex::new(BTreeMap::new())),
            events,
        }
    }
    fn delegate(&self, args: &Value) -> (Value, Option<Launch>) {
        let Some(workspace) = args["workspace"]
            .as_str()
            .and_then(|p| PathBuf::from(p).canonicalize().ok())
            .filter(|p| self.allowed.contains(p))
        else {
            return (result(json!("Workspace is not allowed"), true), None);
        };
        let (kind, executable) = match args.get("agent") {
            None => (AgentKind::Codex, &self.codex_command),
            Some(agent) if agent == "codex" => (AgentKind::Codex, &self.codex_command),
            Some(agent) if agent == "opencode" => (AgentKind::OpenCode, &self.opencode_command),
            Some(_) => return (result(json!("Unknown agent"), true), None),
        };
        let (Some(executable), Some(prompt)) = (
            executable.clone(),
            args["prompt"].as_str().filter(|p| !p.is_empty()),
        ) else {
            return (result(json!("Adapter or prompt unavailable"), true), None);
        };

        let sid = Uuid::new_v4().to_string();
        let tid = Uuid::new_v4().to_string();
        let record = json!({
            "session_id":sid, "turn_id":tid, "workspace":workspace, "agent":kind.name(),
            "status":"running", "result":"", "history":[{"role":"user","text":prompt}],
            "acp_session_id":null, "stop_reason":null, "archived":false
        });
        self.cancel_writers
            .lock()
            .unwrap()
            .insert(sid.clone(), Arc::new(Mutex::new(None)));
        self.sessions.lock().unwrap().insert(sid.clone(), record);
        let (sender, receiver) = mpsc::channel();
        self.workers.lock().unwrap().insert(sid.clone(), sender);
        let job = Launch {
            action: Some(Action::Start {
                workspace,
                turn: Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                },
                receiver,
                workers: self.workers.clone(),
                children: self.children.clone(),
                adapter: Adapter { kind, executable },
            }),
            sessions: self.sessions.clone(),
            id: sid.clone(),
            turn_id: tid.clone(),
            previous: None,
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(json!({"session_id":sid,"turn_id":tid}), false),
            Some(job),
        )
    }
    fn follow_up(&self, args: &Value) -> (Value, Option<Launch>) {
        let (Some(id), Some(prompt)) = (
            args["session_id"].as_str(),
            args["prompt"].as_str().filter(|p| !p.is_empty()),
        ) else {
            return (result(json!("Session and prompt required"), true), None);
        };
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(id) else {
            return (result(json!("Session not found"), true), None);
        };
        if matches!(
            record["status"].as_str(),
            Some("running" | "cancelling" | "completion_pending" | "unknown")
        ) || record["archived"] == true
        {
            return (result(json!("Session is not available"), true), None);
        }
        let Some(sender) = self.workers.lock().unwrap().get(id).cloned() else {
            return (result(json!("Session is not available"), true), None);
        };
        let usable = self
            .children
            .lock()
            .unwrap()
            .children
            .get(id)
            .map(|child| matches!(child.lock().unwrap().try_wait(), Ok(None)))
            .unwrap_or(false);
        if !usable {
            self.workers.lock().unwrap().remove(id);
            return (result(json!("Session is not available"), true), None);
        }
        let previous = record.clone();
        let tid = Uuid::new_v4().to_string();
        record["turn_id"] = json!(tid);
        record["status"] = json!("running");
        record["result"] = json!("");
        record["stop_reason"] = Value::Null;
        record["history"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","text":prompt}));
        let job = Launch {
            action: Some(Action::Follow(
                sender,
                Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                },
            )),
            sessions: self.sessions.clone(),
            id: id.to_owned(),
            turn_id: tid.clone(),
            previous: Some(previous),
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(json!({"session_id":id,"turn_id":tid}), false),
            Some(job),
        )
    }
    fn cancel_turn(&self, args: &Value) -> Value {
        let (Some(id), Some(turn_id)) = (args["session_id"].as_str(), args["turn_id"].as_str())
        else {
            return result(json!("Session and turn required"), true);
        };
        let mut records = self.sessions.lock().unwrap();
        let Some(record) = records.get_mut(id) else {
            return result(json!("Session not found"), true);
        };
        if record["turn_id"] != turn_id
            || record["status"] != "running"
            || record["archived"] == true
        {
            return result(json!("Turn is not available for cancellation"), true);
        }
        record["status"] = json!("cancelling");
        drop(records);
        if let Some(slot) = self.cancel_writers.lock().unwrap().get(id).cloned() {
            schedule_cancel(
                slot,
                self.sessions.clone(),
                id.to_owned(),
                turn_id.to_owned(),
            );
        }
        result(
            json!({"session_id":id,"turn_id":turn_id,"status":"cancelling"}),
            false,
        )
    }
    fn archive_session(&self, args: &Value) -> Value {
        let Some(id) = args["session_id"].as_str() else {
            return result(json!("Session required"), true);
        };
        let mut records = self.sessions.lock().unwrap();
        let Some(record) = records.get_mut(id) else {
            return result(json!("Session not found"), true);
        };
        if !matches!(
            record["status"].as_str(),
            Some("completed" | "failed" | "cancelled")
        ) || record["archived"] == true
        {
            return result(json!("Session cannot be archived"), true);
        }
        record["archived"] = json!(true);
        self.workers.lock().unwrap().remove(id);
        self.cancel_writers.lock().unwrap().remove(id);
        result(json!({"session_id":id,"archived":true}), false)
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
                json!({"tools":[{
                    "name":"delegate",
                    "description":"Delegate work to Codex or OpenCode",
                    "inputSchema":{
                        "type":"object",
                        "properties":{
                            "workspace":{"type":"string"},
                            "prompt":{"type":"string"},
                            "agent":{"type":"string","enum":["codex","opencode"],"default":"codex"}
                        },
                        "required":["workspace","prompt"]
                    }
                },{"name":"follow_up","description":"Continue an existing session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"prompt":{"type":"string"}},"required":["session_id","prompt"]}},{"name":"list_sessions","description":"List sessions","inputSchema":{"type":"object","properties":{}}},{"name":"inspect_session","description":"Inspect a session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}, {"name":"cancel_turn","description":"Request cancellation of a specific turn","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"}},"required":["session_id","turn_id"]}}, {"name":"archive_session","description":"Archive a session with a confirmed outcome","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}]})
            }
            "tools/call" => {
                let args = &request["params"]["arguments"];
                match request["params"]["name"].as_str().unwrap_or("") {
                    "list_sessions" => result(
                        json!(sessions
                            .lock()
                            .unwrap()
                            .values()
                            .filter(|s| s["archived"] != true)
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
                    "cancel_turn" => self.cancel_turn(args),
                    "archive_session" => self.archive_session(args),
                    "follow_up" => {
                        let (response, job) = self.follow_up(args);
                        launch = job;
                        response
                    }
                    "delegate" => {
                        let (response, job) = self.delegate(args);
                        launch = job;
                        response
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
        (
            Some(json!({"jsonrpc":"2.0","id":id,"result":response})),
            launch,
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
