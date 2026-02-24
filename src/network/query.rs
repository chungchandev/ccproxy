use crate::config::ProxyQueryConfig;
use crate::error::{CCProxyError, CCProxyResult, sub_sys_err_to_ccproxy_err};
use std::collections::HashMap;
use std::ffi::CString;
use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::{Mutex, RwLock};
use tokio_graceful_shutdown::{ErrorAction, SubsystemBuilder, SubsystemHandle};

/// A magic bytes in Query Protocol request packets.
pub const QUERY_PACKET_MAGIC: u16 = 0xFEFD;

pub struct QueryHandler {
    upstream_address: SocketAddr,

    query: Arc<RwLock<ProxyQueryConfig>>,

    challenge_tokens: Arc<Mutex<HashMap<String, i32>>>,
}

impl QueryHandler {
    pub fn new(upstream_address: SocketAddr, fallback_query: &ProxyQueryConfig) -> Self {
        Self {
            upstream_address,
            query: Arc::new(RwLock::new(fallback_query.clone())),
            challenge_tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn init(&self, sub_sys: &mut SubsystemHandle<CCProxyError>) {
        let challenge_tokens = self.challenge_tokens.clone();

        // Reset all challenge tokens every 30 seconds.
        sub_sys.start(SubsystemBuilder::new(
            "QueryHandlerTicker",
            async move |sub: &mut SubsystemHandle<CCProxyError>| {
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                            challenge_tokens.lock().await.clear();

                            tracing::debug!("Challenge tokens are cleared.");
                        },
                        _ = sub.on_shutdown_requested() => {
                            break;
                        }
                    }
                }

                Ok::<_, CCProxyError>(())
            },
        ));

        let upstream_address = self.upstream_address;
        let fallback_query = { self.query.read().await.clone() };
        let query_clone = self.query.clone();

        // Get the upstream Query every 10 seconds.
        sub_sys.start(SubsystemBuilder::new("QueryHandlerUpdater", async move |sub: &mut SubsystemHandle<CCProxyError>| {
            let query_clone2 = query_clone.clone();

            let mut interval = tokio::time::interval(Duration::from_secs(5));
            loop {
                tokio::select! {
                    // Request a query every 10 seconds.
                    _ = interval.tick() => {
                        let query_clone = query_clone.clone();
                        let query_task = SubsystemBuilder::new("QueryHandlerUpdater_Query", async move |sub: &mut SubsystemHandle<CCProxyError>| {
                            tokio::select! {
                                query = Self::query(&upstream_address, Duration::from_secs(5), 1, true) => {
                                    if let QueryResponsePacketPayload::FullStat { k_v_section, players } = query?.payload {
                                        let mut query = ProxyQueryConfig::from_kv_and_players(k_v_section, players)?;
                                        query.host_ip = fallback_query.host_ip;
                                        query.host_port = fallback_query.host_port;

                                        tracing::debug!("The Query is updated from the upstream query server: {:?}", query);
                                        let mut query_clone = query_clone.write().await;
                                        *query_clone = query;

                                    }
                                },
                                _ = sub.on_shutdown_requested() => {
                                    return Ok(());
                                }
                            }

                            Ok::<_, CCProxyError>(())
                        })
                            .on_failure(ErrorAction::CatchAndLocalShutdown);

                        if let Err(err) = sub.start(query_task).join().await {
                            if let Some(err) = sub_sys_err_to_ccproxy_err(&err) {
                                tracing::error!("Cannot update the Query from the upstream query server: {}", err);
                            } else {
                                tracing::error!("Cannot update the Query from the upstream query server: {}", err);
                            }

                            // Reset to fallback.
                            {
                                let mut query_clone2 = query_clone2.write().await;
                                *query_clone2 = fallback_query.clone();
                            }
                        };
                    },
                    // Shutdown handler
                    _ = sub.on_shutdown_requested() => {
                        break;
                    }
                }
            }

            Ok::<_, CCProxyError>(())
        }));
    }

    pub async fn handle_packet(
        &self,
        socket: &UdpSocket,
        address: &SocketAddr,
        buf: &mut Cursor<Vec<u8>>,
    ) -> CCProxyResult<()> {
        let request = QueryRequestPacket::decode(buf).await?;

        tracing::trace!("The query packet received from ({})", address);

        use QueryRequestPacketPayload::*;
        match request.payload {
            Handshake => {
                let challenge_token = rand::random::<i32>();

                {
                    self.challenge_tokens
                        .lock()
                        .await
                        .insert(address.to_string(), challenge_token);
                }

                let response = QueryResponsePacket {
                    ty: QueryPacketType::Handshake,
                    session_id: request.session_id,
                    payload: QueryResponsePacketPayload::Handshake { challenge_token },
                };
                Self::send_response_packet(socket, address, response).await?;
            }
            BasicStat { challenge_token } => {
                if !self
                    .validate_challenge_token(address, challenge_token)
                    .await
                {
                    return Err(CCProxyError::QueryInvalid);
                }

                let query = { self.query.read().await.clone() };

                let response = QueryResponsePacket {
                    ty: QueryPacketType::Stat,
                    session_id: request.session_id,
                    payload: QueryResponsePacketPayload::BasicStat {
                        motd: query.motd,
                        game_type: query.game_type,
                        map: query.map,
                        num_players: query.num_players,
                        max_players: query.max_players,
                        host_port: query.host_port,
                        host_ip: query.host_ip,
                    },
                };
                Self::send_response_packet(socket, address, response).await?;
            }
            FullStat { challenge_token } => {
                if !self
                    .validate_challenge_token(address, challenge_token)
                    .await
                {
                    return Err(CCProxyError::QueryInvalid);
                }

                let query = { self.query.read().await.clone() };

                let response = QueryResponsePacket {
                    ty: QueryPacketType::Stat,
                    session_id: request.session_id,
                    payload: QueryResponsePacketPayload::FullStat {
                        players: query.players.clone(),
                        k_v_section: QueryResponsePacketPayload::query_config_to_k_v_section(query),
                    },
                };
                Self::send_response_packet(socket, address, response).await?;
            }
        };

        Ok(())
    }

    pub async fn validate_challenge_token(
        &self,
        address: &SocketAddr,
        challenge_token: i32,
    ) -> bool {
        let saved_challenge_token = {
            self.challenge_tokens
                .lock()
                .await
                .get(&address.to_string())
                .cloned()
        };

        saved_challenge_token == Some(challenge_token)
    }

    pub async fn query(
        address: &SocketAddr,
        timeout: Duration,
        retry: u32,
        is_full: bool,
    ) -> CCProxyResult<QueryResponsePacket> {
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(address).await?;

        let mut response = None;
        for _ in 0..retry {
            let session_id = QueryRequestPacket::generate_session_id();
            let request = QueryRequestPacket {
                ty: QueryPacketType::Handshake,
                session_id,
                payload: QueryRequestPacketPayload::Handshake,
            };

            socket
                .send(request.encode().await?.into_inner().as_slice())
                .await?;

            if let Ok(r) = Self::recv_response_packet(&socket, timeout).await {
                response = Some(r);
                break;
            } else {
                // Retry.
                continue;
            };
        }

        let response = match response {
            Some(response) => response,
            None => return Err(CCProxyError::QueryTimeout),
        };

        for _ in 0..retry {
            let challenge_token = match response.payload {
                QueryResponsePacketPayload::Handshake { challenge_token } => challenge_token,
                _ => return Err(CCProxyError::QueryInvalid),
            };

            let session_id = QueryRequestPacket::generate_session_id();
            let request = QueryRequestPacket {
                ty: QueryPacketType::Stat,
                session_id,
                payload: if !is_full {
                    QueryRequestPacketPayload::BasicStat { challenge_token }
                } else {
                    QueryRequestPacketPayload::FullStat { challenge_token }
                },
            };

            socket
                .send(request.encode().await?.into_inner().as_slice())
                .await?;

            if let Ok(response) = Self::recv_response_packet(&socket, timeout).await {
                return Ok(response);
            } else {
                // Retry.
                continue;
            }
        }

        Err(CCProxyError::QueryTimeout)
    }

    async fn send_response_packet(
        socket: &UdpSocket,
        address: &SocketAddr,
        packet: QueryResponsePacket,
    ) -> CCProxyResult<()> {
        socket
            .send_to(packet.encode().await?.into_inner().as_slice(), address)
            .await?;

        Ok(())
    }

    async fn recv_response_packet(
        socket: &UdpSocket,
        timeout: Duration,
    ) -> CCProxyResult<QueryResponsePacket> {
        let mut response_buf = vec![0u8; 1024];
        tokio::time::timeout(timeout, socket.recv(&mut response_buf))
            .await
            .map_err(|_| CCProxyError::QueryTimeout)??;

        // FIXME: is_full is hardcoded.
        let response = QueryResponsePacket::decode(&mut Cursor::new(response_buf), true).await?;
        Ok(response)
    }
}

#[derive(Debug)]
#[repr(u8)]
pub enum QueryPacketType {
    Stat = 0,

    Handshake = 9,
}

impl QueryPacketType {
    pub async fn decode(buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<Self> {
        let ty = match buf.read_u8().await? {
            0 => Self::Stat,
            9 => Self::Handshake,
            _ => return Err(CCProxyError::QueryInvalid),
        };

        Ok(ty)
    }

    pub async fn encode(&self, buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<()> {
        let ty = match self {
            Self::Stat => 0,
            Self::Handshake => 9,
        };

        buf.write_u8(ty).await?;

        Ok(())
    }
}

/// Default structure for Query Protocol request.
///
/// The Minecraft server doesn't parse higher 4 bits in each bytes in `session_id`.
/// So mask with `0x0f0f0f0f` to convert it to the Minecraft parsable session ID.
#[derive(Debug)]
pub struct QueryRequestPacket {
    pub ty: QueryPacketType,

    pub session_id: i32,

    pub payload: QueryRequestPacketPayload,
}

impl QueryRequestPacket {
    pub async fn decode(buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<Self> {
        // Check magic bytes.
        if buf.read_u16().await? != QUERY_PACKET_MAGIC {
            return Err(CCProxyError::QueryInvalid);
        }

        let ty = QueryPacketType::decode(buf).await?;
        let session_id = buf.read_i32().await? & 0x0f0f0f0f;
        let payload = QueryRequestPacketPayload::decode(buf, &ty).await?;

        Ok(Self {
            ty,
            session_id,
            payload,
        })
    }

    pub async fn encode(&self) -> CCProxyResult<Cursor<Vec<u8>>> {
        let mut buf = Cursor::new(Vec::with_capacity(20));

        buf.write_u16(QUERY_PACKET_MAGIC).await?;
        self.ty.encode(&mut buf).await?;
        buf.write_i32(self.session_id).await?;
        self.payload.encode(&mut buf).await?;

        Ok(buf)
    }

    pub fn generate_session_id() -> i32 {
        rand::random::<i32>() & 0x0f0f0f0f
    }
}

/// Default structure for Query Protocol response.
#[derive(Debug)]
pub struct QueryResponsePacket {
    pub ty: QueryPacketType,

    pub session_id: i32,

    pub payload: QueryResponsePacketPayload,
}

impl QueryResponsePacket {
    pub async fn decode(buf: &mut Cursor<Vec<u8>>, is_full: bool) -> CCProxyResult<Self> {
        let ty = QueryPacketType::decode(buf).await?;
        let session_id = buf.read_i32().await?;
        let payload = QueryResponsePacketPayload::decode(buf, &ty, is_full).await?;

        Ok(Self {
            ty,
            session_id,
            payload,
        })
    }

    pub async fn encode(&self) -> CCProxyResult<Cursor<Vec<u8>>> {
        let mut buf = Cursor::new(Vec::with_capacity(20));

        self.ty.encode(&mut buf).await?;
        buf.write_i32(self.session_id).await?;
        self.payload.encode(&mut buf).await?;

        Ok(buf)
    }
}

#[derive(Debug)]
pub enum QueryRequestPacketPayload {
    /// Request a server to get challenge token to query stats.
    Handshake,

    BasicStat {
        challenge_token: i32,
    },

    FullStat {
        challenge_token: i32,
    },
}

impl QueryRequestPacketPayload {
    pub async fn decode(buf: &mut Cursor<Vec<u8>>, ty: &QueryPacketType) -> CCProxyResult<Self> {
        let payload = match ty {
            QueryPacketType::Stat => {
                let challenge_token = buf.read_i32().await?;

                // 4 bytes padding means it is Full Stat request.
                if let Ok(0) = buf.read_u32().await {
                    Self::FullStat { challenge_token }
                } else {
                    Self::BasicStat { challenge_token }
                }
            }
            QueryPacketType::Handshake => Self::Handshake,
        };

        Ok(payload)
    }

    pub async fn encode(&self, buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<()> {
        match self {
            Self::Handshake => {
                // Empty payload.
            }
            &Self::BasicStat { challenge_token } => {
                buf.write_i32(challenge_token).await?;
            }
            &Self::FullStat { challenge_token } => {
                buf.write_i32(challenge_token).await?;
                // 4 byte padding.
                buf.write_u32(0).await?;
            }
        };

        Ok(())
    }
}

#[derive(Debug)]
pub enum QueryResponsePacketPayload {
    /// Response a challenge token to query stats.
    ///
    /// All `challenge_token` is expired every 30 seconds, so it can be expired immediately
    /// after receiving a new one.
    Handshake {
        /// This should be encoded as a null-terminated string.
        challenge_token: i32,
    },

    BasicStat {
        motd: String,

        game_type: String,

        map: String,

        num_players: u64,

        max_players: u64,

        host_port: u16,

        host_ip: IpAddr,
    },

    FullStat {
        k_v_section: HashMap<String, String>,

        players: Vec<String>,
    },
}

impl QueryResponsePacketPayload {
    pub async fn decode(
        buf: &mut Cursor<Vec<u8>>,
        ty: &QueryPacketType,
        is_full: bool,
    ) -> CCProxyResult<Self> {
        let payload = match ty {
            QueryPacketType::Stat => {
                if !is_full {
                    let motd = read_string_to_null(buf).await?;
                    let game_type = read_string_to_null(buf).await?;
                    let map = read_string_to_null(buf).await?;
                    let num_players = read_string_to_null(buf)
                        .await?
                        .parse()
                        .map_err(|_| CCProxyError::QueryInvalid)?;
                    let max_players = read_string_to_null(buf)
                        .await?
                        .parse()
                        .map_err(|_| CCProxyError::QueryInvalid)?;
                    let host_port = buf.read_u16_le().await?;
                    let host_ip = read_string_to_null(buf)
                        .await?
                        .parse()
                        .map_err(|_| CCProxyError::QueryInvalid)?;

                    Self::BasicStat {
                        motd,
                        game_type,
                        map,
                        num_players,
                        max_players,
                        host_port,
                        host_ip,
                    }
                } else {
                    buf.read_exact(&mut [0u8; 11]).await?;

                    let mut k_v_section = HashMap::new();
                    while let (Ok(k), Ok(v)) = (
                        read_string_to_null(buf).await,
                        read_string_to_null(buf).await,
                    ) {
                        if k.is_empty() {
                            break;
                        }

                        k_v_section.insert(k, v);
                    }

                    buf.read_exact(&mut [0u8; 10]).await?;

                    let mut players = Vec::new();
                    while let Ok(p) = read_string_to_null(buf).await {
                        if p.is_empty() {
                            break;
                        }

                        players.push(p);
                    }

                    Self::FullStat {
                        k_v_section,
                        players,
                    }
                }
            }
            QueryPacketType::Handshake => {
                let challenge_token = read_string_to_null(buf)
                    .await?
                    .parse()
                    .map_err(|_| CCProxyError::QueryInvalid)?;
                Self::Handshake { challenge_token }
            }
        };

        Ok(payload)
    }

    pub async fn encode(&self, buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<()> {
        match self {
            Self::Handshake { challenge_token } => {
                // The token is encoded as null-terminated string.
                let challenge_token = to_cstring(challenge_token.to_string())?;
                buf.write_all(challenge_token.as_bytes_with_nul()).await?;
            }
            Self::BasicStat {
                motd,
                game_type,
                map,
                num_players,
                max_players,
                host_port,
                host_ip: host_address,
            } => {
                let motd = to_cstring(motd.as_str())?;
                buf.write_all(motd.as_bytes_with_nul()).await?;
                let game_type = to_cstring(game_type.as_str())?;
                buf.write_all(game_type.as_bytes_with_nul()).await?;
                let map = to_cstring(map.as_str())?;
                buf.write_all(map.as_bytes_with_nul()).await?;
                let num_players = to_cstring(num_players.to_string())?;
                buf.write_all(num_players.as_bytes_with_nul()).await?;
                let max_players = to_cstring(max_players.to_string())?;
                buf.write_all(max_players.as_bytes_with_nul()).await?;
                buf.write_u16_le(*host_port).await?;
                let host_address = to_cstring(host_address.to_string())?;
                buf.write_all(host_address.as_bytes_with_nul()).await?;
            }
            Self::FullStat {
                k_v_section,
                players,
            } => {
                buf.write_all(b"splitnum\x00\x80\x00").await?;
                for (k, v) in k_v_section {
                    let k = to_cstring(k.as_str())?;
                    buf.write_all(k.as_bytes_with_nul()).await?;
                    let v = to_cstring(v.as_str())?;
                    buf.write_all(v.as_bytes_with_nul()).await?;
                }
                buf.write_u8(0x00).await?;
                buf.write_all(b"\x01player_\x00\x00").await?;
                for p in players {
                    let p = to_cstring(p.as_str())?;
                    buf.write_all(p.as_bytes_with_nul()).await?;
                }
                buf.write_u8(0x00).await?;
            }
        };

        Ok(())
    }

    pub fn query_config_to_k_v_section(query: ProxyQueryConfig) -> HashMap<String, String> {
        HashMap::from([
            ("hostname".to_owned(), query.motd),
            ("gametype".to_owned(), query.game_type),
            ("game_id".to_owned(), "MINECRAFT".to_owned()),
            ("version".to_owned(), query.version),
            ("plugins".to_owned(), query.plugins.unwrap_or_default()),
            ("map".to_owned(), query.map),
            ("numplayers".to_owned(), query.num_players.to_string()),
            ("maxplayers".to_owned(), query.max_players.to_string()),
            ("hostport".to_owned(), query.host_port.to_string()),
            ("hostip".to_owned(), query.host_ip.to_string()),
        ])
    }
}

pub async fn read_string_to_null(buf: &mut Cursor<Vec<u8>>) -> CCProxyResult<String> {
    let mut string = vec![];
    buf.read_until(0x00, &mut string).await?;
    string.pop();
    String::from_utf8(string).map_err(|_| CCProxyError::QueryInvalid)
}

fn to_cstring(s: impl Into<Vec<u8>>) -> CCProxyResult<CString> {
    CString::new(s).map_err(|_| CCProxyError::QueryInvalid)
}
