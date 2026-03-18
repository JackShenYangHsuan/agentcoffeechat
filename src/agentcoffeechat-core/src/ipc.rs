use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// DaemonCommand — shared between CLI and daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// Check that the daemon is running
    Ping,
    /// List currently active sessions
    ListSessions,
    /// Begin pairing with a peer and mint our local 3-word code
    BeginPairing {
        peer_name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint_prefix: Option<String>,
    },
    /// Complete pairing after the human enters the peer's 3-word code
    CompletePairing {
        peer_name: String,
        peer_code: String,
    },
    /// End an existing session
    EndSession { peer_name: String },
    /// List discovered peers
    ListPeers,
    /// Get daemon status information
    GetStatus,
    /// Request the daemon to shut down
    Shutdown,
    /// Ask a peer's agent a specific question (instant question)
    AskQuestion {
        peer_name: String,
        question: String,
    },
    /// Start a coffee chat with a peer
    StartChat {
        peer_name: String,
    },
    /// List past chat history
    ListHistory,
    /// Get details of a specific past chat by index (0 = most recent)
    GetHistory {
        index: u32,
    },
    /// Run diagnostics
    RunDoctor,
    /// Update daemon project/tool context from the current CLI invocation
    UpdateContext {
        project_root: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ai_tool: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// DaemonResponse — shared between CLI and daemon
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl DaemonResponse {
    pub fn success(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            data: None,
        }
    }

    pub fn success_with_data(message: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            ok: true,
            message: Some(message.into()),
            data: Some(data),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            message: Some(message.into()),
            data: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Socket path helper
// ---------------------------------------------------------------------------

/// Return the path to the daemon's Unix socket for the current user.
pub fn socket_path() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/tmp/agentcoffeechat-{uid}.sock"))
}

// ---------------------------------------------------------------------------
// IpcClient — synchronous client that talks to the daemon
// ---------------------------------------------------------------------------

pub struct IpcClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl IpcClient {
    /// Connect to the daemon's Unix socket at the default path.
    pub fn new() -> Result<Self> {
        let path = socket_path();
        Self::connect(&path)
    }

    /// Connect to a specific socket path.
    pub fn connect(path: &std::path::Path) -> Result<Self> {
        let stream = UnixStream::connect(path)
            .with_context(|| format!("failed to connect to daemon socket at {}", path.display()))?;
        let reader_stream = stream
            .try_clone()
            .context("failed to clone UnixStream for reader")?;
        Ok(Self {
            reader: BufReader::new(reader_stream),
            writer: stream,
        })
    }

    /// Send a command to the daemon and read the response.
    pub fn send(&mut self, cmd: &DaemonCommand) -> Result<DaemonResponse> {
        let mut json =
            serde_json::to_string(cmd).context("failed to serialize DaemonCommand")?;
        json.push('\n');

        self.writer
            .write_all(json.as_bytes())
            .context("failed to write to daemon socket")?;
        self.writer
            .flush()
            .context("failed to flush daemon socket")?;

        let mut response_line = String::new();
        self.reader
            .read_line(&mut response_line)
            .context("failed to read response from daemon")?;

        if response_line.is_empty() {
            anyhow::bail!("daemon closed the connection without responding");
        }

        let response: DaemonResponse = serde_json::from_str(response_line.trim())
            .context("failed to deserialize DaemonResponse")?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_command_serialization_roundtrip() {
        let cmds = vec![
            DaemonCommand::Ping,
            DaemonCommand::ListSessions,
            DaemonCommand::BeginPairing {
                peer_name: "alice".into(),
                fingerprint_prefix: Some("abcdef0123456789".into()),
            },
            DaemonCommand::CompletePairing {
                peer_name: "alice".into(),
                peer_code: "alpha-bravo-charlie".into(),
            },
            DaemonCommand::EndSession {
                peer_name: "bob".into(),
            },
            DaemonCommand::ListPeers,
            DaemonCommand::GetStatus,
            DaemonCommand::Shutdown,
            DaemonCommand::AskQuestion {
                peer_name: "carol".into(),
                question: "What database do you use?".into(),
            },
            DaemonCommand::StartChat {
                peer_name: "dave".into(),
            },
            DaemonCommand::ListHistory,
            DaemonCommand::GetHistory { index: 0 },
            DaemonCommand::RunDoctor,
            DaemonCommand::UpdateContext {
                project_root: "/tmp/project".into(),
                ai_tool: Some("claude".into()),
            },
        ];
        for cmd in &cmds {
            let json = serde_json::to_string(cmd).unwrap();
            let deser: DaemonCommand = serde_json::from_str(&json).unwrap();
            // Verify the tag survives roundtrip
            let json2 = serde_json::to_string(&deser).unwrap();
            assert_eq!(json, json2, "Roundtrip failed for {:?}", cmd);
        }
    }

    #[test]
    fn daemon_response_success() {
        let resp = DaemonResponse::success("pong");
        assert!(resp.ok);
        assert_eq!(resp.message.as_deref(), Some("pong"));
        assert!(resp.data.is_none());
    }

    #[test]
    fn daemon_response_success_with_data() {
        let data = serde_json::json!(["a", "b"]);
        let resp = DaemonResponse::success_with_data("ok", data.clone());
        assert!(resp.ok);
        assert_eq!(resp.data, Some(data));
    }

    #[test]
    fn daemon_response_error() {
        let resp = DaemonResponse::error("something broke");
        assert!(!resp.ok);
        assert_eq!(resp.message.as_deref(), Some("something broke"));
    }

    #[test]
    fn daemon_response_serialization_roundtrip() {
        let resp = DaemonResponse::success_with_data("ok", serde_json::json!({"count": 3}));
        let json = serde_json::to_string(&resp).unwrap();
        let deser: DaemonResponse = serde_json::from_str(&json).unwrap();
        assert!(deser.ok);
        assert_eq!(deser.message.as_deref(), Some("ok"));
        assert_eq!(deser.data, Some(serde_json::json!({"count": 3})));
    }

    #[test]
    fn socket_path_contains_uid() {
        let path = socket_path();
        let path_str = path.to_string_lossy();
        assert!(path_str.starts_with("/tmp/agentcoffeechat-"));
        assert!(path_str.ends_with(".sock"));
    }
}
