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

/// Legacy briefing format (kept for backward compatibility with old chats).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChatBriefing {
    pub what_building: String,
    pub learnings: Vec<String>,
    pub tips: Vec<String>,
    pub ideas_to_explore: Vec<String>,
}

// ---------------------------------------------------------------------------
// HumanBriefing — pre-meeting note for the developer
// ---------------------------------------------------------------------------

/// Human-facing briefing: a pre-meeting note that helps the developer
/// understand who the other person is and gives conversation starters.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HumanBriefing {
    /// Full project story: why it started, key pivots, where it's headed.
    pub project_arc: String,
    /// What they're actively building right now (branch, recent commits, task).
    pub current_focus: String,
    /// Their agent setup compared to yours (what they have that you don't).
    pub setup_comparison: String,
    /// Thematic and code-level overlap between your projects.
    pub overlaps: String,
    /// Candid takes: frustrations, failures, what they'd do differently.
    pub candid_takes: String,
    /// Layered conversation starters:
    /// 1. Understanding (high-level), 2. Collaboration, 3. Spicy/provocative.
    pub conversation_starters: Vec<String>,
}

// ---------------------------------------------------------------------------
// AgentMemo — structured actionable memo for the coding agent
// ---------------------------------------------------------------------------

/// Agent-facing memo: structured data the coding agent can act on in future
/// sessions to improve its own setup and workflow.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentMemo {
    /// Setup diffs: plugins/MCP/hooks they have that we don't, and vice versa.
    pub setup_diffs: SetupDiffs,
    /// Concrete workflow improvements observed from the peer.
    pub workflow_improvements: Vec<String>,
    /// Patterns from their setup that could speed up our sessions.
    pub debottleneck_ideas: Vec<String>,
    /// Blindspots: gaps the peer surfaced (e.g. no tests, missing tooling).
    pub blindspots_surfaced: Vec<String>,
    /// Tips for working agentically — human workflows + agent techniques.
    pub agentic_tips: AgenticTips,
    /// Concrete follow-up actions to take.
    pub follow_up_actions: Vec<String>,
}

/// Differences in agent setup between the two sides.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SetupDiffs {
    /// Things the peer has that we don't.
    pub they_have: Vec<String>,
    /// Things we have that the peer doesn't.
    pub we_have: Vec<String>,
    /// Specific additions suggested based on the comparison.
    pub suggested_additions: Vec<String>,
}

/// Tips for building with AI agents effectively.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgenticTips {
    /// How the human uses their AI agent (prompting, context, commands).
    pub human_workflows: Vec<String>,
    /// Techniques the agent itself has found effective.
    pub agent_techniques: Vec<String>,
}

// ---------------------------------------------------------------------------
// CoffeeChatOutput — bundles both briefings
// ---------------------------------------------------------------------------

/// The complete output of a coffee chat: one document for the human,
/// one structured memo for the agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CoffeeChatOutput {
    pub human_briefing: HumanBriefing,
    pub agent_memo: AgentMemo,
    /// Legacy briefing for backward compatibility.
    pub legacy_briefing: ChatBriefing,
}

// ---------------------------------------------------------------------------
// ChatMetadata — structured metadata saved with each chat
// ---------------------------------------------------------------------------

/// Metadata saved alongside each chat for identity, correlation, and replay.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMetadata {
    /// Peer's display name.
    pub peer_name: String,
    /// Peer's fingerprint prefix (first 16 hex chars of identity hash).
    pub peer_fingerprint: String,
    /// Our own display name.
    pub local_name: String,
    /// Our own fingerprint prefix.
    pub local_fingerprint: String,
    /// Which AI tool was used for the chat.
    pub ai_tool: String,
    /// When the chat started (ISO 8601).
    pub started_at: DateTime<Utc>,
    /// When the chat ended (ISO 8601).
    pub ended_at: DateTime<Utc>,
    /// Total number of messages exchanged.
    pub message_count: usize,
    /// Duration in seconds.
    pub duration_secs: u64,
    /// Whether the chat completed fully or was partial (peer disconnect, etc.).
    pub completed: bool,
    /// Number of conversation phases completed (out of 5).
    pub phases_completed: u32,
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
    fn human_briefing_default() {
        let hb = HumanBriefing::default();
        assert!(hb.project_arc.is_empty());
        assert!(hb.conversation_starters.is_empty());
    }

    #[test]
    fn agent_memo_default() {
        let memo = AgentMemo::default();
        assert!(memo.setup_diffs.they_have.is_empty());
        assert!(memo.follow_up_actions.is_empty());
    }

    #[test]
    fn coffee_chat_output_serialization() {
        let output = CoffeeChatOutput::default();
        let json = serde_json::to_string(&output).unwrap();
        let deser: CoffeeChatOutput = serde_json::from_str(&json).unwrap();
        assert!(deser.human_briefing.project_arc.is_empty());
        assert!(deser.agent_memo.agentic_tips.human_workflows.is_empty());
    }

    #[test]
    fn config_default() {
        let config = Config::default();
        assert_eq!(config.ai_tool, "claude");
        assert_eq!(config.display_name, "agent");
    }
}
