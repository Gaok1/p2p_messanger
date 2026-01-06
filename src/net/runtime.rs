use std::{
    collections::HashMap,
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use base64::Engine;
use quinn::{Endpoint, EndpointConfig, RecvStream, ServerConfig};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;
use tokio::sync::mpsc as tokio_mpsc;

use super::autotune::{AutotuneConfig, AutotuneState};
use super::commands::{NetCommand, NetEvent};
use super::messages::{InboundFrame, WireMessage, decode_payload, spawn_send_task};
use super::mobility::{ConnSignal, ConnectAttempt, ConnectResult, MobilityConfig, ReconnectState};
use super::stun;
use super::transfer::{
    IncomingTransfer, SendOutcome, SendResult, handle_incoming_message, handle_incoming_stream,
    send_message,
};

pub const CHUNK_SIZE: usize = 64 * 1024;
pub(crate) const PROTOCOL_VERSION: u8 = 3;
pub(crate) const OBSERVED_ENDPOINT_VERSION: u8 = 2;
pub(crate) const HEARTBEAT_VERSION: u8 = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(6);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const MOBILITY_TIMER_PARK: Duration = Duration::from_secs(3600);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_MAX_MISSES: u32 = 3;

/// Inicia a thread de rede e retorna os canais de comunicação com a UI.
pub fn start_network(
    bind_addr: SocketAddr,
    peer_addr: Option<SocketAddr>,
    autotune: AutotuneConfig,
) -> (
    tokio_mpsc::UnboundedSender<NetCommand>,
    Receiver<NetEvent>,
    thread::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
    let (evt_tx, evt_rx) = mpsc::channel();
    let handle = thread::spawn(move || run_network(bind_addr, peer_addr, autotune, cmd_rx, evt_tx));
    (cmd_tx, evt_rx, handle)
}

fn run_network(
    bind_addr: SocketAddr,
    initial_peer: Option<SocketAddr>,
    autotune: AutotuneConfig,
    cmd_rx: tokio_mpsc::UnboundedReceiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
) {
    let runtime = Runtime::new().expect("runtime");
    runtime.block_on(async move {
        if let Err(err) = run_network_async(bind_addr, initial_peer, autotune, cmd_rx, evt_tx).await
        {
            eprintln!("net thread error: {err}");
        }
    });
}

async fn run_network_async(
    mut bind_addr: SocketAddr,
    initial_peer: Option<SocketAddr>,
    autotune: AutotuneConfig,
    mut cmd_rx: tokio_mpsc::UnboundedReceiver<NetCommand>,
    evt_tx: Sender<NetEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    log_transport_config(&evt_tx);

    let mut pending_cmds: Vec<NetCommand> = Vec::new();

    let autotune_state = std::sync::Arc::new(Mutex::new(AutotuneState::new(autotune.clone())));

    let (mut endpoint, _cert, initial_stun) = loop {
        let target_window = autotune_state.lock().await.current_target();
        match make_endpoint(bind_addr, target_window, &autotune, Some(&evt_tx)) {
            Ok(ctx) => break ctx,
            Err(err) => {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao abrir endpoint {err}")));

                match cmd_rx.recv().await {
                    Some(NetCommand::Rebind(new_bind)) => {
                        if new_bind != bind_addr {
                            bind_addr = new_bind;
                            let _ = evt_tx.send(NetEvent::Log(
                                "stun sera executado ao reabrir endpoint".to_string(),
                            ));
                        }
                    }
                    Some(NetCommand::Shutdown) | None => return Ok(()),
                    Some(other) => pending_cmds.push(other),
                }
            }
        }
    };

    if let Ok(local_addr) = endpoint.local_addr() {
        bind_addr = local_addr;
        let _ = evt_tx.send(NetEvent::Bound(local_addr));
    }
    let mut public_endpoint = initial_stun.as_ref().ok().copied().flatten();
    publish_stun_result(initial_stun, &evt_tx);

    let mut connected_peer: Option<SocketAddr> = None;
    let mut session_dir: Option<PathBuf> = None;
    let mut incoming: HashMap<u64, IncomingTransfer> = HashMap::new();
    let mut next_file_id = 1u64;
    let (mut inbound_tx, mut inbound_rx) = tokio_mpsc::unbounded_channel::<InboundFrame>();
    let (completion_tx, mut completion_rx) = tokio_mpsc::unbounded_channel::<u64>();
    let mut reader_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut metrics_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut send_task: Option<tokio::task::JoinHandle<io::Result<SendResult>>> = None;
    let mut send_cmd_tx: Option<tokio_mpsc::UnboundedSender<NetCommand>> = None;
    let mut connection: Option<quinn::Connection> = None;
    let mut peer_protocol_version: Option<u8> = None;
    let mut heartbeat_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut heartbeat_inflight: Option<u64> = None;
    let mut heartbeat_misses: u32 = 0;
    let mut heartbeat_nonce: u64 = 0;

    let mobility = MobilityConfig::default();
    let (conn_signal_tx, mut conn_signal_rx) = tokio_mpsc::unbounded_channel::<ConnSignal>();
    let mut liveness_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut observe_task: Option<tokio::task::JoinHandle<()>> = None;

    let (connect_tx, mut connect_rx) = tokio_mpsc::unbounded_channel::<ConnectResult>();
    let mut connect_attempt: Option<ConnectAttempt> = None;
    let mut next_connect_id: u64 = 0;

    let mut last_peer: Option<SocketAddr> = initial_peer;
    let mut reconnect: Option<ReconnectState> = None;
    let mut reconnect_deadline: Option<tokio::time::Instant> = None;
    let reconnect_sleep = tokio::time::sleep(MOBILITY_TIMER_PARK);
    tokio::pin!(reconnect_sleep);

    if let Some(peer) = initial_peer {
        pending_cmds.push(NetCommand::ConnectPeer(peer));
    }

    loop {
        if !pending_cmds.is_empty() {
            let queued: Vec<_> = pending_cmds.drain(..).collect();
            for cmd in queued {
                match cmd {
                    NetCommand::SendFiles(files) => {
                        if send_task.is_some() {
                            let _ = evt_tx
                                .send(NetEvent::Log("ja existe um envio em andamento".to_string()));
                            continue;
                        }

                        if let Some((task, tx)) = spawn_send_task(
                            files.clone(),
                            &connection,
                            connected_peer,
                            next_file_id,
                            &evt_tx,
                        ) {
                            send_task = Some(task);
                            send_cmd_tx = Some(tx);
                        } else {
                            pending_cmds.push(NetCommand::SendFiles(files));
                        }
                    }
                    NetCommand::Rebind(addr) if send_task.is_some() => {
                        if let Some(tx) = &send_cmd_tx {
                            let _ = tx.send(NetCommand::Rebind(addr));
                        }
                        pending_cmds.push(NetCommand::Rebind(addr));
                        continue;
                    }
                    NetCommand::CancelTransfers if send_task.is_some() => {
                        if let Some(tx) = &send_cmd_tx {
                            let _ = tx.send(NetCommand::CancelTransfers);
                        }
                        let _ = evt_tx.send(NetEvent::Log("cancelamento solicitado".to_string()));
                        continue;
                    }
                    other => {
                        let exit = handle_command(
                            other,
                            &mut bind_addr,
                            &mut connected_peer,
                            &mut connection,
                            &mut next_file_id,
                            &evt_tx,
                            &mut cmd_rx,
                            &mut pending_cmds,
                            &mut endpoint,
                            &mut inbound_tx,
                            &mut reader_task,
                            &mut metrics_task,
                            &connect_tx,
                            &mut connect_attempt,
                            &mut next_connect_id,
                            &mut last_peer,
                            &mut liveness_task,
                            &mut observe_task,
                            &mut public_endpoint,
                            &autotune,
                            &autotune_state,
                        )
                        .await?;
                        if exit {
                            return Ok(());
                        }
                    }
                }
            }
        }

        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(NetCommand::SendFiles(files)) => {
                        if send_task.is_some() {
                            let _ = evt_tx.send(NetEvent::Log(
                                "ja existe um envio em andamento".to_string(),
                            ));
                            continue;
                        }

                        if let Some((task, tx)) =
                            spawn_send_task(files.clone(), &connection, connected_peer, next_file_id, &evt_tx)
                        {
                            send_task = Some(task);
                            send_cmd_tx = Some(tx);
                        } else {
                            pending_cmds.push(NetCommand::SendFiles(files));
                        }
                    }
                    Some(NetCommand::Rebind(new_bind)) if send_task.is_some() => {
                        if let Some(tx) = &send_cmd_tx {
                            let _ = tx.send(NetCommand::Rebind(new_bind));
                        }
                        pending_cmds.push(NetCommand::Rebind(new_bind));
                        continue;
                    }
                    Some(NetCommand::Rebind(new_bind)) => {
                        if new_bind != bind_addr {
                            let _ = evt_tx.send(NetEvent::Log(format!("reconfigurando bind para {new_bind}")));
                            let target_window = autotune_state.lock().await.current_target();
                            match make_endpoint(new_bind, target_window, &autotune, Some(&evt_tx)) {
                                Ok((new_endpoint, _, stun_res)) => {
                                    endpoint = new_endpoint;
                                    bind_addr = endpoint.local_addr().unwrap_or(new_bind);
                                    let _ = evt_tx.send(NetEvent::Bound(bind_addr));
                                    connection = None;
                                    connected_peer = None;
                                    connect_attempt = None;
                                    session_dir = None;
                                    incoming.clear();
                                    next_file_id = 1;
                                    if let Some(task) = reader_task.take() {
                                        task.abort();
                                    }
                                    if let Some(task) = metrics_task.take() {
                                        task.abort();
                                    }
                                    if let Some(task) = liveness_task.take() {
                                        task.abort();
                                    }
                                    if let Some(task) = observe_task.take() {
                                        task.abort();
                                    }
                                    peer_protocol_version = None;
                                    heartbeat_inflight = None;
                                    heartbeat_misses = 0;
                                    reconnect = None;
                                    reconnect_deadline = None;
                                    reconnect_sleep
                                        .as_mut()
                                        .reset(tokio::time::Instant::now() + MOBILITY_TIMER_PARK);
                                    public_endpoint = stun_res.as_ref().ok().copied().flatten();
                                    publish_stun_result(stun_res, &evt_tx);
                                }
                                Err(err) => {
                                    let _ = evt_tx.send(NetEvent::Log(format!("erro ao reconfigurar {err}")));
                                }
                            }
                        }
                    }
                    Some(NetCommand::CancelTransfers) if send_task.is_some() => {
                        if let Some(tx) = &send_cmd_tx {
                            let _ = tx.send(NetCommand::CancelTransfers);
                        }
                        let _ = evt_tx.send(NetEvent::Log("cancelamento solicitado".to_string()));
                        continue;
                    }
                    Some(other) => {
                        if mobility.enabled && matches!(other, NetCommand::ConnectPeer(_)) {
                            reconnect = None;
                            reconnect_deadline = None;
                            reconnect_sleep
                                .as_mut()
                                .reset(tokio::time::Instant::now() + MOBILITY_TIMER_PARK);
                        }
                        if matches!(other, NetCommand::Shutdown) {
                            if let Some(tx) = &send_cmd_tx {
                                let _ = tx.send(NetCommand::Shutdown);
                            }
                        }
                        let exit = handle_command(
                            other,
                            &mut bind_addr,
                            &mut connected_peer,
                            &mut connection,
                            &mut next_file_id,
                            &evt_tx,
                            &mut cmd_rx,
                            &mut pending_cmds,
                            &mut endpoint,
                             &mut inbound_tx,
                             &mut reader_task,
                             &mut metrics_task,
                             &connect_tx,
                             &mut connect_attempt,
                             &mut next_connect_id,
                             &mut last_peer,
                             &mut liveness_task,
                             &mut observe_task,
                             &mut public_endpoint,
                             &autotune,
                             &autotune_state,
                          ).await?;
                          if exit {
                              return Ok(());
                         }
                    }
                    None => return Ok(()),
                }
            }
            Some(signal) = conn_signal_rx.recv() => {
                match signal {
                    ConnSignal::Closed { peer, error } => {
                        let matches_peer = connection
                            .as_ref()
                            .is_some_and(|conn| conn.remote_address() == peer);
                        if !matches_peer {
                            continue;
                        }

                        connection = None;
                        connected_peer = None;
                        connect_attempt = None;
                        if let Some(task) = reader_task.take() {
                            task.abort();
                        }
                        if let Some(task) = metrics_task.take() {
                            task.abort();
                        }
                        if let Some(task) = liveness_task.take() {
                            task.abort();
                        }
                        if let Some(task) = observe_task.take() {
                            task.abort();
                        }
                        peer_protocol_version = None;
                        heartbeat_inflight = None;
                        heartbeat_misses = 0;

                        let _ = evt_tx.send(NetEvent::Log(format!(
                            "conexao perdida {peer}: {error}"
                        )));
                        let _ = evt_tx.send(NetEvent::PeerDisconnected(peer));

                        if mobility.enabled && mobility.reconnect_enabled {
                            reconnect = Some(ReconnectState {
                                peer,
                                backoff: mobility.reconnect_initial,
                                failures: 0,
                            });
                            let next = tokio::time::Instant::now() + mobility.reconnect_initial;
                            reconnect_deadline = Some(next);
                            reconnect_sleep.as_mut().reset(next);
                        }
                    }
                }
            }
            Some(send_result) = async {
                if let Some(task) = send_task.as_mut() {
                    Some(task.await)
                } else {
                    None
                }
            } => {
                send_task = None;
                send_cmd_tx = None;

                match send_result {
                    Ok(Ok(result)) => {
                        next_file_id = result.next_file_id;
                        pending_cmds.extend(result.pending_cmds);

                        match result.outcome {
                            SendOutcome::Completed => {}
                            SendOutcome::Canceled => {
                                let _ = evt_tx
                                    .send(NetEvent::Log("transferencia cancelada".to_string()));
                            }
                            SendOutcome::Shutdown => return Ok(()),
                        }
                    }
                    Ok(Err(err)) => {
                        let _ = evt_tx.send(NetEvent::Log(format!("erro no envio {err}")));
                    }
                    Err(err) => {
                        let _ = evt_tx.send(NetEvent::Log(format!(
                            "task de envio encerrada inesperadamente {err}",
                        )));
                    }
                }
            }
            _ = heartbeat_tick.tick(), if connection.is_some() && peer_protocol_version.unwrap_or(0) >= HEARTBEAT_VERSION => {
                if heartbeat_inflight.is_some() {
                    heartbeat_misses = heartbeat_misses.saturating_add(1);
                    if heartbeat_misses >= HEARTBEAT_MAX_MISSES {
                        if let Some(conn) = connection.take() {
                            let peer = conn.remote_address();
                            conn.close(quinn::VarInt::from_u32(0), b"heartbeat timeout");
                            connected_peer = None;
                            connect_attempt = None;
                            if let Some(task) = reader_task.take() {
                                task.abort();
                            }
                            if let Some(task) = metrics_task.take() {
                                task.abort();
                            }
                            if let Some(task) = liveness_task.take() {
                                task.abort();
                            }
                            if let Some(task) = observe_task.take() {
                                task.abort();
                            }
                            peer_protocol_version = None;
                            heartbeat_inflight = None;
                            heartbeat_misses = 0;
                            let _ = evt_tx.send(NetEvent::Log(format!(
                                "heartbeat: sem resposta de {peer} ({}x), desconectando",
                                HEARTBEAT_MAX_MISSES
                            )));
                            let _ = evt_tx.send(NetEvent::PeerDisconnected(peer));

                            if mobility.enabled && mobility.reconnect_enabled {
                                reconnect = Some(ReconnectState {
                                    peer,
                                    backoff: mobility.reconnect_initial,
                                    failures: 0,
                                });
                                let next = tokio::time::Instant::now() + mobility.reconnect_initial;
                                reconnect_deadline = Some(next);
                                reconnect_sleep.as_mut().reset(next);
                            }
                        }
                        continue;
                    }
                }

                heartbeat_nonce = heartbeat_nonce.wrapping_add(1);
                let nonce = heartbeat_nonce;
                heartbeat_inflight = Some(nonce);
                if let Some(conn) = connection.as_ref() {
                    let _ = send_message(conn, &WireMessage::Ping { nonce }, &evt_tx).await;
                }
            }
            _ = &mut reconnect_sleep, if reconnect_deadline.is_some() => {
                reconnect_deadline = None;
                if !mobility.enabled || !mobility.reconnect_enabled {
                    continue;
                }
                let Some(state) = reconnect.as_ref() else {
                    continue;
                };
                if connection.is_some() || connect_attempt.is_some() {
                    let next = tokio::time::Instant::now() + Duration::from_millis(250);
                    reconnect_deadline = Some(next);
                    reconnect_sleep.as_mut().reset(next);
                    continue;
                }
                pending_cmds.push(NetCommand::ConnectPeer(state.peer));
            }
            Some(connect_result) = connect_rx.recv() => {
                let Some(attempt) = connect_attempt else {
                    continue;
                };
                if attempt.id != connect_result.id || attempt.peer != connect_result.peer {
                     continue;
                 }

                connect_attempt = None;
                match connect_result.connection {
                    Some(new_conn) => {
                        if let Some(task) = reader_task.take() {
                            task.abort();
                        }
                        if let Some(task) = metrics_task.take() {
                            task.abort();
                        }
                        if let Some(task) = liveness_task.take() {
                            task.abort();
                        }
                        if let Some(task) = observe_task.take() {
                            task.abort();
                        }
                        peer_protocol_version = None;
                        heartbeat_inflight = None;
                        heartbeat_misses = 0;
                        setup_connection_reader(
                            new_conn.clone(),
                            &mut inbound_tx,
                            &evt_tx,
                            &mut reader_task,
                        );
                        metrics_task = Some(start_autotune_task(
                            new_conn.clone(),
                            autotune.clone(),
                            autotune_state.clone(),
                            evt_tx.clone(),
                        ));
                        if mobility.enabled {
                            liveness_task = Some(start_liveness_task(new_conn.clone(), conn_signal_tx.clone()));
                        }
                        reconnect = None;
                        reconnect_deadline = None;
                        reconnect_sleep
                            .as_mut()
                            .reset(tokio::time::Instant::now() + MOBILITY_TIMER_PARK);
                        connected_peer = Some(new_conn.remote_address());
                        last_peer = Some(new_conn.remote_address());
                        connection = Some(new_conn.clone());
                        let _ = send_message(
                            &new_conn,
                            &WireMessage::Hello {
                                version: PROTOCOL_VERSION,
                            },
                            &evt_tx,
                        )
                        .await;
                        let _ = evt_tx.send(NetEvent::PeerConnected(new_conn.remote_address()));
                    }
                    None => {
                        if connection.is_none() && connected_peer == Some(connect_result.peer) {
                            connected_peer = None;
                        }
                        let _ = evt_tx.send(NetEvent::PeerTimeout(connect_result.peer));

                        if mobility.enabled && mobility.reconnect_enabled {
                            let peer = connect_result.peer;
                            let state = reconnect.get_or_insert(ReconnectState {
                                peer,
                                backoff: mobility.reconnect_initial,
                                failures: 0,
                            });
                            if state.peer != peer {
                                *state = ReconnectState {
                                    peer,
                                    backoff: mobility.reconnect_initial,
                                    failures: 0,
                                };
                            }
                            state.failures = state.failures.saturating_add(1);

                            if mobility.rebind_after_failures > 0
                                && state.failures >= mobility.rebind_after_failures
                                && state.failures % mobility.rebind_after_failures == 0
                            {
                                let new_bind = SocketAddr::new(bind_addr.ip(), 0);
                                let _ = evt_tx.send(NetEvent::Log(format!(
                                    "mobilidade: rebind apos {} falhas (novo bind {new_bind})",
                                    state.failures
                                )));
                                let target_window = autotune_state.lock().await.current_target();
                                match make_endpoint(new_bind, target_window, &autotune, Some(&evt_tx))
                                {
                                    Ok((new_endpoint, _, stun_res)) => {
                                        endpoint = new_endpoint;
                                        bind_addr = endpoint.local_addr().unwrap_or(new_bind);
                                        let _ = evt_tx.send(NetEvent::Bound(bind_addr));
                                        public_endpoint =
                                            stun_res.as_ref().ok().copied().flatten();
                                        publish_stun_result(stun_res, &evt_tx);
                                    }
                                    Err(err) => {
                                        let _ = evt_tx.send(NetEvent::Log(format!(
                                            "mobilidade: falha ao rebind {err}"
                                        )));
                                    }
                                }
                            }

                            let delay = state.backoff;
                            state.backoff = state
                                .backoff
                                .saturating_mul(2)
                                .min(mobility.reconnect_max);
                            let next = tokio::time::Instant::now() + delay;
                            reconnect_deadline = Some(next);
                            reconnect_sleep.as_mut().reset(next);
                            let _ = evt_tx.send(NetEvent::Log(format!(
                                "reconectando {peer} em {}ms",
                                delay.as_millis()
                            )));
                        }
                    }
                }
            }
            incoming = endpoint.accept() => {
                if let Some(connecting) = incoming {
                     match connecting.await {
                          Ok(new_conn) => {
                              if let Some(attempt) = connect_attempt {
                                  let remote = new_conn.remote_address();
                                  if attempt.peer != remote {
                                      let _ = evt_tx.send(NetEvent::Log(format!(
                                          "conexao recebida de {} (aguardando {}), ignorando",
                                          remote,
                                          attempt.peer
                                      )));
                                      continue;
                                  }

                                  if public_endpoint
                                      .map(|local| local < attempt.peer)
                                      .unwrap_or(false)
                                  {
                                      let _ = evt_tx.send(NetEvent::Log(format!(
                                          "simultaneo: mantendo conexao de saida (local={public_endpoint:?} < peer={}), ignorando entrada {}",
                                          attempt.peer,
                                          remote
                                      )));
                                      continue;
                                  }
                                  connect_attempt = None;
                              } else if connection.is_some() {
                                  let _ = evt_tx.send(NetEvent::Log(format!(
                                      "conexao extra ignorada de {} (ja conectado)",
                                      new_conn.remote_address()
                                 )));
                                 continue;
                             }

                              if let Some(task) = reader_task.take() {
                                  task.abort();
                              }
                              if let Some(task) = liveness_task.take() {
                                  task.abort();
                              }
                              if let Some(task) = observe_task.take() {
                                  task.abort();
                              }
                              peer_protocol_version = None;
                              heartbeat_inflight = None;
                              heartbeat_misses = 0;
                              connected_peer = Some(new_conn.remote_address());
                              last_peer = Some(new_conn.remote_address());
                              setup_connection_reader(
                                  new_conn.clone(),
                                  &mut inbound_tx,
                                  &evt_tx,
                                  &mut reader_task,
                              );
                             if let Some(task) = metrics_task.take() {
                                 task.abort();
                             }
                             metrics_task = Some(start_autotune_task(
                                 new_conn.clone(),
                                 autotune.clone(),
                                  autotune_state.clone(),
                                  evt_tx.clone(),
                              ));
                              if mobility.enabled {
                                  liveness_task = Some(start_liveness_task(new_conn.clone(), conn_signal_tx.clone()));
                              }
                              reconnect = None;
                              reconnect_deadline = None;
                              reconnect_sleep.as_mut().reset(
                                  tokio::time::Instant::now() + MOBILITY_TIMER_PARK
                              );
                              connection = Some(new_conn.clone());
                              let _ = send_message(
                                  &new_conn,
                                  &WireMessage::Hello {
                                      version: PROTOCOL_VERSION,
                                  },
                                  &evt_tx,
                              )
                              .await;
                              let _ = evt_tx.send(NetEvent::PeerConnected(new_conn.remote_address()));
                          }
                          Err(err) => {
                              let _ = evt_tx.send(NetEvent::Log(format!("erro ao aceitar {err}")));
                        }
                    }
                }
            }
            Some(inbound) = async { inbound_rx.recv().await } => {
                match inbound {
                    InboundFrame::Control(message, from) => {
                        if let Some(conn) = connection.as_ref() {
                            match message {
                                WireMessage::Hello { version } => {
                                    peer_protocol_version = Some(version);
                                    heartbeat_inflight = None;
                                    heartbeat_misses = 0;
                                    if version >= OBSERVED_ENDPOINT_VERSION
                                        && mobility.enabled
                                        && observe_task.is_none()
                                    {
                                        observe_task = Some(start_observe_task(
                                            conn.clone(),
                                            evt_tx.clone(),
                                            mobility.observe_interval,
                                        ));
                                    }
                                    let new_peer = handle_incoming_message(
                                        conn,
                                        WireMessage::Hello { version },
                                        from,
                                        &mut connected_peer,
                                        &mut session_dir,
                                        &mut incoming,
                                        &mut public_endpoint,
                                        &evt_tx,
                                    )
                                    .await;
                                    if new_peer {
                                        last_peer = Some(from);
                                        let _ = evt_tx.send(NetEvent::PeerConnected(from));
                                    }
                                    continue;
                                }
                                WireMessage::Ping { nonce } => {
                                    if peer_protocol_version.is_none() {
                                        peer_protocol_version = Some(HEARTBEAT_VERSION);
                                    }
                                    heartbeat_inflight = None;
                                    heartbeat_misses = 0;
                                    let _ = send_message(
                                        conn,
                                        &WireMessage::Pong { nonce },
                                        &evt_tx,
                                    )
                                    .await;
                                    if connected_peer.is_none() {
                                        connected_peer = Some(from);
                                        last_peer = Some(from);
                                        let _ = evt_tx.send(NetEvent::PeerConnected(from));
                                    }
                                    continue;
                                }
                                WireMessage::Pong { .. } => {
                                    if peer_protocol_version.is_none() {
                                        peer_protocol_version = Some(HEARTBEAT_VERSION);
                                    }
                                    heartbeat_inflight = None;
                                    heartbeat_misses = 0;
                                    if connected_peer.is_none() {
                                        connected_peer = Some(from);
                                        last_peer = Some(from);
                                        let _ = evt_tx.send(NetEvent::PeerConnected(from));
                                    }
                                    continue;
                                }
                                message => {
                                    heartbeat_inflight = None;
                                    heartbeat_misses = 0;
                                    let new_peer = handle_incoming_message(
                                        conn,
                                        message,
                                        from,
                                        &mut connected_peer,
                                        &mut session_dir,
                                        &mut incoming,
                                        &mut public_endpoint,
                                        &evt_tx,
                                    )
                                    .await;
                                    if new_peer {
                                        last_peer = Some(from);
                                        let _ = evt_tx.send(NetEvent::PeerConnected(from));
                                    }
                                    continue;
                                }
                            }
                        } else {
                            let _ = evt_tx.send(NetEvent::Log("mensagem recebida sem conexao".to_string()));
                        }
                    }
                    InboundFrame::FileStream { file_id, name, size, from, stream } => {
                        if connection.is_none() {
                            let _ = evt_tx.send(NetEvent::Log("stream recebido sem conexao".to_string()));
                            continue;
                        }
                        heartbeat_inflight = None;
                        heartbeat_misses = 0;

                        if let Err(err) = handle_incoming_stream(
                            file_id,
                            name,
                            size,
                            from,
                            stream,
                            &mut session_dir,
                            &mut incoming,
                            &evt_tx,
                            completion_tx.clone(),
                        ).await {
                            let _ = evt_tx.send(NetEvent::Log(format!("erro ao receber stream: {err}")));
                        } else if connected_peer.is_none() {
                            connected_peer = Some(from);
                            last_peer = Some(from);
                            let _ = evt_tx.send(NetEvent::PeerConnected(from));
                        }
                    }
                }
            }
            Some(done_id) = completion_rx.recv() => {
                incoming.remove(&done_id);
            }
            else => {
                break;
            }
        }
    }

    let _ = reader_task.take().map(|t| t.abort());
    let _ = metrics_task.take().map(|t| t.abort());
    let _ = liveness_task.take().map(|t| t.abort());
    let _ = observe_task.take().map(|t| t.abort());
    Ok(())
}

async fn handle_command(
    cmd: NetCommand,
    bind_addr: &mut SocketAddr,
    connected_peer: &mut Option<SocketAddr>,
    connection: &mut Option<quinn::Connection>,
    _next_file_id: &mut u64,
    evt_tx: &Sender<NetEvent>,
    _cmd_rx: &mut tokio_mpsc::UnboundedReceiver<NetCommand>,
    pending_cmds: &mut Vec<NetCommand>,
    endpoint: &mut Endpoint,
    _inbound_tx: &mut tokio_mpsc::UnboundedSender<InboundFrame>,
    reader_task: &mut Option<tokio::task::JoinHandle<()>>,
    metrics_task: &mut Option<tokio::task::JoinHandle<()>>,
    connect_tx: &tokio_mpsc::UnboundedSender<ConnectResult>,
    connect_attempt: &mut Option<ConnectAttempt>,
    next_connect_id: &mut u64,
    last_peer: &mut Option<SocketAddr>,
    liveness_task: &mut Option<tokio::task::JoinHandle<()>>,
    observe_task: &mut Option<tokio::task::JoinHandle<()>>,
    public_endpoint: &mut Option<SocketAddr>,
    autotune: &AutotuneConfig,
    autotune_state: &std::sync::Arc<Mutex<AutotuneState>>,
) -> io::Result<bool> {
    match cmd {
        NetCommand::ConnectPeer(addr) => {
            *last_peer = Some(addr);
            let family_mismatch = endpoint
                .local_addr()
                .ok()
                .is_some_and(|local| local.is_ipv4() != addr.is_ipv4());
            if family_mismatch {
                let new_bind = if addr.is_ipv4() {
                    SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
                } else {
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
                };
                let target_window = autotune_state.lock().await.current_target();
                match make_endpoint(new_bind, target_window, autotune, Some(evt_tx)) {
                    Ok((new_endpoint, _, stun_res)) => {
                        *endpoint = new_endpoint;
                        *bind_addr = endpoint.local_addr().unwrap_or(new_bind);
                        let _ = evt_tx.send(NetEvent::Bound(*bind_addr));
                        *connection = None;
                        *connected_peer = None;
                        *connect_attempt = None;
                        let _ = reader_task.take().map(|t| t.abort());
                        if let Some(task) = metrics_task.take() {
                            task.abort();
                        }
                        let _ = liveness_task.take().map(|t| t.abort());
                        let _ = observe_task.take().map(|t| t.abort());
                        *public_endpoint = stun_res.as_ref().ok().copied().flatten();
                        publish_stun_result(stun_res, evt_tx);
                    }
                    Err(err) => {
                        let _ = evt_tx.send(NetEvent::Log(format!("erro ao reconfigurar {err}")));
                    }
                }
            }
            if connection.is_some() {
                *connection = None;
                *connected_peer = None;
                *connect_attempt = None;
                if let Some(task) = reader_task.take() {
                    task.abort();
                }
                if let Some(task) = metrics_task.take() {
                    task.abort();
                }
                let _ = liveness_task.take().map(|t| t.abort());
                let _ = observe_task.take().map(|t| t.abort());
            }
            *connected_peer = Some(addr);
            let _ = evt_tx.send(NetEvent::PeerConnecting(addr));

            *next_connect_id = next_connect_id.wrapping_add(1);
            let attempt_id = *next_connect_id;
            *connect_attempt = Some(ConnectAttempt {
                id: attempt_id,
                peer: addr,
            });

            let endpoint = endpoint.clone();
            let evt_tx = evt_tx.clone();
            let connect_tx = connect_tx.clone();
            tokio::spawn(async move {
                let connection = connect_peer(&endpoint, addr, &evt_tx).await;
                let _ = connect_tx.send(ConnectResult {
                    id: attempt_id,
                    peer: addr,
                    connection,
                });
            });
        }
        NetCommand::ProbePeer(peer) => {
            let evt_tx = evt_tx.clone();
            if endpoint
                .local_addr()
                .ok()
                .is_some_and(|local| local.is_ipv4() != peer.is_ipv4())
            {
                let _ = evt_tx.send(NetEvent::ProbeFinished {
                    peer,
                    ok: false,
                    message: "familia IP diferente do bind atual".to_string(),
                });
            } else {
                let endpoint = endpoint.clone();
                tokio::spawn(async move {
                    let result = quick_probe_peer(&endpoint, peer).await;
                    match result {
                        Ok(duration) => {
                            let _ = evt_tx.send(NetEvent::ProbeFinished {
                                peer,
                                ok: true,
                                message: format!(
                                    "respondeu em {} ms (teste UDP/TLS)",
                                    duration.as_millis()
                                ),
                            });
                        }
                        Err(err) => {
                            let _ = evt_tx.send(NetEvent::ProbeFinished {
                                peer,
                                ok: false,
                                message: err,
                            });
                        }
                    }
                });
            }
        }
        NetCommand::Rebind(_) => {}
        NetCommand::CancelTransfers => {
            let _ = evt_tx.send(NetEvent::Log("cancelamento solicitado".to_string()));
        }
        NetCommand::SendFiles(files) => {
            pending_cmds.push(NetCommand::SendFiles(files));
            let _ = evt_tx.send(NetEvent::Log(
                "envio de arquivos sera iniciado quando possivel".to_string(),
            ));
        }
        NetCommand::Shutdown => return Ok(true),
    }
    Ok(false)
}

fn publish_stun_result(result: Result<Option<SocketAddr>, String>, evt_tx: &Sender<NetEvent>) {
    match result {
        Ok(Some(endpoint)) => {
            let _ = evt_tx.send(NetEvent::PublicEndpoint(endpoint));
        }
        Ok(None) => {
            let _ = evt_tx.send(NetEvent::Log("stun indisponivel".to_string()));
        }
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("stun erro {err}")));
        }
    }
}

fn log_transport_config(evt_tx: &Sender<NetEvent>) {
    let _ = evt_tx.send(NetEvent::Log(
        "transporte QUIC: streams confiaveis com TLS 1.3".to_string(),
    ));
    let _ = evt_tx.send(NetEvent::Log(format!(
        "dados de arquivo: streams unidirecionais com chunks de {CHUNK_SIZE} bytes"
    )));
}

fn make_endpoint(
    bind_addr: SocketAddr,
    target_window: u64,
    autotune: &AutotuneConfig,
    evt_tx: Option<&Sender<NetEvent>>,
) -> Result<(Endpoint, Vec<u8>, Result<Option<SocketAddr>, String>), Box<dyn std::error::Error>> {
    let cert = rcgen::generate_simple_self_signed(["pasta-p2p.local".into()])?;
    let cert_der = cert.serialize_der()?;
    let priv_key = cert.serialize_private_key_der();

    let server_config = make_server_config(
        cert_der.clone(),
        priv_key.clone(),
        make_transport_config(target_window, autotune),
    )?;
    let client_config = make_client_config(make_transport_config(target_window, autotune));

    let socket = match std::net::UdpSocket::bind(bind_addr) {
        Ok(socket) => socket,
        Err(err) if err.kind() == io::ErrorKind::AddrInUse && bind_addr.port() != 0 => {
            let fallback = SocketAddr::new(bind_addr.ip(), 0);
            std::net::UdpSocket::bind(fallback)?
        }
        Err(err) => return Err(Box::new(err)),
    };
    let local_addr = socket.local_addr()?;
    let stun_result = match evt_tx {
        Some(evt_tx) if stun::stun_trace_enabled() => {
            stun::detect_public_endpoint_on_socket_with_trace(&socket, local_addr, |line| {
                let _ = evt_tx.send(NetEvent::Log(line));
            })
        }
        Some(evt_tx) => {
            let mut trace_lines: Vec<String> = Vec::new();
            let result =
                stun::detect_public_endpoint_on_socket_with_trace(&socket, local_addr, |line| {
                    trace_lines.push(line);
                });
            if result.is_err() {
                for line in trace_lines {
                    let _ = evt_tx.send(NetEvent::Log(line));
                }
            }
            result
        }
        None => stun::detect_public_endpoint_on_socket(&socket, local_addr),
    };
    let _ = socket.set_read_timeout(None);

    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config),
        socket,
        std::sync::Arc::new(quinn::TokioRuntime),
    )?;
    endpoint.set_default_client_config(client_config);
    Ok((endpoint, cert_der, stun_result))
}

fn make_transport_config(target_window: u64, autotune: &AutotuneConfig) -> quinn::TransportConfig {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(5)));
    if let Ok(timeout) = quinn::IdleTimeout::try_from(Duration::from_secs(60)) {
        transport.max_idle_timeout(Some(timeout));
    }

    configure_flow_control(&mut transport, target_window, autotune);
    transport
}

fn make_server_config(
    cert_der: Vec<u8>,
    key_der: Vec<u8>,
    transport: quinn::TransportConfig,
) -> Result<ServerConfig, Box<dyn std::error::Error>> {
    use quinn::rustls;
    use std::sync::Arc;

    let cert_chain = vec![rustls::pki_types::CertificateDer::from(cert_der)];
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(key_der.into());

    let mut crypto = rustls::ServerConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(cert_chain, key)?;
    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(crypto)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(crypto));
    server_config.transport = std::sync::Arc::new(transport);
    Ok(server_config)
}

fn make_client_config(mut transport: quinn::TransportConfig) -> quinn::ClientConfig {
    use quinn::rustls::{self, DigitallySignedStruct, SignatureScheme, client::danger};

    #[derive(Debug)]
    struct SkipVerifier;
    impl danger::ServerCertVerifier for SkipVerifier {
        fn verify_server_cert(
            &self,
            _end_entity: &rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[rustls::pki_types::CertificateDer<'_>],
            _server_name: &rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: rustls::pki_types::UnixTime,
        ) -> Result<danger::ServerCertVerified, rustls::Error> {
            Ok(danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
            Ok(danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &rustls::pki_types::CertificateDer<'_>,
            _dss: &DigitallySignedStruct,
        ) -> Result<danger::HandshakeSignatureValid, rustls::Error> {
            Ok(danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }

    let mut crypto = rustls::ClientConfig::builder_with_provider(
        rustls::crypto::ring::default_provider().into(),
    )
    .with_protocol_versions(&[&rustls::version::TLS13])
    .expect("tls13")
    .dangerous()
    .with_custom_certificate_verifier(std::sync::Arc::new(SkipVerifier))
    .with_no_client_auth();

    crypto.alpn_protocols = vec![b"hq-29".to_vec()];

    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto).expect("quic client config");

    let mut client = quinn::ClientConfig::new(std::sync::Arc::new(crypto));
    transport.max_concurrent_uni_streams(quinn::VarInt::from_u32(32));
    client.transport_config(std::sync::Arc::new(transport));
    client
}

fn configure_flow_control(
    transport: &mut quinn::TransportConfig,
    target_window: u64,
    autotune: &AutotuneConfig,
) {
    let clamped = autotune.clamp_target(target_window);
    let flow_window = clamped.min(quinn::VarInt::MAX.into_inner());
    let flow_window = quinn::VarInt::from_u64(flow_window)
        .expect("flow control window within QUIC varint bounds");
    transport.stream_receive_window(flow_window);
    transport.receive_window(flow_window);
    transport.send_window(clamped);
}

fn apply_autotune_target(
    connection: &quinn::Connection,
    target_window: u64,
    autotune: &AutotuneConfig,
) {
    let clamped = autotune.clamp_target(target_window);
    let flow_window = quinn::VarInt::from_u64(clamped.min(quinn::VarInt::MAX.into_inner()))
        .expect("flow control window within QUIC varint bounds");
    connection.set_receive_window(flow_window);
    connection.set_send_window(clamped);
}

fn start_liveness_task(
    connection: quinn::Connection,
    signal_tx: tokio_mpsc::UnboundedSender<ConnSignal>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let peer = connection.remote_address();
        let err = connection.closed().await;
        let _ = signal_tx.send(ConnSignal::Closed {
            peer,
            error: err.to_string(),
        });
    })
}

fn start_observe_task(
    connection: quinn::Connection,
    evt_tx: Sender<NetEvent>,
    interval: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if interval.is_zero() {
            return;
        }

        let mut ticker = tokio::time::interval(interval);
        let closed = connection.closed();
        tokio::pin!(closed);
        loop {
            tokio::select! {
                _ = &mut closed => break,
                _ = ticker.tick() => {
                    let observed = connection.remote_address();
                    let _ = send_message(
                        &connection,
                        &WireMessage::ObservedEndpoint { addr: observed },
                        &evt_tx,
                    )
                    .await;
                }
            }
        }
    })
}

fn start_autotune_task(
    connection: quinn::Connection,
    autotune: AutotuneConfig,
    autotune_state: std::sync::Arc<Mutex<AutotuneState>>,
    evt_tx: Sender<NetEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if !autotune.enabled {
            return;
        }

        let mut ticker = tokio::time::interval(autotune.sample_interval);
        let closed = connection.closed();
        tokio::pin!(closed);
        let mut last_applied = 0u64;
        let mut last_log = Instant::now();
        loop {
            tokio::select! {
                _ = &mut closed => break,
                _ = ticker.tick() => {
                    let stats = connection.stats();
                    let snapshot = {
                        let mut state = autotune_state.lock().await;
                        state.update(&stats, Instant::now())
                    };

                    if last_applied == 0 || should_refresh_window(last_applied, snapshot.target_inflight) {
                        apply_autotune_target(&connection, snapshot.target_inflight, &autotune);
                        last_applied = snapshot.target_inflight;
                    }

                    if last_log.elapsed() >= Duration::from_secs(1) {
                        let rate_mbps = snapshot.delivery_rate_max / 1_000_000.0;
                        let _ = evt_tx.send(NetEvent::Log(format!(
                            "autotune inflight: min_rtt={:.2?} srtt={:.2?} rate={:.2} Mbps bdp={} target={}",
                            snapshot.min_rtt,
                            snapshot.srtt,
                            rate_mbps,
                            format_bytes(snapshot.bdp_estimate),
                            format_bytes(snapshot.target_inflight)
                        )));
                        last_log = Instant::now();
                    }
                }
            }
        }
    })
}

fn should_refresh_window(previous: u64, new_value: u64) -> bool {
    if previous == 0 {
        return true;
    }
    let lower = previous.saturating_sub(previous / 10);
    let upper = previous + previous / 10;
    new_value < lower || new_value > upper
}

fn format_bytes(value: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value_f = value as f64;
    if value_f >= GB {
        format!("{:.2} GiB", value_f / GB)
    } else if value_f >= MB {
        format!("{:.2} MiB", value_f / MB)
    } else if value_f >= KB {
        format!("{:.1} KiB", value_f / KB)
    } else {
        format!("{value} B")
    }
}

#[cfg(test)]
mod endpoint_tests {
    use super::*;

    #[test]
    fn creates_endpoint_inside_tokio_runtime() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("addr");
            let result = make_endpoint(bind_addr, 512 * 1024, &AutotuneConfig::default(), None);
            assert!(
                result.is_ok(),
                "expected endpoint creation to succeed, got {result:?}"
            );
        });
    }

    #[test]
    fn can_connect_over_loopback() {
        let runtime = tokio::runtime::Runtime::new().expect("runtime");
        runtime.block_on(async {
            let (evt_tx, _evt_rx) = std::sync::mpsc::channel::<NetEvent>();

            let (server, _, _) = make_endpoint(
                "127.0.0.1:0".parse().expect("server bind"),
                512 * 1024,
                &AutotuneConfig::default(),
                None,
            )
            .expect("server endpoint");
            let server_addr = server.local_addr().expect("server addr");

            let (mut client, _, _) = make_endpoint(
                "127.0.0.1:0".parse().expect("client bind"),
                512 * 1024,
                &AutotuneConfig::default(),
                None,
            )
            .expect("client endpoint");

            let accept_fut = async {
                let incoming = server.accept().await.expect("incoming");
                incoming.await.expect("server connection")
            };

            let connect_fut = async {
                connect_peer(&mut client, server_addr, &evt_tx)
                    .await
                    .expect("client connection")
            };

            let _ = tokio::time::timeout(Duration::from_secs(3), async {
                let (_server_conn, _client_conn) = tokio::join!(accept_fut, connect_fut);
            })
            .await
            .expect("connect timeout");
        });
    }
}

async fn connect_peer(
    endpoint: &Endpoint,
    peer: SocketAddr,
    evt_tx: &Sender<NetEvent>,
) -> Option<quinn::Connection> {
    match endpoint.connect(peer, "pasta") {
        Ok(connecting) => match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
            Ok(Ok(connection)) => Some(connection),
            Ok(Err(err)) => {
                let _ = evt_tx.send(NetEvent::Log(format!("erro ao conectar {err}")));
                None
            }
            Err(_) => {
                let _ = evt_tx.send(NetEvent::Log(format!(
                    "tempo esgotado ao conectar {peer} (verifique NAT/firewall)"
                )));
                None
            }
        },
        Err(err) => {
            let _ = evt_tx.send(NetEvent::Log(format!("erro ao iniciar conexao {err}")));
            None
        }
    }
}

async fn quick_probe_peer(endpoint: &Endpoint, peer: SocketAddr) -> Result<Duration, String> {
    let started = Instant::now();
    let connecting = endpoint
        .connect(peer, "pasta")
        .map_err(|err| format!("erro ao iniciar teste {err}"))?;

    match tokio::time::timeout(PROBE_TIMEOUT, connecting).await {
        Ok(Ok(connection)) => {
            let elapsed = started.elapsed();
            connection.close(0u32.into(), b"probe");
            Ok(elapsed)
        }
        Ok(Err(err)) => Err(format!("erro ao conectar: {err}")),
        Err(_) => Err(format!(
            "sem resposta em {}s (firewall/NAT?)",
            PROBE_TIMEOUT.as_secs()
        )),
    }
}

fn setup_connection_reader(
    connection: quinn::Connection,
    inbound_tx: &mut tokio_mpsc::UnboundedSender<InboundFrame>,
    evt_tx: &Sender<NetEvent>,
    reader_task: &mut Option<tokio::task::JoinHandle<()>>,
) {
    let inbound = inbound_tx.clone();
    let evt_tx = evt_tx.clone();
    let handle = tokio::spawn(async move {
        loop {
            match connection.accept_uni().await {
                Ok(mut stream) => match read_frame(&mut stream).await {
                    Ok(Some(payload)) => match decode_payload(&payload) {
                        Ok(WireMessage::FileMeta {
                            file_id,
                            name,
                            size,
                        }) => {
                            let _ = inbound.send(InboundFrame::FileStream {
                                file_id,
                                name,
                                size,
                                from: connection.remote_address(),
                                stream,
                            });
                        }
                        Ok(message) => {
                            let _ = inbound
                                .send(InboundFrame::Control(message, connection.remote_address()));
                        }
                        Err(err) => {
                            let base64_preview =
                                base64::engine::general_purpose::STANDARD.encode(&payload);
                            let preview = base64_preview.chars().take(48).collect::<String>();
                            let _ = evt_tx.send(NetEvent::Log(format!(
                                "erro ao decodificar {err}; payload(base64)={preview}"
                            )));
                        }
                    },
                    Ok(None) => {}
                    Err(err) => {
                        let _ = evt_tx.send(NetEvent::Log(format!("stream encerrado {err}")));
                    }
                },
                Err(err) => {
                    let _ = evt_tx.send(NetEvent::Log(format!("conexao encerrada {err}")));
                    break;
                }
            }
        }
    });
    *reader_task = Some(handle);
}

async fn read_frame(stream: &mut quinn::RecvStream) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    let mut read_len = 0usize;
    while read_len < len_buf.len() {
        let n = tokio::io::AsyncReadExt::read(stream, &mut len_buf[read_len..]).await?;
        if n == 0 {
            if read_len == 0 {
                return Ok(None);
            }
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "frame header incompleto",
            ));
        }
        read_len += n;
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    let mut read_payload = 0usize;
    while read_payload < len {
        let n = tokio::io::AsyncReadExt::read(stream, &mut payload[read_payload..]).await?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "frame payload incompleto",
            ));
        }
        read_payload += n;
    }

    Ok(Some(payload))
}

#[cfg(test)]
mod tests {
    use super::stun::stun_server_list;
    use super::*;
    use crate::net::serialize_message;

    #[test]
    fn stun_defaults_not_empty() {
        assert!(!stun_server_list("0.0.0.0:5000".parse().unwrap()).is_empty());
        assert!(!stun_server_list("[::]:5000".parse().unwrap()).is_empty());
    }

    #[test]
    fn wire_message_variant_tags_stable() {
        fn tag(message: &WireMessage) -> u32 {
            let bytes = serialize_message(message).expect("serialize wire message");
            u32::from_le_bytes(bytes[..4].try_into().expect("tag bytes"))
        }

        assert_eq!(
            tag(&WireMessage::Hello {
                version: PROTOCOL_VERSION,
            }),
            0
        );
        assert_eq!(tag(&WireMessage::Punch { nonce: 0 }), 1);
        assert_eq!(tag(&WireMessage::Cancel { file_id: 0 }), 2);
        assert_eq!(
            tag(&WireMessage::FileMeta {
                file_id: 0,
                name: "file.bin".to_string(),
                size: 0,
            }),
            3
        );
        assert_eq!(
            tag(&WireMessage::FileChunk {
                file_id: 0,
                data: Vec::new(),
            }),
            4
        );
        assert_eq!(tag(&WireMessage::FileDone { file_id: 0 }), 5);
        assert_eq!(
            tag(&WireMessage::ObservedEndpoint {
                addr: "127.0.0.1:1".parse().expect("addr"),
            }),
            6
        );
        assert_eq!(tag(&WireMessage::Ping { nonce: 0 }), 7);
        assert_eq!(tag(&WireMessage::Pong { nonce: 0 }), 8);
    }
}
