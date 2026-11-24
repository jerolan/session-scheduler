use crate::state_files::{StateFile, StateFiles};
use base64::{engine::general_purpose::STANDARD, Engine};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env,
    io::{self, Read},
    net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs},
    path::PathBuf,
    sync::Mutex,
    time::Duration,
};
use url::Url;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;
#[derive(Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub(super) enum TurnEvent {
    #[default]
    Completed,
    RecoveryRequired,
}

impl TurnEvent {
    fn name(self) -> &'static str {
        match self {
            Self::Completed => "turn.completed",
            Self::RecoveryRequired => "turn.recovery_required",
        }
    }

    fn catalog_entry(self, description: &str) -> Value {
        json!({"name":self.name(),"description":description,"delivery":["webhook"],"inputSchema":{"type":"object","properties":{},"additionalProperties":false},"payloadSchema":{"type":"object","properties":{"session_id":{"type":"string"},"turn_id":{"type":"string"},"reason":{"type":"string"}},"required":["session_id","turn_id","reason"],"additionalProperties":false}})
    }
}

const MAX_TTL_MS: i64 = 24 * 60 * 60 * 1000;
const ROTATION_SECONDS: i64 = 300;
const VERIFICATION_SECONDS: i64 = 3600;

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Subscription {
    #[serde(default)]
    event: TurnEvent,
    id: String,
    account: String,
    url: String,
    secret: String,
    expires: DateTime<Utc>,
    old_secret: Option<String>,
    old_secret_until: Option<DateTime<Utc>>,
    #[serde(default)]
    verified_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingEvent {
    #[serde(default)]
    event: TurnEvent,
    subscription_id: String,
    event_id: String,
    timestamp: String,
    session_id: String,
    turn_id: String,
    reason: String,
}

impl PendingEvent {
    fn body(&self) -> Value {
        json!({
            "eventId": self.event_id,
            "name": self.event.name(),
            "timestamp": self.timestamp,
            "data": {
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "reason": self.reason,
            },
            "cursor": null,
        })
    }
}

struct PendingEvents {
    entries: Vec<PendingEvent>,
    publish_after_sync: Vec<Box<dyn FnOnce() + Send>>,
}

pub struct Events {
    account: String,
    state: StateFiles,
    callback: Option<String>,
    callback_host: Option<String>,
    ip: Option<IpAddr>,
    loopback: bool,
    subscriptions: Mutex<Vec<Subscription>>,
    pending: Mutex<PendingEvents>,
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
fn callback_url(raw: &str) -> Result<Url, ()> {
    let url = Url::parse(raw).map_err(|_| ())?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.host_str().is_none()
    {
        return Err(());
    }
    Ok(url)
}

fn matches_callback_host(url: &Url, host: &str) -> bool {
    url.scheme() == "https"
        && url.host_str() == Some(host)
        && url.port_or_known_default() == Some(443)
}

fn public_callback_addresses(
    addresses: impl Iterator<Item = SocketAddr>,
) -> Result<Vec<SocketAddr>, &'static str> {
    let addresses: Vec<_> = addresses.collect();
    if addresses.is_empty() {
        return Err("transient");
    }
    if addresses.iter().any(|addr| !public_ip(addr.ip())) {
        return Err("invalid_url");
    }
    Ok(addresses)
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
        let state = StateFiles::open(dir)?;
        let subscriptions: Vec<Subscription> = match state.read(StateFile::Subscriptions)? {
            Some(file) => serde_json::from_reader(file).map_err(io::Error::other)?,
            None => Vec::new(),
        };
        if subscriptions.iter().any(|s| s.account != account) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "state belongs to another account",
            ));
        }
        let pending = match state.read(StateFile::PendingEvents)? {
            Some(file) => serde_json::from_reader(file).map_err(io::Error::other)?,
            None => Vec::new(),
        };
        let callback = env::var("SESSION_SCHEDULER_CALLBACK_URL").ok();
        let ip = env::var("SESSION_SCHEDULER_CALLBACK_IP")
            .ok()
            .map(|s| s.parse())
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid callback IP"))?;
        let loopback = env::var("SESSION_SCHEDULER_TEST_LOOPBACK_CALLBACK").as_deref() == Ok("1");
        let callback_host = match env::var("SESSION_SCHEDULER_CALLBACK_HOST") {
            Ok(host)
                if matches!(url::Host::parse(&host), Ok(url::Host::Domain(domain)) if domain == host)
                    && callback.is_none()
                    && ip.is_none()
                    && !loopback =>
            {
                Some(host)
            }
            Err(env::VarError::NotPresent) => None,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "callback host must be a domain and cannot be combined with a fixed callback",
                ));
            }
        };
        let service = Self {
            account,
            state,
            callback,
            callback_host,
            ip,
            loopback,
            subscriptions: Mutex::new(subscriptions),
            pending: Mutex::new(PendingEvents {
                entries: pending,
                publish_after_sync: Vec::new(),
            }),
        };
        if let Some(url) = &service.callback {
            service.target(url).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid callback configuration",
                )
            })?;
        }
        service.cleanup()?;
        Ok(service)
    }
    fn target(&self, raw: &str) -> Result<(Url, Option<IpAddr>), ()> {
        let url = callback_url(raw)?;
        if let Some(host) = &self.callback_host {
            return matches_callback_host(&url, host)
                .then_some((url, None))
                .ok_or(());
        }
        if self.callback.as_deref() != Some(raw) {
            return Err(());
        }
        if self.loopback
            && url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.port().is_some()
        {
            return Ok((url, Some(IpAddr::V4(Ipv4Addr::LOCALHOST))));
        }
        let ip = self.ip.ok_or(())?;
        if url.scheme() != "https"
            || !matches!(url.host(), Some(url::Host::Domain(_)))
            || !public_ip(ip)
        {
            return Err(());
        }
        Ok((url, Some(ip)))
    }
    fn persist(&self, subs: &[Subscription]) -> io::Result<()> {
        self.state.replace(StateFile::Subscriptions, |file| {
            serde_json::to_writer(file, subs).map_err(io::Error::other)
        })
    }
    pub fn cleanup(&self) -> io::Result<()> {
        let mut lock = self
            .subscriptions
            .lock()
            .map_err(|_| io::Error::other("subscription lock poisoned"))?;
        let now = Utc::now();
        let mut next = lock.clone();
        next.retain(|s| s.expires > now);
        for sub in &mut next {
            if sub.old_secret_until.is_none_or(|until| until <= now) {
                sub.old_secret = None;
                sub.old_secret_until = None;
            }
        }
        if *lock != next {
            self.persist(&next)?;
            *lock = next;
        }
        Ok(())
    }
    fn id(&self, url: &str, event: TurnEvent) -> String {
        let mut h = Sha256::new();
        for part in [&self.account[..], url, event.name(), "{}"] {
            h.update((part.len() as u64).to_be_bytes());
            h.update(part.as_bytes());
        }
        format!("sub_{:x}", h.finalize())
    }
    fn send(&self, sub: &Subscription, id: &str, body: &Value) -> Result<Value, &'static str> {
        let (url, ip) = self.target(&sub.url).map_err(|_| "invalid_url")?;
        let host = url.host_str().ok_or("invalid_url")?;
        let port = url.port_or_known_default().ok_or("invalid_url")?;
        let addresses = match ip {
            Some(ip) => vec![SocketAddr::new(ip, port)],
            None => {
                public_callback_addresses((host, port).to_socket_addrs().map_err(|_| "transient")?)?
            }
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5))
            .resolve_to_addrs(host, &addresses)
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
            "events/list" => Some(Ok(json!({"events":[
                TurnEvent::Completed.catalog_entry("A delegated turn reached a confirmed terminal outcome."),
                TurnEvent::RecoveryRequired.catalog_entry("A delegated turn still has an unknown outcome after automatic recovery."),
            ]}))),
            "events/subscribe" | "events/unsubscribe" => Some(self.change(method, params)),
            _ => None,
        }
    }
    fn change(&self, method: &str, p: &Value) -> Result<Value, Value> {
        let event = match p.get("name").and_then(Value::as_str) {
            Some("turn.completed") => TurnEvent::Completed,
            Some("turn.recovery_required") => TurnEvent::RecoveryRequired,
            _ => return Err(invalid()),
        };
        if p.get("arguments") != Some(&json!({}))
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
        self.cleanup()
            .map_err(|_| json!({"code":-32603,"message":"Internal error"}))?;
        let id = self.id(url, event);
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
            event,
            id: id.clone(),
            account: self.account.clone(),
            url: url.to_owned(),
            secret: secret.to_owned(),
            expires,
            old_secret: None,
            old_secret_until: None,
            verified_at: None,
        };
        let mut lock = self.subscriptions.lock().map_err(|_| invalid())?;
        let cached = lock
            .iter()
            .find(|s| {
                s.id == id
                    && s.account == self.account
                    && s.url == url
                    && s.secret == secret
                    && s.expires > Utc::now()
                    && s.verified_at.is_some_and(|at| {
                        at <= Utc::now()
                            && at + ChronoDuration::seconds(VERIFICATION_SECONDS) > Utc::now()
                    })
            })
            .and_then(|s| s.verified_at);
        if cached.is_none() {
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
            sub.verified_at = Some(Utc::now());
        } else {
            sub.verified_at = cached;
        }
        let expires = Utc::now() + ChronoDuration::milliseconds(ttl);
        sub.expires = expires;
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
            json!({"id":id,"refreshBefore":expires.to_rfc3339_opts(chrono::SecondsFormat::AutoSi,true),"cursor":null,"truncated":false}),
        )
    }
    pub fn queue_turn_event(
        &self,
        event: TurnEvent,
        session_id: &str,
        turn_id: &str,
        reason: &str,
        publish: impl FnOnce() + Send + 'static,
    ) -> io::Result<()> {
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
                (event, reason),
                (
                    TurnEvent::Completed,
                    "end_turn"
                        | "refusal"
                        | "max_tokens"
                        | "max_turn_requests"
                        | "failed"
                        | "cancelled"
                ) | (TurnEvent::RecoveryRequired, "outcome_unknown")
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid turn event",
            ));
        }

        let subscriptions = self
            .subscriptions
            .lock()
            .map_err(|_| io::Error::other("subscription lock poisoned"))?;
        let now = Utc::now();
        let events = subscriptions
            .iter()
            .filter(|sub| sub.event == event && sub.expires > now)
            .map(|sub| PendingEvent {
                event,
                subscription_id: sub.id.clone(),
                event_id: format!("evt_{}", Uuid::new_v4()),
                timestamp: now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                session_id: session_id.to_owned(),
                turn_id: turn_id.to_owned(),
                reason: reason.to_owned(),
            })
            .collect::<Vec<_>>();
        drop(subscriptions);
        if events.is_empty() {
            publish();
            return Ok(());
        }

        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("pending events lock poisoned"))?;
        pending.entries.extend(events);
        pending.publish_after_sync.push(Box::new(publish));
        self.sync_pending(&mut pending)
    }

    fn sync_pending(&self, pending: &mut PendingEvents) -> io::Result<()> {
        if !pending.publish_after_sync.is_empty() {
            self.persist_pending(&pending.entries)?;
            // Publish before the delivery worker can take a snapshot of these events.
            for publish in pending.publish_after_sync.drain(..) {
                publish();
            }
        }
        Ok(())
    }

    fn persist_pending(&self, pending: &[PendingEvent]) -> io::Result<()> {
        self.state.replace(StateFile::PendingEvents, |file| {
            serde_json::to_writer(file, pending).map_err(io::Error::other)
        })
    }

    // Only the delivery worker calls this. Queue writes can proceed while it sends.
    pub fn deliver_pending(&self) -> io::Result<()> {
        let batch = {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| io::Error::other("pending events lock poisoned"))?;
            self.sync_pending(&mut pending)?;
            pending.entries.clone()
        };
        let mut finished = HashSet::new();
        for event in batch {
            let current = self
                .subscriptions
                .lock()
                .map_err(|_| io::Error::other("subscription lock poisoned"))?
                .iter()
                .find(|sub| sub.id == event.subscription_id && sub.expires > Utc::now())
                .cloned();
            if let Some(sub) = current {
                if matches!(
                    self.send(&sub, &event.event_id, &event.body()),
                    Err("transient" | "timeout")
                ) {
                    continue;
                }
            }
            finished.insert(event.event_id);
        }
        if !finished.is_empty() {
            let mut pending = self
                .pending
                .lock()
                .map_err(|_| io::Error::other("pending events lock poisoned"))?;
            self.sync_pending(&mut pending)?;
            let mut next = pending.entries.clone();
            next.retain(|event| !finished.contains(&event.event_id));
            self.persist_pending(&next)?;
            pending.entries = next;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trusted_callback_host_allows_subscription_paths_on_https() {
        let host = "receiver.example.com";
        for raw in [
            "https://receiver.example.com/events/first",
            "https://receiver.example.com:443/events/second?delivery=webhook",
        ] {
            let url = callback_url(raw).unwrap();
            assert!(matches_callback_host(&url, host), "{raw}");
        }

        for raw in [
            "http://receiver.example.com/events",
            "https://receiver.example.com:8443/events",
            "https://nested.receiver.example.com/events",
            "https://receiver.example.com.example.org/events",
            "https://receiver.example.com./events",
            "https://user:password@receiver.example.com/events",
            "https://receiver.example.com/events#fragment",
            "https://127.0.0.1/events",
        ] {
            assert!(
                !callback_url(raw).is_ok_and(|url| matches_callback_host(&url, host)),
                "{raw}"
            );
        }
    }

    #[test]
    fn callback_dns_rejects_mixed_public_and_nonpublic_answers() {
        let v4 = "8.8.8.8:443".parse().unwrap();
        let v6 = "[2606:4700:4700::1111]:443".parse().unwrap();
        assert_eq!(
            public_callback_addresses([v4, v6].into_iter()),
            Ok(vec![v4, v6])
        );
        assert_eq!(
            public_callback_addresses(std::iter::empty()),
            Err("transient")
        );
        for raw in [
            "127.0.0.1:443",
            "192.168.0.1:443",
            "169.254.169.254:443",
            "100.64.0.1:443",
            "[::1]:443",
            "[::ffff:10.0.0.1]:443",
            "[fd00::1]:443",
            "[2001:db8::1]:443",
        ] {
            let nonpublic = raw.parse().unwrap();
            assert_eq!(
                public_callback_addresses([v4, nonpublic].into_iter()),
                Err("invalid_url"),
                "{raw}"
            );
        }
    }
}
