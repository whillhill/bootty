use crate::sdp::SessionDescription;
use crate::session::{sdp_has_candidate, Session};
use crate::ten_kb_site::create_10kb_file;
use crate::terminal::{is_stdin_terminal, TerminalState};
use anyhow::{bail, Context, Result};
use bytes::Bytes;
use std::io::{self, Read, Write};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;

pub struct ClientSession {
    session: Session,
    offer_string: String,
}

impl ClientSession {
    pub async fn new(offer_string: String, stun_servers: Vec<String>) -> Result<Self> {
        let session = Session::new(stun_servers, false).await?;
        Ok(ClientSession {
            session,
            offer_string,
        })
    }

    pub async fn run(&mut self) -> Result<()> {
        let mut offer = SessionDescription::decode(&self.offer_string)?;
        if !offer.key.is_empty() {
            offer.decrypt()?;
        }

        if !sdp_has_candidate(&offer.sdp) {
            bail!(
                "The Host Offer contained no ICE candidates. Ask the host to check their network and generate a new Offer."
            );
        }

        let pc = Arc::clone(&self.session.pc);
        let err_tx = self.session.err_tx.clone();
        let err_tx_state = self.session.err_tx.clone();

        pc.on_peer_connection_state_change(Box::new(move |state: RTCPeerConnectionState| {
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
        }));

        pc.on_data_channel(Box::new(move |dc: Arc<RTCDataChannel>| {
            let dc_open = Arc::clone(&dc);
            let dc_msg = Arc::clone(&dc);
            let err_tx_open = err_tx.clone();
            let err_tx_msg = err_tx.clone();

            dc.on_open(Box::new(move || {
                let err_tx = err_tx_open.clone();
                let dc = Arc::clone(&dc_open);
                Box::pin(async move {
                    if let Err(e) = data_channel_on_open(dc, err_tx).await {
                        tracing::error!("data channel on_open error: {e}");
                    }
                })
            }));

            dc_msg.on_message(Box::new(move |msg: DataChannelMessage| {
                let err_tx = err_tx_msg.clone();
                Box::pin(async move {
                    if let Err(e) = handle_client_message(msg, err_tx).await {
                        tracing::error!("message handle error: {e}");
                    }
                })
            }));

            Box::pin(async {})
        }));

        let offer_desc = webrtc::peer_connection::sdp::session_description::RTCSessionDescription::offer(
            offer.sdp,
        )?;
        pc.set_remote_description(offer_desc)
            .await
            .context("set remote description failed")?;

        let answer = pc
            .create_answer(None)
            .await
            .context("create answer failed")?;

        let mut gather_complete = pc.gathering_complete_promise().await;
        pc.set_local_description(answer)
            .await
            .context("set local description failed")?;
        let _ = gather_complete.recv().await;

        let mut answer_sd = SessionDescription {
            sdp: pc
                .local_description()
                .await
                .context("no local description")?
                .sdp,
            ten_kb_site_loc: String::new(),
            key: offer.key.clone(),
            nonce: offer.nonce.clone(),
        };

        if !sdp_has_candidate(&answer_sd.sdp) {
            bail!(
                "No ICE candidates were gathered. Check your network/firewall and try again."
            );
        }

        if !offer.key.is_empty() {
            answer_sd.encrypt()?;
            answer_sd.key = String::new();
            answer_sd.nonce = String::new();
        }

        let encoded = answer_sd.encode()?;
        if offer.ten_kb_site_loc.is_empty() {
            println!("Answer created. Send the following answer to the host:\n");
            println!("{encoded}");
        } else {
            create_10kb_file(&offer.ten_kb_site_loc, &encoded).await?;
            println!("Answer uploaded to 10kb.site");
        }

        match self.session.err_rx.recv().await {
            Some(None) => {
                self.cleanup();
                Ok(())
            }
            Some(Some(err)) => {
                self.cleanup();
                Err(err)
            }
            None => {
                self.cleanup();
                Ok(())
            }
        }
    }

    fn cleanup(&self) {
        if is_stdin_terminal() {
            let _ = TerminalState::restore();
        }
    }
}

async fn data_channel_on_open(
    dc: Arc<RTCDataChannel>,
    err_tx: mpsc::Sender<Option<anyhow::Error>>,
) -> Result<()> {
    tracing::info!(
        "Data channel '{}'-'{}' open.",
        dc.label(),
        dc.id()
    );
    println!("Terminal session started.");

    if is_stdin_terminal() {
        TerminalState::make_raw()?;
    }

    let dc_clone = Arc::clone(&dc);
    tokio::spawn(async move {
        let mut buf = [0u8; 1024];
        loop {
            match io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if let Err(e) = dc_clone.send(&Bytes::copy_from_slice(&buf[..n])).await {
                        let _ = err_tx.send(Some(anyhow::anyhow!("stdin send error: {e}"))).await;
                        break;
                    }
                }
                Err(e) => {
                    let _ = err_tx.send(Some(anyhow::anyhow!("stdin read error: {e}"))).await;
                    break;
                }
            }
        }
    });

    #[cfg(unix)]
    {
        let dc = Arc::clone(&dc);
        tokio::spawn(async move {
            let mut sigwinch = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("Failed to register SIGWINCH handler: {e}");
                    return;
                }
            };

            let _ = send_term_size(&dc).await;

            loop {
                sigwinch.recv().await;
                let _ = send_term_size(&dc).await;
            }
        });
    }

    Ok(())
}

async fn send_term_size(dc: &Arc<RTCDataChannel>) -> Result<()> {
    let (cols, rows) = TerminalState::size()?;
    let msg = format!(r#"["set_size",{},{}]"#, rows, cols);
    dc.send_text(&msg).await?;
    Ok(())
}

async fn handle_client_message(
    msg: DataChannelMessage,
    err_tx: mpsc::Sender<Option<anyhow::Error>>,
) -> Result<()> {
    if msg.is_string {
        let data = String::from_utf8_lossy(&msg.data);
        if data == "quit" {
            if is_stdin_terminal() {
                let _ = TerminalState::restore();
            }
            let _ = err_tx.send(None).await;
            return Ok(());
        }
        tracing::warn!("Unmatched string message: {}", data);
    } else {
        let mut stdout = io::stdout();
        stdout.write_all(&msg.data)?;
        stdout.flush()?;
    }
    Ok(())
}
