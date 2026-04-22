use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::mpsc;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::APIBuilder;
use webrtc::dtls_transport::dtls_role::DTLSRole;
use webrtc::dtls_transport::dtls_transport_state::RTCDtlsTransportState;
use webrtc::ice::network_type::NetworkType;
use webrtc::ice_transport::ice_candidate::RTCIceCandidate;
use webrtc::ice_transport::ice_connection_state::RTCIceConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;

pub struct Session {
    pub pc: Arc<RTCPeerConnection>,
    pub err_tx: mpsc::Sender<Option<anyhow::Error>>,
    pub err_rx: mpsc::Receiver<Option<anyhow::Error>>,
    pub stun_servers: Vec<String>,
}

impl Session {
    pub async fn new(stun_servers: Vec<String>, is_offerer: bool) -> Result<Self> {
        let (err_tx, err_rx) = mpsc::channel(1);

        let mut ice_servers = vec![];
        for url in &stun_servers {
            ice_servers.push(RTCIceServer {
                urls: vec![url.clone()],
                ..Default::default()
            });
        }

        let config = RTCConfiguration {
            ice_servers,
            ..Default::default()
        };

        let mut setting_engine = SettingEngine::default();
        // Allow loopback candidates so localhost / same-machine testing works.
        setting_engine.set_include_loopback_candidate(true);
        // Some environments show IPv6 ICE issues; prefer IPv4 UDP to reduce early failures.
        setting_engine.set_network_types(vec![NetworkType::Udp4]);
        // Make the offerer use the DTLS server role to avoid role conflicts with browsers.
        if is_offerer {
            setting_engine
                .set_answering_dtls_role(DTLSRole::Server)
                .context("set answering dtls role failed")?;
        }

        let api = APIBuilder::new().with_setting_engine(setting_engine).build();

        let pc = Arc::new(
            api.new_peer_connection(config)
                .await
                .context("create peer connection failed")?,
        );

        let pc_clone = Arc::clone(&pc);
        pc_clone.on_ice_connection_state_change(Box::new(move |state| {
            tracing::info!("ICE connection state changed: {state}");
            if state == RTCIceConnectionState::Failed {
                eprintln!("[warn] ICE connection state failed: {state}");
            }
            Box::pin(async {})
        }));

        let pc_clone = Arc::clone(&pc);
        pc_clone.on_signaling_state_change(Box::new(move |state| {
            tracing::info!("Signaling state changed: {state}");
            Box::pin(async {})
        }));

        let pc_clone = Arc::clone(&pc);
        pc_clone.on_ice_gathering_state_change(Box::new(move |state| {
            tracing::info!("ICE gathering state changed: {state}");
            Box::pin(async {})
        }));

        let pc_clone = Arc::clone(&pc);
        pc_clone.on_ice_candidate(Box::new(move |cand: Option<RTCIceCandidate>| {
            Box::pin(async move {
                match cand {
                    Some(c) => tracing::info!("Local ICE candidate: {c}"),
                    None => tracing::info!("Local ICE candidate gathering complete"),
                }
            })
        }));

        pc.sctp()
            .transport()
            .ice_transport()
            .on_selected_candidate_pair_change(Box::new(move |pair| {
            tracing::info!("Selected ICE candidate pair: {pair}");
            Box::pin(async {})
        }));

        let dtls_transport = pc.sctp().transport();
        let err_tx_dtls = err_tx.clone();
        dtls_transport.on_state_change(Box::new(move |state| {
            let err_tx = err_tx_dtls.clone();
            tracing::info!("DTLS transport state changed: {state}");
            if state == RTCDtlsTransportState::Failed {
                eprintln!("[warn] DTLS transport state failed: {state}");
                Box::pin(async move {
                    let _ = err_tx
                        .send(Some(anyhow::anyhow!("DTLS handshake failed, state: {state}")))
                        .await;
                })
            } else {
                Box::pin(async {})
            }
        }));

        let sctp = pc.sctp();
        let err_tx_sctp = err_tx.clone();
        sctp.on_error(Box::new(move |err| {
            let err_tx = err_tx_sctp.clone();
            let msg = format!("SCTP transport error: {err}");
            tracing::error!("{msg}");
            eprintln!("[warn] {msg}");
            Box::pin(async move {
                let _ = err_tx.send(Some(anyhow::anyhow!(msg))).await;
            })
        }));

        let sctp = pc.sctp();
        sctp.on_data_channel_opened(Box::new(move |dc| {
            tracing::info!(
                "SCTP data channel opened: label='{}' id='{}'",
                dc.label(),
                dc.id()
            );
            Box::pin(async {})
        }));

        Ok(Session {
            pc,
            err_tx,
            err_rx,
            stun_servers,
        })
    }
}

pub fn sdp_has_candidate(sdp: &str) -> bool {
    sdp.lines().any(|line| line.starts_with("a=candidate:"))
}
