use crate::pty::PtyMaster;
use crate::sdp::SessionDescription;
use crate::session::{sdp_has_candidate, Session};
use crate::serve::start_server;
use crate::ten_kb_site::{create_10kb_file, poll_for_response, rand_seq};
use crate::terminal::{is_stdin_terminal, TerminalState};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::io::{self, Read, Write};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::task;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub struct HostSession {
    session: Session,
    cmd: Vec<String>,
    non_interactive: bool,
    one_way: bool,
    serve: Option<u16>,
    max_sessions: usize,
    dc: Arc<std::sync::Mutex<Option<Arc<RTCDataChannel>>>>,
}

impl HostSession {
    pub async fn new(
        cmd: Vec<String>,
        non_interactive: bool,
        one_way: bool,
        stun_servers: Vec<String>,
        serve: Option<u16>,
        max_sessions: usize,
    ) -> Result<Self> {
        let session = Session::new(stun_servers, true).await?;
        Ok(HostSession {
            session,
            cmd,
            non_interactive,
            one_way,
            serve,
            max_sessions,
            dc: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        if let Some(port) = self.serve {
            println!("Starting multi-session web server...");
            return start_server(
                port,
                self.cmd.clone(),
                self.non_interactive,
                self.session.stun_servers.clone(),
                self.max_sessions,
            )
            .await;
        }

        let err_tx_state = self.session.err_tx.clone();
        self.session.pc.on_peer_connection_state_change(Box::new(
            move |state: RTCPeerConnectionState| {
                let err_tx = err_tx_state.clone();
                Box::pin(async move {
                    tracing::info!("PeerConnection state changed: {state}");
                    if state == RTCPeerConnectionState::Failed {
                        let _ = err_tx
                            .send(Some(anyhow::anyhow!(
                                "PeerConnection entered the failed state (usually DTLS/SCTP handshake failure)."
                            )))
                            .await;
                    }
                })
            },
        ));

        println!("Initializing connection...\n");
        if self.one_way {
            println!(
                "Warning: one-way connections rely on a third-party relay service.\n"
            );
        }

        let offer = self.create_offer().await?;

        if !self.one_way {
            println!("Connection data is ready:\n");
            println!("{}\n", offer.encode()?);
            println!(
                "Share the connection string with the peer. They should run:\n  bootty \"<CONNECTION_STRING>\"\n"
            );
        }

        let mut answer: SessionDescription;
        if !self.one_way {
            println!("Paste the Answer below and press Enter:");
            let mut input = String::new();
            io::stdin().read_line(&mut input)?;
            let input = input.trim();
            answer = SessionDescription::decode(input)?;
            println!("Answer received. Establishing connection...");
        } else {
            let encoded_offer = offer.encode()?;
            create_10kb_file(&offer.ten_kb_site_loc, &encoded_offer).await?;
            println!(
                "Connection is ready. Share this link:\nhttps://www.10kb.site/{}\n",
                offer.ten_kb_site_loc
            );
            let body = poll_for_response(&offer.ten_kb_site_loc).await?;
            answer = SessionDescription::decode(&body)?;
            answer.key = offer.key;
            answer.nonce = offer.nonce;
            answer.decrypt()?;
        }

        self.set_remote_description_and_wait(answer.sdp).await
    }

    async fn create_offer(&self) -> Result<SessionDescription> {
        let pc = Arc::clone(&self.session.pc);
        let cmd = self.cmd.clone();
        let non_interactive = self.non_interactive;
        let err_tx = self.session.err_tx.clone();

        let writer_shared: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>> =
            Arc::new(std::sync::Mutex::new(None));
        let pty_master_shared: Arc<Mutex<Option<PtyMaster>>> = Arc::new(Mutex::new(None));

        let dc = pc
            .create_data_channel("offerer-channel", None)
            .await
            .context("create data channel failed")?;

        {
            let mut guard = self.dc.lock().unwrap();
            *guard = Some(Arc::clone(&dc));
        }

        let dc_open = Arc::clone(&dc);
        let dc_msg = Arc::clone(&dc);
        let err_tx_open = err_tx.clone();
        let err_tx_msg = err_tx.clone();
        let writer_open = Arc::clone(&writer_shared);
        let writer_msg = Arc::clone(&writer_shared);
        let pty_master_open = Arc::clone(&pty_master_shared);
        let pty_master_msg = Arc::clone(&pty_master_shared);

        dc.on_open(Box::new(move || {
            let cmd = cmd.clone();
            let err_tx = err_tx_open.clone();
            let non_interactive = non_interactive;
            let dc = Arc::clone(&dc_open);
            let writer_shared = Arc::clone(&writer_open);
            let pty_master = Arc::clone(&pty_master_open);

            Box::pin(async move {
                if let Err(e) = data_channel_on_open(
                    dc, cmd, err_tx, non_interactive, writer_shared, pty_master,
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

        let offer = pc
            .create_offer(None)
            .await
            .context("create offer failed")?;

        let mut gather_complete = pc.gathering_complete_promise().await;
        pc.set_local_description(offer)
            .await
            .context("set local description failed")?;
        let _ = gather_complete.recv().await;

        let sdp = pc
            .local_description()
            .await
            .context("no local description")?
            .sdp;

        if !sdp_has_candidate(&sdp) {
            bail!("No ICE candidates were gathered. Check your network/firewall, or try localhost for same-machine testing.");
        }

        let mut sd = SessionDescription {
            sdp,
            ten_kb_site_loc: String::new(),
            key: String::new(),
            nonce: String::new(),
        };

        if self.one_way {
            sd.gen_keys()?;
            sd.encrypt()?;
            sd.ten_kb_site_loc = rand_seq(100);
        }

        Ok(sd)
    }

    async fn set_remote_description_and_wait(&mut self, answer_sdp: String) -> Result<()> {
        if !sdp_has_candidate(&answer_sdp) {
            bail!("The peer Answer contained no ICE candidates. Ensure the browser/client network is reachable and try again.");
        }

        let desc = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::answer(
            answer_sdp,
        )?;
        self.session
            .pc
            .set_remote_description(desc)
            .await
            .context("set remote description failed")?;

        tokio::select! {
            msg = self.session.err_rx.recv() => {
                match msg {
                    Some(None) => {
                        self.cleanup().await;
                        Ok(())
                    }
                    Some(Some(err)) => {
                        self.cleanup().await;
                        Err(err)
                    }
                    None => {
                        self.cleanup().await;
                        Ok(())
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("Sigint received, shutting down...");
                self.cleanup().await;
                Ok(())
            }
        }
    }

    async fn cleanup(&self) {
        if let Ok(guard) = self.dc.lock() {
            if let Some(ref dc) = *guard {
                let _ = dc.send_text("quit").await;
            }
        }
        if is_stdin_terminal() {
            let _ = TerminalState::restore();
        }
    }
}

async fn data_channel_on_open(
    dc: Arc<RTCDataChannel>,
    cmd: Vec<String>,
    err_tx: mpsc::Sender<Option<anyhow::Error>>,
    non_interactive: bool,
    writer_shared: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>,
    pty_master_shared: Arc<Mutex<Option<PtyMaster>>>,
) -> Result<()> {
    println!("Terminal session started.");

    let (pty_master, mut reader, writer) = PtyMaster::new(&cmd)?;
    *pty_master_shared.lock().await = Some(pty_master);

    {
        let mut guard = writer_shared.lock().unwrap();
        *guard = Some(writer);
    }

    if !non_interactive && is_stdin_terminal() {
        TerminalState::make_raw()?;

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

    let dc_clone = Arc::clone(&dc);
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                let _ = err_tx.send(Some(anyhow::anyhow!("pty eof"))).await;
                break;
            }
            Ok(n) => {
                if !non_interactive {
                    let _ = io::stdout().write_all(&buf[..n]);
                    let _ = io::stdout().flush();
                }
                if let Err(e) = dc_clone.send(&Bytes::copy_from_slice(&buf[..n])).await {
                    let _ = err_tx.send(Some(anyhow::anyhow!("dc send error: {e}"))).await;
                    break;
                }
            }
            Err(e) => {
                let _ = err_tx.send(Some(anyhow::anyhow!("pty read error: {e}"))).await;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_host_message(
    msg: DataChannelMessage,
    writer_shared: Arc<std::sync::Mutex<Option<Box<dyn Write + Send>>>>,
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
                                let _ = pm.resize(rows, cols);
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
