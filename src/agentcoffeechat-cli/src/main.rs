mod chat_ui;

use std::io::{self, Write};
use std::process::Command;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};

use agentcoffeechat_core::{
    detect_ai_tool, detect_all_ai_tools, install_plugin, is_plugin_installed, socket_path,
    run_doctor_checks, CheckStatus,
    DaemonCommand, DaemonResponse, IpcClient,
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---------------------------------------------------------------------------
// Top-level CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "agentcoffeechat",
    about = "AgentCoffeeChat \u{2014} Coffee chats for AI coding agents",
    version = VERSION,
    propagate_version = true,
)]
struct Cli {
    /// Disable colored output
    #[arg(long, global = true)]
    no_color: bool,

    /// Enable verbose output
    #[arg(long, short, global = true)]
    verbose: bool,

    /// Output in JSON format
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Commands,
}

// ---------------------------------------------------------------------------
// Subcommands
// ---------------------------------------------------------------------------

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start daemon (idempotent: init + plugin install + daemon start)
    Start,

    /// Stop the daemon
    Stop,

    /// Show daemon status and nearby peers
    Status,

    /// Connect to a peer
    Connect {
        /// Peer name to connect to
        name: String,

        /// Peer's 3-word code (required in --json mode, optional in interactive mode)
        #[arg(long)]
        peer_code: Option<String>,
    },

    /// Disconnect from a peer (all peers if name omitted)
    Disconnect {
        /// Peer name to disconnect from (omit to disconnect all)
        name: Option<String>,
    },

    /// Start a coffee chat
    Chat {
        /// Peer to chat with
        #[arg(long)]
        to: Option<String>,

        /// Dry run — plan the chat without executing
        #[arg(long)]
        dry_run: bool,
    },

    /// Ask a peer's agent a question
    Ask {
        /// Peer name
        name: String,

        /// Question to ask
        question: String,
    },

    /// View past chats
    History {
        /// Number of past chats to show
        number: Option<u32>,

        /// Show briefing summaries
        #[arg(long)]
        briefing: bool,
    },

    /// List nearby peers
    Peers,

    /// List active sessions
    Sessions,

    /// Run diagnostics
    Doctor,

    /// Print setup instructions for a peer
    Invite,

    /// Show daemon logs
    Logs,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Try to connect to the daemon. Returns None if the daemon is not reachable.
fn try_connect() -> Option<IpcClient> {
    IpcClient::new().ok()
}

/// Connect to the daemon or exit with an error message.
fn connect_or_exit(json: bool) -> IpcClient {
    match IpcClient::new() {
        Ok(client) => client,
        Err(_) => {
            if json {
                let resp = serde_json::json!({
                    "ok": false,
                    "message": "daemon is not running — start it with `agentcoffeechat start`",
                });
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: daemon is not running. Start it with: agentcoffeechat start");
            }
            std::process::exit(1);
        }
    }
}

/// Send a command to the daemon and return the response, or exit on failure.
fn send_or_exit(client: &mut IpcClient, cmd: &DaemonCommand, json: bool) -> DaemonResponse {
    match client.send(cmd) {
        Ok(resp) => resp,
        Err(e) => {
            if json {
                let resp = serde_json::json!({
                    "ok": false,
                    "message": format!("failed to communicate with daemon: {}", e),
                });
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: failed to communicate with daemon: {}", e);
            }
            std::process::exit(1);
        }
    }
}

/// Print a DaemonResponse in the appropriate format.
fn print_response(resp: &DaemonResponse, json_mode: bool) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(resp).unwrap_or_else(|_| "{}".to_string()));
    } else if let Some(msg) = &resp.message {
        if resp.ok {
            println!("{}", msg);
        } else {
            eprintln!("Error: {}", msg);
        }
        if let Some(data) = &resp.data {
            println!("{}", serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string()));
        }
    }
}

fn detected_ai_tool_slug() -> Option<String> {
    match detect_ai_tool() {
        agentcoffeechat_core::AiTool::ClaudeCode => Some("claude".to_string()),
        agentcoffeechat_core::AiTool::Codex => Some("codex".to_string()),
        agentcoffeechat_core::AiTool::GeminiCli => Some("gemini".to_string()),
        agentcoffeechat_core::AiTool::Unknown => None,
    }
}

fn update_daemon_context(client: &mut IpcClient) {
    let project_root = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .to_string_lossy()
        .to_string();
    let _ = client.send(&DaemonCommand::UpdateContext {
        project_root,
        ai_tool: detected_ai_tool_slug(),
    });
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

fn handle_start(json: bool, verbose: bool) {
    // --- Plugin installation (idempotent) — install for ALL detected tools ---
    let detected_tools = detect_all_ai_tools();
    if verbose && !json {
        if detected_tools.is_empty() {
            eprintln!("[verbose] no AI tools detected");
        } else {
            let names: Vec<String> = detected_tools.iter().map(|t| t.to_string()).collect();
            eprintln!("[verbose] detected AI tools: {}", names.join(", "));
        }
    }
    for tool in &detected_tools {
        if !is_plugin_installed(tool) {
            match install_plugin(tool) {
                Ok(()) => {
                    if verbose && !json {
                        eprintln!("[verbose] installed plugin for {}", tool);
                    }
                }
                Err(e) => {
                    if verbose && !json {
                        eprintln!("[verbose] failed to install plugin for {}: {}", tool, e);
                    }
                    // Non-fatal — continue with other tools and daemon start.
                }
            }
        } else if verbose && !json {
            eprintln!("[verbose] plugin already installed for {}", tool);
        }
    }
    // Fall back to single-tool detection for backward compat logging
    if detected_tools.is_empty() {
        let detected_tool = detect_ai_tool();
        if verbose && !json {
            eprintln!("[verbose] fallback detected AI tool: {}", detected_tool);
        }
    }

    // Check if daemon is already running by trying to Ping it.
    if let Some(mut client) = try_connect() {
        if let Ok(resp) = client.send(&DaemonCommand::Ping) {
            if resp.ok {
                update_daemon_context(&mut client);
                if json {
                    let r = serde_json::json!({
                        "ok": true,
                        "message": "daemon is already running and context was refreshed",
                    });
                    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                } else {
                    println!("Daemon is already running. Context refreshed.");
                }
                return;
            }
        }
    }

    // Daemon is not running — spawn it as a background process.
    if !json {
        println!("Starting daemon...");
    }

    // Try to find the daemon binary. Look next to the CLI binary first, then
    // fall back to PATH.
    let daemon_bin = std::env::current_exe()
        .ok()
        .and_then(|p| {
            let dir = p.parent()?;
            let candidate = dir.join("agentcoffeechat-daemon");
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        })
        .unwrap_or_else(|| "agentcoffeechat-daemon".into());

    if verbose && !json {
        eprintln!("[verbose] spawning daemon: {}", daemon_bin.display());
    }

    match Command::new(&daemon_bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_child) => {
            // Wait for the socket to appear (up to 5 seconds).
            let sock = socket_path();
            let mut started = false;
            for _ in 0..50 {
                thread::sleep(Duration::from_millis(100));
                if sock.exists() {
                    // Socket exists, try to ping.
                    if let Some(mut client) = try_connect() {
                        if let Ok(resp) = client.send(&DaemonCommand::Ping) {
                            if resp.ok {
                                update_daemon_context(&mut client);
                                started = true;
                                break;
                            }
                        }
                    }
                }
            }

            if started {
                if json {
                    let r = serde_json::json!({
                        "ok": true,
                        "message": "daemon started",
                    });
                    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                } else {
                    println!("Daemon started successfully.");
                }
            } else if json {
                let r = serde_json::json!({
                    "ok": false,
                    "message": "daemon was spawned but did not become ready in time",
                });
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Warning: daemon was spawned but did not become ready in time.");
                eprintln!("Check daemon logs or try again.");
            }
        }
        Err(e) => {
            if json {
                let r = serde_json::json!({
                    "ok": false,
                    "message": format!("failed to start daemon: {}", e),
                });
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: failed to start daemon: {}", e);
                eprintln!(
                    "Make sure '{}' is installed and in your PATH.",
                    daemon_bin.display()
                );
            }
        }
    }
}

fn handle_stop(json: bool) {
    let mut client = connect_or_exit(json);
    let resp = send_or_exit(&mut client, &DaemonCommand::Shutdown, json);
    print_response(&resp, json);
}

fn handle_status(json: bool) {
    let mut client = connect_or_exit(json);
    let resp = send_or_exit(&mut client, &DaemonCommand::GetStatus, json);
    print_response(&resp, json);
}

fn handle_peers(json: bool) {
    let mut client = connect_or_exit(json);
    let resp = send_or_exit(&mut client, &DaemonCommand::ListPeers, json);
    print_response(&resp, json);
}

fn handle_sessions(json: bool) {
    let mut client = connect_or_exit(json);
    let resp = send_or_exit(&mut client, &DaemonCommand::ListSessions, json);
    print_response(&resp, json);
}

fn handle_connect(name: &str, peer_code: Option<&str>, json: bool) {
    let mut client = connect_or_exit(json);
    let fingerprint_prefix = send_or_exit(&mut client, &DaemonCommand::ListPeers, true)
        .data
        .and_then(|data| data.as_array().cloned())
        .and_then(|arr| {
            arr.into_iter().find_map(|peer| {
                let peer_name = peer.get("name").and_then(|v| v.as_str())?;
                if peer_name == name {
                    peer.get("fingerprint_prefix")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                } else {
                    None
                }
            })
        });

    if let Some(code) = peer_code {
        if !agentcoffeechat_core::validate_code(code) {
            if json {
                let r = serde_json::json!({"ok": false, "message": "invalid peer code"});
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: '{}' is not a valid three-word code.", code);
            }
            return;
        }

        let resp = send_or_exit(
            &mut client,
            &DaemonCommand::CompletePairing {
                peer_name: name.to_string(),
                peer_code: code.to_string(),
            },
            json,
        );
        print_response(&resp, json);
        return;
    }

    let begin = send_or_exit(
        &mut client,
        &DaemonCommand::BeginPairing {
            peer_name: name.to_string(),
            fingerprint_prefix,
        },
        true,
    );
    let our_code = begin
        .data
        .as_ref()
        .and_then(|d| d.get("your_code"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if json {
        let info = serde_json::json!({
            "ok": true,
            "step": "pairing",
            "your_code": our_code,
            "message": "Share this code with your peer, then re-run with --peer-code <their-code>",
        });
        println!("{}", serde_json::to_string_pretty(&info).unwrap_or_else(|_| "{}".to_string()));
        return;
    }

    println!("Your pairing code: {}", our_code);
    print!("Enter peer's code: ");
    io::stdout().flush().ok();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        eprintln!("Error: failed to read peer code from stdin.");
        return;
    }
    let peer_code = input.trim().to_string();
    if !agentcoffeechat_core::validate_code(&peer_code) {
        eprintln!("Error: '{}' is not a valid three-word code.", peer_code);
        return;
    }

    let resp = send_or_exit(
        &mut client,
        &DaemonCommand::CompletePairing {
            peer_name: name.to_string(),
            peer_code,
        },
        json,
    );
    print_response(&resp, json);
}

fn handle_disconnect(name: Option<&str>, json: bool) {
    let mut client = connect_or_exit(json);

    let peer_name = match name {
        Some(n) => n.to_string(),
        None => {
            // If no name given, try to end all sessions by listing first.
            let list_resp = send_or_exit(&mut client, &DaemonCommand::ListSessions, json);
            if let Some(data) = &list_resp.data {
                if let Some(arr) = data.as_array() {
                    if arr.is_empty() {
                        if json {
                            let r = serde_json::json!({"ok": true, "message": "no active sessions"});
                            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                        } else {
                            println!("No active sessions to disconnect.");
                        }
                        return;
                    }
                    // End each session.
                    for session in arr {
                        if let Some(pn) = session.get("peer_name").and_then(|v| v.as_str()) {
                            let cmd = DaemonCommand::EndSession {
                                peer_name: pn.to_string(),
                            };
                            // Reconnect for each command since the connection may
                            // be consumed.
                            let resp = send_or_exit(&mut client, &cmd, json);
                            print_response(&resp, json);
                        }
                    }
                    return;
                }
            }
            // Fallback: nothing to disconnect.
            if json {
                let r = serde_json::json!({"ok": true, "message": "no active sessions"});
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
            } else {
                println!("No active sessions to disconnect.");
            }
            return;
        }
    };

    let cmd = DaemonCommand::EndSession { peer_name };
    let resp = send_or_exit(&mut client, &cmd, json);
    print_response(&resp, json);
}

fn handle_ask(name: &str, question: &str, json: bool) {
    let mut client = connect_or_exit(json);
    let cmd = DaemonCommand::AskQuestion {
        peer_name: name.to_string(),
        question: question.to_string(),
    };

    if !json {
        println!("Asking {}...", name);
    }

    let resp = send_or_exit(&mut client, &cmd, json);

    if json {
        println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
    } else if resp.ok {
        if let Some(data) = &resp.data {
            if let Some(answer) = data.get("answer").and_then(|v| v.as_str()) {
                println!("\n{}", answer);
            }
            if let Some(ms) = data.get("duration_ms").and_then(|v| v.as_u64()) {
                println!("\n(answered in {}ms)", ms);
            }
        } else if let Some(msg) = &resp.message {
            println!("{}", msg);
        }
    } else if let Some(msg) = &resp.message {
        eprintln!("Error: {}", msg);
    }
}

// ---------------------------------------------------------------------------
// Chat command
// ---------------------------------------------------------------------------

fn handle_chat(to: Option<&str>, dry_run: bool, json: bool) {
    let mut client = connect_or_exit(json);

    // Determine which peer to chat with.
    let peer_name = match to {
        Some(name) => name.to_string(),
        None => {
            // If no peer specified, list peers and pick the first one.
            let resp = send_or_exit(&mut client, &DaemonCommand::ListPeers, json);
            if let Some(data) = &resp.data {
                if let Some(arr) = data.as_array() {
                    if let Some(first) = arr.first() {
                        if let Some(name) = first.get("name").and_then(|v| v.as_str()) {
                            name.to_string()
                        } else if json {
                            let r = serde_json::json!({"ok": false, "message": "no peers available"});
                            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                            return;
                        } else {
                            eprintln!("Error: no peers discovered. Use --to <peer> or wait for discovery.");
                            return;
                        }
                    } else if json {
                        let r = serde_json::json!({"ok": false, "message": "no peers discovered"});
                        println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                        return;
                    } else {
                        eprintln!("Error: no peers discovered. Use --to <peer> or wait for discovery.");
                        return;
                    }
                } else if json {
                    let r = serde_json::json!({"ok": false, "message": "unexpected response format"});
                    println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                    return;
                } else {
                    eprintln!("Error: unexpected response from daemon.");
                    return;
                }
            } else if json {
                let r = serde_json::json!({"ok": false, "message": "no peers data in response"});
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
                return;
            } else {
                eprintln!("Error: no peers discovered. Use --to <peer> or wait for discovery.");
                return;
            }
        }
    };

    if dry_run {
        if json {
            let r = serde_json::json!({
                "ok": true,
                "message": format!("would start chat with {}", peer_name),
                "dry_run": true,
                "peer_name": peer_name,
            });
            println!("{}", serde_json::to_string_pretty(&r).unwrap_or_else(|_| "{}".to_string()));
        } else {
            println!("Dry run: would start coffee chat with '{}'", peer_name);
        }
        return;
    }

    if json {
        // JSON mode: no TUI, just send command and print raw response.
        let cmd = DaemonCommand::StartChat {
            peer_name: peer_name.clone(),
        };
        let resp = send_or_exit(&mut client, &cmd, json);
        println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
        return;
    }

    // --- Interactive TUI mode ---
    let display = chat_ui::ChatDisplay::new(&peer_name);

    display.show_status(&format!("Starting coffee chat with '{}'...", peer_name));
    display.show_status("This may take a few minutes. The agents will converse autonomously.");
    println!();

    let spinner = display.show_spinner(&format!("Chatting with {}...", peer_name));

    let cmd = DaemonCommand::StartChat {
        peer_name: peer_name.clone(),
    };
    let resp = send_or_exit(&mut client, &cmd, false);

    spinner.finish_and_clear();

    if resp.ok {
        if let Some(data) = &resp.data {
            chat_ui::replay_chat(&display, data);
        } else if let Some(msg) = &resp.message {
            display.show_status(msg);
        }
    } else if let Some(msg) = &resp.message {
        eprintln!("Error: {}", msg);
    }
}

// ---------------------------------------------------------------------------
// History command
// ---------------------------------------------------------------------------

fn handle_history(number: Option<u32>, show_briefing: bool, json: bool) {
    let mut client = connect_or_exit(json);

    if let Some(index) = number {
        // Show a specific chat by index.
        let cmd = DaemonCommand::GetHistory { index };
        let resp = send_or_exit(&mut client, &cmd, json);

        if json {
            println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
        } else if resp.ok {
            if let Some(data) = &resp.data {
                let peer = data.get("peer_name").and_then(|v| v.as_str()).unwrap_or("unknown");
                let ts = data.get("timestamp").and_then(|v| v.as_str()).unwrap_or("unknown");
                println!("Chat with {} ({})\n", peer, ts);

                if show_briefing {
                    if let Some(briefing) = data.get("briefing").and_then(|v| v.as_str()) {
                        println!("{}", briefing);
                    }
                } else if let Some(transcript) = data.get("transcript").and_then(|v| v.as_str()) {
                    println!("{}", transcript);
                }
            } else if let Some(msg) = &resp.message {
                println!("{}", msg);
            }
        } else if let Some(msg) = &resp.message {
            eprintln!("Error: {}", msg);
        }
    } else {
        // List all past chats.
        let cmd = DaemonCommand::ListHistory;
        let resp = send_or_exit(&mut client, &cmd, json);

        if json {
            println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
        } else if resp.ok {
            if let Some(msg) = &resp.message {
                println!("{}\n", msg);
            }
            if let Some(data) = &resp.data {
                if let Some(arr) = data.as_array() {
                    if arr.is_empty() {
                        println!("No past chats found.");
                    } else {
                        for entry in arr {
                            let idx = entry.get("index").and_then(|v| v.as_u64()).unwrap_or(0);
                            let peer = entry.get("peer_name").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let ts = entry.get("timestamp").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let summary = entry.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                            println!("  [{}] {} ({}) - {}", idx, peer, ts, summary);
                        }
                        println!("\nUse 'agentcoffeechat history <index>' to view details.");
                    }
                }
            }
        } else if let Some(msg) = &resp.message {
            eprintln!("Error: {}", msg);
        }
    }
}

// ---------------------------------------------------------------------------
// Doctor command
// ---------------------------------------------------------------------------

fn handle_doctor(json_mode: bool) {
    // Run local (client-side) checks.
    let local_checks = run_doctor_checks();

    // Also query the daemon's RunDoctor endpoint for daemon-side checks.
    let daemon_checks: Vec<serde_json::Value> = if let Some(mut client) = try_connect() {
        match client.send(&DaemonCommand::RunDoctor) {
            Ok(resp) if resp.ok => {
                resp.data
                    .and_then(|d| d.as_array().cloned())
                    .unwrap_or_default()
            }
            _ => vec![],
        }
    } else {
        vec![]
    };

    if json_mode {
        let mut json_checks: Vec<serde_json::Value> = local_checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "name": c.name,
                    "source": "client",
                    "status": match c.status {
                        CheckStatus::Pass => "pass",
                        CheckStatus::Warning => "warning",
                        CheckStatus::Fail => "fail",
                    },
                    "message": c.message,
                })
            })
            .collect();
        for dc in &daemon_checks {
            let mut entry = dc.clone();
            if let Some(obj) = entry.as_object_mut() {
                obj.insert("source".to_string(), serde_json::json!("daemon"));
            }
            json_checks.push(entry);
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::Value::Array(json_checks)).unwrap_or_else(|_| "[]".to_string())
        );
        return;
    }

    println!("AgentCoffeeChat Doctor");
    println!("======================\n");

    let mut pass_count = 0u32;
    let mut warn_count = 0u32;
    let mut fail_count = 0u32;

    println!("  Client-side checks:");
    for check in &local_checks {
        let icon = match check.status {
            CheckStatus::Pass => {
                pass_count += 1;
                "\x1b[32m[PASS]\x1b[0m"
            }
            CheckStatus::Warning => {
                warn_count += 1;
                "\x1b[33m[WARN]\x1b[0m"
            }
            CheckStatus::Fail => {
                fail_count += 1;
                "\x1b[31m[FAIL]\x1b[0m"
            }
        };
        println!("    {} {}: {}", icon, check.name, check.message);
    }

    if !daemon_checks.is_empty() {
        println!("\n  Daemon-side checks:");
        for dc in &daemon_checks {
            let name = dc.get("name").and_then(|v| v.as_str()).unwrap_or("unknown");
            let status = dc.get("status").and_then(|v| v.as_str()).unwrap_or("unknown");
            let message = dc.get("message").and_then(|v| v.as_str()).unwrap_or("");

            let icon = match status {
                "ok" => {
                    pass_count += 1;
                    "\x1b[32m[PASS]\x1b[0m"
                }
                "warn" => {
                    warn_count += 1;
                    "\x1b[33m[WARN]\x1b[0m"
                }
                "error" => {
                    fail_count += 1;
                    "\x1b[31m[FAIL]\x1b[0m"
                }
                _ => {
                    pass_count += 1;
                    "\x1b[32m[PASS]\x1b[0m"
                }
            };
            println!("    {} {}: {}", icon, name, message);
        }
    }

    println!();
    println!(
        "Summary: {} passed, {} warnings, {} failed",
        pass_count, warn_count, fail_count
    );

    if fail_count > 0 {
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------------------
// Invite command
// ---------------------------------------------------------------------------

fn handle_invite(json: bool) {
    let instructions_text = "\
1. Install:
   brew install agentcoffeechat

2. Set up:
   acc start

3. Or paste this into Claude Code / Codex:
   Install AgentCoffeeChat by following
   github.com/agentcoffeechat/agentcoffeechat/
   blob/main/INSTALL.md";

    if json {
        let obj = serde_json::json!({
            "instructions": {
                "install": "brew install agentcoffeechat",
                "setup": "acc start",
                "docs_url": "github.com/agentcoffeechat/agentcoffeechat/blob/main/INSTALL.md",
            },
            "text": instructions_text,
        });
        println!("{}", serde_json::to_string_pretty(&obj).unwrap_or_else(|_| "{}".to_string()));
        return;
    }

    // Print the box
    let box_content = r#"
┌─ Share This With Your Peer ─────────────────────┐
│                                                  │
│  1. Install:                                     │
│     brew install agentcoffeechat                 │
│                                                  │
│  2. Set up:                                      │
│     acc start                                    │
│                                                  │
│  3. Or paste this into Claude Code / Codex:      │
│     Install AgentCoffeeChat by following         │
│     github.com/agentcoffeechat/agentcoffeechat/  │
│     blob/main/INSTALL.md                         │
│                                                  │
└──────────────────────────────────────────────────┘"#;

    println!("{}", box_content.trim_start_matches('\n'));

    // Try to copy to clipboard via pbcopy on macOS
    let clipboard_text = instructions_text;
    match Command::new("pbcopy")
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                let _ = stdin.write_all(clipboard_text.as_bytes());
            }
            let _ = child.wait();
            println!("\n(Copied to clipboard)");
        }
        Err(_) => {
            // pbcopy not available, silently skip
        }
    }
}

// ---------------------------------------------------------------------------
// Logs command
// ---------------------------------------------------------------------------

fn handle_logs(json: bool) {
    let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let log_path = home
        .join(".agentcoffeechat")
        .join("logs")
        .join("agentcoffeechatd.log");

    if !log_path.exists() {
        if json {
            let resp = serde_json::json!({
                "ok": false,
                "message": "No logs found.",
                "lines": [],
            });
            println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
        } else {
            println!("No logs found.");
        }
        return;
    }

    // Read the file and take the last 50 lines
    match std::fs::read_to_string(&log_path) {
        Ok(contents) => {
            let all_lines: Vec<&str> = contents.lines().collect();
            let start = if all_lines.len() > 50 {
                all_lines.len() - 50
            } else {
                0
            };
            let last_lines: Vec<&str> = all_lines[start..].to_vec();

            if json {
                let json_lines: Vec<serde_json::Value> = last_lines
                    .iter()
                    .map(|l| serde_json::Value::String(l.to_string()))
                    .collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::Value::Array(json_lines)).unwrap_or_else(|_| "[]".to_string())
                );
            } else {
                for line in &last_lines {
                    println!("{}", line);
                }
            }
        }
        Err(e) => {
            if json {
                let resp = serde_json::json!({
                    "ok": false,
                    "message": format!("Failed to read log file: {}", e),
                    "lines": [],
                });
                println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_else(|_| "{}".to_string()));
            } else {
                eprintln!("Error: failed to read log file: {}", e);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let cli = Cli::parse();
    let json = cli.json;

    if cli.verbose && !json {
        eprintln!("[verbose] agentcoffeechat v{VERSION}");
    }

    match &cli.command {
        Commands::Start => handle_start(json, cli.verbose),
        Commands::Stop => handle_stop(json),
        Commands::Status => handle_status(json),
        Commands::Connect { name, peer_code } => handle_connect(name, peer_code.as_deref(), json),
        Commands::Disconnect { name } => handle_disconnect(name.as_deref(), json),
        Commands::Chat { to, dry_run } => {
            handle_chat(to.as_deref(), *dry_run, json);
        }
        Commands::Ask { name, question } => {
            handle_ask(name, question, json);
        }
        Commands::History { number, briefing } => {
            handle_history(*number, *briefing, json);
        }
        Commands::Peers => handle_peers(json),
        Commands::Sessions => handle_sessions(json),
        Commands::Doctor => handle_doctor(json),
        Commands::Invite => handle_invite(json),
        Commands::Logs => handle_logs(json),
    }
}
