use crate::{
    acp::{known_stop_reason, Connection},
    send,
    state_files::{StateFile, StateFiles},
    Adapter, AgentKind, ChildRegistry,
};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;
use std::{
    collections::BTreeSet,
    env,
    io::{self, BufRead, BufReader, Read},
    net::{Ipv4Addr, Shutdown, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{mpsc, Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_FRAME: u64 = 2 * 1024 * 1024;

#[derive(Deserialize, Serialize)]
struct Endpoint {
    port: u16,
    key: [Uuid; 2],
    pid: u32,
}

fn unavailable() -> io::Error {
    io::Error::other("OpenCode owner is unavailable")
}

fn state_directory() -> io::Result<PathBuf> {
    env::var_os("SESSION_SCHEDULER_STATE_DIR")
        .map(Into::into)
        .ok_or_else(unavailable)
}

fn message(reader: &mut impl BufRead) -> io::Result<Value> {
    let mut bytes = Vec::new();
    if reader.take(MAX_FRAME).read_until(b'\n', &mut bytes)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if bytes.last() != Some(&b'\n') {
        return Err(io::Error::other("invalid OpenCode owner frame"));
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

fn authenticator(endpoint: &Endpoint, role: &[u8], client: Uuid, server: Uuid) -> Hmac<Sha256> {
    let mut key = [0; 32];
    key[..16].copy_from_slice(endpoint.key[0].as_bytes());
    key[16..].copy_from_slice(endpoint.key[1].as_bytes());
    let mut mac = Hmac::new_from_slice(&key).unwrap();
    mac.update(b"session-scheduler/opencode-owner/v1");
    mac.update(role);
    mac.update(client.as_bytes());
    mac.update(server.as_bytes());
    mac
}

fn authenticate_client(
    endpoint: &Endpoint,
    reader: &mut BufReader<TcpStream>,
    output: &mut TcpStream,
) -> io::Result<()> {
    let client = Uuid::new_v4();
    send(output, json!(client))?;
    let reply = message(reader)?;
    let server: Uuid = serde_json::from_value(reply["nonce"].clone()).map_err(io::Error::other)?;
    let proof: Vec<u8> =
        serde_json::from_value(reply["proof"].clone()).map_err(io::Error::other)?;
    authenticator(endpoint, b"owner", client, server)
        .verify_slice(&proof)
        .map_err(|_| unavailable())?;
    let proof = authenticator(endpoint, b"scheduler", client, server)
        .finalize()
        .into_bytes();
    send(output, json!(proof.as_slice()))
}

fn authenticate_owner(
    endpoint: &Endpoint,
    reader: &mut BufReader<TcpStream>,
    output: &mut TcpStream,
) -> io::Result<()> {
    let client: Uuid = serde_json::from_value(message(reader)?).map_err(io::Error::other)?;
    let server = Uuid::new_v4();
    let proof = authenticator(endpoint, b"owner", client, server)
        .finalize()
        .into_bytes();
    send(output, json!({"nonce":server,"proof":proof.as_slice()}))?;
    let proof: Vec<u8> = serde_json::from_value(message(reader)?).map_err(io::Error::other)?;
    authenticator(endpoint, b"scheduler", client, server)
        .verify_slice(&proof)
        .map_err(|_| unavailable())
}

fn open(directory: PathBuf, id: Uuid) -> io::Result<Option<(BufReader<TcpStream>, TcpStream)>> {
    let state = StateFiles::open(directory)?;
    let Some(file) = state.read(StateFile::OpenCodeOwner(id))? else {
        return Ok(None);
    };
    let endpoint: Endpoint = serde_json::from_reader(file).map_err(io::Error::other)?;
    let mut socket =
        TcpStream::connect_timeout(&(Ipv4Addr::LOCALHOST, endpoint.port).into(), IO_TIMEOUT)?;
    socket.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(socket.try_clone()?);
    reader.get_ref().set_read_timeout(Some(IO_TIMEOUT))?;
    authenticate_client(&endpoint, &mut reader, &mut socket)
        .map_err(|error| io::Error::other(format!("authenticate owner: {error}")))?;
    Ok(Some((reader, socket)))
}

fn bridge(
    children: &Weak<Mutex<ChildRegistry>>,
    id: &str,
    slot: &str,
    workspace: &Path,
) -> io::Result<Connection> {
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--opencode-owner-client")
        .arg(state_directory()?)
        .arg(id);
    Adapter::connect_command(command, children, slot, workspace)
}

pub(super) fn start(
    adapter: &Adapter,
    children: &Weak<Mutex<ChildRegistry>>,
    id: &str,
    workspace: &Path,
) -> io::Result<Connection> {
    let directory = state_directory()?;
    let id_value = Uuid::parse_str(id).map_err(io::Error::other)?;
    let files = StateFiles::open(directory.clone())?;
    if files.read(StateFile::OpenCodeOwner(id_value))?.is_some() {
        return Err(unavailable());
    }
    let mut command = Command::new(env::current_exe()?);
    command
        .arg("--opencode-owner")
        .arg(directory)
        .arg(id)
        .arg(workspace)
        .arg(&adapter.executable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    crate::acp::remove_scheduler_credentials(&mut command);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    let mut child = command.spawn()?;
    let expires = Instant::now() + START_TIMEOUT;
    loop {
        if files.read(StateFile::OpenCodeOwner(id_value))?.is_some() {
            return bridge(children, id, id, workspace);
        }
        if let Some(status) = child.try_wait()? {
            if status.code() == Some(2) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "unsupported OpenCode owner protocol",
                ));
            }
            return Err(unavailable());
        }
        if Instant::now() >= expires {
            let _ = child.kill();
            let _ = child.wait();
            return Err(unavailable());
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub(super) fn observe(
    children: &Weak<Mutex<ChildRegistry>>,
    id: &str,
    workspace: &Path,
    native_id: &str,
    turn: Option<&str>,
) -> io::Result<(Connection, Value)> {
    let slot = Uuid::new_v4().to_string();
    let mut connection = bridge(children, id, &slot, workspace)?;
    // An observation is read-only and carries the registered native ID and workspace.
    // No initialization, session load or new provider process is involved.
    connection.set_observation_deadline();
    let observation = connection.setup(
        2,
        "owner/observe",
        json!({
            "sessionId":native_id,"cwd":workspace,"turnId":turn,
        }),
    )?;
    connection.clear_observation_deadline();
    Ok((connection, observation))
}

pub(super) fn archive(id: &str) -> io::Result<()> {
    let id = Uuid::parse_str(id).map_err(io::Error::other)?;
    let directory = state_directory()?;
    let response = (|| {
        let Some((mut reader, mut socket)) = open(directory.clone(), id)? else {
            return Ok(());
        };
        send(
            &mut socket,
            json!({"jsonrpc":"2.0","id":1,"method":"owner/archive"}),
        )?;
        let reply = message(&mut reader)?;
        if reply["jsonrpc"] == "2.0" && reply["id"] == 1 && reply["result"]["archived"] == true {
            Ok(())
        } else {
            Err(unavailable())
        }
    })();
    // Cleanup can race a retry or finish after the client times out.
    // The endpoint is removed only after the provider has exited.
    if response.is_err()
        && StateFiles::open(directory)?
            .read(StateFile::OpenCodeOwner(id))?
            .is_none()
    {
        return Ok(());
    }
    response
}

fn relay(directory: PathBuf, id: Uuid) -> io::Result<()> {
    let (mut reader, mut socket) = open(directory, id)?.ok_or_else(unavailable)?;
    // On Windows the cloned read handle retains its own handshake timeout.
    reader.get_ref().set_read_timeout(None)?;
    // Exit on either EOF. The owner independently drains the original ACP request.
    thread::spawn(move || {
        if let Err(error) = io::copy(&mut reader, &mut io::stdout().lock()) {
            eprintln!("OpenCode relay output: {error}");
        }
        let _ = reader.get_ref().shutdown(Shutdown::Both);
        std::process::exit(0);
    });
    io::copy(&mut io::stdin().lock(), &mut socket)?;
    socket.shutdown(Shutdown::Both)
}

struct OwnerState {
    busy: Option<String>,
    latest: Option<Completion>,
    admitted: BTreeSet<String>,
    closing: bool,
    archive_reply: Option<TcpStream>,
}

struct Completion {
    turn: String,
    reason: String,
    ordinary: bool,
}

struct Prompt {
    request: Value,
    id: String,
    ordinary: bool,
    reply: TcpStream,
}

struct Owner {
    native_id: String,
    workspace: PathBuf,
    state: Mutex<OwnerState>,
    prompts: mpsc::Sender<Prompt>,
    cancel: Arc<Mutex<std::process::ChildStdin>>,
    provider: Arc<Mutex<std::process::Child>>,
}

fn answer(output: &mut TcpStream, id: &Value, result: Option<Value>) -> io::Result<()> {
    let reply = match result {
        Some(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        None => {
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"OpenCode owner evidence unavailable"}})
        }
    };
    send(output, reply)
}

fn client(mut output: TcpStream, owner: Arc<Owner>, endpoint: Arc<Endpoint>) -> io::Result<()> {
    output.set_nonblocking(false)?;
    output.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut input = BufReader::new(output.try_clone()?);
    input.get_ref().set_read_timeout(Some(IO_TIMEOUT))?;
    authenticate_owner(&endpoint, &mut input, &mut output)
        .map_err(|error| io::Error::other(format!("authenticate client: {error}")))?;
    input.get_ref().set_read_timeout(None)?;
    let mut attached_at: Option<Option<String>> = None;
    let mut sent_turn = None;
    while let Ok(request) = message(&mut input) {
        let id = &request["id"];
        let params = &request["params"];
        match request["method"].as_str() {
            Some("owner/archive") if id == 1 => {
                let mut state = owner.state.lock().unwrap();
                if state.busy.is_some() || state.archive_reply.is_some() {
                    drop(state);
                    return answer(&mut output, id, None);
                }
                state.closing = true;
                state.archive_reply = Some(output);
                return Ok(());
            }
            Some("initialize") => answer(&mut output, id, Some(json!({"protocolVersion":1})))?,
            Some("session/new") if params["cwd"] == json!(owner.workspace) => {
                let state = owner.state.lock().unwrap();
                if !state.admitted.is_empty() || state.closing {
                    answer(&mut output, id, None)?;
                    continue;
                }
                attached_at = Some(None);
                answer(&mut output, id, Some(json!({"sessionId":owner.native_id})))?;
            }
            Some("owner/observe")
                if params["sessionId"] == owner.native_id
                    && params["cwd"] == json!(owner.workspace) =>
            {
                let result = {
                    let state = owner.state.lock().unwrap();
                    let live = matches!(owner.provider.lock().unwrap().try_wait(), Ok(None));
                    state.latest.as_ref()
                        .filter(|completion| completion.ordinary && live && state.busy.is_none() && !state.closing)
                        .map(|completion| {
                            attached_at = Some(Some(completion.turn.clone()));
                            let mut evidence = json!({"_meta":{"session-scheduler/sessionState":{
                                "sessionId":owner.native_id,"cwd":owner.workspace,"status":"idle"
                            }}});
                            if params["turnId"] == completion.turn {
                                evidence["_meta"]["session-scheduler/turnOutcome"] = json!({
                                    "sessionId":owner.native_id,"promptRequestId":completion.turn,"stopReason":completion.reason,
                                });
                            }
                            evidence
                        })
                };
                answer(&mut output, id, result)?;
            }
            Some("session/prompt")
                if attached_at.is_some() && params["sessionId"] == owner.native_id =>
            {
                let (Some(turn), Some(text)) = (id.as_str(), params["prompt"][0]["text"].as_str())
                else {
                    answer(&mut output, id, None)?;
                    continue;
                };
                let reply = output.try_clone()?;
                let mut state = owner.state.lock().unwrap();
                let current = state.latest.as_ref().map(|completion| &completion.turn);
                let previous = attached_at.as_ref().and_then(|previous| previous.as_ref());
                if state.closing
                    || state.busy.is_some()
                    || previous != current
                    || (current.is_none() && !state.admitted.is_empty())
                    || !state.admitted.insert(turn.to_owned())
                {
                    drop(state);
                    answer(&mut output, id, None)?;
                    continue;
                }
                let prompt = Prompt {
                    request: json!({"jsonrpc":"2.0","id":turn,"method":"session/prompt","params":{
                        "sessionId":owner.native_id,"prompt":[{"type":"text","text":text}]
                    }}),
                    id: turn.to_owned(),
                    ordinary: !text
                        .trim_start_matches(|ch: char| ch.is_whitespace() || ch == '\u{feff}')
                        .starts_with('/'),
                    reply,
                };
                attached_at = Some(Some(turn.to_owned()));
                sent_turn = Some(turn.to_owned());
                state.busy = Some(turn.to_owned());
                state.latest = None;
                owner.prompts.send(prompt).map_err(|_| unavailable())?;
            }
            Some("session/cancel")
                if attached_at.is_some() && params["sessionId"] == owner.native_id =>
            {
                let mut cancel = owner.cancel.lock().unwrap();
                let state = owner.state.lock().unwrap();
                if state.busy.is_some() && state.busy == sent_turn {
                    send(
                        &mut *cancel,
                        json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":owner.native_id}}),
                    )?;
                }
            }
            _ if !id.is_null() => answer(&mut output, id, None)?,
            _ => {}
        }
    }
    Ok(())
}

fn process_prompts(mut connection: Connection, inbox: mpsc::Receiver<Prompt>, owner: Arc<Owner>) {
    while let Ok(mut prompt) = inbox.recv() {
        let id = json!(prompt.id);
        let mut client_connected = true;
        let result = connection.send(prompt.request).and_then(|()| {
            connection.response(&id, |params| {
                if client_connected && params["sessionId"] == owner.native_id {
                    client_connected = send(
                        &mut prompt.reply,
                        json!({"jsonrpc":"2.0","method":"session/update","params":params}),
                    )
                    .is_ok();
                }
            })
        });
        let writer = owner.cancel.lock().unwrap();
        let mut state = owner.state.lock().unwrap();
        // An ordinary OpenCode ACP prompt returns after its own idle event. Slash
        // commands retain live ACP behavior but cannot supply evidence after reattachment.
        state.latest = result
            .as_ref()
            .ok()
            .and_then(|reply| reply.as_ref())
            .and_then(|reply| {
                reply["stopReason"]
                    .as_str()
                    .filter(|reason| known_stop_reason(reason))
                    .map(|reason| Completion {
                        turn: prompt.id,
                        reason: reason.to_owned(),
                        ordinary: prompt.ordinary,
                    })
            });
        state.busy = None;
        drop(state);
        drop(writer);
        match result {
            Ok(reply) if client_connected => {
                let _ = answer(&mut prompt.reply, &id, reply);
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

fn serve(directory: PathBuf, id: Uuid, workspace: PathBuf, executable: PathBuf) -> io::Result<()> {
    let mut files = StateFiles::open(directory)?;
    let database = files.opencode_database_path(id)?;
    let registry = Arc::new(Mutex::new(ChildRegistry::default()));
    let mut command = (Adapter {
        kind: AgentKind::OpenCode,
        executable,
    })
    .provider_command();
    let password = format!("{}{}", Uuid::new_v4(), Uuid::new_v4());
    command
        .args(["--hostname", "127.0.0.1", "--port", "0", "--no-mdns"])
        .env("OPENCODE_DB", database)
        .env("OPENCODE_SERVER_PASSWORD", password)
        .stderr(Stdio::null());
    let mut connection = Adapter::connect_command(
        command,
        &Arc::downgrade(&registry),
        &id.to_string(),
        &workspace,
    )?;
    let initialized = connection.initialize()?;
    let version = &initialized["agentInfo"];
    if version["name"] != "OpenCode" || version["version"] != "1.19.6" {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unsupported OpenCode owner protocol",
        ));
    }
    let native = connection.setup(2, "session/new", json!({"cwd":workspace,"mcpServers":[]}))?;
    let native_id = native["sessionId"]
        .as_str()
        .filter(|id| !id.is_empty())
        .ok_or_else(unavailable)?
        .to_owned();
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let endpoint = Arc::new(Endpoint {
        port: listener.local_addr()?.port(),
        key: [Uuid::new_v4(), Uuid::new_v4()],
        pid: std::process::id(),
    });
    let (prompts, inbox) = mpsc::channel();
    let provider = connection.child_handle();
    let owner = Arc::new(Owner {
        native_id,
        workspace,
        prompts,
        cancel: connection.input_handle(),
        provider: provider.clone(),
        state: Mutex::new(OwnerState {
            busy: None,
            latest: None,
            admitted: BTreeSet::new(),
            closing: false,
            archive_reply: None,
        }),
    });
    let worker = owner.clone();
    let engine = thread::spawn(move || process_prompts(connection, inbox, worker));
    files.replace(StateFile::OpenCodeOwner(id), |file| {
        serde_json::to_writer(file, &*endpoint).map_err(io::Error::other)
    })?;
    let expires = Instant::now() + START_TIMEOUT;
    loop {
        let closing = {
            let mut state = owner.state.lock().unwrap();
            state.closing |= (state.admitted.is_empty() && Instant::now() >= expires)
                || engine.is_finished()
                || !matches!(provider.lock().unwrap().try_wait(), Ok(None));
            state.closing
        };
        if closing {
            let closed = stop_provider(&provider).and_then(|()| files.remove_owner(id));
            let reply = owner.state.lock().unwrap().archive_reply.take();
            if let Some(mut reply) = reply {
                let result = closed.as_ref().ok().map(|()| json!({"archived":true}));
                let _ = answer(&mut reply, &json!(1), result);
            }
            if closed.is_ok() {
                return Ok(());
            }
            // Keep the authenticated endpoint available for retry if cleanup failed.
            thread::sleep(Duration::from_millis(250));
        }
        match listener.accept() {
            Ok((socket, _)) => {
                let owner = owner.clone();
                let endpoint = endpoint.clone();
                thread::spawn(move || {
                    if let Err(error) = client(socket, owner, endpoint) {
                        eprintln!("OpenCode owner client: {error}");
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(error) => {
                stop_provider(&provider)?;
                files.remove_owner(id)?;
                return Err(error);
            }
        }
    }
}

fn stop_provider(provider: &Mutex<std::process::Child>) -> io::Result<()> {
    let mut provider = provider.lock().unwrap();
    if provider.try_wait()?.is_none() {
        // A provider can exit between try_wait and kill.
        if let Err(error) = provider.kill() {
            if provider.try_wait()?.is_none() {
                return Err(error);
            }
        }
        provider.wait()?;
    }
    Ok(())
}

pub(super) fn command() -> Option<io::Result<()>> {
    let mut args = env::args_os().skip(1);
    let mode = args.next()?;
    if mode != "--opencode-owner" && mode != "--opencode-owner-client" {
        return None;
    }
    Some((|| {
        let directory = args.next().map(PathBuf::from).ok_or_else(unavailable)?;
        let id = args
            .next()
            .and_then(|id| Uuid::try_parse(&id.to_string_lossy()).ok())
            .ok_or_else(unavailable)?;
        if mode == "--opencode-owner-client" {
            return relay(directory, id);
        }
        let workspace = args.next().map(PathBuf::from).ok_or_else(unavailable)?;
        let executable = args.next().map(PathBuf::from).ok_or_else(unavailable)?;
        serve(directory, id, workspace, executable)
    })())
}
