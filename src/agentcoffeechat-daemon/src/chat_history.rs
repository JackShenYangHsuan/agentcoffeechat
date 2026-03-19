// Chat history — persists transcripts and briefings to disk, and provides
// retrieval helpers for past chats and briefings.
//
// Storage layout:
//   ~/.agentcoffeechat/chats/<peer>-<timestamp>/
//       transcript.md         — Full chat transcript
//       briefing.md           — Legacy briefing (backward compat)
//       briefing-human.md     — Human-facing pre-meeting note
//       briefing-agent.json   — Structured agent memo (machine-actionable)
//       metadata.json         — Identity, timestamps, completion status

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use crate::chat_engine::ChatResult;
use agentcoffeechat_core::types::{ChatBriefing, ChatMetadata, HumanBriefing, Message};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Base directory name under the user's home directory.
const BASE_DIR: &str = ".agentcoffeechat";

/// Subdirectory for chat transcripts.
const CHATS_DIR: &str = "chats";

// ---------------------------------------------------------------------------
// ChatHistoryEntry
// ---------------------------------------------------------------------------

/// A summary entry for a past chat, used when listing history.
#[derive(Debug, Clone)]
pub struct ChatHistoryEntry {
    /// The peer's display name.
    pub peer_name: String,
    /// When the chat took place (derived from the directory timestamp).
    pub timestamp: DateTime<Utc>,
    /// First line of the briefing (a short summary).
    pub summary: String,
    /// Path to the chat directory.
    pub path: PathBuf,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Save a chat result (transcript + briefing + metadata) to disk.
///
/// `metadata` is optional — if provided, it's saved as `metadata.json` alongside
/// the transcript and briefings. This enables future session pickup and correlation.
///
/// Returns the path to the created directory.
pub fn save_chat(peer_name: &str, result: &ChatResult, metadata: Option<&ChatMetadata>) -> Result<PathBuf> {
    let now = Utc::now();
    let timestamp_str = now.format("%Y%m%d-%H%M%S").to_string();
    let dir_name = format!("{}-{}", sanitize_filename(peer_name), timestamp_str);

    let chat_dir = chats_base_dir()?.join(dir_name);
    std::fs::create_dir_all(&chat_dir).with_context(|| {
        format!(
            "failed to create chat directory: {}",
            chat_dir.display()
        )
    })?;

    // Write transcript.md
    let transcript_path = chat_dir.join("transcript.md");
    let transcript_md = format_transcript(&result.transcript, peer_name, result.duration_secs);
    std::fs::write(&transcript_path, &transcript_md).with_context(|| {
        format!(
            "failed to write transcript: {}",
            transcript_path.display()
        )
    })?;

    // Write briefing.md (legacy format, for backward compat)
    let briefing_path = chat_dir.join("briefing.md");
    let briefing_md = format_briefing_md(&result.briefing, peer_name);
    std::fs::write(&briefing_path, &briefing_md).with_context(|| {
        format!(
            "failed to write briefing: {}",
            briefing_path.display()
        )
    })?;

    // Write briefing-human.md (new human-facing pre-meeting note)
    let human_path = chat_dir.join("briefing-human.md");
    let human_md = format_human_briefing_md(&result.output.human_briefing, peer_name);
    std::fs::write(&human_path, &human_md).with_context(|| {
        format!(
            "failed to write human briefing: {}",
            human_path.display()
        )
    })?;

    // Write briefing-agent.json (structured agent memo)
    let agent_path = chat_dir.join("briefing-agent.json");
    let agent_json = serde_json::to_string_pretty(&result.output.agent_memo)
        .unwrap_or_else(|_| "{}".to_string());
    std::fs::write(&agent_path, &agent_json).with_context(|| {
        format!(
            "failed to write agent memo: {}",
            agent_path.display()
        )
    })?;

    // Write metadata.json (identity, timestamps, completion status)
    if let Some(meta) = metadata {
        let meta_path = chat_dir.join("metadata.json");
        let meta_json = serde_json::to_string_pretty(meta)
            .unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&meta_path, &meta_json).with_context(|| {
            format!(
                "failed to write metadata: {}",
                meta_path.display()
            )
        })?;
    }

    Ok(chat_dir)
}

/// Load the most recent briefings for a specific peer.
///
/// Returns up to `max` briefing texts (most recent first).
pub fn load_recent_briefings(peer_name: &str, max: usize) -> Result<Vec<String>> {
    let base = chats_base_dir()?;
    if !base.exists() {
        return Ok(vec![]);
    }

    let prefix = format!("{}-", sanitize_filename(peer_name));
    let mut matching_dirs: Vec<PathBuf> = Vec::new();

    for entry in std::fs::read_dir(&base).context("failed to read chats directory")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with(&prefix) && entry.file_type()?.is_dir() {
            matching_dirs.push(entry.path());
        }
    }

    // Sort by name descending (timestamp is embedded, so lexicographic order works).
    matching_dirs.sort();
    matching_dirs.reverse();

    let mut briefings = Vec::new();
    for dir in matching_dirs.into_iter().take(max) {
        // Prefer the richer human briefing format; fall back to legacy.
        let human_path = dir.join("briefing-human.md");
        let legacy_path = dir.join("briefing.md");
        let briefing_path = if human_path.exists() {
            human_path
        } else {
            legacy_path
        };
        if briefing_path.exists() {
            let content = std::fs::read_to_string(&briefing_path)
                .with_context(|| format!("failed to read {}", briefing_path.display()))?;
            briefings.push(content);
        }
    }

    Ok(briefings)
}

/// List all past chats across all peers.
///
/// Returns entries sorted by timestamp descending (most recent first).
pub fn list_chats() -> Result<Vec<ChatHistoryEntry>> {
    let base = chats_base_dir()?;
    if !base.exists() {
        return Ok(vec![]);
    }

    let mut entries: Vec<ChatHistoryEntry> = Vec::new();

    for entry in std::fs::read_dir(&base).context("failed to read chats directory")? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }

        let dir_name = entry.file_name().to_string_lossy().to_string();

        // Parse peer name and timestamp from directory name:
        // format is "<peer>-<YYYYMMDD>-<HHMMSS>"
        if let Some((peer_name, timestamp)) = parse_chat_dir_name(&dir_name) {
            let summary = read_briefing_summary(&entry.path());
            entries.push(ChatHistoryEntry {
                peer_name,
                timestamp,
                summary,
                path: entry.path(),
            });
        }
    }

    // Sort by timestamp descending.
    entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Return the base directory for all chat history.
fn chats_base_dir() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(BASE_DIR).join(CHATS_DIR))
}

/// Get the user's home directory.
fn dirs_home() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME environment variable not set")
}

/// Sanitize a peer name for use in a directory name.
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Format the transcript as Markdown.
fn format_transcript(messages: &[Message], peer_name: &str, duration_secs: u64) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Coffee Chat with {}\n\n", peer_name));
    out.push_str(&format!(
        "Duration: {} minutes\n",
        duration_secs / 60
    ));
    out.push_str(&format!("Messages: {}\n\n", messages.len()));
    out.push_str("---\n\n");

    for msg in messages {
        let speaker = if msg.from.name == crate::chat_engine::PEER_SENDER_NAME {
            peer_name
        } else {
            &msg.from.name
        };
        let phase = match msg.phase {
            agentcoffeechat_core::types::MessagePhase::Opening => " (opening)",
            agentcoffeechat_core::types::MessagePhase::Exchange => "",
            agentcoffeechat_core::types::MessagePhase::Closing => " (closing)",
        };
        out.push_str(&format!(
            "**{}**{} _(turn {})_:\n{}\n\n",
            speaker, phase, msg.turn, msg.body
        ));
    }

    out
}

/// Format a briefing as Markdown.
fn format_briefing_md(briefing: &ChatBriefing, peer_name: &str) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Briefing: Chat with {}\n\n", peer_name));

    out.push_str("## What they're building\n");
    out.push_str(&briefing.what_building);
    out.push_str("\n\n");

    if !briefing.learnings.is_empty() {
        out.push_str("## Key learnings\n");
        for item in &briefing.learnings {
            out.push_str(&format!("- {}\n", item));
        }
        out.push('\n');
    }

    if !briefing.tips.is_empty() {
        out.push_str("## Tips to try\n");
        for item in &briefing.tips {
            out.push_str(&format!("- {}\n", item));
        }
        out.push('\n');
    }

    if !briefing.ideas_to_explore.is_empty() {
        out.push_str("## Ideas to explore\n");
        for item in &briefing.ideas_to_explore {
            out.push_str(&format!("- {}\n", item));
        }
        out.push('\n');
    }

    out
}

/// Format a human briefing as Markdown.
fn format_human_briefing_md(briefing: &HumanBriefing, peer_name: &str) -> String {
    let mut out = String::new();

    out.push_str(&format!("# Pre-Meeting Briefing: {}\n\n", peer_name));

    if !briefing.project_arc.is_empty() {
        out.push_str("## Their project\n");
        out.push_str(&briefing.project_arc);
        out.push_str("\n\n");
    }

    if !briefing.current_focus.is_empty() {
        out.push_str("## What they're building now\n");
        out.push_str(&briefing.current_focus);
        out.push_str("\n\n");
    }

    if !briefing.setup_comparison.is_empty() {
        out.push_str("## Their setup vs yours\n");
        out.push_str(&briefing.setup_comparison);
        out.push_str("\n\n");
    }

    if !briefing.overlaps.is_empty() {
        out.push_str("## Where your work overlaps\n");
        out.push_str(&briefing.overlaps);
        out.push_str("\n\n");
    }

    if !briefing.candid_takes.is_empty() {
        out.push_str("## Candid takes\n");
        out.push_str(&briefing.candid_takes);
        out.push_str("\n\n");
    }

    if !briefing.conversation_starters.is_empty() {
        out.push_str("## Conversation starters\n");
        for starter in &briefing.conversation_starters {
            out.push_str(&format!("- {}\n", starter));
        }
        out.push('\n');
    }

    out
}

/// Parse a chat directory name into (peer_name, timestamp).
///
/// Expected format: `<peer>-<YYYYMMDD>-<HHMMSS>`
fn parse_chat_dir_name(name: &str) -> Option<(String, DateTime<Utc>)> {
    // The timestamp portion is always the last 15 characters: YYYYMMDD-HHMMSS
    if name.len() < 16 {
        return None;
    }

    let timestamp_part = &name[name.len() - 15..];
    // Validate it looks like a timestamp: YYYYMMDD-HHMMSS
    if timestamp_part.len() != 15 || timestamp_part.as_bytes()[8] != b'-' {
        return None;
    }

    let date_str = &timestamp_part[..8];
    let time_str = &timestamp_part[9..];

    let datetime_str = format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date_str[..4],
        &date_str[4..6],
        &date_str[6..8],
        &time_str[..2],
        &time_str[2..4],
        &time_str[4..6],
    );

    let timestamp: DateTime<Utc> = datetime_str.parse().ok()?;

    // Peer name is everything before the timestamp, minus the separating dash.
    let peer_end = name.len() - 16;
    if peer_end == 0 {
        return None;
    }
    let peer_name = name[..peer_end].to_string();

    Some((peer_name, timestamp))
}

/// Read the first non-empty line of briefing.md as a summary.
fn read_briefing_summary(dir: &Path) -> String {
    let briefing_path = dir.join("briefing.md");
    if let Ok(content) = std::fs::read_to_string(&briefing_path) {
        for line in content.lines() {
            let trimmed = line.trim();
            // Skip markdown headers and empty lines.
            if !trimmed.is_empty() && !trimmed.starts_with('#') {
                return trimmed.to_string();
            }
        }
    }
    "No summary available".to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use agentcoffeechat_core::types::MessageSender;

    #[test]
    fn sanitize_filename_basic() {
        assert_eq!(sanitize_filename("alice"), "alice");
        assert_eq!(sanitize_filename("bob-123"), "bob-123");
        assert_eq!(sanitize_filename("carol's machine"), "carol_s_machine");
        assert_eq!(sanitize_filename("a/b\\c"), "a_b_c");
    }

    #[test]
    fn parse_chat_dir_name_valid() {
        let (peer, ts) = parse_chat_dir_name("alice-20260318-143022").unwrap();
        assert_eq!(peer, "alice");
        assert_eq!(ts.format("%Y%m%d").to_string(), "20260318");
    }

    #[test]
    fn parse_chat_dir_name_with_dashes_in_peer() {
        let (peer, _ts) = parse_chat_dir_name("bob-machine-20260318-143022").unwrap();
        assert_eq!(peer, "bob-machine");
    }

    #[test]
    fn parse_chat_dir_name_too_short() {
        assert!(parse_chat_dir_name("short").is_none());
    }

    #[test]
    fn format_transcript_basic() {
        let messages = vec![
            Message::new(
                agentcoffeechat_core::types::MessageType::Chat,
                agentcoffeechat_core::types::MessagePhase::Opening,
                MessageSender::new("alice", "fp1", "claude-code"),
                "Hello!",
                1,
            ),
            Message::new(
                agentcoffeechat_core::types::MessageType::Chat,
                agentcoffeechat_core::types::MessagePhase::Opening,
                MessageSender::new("peer", "fp2", "claude-code"),
                "Hi there!",
                2,
            ),
        ];
        let md = format_transcript(&messages, "bob", 120);
        assert!(md.contains("Coffee Chat with bob"));
        assert!(md.contains("Hello!"));
        assert!(md.contains("Hi there!"));
        assert!(md.contains("2 minutes"));
    }

    #[test]
    fn format_briefing_md_basic() {
        let briefing = ChatBriefing {
            what_building: "A CLI tool".into(),
            learnings: vec!["Insight 1".into()],
            tips: vec!["Tip 1".into()],
            ideas_to_explore: vec!["Idea 1".into()],
        };
        let md = format_briefing_md(&briefing, "carol");
        assert!(md.contains("Briefing: Chat with carol"));
        assert!(md.contains("A CLI tool"));
        assert!(md.contains("Insight 1"));
        assert!(md.contains("Tip 1"));
        assert!(md.contains("Idea 1"));
    }

    #[test]
    fn chats_base_dir_under_home() {
        // This test only checks the structure, not actual filesystem.
        if let Ok(dir) = chats_base_dir() {
            let dir_str = dir.to_string_lossy();
            assert!(dir_str.contains(".agentcoffeechat"));
            assert!(dir_str.contains("chats"));
        }
    }
}
