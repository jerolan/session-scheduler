use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::{self, OpenOptions},
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use url::Url;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
const NAME: &str = "turn.completed";
const MAX_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const ROTATION_SECONDS: i64 = 300;

#[derive(Clone, Serialize, Deserialize)]
struct Subscription {
    id: String,
    account: String,
    url: String,
    secret: String,
    expires: DateTime<Utc>,
    old_secret: Option<String>,
    old_secret_until: Option<DateTime<Utc>>,
}

pub struct Events {
    account: String,
    state: PathBuf,
    callback: Option<String>,
    ip: Option<IpAddr>,
    loopback: bool,
    subscriptions: Mutex<Vec<Subscription>>,
}

fn invalid() -> Value {
    json!({"code":-32602,"message":"Invalid params"})
}
fn callback_error(reason: &str) -> Value {
    json!({"code":-32015,"message":"CallbackEndpointError","data":{"reason":reason}})
}
fn secret_key(secret: &str) -> Option<Vec<u8>> {
    let key = STANDARD.decode(secret.strip_prefix("whsec_")?).ok()?;
    (24..=64).contains(&key.len()).then_some(key)
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let o = ip.octets();
            !(o[0] == 0
                || o[0] == 10
                || o[0] == 127
                || o[0] >= 224
                || (o[0] == 100 && (64..=127).contains(&o[1]))
                || (o[0] == 169 && o[1] == 254)
                || (o[0] == 172 && (16..=31).contains(&o[1]))
                || (o[0] == 192 && (o[1] == 168 || o[1] == 0 || (o[1] == 88 && o[2] == 99)))
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19 || (o[1] == 51 && o[2] == 100)))
                || (o[0] == 203 && o[1] == 0 && o[2] == 113))
        }
        IpAddr::V6(ip) => {
            if let Some(v4) = ip.to_ipv4_mapped() {
                return public_ip(IpAddr::V4(v4));
            }
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}
fn constant_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}
impl Events {
    pub fn from_env() -> io::Result<Self> {
        let account = env::var("SESSION_SCHEDULER_ACCOUNT").map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "SESSION_SCHEDULER_ACCOUNT required",
            )
        })?;
        if account.is_empty() || account.len() > 256 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid account",
            ));
        }
        let dir = PathBuf::from(env::var("SESSION_SCHEDULER_STATE_DIR").map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "SESSION_SCHEDULER_STATE_DIR required",
            )
        })?);
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&dir)?;
        let metadata = fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state directory must be private",
            ));
        }
        let state = dir.join("event-subscriptions.json");
        let subscriptions: Vec<Subscription> = match fs::symlink_metadata(&state) {
            Ok(meta) if meta.file_type().is_file() && meta.permissions().mode() & 0o077 == 0 => {
                serde_json::from_slice(&fs::read(&state)?).map_err(io::Error::other)?
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "state file must be private",
                ))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        if subscriptions.iter().any(|s| s.account != account) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state belongs to another account",
            ));
        }
        let callback = env::var("SESSION_SCHEDULER_CALLBACK_URL").ok();
        let ip = env::var("SESSION_SCHEDULER_CALLBACK_IP")
            .ok()
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid callback IP"))?;
        let loopback = env::var("SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK").as_deref() == Ok("1");
        let service = Self {
            account,
            state,
            callback,
            ip,
            loopback,
            subscriptions: Mutex::new(subscriptions),
        };
        if let Some(url) = &service.callback {
            service.target(url).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid callback configuration",
                )
            })?;
        }
        Ok(service)
    }
    fn target(&self, raw: &str) -> Result<(Url, IpAddr), ()> {
        if self.callback.as_deref() != Some(raw) {
            return Err(());
        }
        let url = Url::parse(raw).map_err(|_| ())?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.host_str().is_none()
        {
            return Err(());
        }
        if self.loopback
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some()
        {
            return Ok((url, IpAddr::V4(Ipv4Addr::LOCALHOST)));
        }
        let ip = self.ip.ok_or(())?;
        if url.scheme() != "https"
            || !matches!(url.host(), Some(url::Host::Domain(_)))
            || !public_ip(ip)
        {
            return Err(());
        }
        Ok((url, ip))
    }
    fn persist(&self, subs: &[Subscription]) -> io::Result<()> {
        let tmp = self
            .state
            .with_file_name(format!(".events-{}", Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            serde_json::to_writer(&mut file, subs).map_err(io::Error::other)?;
            file.flush()?;
            file.sync_all()?;
            fs::rename(&tmp, &self.state)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(tmp);
        }
        result
    }
    fn id(&self, url: &str) -> String {
        let mut h = Sha256::new();
        for part in [&self.account[..], url, NAME, "{}"] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        format!("sub_{:x}", h.finalize())
    }
    fn send(&self, sub: &Subscription, id: &str, body: &Value) -> Result<Value, &'static str> {
        let (url, ip) = self.target(&sub.url).map_err(|_| "invalid_url")?;
        let host = url.host_str().ok_or("invalid_url")?;
        let port = url.port_or_known_default().ok_or("invalid_url")?;
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .resolve(host, SocketAddr::new(ip, port))
            .build()
            .map_err(|_| "invalid_url")?;
        let body = serde_json::to_vec(body).map_err(|_| "challenge_failed")?;
        if body.len() > 262144 {
            return Err("challenge_failed");
        }
        let ts = Utc::now().timestamp().to_string();
        let sign = |secret: &str| -> Option<String> {
            let key = secret_key(secret)?;
            let mut mac = HmacSha256::new_from_slice(&key).ok()?;
            mac.update(format!("{id}.{ts}.").as_bytes());
            mac.update(&body);
            Some(format!(
                "v1,{}",
                STANDARD.encode(mac.finalize().into_bytes())
            ))
        };
        let mut signature = sign(&sub.secret).ok_or("challenge_failed")?;
        if sub.old_secret_until.is_some_and(|until| until > Utc::now()) {
            if let Some(old) = sub.old_secret.as_deref().and_then(sign) {
                signature.push(' ');
                signature.push_str(&old);
            }
        }
        let mut response = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", ts)
            .header("webhook-signature", signature)
            .header("X-MCP-Subscription-Id", &sub.id)
            .body(body)
            .send()
            .map_err(|e| {
                if e.is_timeout() {
                    "timeout"
                } else {
                    "transient"
                }
            })?;
        if !response.status().is_success() {
            return Err(if matches!(response.status().as_u16(), 410 | 413) {
                "permanent"
            } else if response.status().is_server_error() || response.status().as_u16() == 429 {
                "transient"
            } else {
                "challenge_failed"
            });
        }
        let mut bytes = Vec::new();
        response
            .by_ref()
            .take(8193)
            .read_to_end(&mut bytes)
            .map_err(|_| "challenge_failed")?;
        if bytes.len() > 8192 {
            return Err("challenge_failed");
        }
        Ok(serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    pub fn dispatch(&self, method: &str, params: &Value) -> Option<Result<Value, Value>> {
        match method {
            "events/list" => Some(Ok(
                json!({"events":[{"name":NAME,"description":"A delegated turn reached a confirmed terminal outcome.","delivery":["webhook"],"inputSchema":{"type":"object","properties":{},"additionalProperties":false},"payloadSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"},"reason":{"type":"string"}},"required":["session_id","turn_id","reason"],"additionalProperties":false}}]}),
            )),
            "events/subscribe" | "events/unsubscribe" => Some(self.change(method, params)),
            _ => None,
        }
    }
    fn change(&self, method: &str, p: &Value) -> Result<Value, Value> {
        if p.get("name").and_then(Value::as_str) != Some(NAME)
            || p.get("arguments") != Some(&json!({}))
            || p.pointer("/delivery/mode").and_then(Value::as_str) != Some("webhook")
        {
            return Err(invalid());
        }
        let url = p
            .pointer("/delivery/url")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        self.target(url)
            .map_err(|_| callback_error("invalid_url"))?;
        let id = self.id(url);
        if method == "events/unsubscribe" {
            let mut lock = self.subscriptions.lock().map_err(|_| invalid())?;
            let mut next = lock.clone();
            next.retain(|s| s.id != id);
            self.persist(&next)
                .map_err(|_| json!({"code":-32603,"message":"Internal error"}))?;
            *lock = next;
            return Ok(json!({}));
        }
        let secret = p
            .pointer("/delivery/secret")
            .and_then(Value::as_str)
            .filter(|s| secret_key(s).is_some())
            .ok_or_else(invalid)?;
        if p.get("cursor").is_some_and(|v| !v.is_null()) {
            return Err(invalid());
        }
        let ttl = match p.get("ttlMs") {
            None | Some(Value::Null) => MAX_TTL_MS,
            Some(v) => v
                .as_i64()
                .filter(|x| *x > 0)
                .ok_or_else(invalid)?
                .min(MAX_TTL_MS),
        };
        let expires = Utc::now() + ChronoDuration::milliseconds(ttl);
        let mut sub = Subscription {
            id: id.clone(),
            account: self.account.clone(),
            url: url.to_owned(),
            secret: secret.to_owned(),
            expires,
            old_secret: None,
            old_secret_until: None,
        };
        let challenge = Uuid::new_v4().to_string();
        let response = self
            .send(
                &sub,
                &format!("msg_{}", Uuid::new_v4()),
                &json!({"type":"verification","challenge":challenge}),
            )
            .map_err(|reason| {
                callback_error(if reason == "transient" || reason == "permanent" {
                    "challenge_failed"
                } else {
                    reason
                })
            })?;
        if !response
            .get("challenge")
            .and_then(Value::as_str)
            .is_some_and(|s| constant_eq(s, &challenge))
        {
            return Err(callback_error("challenge_failed"));
        }
        let mut lock = self.subscriptions.lock().map_err(|_| invalid())?;
        let mut next = lock.clone();
        if let Some(old) = next.iter().find(|s| s.id == id && s.expires > Utc::now()) {
            if old.secret != sub.secret {
                sub.old_secret = Some(old.secret.clone());
                sub.old_secret_until = Some(Utc::now() + ChronoDuration::seconds(ROTATION_SECONDS));
            } else {
                sub.old_secret = old.old_secret.clone();
                sub.old_secret_until = old.old_secret_until;
            }
        }
        next.retain(|s| s.id != id && s.expires > Utc::now());
        next.push(sub);
        self.persist(&next)
            .map_err(|_| json!({"code":-32603,"message":"Internal error"}))?;
        *lock = next;
        Ok(
            json!({"id":id,"refreshBefore":expires.to_rfc3339_opts(chrono::SecondsFormat::Secs,true),"cursor":null,"truncated":false}),
        )
    }
    pub fn complete(&self, session_id: &str, turn_id: &str, reason: &str) {
        if session_id.is_empty()
            || turn_id.is_empty()
            || session_id.len() > 128
            || turn_id.len() > 128
            || !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || !turn_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            || !matches!(
                reason,
                "end_turn"
                    | "refusal"
                    | "max_tokens"
                    | "max_turn_requests"
                    | "failed"
                    | "cancelled"
            )
        {
            return;
        }
        let eligible = match self.subscriptions.lock() {
            Ok(lock) => lock
                .iter()
                .filter(|s| s.expires > Utc::now() && s.account == self.account)
                .cloned()
                .collect::<Vec<_>>(),
            Err(_) => return,
        };
        for sub in &eligible {
            let id = format!("evt_{}", Uuid::new_v4());
            let event = json!({"eventId":id,"name":NAME,"timestamp":Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs,true),"data":{"session_id":session_id,"turn_id":turn_id,"reason":reason},"cursor":null});
            for attempt in 0..3 {
                match self.send(sub, &id, &event) {
                    Ok(_) | Err("permanent") | Err("challenge_failed") | Err("invalid_url") => {
                        break
                    }
                    _ if attempt < 2 => {
                        std::thread::sleep(Duration::from_millis(200 * (1 << attempt)))
                    }
                    _ => break,
                }
            }
        }
    }
}
