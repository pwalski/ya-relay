use std::collections::{HashMap, HashSet};
use std::convert::{TryFrom, TryInto};
use std::net::{Ipv6Addr, SocketAddr};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use derive_more::From;
use ethsign::PublicKey;
use futures::channel::mpsc;
use futures::future::LocalBoxFuture;
use futures::{FutureExt, SinkExt, StreamExt};
use tokio::sync::RwLock;
use url::Url;

use ya_client_model::NodeId;
use ya_net_stack::interface::*;
use ya_net_stack::smoltcp::iface::Route;
use ya_net_stack::smoltcp::wire::{IpAddress, IpCidr, IpEndpoint};
use ya_net_stack::socket::{SocketEndpoint, TCP_CONN_TIMEOUT};
use ya_net_stack::{Channel, IngressEvent, Network, Protocol, Stack};
use ya_relay_proto::codec;
use ya_relay_proto::proto::{self, Forward, RequestId, SlotId};

use crate::crypto::{Crypto, CryptoProvider, FallbackCryptoProvider};
use crate::server::Server;
use crate::testing::dispatch::{dispatch, Dispatched, Dispatcher, Handler};
use crate::testing::session::{Session, StartingSessions};
use crate::udp_stream::{udp_bind, OutStream};
use crate::{parse_udp_url, SessionId};

pub type ForwardSender = mpsc::Sender<Vec<u8>>;
pub type ForwardReceiver = tokio::sync::mpsc::UnboundedReceiver<Forwarded>;

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_millis(3000);
const NEIGHBOURHOOD_TTL: Duration = Duration::from_secs(300);

const TCP_BIND_PORT: u16 = 1;
const IPV6_DEFAULT_CIDR: u8 = 0;

#[derive(Clone)]
pub struct Client {
    pub(crate) state: Arc<RwLock<ClientState>>,
    net: Network,
}

pub(crate) struct ClientState {
    pub(crate) config: ClientConfig,
    pub(crate) sink: Option<OutStream>,
    pub(crate) bind_addr: Option<SocketAddr>,

    /// If address is None after registering endpoints on Server, that means
    /// we don't have public IP.
    pub(crate) public_addr: Option<SocketAddr>,

    pub(crate) starting_sessions: Option<StartingSessions>,

    pub(crate) p2p_sessions: HashMap<NodeId, Session>,
    pub(crate) sessions: HashMap<SocketAddr, Session>,

    pub(crate) responses: HashMap<SocketAddr, Dispatcher>,
    pub(crate) slots: HashMap<SocketAddr, HashSet<SlotId>>,
    pub(crate) neighbours: Option<Neighbourhood>,
    pub(crate) forward_unreliable: HashMap<(NodeId, SocketAddr), ForwardSender>,

    pub(crate) virt_ingress: Channel<Forwarded>,
    pub(crate) virt_nodes: HashMap<Box<[u8]>, VirtNode>,
    pub(crate) virt_ips: HashMap<(SlotId, SocketAddr), Box<[u8]>>,
}

impl ClientState {
    fn new(config: ClientConfig) -> Self {
        Self {
            config,
            sink: Default::default(),
            bind_addr: Default::default(),
            public_addr: None,
            starting_sessions: None,
            p2p_sessions: Default::default(),
            sessions: Default::default(),
            responses: Default::default(),
            slots: Default::default(),
            neighbours: Default::default(),
            forward_unreliable: Default::default(),
            virt_ingress: Default::default(),
            virt_nodes: Default::default(),
            virt_ips: Default::default(),
        }
    }

    fn reset(&mut self) {
        *self = Self::new(self.config.clone());
    }
}

#[derive(Clone)]
pub struct ClientConfig {
    pub node_id: NodeId,
    pub node_pub_key: PublicKey,
    pub crypto: Rc<dyn CryptoProvider>,
    pub bind_url: Url,
    pub srv_addr: SocketAddr,
    pub auto_connect: bool,
}

pub struct ClientBuilder {
    bind_url: Option<Url>,
    srv_url: Url,
    crypto: Option<Rc<dyn CryptoProvider>>,
    auto_connect: bool,
}

impl ClientBuilder {
    pub fn from_server(server: &Server) -> ClientBuilder {
        let url = { server.inner.url.clone() };
        ClientBuilder::from_url(url)
    }

    pub fn from_url(url: Url) -> ClientBuilder {
        ClientBuilder {
            bind_url: None,
            srv_url: url,
            crypto: None,
            auto_connect: false,
        }
    }

    pub fn crypto(mut self, provider: impl CryptoProvider + 'static) -> ClientBuilder {
        self.crypto = Some(Rc::new(provider));
        self
    }

    pub fn connect(mut self) -> ClientBuilder {
        self.auto_connect = true;
        self
    }

    pub async fn build(self) -> anyhow::Result<Client> {
        let bind_url = self
            .bind_url
            .unwrap_or_else(|| Url::parse("udp://0.0.0.0:0").unwrap());
        let crypto = self
            .crypto
            .unwrap_or_else(|| Rc::new(FallbackCryptoProvider::default()));

        let default_id = crypto.default_id().await?;
        let default_pub_key = crypto.get(default_id).await?.public_key().await?;

        let mut client = Client::new(ClientConfig {
            node_id: default_id,
            node_pub_key: default_pub_key,
            crypto,
            bind_url,
            srv_addr: parse_udp_url(&self.srv_url)?.parse()?,
            auto_connect: self.auto_connect,
        });

        client.spawn().await?;

        Ok(client)
    }
}

impl Client {
    fn new(config: ClientConfig) -> Self {
        let stack = default_network(config.node_pub_key.clone());
        let state = Arc::new(RwLock::new(ClientState::new(config)));
        Self { state, net: stack }
    }

    pub fn id(&self) -> String {
        self.net.name.as_ref().clone()
    }

    pub async fn node_id(&self) -> NodeId {
        let state = self.state.read().await;
        state.config.node_id
    }

    pub async fn bind_addr(&self) -> anyhow::Result<SocketAddr> {
        self.state
            .read()
            .await
            .bind_addr
            .ok_or_else(|| anyhow!("client not started"))
    }

    pub async fn public_addr(&self) -> Option<SocketAddr> {
        self.state.read().await.public_addr
    }

    pub async fn crypto(&self) -> anyhow::Result<Rc<dyn Crypto>> {
        let state = self.state.read().await;
        let default_id = { state.config.crypto.default_id() }.await?;
        Ok(state.config.crypto.get(default_id).await?)
    }

    pub async fn forward_receiver(&self) -> Option<ForwardReceiver> {
        let state = self.state.read().await;
        state.virt_ingress.receiver()
    }

    async fn spawn(&mut self) -> anyhow::Result<()> {
        log::debug!("[{}] starting...", self.id());

        let (stream, auto_connect) = {
            let bind_url = {
                let mut state = self.state.write().await;
                state.reset();
                state.config.bind_url.clone()
            };

            let (stream, sink, bind_addr) = udp_bind(&bind_url).await?;
            let mut state = self.state.write().await;
            state.sink = Some(sink);
            state.starting_sessions = Some(StartingSessions::new(self.clone()));
            state.bind_addr = Some(bind_addr);
            (stream, state.config.auto_connect)
        };

        let node_id = self.node_id().await;
        let virt_endpoint: IpEndpoint = (to_ipv6(&node_id), TCP_BIND_PORT).into();

        self.net.spawn_local();
        self.net.bind(Protocol::Tcp, virt_endpoint)?;

        self.spawn_ingress_router()?;
        self.spawn_egress_router()?;

        tokio::task::spawn_local(dispatch(self.clone(), stream));

        if auto_connect {
            let session = self.server_session().await?;
            let endpoints = session.register_endpoints(vec![]).await?;

            // If there is any (correct) endpoint on the list, that means we have public IP.
            match endpoints
                .into_iter()
                .find_map(|endpoint| endpoint.try_into().ok())
            {
                Some(addr) => self.state.write().await.public_addr = Some(addr),
                None => self.state.write().await.public_addr = None,
            }
        }

        log::debug!("[{}] started", self.id());
        Ok(())
    }

    fn spawn_ingress_router(&self) -> anyhow::Result<()> {
        let ingress_rx = self
            .net
            .ingress_receiver()
            .ok_or_else(|| anyhow::anyhow!("ingress traffic router already spawned"))?;

        let client = self.clone();
        tokio::task::spawn_local(ingress_rx.for_each(move |event| {
            let client = client.clone();
            async move {
                let (desc, payload) = match event {
                    IngressEvent::InboundConnection { desc } => {
                        log::trace!(
                            "[{}] ingress router: new connection from {:?} to {:?} ",
                            client.id(),
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Disconnected { desc } => {
                        log::trace!(
                            "[{}] ingress router: ({}) {:?} disconnected from {:?}",
                            client.id(),
                            desc.protocol,
                            desc.remote,
                            desc.local,
                        );
                        return;
                    }
                    IngressEvent::Packet { desc, payload, .. } => (desc, payload),
                };

                if desc.protocol != Protocol::Tcp {
                    log::trace!(
                        "[{}] ingress router: dropping {} payload",
                        client.id(),
                        desc.protocol
                    );
                    return;
                }

                let remote_address = match desc.remote {
                    SocketEndpoint::Ip(endpoint) => endpoint.addr,
                    _ => {
                        log::trace!(
                            "[{}] ingress router: remote endpoint {:?} is not supported",
                            client.id(),
                            desc.remote
                        );
                        return;
                    }
                };

                match {
                    // nodes are populated via `Client::on_forward` and `Client::forward`
                    let state = client.state.read().await;
                    state
                        .virt_nodes
                        .get(remote_address.as_bytes())
                        .map(|node| (node.id, state.virt_ingress.tx.clone()))
                } {
                    Some((node_id, tx)) => {
                        let payload = Forwarded {
                            reliable: true,
                            node_id,
                            payload,
                        };

                        let payload_len = payload.payload.len();

                        if tx.send(payload).is_err() {
                            log::trace!(
                                "[{}] ingress router: ingress handler closed for node {}",
                                client.id(),
                                node_id
                            );
                        } else {
                            log::trace!(
                                "[{}] ingress router: forwarded {} B",
                                client.id(),
                                payload_len
                            );
                        }
                    }
                    _ => log::trace!(
                        "[{}] ingress router: unknown remote address {}",
                        client.id(),
                        remote_address
                    ),
                };
            }
        }));

        Ok(())
    }

    fn spawn_egress_router(&self) -> anyhow::Result<()> {
        let egress_rx = self
            .net
            .egress_receiver()
            .ok_or_else(|| anyhow::anyhow!("egress traffic router already spawned"))?;

        let client = self.clone();
        tokio::task::spawn_local(egress_rx.for_each(move |egress| {
            let client = client.clone();
            async move {
                let node = match {
                    let state = client.state.read().await;
                    state.virt_nodes.get(&egress.remote).cloned()
                } {
                    Some(node) => node,
                    None => {
                        log::trace!(
                            "[{}] egress router: unknown address {:02x?}",
                            client.id(),
                            egress.remote
                        );
                        return;
                    }
                };

                let forward = Forward::new(node.session_id, node.session_slot, egress.payload);
                if let Err(error) = client.send(forward, node.session_addr).await {
                    log::trace!(
                        "[{}] egress router: forward to {} failed: {}",
                        client.id(),
                        node.session_addr,
                        error
                    );
                }
            }
        }));

        Ok(())
    }

    async fn resolve_node(&self, node_id: NodeId, addr: SocketAddr) -> anyhow::Result<VirtNode> {
        let ip = to_ipv6(node_id);
        match self.get_node(&ip.octets()).await {
            Some(node) => Ok(node),
            None => match async {
                let session = self.session(addr).await?;
                session.find_node(node_id).await?;
                Ok::<_, anyhow::Error>(self.get_node(&ip.octets()).await)
            }
            .await
            {
                Ok(Some(node)) => Ok(node),
                Ok(None) => anyhow::bail!("empty node response"),
                Err(err) => anyhow::bail!("node resolution error: {}", err),
            },
        }
    }

    async fn resolve_slot(&self, slot: SlotId, addr: SocketAddr) -> anyhow::Result<VirtNode> {
        match self.get_slot(slot, addr).await {
            Some(node) => Ok(node),
            None => match async {
                let session = self.session(addr).await?;
                session.find_slot(slot).await?;
                Ok::<_, anyhow::Error>(self.get_slot(slot, addr).await)
            }
            .await
            {
                Ok(Some(node)) => Ok(node),
                Ok(None) => anyhow::bail!("empty node response"),
                Err(err) => anyhow::bail!("slot resolution error: {}", err),
            },
        }
    }

    async fn get_node(&self, ip: &[u8]) -> Option<VirtNode> {
        let state = self.state.read().await;
        state.virt_nodes.get(ip).cloned()
    }

    async fn get_slot(&self, slot: SlotId, addr: SocketAddr) -> Option<VirtNode> {
        let state = self.state.read().await;
        state
            .virt_ips
            .get(&(slot, addr))
            .map(|ip| state.virt_nodes.get(ip).cloned())
            .flatten()
    }

    async fn remove_slot(&self, slot: SlotId, addr: SocketAddr) {
        let mut state = self.state.write().await;

        if let Some(slots) = state.slots.get_mut(&addr) {
            slots.remove(&slot);

            if let Some(ip) = state.virt_ips.remove(&(slot, addr)) {
                if let Some(node) = state.virt_nodes.remove(&ip) {
                    state.forward_unreliable.remove(&(node.id, addr));
                }
            }
        }
    }
}

impl Client {
    pub(crate) async fn find_node(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        node_id: NodeId,
    ) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Node {
            node_id: node_id.into_array().to_vec(),
            public_key: true,
        };
        self.find_node_by(addr, session_id, packet).await
    }

    pub(crate) async fn find_slot(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        slot: SlotId,
    ) -> anyhow::Result<proto::response::Node> {
        let packet = proto::request::Slot {
            slot,
            public_key: true,
        };
        self.find_node_by(addr, session_id, packet).await
    }

    async fn find_node_by(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        packet: impl Into<proto::Request>,
    ) -> anyhow::Result<proto::response::Node> {
        let response = self
            .request::<proto::response::Node>(
                packet.into(),
                session_id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        self.add_virt_node(addr, session_id, &response).await?;
        Ok(response)
    }

    async fn add_virt_node(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        packet: &proto::response::Node,
    ) -> anyhow::Result<()> {
        // If node has public IP, we can establish direct session with him
        // instead of forwarding messages through relay.
        let (addr, session_id) = match self
            .try_direct_session(packet)
            .await
            .map_err(|e| log::info!("{}", e))
        {
            Ok(session) => (session.remote_addr, session.id().await?),
            Err(_) => (addr, session_id),
        };

        let node = VirtNode::try_new(&packet.node_id, session_id, addr, packet.slot)?;
        {
            let mut state = self.state.write().await;
            let ip: Box<[u8]> = node.endpoint.addr.as_bytes().into();

            state.virt_nodes.insert(ip.clone(), node);
            state
                .virt_ips
                .insert((node.session_slot, node.session_addr), ip);
            state
                .slots
                .entry(node.session_addr)
                .or_default()
                .insert(node.session_slot);
        }
        Ok(())
    }

    pub(crate) async fn neighbours(
        &self,
        addr: SocketAddr,
        session_id: SessionId,
        count: u32,
    ) -> anyhow::Result<proto::response::Neighbours> {
        if let Some(neighbours) = {
            let state = self.state.read().await;
            state.neighbours.clone()
        } {
            if neighbours.response.nodes.len() as u32 >= count
                && neighbours.updated + NEIGHBOURHOOD_TTL > Instant::now()
            {
                return Ok(neighbours.response);
            }
        }

        let packet = proto::request::Neighbours {
            count,
            public_key: true,
        };
        let response = self
            .request::<proto::response::Neighbours>(
                packet.into(),
                session_id.to_vec(),
                DEFAULT_REQUEST_TIMEOUT,
                addr,
            )
            .await?
            .packet;

        for node in &response.nodes {
            self.add_virt_node(addr, session_id, node).await?;
        }

        {
            let mut state = self.state.write().await;
            state.neighbours.replace(Neighbourhood {
                updated: Instant::now(),
                response: response.clone(),
            });
        }

        Ok(response)
    }

    pub(crate) async fn ping(&self, addr: SocketAddr, session_id: SessionId) -> anyhow::Result<()> {
        let packet = proto::request::Ping {};
        self.request::<proto::response::Pong>(
            packet.into(),
            session_id.to_vec(),
            DEFAULT_REQUEST_TIMEOUT,
            addr,
        )
        .await?;

        Ok(())
    }

    pub(crate) async fn forward(
        &self,
        session_addr: SocketAddr,
        forward_id: impl Into<ForwardId>,
    ) -> anyhow::Result<ForwardSender> {
        let node = match forward_id.into() {
            ForwardId::NodeId(node_id) => self.resolve_node(node_id, session_addr).await?,
            ForwardId::SlotId(slot) => self.resolve_slot(slot, session_addr).await?,
        };
        let connection = self.net.connect(node.endpoint, TCP_CONN_TIMEOUT).await?;

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(1);
        let client = self.clone();
        let id = client.id();

        tokio::task::spawn_local(async move {
            log::trace!("forwarding messages to {:?}", node);

            while let Some(payload) = rx.next().await {
                let _ = client
                    .net
                    .send(payload, connection)
                    .unwrap_or_else(|e| Box::pin(futures::future::err(e)))
                    .await
                    .map_err(|e| {
                        log::warn!("[{}] unable to forward via {}: {}", id, session_addr, e)
                    });
            }

            client.remove_slot(node.session_slot, session_addr).await;
            rx.close();

            log::trace!(
                "[{}] forward: disconnected from server: {}",
                id,
                session_addr
            );
        });

        Ok(tx)
    }

    pub(crate) async fn forward_unreliable(
        &self,
        session_addr: SocketAddr,
        forward_id: impl Into<ForwardId>,
    ) -> anyhow::Result<ForwardSender> {
        let node = match forward_id.into() {
            ForwardId::NodeId(node_id) => self.resolve_node(node_id, session_addr).await?,
            ForwardId::SlotId(slot) => self.resolve_slot(slot, session_addr).await?,
        };

        let (tx, mut rx) = {
            let mut state = self.state.write().await;
            match state.forward_unreliable.get(&(node.id, session_addr)) {
                Some(tx) => return Ok(tx.clone()),
                None => {
                    let (tx, rx) = mpsc::channel(1);
                    state
                        .forward_unreliable
                        .insert((node.id, session_addr), tx.clone());
                    (tx, rx)
                }
            }
        };

        let client = self.clone();
        tokio::task::spawn_local(async move {
            while let Some(payload) = rx.next().await {
                log::trace!("forwarding message (U) to {:?}", node);

                let forward = Forward::unreliable(node.session_id, node.session_slot, payload);
                if let Err(error) = client.send(forward, session_addr).await {
                    log::trace!(
                        "[{}] forward (U) to {} failed: {}",
                        client.id(),
                        node.session_addr,
                        error
                    );
                }
            }

            client.remove_slot(node.session_slot, session_addr).await;
            rx.close();

            log::trace!(
                "[{}] forward (U): disconnected from server: {}",
                node.id,
                session_addr
            );
        });

        Ok(tx)
    }

    pub(crate) async fn broadcast(
        &self,
        session_addr: SocketAddr,
        session_id: SessionId,
        data: Vec<u8>,
        count: u32,
    ) -> anyhow::Result<()> {
        let response = self.neighbours(session_addr, session_id, count).await?;
        let node_ids = response
            .nodes
            .into_iter()
            .filter_map(|n| NodeId::try_from(n.node_id.as_slice()).ok())
            .collect::<Vec<_>>();

        log::debug!("broadcasting message to {} node(s)", node_ids.len());

        for node_id in node_ids {
            let data = data.clone();
            let session = self.optimal_session(node_id).await?;

            tokio::task::spawn_local(async move {
                log::trace!("broadcasting message to {}", node_id);

                match session.forward_unreliable(node_id).await {
                    Ok(mut forward) => {
                        if forward.send(data).await.is_err() {
                            log::debug!("cannot broadcast to {}: channel closed", node_id);
                        }
                    }
                    Err(e) => {
                        log::debug!("cannot broadcast to {}: channel error: {}", node_id, e);
                    }
                }
            });
        }

        Ok(())
    }
}

impl Client {
    pub(crate) async fn request<T>(
        &self,
        request: proto::Request,
        session_id: Vec<u8>,
        timeout: Duration,
        addr: SocketAddr,
    ) -> anyhow::Result<Dispatched<T>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let response = self.response::<T>(request.request_id, timeout, addr).await;
        let packet = proto::Packet {
            session_id,
            kind: Some(proto::packet::Kind::Request(request)),
        };
        self.send(packet, addr).await?;

        Ok(response.await?)
    }

    #[inline(always)]
    async fn response<'a, T>(
        &self,
        request_id: RequestId,
        timeout: Duration,
        addr: SocketAddr,
    ) -> LocalBoxFuture<'a, anyhow::Result<Dispatched<T>>>
    where
        proto::response::Kind: TryInto<T, Error = ()>,
        T: 'static,
    {
        let dispatcher = {
            let mut state = self.state.write().await;
            (*state).responses.entry(addr).or_default().clone()
        };
        dispatcher.response::<T>(request_id, timeout)
    }

    pub(crate) async fn send(
        &self,
        packet: impl Into<codec::PacketKind>,
        addr: SocketAddr,
    ) -> anyhow::Result<()> {
        let mut sink = {
            let state = self.state.read().await;
            match state.sink {
                Some(ref sink) => sink.clone(),
                None => bail!("Not connected"),
            }
        };
        Ok(sink.send((packet.into(), addr)).await?)
    }
}

impl Handler for Client {
    fn dispatcher(&self, from: SocketAddr) -> LocalBoxFuture<Option<Dispatcher>> {
        let handler = self.clone();
        async move {
            let state = handler.state.read().await;
            state.responses.get(&from).cloned()
        }
        .boxed_local()
    }

    fn on_control(
        &self,
        _session_id: Vec<u8>,
        control: proto::Control,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!("received control packet from {}: {:?}", from, control);
        Box::pin(futures::future::ready(()))
    }

    fn on_request(
        &self,
        session_id: Vec<u8>,
        request: proto::Request,
        from: SocketAddr,
    ) -> LocalBoxFuture<()> {
        log::debug!("received request packet from {}: {:?}", from, request);

        let (request_id, kind) = match request {
            proto::Request {
                request_id,
                kind: Some(kind),
            } => (request_id, kind),
            _ => return Box::pin(futures::future::ready(())),
        };

        match kind {
            proto::request::Kind::Ping(_) => {
                let packet = proto::Packet::response(
                    request_id,
                    session_id,
                    proto::StatusCode::Ok,
                    proto::response::Pong {},
                );

                let client = self.clone();
                async move {
                    if let Err(e) = client.send(packet, from).await {
                        log::warn!("unable to send Pong to {}: {}", from, e);
                    }
                }
                .boxed_local()
            }
            proto::request::Kind::Session(request) => {
                Box::pin(self.dispatch_session(session_id, request_id, from, request))
            }

            _ => Box::pin(futures::future::ready(())),
        }
    }

    fn on_forward(&self, forward: proto::Forward, from: SocketAddr) -> LocalBoxFuture<()> {
        let client = self.clone();
        let fut = async move {
            log::trace!(
                "[{}] received forward packet ({} B) via {}",
                client.id(),
                forward.payload.len(),
                from
            );

            let node = match client.resolve_slot(forward.slot, from).await {
                Ok(node) => node,
                Err(err) => {
                    log::error!("[{}] on forward error: {}", client.id(), err);
                    return;
                }
            };

            if forward.is_reliable() {
                client.net.receive(forward.payload.into_vec());
                client.net.poll();
            } else {
                let tx = {
                    let state = client.state.read().await;
                    state.virt_ingress.tx.clone()
                };

                let payload = Forwarded {
                    reliable: false,
                    node_id: node.id,
                    payload: forward.payload.into_vec(),
                };

                if tx.send(payload).is_err() {
                    log::trace!(
                        "[{}] ingress router: ingress handler closed for node {}",
                        client.id(),
                        node.id
                    );
                }
            }
        };

        tokio::task::spawn_local(fut);
        Box::pin(futures::future::ready(()))
    }
}

#[derive(Copy, Clone, Debug)]
pub struct VirtNode {
    pub(crate) id: NodeId,
    endpoint: IpEndpoint,
    session_id: SessionId,
    session_addr: SocketAddr,
    session_slot: SlotId,
}

impl VirtNode {
    pub fn try_new(
        id: &[u8],
        session_id: SessionId,
        session_addr: SocketAddr,
        session_slot: SlotId,
    ) -> anyhow::Result<Self> {
        let default_id = NodeId::default();
        if id.len() != default_id.as_ref().len() {
            anyhow::bail!("invalid NodeId");
        }

        let id = NodeId::from(id);
        let ip = IpAddress::from(to_ipv6(&id));
        let endpoint = (ip, TCP_BIND_PORT).into();

        Ok(Self {
            id,
            endpoint,
            session_id,
            session_addr,
            session_slot,
        })
    }
}

#[derive(Clone)]
pub(crate) struct Neighbourhood {
    updated: Instant,
    response: proto::response::Neighbours,
}

#[derive(Clone, Debug)]
pub struct Forwarded {
    pub reliable: bool,
    pub node_id: NodeId,
    pub payload: Vec<u8>,
}

#[derive(From)]
pub enum ForwardId {
    SlotId(SlotId),
    NodeId(NodeId),
}

fn default_network(key: PublicKey) -> Network {
    let address = key.address();
    let ipv6_addr = to_ipv6(address);
    let ipv6_cidr = IpCidr::new(IpAddress::from(ipv6_addr), IPV6_DEFAULT_CIDR);
    let mut iface = default_iface();

    let name = format!(
        "{:02x}{:02x}{:02x}{:02x}",
        address[0], address[1], address[2], address[3]
    );

    log::debug!("[{}] Ethernet address: {}", name, iface.ethernet_addr());
    log::debug!("[{}] IP address: {}", name, ipv6_addr);

    add_iface_address(&mut iface, ipv6_cidr);
    add_iface_route(
        &mut iface,
        ipv6_cidr,
        Route::new_ipv6_gateway(ipv6_addr.into()),
    );

    Network::new(name, Stack::with(iface))
}

fn to_ipv6(bytes: impl AsRef<[u8]>) -> Ipv6Addr {
    const IPV6_ADDRESS_LEN: usize = 16;

    let bytes = bytes.as_ref();
    let len = IPV6_ADDRESS_LEN.min(bytes.len());
    let mut ipv6_bytes = [0u8; IPV6_ADDRESS_LEN];

    // copy source bytes
    ipv6_bytes[..len].copy_from_slice(&bytes[..len]);
    // no multicast addresses
    ipv6_bytes[0] %= 0xff;
    // no unspecified or localhost addresses
    if ipv6_bytes[0..15] == [0u8; 15] && ipv6_bytes[15] < 0x02 {
        ipv6_bytes[15] = 0x02;
    }

    Ipv6Addr::from(ipv6_bytes)
}
