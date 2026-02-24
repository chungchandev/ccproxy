use crate::built_info;
use crate::config::CCProxyConfig;
use crate::error::{CCProxyError, CCProxyResult, sub_sys_err_to_ccproxy_err};
use crate::network::bedrock::BedrockMotd;
use crate::network::query::QueryHandler;
use rust_raknet::error::RaknetError;
use rust_raknet::{RaknetListener, RaknetSocket, Reliability};
use std::io::Cursor;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::Instant;
use tokio_graceful_shutdown::{ErrorAction, SubsystemBuilder, SubsystemHandle, Toplevel};

const RAKNET_GAME_PACKET_ID: u8 = 0xfe;

pub async fn run(config: CCProxyConfig) -> CCProxyResult<()> {
    tracing::info!(
        "The proxy server (v{}) is starting...",
        built_info::PKG_VERSION
    );

    Toplevel::<CCProxyError>::new(async move |s: &mut SubsystemHandle<CCProxyError>| {
        s.start(SubsystemBuilder::new(
            "ProxyServer",
            async move |s: &mut SubsystemHandle<CCProxyError>| listen(s, config).await,
        ));
    })
    .catch_signals()
    .handle_shutdown_requests(std::time::Duration::from_millis(5_000))
    .await?;

    tracing::info!("The proxy server is stopped. Good bye!");

    Ok(())
}

async fn listen(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    config: CCProxyConfig,
) -> CCProxyResult<()> {
    let start_time = Instant::now();

    let mut server = RaknetListener::bind_with(&config.proxy.address, true, Some(15_000)).await?;

    server
        .set_full_motd(
            config
                .proxy
                .fallback_motd
                .clone()
                .encode(Some(server.guid())),
        )
        .await?;

    // MOTD updater
    let motd = server.motd().await;

    let updater_config = config.clone();
    let guid = server.guid();
    sub_sys.start(SubsystemBuilder::new(
        "ProxyMotdUpdater",
        async move |sub: &mut SubsystemHandle<CCProxyError>| {
            run_motd_updater(sub, updater_config, motd, guid).await
        },
    ));

    server.listen().await;
    tracing::debug!("RaknetListener(GUID: {guid}) is started.");

    // Query Protocol handler
    if let Some(query_address) = config.upstream.query_address {
        let query_recv = server.get_recv_query()?;
        let query_socket = server.get_raw_socket().unwrap();
        sub_sys.start(SubsystemBuilder::new(
            "QueryHandler",
            async move |sub: &mut SubsystemHandle<CCProxyError>| {
                let query_handler = QueryHandler::new(query_address, &config.proxy.fallback_query);
                query_handler.init(sub).await;

                loop {
                    tokio::select! {
                        Some((address, packet)) = async { query_recv.lock().await.recv().await } => {
                            if let Err(err) = query_handler.handle_packet(&query_socket, &address, &mut Cursor::new(packet)).await {
                                tracing::debug!("Failed to handle a Query packet from the client ({address}): {err}");
                            }
                        },
                        _ = sub.on_shutdown_requested() => {
                            break;
                        },
                    }
                }

                Ok::<_, CCProxyError>(())
            },
        ));
    }

    tracing::info!(
        "The proxy server is started on {} in {:.2?}. Have a great day!",
        config.proxy.address,
        start_time.elapsed()
    );

    loop {
        tokio::select! {
            conn = server.accept() => {
                let conn = conn?;
                let client_address = conn.peer_addr().unwrap();
                let upstream_address = config.upstream.address;
                let upstream_proxy_protocol = config.upstream.proxy_protocol;

                let conn_task = SubsystemBuilder::new(
                    format!("Client_{client_address}"), async move |sub: &mut SubsystemHandle<CCProxyError>| handle_connection(sub, upstream_address, upstream_proxy_protocol, conn).await
                )
                    .on_failure(ErrorAction::CatchAndLocalShutdown);
                let conn_task_start = sub_sys.start(conn_task);

                // Should not block server.accept() so use new task for catching errors.
                let conn_catch_task = SubsystemBuilder::new(format!("ClientCatch_{client_address}"), async move |sub: &mut SubsystemHandle<CCProxyError>| {
                    tokio::select! {
                        err = conn_task_start.join() => {
                            if let Err(err) = err && let Some(err) = sub_sys_err_to_ccproxy_err(&err) {
                                match err {
                                    CCProxyError::RakNet { err: err_raknet } => match err_raknet {
                                        rust_raknet::error::RaknetError::ConnectionClosed => (),
                                        _ => tracing::error!("The client ({client_address}) error is occurred: {err}")
                                    },
                                    _ => tracing::error!("The client ({client_address}) error is occurred: {err}")
                                }
                            }

                            tracing::info!("The client ({client_address}) is disconnected.");
                        },
                        _ = sub.on_shutdown_requested() => (),
                    };

                    Ok::<_, CCProxyError>(())
                });
                sub_sys.start(conn_catch_task);
            },
            _ = sub_sys.on_shutdown_requested() => {
                tracing::info!("The proxy server is stopping...");

                server.close().await.ok();

                break;
            },
        };
    }

    Ok(())
}

async fn handle_connection(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    upstream_address: SocketAddr,
    upstream_proxy_protocol: bool,
    client: RaknetSocket,
) -> CCProxyResult<()> {
    let client_address = client.peer_addr()?;

    tracing::info!("A new client ({client_address}) is connected to the proxy server.");

    // Try to connect to he upstream server for the new client.
    let server = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        RaknetSocket::connect_with(
            &upstream_address,
            11,
            Some(15_000),
            upstream_proxy_protocol.then_some(&client_address),
        ),
    )
    .await
    {
        Ok(server) => {
            tracing::info!(
                "The client ({client_address}) is connected to the upstream server ({upstream_address})."
            );

            server?
        }
        Err(_) => {
            tracing::error!(
                "Cannot connect to upstream server ({upstream_address}). Closing the client ({client_address})."
            );

            client.close().await?;

            Err(RaknetError::ConnectionClosed)?
        }
    };

    let client_clone = Arc::new(client);
    let c2s_client = client_clone.clone();
    let s2c_client = client_clone.clone();
    let server_clone = Arc::new(server);
    let c2s_server = server_clone.clone();
    let s2c_server = server_clone.clone();

    let c2s = SubsystemBuilder::new(
        format!("Client_{client_address}_c2s"),
        async move |sub: &mut SubsystemHandle<CCProxyError>| {
            handle_c2s(sub, c2s_client.clone(), c2s_server.clone()).await
        },
    );
    let s2c = SubsystemBuilder::new(
        format!("Client_{client_address}_s2c"),
        async move |sub: &mut SubsystemHandle<CCProxyError>| {
            handle_s2c(sub, s2c_client.clone(), s2c_server.clone()).await
        },
    );

    sub_sys.start(c2s);
    sub_sys.start(s2c);

    sub_sys.wait_for_children().await;

    let _ = tokio::join!(client_clone.close(), server_clone.close());

    Ok(())
}

async fn handle_c2s(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    client: Arc<RaknetSocket>,
    server: Arc<RaknetSocket>,
) -> CCProxyResult<()> {
    let client_address = client.peer_addr()?;

    loop {
        // Check the s2c connection is closed.
        if server.is_closed() {
            client.close().await?;
            break;
        }

        tokio::select! {
            // Client -> Server
            packet = client.recv() => {
                handle_c2s_packet(packet?, &server, &client_address).await?;
            }
            // Shutdown handler
            _ = sub_sys.on_shutdown_requested() => {
                client.close().await?;
                break;
            }
        }
    }

    Ok(())
}

async fn handle_s2c(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    client: Arc<RaknetSocket>,
    server: Arc<RaknetSocket>,
) -> CCProxyResult<()> {
    let client_address = client.peer_addr()?;

    loop {
        // Check the c2s connection is closed.
        if client.is_closed() {
            server.close().await?;
            break;
        }

        tokio::select! {
            // Server -> Client
            packet = server.recv() => {
                handle_s2c_packet(packet?, &client, &client_address).await?;
            }
            // Shutdown handler
            _ = sub_sys.on_shutdown_requested() => {
                server.close().await?;

                break;
            }
        }
    }

    Ok(())
}

async fn handle_c2s_packet(
    packet: Vec<u8>,
    server: &RaknetSocket,
    #[allow(unused_variables)] client_address: &SocketAddr,
) -> CCProxyResult<()> {
    #[cfg(debug_assertions)]
    tracing::trace!("The client ({client_address}) got a packet: {packet:?}");

    if packet[0] != RAKNET_GAME_PACKET_ID {
        return Ok(());
    }

    server.send(&packet, Reliability::ReliableOrdered).await?;

    Ok(())
}

async fn handle_s2c_packet(
    packet: Vec<u8>,
    client: &RaknetSocket,
    #[allow(unused_variables)] client_address: &SocketAddr,
) -> CCProxyResult<()> {
    #[cfg(debug_assertions)]
    tracing::trace!("The server from the client ({client_address}) got a packet: {packet:?}");

    if packet[0] != RAKNET_GAME_PACKET_ID {
        return Ok(());
    }

    client.send(&packet, Reliability::ReliableOrdered).await?;

    Ok(())
}

async fn run_motd_updater(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    config: CCProxyConfig,
    motd: Arc<RwLock<String>>,
    guid: u64,
) -> CCProxyResult<()> {
    let upstream_address = config.upstream.address;
    let fallback_motd = config.proxy.fallback_motd.clone();
    let proxy_protocol = config.upstream.proxy_protocol;

    let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
    loop {
        let fallback_motd_clone = fallback_motd.clone();
        let motd_clone = motd.clone();

        tokio::select! {
            // Update MOTD from the upstream server every 5 seconds.
            _ = interval.tick() => {
                let ping_task = SubsystemBuilder::new("ProxyMotdUpdater_Ping", async move |sub: &mut SubsystemHandle<CCProxyError>| {
                    let motd_clone = motd_clone.clone();

                    update_motd(sub, upstream_address, motd_clone.clone(), fallback_motd_clone, guid, proxy_protocol).await
                })
                    .on_failure(ErrorAction::CatchAndLocalShutdown);

                if let Err(err) = sub_sys.start(ping_task).join().await {
                    tracing::error!("Cannot update the MOTD from the upstream server: {err}");

                    let fallback_motd = fallback_motd.clone().encode(Some(guid));

                    {
                        let mut motd = motd.write().await;
                        *motd = fallback_motd;
                    }
                };
            },
            // Shutdown handler.
            _ = sub_sys.on_shutdown_requested() => {
                break;
            }
        }
    }

    Ok(())
}

async fn update_motd(
    sub_sys: &mut SubsystemHandle<CCProxyError>,
    upstream_address: SocketAddr,
    motd: Arc<RwLock<String>>,
    fallback_motd: BedrockMotd,
    guid: u64,
    proxy_protocol: bool,
) -> CCProxyResult<()> {
    tokio::select! {
        pong = RaknetSocket::ping_with(&upstream_address, std::time::Duration::from_secs(5), 1, proxy_protocol) => {
            let (pong_latency, pong_motd) = pong?;

            // Preserve server GUID, IPv4 port, and IPv6 port.
            let new_motd = BedrockMotd::decode(pong_motd, None, fallback_motd.ipv4_port, fallback_motd.ipv6_port)
                .map_err(|_| CCProxyError::UpstreamMotdInvalid)?
                .encode(Some(guid));

            {
                let mut motd = motd.write().await;
                *motd = new_motd;
            }

            tracing::debug!("The proxy server MOTD is updated from the upstream server ({upstream_address}). The latency is {pong_latency}ms.");
        },
        _ = sub_sys.on_shutdown_requested() => ()
    };

    Ok(())
}
