use crate::{events::Events, launch, State};
use serde_json::{json, Value};
use std::{
    env,
    io::{self, Read},
    sync::Arc,
    thread,
};
use tiny_http::{Header, Method, Response, Server, StatusCode};

fn equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |x, (a, b)| x | (a ^ b)) == 0
}
fn response(status: u16, body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut r = Response::from_string(body).with_status_code(StatusCode(status));
    r.add_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    r
}
pub fn serve() -> io::Result<()> {
    let port: u16 = env::var("SESSION_SCHEDULER_HTTP_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|p| *p != 0)
        .ok_or_else(|| io::Error::other("invalid port"))?;
    let token = env::var("SESSION_SCHEDULER_BEARER_TOKEN")
        .ok()
        .filter(|s| !s.is_empty() && !s.contains(['\r', '\n']))
        .ok_or_else(|| io::Error::other("missing bearer token"))?;
    let events = Arc::new(Events::from_env()?);
    let state = Arc::new(State::new(Some(events.clone()))?);
    let server = Server::http(("127.0.0.1", port)).map_err(io::Error::other)?;
    let worker_events = events.clone();
    thread::spawn(move || loop {
        if let Err(error) = worker_events.cleanup() {
            eprintln!("event cleanup failed: {error}");
        }
        if let Err(error) = worker_events.deliver_pending() {
            eprintln!("pending event delivery failed: {error}");
        }
        thread::sleep(std::time::Duration::from_secs(1));
    });
    for mut request in server.incoming_requests() {
        let state = state.clone();
        let token = token.clone();
        thread::spawn(move || {
            let header = |name: &str| {
                request
                    .headers()
                    .iter()
                    .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
                    .map(|h| h.value.as_str().to_owned())
            };
            let authorized = header("Authorization")
                .is_some_and(|v| equal(v.as_bytes(), format!("Bearer {token}").as_bytes()));
            if !authorized {
                let _ = request.respond(response(401, String::new()));
                return;
            }
            if header("Origin").is_some() {
                let _ = request.respond(response(403, String::new()));
                return;
            }
            if request.url() != "/mcp" {
                let _ = request.respond(response(404, String::new()));
                return;
            }
            if request.method() != &Method::Post {
                let _ = request.respond(response(405, String::new()));
                return;
            }
            if !header("Content-Type").is_some_and(|v| {
                v.split(';')
                    .next()
                    .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
            }) {
                let _ = request.respond(response(415, String::new()));
                return;
            }
            if header("MCP-Protocol-Version")
                .is_some_and(|v| !matches!(v.as_str(), "2025-03-26" | "2025-11-25"))
            {
                let _ = request.respond(response(400, String::new()));
                return;
            }
            let mut body = Vec::new();
            if request
                .as_reader()
                .take(1_048_577)
                .read_to_end(&mut body)
                .is_err()
                || body.len() > 1_048_576
            {
                let _ = request.respond(response(413, String::new()));
                return;
            }
            let parsed = serde_json::from_slice::<Value>(&body);
            let result = match parsed {
                Ok(value)
                    if value.is_object()
                        && value["jsonrpc"] == "2.0"
                        && value["method"].is_string() =>
                {
                    state.dispatch(value)
                }
                _ => (
                    Some(
                        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid request"}}),
                    ),
                    None,
                ),
            };
            let (result, job) = result;
            let (status, body) = match result {
                Some(v) => (200, v.to_string()),
                None => (202, String::new()),
            };
            if request.respond(response(status, body)).is_ok() {
                if let Some(job) = job {
                    launch(job);
                }
            }
        });
    }
    Ok(())
}
