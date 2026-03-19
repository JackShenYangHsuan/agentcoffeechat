// chat_ui.rs — Styled terminal chat display for `acc chat`.
//
// Uses crossterm ANSI styling and box-drawing characters to render chat
// messages as colored bubbles.  An indicatif spinner is shown while
// waiting for the daemon to complete the chat.

use crossterm::style::{Attribute, Color, Stylize};
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

/// Width (in columns) of the chat bubble boxes.
const BOX_WIDTH: usize = 60;

/// A simple styled chat display that prints messages to stdout.
///
/// Methods like `show_local_message` and `show_remote_message` are part of
/// the public API for future streaming support even if not yet called from
/// `replay_chat`.
pub struct ChatDisplay {
    peer_name: String,
}

#[allow(dead_code)]
impl ChatDisplay {
    /// Create a new `ChatDisplay` for a chat with the given peer.
    pub fn new(peer_name: &str) -> Self {
        Self {
            peer_name: peer_name.to_string(),
        }
    }

    /// Display a phase header (e.g. "icebreaker", "followup", "wrapup", "briefing").
    pub fn show_phase(&self, phase: &str) {
        let label = match phase {
            "icebreaker" => "Icebreaker",
            "followup" => "Follow-up",
            "wrapup" => "Wrap-up",
            "briefing" => "Briefing",
            other => other,
        };

        let label_len = label.len() + 2; // " label "
        let side = if BOX_WIDTH > label_len + 2 {
            (BOX_WIDTH - label_len) / 2
        } else {
            3
        };

        let bar_left = "\u{2550}".repeat(side);
        let bar_right = "\u{2550}".repeat(side);

        println!();
        println!(
            "{}",
            format!("{} {} {}", bar_left, label, bar_right)
                .with(Color::Yellow)
                .attribute(Attribute::Bold)
        );
        println!();
    }

    /// Display a message from our local agent.
    pub fn show_local_message(&self, body: &str) {
        self.render_bubble("You", body, Color::Cyan);
    }

    /// Display a message from the remote peer.
    pub fn show_remote_message(&self, body: &str) {
        self.render_bubble(&self.peer_name, body, Color::Green);
    }

    /// Display a dim gray status message.
    pub fn show_status(&self, msg: &str) {
        println!(
            "{}",
            format!("  {} {}", "\u{2022}", msg)
                .with(Color::DarkGrey)
                .attribute(Attribute::Dim)
        );
    }

    /// Display the final briefing in a styled box.
    pub fn show_briefing(&self, briefing_text: &str) {
        // Top border
        let top = format!(
            "\u{250C}\u{2500} Briefing {}\u{2510}",
            "\u{2500}".repeat(BOX_WIDTH.saturating_sub(12))
        );
        println!("{}", top.with(Color::White).attribute(Attribute::Bold));

        // Body lines
        for line in briefing_text.lines() {
            let padded = if line.len() < BOX_WIDTH - 4 {
                format!("{}{}", line, " ".repeat(BOX_WIDTH - 4 - line.len()))
            } else {
                line[..BOX_WIDTH - 4].to_string()
            };
            println!(
                "{} {} {}",
                "\u{2502}".with(Color::White).attribute(Attribute::Bold),
                padded.with(Color::White),
                "\u{2502}".with(Color::White).attribute(Attribute::Bold),
            );
        }

        // Bottom border
        let bottom = format!(
            "\u{2514}{}\u{2518}",
            "\u{2500}".repeat(BOX_WIDTH - 2)
        );
        println!("{}", bottom.with(Color::White).attribute(Attribute::Bold));
    }

    /// Display a progress spinner while waiting for the daemon.  Returns
    /// the `ProgressBar` so the caller can finish it when the work is done.
    pub fn show_spinner(&self, msg: &str) -> ProgressBar {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} {msg}")
                .unwrap()
                .tick_strings(&[
                    "\u{2581}", "\u{2582}", "\u{2583}", "\u{2584}",
                    "\u{2585}", "\u{2586}", "\u{2587}", "\u{2588}",
                    "\u{2587}", "\u{2586}", "\u{2585}", "\u{2584}",
                    "\u{2583}", "\u{2582}",
                ]),
        );
        pb.set_message(msg.to_string());
        pb.enable_steady_tick(Duration::from_millis(80));
        pb
    }

    // ------------------------------------------------------------------
    // Internal helpers
    // ------------------------------------------------------------------

    /// Render a chat bubble with box-drawing characters.
    fn render_bubble(&self, speaker: &str, body: &str, color: Color) {
        let header_label = format!(" {} ", speaker);
        let header_fill_len = BOX_WIDTH.saturating_sub(header_label.len() + 3);
        let top = format!(
            "\u{250C}\u{2500}{}{}\u{2510}",
            header_label,
            "\u{2500}".repeat(header_fill_len),
        );
        println!("{}", top.with(color));

        // Wrap body text into lines that fit inside the box.
        let inner_width = BOX_WIDTH - 4; // "| " + content + " |"
        for raw_line in body.lines() {
            for chunk in wrap_text(raw_line, inner_width) {
                let pad = inner_width.saturating_sub(chunk.len());
                println!(
                    "{} {}{}{}",
                    "\u{2502}".with(color),
                    chunk,
                    " ".repeat(pad),
                    "\u{2502}".with(color),
                );
            }
        }

        let bottom = format!(
            "\u{2514}{}\u{2518}",
            "\u{2500}".repeat(BOX_WIDTH - 2),
        );
        println!("{}", bottom.with(color));
        println!(); // spacing between bubbles
    }
}

/// Word-wrap `text` to lines of at most `width` characters, breaking on
/// whitespace boundaries when possible.
#[allow(dead_code)]
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();

    for word in text.split_whitespace() {
        if current.is_empty() {
            current = word.to_string();
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(current);
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Replay a completed chat result through the `ChatDisplay`.
///
/// The `data` parameter is the `resp.data` JSON value returned by the daemon
/// for a `StartChat` command.  It contains `peer_name`, `message_count`,
/// `duration_secs`, `briefing_text`, and `saved_to`.
///
/// Because the current daemon returns a single response (not streaming),
/// we display all messages from the briefing text after the fact, rendered
/// as styled chat bubbles.
pub fn replay_chat(display: &ChatDisplay, data: &serde_json::Value) {
    let peer_name = data
        .get("peer_name")
        .and_then(|v| v.as_str())
        .unwrap_or("peer");
    let message_count = data
        .get("message_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let duration_secs = data
        .get("duration_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let briefing_text = data
        .get("briefing_text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let saved_to = data.get("saved_to").and_then(|v| v.as_str());

    // Show summary stats.
    display.show_status(&format!(
        "Chat with {} completed: {} messages in {} min",
        peer_name,
        message_count,
        duration_secs / 60,
    ));
    println!();

    // Display the human briefing if available, otherwise fall back to legacy.
    let has_human_briefing = data
        .get("human_briefing")
        .and_then(|v| v.get("project_arc"))
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    if has_human_briefing {
        display.show_phase("briefing");
        display_human_briefing(display, data.get("human_briefing").unwrap());
    } else if !briefing_text.is_empty() {
        display.show_phase("briefing");
        display.show_briefing(briefing_text);
    }

    // Show agent memo summary if available.
    if let Some(memo) = data.get("agent_memo") {
        let actions = memo
            .get("follow_up_actions")
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if actions > 0 {
            display.show_status(&format!(
                "Agent memo: {} follow-up action(s) saved",
                actions
            ));
        }
    }

    // Footer.
    if let Some(path) = saved_to {
        println!();
        display.show_status(&format!("Saved to: {}", path));
    }
}

/// Render the human briefing from JSON data using the ChatDisplay.
fn display_human_briefing(display: &ChatDisplay, data: &serde_json::Value) {
    let get_str = |key: &str| -> &str {
        data.get(key).and_then(|v| v.as_str()).unwrap_or("")
    };

    let mut briefing_text = String::new();

    let project_arc = get_str("project_arc");
    if !project_arc.is_empty() {
        briefing_text.push_str("## Their project\n");
        briefing_text.push_str(project_arc);
        briefing_text.push_str("\n\n");
    }

    let current_focus = get_str("current_focus");
    if !current_focus.is_empty() {
        briefing_text.push_str("## What they're building now\n");
        briefing_text.push_str(current_focus);
        briefing_text.push_str("\n\n");
    }

    let setup = get_str("setup_comparison");
    if !setup.is_empty() {
        briefing_text.push_str("## Their setup vs yours\n");
        briefing_text.push_str(setup);
        briefing_text.push_str("\n\n");
    }

    let overlaps = get_str("overlaps");
    if !overlaps.is_empty() {
        briefing_text.push_str("## Where your work overlaps\n");
        briefing_text.push_str(overlaps);
        briefing_text.push_str("\n\n");
    }

    let candid = get_str("candid_takes");
    if !candid.is_empty() {
        briefing_text.push_str("## Candid takes\n");
        briefing_text.push_str(candid);
        briefing_text.push_str("\n\n");
    }

    if let Some(starters) = data.get("conversation_starters").and_then(|v| v.as_array()) {
        if !starters.is_empty() {
            briefing_text.push_str("## Conversation starters\n");
            for starter in starters {
                if let Some(s) = starter.as_str() {
                    briefing_text.push_str(&format!("- {}\n", s));
                }
            }
        }
    }

    display.show_briefing(&briefing_text);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_text_short_line() {
        let lines = wrap_text("hello world", 60);
        assert_eq!(lines, vec!["hello world"]);
    }

    #[test]
    fn wrap_text_long_line() {
        let text = "the quick brown fox jumps over the lazy dog";
        let lines = wrap_text(text, 20);
        assert!(lines.len() > 1);
        for line in &lines {
            assert!(line.len() <= 20, "line too long: {}", line);
        }
    }

    #[test]
    fn wrap_text_empty() {
        let lines = wrap_text("", 40);
        assert_eq!(lines, vec![""]);
    }

    #[test]
    fn chat_display_creation() {
        let display = ChatDisplay::new("alice");
        assert_eq!(display.peer_name, "alice");
    }
}
