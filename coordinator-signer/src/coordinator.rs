mod command;
mod manager;
mod session;
mod session_ex;
use crate::keystore::Keystore;
use crate::types::error::SessionError;
use crate::types::message::{
    CoorBehaviour, CoorBehaviourEvent, CoorToSigRequest, CoorToSigResponse, DKGRequestWrap,
    DKGRequestWrapEx, DKGResponseWrap, DKGResponseWrapEx, NodeToCoorRequest, NodeToCoorResponse,
    SigToCoorRequest, SigToCoorResponse, SigningRequestWrap, SigningRequestWrapEx,
    SigningResponseWrap, SigningResponseWrapEx, TargetOrBroadcast, ValidatorIdentityRequest,
    ValidatorIdentityResponse,
};
use crate::types::{AutoDKG, GroupPublicKeyInfo, SignatureSuiteInfo, Validator};
use crate::utils::*;
use crate::{crypto::*, utils};
use anyhow::anyhow;
use command::Command;
use common::Settings;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use libp2p::request_response::{
    InboundRequestId, OutboundRequestId, ProtocolSupport, ResponseChannel,
};
use libp2p::{
    identify::{self},
    noise,
    swarm::SwarmEvent,
    tcp, yamux, Multiaddr,
};
use libp2p::{ping, rendezvous, request_response, PeerId, StreamProtocol};
use manager::CoordinatorStateEx;
use manager::Instruction;
use session::SessionWrap;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::unix::SocketAddr;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{oneshot, RwLock};
use tokio::time::Instant;

macro_rules! handle_err {
    ($e:expr, $msg:expr) => {
        if let Err(e) = $e {
            tracing::error!($msg, e);
        }
    };
}
pub struct Coordinator<VI: ValidatorIdentity> {
    p2p_keypair: libp2p::identity::Keypair,
    signer_whitelist: Option<HashSet<VI::Identity>>,
    listen_port: u16,
    swarm: libp2p::Swarm<CoorBehaviour<VI::Identity>>,
    ipc_path: PathBuf,
    valid_validators: HashMap<VI::Identity, Validator<VI>>,
    p2ppeerid_2_endpoint: HashMap<PeerId, Multiaddr>,
    base_path: PathBuf,
    dkg_request_mapping:
        HashMap<OutboundRequestId, (oneshot::Sender<DKGResponseWrap<VI::Identity>>, PeerId)>,
    signing_request_mapping:
        HashMap<OutboundRequestId, (oneshot::Sender<SigningResponseWrap<VI::Identity>>, PeerId)>,

    dkg_request_mapping_ex:
        HashMap<OutboundRequestId, (oneshot::Sender<DKGResponseWrapEx>, PeerId)>,
    signing_request_mapping_ex:
        HashMap<OutboundRequestId, (oneshot::Sender<SigningResponseWrapEx>, PeerId)>,

    dkg_session_receiver: UnboundedReceiver<(
        DKGRequestWrap<VI::Identity>,
        oneshot::Sender<DKGResponseWrap<VI::Identity>>,
    )>,
    signing_session_receiver: UnboundedReceiver<(
        SigningRequestWrap<VI::Identity>,
        oneshot::Sender<SigningResponseWrap<VI::Identity>>,
    )>,
    dkg_session_receiver_ex: UnboundedReceiver<(
        DKGRequestWrapEx<VI::Identity>,
        oneshot::Sender<DKGResponseWrapEx>,
    )>,
    signing_session_receiver_ex: UnboundedReceiver<(
        SigningRequestWrapEx<VI::Identity>,
        oneshot::Sender<SigningResponseWrapEx>,
    )>,

    dkg_in_final_channel_sender: UnboundedSender<(
        DKGRequestWrapEx<VI::Identity>,
        oneshot::Sender<DKGResponseWrapEx>,
    )>,
    signing_in_final_channel_sender: UnboundedSender<(
        SigningRequestWrapEx<VI::Identity>,
        oneshot::Sender<SigningResponseWrapEx>,
    )>,
    instruction_sender: UnboundedSender<Instruction<VI::Identity>>,
    dkg_response_futures_for_node: FuturesUnordered<
        oneshot::Receiver<(
            Result<PkId, SessionError>,
            ResponseChannel<NodeToCoorResponse<VI::Identity>>,
        )>,
    >,
    signing_response_futures_for_node: FuturesUnordered<
        oneshot::Receiver<(
            Result<SignatureSuiteInfo<VI::Identity>, SessionError>,
            ResponseChannel<NodeToCoorResponse<VI::Identity>>,
        )>,
    >,
    lspk_response_futures_for_node: FuturesUnordered<
        oneshot::Receiver<(
            Result<HashMap<CryptoType, Vec<PkId>>, SessionError>,
            ResponseChannel<NodeToCoorResponse<VI::Identity>>,
        )>,
    >,
    pk_response_futures_for_node: FuturesUnordered<
        oneshot::Receiver<(
            Result<GroupPublicKeyInfo, SessionError>,
            ResponseChannel<NodeToCoorResponse<VI::Identity>>,
        )>,
    >,

    // !WARNING: auto_dkg should not be used in multi-application scenarios.
    // !WARNING: It only generates one base key for each key type, and other keys are derived through tweaking.
    // !WARNING: Note that these are tweaked keys, not derived keys - this is highly insecure for multi-application use cases since tweaking does not provide proper key isolation between applications.
    auto_dkg: Option<Arc<RwLock<AutoDKG<VI::Identity>>>>,
}
impl<VI: ValidatorIdentity> Coordinator<VI> {
    pub fn new(
        p2p_keypair: libp2p::identity::Keypair,
        base_path: PathBuf,
        signer_whitelist: Option<HashSet<VI::Identity>>,
        port: u16,
        automatic_dkg: Option<u16>,
    ) -> anyhow::Result<Self> {
        let swarm = libp2p::SwarmBuilder::with_existing_identity(p2p_keypair.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_behaviour(|key| CoorBehaviour {
                identify: identify::Behaviour::new(identify::Config::new(
                    "/ipfs/id/1.0.0".to_string(),
                    key.public(),
                )),
                ping: ping::Behaviour::new(
                    ping::Config::new().with_interval(Duration::from_secs(1)),
                ),
                rendezvous: rendezvous::server::Behaviour::new(
                    rendezvous::server::Config::default(),
                ),
                node2coor: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/node2coor"), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(
                        Settings::global().connection.node2coor_request_timeout,
                    )),
                ),
                sig2coor: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/sig2coor"), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(
                        Settings::global().connection.sig2coor_request_timeout,
                    )),
                ),
                coor2sig: request_response::cbor::Behaviour::new(
                    [(StreamProtocol::new("/coor2sig"), ProtocolSupport::Full)],
                    request_response::Config::default().with_request_timeout(Duration::from_secs(
                        Settings::global().connection.coor2sig_request_timeout,
                    )),
                ),
            })?
            .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(1000)))
            .build();
        let (dkg_session_sender, dkg_session_receiver) = tokio::sync::mpsc::unbounded_channel();
        let (signing_session_sender, signing_session_receiver) =
            tokio::sync::mpsc::unbounded_channel();

        let (dkg_session_sender_ex, dkg_session_receiver_ex) =
            tokio::sync::mpsc::unbounded_channel();
        let (signing_session_sender_ex, signing_session_receiver_ex) =
            tokio::sync::mpsc::unbounded_channel();
        let (instruction_sender, instruction_receiver) = tokio::sync::mpsc::unbounded_channel();

        let (dkg_in_final_channel_sender, dkg_in_final_channel_receiver) =
            tokio::sync::mpsc::unbounded_channel();
        let (signing_in_final_channel_sender, signing_in_final_channel_receiver) =
            tokio::sync::mpsc::unbounded_channel();

        let keystore =
            Arc::new(Keystore::new(p2p_keypair.derive_secret(b"keystore").unwrap(), None).unwrap());
        manager::CoordiantorSessionManager::new(
            instruction_receiver,
            dkg_session_sender,
            signing_session_sender,
            dkg_session_sender_ex,
            signing_session_sender_ex,
            dkg_in_final_channel_receiver,
            signing_in_final_channel_receiver,
            keystore,
            &base_path,
        )?
        .listening();
        // if auto_dkg file exists, load it
        let auto_dkg_path = base_path.join("auto_dkg.json");
        let auto_dkg = if auto_dkg_path.exists() {
            let auto_dkg_file = File::open(auto_dkg_path).unwrap();
            let auto_dkg_reader = std::io::BufReader::new(auto_dkg_file);
            let auto_dkg: Option<AutoDKG<VI::Identity>> =
                serde_json::from_reader(auto_dkg_reader).unwrap();
            auto_dkg
        } else {
            if let Some(min_signers) = automatic_dkg {
                if signer_whitelist.clone().is_none() {
                    return Err(anyhow::anyhow!(
                        "signer_whitelist is required when automatic_dkg is enabled"
                    ));
                }
                if min_signers == 0
                    || signer_whitelist.clone().unwrap().len() < min_signers as usize
                {
                    return Err(anyhow::anyhow!(
                    "min_signers must be greater than 0 and less than or equal to the number of signers in the whitelist, min signer:{:?}, whitelist:{:?}",
                    min_signers,
                    signer_whitelist.clone().unwrap().len()
                ));
                }
                Some(AutoDKG::new(min_signers, signer_whitelist.clone().unwrap()))
            } else {
                None
            }
        };
        Ok(Self {
            p2p_keypair,
            signer_whitelist,
            listen_port: port,
            swarm,
            ipc_path: base_path.join(Settings::global().coordinator.ipc_socket_path),
            valid_validators: HashMap::new(),
            p2ppeerid_2_endpoint: HashMap::new(),
            dkg_request_mapping: HashMap::new(),
            signing_request_mapping: HashMap::new(),
            dkg_request_mapping_ex: HashMap::new(),
            signing_request_mapping_ex: HashMap::new(),
            dkg_session_receiver,
            signing_session_receiver,
            dkg_session_receiver_ex,
            signing_session_receiver_ex,
            dkg_in_final_channel_sender,
            signing_in_final_channel_sender,
            instruction_sender,
            dkg_response_futures_for_node: FuturesUnordered::new(),
            signing_response_futures_for_node: FuturesUnordered::new(),
            lspk_response_futures_for_node: FuturesUnordered::new(),
            pk_response_futures_for_node: FuturesUnordered::new(),
            auto_dkg: auto_dkg.map(|dkg| Arc::new(RwLock::new(dkg))),
            base_path,
        })
    }
    // if automatic_dkg is Some(min_signers), the coordinator will do dkg for all ciphersuites,
    // if automatic_dkg is None, the coordinator will do dkg manually,
    pub async fn start_listening(mut self) -> Result<(), anyhow::Error> {
        self.swarm
            .listen_on(format!("/ip4/0.0.0.0/tcp/{}", self.listen_port).parse()?)?;
        let listener = self.start_ipc_listening().await?;
        loop {
            tokio::select! {
                event = self.swarm.select_next_some()=> {
                    if let Err(e) = self.handle_swarm_event(event).await{
                        tracing::error!("Error handling swarm event: {}", e);
                    }
                },
                recv_data = self.dkg_session_receiver.recv()=> {
                    tracing::debug!("Received DKG request from session {:?}", recv_data);
                    if let Some((request, sender)) = recv_data {
                        if let Err(e) = self.handle_dkg_request(request, sender).await {
                            tracing::error!("Error handling DKG request: {}", e);
                        }
                    } else {
                        tracing::error!("Error receiving DKG request");
                    }
                }
                recv_data = self.signing_session_receiver.recv()=> {
                    if let Some((request, sender)) = recv_data {
                        if let Err(e) = self.handle_signing_request(request, sender).await {
                            tracing::error!("Error handling signing request: {}", e);
                        }
                    } else {
                        tracing::error!("Error receiving signing request");
                    }
                }
                recv_data = self.dkg_session_receiver_ex.recv()=> {
                    tracing::debug!("Received DKG request from session {:?}", recv_data);
                    if let Some((request, sender)) = recv_data {
                        if let Err(e) = self.handle_dkg_request_ex(request, sender).await {
                            tracing::error!("Error handling DKG request: {}", e);
                        }
                    } else {
                        tracing::error!("Error receiving DKG request");
                    }
                }
                recv_data = self.signing_session_receiver_ex.recv()=> {
                    if let Some((request, sender)) = recv_data {
                        if let Err(e) = self.handle_signing_request_ex(request, sender).await {
                            tracing::error!("Error handling signing request: {}", e);
                        }
                    } else {
                        tracing::error!("Error receiving signing request");
                    }
                }
                command_result = listener.accept()=> {
                    if let Err(e) = self.handle_command(command_result).await {
                        tracing::error!("Error handling command: {}", e);
                    }
                }
                Some(Ok((result, channel))) = self.dkg_response_futures_for_node.next()=> {
                    match result {
                        Ok(pkid) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::DKGResponse { pkid }) {
                                tracing::error!("Error sending DKG response to node: {:?}", e);
                            }
                        }
                        Err(e) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::Failure(e.to_string())) {
                                tracing::error!("Error sending DKG failure response to node: {:?}", e);
                            }
                        }
                    }
                }
                Some(Ok((result, channel))) = self.signing_response_futures_for_node.next()=> {
                    tracing::info!("{}",channel.is_open());
                    match result {
                        Ok(signature_suite_info) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::SigningResponse { signature_suite_info }) {
                                tracing::error!("Error sending signing response to node: {:?}", e);
                            }
                        }
                        Err(e) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::Failure(e.to_string())) {
                                tracing::error!("Error sending signing failure response to node: {:?}", e);
                            }
                        }
                    }
                }
                Some(Ok((result, channel))) = self.lspk_response_futures_for_node.next()=> {
                    match result {
                        Ok(pkids) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::LsPkResponse { pkids }) {
                                tracing::error!("Error sending LsPk response to node: {:?}", e);
                            }
                        }
                        Err(e) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::Failure(e.to_string())) {
                                tracing::error!("Error sending LsPk failure response to node: {:?}", e);
                            }
                        }
                    }
                }
                Some(Ok((result, channel))) = self.pk_response_futures_for_node.next()=> {
                    match result {
                        Ok(group_public_key_info) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::PkTweakResponse { group_public_key_info }) {
                                tracing::error!("Error sending PK response to node: {:?}", e);
                            }
                        }
                        Err(e) => {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(channel, NodeToCoorResponse::Failure(e.to_string())) {
                                tracing::error!("Error sending PK failure response to node: {:?}", e);
                            }
                        }
                    }
                }
            }
        }
    }
    pub(crate) async fn handle_dkg_request(
        &mut self,
        request: DKGRequestWrap<VI::Identity>,
        sender: oneshot::Sender<DKGResponseWrap<VI::Identity>>,
    ) -> Result<(), anyhow::Error> {
        tracing::debug!("Received DKG request From Session: {:?}", request);
        let peer = request.identity();
        let validator = self.valid_validators.get(peer).cloned();
        match validator {
            Some(validator) => {
                tracing::debug!(
                    "Sending DKG request to validator: {:?}",
                    validator.p2p_peer_id
                );
                if let Some(addr) = validator.address {
                    self.swarm.add_peer_address(validator.p2p_peer_id, addr);
                }
                let outbound_request_id = self.swarm.behaviour_mut().coor2sig.send_request(
                    &validator.p2p_peer_id,
                    CoorToSigRequest::DKGRequest(request),
                );
                tracing::debug!("Outbound request id: {:?}", outbound_request_id);
                self.dkg_request_mapping
                    .insert(outbound_request_id, (sender, validator.p2p_peer_id.clone()));
            }
            None => {
                tracing::error!("Validator not found");
                if let Err(e) = sender
                    .send(request.failure(format!("Validator not found: {}", peer.to_fmt_string())))
                {
                    tracing::error!("Error sending failure response: {:?}", e);
                    return Err(anyhow::anyhow!("Error sending failure response: {:?}", e));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_signing_request(
        &mut self,
        request: SigningRequestWrap<VI::Identity>,
        sender: oneshot::Sender<SigningResponseWrap<VI::Identity>>,
    ) -> Result<(), anyhow::Error> {
        tracing::debug!("Received Signing request From Session: {:?}", request);
        let peer = request.identity();
        let validator = self.valid_validators.get(peer).cloned();
        match validator {
            Some(validator) => {
                tracing::debug!(
                    "Sending Signing request to validator: {:?}",
                    validator.p2p_peer_id
                );
                if let Some(addr) = validator.address {
                    self.swarm.add_peer_address(validator.p2p_peer_id, addr);
                }
                let outbound_request_id = self.swarm.behaviour_mut().coor2sig.send_request(
                    &validator.p2p_peer_id,
                    CoorToSigRequest::SigningRequest(request),
                );
                tracing::debug!("Outbound request id: {:?}", outbound_request_id);
                self.signing_request_mapping
                    .insert(outbound_request_id, (sender, validator.p2p_peer_id.clone()));
            }
            None => {
                tracing::error!("Validator not found");
                if let Err(e) = sender
                    .send(request.failure(format!("Validator not found: {}", peer.to_fmt_string())))
                {
                    tracing::error!("Error sending failure response: {:?}", e);
                    return Err(anyhow::anyhow!("Error sending failure response: {:?}", e));
                }
                return Ok(());
            }
        }
        Ok(())
    }
    pub(crate) async fn handle_dkg_request_ex(
        &mut self,
        request: DKGRequestWrapEx<VI::Identity>,
        sender: oneshot::Sender<DKGResponseWrapEx>,
    ) -> Result<(), anyhow::Error> {
        tracing::debug!("Received DKG request From Session: {:?}", request);
        let peer = request.identity();
        let validator = self.valid_validators.get(peer).cloned();
        match validator {
            Some(validator) => {
                tracing::debug!(
                    "Sending DKG request to validator: {:?}",
                    validator.p2p_peer_id
                );
                if let Some(addr) = validator.address {
                    self.swarm.add_peer_address(validator.p2p_peer_id, addr);
                }
                let outbound_request_id = self.swarm.behaviour_mut().coor2sig.send_request(
                    &validator.p2p_peer_id,
                    CoorToSigRequest::DKGRequestEx(request),
                );
                tracing::info!("Outbound request id: {:?}", outbound_request_id);

                self.dkg_request_mapping_ex
                    .insert(outbound_request_id, (sender, validator.p2p_peer_id.clone()));
            }
            None => {
                tracing::error!("Validator not found");
                if let Err(e) = sender
                    .send(request.failure(format!("Validator not found: {}", peer.to_fmt_string())))
                {
                    tracing::error!("Error sending failure response: {:?}", e);
                    return Err(anyhow::anyhow!("Error sending failure response: {:?}", e));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_signing_request_ex(
        &mut self,
        request: SigningRequestWrapEx<VI::Identity>,
        sender: oneshot::Sender<SigningResponseWrapEx>,
    ) -> Result<(), anyhow::Error> {
        tracing::debug!("Received Signing request From Session: {:?}", request);
        let peer = request.identity();
        let validator = self.valid_validators.get(peer).cloned();
        match validator {
            Some(validator) => {
                tracing::debug!(
                    "Sending Signing request to validator: {:?}",
                    validator.p2p_peer_id
                );
                if let Some(addr) = validator.address {
                    self.swarm.add_peer_address(validator.p2p_peer_id, addr);
                }
                let outbound_request_id = self.swarm.behaviour_mut().coor2sig.send_request(
                    &validator.p2p_peer_id,
                    CoorToSigRequest::SigningRequestEx(request),
                );
                tracing::debug!("Outbound request id: {:?}", outbound_request_id);
                self.signing_request_mapping_ex
                    .insert(outbound_request_id, (sender, validator.p2p_peer_id.clone()));
            }
            None => {
                tracing::error!("Validator not found");
                if let Err(e) = sender
                    .send(request.failure(format!("Validator not found: {}", peer.to_fmt_string())))
                {
                    tracing::error!("Error sending failure response: {:?}", e);
                    return Err(anyhow::anyhow!("Error sending failure response: {:?}", e));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    pub(crate) async fn handle_swarm_event(
        &mut self,
        event: SwarmEvent<CoorBehaviourEvent<VI::Identity>>,
    ) -> Result<(), anyhow::Error> {
        match event {
            SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                tracing::info!("Coordinator connected to {}", peer_id);
                self.p2ppeerid_2_endpoint
                    .insert(peer_id, endpoint.get_remote_address().clone());
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                tracing::warn!("Coordinator disconnected from {}", peer_id);
                // self.p2ppeerid_2_endpoint.remove(&peer_id);
            }
            SwarmEvent::OutgoingConnectionError { error, .. } => {
                tracing::error!("Coordinator outgoing connection error: {:?}", error);
            }
            SwarmEvent::IncomingConnectionError { error, .. } => {
                tracing::error!("Coordinator incoming connection error: {:?}", error);
            }
            SwarmEvent::ListenerError { error, .. } => {
                tracing::error!("Coordinator listener error: {:?}", error);
            }
            SwarmEvent::Behaviour(CoorBehaviourEvent::Rendezvous(
                rendezvous::server::Event::PeerRegistered { peer, registration },
            )) => {
                tracing::debug!(
                    "Peer {} registered for namespace '{}'",
                    peer,
                    registration.namespace
                );
            }
            SwarmEvent::Behaviour(CoorBehaviourEvent::Rendezvous(
                rendezvous::server::Event::DiscoverServed {
                    enquirer,
                    registrations,
                },
            )) => {
                tracing::debug!(
                    "Served peer {} with {} registrations",
                    enquirer,
                    registrations.len()
                );
            }
            SwarmEvent::Behaviour(CoorBehaviourEvent::Coor2sig(
                request_response::Event::Message {
                    peer,
                    message:
                        request_response::Message::Response {
                            request_id,
                            response,
                        },
                    ..
                },
            )) => match response {
                CoorToSigResponse::DKGResponse(response) => {
                    tracing::info!(
                        "Coordinator received dkg response from signer {} with request_id {}",
                        peer,
                        request_id
                    );
                    let (_, validator_peer_id) = self.dkg_request_mapping.get(&request_id).unwrap();
                    if *validator_peer_id != peer {
                        tracing::warn!("Invalid validator peer id: {:?}", validator_peer_id);
                        return Ok(());
                    }
                    if let Some((sender, _)) = self.dkg_request_mapping.remove(&request_id) {
                        tracing::debug!("Sending response {:?} to session", response);
                        if let Err(e) = sender.send(response) {
                            tracing::error!("Coordiantor error sending response: {:?}", e);
                        } else {
                            tracing::debug!("Sent response to session");
                        }
                    } else {
                        tracing::error!("Request id {:?} not found in request mapping", request_id);
                    }
                }
                CoorToSigResponse::SigningResponse(response) => {
                    tracing::info!(
                        "Coordinator received signing response from {} with request_id {}",
                        peer,
                        request_id
                    );
                    let (_, validator_peer_id) =
                        self.signing_request_mapping.get(&request_id).unwrap();
                    if *validator_peer_id != peer {
                        tracing::warn!("Invalid validator peer id: {:?}", validator_peer_id);
                        return Ok(());
                    }
                    if let Some((sender, _)) = self.signing_request_mapping.remove(&request_id) {
                        tracing::debug!("Sending response {:?} to session", response);
                        if let Err(e) = sender.send(response) {
                            tracing::error!("Error sending response: {:?}", e);
                        }
                    }
                }
                CoorToSigResponse::DKGResponseEx(dkg_response_wrap_ex) => {
                    tracing::info!(
                        "Coordinator received dkg response from {} with request_id {}, data:{:?}",
                        peer,
                        request_id,
                        dkg_response_wrap_ex
                    );
                    if self.dkg_request_mapping_ex.contains_key(&request_id) {
                        let (_, validator_peer_id) =
                            self.dkg_request_mapping_ex.get(&request_id).unwrap();
                        if *validator_peer_id != peer {
                            tracing::warn!("Invalid validator peer id: {:?}", validator_peer_id);
                            return Ok(());
                        }
                        if let Some((sender, _)) = self.dkg_request_mapping_ex.remove(&request_id) {
                            tracing::debug!(
                                "Sending response {:?} to session",
                                dkg_response_wrap_ex
                            );
                            if let Err(e) = sender.send(dkg_response_wrap_ex) {
                                tracing::error!("Coordiantor error sending response: {:?}", e);
                            } else {
                                tracing::debug!("Sent response to session");
                            }
                        } else {
                            tracing::error!(
                                "Request id {:?} not found in request mapping",
                                request_id
                            );
                        }
                    } else {
                        tracing::debug!("Request id {:?} not found in request mapping", request_id);
                    }
                }
                CoorToSigResponse::SigningResponseEx(signing_response_wrap_ex) => {
                    tracing::info!(
                        "Coordinator received signing response from {} with request_id {}, data:{:?}",
                        peer,
                        request_id,
                        signing_response_wrap_ex
                    );
                    if self.signing_request_mapping_ex.contains_key(&request_id) {
                        let (_, validator_peer_id) =
                            self.signing_request_mapping_ex.get(&request_id).unwrap();
                        if *validator_peer_id != peer {
                            tracing::warn!("Invalid validator peer id: {:?}", validator_peer_id);
                            return Ok(());
                        }
                        if let Some((sender, _)) =
                            self.signing_request_mapping_ex.remove(&request_id)
                        {
                            tracing::debug!(
                                "Sending response {:?} to session",
                                signing_response_wrap_ex
                            );
                            if let Err(e) = sender.send(signing_response_wrap_ex) {
                                tracing::error!("Coordiantor error sending response: {:?}", e);
                            } else {
                                tracing::debug!("Sent response to session");
                            }
                        } else {
                            tracing::error!(
                                "Request id {:?} not found in request mapping",
                                request_id
                            );
                        }
                    } else {
                        tracing::debug!("Request id {:?} not found in request mapping", request_id);
                    }
                }
            },

            SwarmEvent::Behaviour(CoorBehaviourEvent::Node2coor(
                request_response::Event::Message {
                    peer,
                    message:
                        request_response::Message::Request {
                            request_id,
                            request,
                            channel,
                        },
                    ..
                },
            )) => {
                let request_instruction = request.clone();
                let request = request.get_validator_identity();
                if let Err(e) = self
                    .handle_vi_request(peer, request_id, request, false)
                    .await
                {
                    tracing::error!("Error handling vi request: {}", e);
                    if let Err(e) = self
                        .swarm
                        .behaviour_mut()
                        .node2coor
                        .send_response(channel, NodeToCoorResponse::Failure(e.to_string()))
                    {
                        tracing::error!("Error sending failure response to node: {:?}", e);
                    }
                    return Ok(());
                }
                match request_instruction {
                    NodeToCoorRequest::DKGRequest {
                        crypto_type,
                        participants,
                        min_signers,
                        ..
                    } => {
                        if self.auto_dkg.is_some() {
                            tracing::warn!("AutoDKG is enabled, ignoring DKG request");
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(
                                channel,
                                NodeToCoorResponse::Failure("AutoDKG is enabled".to_string()),
                            ) {
                                tracing::error!("Error sending failure response to node: {:?}", e);
                            }
                            return Ok(());
                        }
                        if participants.len() > 255 {
                            tracing::error!("Invalid participants: {:?}", participants);
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(
                                channel,
                                NodeToCoorResponse::Failure("Too many participants".to_string()),
                            ) {
                                tracing::error!("Error sending failure response to node: {:?}", e);
                            }
                            return Ok(());
                        }
                        let participants = participants
                            .iter()
                            .enumerate()
                            .map(|(i, v)| ((i + 1) as u16, v.clone()))
                            .collect();
                        let (instruction_sender, instruction_receiver) = oneshot::channel();
                        let (node_response_sender, node_response_receiver) = oneshot::channel();
                        self.dkg_response_futures_for_node
                            .push(node_response_receiver);
                        let instruction = Instruction::NewKey {
                            crypto_type,
                            participants,
                            min_signers,
                            pkid_response_oneshot: instruction_sender,
                        };
                        self.instruction_sender.send(instruction).unwrap();
                        tokio::spawn(async move {
                            let result = instruction_receiver.await;
                            match result {
                                Ok(pkid_result) => {
                                    if let Err(e) =
                                        node_response_sender.send((pkid_result, channel))
                                    {
                                        tracing::error!("Error sending response to node: {:?}", e);
                                    }
                                }
                                Err(e) => {
                                    if let Err(e) = node_response_sender.send((
                                        Err(SessionError::InstructionResponseError(e.to_string())),
                                        channel,
                                    )) {
                                        tracing::error!(
                                            "Error sending failure response to node: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        });
                        return Ok(());
                    }
                    NodeToCoorRequest::SigningRequest {
                        pkid,
                        msg,
                        tweak_data,
                        ..
                    } => {
                        // tracing::warn!("{}", hex::encode(msg.clone()));
                        let (instruction_sender, instruction_receiver) = oneshot::channel();
                        let (node_response_sender, node_response_receiver) = oneshot::channel();
                        self.signing_response_futures_for_node
                            .push(node_response_receiver);
                        let instruction = Instruction::Sign {
                            pkid,
                            msg,
                            tweak_data,
                            signature_response_oneshot: instruction_sender,
                        };
                        self.instruction_sender.send(instruction).unwrap();
                        tokio::spawn(async move {
                            let result = instruction_receiver.await;
                            match result {
                                Ok(signature_suite_info) => {
                                    if let Err(e) =
                                        node_response_sender.send((signature_suite_info, channel))
                                    {
                                        tracing::error!("Error sending response to node: {:?}", e);
                                    }
                                }
                                Err(e) => {
                                    if let Err(e) = node_response_sender.send((
                                        Err(SessionError::InstructionResponseError(e.to_string())),
                                        channel,
                                    )) {
                                        tracing::error!(
                                            "Error sending failure response to node: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        });
                        return Ok(());
                    }
                    NodeToCoorRequest::LsPkRequest { .. } => {
                        let (session_response_sender, session_response_receiver) =
                            oneshot::channel();
                        let (node_response_sender, node_response_receiver) = oneshot::channel();
                        self.lspk_response_futures_for_node
                            .push(node_response_receiver);
                        let instruction = Instruction::ListPkIds {
                            list_pkids_response_oneshot: session_response_sender,
                        };
                        self.instruction_sender.send(instruction).unwrap();
                        tokio::spawn(async move {
                            let result = session_response_receiver.await;
                            match result {
                                Ok(pkids) => {
                                    if let Err(e) = node_response_sender.send((Ok(pkids), channel))
                                    {
                                        tracing::error!("Error sending response to node: {:?}", e);
                                    }
                                }
                                Err(e) => {
                                    if let Err(e) = node_response_sender.send((
                                        Err(SessionError::InstructionResponseError(e.to_string())),
                                        channel,
                                    )) {
                                        tracing::error!(
                                            "Error sending failure response to node: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        });
                    }
                    NodeToCoorRequest::AutoDKGRequest { .. } => {
                        if let Some(auto_dkg) = &self.auto_dkg {
                            let auto_dkg_result = auto_dkg.read().await.clone();
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(
                                channel,
                                NodeToCoorResponse::AutoDKGResponse {
                                    auto_dkg_result: Some(auto_dkg_result),
                                },
                            ) {
                                tracing::error!("Error sending response to node: {:?}", e);
                            }
                        } else {
                            if let Err(e) = self.swarm.behaviour_mut().node2coor.send_response(
                                channel,
                                NodeToCoorResponse::AutoDKGResponse {
                                    auto_dkg_result: None,
                                },
                            ) {
                                tracing::error!("Error sending response to node: {:?}", e);
                            }
                        }
                    }
                    NodeToCoorRequest::PkTweakRequest {
                        pkid, tweak_data, ..
                    } => {
                        let (session_response_sender, session_response_receiver) =
                            oneshot::channel();
                        let (node_response_sender, node_response_receiver) = oneshot::channel();
                        self.pk_response_futures_for_node
                            .push(node_response_receiver);
                        let instruction = Instruction::PkTweakRequest {
                            pkid,
                            tweak_data,
                            pk_response_oneshot: session_response_sender,
                        };
                        self.instruction_sender.send(instruction).unwrap();
                        tokio::spawn(async move {
                            let result = session_response_receiver.await;
                            match result {
                                Ok(pk_info) => {
                                    if let Err(e) = node_response_sender.send((pk_info, channel)) {
                                        tracing::error!("Error sending response to node: {:?}", e);
                                    }
                                }
                                Err(e) => {
                                    if let Err(e) = node_response_sender.send((
                                        Err(SessionError::InstructionResponseError(e.to_string())),
                                        channel,
                                    )) {
                                        tracing::error!(
                                            "Error sending failure response to node: {:?}",
                                            e
                                        );
                                    }
                                }
                            }
                        });
                        return Ok(());
                    }
                }
            }
            SwarmEvent::Behaviour(CoorBehaviourEvent::Sig2coor(
                request_response::Event::Message {
                    peer,
                    message:
                        request_response::Message::Request {
                            request_id,
                            request,
                            channel,
                        },
                    ..
                },
            )) => match request {
                SigToCoorRequest::ValidatorIndentity(request) => {
                    if let Err(e) = self
                        .handle_vi_request(peer, request_id, request, true)
                        .await
                    {
                        tracing::error!("Error handling vi request: {}", e);
                        if let Err(e) = self.swarm.behaviour_mut().sig2coor.send_response(
                            channel,
                            SigToCoorResponse::ValidatorIdentityResponse(
                                ValidatorIdentityResponse::Failure(e),
                            ),
                        ) {
                            tracing::error!("Error sending failure response to signer: {:?}", e);
                        }
                    } else {
                        if let Err(e) = self.swarm.behaviour_mut().sig2coor.send_response(
                            channel,
                            SigToCoorResponse::ValidatorIdentityResponse(
                                ValidatorIdentityResponse::Success,
                            ),
                        ) {
                            tracing::error!("Error sending success response to signer: {:?}", e);
                        }
                    }
                }
                SigToCoorRequest::DKGRequestEx(request) => {
                    tracing::debug!("Received signer to coordinator request: {:?}", request);
                    let request_wrap = request.clone();
                    match request.dkg_request_ex() {
                        Ok(request) => match request.stage {
                            crate::types::message::DKGStageEx::Init => {
                                tracing::info!("Received signer to coordinator request: Init");
                                handle_err!(
                                    self.swarm.behaviour_mut().sig2coor.send_response(
                                        channel,
                                        SigToCoorResponse::DKGResponseEx(
                                            DKGResponseWrapEx::Failure(
                                                "Cannot send init message".to_string(),
                                            )
                                        ),
                                    ),
                                    "Error sending success response to signer: {:?}"
                                );
                            }
                            crate::types::message::DKGStageEx::Intermediate(message_ex) => {
                                tracing::info!(
                                    "Received signer to coordinator request: Intermediate {:?}",
                                    message_ex.target
                                );
                                match message_ex.target {
                                    TargetOrBroadcast::Target { to } => {
                                        match request.base_info.participants.get(&to) {
                                            Some(id) => {
                                                match self.send_request_to_signer(
                                                    &id,
                                                    CoorToSigRequest::DKGRequestEx(request_wrap),
                                                ) {
                                                    Ok(_) => {
                                                        handle_err!(
                                                            self.swarm.behaviour_mut().sig2coor.send_response(
                                                                channel,
                                                                SigToCoorResponse::DKGResponseEx(
                                                                    DKGResponseWrapEx::Success,
                                                                ),
                                                            ),
                                                            "Error sending success response to signer: {:?}"
                                                        );
                                                    }
                                                    Err(e) => {
                                                        tracing::error!(
                                                            "Error sending request to signer: {}",
                                                            e
                                                        );
                                                        handle_err!(
                                                            self.swarm.behaviour_mut().sig2coor.send_response(
                                                                channel,
                                                                SigToCoorResponse::DKGResponseEx(
                                                                    DKGResponseWrapEx::Failure(
                                                                        e.to_string(),
                                                                    ),
                                                                ),
                                                            ),
                                                            "Error sending failure response to signer: {:?}"
                                                        );
                                                    }
                                                }
                                            }
                                            None => {
                                                tracing::error!("Invalid participant: {}", to);
                                                handle_err!(
                                                    self.swarm.behaviour_mut().sig2coor.send_response(
                                                        channel,
                                                        SigToCoorResponse::DKGResponseEx(
                                                            DKGResponseWrapEx::Failure(
                                                                "Invalid participant".to_string(),
                                                            ),
                                                        ),
                                                    ),
                                                    "Error sending failure response to signer: {:?}"
                                                );
                                            }
                                        }
                                    }
                                    TargetOrBroadcast::Broadcast => {
                                        for (identifier, peer_id) in
                                            request.base_info.participants.iter()
                                        {
                                            if *identifier != message_ex.from {
                                                if let Err(e) = self.send_request_to_signer(
                                                    peer_id,
                                                    CoorToSigRequest::DKGRequestEx(
                                                        request_wrap.clone(),
                                                    ),
                                                ) {
                                                    tracing::error!(
                                                        "Error sending request to signer: {}",
                                                        e
                                                    );
                                                }
                                            }
                                        }
                                        handle_err!(
                                            self.swarm.behaviour_mut().sig2coor.send_response(
                                                channel,
                                                SigToCoorResponse::DKGResponseEx(
                                                    DKGResponseWrapEx::Success,
                                                ),
                                            ),
                                            "Error sending success response to signer: {:?}"
                                        );
                                    }
                                }
                            }
                            crate::types::message::DKGStageEx::Final(_) => {
                                tracing::info!("Received signer to coordinator request: Final");
                                let sender = utils::new_oneshot_to_receive_success_or_error();
                                self.dkg_in_final_channel_sender
                                    .send((request_wrap, sender))
                                    .unwrap();
                                handle_err!(
                                    self.swarm.behaviour_mut().sig2coor.send_response(
                                        channel,
                                        SigToCoorResponse::DKGResponseEx(
                                            DKGResponseWrapEx::Success
                                        ),
                                    ),
                                    "Error sending success response to signer: {:?}"
                                );
                            }
                        },
                        Err(e) => {
                            handle_err!(
                                self.swarm.behaviour_mut().sig2coor.send_response(
                                    channel,
                                    SigToCoorResponse::DKGResponseEx(DKGResponseWrapEx::Failure(
                                        e.to_string(),
                                    )),
                                ),
                                "Error sending success response to signer: {:?}"
                            );
                        }
                    }
                }
                SigToCoorRequest::SigningRequestEx(request) => {
                    tracing::debug!("Received signer to coordinator request: {:?}", request);
                    let request_wrap = request.clone();
                    match request.signing_request_ex() {
                        Ok(request) => match request.stage {
                            crate::types::message::SigningStageEx::Init(_, _) => {
                                handle_err!(
                                    self.swarm.behaviour_mut().sig2coor.send_response(
                                        channel,
                                        SigToCoorResponse::SigningResponseEx(
                                            SigningResponseWrapEx::Failure(
                                                "Cannot send init message".to_string(),
                                            )
                                        ),
                                    ),
                                    "Error sending success response to signer: {:?}"
                                );
                            }
                            crate::types::message::SigningStageEx::Intermediate(message_ex) => {
                                match message_ex.target {
                                    TargetOrBroadcast::Target { to } => {
                                        match request.base_info.participants.get(&to) {
                                            Some(id) => {
                                                match self.send_request_to_signer(
                                                    &id,
                                                    CoorToSigRequest::SigningRequestEx(
                                                        request_wrap,
                                                    ),
                                                ) {
                                                    Ok(_) => {
                                                        handle_err!(self.swarm.behaviour_mut().sig2coor.send_response(
                                                            channel,
                                                            SigToCoorResponse::SigningResponseEx(SigningResponseWrapEx::Success),
                                                        ),"Error sending success response to signer: {:?}");
                                                    }
                                                    Err(e) => {
                                                        handle_err!(self.swarm.behaviour_mut().sig2coor.send_response(
                                                            channel,
                                                            SigToCoorResponse::SigningResponseEx(SigningResponseWrapEx::Failure(e.to_string())),
                                                        ),"Error sending failure response to signer: {:?}");
                                                    }
                                                }
                                            }
                                            None => {
                                                tracing::error!("Invalid participant: {}", to);
                                                handle_err!(self.swarm.behaviour_mut().sig2coor.send_response(
                                                    channel,
                                                    SigToCoorResponse::SigningResponseEx(SigningResponseWrapEx::Failure(
                                                        "Invalid participant".to_string(),
                                                    )),
                                                ),"Error sending failure response to signer: {:?}");
                                            }
                                        }
                                    }
                                    TargetOrBroadcast::Broadcast => {
                                        for (identifier, peer_id) in
                                            request.base_info.participants.iter()
                                        {
                                            if *identifier != message_ex.from {
                                                match self.send_request_to_signer(
                                                    peer_id,
                                                    CoorToSigRequest::SigningRequestEx(
                                                        request_wrap.clone(),
                                                    ),
                                                ) {
                                                    Ok(_) => {
                                                        tracing::debug!("Successfully sent request to signer: {:?}", peer_id);
                                                    }
                                                    Err(e) => {
                                                        tracing::error!(
                                                            "Error sending request to signer: {}",
                                                            e
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                        handle_err!(
                                            self.swarm.behaviour_mut().sig2coor.send_response(
                                                channel,
                                                SigToCoorResponse::SigningResponseEx(
                                                    SigningResponseWrapEx::Success
                                                ),
                                            ),
                                            "Error sending success response to signer: {:?}"
                                        );
                                    }
                                }
                            }
                            crate::types::message::SigningStageEx::Final(_) => {
                                let sender = utils::new_oneshot_to_receive_success_or_error();
                                self.signing_in_final_channel_sender
                                    .send((request_wrap, sender))
                                    .unwrap();
                                if let Err(e) = self.swarm.behaviour_mut().sig2coor.send_response(
                                    channel,
                                    SigToCoorResponse::SigningResponseEx(
                                        SigningResponseWrapEx::Success,
                                    ),
                                ) {
                                    tracing::error!(
                                        "Error sending success response to signer: {:?}",
                                        e
                                    );
                                }
                            }
                        },
                        Err(e) => {
                            if let Err(e) = self.swarm.behaviour_mut().sig2coor.send_response(
                                channel,
                                SigToCoorResponse::SigningResponseEx(
                                    SigningResponseWrapEx::Failure(e.to_string()),
                                ),
                            ) {
                                tracing::error!(
                                    "Error sending success response to signer: {:?}",
                                    e
                                );
                            }
                        }
                    }
                }
            },
            other => {
                tracing::debug!("Unhandled {:?}", other);
            }
        }
        Ok(())
    }
    pub(crate) fn send_request_to_signer(
        &mut self,
        peer_id: &VI::Identity,
        request: CoorToSigRequest<VI::Identity>,
    ) -> Result<(), String> {
        let validator_peer = self
            .valid_validators
            .get(&peer_id)
            .ok_or(format!("Invalid participant: {:?}", peer_id))?;
        self.swarm
            .behaviour_mut()
            .coor2sig
            .send_request(&validator_peer.p2p_peer_id, request);
        Ok(())
    }
    // handle validator identity request
    pub(crate) async fn handle_vi_request(
        &mut self,
        peer: PeerId,
        request_id: InboundRequestId,
        request: ValidatorIdentityRequest,
        verify_whitelist: bool,
    ) -> Result<(), String> {
        tracing::debug!(
            "Received request from {:?} with request_id {:?}",
            peer,
            request_id
        );
        let ValidatorIdentityRequest {
            signature,
            public_key,
            nonce,
        } = request;
        // Reconstruct the hash that was signed by concatenating the same strings
        let public_key = VI::PublicKey::from_bytes(public_key)
            .map_err(|e| format!("Invalid public key: {}", e))?;
        let validator_peer = public_key.to_identity();
        if let Some(whitelist) = &self.signer_whitelist {
            if !whitelist.contains(&validator_peer) && verify_whitelist {
                tracing::warn!(
                    "Validator peerid {} is not in whitelist",
                    validator_peer.to_fmt_string()
                );
                return Err(format!(
                    "Validator peerid {} is not in whitelist",
                    validator_peer.to_fmt_string()
                ));
            }
        }
        let hash = list_hash(&[
            "register".as_bytes(),
            validator_peer.to_bytes().as_slice(),
            peer.to_bytes().as_slice(),
            self.p2p_keypair.public().to_peer_id().to_bytes().as_slice(),
        ]);
        // Verify the signature
        if public_key.verify(&hash, &signature) {
            if !verify_whitelist {
                return Ok(());
            }

            let address = self.p2ppeerid_2_endpoint.get(&peer).cloned();
            let new_validator = Validator {
                p2p_peer_id: peer,
                validator_peer_id: validator_peer.clone(),
                _validator_public_key: public_key,
                nonce,
                address,
            };

            let old_validator = self.valid_validators.get(&validator_peer).cloned();

            let should_insert = match &old_validator {
                Some(v) => v.nonce < nonce,
                None => true,
            };

            if should_insert {
                self.valid_validators
                    .insert(validator_peer.clone(), new_validator.clone());
            }

            if old_validator.is_none() {
                tracing::info!(
                    "Coordinator finds a new validator with identity: {}, p2p peer id : {}, total validators: {}",
                    validator_peer.to_fmt_string(),
                    peer,
                    self.valid_validators.len()
                );
            } else {
                tracing::info!(
                "Coordinator renews the validator with identity: {}, p2p peer id : {} => {}, total validators: {}",
                validator_peer.to_fmt_string(),
                old_validator.unwrap().p2p_peer_id.to_fmt_string(),
                peer,
                self.valid_validators.len()
            );
            }

            if let Some(addr) = self.p2ppeerid_2_endpoint.get(&peer) {
                self.swarm.add_peer_address(peer, addr.clone());
            }
            // update auto dkg
            if let Some(auto_dkg) = &self.auto_dkg {
                let min_signers = auto_dkg.read().await.min_signers;
                if let Some(participants) = auto_dkg
                    .write()
                    .await
                    .register_signer(validator_peer.clone())
                {
                    // note you cannot use read() here, because it will block the thread
                    tracing::info!(
                        "Coordinator starts auto DKG with {} participants and min signers {}",
                        participants.len(),
                        min_signers
                    );
                    for crypto_type in <CryptoType as strum::IntoEnumIterator>::iter() {
                        let manger_instruction_sender = self.instruction_sender.clone();
                        let participants = participants.clone();
                        let auto_dkg = auto_dkg.clone();
                        let base_path = self.base_path.clone();
                        tokio::spawn(async move {
                            loop {
                                let (instruction_sender, instruction_receiver) = oneshot::channel();
                                let instruction = Instruction::NewKey {
                                    crypto_type,
                                    participants: participants.clone(),
                                    min_signers: auto_dkg.read().await.min_signers,
                                    pkid_response_oneshot: instruction_sender,
                                };
                                manger_instruction_sender.send(instruction).unwrap();
                                let result = instruction_receiver.await;
                                match result {
                                    Ok(Ok(pkid)) => {
                                        if let (true, auto_dkg_value) = auto_dkg
                                            .write()
                                            .await
                                            .update_new_dkg_result(crypto_type, pkid)
                                        {
                                            tracing::info!("All DKGs are done");
                                            // save auto_dkg to base_path
                                            let auto_dkg_path = base_path.join("auto_dkg.json");
                                            let auto_dkg_file =
                                                File::create(auto_dkg_path).unwrap();
                                            let auto_dkg_writer = BufWriter::new(auto_dkg_file);
                                            serde_json::to_writer(auto_dkg_writer, &auto_dkg_value)
                                                .unwrap();
                                        }
                                        break;
                                    }
                                    Ok(Err(e)) => {
                                        tracing::error!("Error starting DKG: {}", e);
                                    }
                                    Err(e) => {
                                        tracing::error!("Error starting DKG: {}", e);
                                    }
                                }
                                tokio::time::sleep(Duration::from_secs(
                                    Settings::global().session.state_channel_retry_interval,
                                ))
                                .await;
                            }
                        });
                    }
                }
            }
            return Ok(());
        } else {
            tracing::error!(
                "Invalid signature from validator {}",
                validator_peer.to_fmt_string()
            );
            return Err(format!(
                "Invalid signature from validator {}",
                validator_peer.to_fmt_string()
            ));
        }
    }
    pub async fn handle_command(
        &mut self,
        command_result: Result<(UnixStream, SocketAddr), std::io::Error>,
    ) -> Result<(), anyhow::Error> {
        match command_result {
            Ok((stream, _addr)) => {
                tracing::debug!("IPC accept success");
                // Spawn a new task to handle the stream
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                let bytes_read = reader.read_line(&mut line).await?;
                let writer = reader.get_mut();
                if bytes_read == 0 {
                    return Ok(());
                }
                let command = Command::parse(&line);
                match command {
                    Command::PeerId => {
                        writer
                            .write_all(
                                self.p2p_keypair
                                    .public()
                                    .to_peer_id()
                                    .to_base58()
                                    .as_bytes(),
                            )
                            .await?;
                        writer.write_all(b"\n").await?;
                    }
                    Command::Help => {
                        reader
                            .get_mut()
                            .write_all(Command::help_text().as_bytes())
                            .await?;
                        reader.get_mut().write_all(b"\n").await?;
                    }
                    Command::ListSignerAddr => {
                        // dump the validators info
                        for validator in self.valid_validators.values() {
                            reader
                                .get_mut()
                                .write_all(format!("{:#?}", validator).as_bytes())
                                .await?;
                            reader.get_mut().write_all(b"\n").await?;
                        }
                    }
                    Command::Unknown(cmd) => {
                        let msg = format!("Unknown command: {}\n", cmd);
                        reader.get_mut().write_all(msg.as_bytes()).await?;
                        reader
                            .get_mut()
                            .write_all(Command::help_text().as_bytes())
                            .await?;
                        reader.get_mut().write_all(b"\n").await?;
                    }
                    Command::Dial(peer_id) => {
                        tracing::debug!("Dialing to peer {}", peer_id);
                        if let Ok(peer_id) = PeerId::from_str(&peer_id) {
                            if let Err(e) = self.swarm.dial(peer_id) {
                                tracing::error!("Error dialing to peer {}: {}", peer_id, e);
                            }
                        } else if let Ok(address) = Multiaddr::from_str(&peer_id) {
                            if let Err(e) = self.swarm.dial(address) {
                                tracing::error!("Error dialing to peer {}: {}", peer_id, e);
                            }
                        } else {
                            tracing::error!("Invalid peer id: {}", peer_id);
                        }
                    }
                    Command::Sign(pkid, msg, tweak_data) => {
                        tracing::debug!("Received sign request: {}", msg);
                        let pkid = PkId::from(pkid);
                        let (sender, receiver) = oneshot::channel();
                        self.instruction_sender
                            .send(Instruction::Sign {
                                pkid,
                                msg: msg.as_bytes().to_vec(),
                                tweak_data: tweak_data.map(|s| s.as_bytes().to_vec()),
                                signature_response_oneshot: sender,
                            })
                            .unwrap();
                        tokio::spawn(async move {
                            let result = receiver.await.unwrap();
                            match result {
                                Ok(signature) => {
                                    reader
                                        .get_mut()
                                        .write_all(signature.pretty_print_original().as_bytes())
                                        .await
                                        .unwrap();
                                    reader.get_mut().write_all(b"\n").await.unwrap();
                                }
                                Err(e) => {
                                    tracing::error!("Error signing: {}", e);
                                    reader
                                        .get_mut()
                                        .write_all(e.to_string().as_bytes())
                                        .await
                                        .unwrap();
                                    reader.get_mut().write_all(b"\n").await.unwrap();
                                }
                            }
                        });
                    }
                    Command::LoopSign(pkid, times) => {
                        let pkid = PkId::from(pkid);
                        let mut queue = Vec::new();
                        for _ in 0..times {
                            let (sender, receiver) = oneshot::channel();
                            //random generate msg
                            let msg: Vec<u8> = random_readable_string(10).as_bytes().to_vec();
                            let tweak_data: Option<Vec<u8>> =
                                Some(random_readable_string(10).as_bytes().to_vec());
                            queue.push(receiver);
                            self.instruction_sender
                                .send(Instruction::Sign {
                                    pkid: pkid.clone(),
                                    msg: msg.clone(),
                                    tweak_data: tweak_data.clone(),
                                    signature_response_oneshot: sender,
                                })
                                .unwrap();
                        }
                        let start = Instant::now();
                        tokio::spawn(async move {
                            for receiver in queue {
                                match receiver.await.unwrap() {
                                    Ok(signature) => {
                                        if let Err(e) = signature.try_verify() {
                                            reader
                                                .get_mut()
                                                .write_all(format!("Error: {:?}\n", e).as_bytes())
                                                .await
                                                .unwrap();
                                            reader
                                                .get_mut()
                                                .write_all(
                                                    signature.pretty_print_original().as_bytes(),
                                                )
                                                .await
                                                .unwrap();
                                        }
                                    }
                                    Err(e) => {
                                        reader
                                            .get_mut()
                                            .write_all(e.to_string().as_bytes())
                                            .await
                                            .unwrap();
                                    }
                                }
                            }
                            reader.get_mut().write_all(b"complete\n").await.unwrap();
                            let end = Instant::now();
                            let duration = end.duration_since(start);
                            reader
                                .get_mut()
                                .write_all(format!("Time: {:?}\n", duration).as_bytes())
                                .await
                                .unwrap();
                        });
                    }
                    Command::ListPkId => {
                        tracing::debug!("Received list pkid request");
                        let (sender, receiver) = oneshot::channel();
                        self.instruction_sender
                            .send(Instruction::ListPkIds {
                                list_pkids_response_oneshot: sender,
                            })
                            .unwrap();
                        let pkids = receiver.await.unwrap();
                        for (crypto_type, pkids) in pkids {
                            reader
                                .get_mut()
                                .write_all(format!("{:?}: {:?}\n", crypto_type, pkids).as_bytes())
                                .await?;
                        }
                    }
                    Command::StartDkg(min_signers, crypto_type) => {
                        let mut participants = Vec::new();
                        for (i, validator) in self.valid_validators.values().enumerate() {
                            tracing::debug!(
                                "Adding validator {} with peer id {}",
                                i + 1,
                                validator.validator_peer_id.to_fmt_string()
                            );
                            participants
                                .push(((i + 1) as u16, validator.validator_peer_id.clone()));
                        }
                        let (sender, receiver) = oneshot::channel();
                        self.instruction_sender
                            .send(Instruction::NewKey {
                                min_signers,
                                crypto_type,
                                participants,
                                pkid_response_oneshot: sender,
                            })
                            .unwrap();
                        tokio::spawn(async move {
                            let result = receiver.await.unwrap();
                            match result {
                                Ok(pkid) => {
                                    reader.get_mut().write_all(b"Success\n").await.unwrap();
                                    reader
                                        .get_mut()
                                        .write_all(pkid.to_string().as_bytes())
                                        .await
                                        .unwrap();
                                    reader.get_mut().write_all(b"\n").await.unwrap();
                                }
                                Err(e) => {
                                    let msg = format!("Error starting DKG: {}", e);
                                    tracing::debug!("{}", msg);
                                    reader.get_mut().write_all(msg.as_bytes()).await.unwrap();
                                    reader.get_mut().write_all(b"\n").await.unwrap();
                                }
                            }
                        });
                    }
                }
            }
            Err(e) => {
                tracing::error!("IPC accept error: {}", e);
            }
        };
        Ok(())
    }
    pub async fn start_ipc_listening(&mut self) -> anyhow::Result<UnixListener> {
        // Remove existing IPC socket file if it exists
        if self.ipc_path.exists() {
            std::fs::remove_file(&self.ipc_path)?;
        } else {
            std::fs::create_dir_all(
                self.ipc_path
                    .parent()
                    .ok_or(anyhow!("Failed to get parent dir"))?,
            )?;
        }

        let listener = UnixListener::bind(&self.ipc_path)?;
        tracing::info!("Coordinator IPC Listening on {}", self.ipc_path.display());
        return Ok(listener);
    }
}
