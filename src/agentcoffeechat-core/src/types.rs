use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Message — the primary chat message envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessageType {
    Chat,
    System,
    Action,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum MessagePhase {
    Opening,
    Exchange,
    Closing,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MessageSender {
    pub name: String,
    pub fingerprint: String,
    pub ai_tool: String,
}

impl MessageSender {
    pub fn new(
        name: impl Into<String>,
        fingerprint: impl Into<String>,
        ai_tool: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            fingerprint: fingerprint.into(),
            ai_tool: ai_tool.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    #[serde(rename = "type")]
    pub msg_type: MessageType,
    pub phase: MessagePhase,
    pub from: MessageSender,
    pub body: String,
    pub turn: u32,
    pub timestamp: DateTime<Utc>,
}

impl Message {
    pub fn new(
        msg_type: MessageType,
        phase: MessagePhase,
        from: MessageSender,
        body: impl Into<String>,
        turn: u32,
    ) -> Self {
        Self {
            msg_type,
            phase,
            from,
            body: body.into(),
            turn,
            timestamp: Utc::now(),
        }
    }
}

// ---------------------------------------------------------------------------
// ControlMessage — out-of-band control signals
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    WrapupSignal,
    ChatComplete,
    EarlyEnd,
    Ping,
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlMessage {
    #[serde(rename = "type")]
    pub msg_type: String,
    pub action: ControlAction,
    pub reason: Option<String>,
}

impl ControlMessage {
    pub fn new(action: ControlAction, reason: Option<String>) -> Self {
        Self {
            msg_type: "control".to_string(),
            action,
            reason,
        }
    }
}

// ---------------------------------------------------------------------------
// Peer — a discovered nearby peer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PeerStatus {
    Available,
    Busy,
    Away,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub fingerprint: String,
    pub status: PeerStatus,
    pub connected: bool,
    pub same_project: bool,
    pub distance: Option<f64>,
}

impl Peer {
    pub fn new(name: impl Into<String>, fingerprint: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            fingerprint: fingerprint.into(),
            status: PeerStatus::Available,
            connected: false,
            same_project: false,
            distance: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Session — an active chat session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub peer_name: String,
    pub started_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub local_code: String,
    pub peer_code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint_prefix: Option<String>,
}

impl Session {
    pub fn new(
        peer_name: impl Into<String>,
        local_code: impl Into<String>,
        peer_code: impl Into<String>,
    ) -> Self {
        Self {
            peer_name: peer_name.into(),
            started_at: Utc::now(),
            expires_at: None,
            local_code: local_code.into(),
            peer_code: peer_code.into(),
            fingerprint_prefix: None,
        }
    }

    pub fn with_expiry(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    pub fn with_fingerprint(mut self, fingerprint_prefix: Option<String>) -> Self {
        self.fingerprint_prefix = fingerprint_prefix;
        self
    }
}

// ---------------------------------------------------------------------------
// ChatBriefing — summary produced after a chat session
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatBriefing {
    pub what_building: String,
    pub learnings: Vec<String>,
    pub tips: Vec<String>,
    pub ideas_to_explore: Vec<String>,
}

// ---------------------------------------------------------------------------
// Config — local configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub display_name: String,
    pub ai_tool: String,
    pub project_root: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            display_name: "agent".to_string(),
            ai_tool: "claude".to_string(),
            project_root: ".".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_serialization_roundtrip() {
        let sender = MessageSender::new("alice", "SHA256:abc123", "claude-code");
        let msg = Message::new(
            MessageType::Chat,
            MessagePhase::Exchange,
            sender,
            "Hello!",
            1,
        );
        let json = serde_json::to_string(&msg).unwrap();
        let deser: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.from.name, "alice");
        assert_eq!(deser.from.fingerprint, "SHA256:abc123");
        assert_eq!(deser.from.ai_tool, "claude-code");
        assert_eq!(deser.body, "Hello!");
        assert_eq!(deser.turn, 1);
        assert_eq!(deser.msg_type, MessageType::Chat);
    }

    #[test]
    fn control_message_creation() {
        let ctrl = ControlMessage::new(ControlAction::EarlyEnd, Some("timeout".to_string()));
        assert_eq!(ctrl.action, ControlAction::EarlyEnd);
        assert_eq!(ctrl.reason.as_deref(), Some("timeout"));
        assert_eq!(ctrl.msg_type, "control");
    }

    #[test]
    fn peer_defaults() {
        let peer = Peer::new("bob", "abc123");
        assert_eq!(peer.status, PeerStatus::Available);
        assert!(!peer.connected);
        assert!(!peer.same_project);
        assert!(peer.distance.is_none());
    }

    #[test]
    fn session_with_expiry() {
        let session = Session::new("carol", "alpha-bravo-charlie", "delta-echo-foxtrot")
            .with_expiry(Utc::now() + chrono::Duration::hours(1));
        assert!(session.expires_at.is_some());
    }

    #[test]
    fn chat_briefing_default() {
        let briefing = ChatBriefing::default();
        assert!(briefing.what_building.is_empty());
        assert!(briefing.learnings.is_empty());
    }

    #[test]
    fn config_default() {
        let config = Config::default();
        assert_eq!(config.ai_tool, "claude");
        assert_eq!(config.display_name, "agent");
    }
}
