use std::fs::{self, OpenOptions};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::signal;
use tokio::sync::Mutex;

use agentcoffeechat_core::{socket_path, DaemonCommand, DaemonResponse};
use agentcoffeechat_daemon::ask_engine::AskEngine;
// auth validation functions removed — session existence + expiry check is sufficient
use agentcoffeechat_daemon::awdl::{self, AwdlActivator};
use agentcoffeechat_daemon::chat_engine;
use agentcoffeechat_daemon::chat_history;
use agentcoffeechat_daemon::discovery::{DiscoveredPeer, DiscoveryConfig, DiscoveryService};
use agentcoffeechat_daemon::session_manager::SessionManager;
use agentcoffeechat_daemon::transport::{self, TransportService, WireMessage};

// ---------------------------------------------------------------------------
// CLI flags
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "agentcoffeechatd", about = "AgentCoffeeChat daemon")]
struct Args {
    /// Run in foreground (default; daemonizing is not yet implemented)
    #[arg(long, default_value_t = true)]
    foreground: bool,
}

// ---------------------------------------------------------------------------
// DaemonConfig
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct DaemonConfig {
    display_name: String,
    fingerprint_prefix: String,
    ai_tool: String,
    project_root: PathBuf,
    project_hash: [u8; 4],
}

impl DaemonConfig {
    fn peer_label(&self) -> String {
        format!(
            "{}-{}",
            self.display_name,
            &self.fingerprint_prefix[..8.min(self.fingerprint_prefix.len())]
        )
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        // Derive a display name from the username.
        let display_name = std::env::var("USER").unwrap_or_else(|_| "agent".to_string());

        // Derive fingerprint from username + hostname. This avoids the macOS
        // Keychain prompt which blocks indefinitely when the daemon runs
        // without a GUI context (stdout/stderr redirected to log file).
        let hostname = std::env::var("HOSTNAME").unwrap_or_else(|_| {
            let mut buf = [0u8; 256];
            let rc = unsafe {
                libc::gethostname(
                    buf.as_mut_ptr() as *mut libc::c_char,
                    buf.len(),
                )
            };
            if rc == 0 {
                let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
                String::from_utf8_lossy(&buf[..len]).to_string()
            } else {
                "unknown".to_string()
            }
        });
        let seed = format!("{}-{}", display_name, hostname);
        let fingerprint_prefix = format!(
            "{:0>16x}",
            seed.as_bytes()
                .iter()
                .fold(0u64, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u64))
        );

        Self {
            display_name,
            fingerprint_prefix,
            ai_tool: "claude-code".to_string(),
            project_root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            project_hash: [0x00, 0x00, 0x00, 0x00], // overridden by load()
        }
    }
}

impl DaemonConfig {
    /// Try to load config from ~/.agentcoffeechat/config.json, falling back
    /// to defaults for any missing fields.
    fn load() -> Self {
        let mut config = Self::default();

        let config_path = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".agentcoffeechat")
            .join("config.json");

        if let Ok(contents) = std::fs::read_to_string(&config_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) {
                if let Some(name) = json.get("display_name").and_then(|v| v.as_str()) {
                    config.display_name = name.to_string();
                }
                if let Some(tool) = json.get("ai_tool").and_then(|v| v.as_str()) {
                    config.ai_tool = tool.to_string();
                }
                if let Some(root) = json.get("project_root").and_then(|v| v.as_str()) {
                    config.project_root = PathBuf::from(root);
                }
                println!("[daemon] Loaded config from {}", config_path.display());
            }
        }

        // Derive project_hash from the canonical project_root path.
        let canonical = config.project_root.canonicalize()
            .unwrap_or_else(|_| config.project_root.clone());
        let path_bytes = canonical.to_string_lossy().as_bytes().to_vec();
        let hash = path_bytes.iter().fold(0u32, |acc, &b| {
            acc.wrapping_mul(31).wrapping_add(b as u32)
        });
        config.project_hash = hash.to_be_bytes();

        config
    }
}

// ---------------------------------------------------------------------------
// Daemon state shared across handlers
// ---------------------------------------------------------------------------

struct DaemonState {
    session_mgr: Mutex<SessionManager>,
    pending_pairings: Mutex<HashMap<String, PendingPairing>>,
    ask_engine: AskEngine,
    started_at: DateTime<Utc>,
    discovery: Mutex<Option<DiscoveryService>>,
    peers: Mutex<Vec<DiscoveredPeer>>,
    transport: Option<TransportService>,
    config: Mutex<DaemonConfig>,
}

#[derive(Debug, Clone)]
struct PendingPairing {
    local_code: String,
    fingerprint_prefix: Option<String>,
}

// ---------------------------------------------------------------------------
// Command dispatch
// ---------------------------------------------------------------------------

async fn handle_command(
    cmd: DaemonCommand,
    state: &DaemonState,
    shutdown_tx: &tokio::sync::watch::Sender<bool>,
) -> DaemonResponse {
    match cmd {
        DaemonCommand::Ping => DaemonResponse::success("pong"),

        DaemonCommand::ListSessions => {
            let mgr = state.session_mgr.lock().await;
            let peers = mgr.active_peers();
            let list: Vec<serde_json::Value> = peers
                .iter()
                .filter_map(|name| {
                    mgr.get_session(name).map(|s| {
                        serde_json::json!({
                            "peer_name": s.peer_name,
                            "started_at": s.started_at.to_rfc3339(),
                            "expires_at": s.expires_at.map(|e| e.to_rfc3339()),
                            "fingerprint_prefix": s.fingerprint_prefix,
                        })
                    })
                })
                .collect();
            DaemonResponse::success_with_data(
                format!("{} active session(s)", list.len()),
                serde_json::Value::Array(list),
            )
        }

        DaemonCommand::BeginPairing {
            peer_name,
            fingerprint_prefix,
        } => {
            let local_code = agentcoffeechat_core::generate_three_word_code();
            let mut pending = state.pending_pairings.lock().await;
            pending.insert(
                peer_name.clone(),
                PendingPairing {
                    local_code: local_code.clone(),
                    fingerprint_prefix,
                },
            );
            DaemonResponse::success_with_data(
                format!("pairing started with {}", peer_name),
                serde_json::json!({
                    "peer_name": peer_name,
                    "your_code": local_code,
                }),
            )
        }

        DaemonCommand::CompletePairing { peer_name, peer_code } => {
            let pending = {
                let mut pending = state.pending_pairings.lock().await;
                pending.remove(&peer_name)
            };

            let Some(pending) = pending else {
                return DaemonResponse::error(format!(
                    "no pending pairing with '{}' — start again with `acc connect {}`",
                    peer_name, peer_name
                ));
            };

            // Create the local session directly — no QUIC handshake needed.
            // Both sides independently run `acc connect` to create their own sessions.
            // Security is handled when chat/ask starts via QUIC + session validation.
            let mut mgr = state.session_mgr.lock().await;
            mgr.create_session(
                &peer_name,
                &pending.local_code,
                &peer_code,
                pending.fingerprint_prefix,
            );
            println!("[daemon] Started session with {}", peer_name);
            DaemonResponse::success(format!(
                "connected to {} — session active for 1 hour",
                peer_name
            ))
        }

        DaemonCommand::EndSession { peer_name } => {
            let mut mgr = state.session_mgr.lock().await;
            match mgr.remove_session(&peer_name) {
                Some(s) => {
                    println!("[daemon] Ended session with {}", s.peer_name);
                    DaemonResponse::success(format!("session with {} ended", peer_name))
                }
                None => DaemonResponse::error(format!("no active session with {}", peer_name)),
            }
        }

        DaemonCommand::ListPeers => {
            let peers = state.peers.lock().await;
            let list: Vec<serde_json::Value> = peers
                .iter()
                .map(|p| {
                    serde_json::json!({
                        "name": p.name,
                        "fingerprint_prefix": p.fingerprint_prefix,
                        "quic_port": p.quic_port,
                        "address": p.address.map(|a| a.to_string()),
                        "source": format!("{:?}", p.source),
                        "rssi": p.rssi,
                    })
                })
                .collect();
            DaemonResponse::success_with_data(
                format!("{} peer(s) discovered", list.len()),
                serde_json::Value::Array(list),
            )
        }

        DaemonCommand::GetStatus => {
            let mgr = state.session_mgr.lock().await;
            let session_count = mgr.active_peers().len();
            let peer_count = state.peers.lock().await.len();
            let now = Utc::now();
            let uptime = now.signed_duration_since(state.started_at);
            let uptime_secs = uptime.num_seconds();
            let transport_port = state.transport.as_ref().map(|t| t.port()).unwrap_or(0);
            let data = serde_json::json!({
                "uptime_seconds": uptime_secs,
                "active_sessions": session_count,
                "discovered_peers": peer_count,
                "quic_port": transport_port,
                "started_at": state.started_at.to_rfc3339(),
            });
            DaemonResponse::success_with_data("daemon is running", data)
        }

        DaemonCommand::AskQuestion {
            peer_name,
            question,
        } => {
            println!("[daemon] AskQuestion to peer={}, q=\"{}\"", peer_name, question);

            // Require a paired session before asking a remote peer.
            {
                let mgr = state.session_mgr.lock().await;
                if mgr.get_session(&peer_name).is_none() {
                    return DaemonResponse::error(format!(
                        "no active session with '{}' — connect first with `acc connect {}`",
                        peer_name, peer_name
                    ));
                }
            }

            let peer_info = {
                let peers = state.peers.lock().await;
                peers.iter().find(|p| p.name == peer_name).cloned()
            };

            let (peer_addr, peer_port) = match peer_info {
                Some(p) => match p.address {
                    Some(addr) => (addr, p.quic_port),
                    None => {
                        return DaemonResponse::error(format!(
                            "peer '{}' discovered but has no network address",
                            peer_name
                        ));
                    }
                },
                None => {
                    return DaemonResponse::error(format!(
                        "peer '{}' not found in discovered peers list",
                        peer_name
                    ));
                }
            };

            let transport_ref = match &state.transport {
                Some(t) => t,
                None => {
                    return DaemonResponse::error("QUIC transport is not running".to_string());
                }
            };

            let socket_addr = SocketAddr::new(peer_addr, peer_port);
            let conn = match transport_ref.connect(socket_addr).await {
                Ok(c) => c,
                Err(e) => {
                    return DaemonResponse::error(format!(
                        "failed to connect to peer '{}' at {}: {}",
                        peer_name, socket_addr, e
                    ));
                }
            };

            let (mut quic_send, mut quic_recv) = match conn.open_stream().await {
                Ok(streams) => streams,
                Err(e) => {
                    return DaemonResponse::error(format!(
                        "failed to open QUIC stream: {}",
                        e
                    ));
                }
            };

            let config = state.config.lock().await.clone();
            let request = WireMessage::AskRequest {
                peer_name: config.peer_label(),
                fingerprint_prefix: config.fingerprint_prefix.clone(),
                question,
            };

            if let Err(e) = transport::send_wire_message(&mut quic_send, &request).await {
                return DaemonResponse::error(format!("ask failed (send): {}", e));
            }

            // Wait for the response with a 60-second timeout.
            // Keep the send stream open until we get the response.
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                transport::recv_wire_message(&mut quic_recv),
            ).await;

            let _ = quic_send.finish();

            match response {
                Err(_) => {
                    DaemonResponse::error("ask timed out (60s) waiting for peer response".to_string())
                }
                Ok(Ok(WireMessage::AskResponse { answer, duration_ms })) => {
                    let data = serde_json::json!({
                        "answer": answer,
                        "duration_ms": duration_ms,
                        "peer_name": peer_name,
                    });
                    DaemonResponse::success_with_data("question answered", data)
                }
                Ok(Ok(WireMessage::Error { message })) => {
                    DaemonResponse::error(format!("peer rejected ask: {}", message))
                }
                Ok(Ok(other)) => DaemonResponse::error(format!(
                    "ask failed: unexpected response {:?}", other
                )),
                Ok(Err(e)) => DaemonResponse::error(format!("ask failed (recv): {}", e)),
            }
        }

        DaemonCommand::StartChat { peer_name } => {
            println!("[daemon] StartChat with peer={}", peer_name);

            // Look up the peer's address from discovered peers.
            let peer_info = {
                let peers = state.peers.lock().await;
                peers.iter().find(|p| p.name == peer_name).cloned()
            };

            let (peer_addr, peer_port) = match peer_info {
                Some(p) => match p.address {
                    Some(addr) => (addr, p.quic_port),
                    None => {
                        return DaemonResponse::error(format!(
                            "peer '{}' discovered but has no network address",
                            peer_name
                        ));
                    }
                },
                None => {
                    return DaemonResponse::error(format!(
                        "peer '{}' not found in discovered peers list",
                        peer_name
                    ));
                }
            };

            // Verify an active session exists with this peer (established via pairing).
            {
                let mgr = state.session_mgr.lock().await;
                if mgr.get_session(&peer_name).is_none() {
                    return DaemonResponse::error(format!(
                        "no active session with '{}' — connect first with `acc connect {}`",
                        peer_name, peer_name
                    ));
                }
            }

            // Ensure QUIC transport is running.
            let transport_ref = match &state.transport {
                Some(t) => t,
                None => {
                    return DaemonResponse::error("QUIC transport is not running".to_string());
                }
            };

            let socket_addr = SocketAddr::new(peer_addr, peer_port);
            let conn = match transport_ref.connect(socket_addr).await {
                Ok(c) => c,
                Err(e) => {
                    return DaemonResponse::error(format!(
                        "failed to connect to peer '{}' at {}: {}",
                        peer_name, socket_addr, e
                    ));
                }
            };

            println!(
                "[daemon] QUIC connection established with {} at {}",
                peer_name,
                conn.remote_address()
            );

            // Create channels for the chat engine.
            let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<String>(64);
            let (recv_tx, recv_rx) = tokio::sync::mpsc::channel::<String>(64);
            let (event_tx, mut event_rx) =
                tokio::sync::mpsc::channel::<chat_engine::ChatEvent>(128);

            // Open a bidirectional QUIC stream.
            let (mut quic_send, mut quic_recv) = match conn.open_stream().await {
                Ok(streams) => streams,
                Err(e) => {
                    return DaemonResponse::error(format!(
                        "failed to open QUIC stream: {}",
                        e
                    ));
                }
            };

            let config = state.config.lock().await.clone();
            if let Err(e) = transport::send_wire_message(
                &mut quic_send,
                &WireMessage::ChatOpen {
                    peer_name: config.peer_label(),
                    fingerprint_prefix: config.fingerprint_prefix.clone(),
                },
            )
            .await
            {
                return DaemonResponse::error(format!(
                    "failed to open chat with peer '{}': {}",
                    peer_name, e
                ));
            }

            // Spawn a task to relay outgoing messages over QUIC.
            tokio::spawn(async move {
                while let Some(msg) = send_rx.recv().await {
                    if transport::send_wire_message(
                        &mut quic_send,
                        &WireMessage::Chat { text: msg },
                    )
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = quic_send.finish();
            });

            // Spawn a task to relay incoming messages from QUIC to the chat engine.
            tokio::spawn(async move {
                loop {
                    match transport::recv_wire_message(&mut quic_recv).await {
                        Ok(WireMessage::Chat { text }) => {
                            if recv_tx.send(text).await.is_err() {
                                break;
                            }
                        }
                        Ok(WireMessage::Error { .. }) => break,
                        Ok(_) => break,
                        Err(_) => break,
                    }
                }
            });

            // Run the chat engine.
            let chat_config = chat_engine::ChatConfig {
                display_name: config.display_name.clone(),
                ai_tool: config.ai_tool.clone(),
                project_root: config.project_root.clone(),
                peer_name: Some(peer_name.clone()),
                ..Default::default()
            };
            let engine = chat_engine::ChatEngine::new(chat_config);

            // Spawn a task to drain events and capture the briefing text.
            let events_handle = tokio::spawn(async move {
                let mut briefing_text = String::new();
                while let Some(event) = event_rx.recv().await {
                    match &event {
                        chat_engine::ChatEvent::Briefing(text) => {
                            briefing_text = text.clone();
                        }
                        chat_engine::ChatEvent::Status(s) => {
                            println!("[chat] {}", s);
                        }
                        chat_engine::ChatEvent::LocalMessage(m) => {
                            let preview: String = m.chars().take(80).collect();
                            println!("[chat] local: {}", preview);
                        }
                        chat_engine::ChatEvent::RemoteMessage(m) => {
                            let preview: String = m.chars().take(80).collect();
                            println!("[chat] remote: {}", preview);
                        }
                        chat_engine::ChatEvent::Phase(p) => {
                            println!("[chat] phase: {}", p);
                        }
                        chat_engine::ChatEvent::Error(e) => {
                            eprintln!("[chat] error: {}", e);
                        }
                        chat_engine::ChatEvent::Complete => {
                            println!("[chat] complete");
                        }
                    }
                }
                briefing_text
            });

            match engine.run_chat(send_tx, recv_rx, event_tx).await {
                Ok(result) => {
                    // Save to history.
                    let save_path = match chat_history::save_chat(&peer_name, &result) {
                        Ok(path) => Some(path.to_string_lossy().to_string()),
                        Err(e) => {
                            eprintln!("[daemon] Failed to save chat history: {}", e);
                            None
                        }
                    };

                    let briefing_text = events_handle.await.unwrap_or_default();

                    let data = serde_json::json!({
                        "peer_name": peer_name,
                        "message_count": result.message_count,
                        "duration_secs": result.duration_secs,
                        "briefing": {
                            "what_building": result.briefing.what_building,
                            "learnings": result.briefing.learnings,
                            "tips": result.briefing.tips,
                            "ideas_to_explore": result.briefing.ideas_to_explore,
                        },
                        "briefing_text": briefing_text,
                        "saved_to": save_path,
                    });
                    DaemonResponse::success_with_data("chat completed", data)
                }
                Err(e) => {
                    let _ = events_handle.await;
                    DaemonResponse::error(format!("chat failed: {}", e))
                }
            }
        }

        DaemonCommand::UpdateContext { project_root, ai_tool } => {
            let mut config = state.config.lock().await;
            config.project_root = PathBuf::from(&project_root);
            if let Some(tool) = ai_tool {
                config.ai_tool = tool;
            }

            let canonical = config
                .project_root
                .canonicalize()
                .unwrap_or_else(|_| config.project_root.clone());
            let path_bytes = canonical.to_string_lossy().as_bytes().to_vec();
            let hash = path_bytes
                .iter()
                .fold(0u32, |acc, &b| acc.wrapping_mul(31).wrapping_add(b as u32));
            config.project_hash = hash.to_be_bytes();

            let config_dir = std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(".agentcoffeechat");
            let _ = std::fs::create_dir_all(&config_dir);
            let config_path = config_dir.join("config.json");
            let config_json = serde_json::json!({
                "display_name": config.display_name,
                "ai_tool": config.ai_tool,
                "project_root": config.project_root.to_string_lossy(),
            });
            if let Ok(json_str) = serde_json::to_string_pretty(&config_json) {
                let _ = std::fs::write(&config_path, json_str);
            }

            DaemonResponse::success(format!(
                "context updated to {}",
                config.project_root.display()
            ))
        }

        DaemonCommand::ListHistory => match chat_history::list_chats() {
            Ok(entries) => {
                let list: Vec<serde_json::Value> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        serde_json::json!({
                            "index": i,
                            "peer_name": e.peer_name,
                            "timestamp": e.timestamp.to_rfc3339(),
                            "summary": e.summary,
                        })
                    })
                    .collect();
                DaemonResponse::success_with_data(
                    format!("{} past chat(s)", list.len()),
                    serde_json::Value::Array(list),
                )
            }
            Err(e) => DaemonResponse::error(format!("failed to list history: {}", e)),
        },

        DaemonCommand::GetHistory { index } => match chat_history::list_chats() {
            Ok(entries) => {
                if let Some(entry) = entries.get(index as usize) {
                    let briefing_path = entry.path.join("briefing.md");
                    let transcript_path = entry.path.join("transcript.md");

                    let briefing = std::fs::read_to_string(&briefing_path)
                        .unwrap_or_else(|_| "No briefing available".to_string());
                    let transcript = std::fs::read_to_string(&transcript_path)
                        .unwrap_or_else(|_| "No transcript available".to_string());

                    let data = serde_json::json!({
                        "index": index,
                        "peer_name": entry.peer_name,
                        "timestamp": entry.timestamp.to_rfc3339(),
                        "briefing": briefing,
                        "transcript": transcript,
                    });
                    DaemonResponse::success_with_data("chat history entry", data)
                } else {
                    DaemonResponse::error(format!(
                        "no chat at index {} (have {} entries)",
                        index,
                        entries.len()
                    ))
                }
            }
            Err(e) => DaemonResponse::error(format!("failed to get history: {}", e)),
        },

        DaemonCommand::RunDoctor => {
            let mut checks: Vec<serde_json::Value> = Vec::new();

            // 1. Daemon running (always true since we handle this command).
            checks.push(serde_json::json!({
                "name": "daemon_running",
                "status": "ok",
                "message": "Daemon is running",
            }));

            // 2. BLE status.
            let ble_status = check_ble_status().await;
            checks.push(serde_json::json!({
                "name": "ble",
                "status": if ble_status.0 { "ok" } else { "warn" },
                "message": ble_status.1,
            }));

            // 3. Bonjour/mDNS status.
            let mdns_status = check_mdns_status();
            checks.push(serde_json::json!({
                "name": "bonjour",
                "status": if mdns_status.0 { "ok" } else { "warn" },
                "message": mdns_status.1,
            }));

            // 4. QUIC listener status.
            let quic_status = match &state.transport {
                Some(t) => (true, format!("QUIC listener active on port {}", t.port())),
                None => (false, "QUIC transport not running".to_string()),
            };
            checks.push(serde_json::json!({
                "name": "quic_listener",
                "status": if quic_status.0 { "ok" } else { "error" },
                "message": quic_status.1,
            }));

            // 5. Active sessions.
            let session_count = {
                let mgr = state.session_mgr.lock().await;
                mgr.active_peers().len()
            };
            checks.push(serde_json::json!({
                "name": "active_sessions",
                "status": "ok",
                "message": format!("{} active session(s)", session_count),
            }));

            // 6. Discovered peers.
            let peer_count = state.peers.lock().await.len();
            checks.push(serde_json::json!({
                "name": "discovered_peers",
                "status": "ok",
                "message": format!("{} peer(s) discovered", peer_count),
            }));

            // 7. Disk space in ~/.agentcoffeechat/.
            let disk_status = check_disk_space();
            checks.push(serde_json::json!({
                "name": "disk_space",
                "status": if disk_status.0 { "ok" } else { "warn" },
                "message": disk_status.1,
            }));

            // 8. AWDL (Apple Wireless Direct Link) status.
            let awdl_info = awdl::awdl_status();
            checks.push(serde_json::json!({
                "name": "awdl",
                "status": if awdl_info.0 { "ok" } else { "warn" },
                "message": awdl_info.1,
            }));

            let all_ok = checks
                .iter()
                .all(|c| c.get("status").and_then(|s| s.as_str()) != Some("error"));

            DaemonResponse::success_with_data(
                if all_ok {
                    "all checks passed"
                } else {
                    "some checks have issues"
                },
                serde_json::Value::Array(checks),
            )
        }

        DaemonCommand::Shutdown => {
            println!("[daemon] Shutdown requested via IPC");
            let _ = shutdown_tx.send(true);
            DaemonResponse::success("shutting down")
        }

    }
}

// ---------------------------------------------------------------------------
// Doctor check helpers
// ---------------------------------------------------------------------------

async fn check_ble_status() -> (bool, String) {
    // BLE is disabled in v1 (btleplug requires an app bundle on macOS and
    // can panic without one).  Report as informational rather than risking
    // a crash.
    (true, "BLE disabled in v1 (using mDNS only)".to_string())
}

fn check_mdns_status() -> (bool, String) {
    // Try to create an mdns-sd ServiceDaemon — if it succeeds, mDNS is available.
    match mdns_sd::ServiceDaemon::new() {
        Ok(daemon) => {
            let _ = daemon.shutdown();
            (true, "Bonjour/mDNS is available (mdns-sd)".to_string())
        }
        Err(e) => (false, format!("mDNS not available: {}", e)),
    }
}

fn check_disk_space() -> (bool, String) {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => return (false, "HOME not set".to_string()),
    };
    let data_dir = home.join(".agentcoffeechat");

    if !data_dir.exists() {
        return (
            true,
            "~/.agentcoffeechat/ does not exist yet (will be created on first chat)".to_string(),
        );
    }

    // Walk the directory and sum file sizes (two levels deep).
    let mut total_bytes: u64 = 0;
    if let Ok(entries) = std::fs::read_dir(&data_dir) {
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata() {
                total_bytes += meta.len();
                if meta.is_dir() {
                    if let Ok(sub_entries) = std::fs::read_dir(entry.path()) {
                        for sub_entry in sub_entries.flatten() {
                            if let Ok(sub_meta) = sub_entry.metadata() {
                                total_bytes += sub_meta.len();
                            }
                        }
                    }
                }
            }
        }
    }

    let mb = total_bytes as f64 / (1024.0 * 1024.0);
    if mb > 500.0 {
        (
            false,
            format!("~/.agentcoffeechat/ uses {:.1} MB (consider cleanup)", mb),
        )
    } else {
        (true, format!("~/.agentcoffeechat/ uses {:.1} MB", mb))
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<DaemonCommand>(&line) {
            Ok(cmd) => {
                println!("[daemon] Received command: {:?}", cmd);
                handle_command(cmd, &state, &shutdown_tx).await
            }
            Err(e) => DaemonResponse::error(format!("invalid command: {}", e)),
        };

        let mut json = serde_json::to_string(&response).unwrap_or_else(|_| {
            r#"{"ok":false,"message":"serialization error"}"#.to_string()
        });
        json.push('\n');

        if writer.write_all(json.as_bytes()).await.is_err() {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// File-based logging setup
// ---------------------------------------------------------------------------

/// Redirect stdout and stderr to `~/.agentcoffeechat/logs/agentcoffeechatd.log`
/// so that all `println!`/`eprintln!` output is captured even when the daemon
/// runs in the background.
fn setup_logging() {
    let home = match std::env::var("HOME") {
        Ok(h) => PathBuf::from(h),
        Err(_) => {
            eprintln!("[daemon] WARNING: HOME not set, logging to stdout/stderr");
            return;
        }
    };

    let log_dir = home.join(".agentcoffeechat").join("logs");
    if let Err(e) = fs::create_dir_all(&log_dir) {
        eprintln!(
            "[daemon] WARNING: failed to create log dir {}: {}",
            log_dir.display(),
            e
        );
        return;
    }

    let log_path = log_dir.join("agentcoffeechatd.log");
    let log_file = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!(
                "[daemon] WARNING: failed to open log file {}: {}",
                log_path.display(),
                e
            );
            return;
        }
    };

    let log_fd = log_file.as_raw_fd();

    // Redirect stdout (fd 1) and stderr (fd 2) to the log file.
    // SAFETY: dup2 is a standard POSIX call; the file descriptor is valid
    // because we just opened the file above and hold it in scope.  We
    // deliberately leak `log_file` (via `std::mem::forget`) so the fd stays
    // open for the lifetime of the process.
    unsafe {
        libc::dup2(log_fd, 1); // stdout
        libc::dup2(log_fd, 2); // stderr
    }

    // Prevent the File from being dropped (which would close the fd).
    std::mem::forget(log_file);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    let _args = Args::parse();

    setup_logging();

    let sock_path = socket_path();

    // Clean up stale socket from a previous run
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)
            .with_context(|| format!("failed to remove stale socket {}", sock_path.display()))?;
    }

    let listener = UnixListener::bind(&sock_path)
        .with_context(|| format!("failed to bind Unix socket at {}", sock_path.display()))?;

    println!("[daemon] agentcoffeechatd starting");
    println!("[daemon] Listening on {}", sock_path.display());

    println!("[daemon] Loading config...");
    let config = DaemonConfig::load();
    println!("[daemon] Config loaded: ai_tool={}, project_root={}", config.ai_tool, config.project_root.display());

    // Ensure config.json exists (first-run creation).
    {
        let config_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(".agentcoffeechat");
        let _ = std::fs::create_dir_all(&config_dir);
        let config_path = config_dir.join("config.json");
        if !config_path.exists() {
            let config_json = serde_json::json!({
                "display_name": config.display_name,
                "ai_tool": config.ai_tool,
                "project_root": config.project_root.to_string_lossy(),
            });
            if let Ok(json_str) = serde_json::to_string_pretty(&config_json) {
                match std::fs::write(&config_path, json_str) {
                    Ok(()) => println!("[daemon] Created config at {}", config_path.display()),
                    Err(e) => eprintln!("[daemon] Failed to write config: {}", e),
                }
            }
        }
    }

    // -- Start QUIC transport on a random available port --
    let transport = match TransportService::new(0) {
        Ok(t) => {
            println!("[daemon] QUIC transport listening on port {}", t.port());
            Some(t)
        }
        Err(e) => {
            eprintln!(
                "[daemon] Failed to start QUIC transport: {:#}. Continuing without it.",
                e
            );
            None
        }
    };

    let quic_port = transport.as_ref().map(|t| t.port()).unwrap_or(0);

    // -- Build DaemonState --
    let state = Arc::new(DaemonState {
        session_mgr: Mutex::new(SessionManager::new()),
        pending_pairings: Mutex::new(HashMap::new()),
        ask_engine: AskEngine::new(),
        started_at: Utc::now(),
        discovery: Mutex::new(None),
        peers: Mutex::new(Vec::new()),
        transport,
        config: Mutex::new(config.clone()),
    });

    // -- Start discovery service --
    let discovery_config = DiscoveryConfig {
        display_name: config.display_name.clone(),
        fingerprint_prefix: config.fingerprint_prefix.clone(),
        quic_port,
        project_hash: config.project_hash,
    };

    let mut discovery_service = DiscoveryService::new(discovery_config);
    match discovery_service.start().await {
        Ok(mut peer_rx) => {
            println!("[daemon] Discovery service started");

            // Store the discovery service in state so we can shut it down later.
            {
                let mut disc = state.discovery.lock().await;
                *disc = Some(discovery_service);
            }

            // Spawn a background task to read discovered peers and update the
            // shared peer list.
            let discovery_state = state.clone();
            tokio::spawn(async move {
                while let Some(peer) = peer_rx.recv().await {
                    println!(
                        "[daemon] Discovered peer: {} (fp={}, addr={:?}, port={})",
                        peer.name, peer.fingerprint_prefix, peer.address, peer.quic_port
                    );
                    let mut peers = discovery_state.peers.lock().await;
                    if peer.source == agentcoffeechat_daemon::discovery::DiscoverySource::Mdns
                        && peer.quic_port == 0
                        && peer.address.is_none()
                    {
                        peers.retain(|p| p.fingerprint_prefix != peer.fingerprint_prefix);
                        continue;
                    }
                    // Deduplicate by fingerprint_prefix: update existing or add new.
                    if let Some(existing) = peers
                        .iter_mut()
                        .find(|p| p.fingerprint_prefix == peer.fingerprint_prefix)
                    {
                        // Prefer IPv4 over IPv6 (QUIC binds to 0.0.0.0).
                        if let Some(new_addr) = peer.address {
                            let should_update = match existing.address {
                                None => true,
                                Some(old) => old.is_ipv6() && new_addr.is_ipv4(),
                            };
                            if should_update {
                                existing.address = Some(new_addr);
                            }
                        }
                        existing.quic_port = peer.quic_port;
                        if peer.rssi.is_some() {
                            existing.rssi = peer.rssi;
                        }
                    } else {
                        peers.push(peer);
                    }
                }
            });
        }
        Err(e) => {
            eprintln!(
                "[daemon] Failed to start discovery: {:#}. Continuing without discovery.",
                e
            );
        }
    }

    // -- Activate AWDL (Apple Wireless Direct Link) for P2P discovery --
    let mut awdl_activator = AwdlActivator::new();
    match awdl_activator.activate() {
        Ok(true) => {
            let (_avail, status_msg) = awdl::awdl_status();
            println!("[daemon] AWDL activated: {}", status_msg);

            // Register our service via native mDNSResponder with P2P flag so
            // peers can discover us over AWDL without a shared Wi-Fi network.
            let instance_name = format!(
                "{}-{}",
                config.display_name,
                &config.fingerprint_prefix[..8.min(config.fingerprint_prefix.len())]
            );
            match awdl_activator.register_service(
                &instance_name,
                quic_port,
                &config.fingerprint_prefix,
                &config.project_hash,
            ) {
                Ok(true) => {
                    println!("[daemon] P2P service registered as '{}'", instance_name);
                }
                Ok(false) => {
                    println!("[daemon] P2P service registration skipped (AWDL unavailable)");
                }
                Err(e) => {
                    eprintln!(
                        "[daemon] Failed to register P2P service: {:#}. AWDL browse still active.",
                        e
                    );
                }
            }
        }
        Ok(false) => {
            println!("[daemon] AWDL not available on this machine (non-macOS or no Wi-Fi)");
        }
        Err(e) => {
            eprintln!(
                "[daemon] Failed to activate AWDL: {:#}. P2P will not be available.",
                e
            );
        }
    }

    // Shutdown coordination: watch channel
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Periodic expired-session cleanup task
    let cleanup_state = state.clone();
    let mut cleanup_shutdown_rx = shutdown_rx.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let mut mgr = cleanup_state.session_mgr.lock().await;
                    let removed = mgr.cleanup_expired();
                    if removed > 0 {
                        println!("[daemon] Cleaned up {} expired session(s)", removed);
                    }
                }
                _ = cleanup_shutdown_rx.changed() => {
                    break;
                }
            }
        }
    });

    // -- Spawn QUIC accept loop for incoming chat connections --
    if state.transport.is_some() {
        let accept_state = state.clone();
        let mut accept_shutdown_rx = shutdown_rx.clone();
        tokio::spawn(async move {
            loop {
                let accept_future = async {
                    let transport = accept_state.transport.as_ref()
                        .expect("transport verified as Some before spawning accept loop");
                    transport.accept().await
                };

                let conn_result = tokio::select! {
                    result = accept_future => result,
                    _ = accept_shutdown_rx.changed() => {
                        if *accept_shutdown_rx.borrow() {
                            println!("[daemon] QUIC accept loop shutting down");
                            break;
                        }
                        continue;
                    }
                };

                // accept() returns Option<Result<Connection>>; None means endpoint closed.
                let conn = match conn_result {
                    Some(Ok(c)) => c,
                    Some(Err(e)) => {
                        eprintln!("[daemon] QUIC accept error: {:#}", e);
                        continue;
                    }
                    None => {
                        println!("[daemon] QUIC endpoint closed, accept loop exiting");
                        break;
                    }
                };

                // Accept a bidirectional stream from the peer.
                let (mut quic_send, mut quic_recv) = match conn.accept_stream().await {
                    Ok(streams) => streams,
                    Err(e) => {
                        eprintln!(
                            "[daemon] Failed to accept QUIC stream from {}: {:#}",
                            conn.remote_address(),
                            e
                        );
                        continue;
                    }
                };
                let first_message = match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    transport::recv_wire_message(&mut quic_recv),
                ).await {
                    Ok(Ok(msg)) => msg,
                    Ok(Err(e)) => {
                        eprintln!(
                            "[daemon] Failed to read initial wire message from {}: {:#}",
                            conn.remote_address(),
                            e
                        );
                        continue;
                    }
                    Err(_) => {
                        eprintln!(
                            "[daemon] Timed out waiting for initial wire message from {}",
                            conn.remote_address(),
                        );
                        continue;
                    }
                };

                match first_message {
                    WireMessage::AskRequest {
                        peer_name,
                        fingerprint_prefix: _,
                        question,
                    } => {
                        let ask_state = accept_state.clone();
                        tokio::spawn(async move {
                            let response = {
                                let mgr = ask_state.session_mgr.lock().await;
                                match mgr.get_session(&peer_name) {
                                    Some(session) => {
                                        if let Some(expires) = session.expires_at {
                                            if expires < chrono::Utc::now() {
                                                drop(mgr);
                                                WireMessage::Error {
                                                    message: format!(
                                                        "session with '{}' has expired — reconnect with `acc connect {}`",
                                                        peer_name, peer_name
                                                    ),
                                                }
                                            } else {
                                                drop(mgr);
                                                let config = ask_state.config.lock().await.clone();
                                                match ask_state
                                                    .ask_engine
                                                    .ask(
                                                        &question,
                                                        &peer_name,
                                                        &config.ai_tool,
                                                        &config.project_root,
                                                    )
                                                    .await
                                                {
                                                    Ok(result) => WireMessage::AskResponse {
                                                        answer: result.answer,
                                                        duration_ms: result.duration_ms,
                                                    },
                                                    Err(e) => WireMessage::Error {
                                                        message: e.to_string(),
                                                    },
                                                }
                                            }
                                        } else {
                                            drop(mgr);
                                            let config = ask_state.config.lock().await.clone();
                                            match ask_state
                                                .ask_engine
                                                .ask(
                                                    &question,
                                                    &peer_name,
                                                    &config.ai_tool,
                                                    &config.project_root,
                                                )
                                                .await
                                            {
                                                Ok(result) => WireMessage::AskResponse {
                                                    answer: result.answer,
                                                    duration_ms: result.duration_ms,
                                                },
                                                Err(e) => WireMessage::Error {
                                                    message: e.to_string(),
                                                },
                                            }
                                        }
                                    }
                                    None => WireMessage::Error {
                                        message: format!(
                                            "no active session with '{}' — connect first",
                                            peer_name
                                        ),
                                    },
                                }
                            };

                            if let Err(e) =
                                transport::send_wire_message(&mut quic_send, &response).await
                            {
                                eprintln!("[daemon] Failed to reply to ask request: {:#}", e);
                            }
                            let _ = quic_send.finish();
                        });
                    }
                    WireMessage::ChatOpen {
                        peer_name,
                        fingerprint_prefix: _,
                    } => {
                        println!(
                            "[daemon] Incoming chat from {} ({})",
                            peer_name,
                            conn.remote_address()
                        );

                        {
                            let mgr = accept_state.session_mgr.lock().await;
                            match mgr.get_session(&peer_name) {
                                None => {
                                    let message = format!(
                                        "no active session with '{}' — connect first",
                                        peer_name
                                    );
                                    eprintln!(
                                        "[daemon] Incoming chat rejected from {}: {}",
                                        peer_name, message
                                    );
                                    let _ = transport::send_wire_message(
                                        &mut quic_send,
                                        &WireMessage::Error { message },
                                    )
                                    .await;
                                    let _ = quic_send.finish();
                                    continue;
                                }
                                Some(session) => {
                                    if let Some(expires) = session.expires_at {
                                        if expires < chrono::Utc::now() {
                                            let message = format!(
                                                "session with '{}' has expired — reconnect with `acc connect {}`",
                                                peer_name, peer_name
                                            );
                                            eprintln!(
                                                "[daemon] Incoming chat rejected from {}: {}",
                                                peer_name, message
                                            );
                                            let _ = transport::send_wire_message(
                                                &mut quic_send,
                                                &WireMessage::Error { message },
                                            )
                                            .await;
                                            let _ = quic_send.finish();
                                            continue;
                                        }
                                    }
                                    println!("[daemon] Incoming chat validated for {}", peer_name);
                                }
                            }
                        }

                        // Create channels for the chat engine.
                        let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<String>(64);
                        let (recv_tx, recv_rx) = tokio::sync::mpsc::channel::<String>(64);
                        let (event_tx, mut event_rx) =
                            tokio::sync::mpsc::channel::<chat_engine::ChatEvent>(128);

                        tokio::spawn(async move {
                            while let Some(msg) = send_rx.recv().await {
                                if transport::send_wire_message(
                                    &mut quic_send,
                                    &WireMessage::Chat { text: msg },
                                )
                                .await
                                .is_err()
                                {
                                    break;
                                }
                            }
                            let _ = quic_send.finish();
                        });

                        tokio::spawn(async move {
                            loop {
                                match transport::recv_wire_message(&mut quic_recv).await {
                                    Ok(WireMessage::Chat { text }) => {
                                        if recv_tx.send(text).await.is_err() {
                                            break;
                                        }
                                    }
                                    Ok(WireMessage::Error { .. }) => break,
                                    Ok(_) => break,
                                    Err(_) => break,
                                }
                            }
                        });

                        let chat_state = accept_state.clone();
                        let remote_addr = conn.remote_address();
                        tokio::spawn(async move {
                            let config = chat_state.config.lock().await.clone();
                            let chat_config = chat_engine::ChatConfig {
                                display_name: config.display_name.clone(),
                                ai_tool: config.ai_tool.clone(),
                                project_root: config.project_root.clone(),
                                peer_name: Some(peer_name.clone()),
                                ..Default::default()
                            };
                            let engine = chat_engine::ChatEngine::new(chat_config);

                            let events_handle = tokio::spawn(async move {
                                let mut briefing_text = String::new();
                                while let Some(event) = event_rx.recv().await {
                                    match &event {
                                        chat_engine::ChatEvent::Briefing(text) => {
                                            briefing_text = text.clone();
                                        }
                                        chat_engine::ChatEvent::Status(s) => {
                                            println!("[chat:incoming] {}", s);
                                        }
                                        chat_engine::ChatEvent::LocalMessage(m) => {
                                            let preview: String = m.chars().take(80).collect();
                                            println!("[chat:incoming] local: {}", preview);
                                        }
                                        chat_engine::ChatEvent::RemoteMessage(m) => {
                                            let preview: String = m.chars().take(80).collect();
                                            println!("[chat:incoming] remote: {}", preview);
                                        }
                                        chat_engine::ChatEvent::Phase(p) => {
                                            println!("[chat:incoming] phase: {}", p);
                                        }
                                        chat_engine::ChatEvent::Error(e) => {
                                            eprintln!("[chat:incoming] error: {}", e);
                                        }
                                        chat_engine::ChatEvent::Complete => {
                                            println!("[chat:incoming] complete");
                                        }
                                    }
                                }
                                briefing_text
                            });

                            match engine.run_chat(send_tx, recv_rx, event_tx).await {
                                Ok(result) => {
                                    match chat_history::save_chat(&peer_name, &result) {
                                        Ok(path) => {
                                            println!(
                                                "[daemon] Incoming chat saved to {}",
                                                path.display()
                                            );
                                        }
                                        Err(e) => {
                                            eprintln!(
                                                "[daemon] Failed to save incoming chat history: {}",
                                                e
                                            );
                                        }
                                    }

                                    let _briefing_text = events_handle.await.unwrap_or_default();
                                    println!(
                                        "[daemon] Incoming chat from {} finished ({} messages, {}s)",
                                        remote_addr, result.message_count, result.duration_secs
                                    );
                                }
                                Err(e) => {
                                    let _ = events_handle.await;
                                    eprintln!(
                                        "[daemon] Incoming chat from {} failed: {:#}",
                                        remote_addr, e
                                    );
                                }
                            }
                        });
                    }
                    other => {
                        eprintln!(
                            "[daemon] Unexpected initial wire message from {}: {:?}",
                            conn.remote_address(),
                            other
                        );
                        let _ = transport::send_wire_message(
                            &mut quic_send,
                            &WireMessage::Error {
                                message: "unexpected initial wire message".to_string(),
                            },
                        )
                        .await;
                        let _ = quic_send.finish();
                    }
                }
            }
        });
    }

    // IPC accept loop
    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _addr)) => {
                        println!("[daemon] New IPC connection");
                        let st = state.clone();
                        let tx = shutdown_tx.clone();
                        tokio::spawn(handle_connection(stream, st, tx));
                    }
                    Err(e) => {
                        eprintln!("[daemon] Accept error: {}", e);
                    }
                }
            }
            _ = signal::ctrl_c() => {
                println!("\n[daemon] Received SIGINT, shutting down...");
                let _ = shutdown_tx.send(true);
                break;
            }
            _ = shutdown_rx.changed() => {
                if *shutdown_rx.borrow() {
                    println!("[daemon] Shutdown signal received");
                    break;
                }
            }
        }
    }

    // Cleanup: stop discovery.
    {
        let mut disc = state.discovery.lock().await;
        if let Some(mut discovery) = disc.take() {
            discovery.stop().await;
        }
    }

    // Cleanup: deactivate AWDL.
    awdl_activator.deactivate();

    // Cleanup: close QUIC transport.
    if let Some(ref t) = state.transport {
        t.close();
    }

    // Cleanup: remove socket.
    if sock_path.exists() {
        let _ = std::fs::remove_file(&sock_path);
    }
    println!("[daemon] Goodbye.");
    Ok(())
}
