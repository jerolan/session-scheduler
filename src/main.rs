mod acp;
mod events;
mod http;
mod state_files;
use acp::{AcpJob, Start};
use events::TurnEvent;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    env,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::{Child, ChildStdin},
    sync::{mpsc, Arc, Mutex},
    thread,
};
use uuid::Uuid;

#[derive(Default)]
struct ChildRegistry {
    stopping: bool,
    children: BTreeMap<String, Vec<Arc<Mutex<Child>>>>,
}
type Children = Arc<Mutex<ChildRegistry>>;
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
type Workers = Arc<Mutex<BTreeMap<String, mpsc::Sender<Work>>>>;
struct Turn {
    id: String,
    prompt: String,
}

enum Work {
    Prompt(Turn),
    Reconnect { turn_id: String },
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
        if let Err(error) =
            events.queue_turn_event(TurnEvent::Completed, id, turn_id, event_reason, publish)
        {
            eprintln!("could not persist completion of turn {turn_id}: {error}");
        }
    } else {
        publish();
    }
}

fn require_recovery(sessions: &Sessions, events: Option<&events::Events>, id: &str, turn_id: &str) {
    let publish = {
        let (sessions, id, turn_id) = (sessions.clone(), id.to_owned(), turn_id.to_owned());
        move || {
            update(&sessions, &id, |record| {
                if record["turn_id"] == turn_id && record["status"] == "recovering" {
                    record["status"] = json!("unknown");
                }
            })
        }
    };
    if let Some(events) = events {
        if let Err(error) = events.queue_turn_event(
            TurnEvent::RecoveryRequired,
            id,
            turn_id,
            "outcome_unknown",
            publish,
        ) {
            eprintln!("could not persist recovery notice for turn {turn_id}: {error}");
        }
    } else {
        publish();
    }
}

fn send(writer: &mut impl Write, value: Value) -> io::Result<()> {
    writeln!(writer, "{value}")?;
    writer.flush()
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
        start: Start,
        receiver: mpsc::Receiver<Work>,
        workers: Workers,
        children: Children,
        adapter: Adapter,
    },
    Send(mpsc::Sender<Work>, Work),
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
                    }
                    if let Some(Action::Start { workers, .. }) = &self.action {
                        workers.lock().unwrap().remove(&self.id);
                        self.cancel_writers.lock().unwrap().remove(&self.id);
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
            start,
            receiver,
            workers,
            children,
            adapter,
        } => {
            let acp = AcpJob {
                sessions: job.sessions.clone(),
                id: job.id.clone(),
                workspace,
                receiver,
                workers: Arc::downgrade(&workers),
                children: Arc::downgrade(&children),
                adapter,
                events: job.events.clone(),
                cancel_writers: job.cancel_writers.clone(),
            };
            thread::spawn(move || acp.run(start));
        }
        Action::Send(sender, work) => {
            if let Err(error) = sender.send(work) {
                if matches!(error.0, Work::Reconnect { .. }) {
                    update(&job.sessions, &job.id, |record| {
                        record["status"] = json!("unknown")
                    });
                    return;
                }
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
        for child in registry.children.values().flatten() {
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
                start: Start::New(Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                }),
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
    fn reconnect_session(&self, args: &Value) -> (Value, Option<Launch>) {
        let Some(id) = args["session_id"].as_str() else {
            return (result(json!("Session required"), true), None);
        };
        let mut sessions = self.sessions.lock().unwrap();
        let Some(record) = sessions.get_mut(id) else {
            return (result(json!("Session not found"), true), None);
        };
        if record["status"] != "unknown" || record["archived"] == true {
            return (
                result(json!("Session does not require recovery"), true),
                None,
            );
        }
        let Some(workspace) = record["workspace"]
            .as_str()
            .and_then(|p| PathBuf::from(p).canonicalize().ok())
            .filter(|p| {
                self.allowed.contains(p)
                    && record["workspace"]
                        .as_str()
                        .is_some_and(|original| p == std::path::Path::new(original))
            })
        else {
            return (result(json!("Workspace is not allowed"), true), None);
        };
        let (kind, executable) = match record["agent"].as_str() {
            Some("codex") => (AgentKind::Codex, &self.codex_command),
            Some("opencode") => (AgentKind::OpenCode, &self.opencode_command),
            _ => return (result(json!("Unknown agent"), true), None),
        };
        let (Some(executable), Some(acp_id), Some(turn_id)) = (
            executable.clone(),
            record["acp_session_id"].as_str(),
            record["turn_id"].as_str(),
        ) else {
            return (result(json!("Session is not available"), true), None);
        };
        let turn_id = turn_id.to_owned();
        let acp_id = acp_id.to_owned();
        let previous = record.clone();
        record["status"] = json!("recovering");
        let mut workers = self.workers.lock().unwrap();
        let action = if let Some(sender) = workers.get(id) {
            Action::Send(
                sender.clone(),
                Work::Reconnect {
                    turn_id: turn_id.clone(),
                },
            )
        } else {
            let (sender, receiver) = mpsc::channel();
            workers.insert(id.to_owned(), sender);
            self.cancel_writers
                .lock()
                .unwrap()
                .insert(id.to_owned(), Arc::new(Mutex::new(None)));
            Action::Start {
                start: Start::Reconnect {
                    acp_id,
                    turn_id: turn_id.clone(),
                },
                workspace,
                receiver,
                workers: self.workers.clone(),
                children: self.children.clone(),
                adapter: Adapter { kind, executable },
            }
        };
        let job = Launch {
            action: Some(action),
            sessions: self.sessions.clone(),
            id: id.to_owned(),
            turn_id: turn_id.clone(),
            previous: Some(previous),
            events: self.events.clone(),
            cancel_writers: self.cancel_writers.clone(),
        };
        (
            result(
                json!({"session_id":id,"turn_id":turn_id,"status":"recovering"}),
                false,
            ),
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
            Some("running" | "cancelling" | "completion_pending" | "recovering")
        ) || record["archived"] == true
        {
            return (result(json!("Session is not available"), true), None);
        }
        if record["status"] == "unknown"
            && !record["workspace"].as_str().is_some_and(|path| {
                PathBuf::from(path).canonicalize().is_ok_and(|workspace| {
                    workspace == std::path::Path::new(path) && workspace.is_dir()
                })
            })
        {
            return (result(json!("Workspace is not allowed"), true), None);
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
            .and_then(|children| children.last())
            .map(|child| matches!(child.lock().unwrap().try_wait(), Ok(None)))
            .unwrap_or(false);
        if !usable {
            return (result(json!("Session is not available"), true), None);
        }
        let previous = record.clone();
        let tid = Uuid::new_v4().to_string();
        if record["status"] == "unknown" {
            let unresolved = record["turn_id"].clone();
            if record["unresolved_turn_ids"].is_null() {
                record["unresolved_turn_ids"] = json!([]);
            }
            record["unresolved_turn_ids"]
                .as_array_mut()
                .unwrap()
                .push(unresolved);
        }
        record["turn_id"] = json!(tid);
        record["status"] = json!("running");
        record["result"] = json!("");
        record["stop_reason"] = Value::Null;
        record["history"]
            .as_array_mut()
            .unwrap()
            .push(json!({"role":"user","text":prompt}));
        let job = Launch {
            action: Some(Action::Send(
                sender,
                Work::Prompt(Turn {
                    id: tid.clone(),
                    prompt: prompt.to_owned(),
                }),
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
                },{"name":"follow_up","description":"Continue an existing session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"prompt":{"type":"string"}},"required":["session_id","prompt"]}},{"name":"list_sessions","description":"List sessions","inputSchema":{"type":"object","properties":{}}},{"name":"inspect_session","description":"Inspect a session","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}, {"name":"cancel_turn","description":"Request cancellation of a specific turn","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"}},"required":["session_id","turn_id"]}}, {"name":"reconnect_session","description":"Recover the existing session and check the original turn","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}, {"name":"archive_session","description":"Archive a session with a confirmed outcome","inputSchema":{"type":"object","properties":{"session_id":{"type":"string"}},"required":["session_id"]}}]})
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
                    "reconnect_session" => {
                        let (response, job) = self.reconnect_session(args);
                        launch = job;
                        response
                    }
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
