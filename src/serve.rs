use crate::pty::PtyMaster;
use crate::session::{sdp_has_candidate, Session};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path, State},
    http::{header, HeaderValue, StatusCode},
    response::Html,
    routing::{delete, get, post},
    Json, Router,
};
use bytes::Bytes;
use rand::{distributions::Alphanumeric, Rng};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, OwnedSemaphorePermit, Semaphore};
use tokio::task;
use tower_http::cors::CorsLayer;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

const INDEX_HTML: &str = r#"
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
(async function() {
  const status = document.getElementById('status');
  const termElement = document.getElementById('terminal');
  let dc = null;
  let pc = null;
  let sessionId = null;

  function log(msg) { status.textContent = msg; console.log(msg); }
  function logIceCandidateError(event) {
    const code = typeof event.errorCode === 'number' ? event.errorCode : 'unknown';
    const text = event.errorText || 'unknown';
    const isStunLookupError =
      code === 701 &&
      typeof text === 'string' &&
      text.toLowerCase().includes('stun host lookup');
    if (isStunLookupError) {
      console.warn(`ICE candidate warning (can be ignored): code=${code}, text=${text}`);
      return;
    }
    log(`ICE candidate error: code=${code}, text=${text}`);
    console.error('ICE candidate error', event);
  }

  function closeSession() {
    if (!sessionId) return;
    fetch(`/api/sessions/${encodeURIComponent(sessionId)}`, {
      method: 'DELETE',
      keepalive: true
    }).catch(() => {});
    sessionId = null;
  }

  window.addEventListener('pagehide', closeSession);

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

  const createResp = await fetch('/api/sessions', { method: 'POST' });
  if (!createResp.ok) {
    const text = await createResp.text();
    log(text || 'Failed to create session');
    return;
  }
  const session = await createResp.json();
  sessionId = session.session_id;
  log(`Session ${sessionId} created. Establishing connection...`);

  pc = new RTCPeerConnection({
    iceServers: [{ urls: session.stun_server }]
  });
  pc.oniceconnectionstatechange = () => {
    log(`ICE: ${pc.iceConnectionState}`);
  };
  pc.onconnectionstatechange = () => {
    log(`Peer: ${pc.connectionState}`);
  };
  pc.onicecandidateerror = logIceCandidateError;

  pc.ondatachannel = (e) => {
    dc = e.channel;
    dc.binaryType = 'arraybuffer';

    dc.onopen = () => {
      log('Connected');
      fitAddon.fit();
      sendTermSize();
      term.focus();
    };

    dc.onmessage = async (evt) => {
      if (typeof evt.data === 'string') {
        if (evt.data === 'quit') {
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

    dc.onclose = () => { log('Disconnected'); closeSession(); };
    dc.onerror = (err) => { log('Data channel error'); console.error(err); };
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
    headers: { 'Content-Type': 'text/plain' },
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
  const status = document.getElementById('status');
  if (status) {
    status.textContent = 'Connection flow error. Check the console for details.';
  }
});
</script>
</body>
</html>"#;

#[derive(Clone)]
pub struct ServeState {
    cmd: Arc<Vec<String>>,
    non_interactive: bool,
    stun_server: String,
    stun_servers: Arc<Vec<String>>,
    max_sessions: usize,
    sessions: Arc<Mutex<HashMap<String, Arc<BrowserSession>>>>,
    limiter: Arc<Semaphore>,
}

struct BrowserSession {
    pc: Arc<RTCPeerConnection>,
    dc: Arc<StdMutex<Option<Arc<RTCDataChannel>>>>,
    answered: Arc<Mutex<bool>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Serialize)]
struct CreateSessionResponse {
    session_id: String,
    offer_sdp: String,
    stun_server: String,
}

pub async fn start_server(
    port: u16,
    cmd: Vec<String>,
    non_interactive: bool,
    stun_servers: Vec<String>,
    max_sessions: usize,
) -> Result<()> {
    if max_sessions == 0 {
        bail!("max_sessions must be greater than 0");
    }
    if stun_servers.is_empty() {
        bail!("At least one STUN server is required");
    }

    let stun_server = stun_servers[0].clone();

    let state = ServeState {
        cmd: Arc::new(cmd),
        non_interactive,
        stun_server,
        stun_servers: Arc::new(stun_servers),
        max_sessions,
        sessions: Arc::new(Mutex::new(HashMap::new())),
        limiter: Arc::new(Semaphore::new(max_sessions)),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/api/sessions", post(create_session_handler))
        .route("/api/sessions/:session_id/answer", post(answer_handler))
        .route("/api/sessions/:session_id", delete(delete_session_handler))
        .route("/assets/xterm.min.css", get(xterm_css_handler))
        .route("/assets/xterm.min.js", get(xterm_js_handler))
        .route("/assets/addon-fit.min.js", get(addon_fit_js_handler))
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind port {} failed", port))?;

    println!("Web server running at http://localhost:{}/", port);
    println!("Each browser tab creates an independent terminal session.");
    println!("Max concurrent sessions: {} (from ~/.bootty/config.json)\n", max_sessions);

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .context("HTTP server exited with error")?;

    close_all_sessions(&state).await;
    Ok(())
}

async fn create_session_handler(
    State(state): State<ServeState>,
) -> Result<Json<CreateSessionResponse>, (StatusCode, String)> {
    let permit = state
        .limiter
        .clone()
        .try_acquire_owned()
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Too many active sessions (max_sessions={}). Close some tabs and try again.",
                    state.max_sessions
                ),
            )
        })?;

    let session_id = allocate_session_id(&state).await;
    let (browser_session, offer_sdp, err_rx) =
        create_browser_session(state.clone(), session_id.clone(), permit)
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
        stun_server: state.stun_server.clone(),
    }))
}

async fn answer_handler(
    Path(session_id): Path<String>,
    State(state): State<ServeState>,
    body: String,
) -> Result<String, (StatusCode, String)> {
    let session = get_session(&state, &session_id)
        .await
        .ok_or((StatusCode::NOT_FOUND, "Session not found or already closed.".to_string()))?;

    let answer = body.trim().to_string();
    if answer.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "Answer must not be empty.".to_string()));
    }
    if !sdp_has_candidate(&answer) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Answer contains no ICE candidates. Check the browser network and try again.".to_string(),
        ));
    }

    {
        let mut answered = session.answered.lock().await;
        if *answered {
            return Err((
                StatusCode::CONFLICT,
            "Answer already received for this session; duplicate submission is not allowed.".to_string(),
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

    Ok("Answer received.".to_string())
}

async fn delete_session_handler(
    Path(session_id): Path<String>,
    State(state): State<ServeState>,
) -> Result<String, (StatusCode, String)> {
    let removed = remove_session(&state, &session_id, true).await;
    if removed {
        Ok("Session closed.".to_string())
    } else {
        Err((StatusCode::NOT_FOUND, "Session not found or already closed.".to_string()))
    }
}

async fn index_handler(State(state): State<ServeState>) -> Html<String> {
    let _ = state;
    Html(INDEX_HTML.to_string())
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

async fn allocate_session_id(state: &ServeState) -> String {
    loop {
        let id = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(12)
            .map(char::from)
            .collect::<String>();

        let exists = {
            let sessions = state.sessions.lock().await;
            sessions.contains_key(&id)
        };

        if !exists {
            return id;
        }
    }
}

async fn create_browser_session(
    state: ServeState,
    session_id: String,
    permit: OwnedSemaphorePermit,
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

    dc.on_open(Box::new(move || {
        let cmd = cmd.clone();
        let err_tx = err_tx_open.clone();
        let dc = Arc::clone(&dc_open);
        let writer_shared = Arc::clone(&writer_open);
        let pty_master = Arc::clone(&pty_master_open);
        Box::pin(async move {
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
        Box::pin(async move {
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
            answered: Arc::new(Mutex::new(false)),
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
                tracing::warn!("session {session_id} ended with error: {err}");
            }
            Some(None) => {
                tracing::info!("session {session_id} closed");
            }
            None => {
                tracing::info!("session {session_id} monitor channel closed");
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
                tracing::info!("session {session_id} answer timeout, cleaning up");
                let _ = remove_session(&state, &session_id, true).await;
            }
        }
    });
}

async fn get_session(state: &ServeState, session_id: &str) -> Option<Arc<BrowserSession>> {
    let sessions = state.sessions.lock().await;
    sessions.get(session_id).cloned()
}

async fn remove_session(state: &ServeState, session_id: &str, send_quit: bool) -> bool {
    let session = {
        let mut sessions = state.sessions.lock().await;
        sessions.remove(session_id)
    };

    if let Some(session) = session {
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

fn internal_error<E: std::fmt::Display>(err: E) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, format!("Internal error: {err}"))
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
