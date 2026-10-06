mod agora_token;
mod livekit_token;

use std::{
    collections::HashMap,
    env,
    io::{Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use salvo::{
    http::{
        HeaderName, HeaderValue, StatusCode,
        cookie::{Cookie, SameSite, time::Duration as CookieDuration},
        header,
    },
    prelude::*,
    serve_static::StaticDir,
    websocket::{Message, WebSocket, WebSocketUpgrade},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, mpsc};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::agora_token::{RtcRole, build_rtc_token, validate_credential};
use crate::livekit_token::{build_livekit_token, validate_livekit_url};

static APP_STATE: OnceLock<Arc<AppState>> = OnceLock::new();

#[derive(Clone, Debug)]
struct Config {
    bind_addr: String,
    static_dir: PathBuf,
    public_base_url: String,
    allowed_origin: Option<String>,
    rtc_provider: String,
    agora_app_id: String,
    agora_app_certificate: String,
    livekit_url: String,
    livekit_api_key: String,
    livekit_api_secret: String,
    channel_prefix: String,
    client_uid: u32,
    bridge_uid: u32,
    rtc_token_ttl_seconds: u32,
    client_access_token: String,
    octos_service_token: String,
    browser_grant_ttl_seconds: u64,
    bridge_shared_secret: String,
    session_ttl_seconds: u64,
    session_capacity: usize,
    bridge_heartbeat_timeout_seconds: u64,
    session_start_timeout_seconds: u64,
    demo_mode: bool,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let demo_mode = env_bool("DEMO_MODE", false)?;
        let config = Self {
            bind_addr: env_or("BIND_ADDR", "0.0.0.0:8080"),
            static_dir: PathBuf::from(env_or("STATIC_DIR", "static")),
            public_base_url: env_or("PUBLIC_BASE_URL", "http://localhost:8080")
                .trim_end_matches('/')
                .to_owned(),
            allowed_origin: env::var("ALLOWED_ORIGIN")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            rtc_provider: env_or("RTC_PROVIDER", "agora"),
            agora_app_id: env::var("AGORA_APP_ID").unwrap_or_default(),
            agora_app_certificate: env::var("AGORA_APP_CERTIFICATE").unwrap_or_default(),
            livekit_url: env_or("LIVEKIT_URL", "").trim_end_matches('/').to_owned(),
            livekit_api_key: env::var("LIVEKIT_API_KEY").unwrap_or_default(),
            livekit_api_secret: env::var("LIVEKIT_API_SECRET").unwrap_or_default(),
            channel_prefix: env_or("RTC_CHANNEL_PREFIX", "asr"),
            client_uid: env_u32("RTC_CLIENT_UID", 1001)?,
            bridge_uid: env_u32("RTC_BRIDGE_UID", 9001)?,
            rtc_token_ttl_seconds: env_u32("RTC_TOKEN_TTL_SECONDS", 1200)?,
            client_access_token: env::var("CLIENT_ACCESS_TOKEN").unwrap_or_default(),
            octos_service_token: env::var("OCTOS_SERVICE_TOKEN").unwrap_or_default(),
            browser_grant_ttl_seconds: env_u64("BROWSER_GRANT_TTL_SECONDS", 60)?,
            bridge_shared_secret: env::var("BRIDGE_SHARED_SECRET").unwrap_or_default(),
            session_ttl_seconds: env_u64("SESSION_TTL_SECONDS", 900)?,
            session_capacity: env_u32("SESSION_CAPACITY", 1)? as usize,
            bridge_heartbeat_timeout_seconds: env_u64("BRIDGE_HEARTBEAT_TIMEOUT_SECONDS", 15)?,
            session_start_timeout_seconds: env_u64("SESSION_START_TIMEOUT_SECONDS", 30)?,
            demo_mode,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if self.bridge_shared_secret.len() < 16 {
            return Err("BRIDGE_SHARED_SECRET must contain at least 16 characters".into());
        }
        if self.session_ttl_seconds == 0 || self.session_ttl_seconds > 3_600 {
            return Err("SESSION_TTL_SECONDS must be between 1 and 3600".into());
        }
        if self.session_capacity == 0 || self.session_capacity > 1_000 {
            return Err("SESSION_CAPACITY must be between 1 and 1000".into());
        }
        if !(1..=300).contains(&self.bridge_heartbeat_timeout_seconds) {
            return Err("BRIDGE_HEARTBEAT_TIMEOUT_SECONDS must be between 1 and 300".into());
        }
        if self.session_start_timeout_seconds == 0
            || self.session_start_timeout_seconds > self.session_ttl_seconds
        {
            return Err(
                "SESSION_START_TIMEOUT_SECONDS must be between 1 and SESSION_TTL_SECONDS".into(),
            );
        }
        if !(10..=300).contains(&self.browser_grant_ttl_seconds) {
            return Err("BROWSER_GRANT_TTL_SECONDS must be between 10 and 300".into());
        }
        if !self.octos_service_token.is_empty() {
            if self.octos_service_token.len() < 24 {
                return Err("OCTOS_SERVICE_TOKEN must contain at least 24 characters".into());
            }
            if secret_matches(&self.octos_service_token, &self.bridge_shared_secret)
                || (!self.client_access_token.is_empty()
                    && secret_matches(&self.octos_service_token, &self.client_access_token))
            {
                return Err(
                    "OCTOS_SERVICE_TOKEN must be different from all other service secrets".into(),
                );
            }
        }
        if self.rtc_token_ttl_seconds > 86_400
            || u64::from(self.rtc_token_ttl_seconds) < self.session_ttl_seconds + 60
        {
            return Err(
                "RTC_TOKEN_TTL_SECONDS must cover SESSION_TTL_SECONDS plus 60 seconds and not exceed 86400"
                    .into(),
            );
        }
        if self.channel_prefix.is_empty()
            || self.channel_prefix.len() > 24
            || !self
                .channel_prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "RTC_CHANNEL_PREFIX must contain 1-24 ASCII letters, digits, '-' or '_'".into(),
            );
        }
        if self.rtc_provider != "agora" && self.rtc_provider != "livekit" {
            return Err("RTC_PROVIDER must be \"agora\" or \"livekit\"".into());
        }
        if !self.demo_mode {
            if self.rtc_provider == "agora" {
                validate_credential("AGORA_APP_ID", &self.agora_app_id)?;
                validate_credential("AGORA_APP_CERTIFICATE", &self.agora_app_certificate)?;
            } else {
                validate_livekit_url(&self.livekit_url)?;
                if !self.livekit_url.starts_with("wss://") {
                    return Err("LIVEKIT_URL must use wss:// when DEMO_MODE=false".into());
                }
                if self.livekit_api_key.len() < 8 {
                    return Err("LIVEKIT_API_KEY must contain at least 8 characters".into());
                }
                if self.livekit_api_secret.len() < 16 {
                    return Err("LIVEKIT_API_SECRET must contain at least 16 characters".into());
                }
            }
            if self.client_access_token.len() < 24 {
                return Err(
                    "CLIENT_ACCESS_TOKEN must contain at least 24 characters when DEMO_MODE=false"
                        .into(),
                );
            }
            if self.bridge_shared_secret.len() < 24 {
                return Err(
                    "BRIDGE_SHARED_SECRET must contain at least 24 characters when DEMO_MODE=false"
                        .into(),
                );
            }
            if secret_matches(&self.client_access_token, &self.bridge_shared_secret) {
                return Err(
                    "CLIENT_ACCESS_TOKEN and BRIDGE_SHARED_SECRET must be different".into(),
                );
            }
            if !self.public_base_url.starts_with("https://") {
                return Err("PUBLIC_BASE_URL must use HTTPS when DEMO_MODE=false".into());
            }
            if self.allowed_origin.as_deref() != Some(self.public_base_url.as_str()) {
                return Err(
                    "ALLOWED_ORIGIN must exactly match PUBLIC_BASE_URL when DEMO_MODE=false".into(),
                );
            }
        }
        if self.client_uid == 0 || self.bridge_uid == 0 {
            return Err("RTC_CLIENT_UID and RTC_BRIDGE_UID must be non-zero".into());
        }
        if self.client_uid == self.bridge_uid {
            return Err("RTC_CLIENT_UID and RTC_BRIDGE_UID must be different".into());
        }
        Ok(())
    }
}

fn env_or(name: &str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn env_bool(name: &str, default: bool) -> Result<bool, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<bool>()
            .map_err(|_| format!("{name} must be true or false")),
        Err(_) => Ok(default),
    }
}

fn env_u32(name: &str, default: u32) -> Result<u32, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u32>()
            .map_err(|_| format!("{name} must be an unsigned integer")),
        Err(_) => Ok(default),
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, String> {
    match env::var(name) {
        Ok(value) => value
            .parse::<u64>()
            .map_err(|_| format!("{name} must be an unsigned integer")),
        Err(_) => Ok(default),
    }
}

#[derive(Clone)]
struct Session {
    id: String,
    ticket: String,
    state: String,
    expires_at_ms: u64,
    started_at: Instant,
    owner_subject: Option<String>,
    owner_profile_id: Option<String>,
}

#[derive(Clone)]
struct BrowserGrant {
    subject: String,
    profile_id: String,
    expires_at_ms: u64,
}

struct IssuedBrowserGrant {
    token: String,
    expires_at_ms: u64,
}

#[derive(Clone)]
struct SocketLink {
    id: Uuid,
    tx: mpsc::UnboundedSender<Message>,
}

struct BridgeLink {
    id: Uuid,
    tx: mpsc::UnboundedSender<Message>,
    last_pong_at: Instant,
}

#[derive(Default)]
struct Inner {
    sessions: HashMap<String, Session>,
    browser_grants: HashMap<String, BrowserGrant>,
    bridge: Option<BridgeLink>,
    clients: HashMap<String, SocketLink>,
}

impl Inner {
    fn bridge_online(&self, now: Instant, timeout: Duration) -> bool {
        self.bridge.as_ref().is_some_and(|bridge| {
            !bridge.tx.is_closed() && now.duration_since(bridge.last_pong_at) < timeout
        })
    }
}

fn bridge_timeout(config: &Config) -> Duration {
    Duration::from_secs(config.bridge_heartbeat_timeout_seconds)
}

fn disconnect_bridge(inner: &mut Inner, socket_id: Uuid, message: &str) {
    // A previous connection must never clear its replacement's state.
    if inner
        .bridge
        .as_ref()
        .is_none_or(|bridge| bridge.id != socket_id)
    {
        return;
    }
    inner.bridge = None;
    for session in inner
        .sessions
        .values_mut()
        .filter(|session| session.state != "closed")
    {
        session.state = "closed".into();
        if let Some(client) = inner.clients.remove(&session.id) {
            send_json(
                &client.tx,
                &json!({
                    "type": "asr.error", "sessionId": session.id,
                    "code": "bridge_offline", "message": message,
                }),
            );
            send_json(
                &client.tx,
                &json!({ "type": "session.closed", "sessionId": session.id }),
            );
        }
    }
}

struct AppState {
    config: Config,
    inner: Mutex<Inner>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            config,
            inner: Mutex::new(Inner::default()),
        }
    }
}

fn state() -> &'static Arc<AppState> {
    APP_STATE.get().expect("application state initialized")
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn browser_grant_digest(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

fn issue_browser_grant(
    inner: &mut Inner,
    subject: &str,
    profile_id: &str,
    now_ms: u64,
    ttl_seconds: u64,
) -> IssuedBrowserGrant {
    let mut random = [0_u8; 32];
    random[..16].copy_from_slice(Uuid::new_v4().as_bytes());
    random[16..].copy_from_slice(Uuid::new_v4().as_bytes());
    let token = URL_SAFE_NO_PAD.encode(random);
    let expires_at_ms = now_ms.saturating_add(ttl_seconds.saturating_mul(1_000));
    inner.browser_grants.insert(
        browser_grant_digest(&token),
        BrowserGrant {
            subject: subject.to_owned(),
            profile_id: profile_id.to_owned(),
            expires_at_ms,
        },
    );
    IssuedBrowserGrant {
        token,
        expires_at_ms,
    }
}

fn consume_browser_grant(inner: &mut Inner, token: &str, now_ms: u64) -> Option<BrowserGrant> {
    let grant = inner.browser_grants.remove(&browser_grant_digest(token))?;
    (grant.expires_at_ms >= now_ms).then_some(grant)
}

fn run_healthcheck() -> Result<(), String> {
    let bind_addr = env_or("BIND_ADDR", "0.0.0.0:8080");
    let connect_addr = bind_addr
        .strip_prefix("0.0.0.0:")
        .map(|port| format!("127.0.0.1:{port}"))
        .unwrap_or(bind_addr);
    let socket_addr = connect_addr
        .to_socket_addrs()
        .map_err(|error| error.to_string())?
        .next()
        .ok_or_else(|| "healthcheck address did not resolve".to_owned())?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|error| error.to_string())?;
    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        Err("health endpoint did not return HTTP 200".into())
    }
}

fn send_json(tx: &mpsc::UnboundedSender<Message>, value: &Value) -> bool {
    tx.send(Message::text(value.to_string())).is_ok()
}

fn insert_metric(event: &mut Value, group: &str, name: &str, value: Value) {
    let Some(event_object) = event.as_object_mut() else {
        return;
    };
    let metrics = event_object
        .entry("metrics")
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(metrics_object) = metrics.as_object_mut() else {
        return;
    };
    let group_value = metrics_object
        .entry(group)
        .or_insert_with(|| Value::Object(Default::default()));
    let Some(group_object) = group_value.as_object_mut() else {
        return;
    };
    group_object.insert(name.to_owned(), value);
}

fn elapsed_ms(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 100_000.0).round() / 100.0
}

fn result_delivery(value: &Value) -> Option<(String, String, u64)> {
    let event_type = value.get("type")?.as_str()?;
    if !matches!(event_type, "asr.partial" | "asr.final") {
        return None;
    }
    Some((
        event_type.to_owned(),
        value.get("utteranceId")?.as_str()?.to_owned(),
        value.get("seq")?.as_u64()?,
    ))
}

fn delivery_key(event_type: &str, utterance_id: &str, sequence: u64) -> String {
    format!("{event_type}:{utterance_id}:{sequence}")
}

fn client_delivery_ack(value: &Value, expected_session_id: &str) -> Option<(String, String, u64)> {
    if value.get("type")?.as_str()? != "client.result_ack"
        || value.get("sessionId")?.as_str()? != expected_session_id
    {
        return None;
    }
    let event_type = value.get("eventType")?.as_str()?;
    if !matches!(event_type, "asr.partial" | "asr.final") {
        return None;
    }
    Some((
        event_type.to_owned(),
        value.get("utteranceId")?.as_str()?.to_owned(),
        value.get("seq")?.as_u64()?,
    ))
}

fn render_error(res: &mut Response, status_code: StatusCode, code: &str, message: &str) {
    res.status_code(status_code);
    res.render(Json(
        json!({ "error": { "code": code, "message": message } }),
    ));
}

fn secret_matches(expected: &str, supplied: &str) -> bool {
    expected.len() == supplied.len() && bool::from(expected.as_bytes().ct_eq(supplied.as_bytes()))
}

fn bearer_token(req: &Request) -> Option<&str> {
    req.headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
}

fn client_authorized(req: &Request, config: &Config) -> bool {
    if config.demo_mode && config.client_access_token.is_empty() {
        return true;
    }
    bearer_token(req).is_some_and(|value| secret_matches(&config.client_access_token, value))
}

fn require_octos_service_access(req: &Request, res: &mut Response) -> bool {
    let config = &state().config;
    if config.octos_service_token.is_empty() {
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "octos_integration_disabled",
            "Octos browser grants are not configured",
        );
        return false;
    }
    if bearer_token(req).is_some_and(|value| secret_matches(&config.octos_service_token, value)) {
        return true;
    }
    res.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"octos-service\""),
    );
    render_error(
        res,
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Invalid Octos service token",
    );
    false
}

async fn session_authorized(req: &Request, session_id: &str) -> bool {
    let app = state();
    if client_authorized(req, &app.config) {
        return true;
    }
    // The session ticket is issued under two distinct names on purpose:
    // cookie jars replace same-named cookies, so a single name for both the
    // WebSocket path and the API path would silently drop one of them.
    let ticket = req
        .cookie("asr_session")
        .or_else(|| req.cookie("asr_session_api"))
        .map(|cookie| cookie.value().to_owned())
        .unwrap_or_default();
    let inner = app.inner.lock().await;
    inner.sessions.get(session_id).is_some_and(|session| {
        session.state != "closed" && secret_matches(&session.ticket, &ticket)
    })
}

async fn require_session_access(req: &Request, res: &mut Response, session_id: &str) -> bool {
    if session_authorized(req, session_id).await {
        return true;
    }
    render_error(
        res,
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "Invalid session authorization",
    );
    false
}

fn session_cookie(session: &Session, config: &Config) -> Cookie<'static> {
    Cookie::build(("asr_session", session.ticket.clone()))
        .path(format!("/ws/client/{}", session.id))
        .http_only(true)
        .secure(config.public_base_url.starts_with("https://"))
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::seconds(
            config.session_ttl_seconds.try_into().unwrap_or(i64::MAX),
        ))
        .build()
}

fn session_api_cookie(session: &Session, config: &Config) -> Cookie<'static> {
    Cookie::build(("asr_session_api", session.ticket.clone()))
        .path(format!("/api/v1/sessions/{}", session.id))
        .http_only(true)
        .secure(config.public_base_url.starts_with("https://"))
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::seconds(
            config.session_ttl_seconds.try_into().unwrap_or(i64::MAX),
        ))
        .build()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct BrowserGrantRequest {
    subject: String,
    profile_id: String,
}

fn valid_grant_identity(value: &str) -> bool {
    !value.trim().is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

#[handler]
async fn create_browser_grant(req: &mut Request, res: &mut Response) {
    if !require_octos_service_access(req, res) {
        return;
    }
    let request = match req.parse_json::<BrowserGrantRequest>().await {
        Ok(request) => request,
        Err(_) => {
            render_error(
                res,
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "A valid subject and profileId are required",
            );
            return;
        }
    };
    if !valid_grant_identity(&request.subject) || !valid_grant_identity(&request.profile_id) {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "A valid subject and profileId are required",
        );
        return;
    }
    let app = state();
    let mut inner = app.inner.lock().await;
    let issued = issue_browser_grant(
        &mut inner,
        request.subject.trim(),
        request.profile_id.trim(),
        unix_ms(),
        app.config.browser_grant_ttl_seconds,
    );
    res.status_code(StatusCode::CREATED);
    res.render(Json(json!({
        "grant": issued.token,
        "expiresAtMs": issued.expires_at_ms,
    })));
}

#[handler]
async fn healthz(res: &mut Response) {
    res.render(Text::Plain("ok"));
}

#[handler]
async fn readyz(res: &mut Response) {
    let inner = state().inner.lock().await;
    if inner.bridge_online(Instant::now(), bridge_timeout(&state().config)) {
        res.render(Text::Plain("ready"));
    } else {
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_offline",
            "The LAN bridge is not connected",
        );
    }
}

#[handler]
async fn status(res: &mut Response) {
    let app = state();
    let inner = app.inner.lock().await;
    let active_sessions = inner
        .sessions
        .values()
        .filter(|session| session.state != "closed")
        .count();
    res.render(Json(json!({
        "service": "agora-ominix-control-plane",
        "bridgeOnline": inner.bridge_online(Instant::now(), bridge_timeout(&app.config)),
        "sessionStartTimeoutSeconds": app.config.session_start_timeout_seconds,
        "demoMode": app.config.demo_mode,
        "rtcProvider": app.config.rtc_provider,
        "accessProtected": !app.config.client_access_token.is_empty(),
        "capacity": app.config.session_capacity,
        "activeSessions": active_sessions,
    })));
}

#[handler]
async fn create_session(req: &mut Request, res: &mut Response) {
    let app = state();
    let operator_authorized = client_authorized(req, &app.config);
    let supplied_grant = (!operator_authorized)
        .then(|| bearer_token(req).map(str::to_owned))
        .flatten();
    if !operator_authorized && supplied_grant.is_none() {
        render_error(
            res,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "A valid browser grant is required",
        );
        return;
    }
    let mut inner = app.inner.lock().await;

    if !inner.bridge_online(Instant::now(), bridge_timeout(&app.config)) {
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_offline",
            "The LAN bridge is not connected",
        );
        return;
    }

    let active_sessions = inner
        .sessions
        .values()
        .filter(|session| session.state != "closed")
        .count();
    if active_sessions >= app.config.session_capacity {
        render_error(
            res,
            StatusCode::CONFLICT,
            "session_busy",
            "The ASR service is at its configured session capacity",
        );
        return;
    }

    let owner = if operator_authorized {
        None
    } else {
        let Some(grant) = supplied_grant
            .as_deref()
            .and_then(|token| consume_browser_grant(&mut inner, token, unix_ms()))
        else {
            render_error(
                res,
                StatusCode::UNAUTHORIZED,
                "invalid_browser_grant",
                "The browser grant is invalid, expired, or already used",
            );
            return;
        };
        Some(grant)
    };

    let now = unix_ms();
    let id = Uuid::new_v4().to_string();
    let channel = format!("{}-{}", app.config.channel_prefix, Uuid::new_v4().simple());
    // Provider-branched credentials: the bridge fragment goes into the
    // session.start event, the client fragment into the HTTP response.
    let (bridge_cred, client_cred) = if app.config.demo_mode {
        (
            json!({"agora": {
                "appId": "",
                "channel": channel,
                "uid": app.config.bridge_uid,
                "token": "",
            }}),
            json!({"agora": {
                "appId": "",
                "channel": channel,
                "uid": app.config.client_uid,
                "token": "",
            }}),
        )
    } else if app.config.rtc_provider == "livekit" {
        let client_identity = format!("client-{}", app.config.client_uid);
        let bridge_identity = format!("bridge-{}", app.config.bridge_uid);
        let client_token = match build_livekit_token(
            &app.config.livekit_api_key,
            &app.config.livekit_api_secret,
            &client_identity,
            &channel,
            true,
            app.config.rtc_token_ttl_seconds,
        ) {
            Ok(token) => token,
            Err(error) => {
                error!(%error, "failed to create client LiveKit token");
                render_error(
                    res,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "token_generation_failed",
                    "Could not issue RTC credentials",
                );
                return;
            }
        };
        let bridge_token = match build_livekit_token(
            &app.config.livekit_api_key,
            &app.config.livekit_api_secret,
            &bridge_identity,
            &channel,
            true,
            app.config.rtc_token_ttl_seconds,
        ) {
            Ok(token) => token,
            Err(error) => {
                error!(%error, "failed to create bridge LiveKit token");
                render_error(
                    res,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "token_generation_failed",
                    "Could not issue RTC credentials",
                );
                return;
            }
        };
        (
            json!({"livekit": {
                "url": app.config.livekit_url,
                "room": channel,
                "identity": bridge_identity,
                "token": bridge_token,
            }}),
            json!({"livekit": {
                "url": app.config.livekit_url,
                "room": channel,
                "identity": client_identity,
                "token": client_token,
            }}),
        )
    } else {
        let client = match build_rtc_token(
            &app.config.agora_app_id,
            &app.config.agora_app_certificate,
            &channel,
            app.config.client_uid,
            RtcRole::AudioPublisher,
            app.config.rtc_token_ttl_seconds,
        ) {
            Ok(token) => token,
            Err(error) => {
                error!(%error, "failed to create client RTC token");
                render_error(
                    res,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "token_generation_failed",
                    "Could not issue RTC credentials",
                );
                return;
            }
        };
        let bridge = match build_rtc_token(
            &app.config.agora_app_id,
            &app.config.agora_app_certificate,
            &channel,
            app.config.bridge_uid,
            RtcRole::Subscriber,
            app.config.rtc_token_ttl_seconds,
        ) {
            Ok(token) => token,
            Err(error) => {
                error!(%error, "failed to create bridge RTC token");
                render_error(
                    res,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "token_generation_failed",
                    "Could not issue RTC credentials",
                );
                return;
            }
        };
        (
            json!({"agora": {
                "appId": app.config.agora_app_id,
                "channel": channel,
                "uid": app.config.bridge_uid,
                "token": bridge,
            }}),
            json!({"agora": {
                "appId": app.config.agora_app_id,
                "channel": channel,
                "uid": app.config.client_uid,
                "token": client,
            }}),
        )
    };
    let session = Session {
        id,
        ticket: Uuid::new_v4().to_string(),
        state: "starting".into(),
        expires_at_ms: now + app.config.session_ttl_seconds * 1000,
        started_at: Instant::now(),
        owner_subject: owner.as_ref().map(|grant| grant.subject.clone()),
        owner_profile_id: owner.as_ref().map(|grant| grant.profile_id.clone()),
    };

    let mut start_event = json!({
        "type": "session.start",
        "sessionId": session.id,
    });
    if let (Some(event_object), Some(cred_object)) =
        (start_event.as_object_mut(), bridge_cred.as_object())
    {
        event_object.extend(cred_object.clone());
    }
    let bridge_sent = inner
        .bridge
        .as_ref()
        .is_some_and(|bridge| send_json(&bridge.tx, &start_event));
    if !bridge_sent {
        if let Some(bridge_id) = inner.bridge.as_ref().map(|bridge| bridge.id) {
            disconnect_bridge(&mut inner, bridge_id, "内网 Bridge 连接中断");
        }
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_offline",
            "The LAN bridge disconnected while starting the session",
        );
        return;
    }

    let mut response = json!({
        "sessionId": session.id,
        "state": session.state,
        "expiresAtMs": session.expires_at_ms,
        "eventsWsPath": format!("/ws/client/{}", session.id),
        "demoMode": app.config.demo_mode,
        "rtcProvider": app.config.rtc_provider,
        "sessionStartTimeoutSeconds": app.config.session_start_timeout_seconds,
    });
    if let (Some(response_object), Some(cred_object)) =
        (response.as_object_mut(), client_cred.as_object())
    {
        response_object.extend(cred_object.clone());
    }
    let cookie = session_cookie(&session, &app.config);
    let api_cookie = session_api_cookie(&session, &app.config);
    info!(
        session_id = %session.id,
        owner_subject = session.owner_subject.as_deref().unwrap_or("operator"),
        owner_profile_id = session.owner_profile_id.as_deref().unwrap_or("operator"),
        "session created"
    );
    inner.sessions.insert(session.id.clone(), session);
    res.add_cookie(cookie);
    res.add_cookie(api_cookie);
    res.status_code(StatusCode::CREATED);
    res.render(Json(response));
}

#[handler]
async fn commit_utterance(req: &mut Request, res: &mut Response) {
    let Some(id) = req.param::<String>("id") else {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "bad_session_id",
            "Missing session id",
        );
        return;
    };
    if !require_session_access(req, res, &id).await {
        return;
    }
    forward_session_command(req, res, "utterance.commit").await;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpeakRequest {
    text: String,
    voice: Option<String>,
    speed: Option<f32>,
    instruct: Option<String>,
}

fn valid_speak_text(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 500 && !value.chars().any(char::is_control)
}

fn valid_tts_voice(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_tts_speed(value: f32) -> bool {
    value.is_finite() && (0.2..=3.0).contains(&value)
}

fn valid_tts_instruct(value: &str) -> bool {
    value.chars().count() <= 200 && !value.chars().any(char::is_control)
}

#[handler]
async fn speak_text(req: &mut Request, res: &mut Response) {
    let Some(id) = req.param::<String>("id") else {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "bad_session_id",
            "Missing session id",
        );
        return;
    };
    if !require_session_access(req, res, &id).await {
        return;
    }
    let request = match req.parse_json::<SpeakRequest>().await {
        Ok(request) => request,
        Err(_) => {
            render_error(
                res,
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "A JSON body with a text field is required",
            );
            return;
        }
    };
    let text = request.text.trim().to_owned();
    if !valid_speak_text(&text) {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "invalid_text",
            "text must contain 1-500 non-control characters",
        );
        return;
    }
    let voice = request
        .voice
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if voice
        .as_deref()
        .is_some_and(|value| !valid_tts_voice(value))
    {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "invalid_voice",
            "voice must be 1-64 ASCII letters, digits, '-' or '_'",
        );
        return;
    }
    let speed = request.speed;
    if speed.is_some_and(|value| !valid_tts_speed(value)) {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "invalid_speed",
            "speed must be a number between 0.2 and 3.0",
        );
        return;
    }
    let instruct = request
        .instruct
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty());
    if instruct
        .as_deref()
        .is_some_and(|value| !valid_tts_instruct(value))
    {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "invalid_instruct",
            "instruct must contain at most 200 non-control characters",
        );
        return;
    }
    let app = state();
    let inner = app.inner.lock().await;
    if !inner.sessions.contains_key(&id) {
        render_error(
            res,
            StatusCode::NOT_FOUND,
            "session_not_found",
            "Session not found",
        );
        return;
    }
    let mut event = json!({
        "type": "tts.speak",
        "sessionId": id,
        "text": text,
    });
    if let Some(voice) = voice {
        event["voice"] = json!(voice);
    }
    if let Some(speed) = speed {
        event["speed"] = json!(speed);
    }
    if let Some(instruct) = instruct {
        event["instruct"] = json!(instruct);
    }
    let sent = inner.bridge_online(Instant::now(), bridge_timeout(&app.config))
        && inner
            .bridge
            .as_ref()
            .is_some_and(|bridge| send_json(&bridge.tx, &event));
    if !sent {
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_offline",
            "The LAN bridge is not connected",
        );
        return;
    }
    res.status_code(StatusCode::ACCEPTED);
    res.render(Json(json!({ "accepted": true })));
}

#[handler]
async fn delete_session(req: &mut Request, res: &mut Response) {
    let Some(id) = req.param::<String>("id") else {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "bad_session_id",
            "Missing session id",
        );
        return;
    };
    if !require_session_access(req, res, &id).await {
        return;
    }
    let app = state();
    let mut inner = app.inner.lock().await;
    let Some(session) = inner.sessions.get_mut(&id) else {
        render_error(
            res,
            StatusCode::NOT_FOUND,
            "session_not_found",
            "Session not found",
        );
        return;
    };
    session.state = "closed".into();
    if let Some(bridge) = &inner.bridge {
        send_json(
            &bridge.tx,
            &json!({ "type": "session.stop", "sessionId": id }),
        );
    }
    if let Some(client) = inner.clients.remove(&id) {
        send_json(
            &client.tx,
            &json!({ "type": "session.closed", "sessionId": id }),
        );
    }
    info!(session_id = %id, "session stopped");
    res.status_code(StatusCode::NO_CONTENT);
}

async fn forward_session_command(req: &mut Request, res: &mut Response, event_type: &str) {
    let Some(id) = req.param::<String>("id") else {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "bad_session_id",
            "Missing session id",
        );
        return;
    };
    let app = state();
    let inner = app.inner.lock().await;
    if !inner.sessions.contains_key(&id) {
        render_error(
            res,
            StatusCode::NOT_FOUND,
            "session_not_found",
            "Session not found",
        );
        return;
    }
    let sent = inner.bridge_online(Instant::now(), bridge_timeout(&app.config))
        && inner.bridge.as_ref().is_some_and(|bridge| {
            send_json(&bridge.tx, &json!({ "type": event_type, "sessionId": id }))
        });
    if !sent {
        render_error(
            res,
            StatusCode::SERVICE_UNAVAILABLE,
            "bridge_offline",
            "The LAN bridge is not connected",
        );
        return;
    }
    res.status_code(StatusCode::ACCEPTED);
    res.render(Json(json!({ "accepted": true })));
}

fn origin_allowed(req: &Request, config: &Config) -> bool {
    let Some(expected) = &config.allowed_origin else {
        return true;
    };
    req.headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|origin| origin == expected)
}

#[handler]
async fn bridge_ws(req: &mut Request, res: &mut Response) {
    let app = state().clone();
    let authorized = bearer_token(req)
        .is_some_and(|value| secret_matches(&app.config.bridge_shared_secret, value));
    if !authorized {
        render_error(
            res,
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Invalid bridge secret",
        );
        return;
    }
    let app_for_socket = app.clone();
    if let Err(error) = WebSocketUpgrade::new()
        .upgrade(req, res, move |socket| {
            bridge_socket(socket, app_for_socket)
        })
        .await
    {
        error!(%error, "bridge websocket upgrade failed");
    }
}

async fn bridge_socket(mut socket: WebSocket, app: Arc<AppState>) {
    let socket_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let timeout = bridge_timeout(&app.config);
    let mut last_pong_at = Instant::now();
    let mut pending_ping: Option<Vec<u8>> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5).min(timeout / 3));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    {
        let mut inner = app.inner.lock().await;
        if let Some(previous_id) = inner.bridge.as_ref().map(|bridge| bridge.id) {
            disconnect_bridge(
                &mut inner,
                previous_id,
                "内网 Bridge 已重新连接，请重新开始会话",
            );
            warn!("replaced an existing bridge connection");
        }
        inner.bridge = Some(BridgeLink {
            id: socket_id,
            tx,
            last_pong_at,
        });
    }
    info!(bridge_connection = %socket_id, "bridge connected");

    loop {
        let deadline = tokio::time::Instant::from_std(last_pong_at + timeout);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                warn!(bridge_connection = %socket_id, "bridge heartbeat timed out");
                break;
            }
            outbound = rx.recv() => {
                let Some(message) = outbound else { break; };
                if !matches!(tokio::time::timeout_at(deadline, socket.send(message)).await, Ok(Ok(()))) {
                    break;
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(message)) if message.is_pong() => {
                        if pending_ping.as_deref() == Some(message.as_bytes()) {
                            pending_ping = None;
                            last_pong_at = Instant::now();
                            let mut inner = app.inner.lock().await;
                            if let Some(bridge) = inner.bridge.as_mut().filter(|bridge| bridge.id == socket_id) {
                                bridge.last_pong_at = last_pong_at;
                            }
                        }
                    }
                    Some(Ok(message)) if message.is_text() => {
                        if let Ok(text) = message.as_str() {
                            handle_bridge_event(&app, socket_id, text).await;
                        }
                    }
                    Some(Ok(message)) if message.is_close() => break,
                    None => break,
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        warn!(%error, "bridge websocket receive error");
                        break;
                    }
                }
            }
            _ = heartbeat.tick() => {
                let payload = pending_ping.get_or_insert_with(|| Uuid::new_v4().as_bytes().to_vec());
                if !matches!(tokio::time::timeout_at(deadline, socket.send(Message::ping(payload.clone()))).await, Ok(Ok(()))) {
                    break;
                }
            }
        }
    }

    disconnect_bridge(
        &mut *app.inner.lock().await,
        socket_id,
        "内网 Bridge 连接中断或心跳超时",
    );
    info!(bridge_connection = %socket_id, "bridge disconnected");
}

async fn handle_bridge_event(app: &Arc<AppState>, socket_id: Uuid, text: &str) {
    let relay_started = Instant::now();
    let bridge_event_received_at_ms = unix_ms();
    let mut event: Value = match serde_json::from_str(text) {
        Ok(event) => event,
        Err(error) => {
            warn!(%error, "ignored invalid bridge message");
            return;
        }
    };
    let Some(event_type) = event.get("type").and_then(Value::as_str).map(str::to_owned) else {
        warn!("ignored bridge message without type");
        return;
    };
    let Some(session_id) = event
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        warn!(event_type, "ignored bridge message without sessionId");
        return;
    };
    let mut inner = app.inner.lock().await;
    if inner
        .bridge
        .as_ref()
        .is_none_or(|bridge| bridge.id != socket_id)
    {
        return;
    }
    let Some(session) = inner
        .sessions
        .get_mut(&session_id)
        .filter(|session| session.state != "closed")
    else {
        warn!(session_id, event_type, "ignored event for unknown session");
        return;
    };
    match event_type.as_str() {
        "session.ready" => session.state = "ready".into(),
        "asr.error" => session.state = "error".into(),
        "session.closed" => session.state = "closed".into(),
        "asr.partial" | "asr.final" | "trace.update" | "tts.started" | "tts.finished"
        | "tts.error" => {}
        _ => {
            warn!(session_id, event_type, "ignored unknown bridge event");
            return;
        }
    }
    if inner.clients.contains_key(&session_id) {
        insert_metric(
            &mut event,
            "vps",
            "bridgeEventReceivedAtUnixMs",
            json!(bridge_event_received_at_ms),
        );
        insert_metric(
            &mut event,
            "vps",
            "relayQueueMs",
            json!(elapsed_ms(relay_started)),
        );
        insert_metric(
            &mut event,
            "vps",
            "clientEnqueuedAtUnixMs",
            json!(unix_ms()),
        );
    }
    if let Some(client) = inner.clients.get(&session_id) {
        send_json(&client.tx, &event);
    }
}

#[handler]
async fn client_ws(req: &mut Request, res: &mut Response) {
    let app = state().clone();
    if !origin_allowed(req, &app.config) {
        render_error(
            res,
            StatusCode::FORBIDDEN,
            "origin_forbidden",
            "Origin is not allowed",
        );
        return;
    }
    let Some(session_id) = req.param::<String>("id") else {
        render_error(
            res,
            StatusCode::BAD_REQUEST,
            "bad_session_id",
            "Missing session id",
        );
        return;
    };
    let ticket = req
        .cookie("asr_session")
        .map(|cookie| cookie.value().to_owned())
        .unwrap_or_default();
    {
        let inner = app.inner.lock().await;
        let valid = inner
            .sessions
            .get(&session_id)
            .is_some_and(|session| session.ticket == ticket && session.state != "closed");
        if !valid {
            render_error(
                res,
                StatusCode::UNAUTHORIZED,
                "invalid_ticket",
                "Invalid session ticket",
            );
            return;
        }
    }
    let app_for_socket = app.clone();
    if let Err(error) = WebSocketUpgrade::new()
        .upgrade(req, res, move |socket| {
            client_socket(socket, app_for_socket, session_id)
        })
        .await
    {
        error!(%error, "client websocket upgrade failed");
    }
}

async fn client_socket(mut socket: WebSocket, app: Arc<AppState>, session_id: String) {
    let socket_id = Uuid::new_v4();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut pending_deliveries: HashMap<String, Instant> = HashMap::new();
    {
        let mut inner = app.inner.lock().await;
        let Some(state_value) = inner
            .sessions
            .get(&session_id)
            .filter(|session| session.state != "closed")
            .map(|session| session.state.clone())
        else {
            return;
        };
        send_json(
            &tx,
            &json!({
                "type": "session.snapshot",
                "sessionId": session_id,
                "state": state_value,
                "bridgeOnline": inner.bridge_online(Instant::now(), bridge_timeout(&app.config)),
            }),
        );
        inner
            .clients
            .insert(session_id.clone(), SocketLink { id: socket_id, tx });
    }

    loop {
        tokio::select! {
            outbound = rx.recv() => {
                let Some(message) = outbound else { break; };
                let tracked_result = message
                    .as_str()
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(text).ok())
                    .as_ref()
                    .and_then(result_delivery);
                if socket.send(message).await.is_err() { break; }
                if let Some((event_type, utterance_id, sequence)) = tracked_result {
                    if pending_deliveries.len() >= 2_048 {
                        pending_deliveries.clear();
                    }
                    pending_deliveries.insert(
                        delivery_key(&event_type, &utterance_id, sequence),
                        Instant::now(),
                    );
                }
            }
            inbound = socket.recv() => {
                match inbound {
                    Some(Ok(message)) if message.is_text() => {
                        let ack = message
                            .as_str()
                            .ok()
                            .and_then(|text| serde_json::from_str::<Value>(text).ok())
                            .as_ref()
                            .and_then(|value| client_delivery_ack(value, &session_id));
                        let Some((event_type, utterance_id, sequence)) = ack else {
                            continue;
                        };
                        let key = delivery_key(&event_type, &utterance_id, sequence);
                        let Some(sent_at) = pending_deliveries.remove(&key) else {
                            continue;
                        };
                        let ack_rtt_ms = elapsed_ms(sent_at);
                        let update = json!({
                            "type": "trace.update",
                            "sessionId": session_id,
                            "utteranceId": utterance_id,
                            "seq": sequence,
                            "eventType": event_type,
                            "metrics": {
                                "delivery": {
                                    "vpsBrowserAckRttMs": ack_rtt_ms,
                                    "estimatedVpsToBrowserMs":
                                        (ack_rtt_ms * 50.0).round() / 100.0,
                                }
                            }
                        });
                        if socket.send(Message::text(update.to_string())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(message)) if message.is_close() => break,
                    None => break,
                    Some(Err(error)) => {
                        warn!(%error, session_id, "client websocket receive error");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    let mut inner = app.inner.lock().await;
    if inner
        .clients
        .get(&session_id)
        .is_some_and(|link| link.id == socket_id)
    {
        inner.clients.remove(&session_id);
    }
    let should_stop = inner.sessions.get_mut(&session_id).is_some_and(|session| {
        if session.state == "closed" {
            false
        } else {
            session.state = "closed".into();
            true
        }
    });
    if should_stop {
        if let Some(bridge) = &inner.bridge {
            send_json(
                &bridge.tx,
                &json!({ "type": "session.stop", "sessionId": session_id }),
            );
        }
        info!(session_id, "session stopped after client disconnect");
    }
}

async fn session_reaper(app: Arc<AppState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let now = unix_ms();
        let mut inner = app.inner.lock().await;
        if !inner.bridge_online(Instant::now(), bridge_timeout(&app.config))
            && let Some(bridge_id) = inner.bridge.as_ref().map(|bridge| bridge.id)
        {
            disconnect_bridge(&mut inner, bridge_id, "内网 Bridge 连接中断或心跳超时");
        }
        expire_starting_sessions(
            &mut inner,
            Instant::now(),
            Duration::from_secs(app.config.session_start_timeout_seconds),
        );
        let expired_ids: Vec<String> = inner
            .sessions
            .values_mut()
            .filter(|session| session.state != "closed" && session.expires_at_ms <= now)
            .map(|session| {
                session.state = "closed".into();
                session.id.clone()
            })
            .collect();
        for session_id in &expired_ids {
            if let Some(bridge) = &inner.bridge {
                send_json(
                    &bridge.tx,
                    &json!({ "type": "session.stop", "sessionId": session_id }),
                );
            }
            if let Some(client) = inner.clients.remove(session_id) {
                send_json(
                    &client.tx,
                    &json!({ "type": "session.expired", "sessionId": session_id }),
                );
            }
            info!(session_id, "expired session stopped");
        }
        inner.sessions.retain(|_, session| {
            session.state != "closed" || session.expires_at_ms.saturating_add(60_000) > now
        });
        inner
            .browser_grants
            .retain(|_, grant| grant.expires_at_ms > now);
    }
}

fn expire_starting_sessions(inner: &mut Inner, now: Instant, timeout: Duration) {
    for session in inner.sessions.values_mut().filter(|session| {
        session.state == "starting" && now.duration_since(session.started_at) >= timeout
    }) {
        session.state = "closed".into();
        if let Some(bridge) = &inner.bridge {
            send_json(
                &bridge.tx,
                &json!({"type": "session.stop", "sessionId": session.id}),
            );
        }
        if let Some(client) = inner.clients.remove(&session.id) {
            send_json(
                &client.tx,
                &json!({
                    "type": "asr.error", "sessionId": session.id,
                    "code": "session_start_timeout", "message": "Bridge 未在限定时间内准备好会话，请稍后重试",
                }),
            );
            send_json(
                &client.tx,
                &json!({"type": "session.closed", "sessionId": session.id}),
            );
        }
    }
}

#[handler]
async fn security_headers(
    req: &mut Request,
    depot: &mut Depot,
    res: &mut Response,
    ctrl: &mut FlowCtrl,
) {
    ctrl.call_next(req, depot, res).await;
    let headers = res.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' https://download.agora.io; style-src 'self'; connect-src 'self' https: wss:; worker-src 'self' blob:; media-src 'self' blob:; img-src 'self' data:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'",
        ),
    );
    headers.insert(
        HeaderName::from_static("permissions-policy"),
        HeaderValue::from_static("microphone=(self), camera=(), geolocation=()"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
}

#[tokio::main]
async fn main() {
    if env::args().any(|argument| argument == "--healthcheck") {
        if let Err(error) = run_healthcheck() {
            eprintln!("healthcheck failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "agora_sensevoice_control_plane=info,salvo=info".into()),
        )
        .init();

    let config = Config::from_env().unwrap_or_else(|error| {
        eprintln!("configuration error: {error}");
        std::process::exit(2);
    });
    let bind_addr = config.bind_addr.clone();
    let static_dir = config.static_dir.clone();
    info!(
        bind_addr,
        public_base_url = %config.public_base_url,
        demo_mode = config.demo_mode,
        "starting control plane"
    );
    APP_STATE
        .set(Arc::new(AppState::new(config)))
        .unwrap_or_else(|_| panic!("application state already initialized"));
    tokio::spawn(session_reaper(state().clone()));

    let api = Router::with_path("api/v1")
        .push(Router::with_path("status").get(status))
        .push(Router::with_path("browser-grants").post(create_browser_grant))
        .push(Router::with_path("sessions").post(create_session))
        .push(
            Router::with_path("sessions/{id}")
                .delete(delete_session)
                .push(Router::with_path("commit").post(commit_utterance))
                .push(Router::with_path("speak").post(speak_text)),
        );
    let router = Router::new()
        .hoop(Logger::new())
        .hoop(CatchPanic::new())
        .hoop(security_headers)
        .push(Router::with_path("healthz").get(healthz))
        .push(Router::with_path("readyz").get(readyz))
        .push(api)
        .push(Router::with_path("ws/bridge").get(bridge_ws))
        .push(Router::with_path("ws/client/{id}").get(client_ws))
        .push(
            Router::with_path("{**path}").get(StaticDir::new([static_dir]).defaults("index.html")),
        );

    let acceptor = TcpListener::new(bind_addr).bind().await;
    Server::new(acceptor).serve(router).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_config() -> Config {
        Config {
            bind_addr: "127.0.0.1:0".into(),
            static_dir: "static".into(),
            public_base_url: "http://localhost".into(),
            allowed_origin: None,
            rtc_provider: "agora".into(),
            agora_app_id: String::new(),
            agora_app_certificate: String::new(),
            livekit_url: String::new(),
            livekit_api_key: String::new(),
            livekit_api_secret: String::new(),
            channel_prefix: "test".into(),
            client_uid: 1001,
            bridge_uid: 9001,
            rtc_token_ttl_seconds: 1200,
            client_access_token: String::new(),
            octos_service_token: String::new(),
            browser_grant_ttl_seconds: 60,
            bridge_shared_secret: "0123456789abcdef".into(),
            session_ttl_seconds: 900,
            session_capacity: 1,
            bridge_heartbeat_timeout_seconds: 15,
            session_start_timeout_seconds: 30,
            demo_mode: true,
        }
    }

    fn test_session(id: &str, state: &str, started_at: Instant) -> Session {
        Session {
            id: id.into(),
            ticket: "ticket".into(),
            state: state.into(),
            expires_at_ms: unix_ms() + 60_000,
            started_at,
            owner_subject: None,
            owner_profile_id: None,
        }
    }

    #[test]
    fn bridge_online_requires_fresh_heartbeat_and_open_sender() {
        let now = Instant::now();
        let timeout = Duration::from_secs(15);
        let (tx, rx) = mpsc::unbounded_channel();
        let mut inner = Inner::default();
        assert!(!inner.bridge_online(now, timeout));
        inner.bridge = Some(BridgeLink {
            id: Uuid::new_v4(),
            tx,
            last_pong_at: now,
        });
        assert!(inner.bridge_online(now + Duration::from_secs(14), timeout));
        assert!(!inner.bridge_online(now + timeout, timeout));
        drop(rx);
        assert!(!inner.bridge_online(now, timeout));
    }

    #[test]
    fn stale_connection_cleanup_cannot_disconnect_replacement() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let mut inner = Inner::default();
        inner.bridge = Some(BridgeLink {
            id,
            tx,
            last_pong_at: Instant::now(),
        });
        inner
            .sessions
            .insert("s1".into(), test_session("s1", "ready", Instant::now()));
        disconnect_bridge(&mut inner, Uuid::new_v4(), "old connection");
        assert_eq!(inner.bridge.as_ref().unwrap().id, id);
        assert_eq!(inner.sessions["s1"].state, "ready");
    }

    #[test]
    fn bridge_disconnect_closes_sessions_and_notifies_clients() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        let mut inner = Inner::default();
        inner.bridge = Some(BridgeLink {
            id,
            tx,
            last_pong_at: Instant::now(),
        });
        inner
            .sessions
            .insert("s1".into(), test_session("s1", "starting", Instant::now()));
        inner
            .sessions
            .insert("s2".into(), test_session("s2", "ready", Instant::now()));
        inner.clients.insert(
            "s1".into(),
            SocketLink {
                id: Uuid::new_v4(),
                tx: client_tx,
            },
        );
        disconnect_bridge(&mut inner, id, "offline");
        assert!(inner.bridge.is_none());
        assert!(
            inner
                .sessions
                .values()
                .all(|session| session.state == "closed")
        );
        assert!(inner.clients.is_empty());
        let error: Value =
            serde_json::from_str(client_rx.try_recv().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(error["code"], "bridge_offline");
        let closed: Value =
            serde_json::from_str(client_rx.try_recv().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(closed["type"], "session.closed");
    }

    #[tokio::test]
    async fn startup_timeout_releases_capacity_and_late_ready_cannot_revive_session() {
        let started = Instant::now();
        let app = Arc::new(AppState::new(base_config()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (client_tx, mut client_rx) = mpsc::unbounded_channel();
        let id = Uuid::new_v4();
        {
            let mut inner = app.inner.lock().await;
            inner.bridge = Some(BridgeLink {
                id,
                tx,
                last_pong_at: started,
            });
            inner
                .sessions
                .insert("s1".into(), test_session("s1", "starting", started));
            inner
                .sessions
                .insert("s2".into(), test_session("s2", "ready", started));
            inner.clients.insert(
                "s1".into(),
                SocketLink {
                    id: Uuid::new_v4(),
                    tx: client_tx,
                },
            );
            expire_starting_sessions(
                &mut inner,
                started + Duration::from_secs(29),
                Duration::from_secs(30),
            );
            assert_eq!(inner.sessions["s1"].state, "starting");
            expire_starting_sessions(
                &mut inner,
                started + Duration::from_secs(30),
                Duration::from_secs(30),
            );
            assert_eq!(inner.sessions["s1"].state, "closed");
            assert_eq!(inner.sessions["s2"].state, "ready");
        }
        let stop: Value = serde_json::from_str(rx.try_recv().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(stop["type"], "session.stop");
        let error: Value =
            serde_json::from_str(client_rx.try_recv().unwrap().as_str().unwrap()).unwrap();
        assert_eq!(error["code"], "session_start_timeout");
        handle_bridge_event(&app, id, r#"{"type":"session.ready","sessionId":"s1"}"#).await;
        assert_eq!(app.inner.lock().await.sessions["s1"].state, "closed");
    }

    #[test]
    fn heartbeat_and_startup_timeouts_are_bounded() {
        let mut config = base_config();
        config.bridge_heartbeat_timeout_seconds = 0;
        assert!(config.validate().is_err());
        config.bridge_heartbeat_timeout_seconds = 301;
        assert!(config.validate().is_err());
        config.bridge_heartbeat_timeout_seconds = 15;
        config.session_start_timeout_seconds = 0;
        assert!(config.validate().is_err());
        config.session_start_timeout_seconds = config.session_ttl_seconds + 1;
        assert!(config.validate().is_err());
    }

    #[test]
    fn mock_mode_does_not_require_agora_credentials() {
        assert!(base_config().validate().is_ok());
    }

    #[test]
    fn real_mode_requires_agora_credentials() {
        let mut config = base_config();
        config.demo_mode = false;
        assert!(config.validate().is_err());
    }

    #[test]
    fn real_mode_accepts_production_security_config() {
        let mut config = base_config();
        config.demo_mode = false;
        config.public_base_url = "https://asr.example.com".into();
        config.allowed_origin = Some("https://asr.example.com".into());
        config.agora_app_id = "970CA35de60c44645bbae8a215061b33".into();
        config.agora_app_certificate = "5CFd2fd1755d40ecb72977518be15d3b".into();
        config.client_access_token = "client-access-token-at-least-24".into();
        config.octos_service_token = "octos-service-token-at-least-24".into();
        config.bridge_shared_secret = "bridge-secret-at-least-24-characters".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn configured_octos_service_token_must_be_independent_and_long_lived_enough() {
        let mut config = base_config();
        config.octos_service_token = "too-short".into();
        assert!(config.validate().is_err());

        config.octos_service_token = config.bridge_shared_secret.clone();
        assert!(config.validate().is_err());
    }

    #[test]
    fn rtc_provider_must_be_known() {
        let mut config = base_config();
        config.rtc_provider = "twilio".into();
        assert!(config.validate().is_err());
    }

    fn livekit_production_config() -> Config {
        let mut config = base_config();
        config.demo_mode = false;
        config.rtc_provider = "livekit".into();
        config.public_base_url = "https://asr.example.com".into();
        config.allowed_origin = Some("https://asr.example.com".into());
        config.client_access_token = "client-access-token-at-least-24".into();
        config.bridge_shared_secret = "bridge-secret-at-least-24-characters".into();
        config.livekit_url = "wss://rtc.example.com:9443".into();
        config.livekit_api_key = "APIkey12345".into();
        config.livekit_api_secret = "livekit-secret-32-characters!!".into();
        config
    }

    #[test]
    fn livekit_provider_needs_no_agora_credentials() {
        assert!(livekit_production_config().validate().is_ok());
    }

    #[test]
    fn livekit_provider_requires_wss_and_credentials_in_production() {
        let mut config = livekit_production_config();
        config.livekit_url = "ws://rtc.example.com:7880".into();
        assert!(config.validate().is_err());

        let mut config = livekit_production_config();
        config.livekit_api_key = String::new();
        assert!(config.validate().is_err());

        let mut config = livekit_production_config();
        config.livekit_api_secret = "short".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn browser_grant_is_one_time_and_expires() {
        let mut inner = Inner::default();
        let issued = issue_browser_grant(&mut inner, "user-1", "profile-1", 1_000, 60);

        let grant = consume_browser_grant(&mut inner, &issued.token, 1_001)
            .expect("fresh grant should be accepted");
        assert_eq!(grant.subject, "user-1");
        assert_eq!(grant.profile_id, "profile-1");
        assert!(consume_browser_grant(&mut inner, &issued.token, 1_002).is_none());

        let expired = issue_browser_grant(&mut inner, "user-2", "profile-2", 2_000, 10);
        assert!(consume_browser_grant(&mut inner, &expired.token, 12_001).is_none());
    }

    #[test]
    fn rtc_token_must_outlive_session() {
        let mut config = base_config();
        config.rtc_token_ttl_seconds = 900;
        assert!(config.validate().is_err());
    }

    #[test]
    fn session_capacity_must_be_within_bounds() {
        let mut config = base_config();
        config.session_capacity = 0;
        assert!(config.validate().is_err());

        config.session_capacity = 1_001;
        assert!(config.validate().is_err());

        config.session_capacity = 50;
        assert!(config.validate().is_ok());
    }

    #[test]
    fn speak_text_validation() {
        assert!(valid_speak_text("你好，世界"));
        assert!(!valid_speak_text(""));
        assert!(!valid_speak_text("有\u{0007}控制符"));
        assert!(valid_speak_text(&"字".repeat(500)));
        assert!(!valid_speak_text(&"字".repeat(501)));
    }

    #[test]
    fn tts_voice_validation() {
        assert!(valid_tts_voice("vivian"));
        assert!(valid_tts_voice("ryan-2_old"));
        assert!(!valid_tts_voice(""));
        assert!(!valid_tts_voice("not a voice"));
        assert!(!valid_tts_voice(&"a".repeat(65)));
    }

    #[test]
    fn tts_speed_and_instruct_validation() {
        assert!(valid_tts_speed(0.2));
        assert!(valid_tts_speed(1.0));
        assert!(valid_tts_speed(3.0));
        assert!(!valid_tts_speed(0.1));
        assert!(!valid_tts_speed(3.1));
        assert!(!valid_tts_speed(f32::NAN));

        assert!(valid_tts_instruct("温柔地"));
        assert!(valid_tts_instruct(""));
        assert!(!valid_tts_instruct("有\u{0007}控制符"));
        assert!(!valid_tts_instruct(&"字".repeat(201)));
    }

    #[test]
    fn secrets_use_constant_time_comparison() {
        assert!(secret_matches("same-secret", "same-secret"));
        assert!(!secret_matches("same-secret", "other-secret"));
    }

    #[test]
    fn uids_must_be_unique() {
        let mut config = base_config();
        config.bridge_uid = config.client_uid;
        assert!(config.validate().is_err());
    }

    #[test]
    fn result_delivery_requires_correlated_asr_event() {
        let event = json!({
            "type": "asr.final",
            "utteranceId": "session:1",
            "seq": 4,
        });
        assert_eq!(
            result_delivery(&event),
            Some(("asr.final".into(), "session:1".into(), 4))
        );
        assert!(result_delivery(&json!({"type": "trace.update"})).is_none());
    }

    #[test]
    fn client_ack_is_scoped_to_its_session() {
        let ack = json!({
            "type": "client.result_ack",
            "sessionId": "session-a",
            "utteranceId": "session-a:1",
            "eventType": "asr.final",
            "seq": 7,
        });
        assert!(client_delivery_ack(&ack, "session-b").is_none());
        assert_eq!(
            client_delivery_ack(&ack, "session-a"),
            Some(("asr.final".into(), "session-a:1".into(), 7))
        );
    }

    #[test]
    fn nested_metrics_preserve_existing_groups() {
        let mut event = json!({"metrics": {"bridge": {"asrTotalMs": 123.0}}});
        insert_metric(&mut event, "vps", "relayQueueMs", json!(1.5));
        assert_eq!(event["metrics"]["bridge"]["asrTotalMs"], 123.0);
        assert_eq!(event["metrics"]["vps"]["relayQueueMs"], 1.5);
    }
}
