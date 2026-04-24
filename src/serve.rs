use crate::config::{
    now_unix_secs, remove_serve_runtime, verify_password, write_serve_runtime, ServeMode,
    ServeRuntimeInfo,
};
use crate::pty::PtyMaster;
use crate::session::{sdp_has_candidate, Session};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{ConnectInfo, Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Html,
    routing::{delete, get, post},
    Json, Router,
};
use bytes::Bytes;
use chrono::{DateTime, Local};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

const AUTH_HEADER: &str = "x-bootty-auth";
const ADMIN_TOKEN_HEADER: &str = "x-bootty-admin-token";
const CLOSED_SESSION_HISTORY_LIMIT: usize = 1024;

const INDEX_HTML_TEMPLATE: &str = r#"
<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>bootty</title>
<link rel="stylesheet" href="/assets/xterm.min.css">
<style>
  :root { color-scheme: dark; }
  * { margin: 0; padding: 0; box-sizing: border-box; }
  body { background: #0b0f14; color: #d6deeb; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace; font-size: 14px; }
  #terminal { width: 100vw; height: 100vh; }
  #status { position: fixed; top: 8px; right: 8px; z-index: 10; background: rgba(0, 0, 0, 0.72); padding: 4px 8px; border-radius: 4px; font-size: 12px; color: #93c5fd; }
</style>
</head>
<body>
<div id="status">Creating session...</div>
<div id="terminal"></div>
<script src="/assets/xterm.min.js"></script>
<script src="/assets/addon-fit.min.js"></script>
<script>
let clearPendingConnectTimeout = () => {};

(async function() {
  const AUTH_REQUIRED = __AUTH_REQUIRED__;
  const AUTH_LABEL = "__AUTH_LABEL__";

  const status = document.getElementById('status');
  const termElement = document.getElementById('terminal');
  let dc = null;
  let pc = null;
  let sessionId = null;
  let authSecret = '';
  let hasConnected = false;
  let sessionEnded = false;
  let connectTimeoutId = null;
  let connectTimedOut = false;
  const CONNECT_TIMEOUT_MS = 20000;

  function log(msg) {
    if ((!connectTimedOut || hasConnected) && (!sessionEnded || msg === 'Session ended')) {
      status.textContent = msg;
    }
    console.log(msg);
  }
  function clearConnectTimeout() {
    if (connectTimeoutId !== null) {
      window.clearTimeout(connectTimeoutId);
      connectTimeoutId = null;
    }
  }
  function startConnectTimeout() {
    clearConnectTimeout();
    connectTimedOut = false;
    connectTimeoutId = window.setTimeout(() => {
      connectTimeoutId = null;
      if (hasConnected || !pc || pc.connectionState === 'closed') return;
      connectTimedOut = true;
      status.textContent = 'Connection timed out. Please refresh and retry.';
      console.warn('Connection timed out after sending Answer.');
      pc.close();
      closeSession();
    }, CONNECT_TIMEOUT_MS);
  }
  clearPendingConnectTimeout = clearConnectTimeout;
  function authHeaders() {
    if (!AUTH_REQUIRED || !authSecret) return {};
    return { 'x-bootty-auth': authSecret };
  }

  function logIceCandidateError(event) {
    const code = typeof event.errorCode === 'number' ? event.errorCode : 'unknown';
    const text = event.errorText || 'unknown';
    if (code === 701) {
      console.warn(`ICE candidate warning (can be ignored): code=${code}, text=${text}`);
      return;
    }
    log(`ICE candidate error: code=${code}, text=${text}`);
    console.error('ICE candidate error', event);
  }

  function closeSession() {
    clearConnectTimeout();
    if (!sessionId) return;
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}`, {
      method: 'DELETE',
      headers: authHeaders(),
      keepalive: true
    }).catch(() => {});
    sessionId = null;
  }

  window.addEventListener('pagehide', closeSession);

  if (AUTH_REQUIRED) {
    authSecret = window.prompt(`Authentication required: ${AUTH_LABEL}`) || '';
    if (!authSecret) {
      log('Authentication cancelled.');
      return;
    }
  }

  if (!window.Terminal || !window.FitAddon || !window.FitAddon.FitAddon) {
    log('Failed to load terminal assets. Please refresh and try again.');
    return;
  }

  const term = new window.Terminal({
    cursorBlink: true,
    allowProposedApi: false,
    scrollback: 10000,
    theme: {
      background: '#0b0f14',
      foreground: '#d6deeb',
      cursor: '#93c5fd'
    }
  });
  const fitAddon = new window.FitAddon.FitAddon();
  term.loadAddon(fitAddon);
  term.open(termElement);
  fitAddon.fit();
  term.focus();

  function sendControlMessage(payload) {
    if (!dc || dc.readyState !== 'open') return;
    dc.send(JSON.stringify(payload));
  }

  function sendTermSize() {
    sendControlMessage(['set_size', term.rows, term.cols]);
  }

  const createResp = await fetch('/api/sessions', {
    method: 'POST',
    headers: authHeaders()
  });
  if (!createResp.ok) {
    const text = await createResp.text();
    log(text || 'Failed to create session');
    return;
  }
  const session = await createResp.json();
  sessionId = session.session_id;
  log(`Session ${sessionId} created. Establishing connection...`);

  pc = new RTCPeerConnection({
    iceServers: session.stun_servers.map((url) => ({ urls: url }))
  });
  pc.oniceconnectionstatechange = () => {
    console.log(`ICE: ${pc.iceConnectionState}`);
    if (sessionEnded) {
      return;
    }
    if (connectTimedOut && !hasConnected) {
      return;
    }
    if (pc.iceConnectionState === 'checking') {
      status.textContent = 'Establishing connection...';
      return;
    }
    if (pc.iceConnectionState === 'connected' || pc.iceConnectionState === 'completed') {
      clearConnectTimeout();
      hasConnected = true;
      status.textContent = 'ICE connected. Waiting for session...';
      return;
    }
    if (pc.iceConnectionState === 'disconnected' || pc.iceConnectionState === 'failed') {
      if (hasConnected) {
        status.textContent = 'Connection interrupted. Waiting for recovery...';
      }
      console.warn(`ICE state during negotiation: ${pc.iceConnectionState}`);
      return;
    }
    status.textContent = `ICE: ${pc.iceConnectionState}`;
  };
  pc.onconnectionstatechange = () => {
    console.log(`Peer: ${pc.connectionState}`);
    if (sessionEnded) {
      return;
    }
    if (connectTimedOut && !hasConnected) {
      return;
    }
    if (pc.connectionState === 'connecting') {
      status.textContent = 'Establishing connection...';
      return;
    }
    if (pc.connectionState === 'connected') {
      clearConnectTimeout();
      hasConnected = true;
      status.textContent = 'Connected';
      return;
    }
    if (pc.connectionState === 'disconnected' || pc.connectionState === 'failed') {
      if (hasConnected) {
        status.textContent = pc.connectionState === 'failed'
          ? 'Connection lost. Please refresh and retry.'
          : 'Connection interrupted. Waiting for recovery...';
      }
      console.warn(`Peer state during negotiation: ${pc.connectionState}`);
      return;
    }
    if (pc.connectionState === 'closed') {
      clearConnectTimeout();
      status.textContent = 'Disconnected';
      return;
    }
    status.textContent = `Peer: ${pc.connectionState}`;
  };
  pc.onicecandidateerror = logIceCandidateError;

  pc.ondatachannel = (e) => {
    dc = e.channel;
    dc.binaryType = 'arraybuffer';

    dc.onopen = () => {
      clearConnectTimeout();
      hasConnected = true;
      log('Connected');
      fitAddon.fit();
      sendTermSize();
      term.focus();
    };

    dc.onmessage = async (evt) => {
      if (typeof evt.data === 'string') {
        if (evt.data === 'quit') {
          sessionEnded = true;
          clearConnectTimeout();
          log('Session ended');
          dc.close();
          closeSession();
          return;
        }
        return;
      }
      if (evt.data instanceof ArrayBuffer) {
        term.write(new Uint8Array(evt.data));
        return;
      }
      if (evt.data instanceof Blob) {
        const buffer = await evt.data.arrayBuffer();
        term.write(new Uint8Array(buffer));
      }
    };

    dc.onclose = () => {
      clearConnectTimeout();
      if (!sessionEnded) {
        log('Disconnected');
      }
      closeSession();
    };
    dc.onerror = (err) => {
      clearConnectTimeout();
      if (!sessionEnded) {
        log('Data channel error');
      }
      console.error(err);
    };
  };

  await pc.setRemoteDescription({ type: 'offer', sdp: session.offer_sdp });

  const answer = await pc.createAnswer();
  await pc.setLocalDescription(answer);

  await new Promise(resolve => {
    if (pc.iceGatheringState === 'complete') { resolve(); return; }
    pc.onicegatheringstatechange = () => {
      if (pc.iceGatheringState === 'complete') resolve();
    };
  });

  const localSdp = pc.localDescription?.sdp || '';
  if (!localSdp.includes('a=candidate:')) {
    log('No ICE candidates gathered. Check network/firewall and try again.');
    closeSession();
    return;
  }

  log('Sending Answer...');
  const answerResp = await fetch(`/api/sessions/${encodeURIComponent(sessionId)}/answer`, {
    method: 'POST',
    headers: {
      'Content-Type': 'text/plain',
      ...authHeaders()
    },
    body: localSdp
  });
  if (!answerResp.ok) {
    const text = await answerResp.text();
    log(text || 'Failed to send Answer');
    pc.close();
    closeSession();
    return;
  }
  log('Answer sent. Waiting for connection...');
  startConnectTimeout();

  term.onData((data) => {
    sendControlMessage(['stdin', data]);
  });
  term.onResize(({ rows, cols }) => {
    sendControlMessage(['set_size', rows, cols]);
  });
  window.addEventListener('resize', () => {
    fitAddon.fit();
  });
  window.addEventListener('click', () => term.focus());
})().catch((err) => {
  console.error('Connection flow error', err);
  clearPendingConnectTimeout();
  const status = document.getElementById('status');
  if (status) {
    status.textContent = 'Connection flow error. Check the console for details.';
  }
});
</script>
</body>
</html>"#;

#[derive(Debug, Clone)]
pub enum ServeAuthRuntime {
    None,
    Pin(String),
    PasswordHash(String),
}

impl ServeAuthRuntime {
    fn is_required(&self) -> bool {
        !matches!(self, Self::None)
    }

    fn label(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pin(_) => "6-digit PIN",
            Self::PasswordHash(_) => "password",
        }
    }

    fn mode_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Pin(_) => "pin",
            Self::PasswordHash(_) => "password",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServeLaunchOptions {
    pub host: String,
    pub port: u16,
    pub cmd: Vec<String>,
    pub non_interactive: bool,
    pub stun_servers: Vec<String>,
    pub max_sessions: usize,
    pub mode: ServeMode,
    pub auth: ServeAuthRuntime,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionLifecycleState {
    Pending,
    Connected,
    Closed,
}

impl SessionLifecycleState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Connected => "connected",
            Self::Closed => "closed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminSessionItem {
    pub session_id: String,
    pub state: SessionLifecycleState,
    pub client_addr: Option<String>,
    pub created_at: String,
    pub last_active_at: String,
    pub auth_mode: String,
    pub cmd_preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminCmdRequest {
    pub cmd: String,
    pub append_enter: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdminCmdResponse {
    pub session_id: String,
    pub bytes_sent: usize,
}

#[derive(Clone)]
pub struct ServeState {
    cmd: Arc<Vec<String>>,
    cmd_preview: Arc<String>,
    non_interactive: bool,
    stun_servers: Arc<Vec<String>>,
    max_sessions: usize,
    mode: ServeMode,
    auth: ServeAuthRuntime,
    sessions: Arc<Mutex<HashMap<String, Arc<BrowserSession>>>>,
    closed_sessions: Arc<Mutex<HashMap<String, ClosedSessionInfo>>>,
    closed_session_order: Arc<Mutex<VecDeque<String>>>,
    auth_mode: Arc<String>,
    limiter: Arc<Semaphore>,
}

#[derive(Clone)]
struct AdminState {
    serve_state: ServeState,
    admin_token: String,
}

struct BrowserSession {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<StdMutex<Option<Arc<RTCDataChannel>>>>,
    writer: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
    answered: Arc<Mutex<bool>>,
    lifecycle: Arc<Mutex<SessionLifecycleState>>,
    client_addr: Option<String>,
    created_at_unix: u64,
    last_active_at_unix: Arc<Mutex<u64>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Debug, Clone)]
struct ClosedSessionInfo {
    client_addr: Option<String>,
    created_at_unix: u64,
    last_active_at_unix: u64,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
    offer_sdp: String,
    stun_servers: Vec<String>,
}

#[derive(Deserialize)]
struct AdminListQuery {
    state: Option<String>,
}

pub async fn start_server(options: ServeLaunchOptions) -> Result<()> {
    validate_launch_options(&options)?;

    if options.stun_servers.is_empty() {
        bail!("At least one STUN server is required");
    }

    let cmd_preview = if options.cmd.is_empty() {
        "bash -l".to_string()
    } else {
        options.cmd.join(" ")
    };

    let state = ServeState {
        cmd: Arc::new(options.cmd.clone()),
        cmd_preview: Arc::new(cmd_preview),
        non_interactive: options.non_interactive,
        stun_servers: Arc::new(options.stun_servers.clone()),
        max_sessions: options.max_sessions,
        mode: options.mode.clone(),
        auth: options.auth.clone(),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        closed_sessions: Arc::new(Mutex::new(HashMap::new())),
        closed_session_order: Arc::new(Mutex::new(VecDeque::new())),
        auth_mode: Arc::new(options.auth.mode_name().to_string()),
        limiter: Arc::new(Semaphore::new(options.max_sessions)),
    };

    let public_app = Router::new()
        .route("/", get(index_handler))
        .route("/api/sessions", post(create_session_handler))
        .route("/api/sessions/:session_id/answer", post(answer_handler))
        .route("/api/sessions/:session_id", delete(delete_session_handler))
        .route("/assets/xterm.min.css", get(xterm_css_handler))
        .route("/assets/xterm.min.js", get(xterm_js_handler))
        .route("/assets/addon-fit.min.js", get(addon_fit_js_handler))
        .with_state(state.clone());

    let public_listener = tokio::net::TcpListener::bind(format!("{}:{}", options.host, options.port))
        .await
        .with_context(|| format!("Failed to bind public service address: {}:{}", options.host, options.port))?;

    let admin_token = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(32)
        .map(char::from)
        .collect::<String>();
    let admin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("Failed to bind local admin interface")?;
    let admin_addr = admin_listener
        .local_addr()
        .context("Failed to read local admin interface address")?;

    let runtime_info = ServeRuntimeInfo {
        pid: std::process::id(),
        admin_addr: admin_addr.to_string(),
        admin_token: admin_token.clone(),
        started_at_unix: now_unix_secs(),
    };
    write_serve_runtime(&runtime_info)?;

    let admin_state = AdminState {
        serve_state: state.clone(),
        admin_token: admin_token.clone(),
    };

    let admin_app = Router::new()
        .route("/admin/sessions", get(admin_list_sessions_handler))
        .route("/admin/sessions/:session_id/cmd", post(admin_send_cmd_handler))
        .with_state(admin_state);

    let admin_task = tokio::spawn(async move {
        if let Err(err) = axum::serve(admin_listener, admin_app).await {
            tracing::error!("Admin interface exited unexpectedly: {err}");
        }
    });

    println!("Service started: http://{}:{}/", options.host, options.port);
    println!("Mode: {:?}", state.mode);
    println!("Session command: {}", state.cmd_preview.as_ref());
    println!("Max sessions: {}", state.max_sessions);
    println!("Local admin interface: {}", admin_addr);
    if let ServeAuthRuntime::Pin(pin) = &state.auth {
        println!("PIN: {pin}");
    }

    let serve_result = axum::serve(
        public_listener,
        public_app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    .context("Public service exited unexpectedly");

    close_all_sessions(&state).await;
    let _ = remove_serve_runtime();
    admin_task.abort();

    serve_result
}

fn validate_launch_options(options: &ServeLaunchOptions) -> Result<()> {
    if options.max_sessions == 0 {
        bail!("max_sessions must be greater than 0");
    }

    match options.mode {
        ServeMode::Local => {
            if options.host != "127.0.0.1" && options.host != "localhost" {
                bail!("serve mode=local requires --host to be 127.0.0.1 or localhost");
            }
            if !matches!(options.auth, ServeAuthRuntime::None) {
                bail!("serve mode=local does not allow enabling auth");
            }
        }
        ServeMode::LanOpen => {
            if !matches!(options.auth, ServeAuthRuntime::None) {
                bail!("serve mode=lan-open does not allow enabling auth");
            }
        }
        ServeMode::LanAuth => {
            if matches!(options.auth, ServeAuthRuntime::None) {
                bail!("serve mode=lan-auth requires an auth method");
            }
        }
    }

    Ok(())
}

fn ensure_public_auth(state: &ServeState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    match &state.auth {
        ServeAuthRuntime::None => Ok(()),
        ServeAuthRuntime::Pin(pin) => {
            let provided = read_header_value(headers, AUTH_HEADER)
                .ok_or((StatusCode::UNAUTHORIZED, "Missing auth header".to_string()))?;
            if provided == *pin {
                Ok(())
            } else {
                Err((StatusCode::UNAUTHORIZED, "Invalid PIN".to_string()))
            }
        }
        ServeAuthRuntime::PasswordHash(encoded) => {
            let provided = read_header_value(headers, AUTH_HEADER)
                .ok_or((StatusCode::UNAUTHORIZED, "Missing auth header".to_string()))?;
            let matched = verify_password(&provided, encoded).map_err(internal_error)?;
            if matched {
                Ok(())
            } else {
                Err((StatusCode::UNAUTHORIZED, "Invalid password".to_string()))
            }
        }
    }
}

fn ensure_admin_token(state: &AdminState, headers: &HeaderMap) -> Result<(), (StatusCode, String)> {
    let provided = read_header_value(headers, ADMIN_TOKEN_HEADER)
        .ok_or((StatusCode::UNAUTHORIZED, "Missing admin token".to_string()))?;
    if provided == state.admin_token {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "Invalid admin token".to_string()))
    }
}

fn read_header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn create_session_handler(
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<ServeState>,
    headers: HeaderMap,
) -> Result<Json<CreateSessionResponse>, (StatusCode, String)> {
    ensure_public_auth(&state, &headers)?;

    let permit = state
        .limiter
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                format!("Too many active sessions (max_sessions={}), please close some sessions and retry", state.max_sessions),
            )
        })?;

    let session_id = allocate_session_id(&state).await;
    let (browser_session, offer_sdp, err_rx) = create_browser_session(
        state.clone(),
        session_id.clone(),
        permit,
        Some(addr.ip().to_string()),
    )
    .await
    .map_err(internal_error)?;

    {
        let mut sessions = state.sessions.lock().await;
        sessions.insert(session_id.clone(), browser_session);
    }

    spawn_session_monitor(state.clone(), session_id.clone(), err_rx);
    spawn_answer_timeout(state.clone(), session_id.clone());

    Ok(Json(CreateSessionResponse {
        session_id,
        offer_sdp,
        stun_servers: state.stun_servers.as_ref().clone(),
    }))
}

async fn answer_handler(
    Path(session_id): Path<String>,
    State(state): State<ServeState>,
    headers: HeaderMap,
    body: String,
) -> Result<String, (StatusCode, String)> {
    ensure_public_auth(&state, &headers)?;

    let session = get_session(&state, &session_id)
        .await
        .ok_or((StatusCode::NOT_FOUND, "Session does not exist or has been closed".to_string()))?;

    let answer = body.trim().to_string();
    if answer.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Answer cannot be empty".to_string()));
    }
    if !sdp_has_candidate(&answer) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Missing ICE candidate in Answer, please check browser network and retry".to_string(),
        ));
    }

    {
        let mut answered = session.answered.lock().await;
        if *answered {
            return Err((
                StatusCode::CONFLICT,
                "Answer has already been submitted for this session".to_string(),
            ));
        }
        *answered = true;
    }

    let desc = RTCSessionDescription::answer(answer).map_err(internal_error)?;
    if let Err(e) = session.pc.set_remote_description(desc).await {
        let mut answered = session.answered.lock().await;
        *answered = false;
        return Err(internal_error(e));
    }

    update_last_active(&session.last_active_at_unix).await;
    Ok("Answer received".to_string())
}

async fn delete_session_handler(
    Path(session_id): Path<String>,
    State(state): State<ServeState>,
    headers: HeaderMap,
) -> Result<String, (StatusCode, String)> {
    ensure_public_auth(&state, &headers)?;

    let removed = remove_session(&state, &session_id, true).await;
    if removed {
        Ok("Session closed".to_string())
    } else {
        Err((StatusCode::NOT_FOUND, "Session does not exist or has been closed".to_string()))
    }
}

async fn index_handler(State(state): State<ServeState>) -> Html<String> {
    let rendered = INDEX_HTML_TEMPLATE
        .replace(
            "__AUTH_REQUIRED__",
            if state.auth.is_required() { "true" } else { "false" },
        )
        .replace("__AUTH_LABEL__", state.auth.label());
    Html(rendered)
}

async fn xterm_css_handler() -> ([(header::HeaderName, HeaderValue); 1], &'static str) {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/css; charset=utf-8"),
        )],
        include_str!("../assets/xterm/xterm.min.css"),
    )
}

async fn xterm_js_handler() -> ([(header::HeaderName, HeaderValue); 1], &'static str) {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript; charset=utf-8"),
        )],
        include_str!("../assets/xterm/xterm.min.js"),
    )
}

async fn addon_fit_js_handler() -> ([(header::HeaderName, HeaderValue); 1], &'static str) {
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/javascript; charset=utf-8"),
        )],
        include_str!("../assets/xterm/addon-fit.min.js"),
    )
}

async fn admin_list_sessions_handler(
    Query(query): Query<AdminListQuery>,
    State(state): State<AdminState>,
    headers: HeaderMap,
) -> Result<Json<Vec<AdminSessionItem>>, (StatusCode, String)> {
    ensure_admin_token(&state, &headers)?;

    let items = list_sessions(&state.serve_state).await;
    if let Some(filter_state) = query.state.as_deref() {
        let normalized = filter_state.trim().to_lowercase();
        if normalized != "pending" && normalized != "connected" && normalized != "closed" {
            return Err((StatusCode::BAD_REQUEST, "state only supports pending|connected|closed".to_string()));
        }
        let filtered = items
            .into_iter()
            .filter(|item| item.state.as_str() == normalized)
            .collect::<Vec<_>>();
        return Ok(Json(filtered));
    }

    Ok(Json(items))
}

async fn admin_send_cmd_handler(
    Path(session_id): Path<String>,
    State(state): State<AdminState>,
    headers: HeaderMap,
    Json(payload): Json<AdminCmdRequest>,
) -> Result<Json<AdminCmdResponse>, (StatusCode, String)> {
    ensure_admin_token(&state, &headers)?;

    let Some(session) = get_session(&state.serve_state, &session_id).await else {
        if is_closed_session(&state.serve_state, &session_id).await {
            return Err((StatusCode::CONFLICT, "Session is closed, cannot inject command".to_string()));
        }
        return Err((StatusCode::NOT_FOUND, "Session does not exist".to_string()));
    };

    let lifecycle = *session.lifecycle.lock().await;
    if lifecycle != SessionLifecycleState::Connected {
        return Err((
            StatusCode::CONFLICT,
            "Session is not fully connected, cannot inject command".to_string(),
        ));
    }

    if payload.cmd.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Command cannot be empty".to_string()));
    }

    let mut bytes = payload.cmd.as_bytes().to_vec();
    if payload.append_enter {
        bytes.push(b'\n');
    }

    {
        let mut guard = session.writer.lock().unwrap();
        let writer = guard
            .as_mut()
            .ok_or((StatusCode::CONFLICT, "Session stdin is not ready yet".to_string()))?;
        writer
            .write_all(&bytes)
            .map_err(|e| internal_error(format!("Failed to write command: {e}")))?;
        writer
            .flush()
            .map_err(|e| internal_error(format!("Failed to flush command: {e}")))?;
    }

    update_last_active(&session.last_active_at_unix).await;

    Ok(Json(AdminCmdResponse {
        session_id,
        bytes_sent: bytes.len(),
    }))
}

async fn list_sessions(state: &ServeState) -> Vec<AdminSessionItem> {
    let sessions = state.sessions.lock().await;
    let closed = state.closed_sessions.lock().await;
    let mut items = Vec::with_capacity(sessions.len());
    let mut active_ids = HashSet::with_capacity(sessions.len());

    for (session_id, session) in sessions.iter() {
        let state_value = *session.lifecycle.lock().await;
        let last_active = *session.last_active_at_unix.lock().await;
        active_ids.insert(session_id.clone());
        items.push((
            session.created_at_unix,
            AdminSessionItem {
                session_id: session_id.clone(),
                state: state_value,
                client_addr: session.client_addr.clone(),
                created_at: format_unix_secs(session.created_at_unix),
                last_active_at: format_unix_secs(last_active),
                auth_mode: state.auth_mode.as_ref().clone(),
                cmd_preview: state.cmd_preview.as_ref().clone(),
            },
        ));
    }

    for (session_id, closed_item) in closed.iter() {
        if active_ids.contains(session_id) {
            continue;
        }
        items.push((
            closed_item.created_at_unix,
            AdminSessionItem {
                session_id: session_id.clone(),
                state: SessionLifecycleState::Closed,
                client_addr: closed_item.client_addr.clone(),
                created_at: format_unix_secs(closed_item.created_at_unix),
                last_active_at: format_unix_secs(closed_item.last_active_at_unix),
                auth_mode: state.auth_mode.as_ref().clone(),
                cmd_preview: state.cmd_preview.as_ref().clone(),
            },
        ));
    }

    items.sort_by(|a, b| a.0.cmp(&b.0));
    items.into_iter().map(|(_, item)| item).collect()
}

fn format_unix_secs(unix_secs: u64) -> String {
    if let Some(dt) = DateTime::from_timestamp(unix_secs as i64, 0) {
        return dt
            .with_timezone(&Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
    }
    unix_secs.to_string()
}

async fn allocate_session_id(state: &ServeState) -> String {
    loop {
        let id = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(12)
            .map(char::from)
            .collect::<String>();

        let exists_in_active = {
            let sessions = state.sessions.lock().await;
            sessions.contains_key(&id)
        };
        let exists_in_closed = {
            let sessions = state.closed_sessions.lock().await;
            sessions.contains_key(&id)
        };

        if !exists_in_active && !exists_in_closed {
            return id;
        }
    }
}

async fn create_browser_session(
    state: ServeState,
    session_id: String,
    permit: OwnedSemaphorePermit,
    client_addr: Option<String>,
) -> Result<(
    Arc<BrowserSession>,
    String,
    mpsc::Receiver<Option<anyhow::Error>>,
)> {
    let Session { pc, err_tx, err_rx, .. } =
        Session::new(state.stun_servers.as_ref().clone(), true).await?;

    let writer_shared: Arc<StdMutex<Option<Box<dyn Write + Send>>>> =
        Arc::new(StdMutex::new(None));
    let pty_master_shared: Arc<Mutex<Option<PtyMaster>>> = Arc::new(Mutex::new(None));
    let dc_holder: Arc<StdMutex<Option<Arc<RTCDataChannel>>>> = Arc::new(StdMutex::new(None));
    let lifecycle = Arc::new(Mutex::new(SessionLifecycleState::Pending));
    let last_active = Arc::new(Mutex::new(now_unix_secs()));

    let err_tx_state = err_tx.clone();
    let session_id_state = session_id.clone();
    pc.on_peer_connection_state_change(Box::new(move |peer_state| {
        let err_tx = err_tx_state.clone();
        let session_id = session_id_state.clone();
        Box::pin(async move {
            tracing::info!("session {session_id} peer state: {peer_state}");
            if matches!(
                peer_state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                let _ = err_tx
                    .send(Some(anyhow!(
                        "session {session_id} peer state changed to {peer_state}"
                    )))
                    .await;
            }
        })
    }));

    let dc = pc
        .create_data_channel("offerer-channel", None)
        .await
        .context("create data channel failed")?;

    {
        let mut guard = dc_holder.lock().unwrap();
        *guard = Some(Arc::clone(&dc));
    }

    let cmd = state.cmd.as_ref().clone();
    let non_interactive = state.non_interactive;

    let dc_open = Arc::clone(&dc);
    let dc_msg = Arc::clone(&dc);
    let err_tx_open = err_tx.clone();
    let err_tx_msg = err_tx.clone();
    let err_tx_close = err_tx.clone();
    let writer_open = Arc::clone(&writer_shared);
    let writer_msg = Arc::clone(&writer_shared);
    let pty_master_open = Arc::clone(&pty_master_shared);
    let pty_master_msg = Arc::clone(&pty_master_shared);
    let lifecycle_open = Arc::clone(&lifecycle);
    let last_active_open = Arc::clone(&last_active);
    let last_active_msg = Arc::clone(&last_active);

    dc.on_open(Box::new(move || {
        let cmd = cmd.clone();
        let err_tx = err_tx_open.clone();
        let dc = Arc::clone(&dc_open);
        let writer_shared = Arc::clone(&writer_open);
        let pty_master = Arc::clone(&pty_master_open);
        let lifecycle = Arc::clone(&lifecycle_open);
        let last_active = Arc::clone(&last_active_open);

        Box::pin(async move {
            {
                let mut lifecycle_guard = lifecycle.lock().await;
                *lifecycle_guard = SessionLifecycleState::Connected;
            }
            update_last_active(&last_active).await;

            if let Err(e) = data_channel_on_open(
                dc,
                cmd,
                err_tx,
                non_interactive,
                writer_shared,
                pty_master,
            )
            .await
            {
                tracing::error!("data channel on_open error: {e}");
            }
        })
    }));

    dc_msg.on_message(Box::new(move |msg: DataChannelMessage| {
        let writer_shared = Arc::clone(&writer_msg);
        let pty_master = Arc::clone(&pty_master_msg);
        let err_tx = err_tx_msg.clone();
        let last_active = Arc::clone(&last_active_msg);

        Box::pin(async move {
            update_last_active(&last_active).await;
            if let Err(e) = handle_host_message(msg, writer_shared, pty_master, err_tx).await {
                tracing::error!("message handle error: {e}");
            }
        })
    }));

    dc.on_close(Box::new(move || {
        let err_tx = err_tx_close.clone();
        Box::pin(async move {
            let _ = err_tx.send(None).await;
        })
    }));

    let offer = pc.create_offer(None).await.context("create offer failed")?;
    let mut gather_complete = pc.gathering_complete_promise().await;
    pc.set_local_description(offer)
        .await
        .context("set local SDP failed")?;
    let _ = gather_complete.recv().await;

    let offer_sdp = pc
        .local_description()
        .await
        .context("missing local SDP")?
        .sdp;

    if !sdp_has_candidate(&offer_sdp) {
        bail!("No ICE candidates were gathered. Check network/firewall and try again.");
    }

    Ok((
        Arc::new(BrowserSession {
            pc,
            dc: dc_holder,
            writer: writer_shared,
            answered: Arc::new(Mutex::new(false)),
            lifecycle,
            client_addr,
            created_at_unix: now_unix_secs(),
            last_active_at_unix: last_active,
            _permit: permit,
        }),
        offer_sdp,
        err_rx,
    ))
}

fn spawn_session_monitor(
    state: ServeState,
    session_id: String,
    mut err_rx: mpsc::Receiver<Option<anyhow::Error>>,
) {
    tokio::spawn(async move {
        match err_rx.recv().await {
            Some(Some(err)) => {
                tracing::warn!("Session {session_id} ended abnormally: {err}");
            }
            Some(None) => {
                tracing::info!("Session {session_id} closed");
            }
            None => {
                tracing::info!("Session {session_id} monitor channel closed");
            }
        }
        let _ = remove_session(&state, &session_id, false).await;
    });
}

fn spawn_answer_timeout(state: ServeState, session_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(90)).await;
        let session = get_session(&state, &session_id).await;
        if let Some(session) = session {
            let answered = *session.answered.lock().await;
            if !answered {
                tracing::info!("Session {session_id} Answer timed out, starting cleanup");
                let _ = remove_session(&state, &session_id, true).await;
            }
        }
    });
}

async fn get_session(state: &ServeState, session_id: &str) -> Option<Arc<BrowserSession>> {
    let sessions = state.sessions.lock().await;
    sessions.get(session_id).cloned()
}

async fn is_closed_session(state: &ServeState, session_id: &str) -> bool {
    let sessions = state.closed_sessions.lock().await;
    sessions.contains_key(session_id)
}

async fn remove_session(state: &ServeState, session_id: &str, send_quit: bool) -> bool {
    let session = {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(session_id)
    };

    if let Some(session) = session {
        {
            let mut lifecycle = session.lifecycle.lock().await;
            *lifecycle = SessionLifecycleState::Closed;
        }

        if send_quit {
            let dc_opt = {
                let guard = session.dc.lock().unwrap();
                guard.as_ref().cloned()
            };
            if let Some(dc) = dc_opt {
                let _ = dc.send_text("quit").await;
            }
        }
        let _ = session.pc.close().await;

        let last_active = *session.last_active_at_unix.lock().await;
        let mut closed_sessions = state.closed_sessions.lock().await;
        let mut closed_order = state.closed_session_order.lock().await;
        closed_sessions.insert(
            session_id.to_string(),
            ClosedSessionInfo {
                client_addr: session.client_addr.clone(),
                created_at_unix: session.created_at_unix,
                last_active_at_unix: last_active,
            },
        );
        closed_order.push_back(session_id.to_string());

        while closed_order.len() > CLOSED_SESSION_HISTORY_LIMIT {
            if let Some(evicted_id) = closed_order.pop_front() {
                closed_sessions.remove(&evicted_id);
            }
        }
        true
    } else {
        false
    }
}

async fn close_all_sessions(state: &ServeState) {
    let ids = {
        let sessions = state.sessions.lock().await;
        sessions.keys().cloned().collect::<Vec<_>>()
    };
    for session_id in ids {
        let _ = remove_session(state, &session_id, true).await;
    }
}

async fn update_last_active(last_active: &Arc<Mutex<u64>>) {
    let mut guard = last_active.lock().await;
    *guard = now_unix_secs();
}

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Internal error: {err}"),
    )
}

async fn data_channel_on_open(
    dc: Arc<RTCDataChannel>,
    cmd: Vec<String>,
    err_tx: mpsc::Sender<Option<anyhow::Error>>,
    non_interactive: bool,
    writer_shared: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
    pty_master_shared: Arc<Mutex<Option<PtyMaster>>>,
) -> Result<()> {
    let (pty_master, mut reader, writer) = PtyMaster::new(&cmd)?;
    *pty_master_shared.lock().await = Some(pty_master);

    {
        let mut guard = writer_shared.lock().unwrap();
        *guard = Some(writer);
    }

    if !non_interactive {
        task::spawn_blocking(move || {
            let mut buf = [0u8; 1024];
            loop {
                match io::stdin().read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut guard = writer_shared.lock().unwrap();
                        if let Some(ref mut w) = *guard {
                            if w.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = err_tx.send(Some(anyhow!("pty eof"))).await;
                break;
            }
            Ok(n) => {
                if let Err(e) = dc.send(&Bytes::copy_from_slice(&buf[..n])).await {
                    let _ = err_tx.send(Some(anyhow!("dc send error: {e}"))).await;
                    break;
                }
            }
            Err(e) => {
                let _ = err_tx.send(Some(anyhow!("pty read error: {e}"))).await;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_host_message(
    msg: DataChannelMessage,
    writer_shared: Arc<StdMutex<Option<Box<dyn Write + Send>>>>,
    pty_master_shared: Arc<Mutex<Option<PtyMaster>>>,
    err_tx: mpsc::Sender<Option<anyhow::Error>>,
) -> Result<()> {
    if msg.is_string {
        let data = String::from_utf8_lossy(&msg.data);
        if data == "quit" {
            let _ = err_tx.send(None).await;
            return Ok(());
        }

        if data.starts_with("[\"") {
            let parsed: serde_json::Value = serde_json::from_str(&data)?;
            if let Some(arr) = parsed.as_array() {
                if let Some(cmd) = arr.first().and_then(|v| v.as_str()) {
                    match cmd {
                        "stdin" => {
                            if let Some(v) = arr.get(1).and_then(|v| v.as_str()) {
                                let mut guard = writer_shared.lock().unwrap();
                                if let Some(ref mut w) = *guard {
                                    w.write_all(v.as_bytes())?;
                                }
                            }
                        }
                        "set_size" => {
                            let rows = arr.get(1).and_then(|v| v.as_u64()).unwrap_or(24) as u16;
                            let cols = arr.get(2).and_then(|v| v.as_u64()).unwrap_or(80) as u16;
                            let guard = pty_master_shared.lock().await;
                            if let Some(ref pm) = *guard {
                                pm.resize(rows, cols)?;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    } else {
        let mut guard = writer_shared.lock().unwrap();
        if let Some(ref mut w) = *guard {
            w.write_all(&msg.data)?;
        }
    }

    Ok(())
}
