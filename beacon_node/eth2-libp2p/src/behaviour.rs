use crate::discovery::{enr::Eth2Enr, Discovery};
use crate::peer_manager::{PeerManager, PeerManagerEvent};
use crate::rpc::*;
use crate::types::{GossipEncoding, GossipKind, GossipTopic};
use crate::{error, Enr, NetworkConfig, NetworkGlobals, PubsubMessage, TopicHash};
use discv5::Discv5Event;
use futures::prelude::*;
use libp2p::{
    core::{identity::Keypair, ConnectedPoint},
    gossipsub::{Gossipsub, GossipsubEvent, MessageId},
    identify::{Identify, IdentifyEvent},
    swarm::{NetworkBehaviourAction, NetworkBehaviourEventProcess, PollParameters},
    NetworkBehaviour, PeerId,
};
use lru::LruCache;
use slog::{crit, debug, o, warn};
use std::{
    marker::PhantomData,
    sync::Arc,
    task::{Context, Poll},
};
use types::{EnrForkId, EthSpec, SubnetId};

const MAX_IDENTIFY_ADDRESSES: usize = 10;

/// Builds the network behaviour that manages the core protocols of eth2.
/// This core behaviour is managed by `Behaviour` which adds peer management to all core
/// behaviours.
#[derive(NetworkBehaviour)]
#[behaviour(out_event = "BehaviourEvent<TSpec>", poll_method = "poll")]
pub struct Behaviour<TSpec: EthSpec> {
    /// The routing pub-sub mechanism for eth2.
    gossipsub: Gossipsub,
    /// The Eth2 RPC specified in the wire-0 protocol.
    eth2_rpc: RPC<TSpec>,
    /// Keep regular connection to peers and disconnect if absent.
    // TODO: Using id for initial interop. This will be removed by mainnet.
    /// Provides IP addresses and peer information.
    identify: Identify,
    /// Discovery behaviour.
    discovery: Discovery<TSpec>,
    /// The peer manager that keeps track of peer's reputation and status.
    #[behaviour(ignore)]
    peer_manager: PeerManager<TSpec>,
    /// The events generated by this behaviour to be consumed in the swarm poll.
    #[behaviour(ignore)]
    events: Vec<BehaviourEvent<TSpec>>,
    /// The current meta data of the node, so respond to pings and get metadata
    #[behaviour(ignore)]
    meta_data: MetaData<TSpec>,
    /// A cache of recently seen gossip messages. This is used to filter out any possible
    /// duplicates that may still be seen over gossipsub.
    #[behaviour(ignore)]
    // TODO: Remove this
    seen_gossip_messages: LruCache<MessageId, ()>,
    /// A collections of variables accessible outside the network service.
    #[behaviour(ignore)]
    network_globals: Arc<NetworkGlobals<TSpec>>,
    #[behaviour(ignore)]
    /// Keeps track of the current EnrForkId for upgrading gossipsub topics.
    // NOTE: This can be accessed via the network_globals ENR. However we keep it here for quick
    // lookups for every gossipsub message send.
    enr_fork_id: EnrForkId,
    #[behaviour(ignore)]
    /// Logger for behaviour actions.
    log: slog::Logger,
}

/// Implements the combined behaviour for the libp2p service.
impl<TSpec: EthSpec> Behaviour<TSpec> {
    pub fn new(
        local_key: &Keypair,
        net_conf: &NetworkConfig,
        network_globals: Arc<NetworkGlobals<TSpec>>,
        log: &slog::Logger,
    ) -> error::Result<Self> {
        let local_peer_id = local_key.public().into_peer_id();
        let behaviour_log = log.new(o!());

        let identify = Identify::new(
            "lighthouse/libp2p".into(),
            version::version(),
            local_key.public(),
        );

        let enr_fork_id = network_globals
            .local_enr
            .read()
            .eth2()
            .expect("Local ENR must have a fork id");

        let attnets = network_globals
            .local_enr
            .read()
            .bitfield::<TSpec>()
            .expect("Local ENR must have subnet bitfield");

        let meta_data = MetaData {
            seq_number: 1,
            attnets,
        };

        Ok(Behaviour {
            eth2_rpc: RPC::new(log.clone()),
            gossipsub: Gossipsub::new(local_peer_id, net_conf.gs_config.clone()),
            discovery: Discovery::new(local_key, net_conf, network_globals.clone(), log)?,
            identify,
            peer_manager: PeerManager::new(network_globals.clone(), log),
            events: Vec::new(),
            seen_gossip_messages: LruCache::new(100_000),
            meta_data,
            network_globals,
            enr_fork_id,
            log: behaviour_log,
        })
    }

    /// Obtain a reference to the discovery protocol.
    pub fn discovery(&self) -> &Discovery<TSpec> {
        &self.discovery
    }

    /// Obtain a reference to the gossipsub protocol.
    pub fn gs(&self) -> &Gossipsub {
        &self.gossipsub
    }

    /* Pubsub behaviour functions */

    /// Subscribes to a gossipsub topic kind, letting the network service determine the
    /// encoding and fork version.
    pub fn subscribe_kind(&mut self, kind: GossipKind) -> bool {
        let gossip_topic = GossipTopic::new(
            kind,
            GossipEncoding::default(),
            self.enr_fork_id.fork_digest,
        );
        self.subscribe(gossip_topic)
    }

    /// Unsubscribes from a gossipsub topic kind, letting the network service determine the
    /// encoding and fork version.
    pub fn unsubscribe_kind(&mut self, kind: GossipKind) -> bool {
        let gossip_topic = GossipTopic::new(
            kind,
            GossipEncoding::default(),
            self.enr_fork_id.fork_digest,
        );
        self.unsubscribe(gossip_topic)
    }

    /// Subscribes to a specific subnet id;
    pub fn subscribe_to_subnet(&mut self, subnet_id: SubnetId) -> bool {
        let topic = GossipTopic::new(
            subnet_id.into(),
            GossipEncoding::default(),
            self.enr_fork_id.fork_digest,
        );
        self.subscribe(topic)
    }

    /// Un-Subscribes from a specific subnet id;
    pub fn unsubscribe_from_subnet(&mut self, subnet_id: SubnetId) -> bool {
        let topic = GossipTopic::new(
            subnet_id.into(),
            GossipEncoding::default(),
            self.enr_fork_id.fork_digest,
        );
        self.unsubscribe(topic)
    }

    /// Subscribes to a gossipsub topic.
    fn subscribe(&mut self, topic: GossipTopic) -> bool {
        // update the network globals
        self.network_globals
            .gossipsub_subscriptions
            .write()
            .insert(topic.clone());

        let topic_str: String = topic.clone().into();
        debug!(self.log, "Subscribed to topic"; "topic" => topic_str);
        self.gossipsub.subscribe(topic.into())
    }

    /// Unsubscribe from a gossipsub topic.
    fn unsubscribe(&mut self, topic: GossipTopic) -> bool {
        // update the network globals
        self.network_globals
            .gossipsub_subscriptions
            .write()
            .remove(&topic);
        // unsubscribe from the topic
        self.gossipsub.unsubscribe(topic.into())
    }

    /// Publishes a list of messages on the pubsub (gossipsub) behaviour, choosing the encoding.
    pub fn publish(&mut self, messages: Vec<PubsubMessage<TSpec>>) {
        for message in messages {
            for topic in message.topics(GossipEncoding::default(), self.enr_fork_id.fork_digest) {
                match message.encode(GossipEncoding::default()) {
                    Ok(message_data) => {
                        self.gossipsub.publish(&topic.into(), message_data);
                    }
                    Err(e) => crit!(self.log, "Could not publish message"; "error" => e),
                }
            }
        }
    }

    /// Forwards a message that is waiting in gossipsub's mcache. Messages are only propagated
    /// once validated by the beacon chain.
    pub fn propagate_message(&mut self, propagation_source: &PeerId, message_id: MessageId) {
        self.gossipsub
            .propagate_message(&message_id, propagation_source);
    }

    /* Eth2 RPC behaviour functions */

    /// Sends an RPC Request/Response via the RPC protocol.
    pub fn send_rpc(&mut self, peer_id: PeerId, rpc_event: RPCEvent<TSpec>) {
        self.eth2_rpc.send_rpc(peer_id, rpc_event);
    }

    /* Discovery / Peer management functions */

    /// Notify discovery that the peer has been banned.
    pub fn peer_banned(&mut self, peer_id: PeerId) {
        self.discovery.peer_banned(peer_id);
    }

    /// Notify discovery that the peer has been unbanned.
    pub fn peer_unbanned(&mut self, peer_id: &PeerId) {
        self.discovery.peer_unbanned(peer_id);
    }

    /// Returns an iterator over all enr entries in the DHT.
    pub fn enr_entries(&mut self) -> impl Iterator<Item = &Enr> {
        self.discovery.enr_entries()
    }

    /// Add an ENR to the routing table of the discovery mechanism.
    pub fn add_enr(&mut self, enr: Enr) {
        self.discovery.add_enr(enr);
    }

    /// Updates a subnet value to the ENR bitfield.
    ///
    /// The `value` is `true` if a subnet is being added and false otherwise.
    pub fn update_enr_subnet(&mut self, subnet_id: SubnetId, value: bool) {
        if let Err(e) = self.discovery.update_enr_bitfield(subnet_id, value) {
            crit!(self.log, "Could not update ENR bitfield"; "error" => e);
        }
        // update the local meta data which informs our peers of the update during PINGS
        self.update_metadata();
    }

    /// A request to search for peers connected to a long-lived subnet.
    pub fn peers_request(&mut self, subnet_id: SubnetId) {
        self.discovery.peers_request(subnet_id);
    }

    /// Updates the local ENR's "eth2" field with the latest EnrForkId.
    pub fn update_fork_version(&mut self, enr_fork_id: EnrForkId) {
        self.discovery.update_eth2_enr(enr_fork_id.clone());

        // unsubscribe from all gossip topics and re-subscribe to their new fork counterparts
        let subscribed_topics = self
            .network_globals
            .gossipsub_subscriptions
            .read()
            .iter()
            .cloned()
            .collect::<Vec<GossipTopic>>();

        //  unsubscribe from all topics
        for topic in &subscribed_topics {
            self.unsubscribe(topic.clone());
        }

        // re-subscribe modifying the fork version
        for mut topic in subscribed_topics {
            *topic.digest() = enr_fork_id.fork_digest;
            self.subscribe(topic);
        }

        // update the local reference
        self.enr_fork_id = enr_fork_id;
    }

    /* Private internal functions */

    /// Updates the current meta data of the node.
    fn update_metadata(&mut self) {
        self.meta_data.seq_number += 1;
        self.meta_data.attnets = self
            .discovery
            .local_enr()
            .bitfield::<TSpec>()
            .expect("Local discovery must have bitfield");
    }

    /// Sends a PING/PONG request/response to a peer.
    fn send_ping(&mut self, id: RequestId, peer_id: PeerId, is_request: bool) {
        let ping = crate::rpc::methods::Ping {
            data: self.meta_data.seq_number,
        };

        let event = if is_request {
            debug!(self.log, "Sending Ping"; "request_id" => id);
            RPCEvent::Request(id, RPCRequest::Ping(ping))
        } else {
            debug!(self.log, "Sending Pong"; "request_id" => id);
            RPCEvent::Response(id, RPCCodedResponse::Success(RPCResponse::Pong(ping)))
        };
        self.send_rpc(peer_id, event);
    }

    /// Sends a METADATA request to a peer.
    fn send_meta_data_request(&mut self, peer_id: PeerId) {
        let metadata_request =
            RPCEvent::Request(RequestId::from(0usize), RPCRequest::MetaData(PhantomData));
        self.send_rpc(peer_id, metadata_request);
    }

    /// Sends a METADATA response to a peer.
    fn send_meta_data_response(&mut self, id: RequestId, peer_id: PeerId) {
        let metadata_response = RPCEvent::Response(
            id,
            RPCCodedResponse::Success(RPCResponse::MetaData(self.meta_data.clone())),
        );
        self.send_rpc(peer_id, metadata_response);
    }

    // Temporary function until the behaviour is upgraded
    /// Notifies the behaviour that a peer has disconnected.
    pub fn notify_peer_disconnect(&mut self, peer_id: PeerId, _endpoint: ConnectedPoint) {
        self.peer_manager.notify_disconnect(&peer_id)
    }

    /// Notifies the behaviour that a peer has connected.
    pub fn notify_peer_connect(&mut self, peer_id: PeerId, endpoint: ConnectedPoint) {
        match endpoint {
            ConnectedPoint::Dialer { .. } => self.peer_manager.connect_outgoing(&peer_id),
            ConnectedPoint::Listener { .. } => self.peer_manager.connect_ingoing(&peer_id),
        };

        // Find ENR info about a peer if possible.
        if let Some(enr) = self.discovery.enr_of_peer(&peer_id) {
            let bitfield = match enr.bitfield::<TSpec>() {
                Ok(v) => v,
                Err(e) => {
                    warn!(self.log, "Peer has invalid ENR bitfield";
                                        "peer_id" => format!("{}", peer_id),
                                        "error" => format!("{:?}", e));
                    return;
                }
            };

            // use this as a baseline, until we get the actual meta-data
            let meta_data = MetaData {
                seq_number: 0,
                attnets: bitfield,
            };
            // TODO: Shift to the peer manager
            self.network_globals
                .peers
                .write()
                .add_metadata(&peer_id, meta_data);
        }
    }
}

// Implement the NetworkBehaviourEventProcess trait so that we can derive NetworkBehaviour for Behaviour
impl<TSpec: EthSpec> NetworkBehaviourEventProcess<GossipsubEvent> for Behaviour<TSpec> {
    fn inject_event(&mut self, event: GossipsubEvent) {
        match event {
            GossipsubEvent::Message(propagation_source, id, gs_msg) => {
                // Note: We are keeping track here of the peer that sent us the message, not the
                // peer that originally published the message.
                if self.seen_gossip_messages.put(id.clone(), ()).is_none() {
                    match PubsubMessage::decode(&gs_msg.topics, &gs_msg.data) {
                        Err(e) => {
                            debug!(self.log, "Could not decode gossipsub message"; "error" => format!("{}", e))
                        }
                        Ok(msg) => {
                            // if this message isn't a duplicate, notify the network
                            self.events.push(BehaviourEvent::PubsubMessage {
                                id,
                                source: propagation_source,
                                topics: gs_msg.topics,
                                message: msg,
                            });
                        }
                    }
                } else {
                    match PubsubMessage::<TSpec>::decode(&gs_msg.topics, &gs_msg.data) {
                        Err(e) => {
                            debug!(self.log, "Could not decode gossipsub message"; "error" => format!("{}", e))
                        }
                        Ok(msg) => {
                            debug!(self.log, "A duplicate gossipsub message was received"; "message_source" => format!("{}", gs_msg.source), "propagated_peer" => format!("{}",propagation_source), "message" => format!("{}", msg));
                        }
                    }
                }
            }
            GossipsubEvent::Subscribed { peer_id, topic } => {
                self.events
                    .push(BehaviourEvent::PeerSubscribed(peer_id, topic));
            }
            GossipsubEvent::Unsubscribed { .. } => {}
        }
    }
}

impl<TSpec: EthSpec> NetworkBehaviourEventProcess<RPCMessage<TSpec>> for Behaviour<TSpec> {
    fn inject_event(&mut self, message: RPCMessage<TSpec>) {
        let peer_id = message.peer_id;
        // The METADATA and PING RPC responses are handled within the behaviour and not
        // propagated
        // TODO: Improve the RPC types to better handle this logic discrepancy
        match message.event {
            RPCEvent::Request(id, RPCRequest::Ping(ping)) => {
                // inform the peer manager and send the response
                self.peer_manager.ping_request(&peer_id, ping.data);
                // send a ping response
                self.send_ping(id, peer_id, false);
            }
            RPCEvent::Request(id, RPCRequest::MetaData(_)) => {
                // send the requested meta-data
                self.send_meta_data_response(id, peer_id);
            }
            RPCEvent::Response(_, RPCCodedResponse::Success(RPCResponse::Pong(ping))) => {
                self.peer_manager.pong_response(&peer_id, ping.data);
            }
            RPCEvent::Response(_, RPCCodedResponse::Success(RPCResponse::MetaData(meta_data))) => {
                self.peer_manager.meta_data_response(&peer_id, meta_data);
            }
            RPCEvent::Request(_, RPCRequest::Status(_))
            | RPCEvent::Response(_, RPCCodedResponse::Success(RPCResponse::Status(_))) => {
                // inform the peer manager that we have received a status from a peer
                self.peer_manager.peer_statusd(&peer_id);
                // propagate the STATUS message upwards
                self.events
                    .push(BehaviourEvent::RPC(peer_id, message.event));
            }
            RPCEvent::Error(_, protocol, ref err) => {
                self.peer_manager.handle_rpc_error(&peer_id, protocol, err);
                self.events
                    .push(BehaviourEvent::RPC(peer_id, message.event));
            }
            _ => {
                // propagate all other RPC messages upwards
                self.events
                    .push(BehaviourEvent::RPC(peer_id, message.event))
            }
        }
    }
}

impl<TSpec: EthSpec> Behaviour<TSpec> {
    /// Consumes the events list when polled.
    fn poll<TBehaviourIn>(
        &mut self,
        cx: &mut Context,
        _: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<TBehaviourIn, BehaviourEvent<TSpec>>> {
        // check the peer manager for events
        loop {
            match self.peer_manager.poll_next_unpin(cx) {
                Poll::Ready(Some(event)) => match event {
                    PeerManagerEvent::Status(peer_id) => {
                        // it's time to status. We don't keep a beacon chain reference here, so we inform
                        // the network to send a status to this peer
                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(
                            BehaviourEvent::StatusPeer(peer_id),
                        ));
                    }
                    PeerManagerEvent::Ping(peer_id) => {
                        // send a ping request to this peer
                        self.send_ping(RequestId::from(0usize), peer_id, true);
                    }
                    PeerManagerEvent::MetaData(peer_id) => {
                        self.send_meta_data_request(peer_id);
                    }
                    PeerManagerEvent::_DisconnectPeer(_peer_id) => {
                        //TODO: Implement
                    }
                    PeerManagerEvent::_BanPeer(_peer_id) => {
                        //TODO: Implement
                    }
                },
                Poll::Pending => break,
                Poll::Ready(None) => break, // peer manager ended
            }
        }

        if !self.events.is_empty() {
            return Poll::Ready(NetworkBehaviourAction::GenerateEvent(self.events.remove(0)));
        }

        Poll::Pending
    }
}

impl<TSpec: EthSpec> NetworkBehaviourEventProcess<IdentifyEvent> for Behaviour<TSpec> {
    fn inject_event(&mut self, event: IdentifyEvent) {
        match event {
            IdentifyEvent::Received {
                peer_id,
                mut info,
                observed_addr,
            } => {
                if info.listen_addrs.len() > MAX_IDENTIFY_ADDRESSES {
                    debug!(
                        self.log,
                        "More than 10 addresses have been identified, truncating"
                    );
                    info.listen_addrs.truncate(MAX_IDENTIFY_ADDRESSES);
                }
                // send peer info to the peer manager.
                self.peer_manager.identify(&peer_id, &info);

                debug!(self.log, "Identified Peer"; "peer" => format!("{}", peer_id),
                "protocol_version" => info.protocol_version,
                "agent_version" => info.agent_version,
                "listening_ addresses" => format!("{:?}", info.listen_addrs),
                "observed_address" => format!("{:?}", observed_addr),
                "protocols" => format!("{:?}", info.protocols)
                );
            }
            IdentifyEvent::Sent { .. } => {}
            IdentifyEvent::Error { .. } => {}
        }
    }
}

impl<TSpec: EthSpec> NetworkBehaviourEventProcess<Discv5Event> for Behaviour<TSpec> {
    fn inject_event(&mut self, _event: Discv5Event) {
        // discv5 has no events to inject
    }
}

/// The types of events than can be obtained from polling the behaviour.
#[derive(Debug)]
pub enum BehaviourEvent<TSpec: EthSpec> {
    /// A received RPC event and the peer that it was received from.
    RPC(PeerId, RPCEvent<TSpec>),
    PubsubMessage {
        /// The gossipsub message id. Used when propagating blocks after validation.
        id: MessageId,
        /// The peer from which we received this message, not the peer that published it.
        source: PeerId,
        /// The topics that this message was sent on.
        topics: Vec<TopicHash>,
        /// The message itself.
        message: PubsubMessage<TSpec>,
    },
    /// Subscribed to peer for given topic
    PeerSubscribed(PeerId, TopicHash),
    /// Inform the network to send a Status to this peer.
    StatusPeer(PeerId),
}
