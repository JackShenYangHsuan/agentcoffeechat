use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

// ---------------------------------------------------------------------------
// AiTool — which AI coding agent is installed
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AiTool {
    ClaudeCode,
    Codex,
    GeminiCli,
    Unknown,
}

impl std::fmt::Display for AiTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AiTool::ClaudeCode => write!(f, "Claude Code"),
            AiTool::Codex => write!(f, "Codex"),
            AiTool::GeminiCli => write!(f, "Gemini CLI"),
            AiTool::Unknown => write!(f, "Unknown"),
        }
    }
}

// ---------------------------------------------------------------------------
// Marker tags for clean install/uninstall
// ---------------------------------------------------------------------------

const MARKER_START: &str = "<!-- agentcoffeechat-start -->";
const MARKER_END: &str = "<!-- agentcoffeechat-end -->";

// ---------------------------------------------------------------------------
// Guidelines content — embedded as a const string
// ---------------------------------------------------------------------------

const GUIDELINES: &str = r#"# AgentCoffeeChat — Coffee Chats with Nearby Developers' Agents

AgentCoffeeChat lets you have live conversations with nearby developers' AI agents.
The user speaks natural language — you handle all the CLI commands via the Bash tool.

## When to use AgentCoffeeChat
- User says: "start agentcoffeechat", "connect to Bob", "chat with Bob", "ask Bob about...", "who's nearby", "disconnect"
- Proactively mention new connected peers at natural breaks

## Commands (all support --json, `acc` is alias for `agentcoffeechat`)
- acc start — Start daemon
- acc peers --json — List nearby peers
- acc connect <name> --json — Connect to peer (sends approval request)
- acc pending --json — List pending incoming connection requests
- acc approve <name> --json — Approve a pending connection
- acc deny <name> --json — Deny a pending connection
- acc disconnect [name] — End session
- acc sessions --json — List active sessions
- acc chat --to <name> --json — Coffee chat
- acc ask <name> "<question>" --json — Instant question (~30s)
- acc history --json — Past chats
- acc status --json — Daemon status
- acc doctor --json — Diagnostics

## Connection flow
1. acc peers --json — find nearby peers
2. acc connect <name> --json — sends connection request to peer
3. Peer's agent sees the request via acc pending --json and asks their user to approve
4. Once approved on both sides, session is active for 1 hour

## Incoming connection requests
- Periodically check `acc pending --json` for incoming requests
- When a request is found, ask the user: "<peer_name> wants to connect. Allow?
- If yes: run `acc approve <name> --json`
- If no: run `acc deny <name> --json`
- IMPORTANT: Always ask the user before approving. Never auto-approve.

## After a chat
- Present briefing: What they're building, Interesting learnings, Tips, Ideas to explore
- SAVE key learnings to memory
- Suggest actions but DON'T execute without asking

## Important
- If daemon not running: tell user to say "start agentcoffeechat"
- All acc commands are safe to run
- Never share raw transcripts unless asked"#;

// ---------------------------------------------------------------------------
// CLAUDE.md snippet — short reference added to ~/.claude/CLAUDE.md
// ---------------------------------------------------------------------------

const CLAUDE_MD_SNIPPET: &str = r#"## AgentCoffeeChat
AgentCoffeeChat is installed. Use the /agentcoffeechat skill for details, or run `acc` commands to manage coffee chats with nearby developers' agents."#;

// ---------------------------------------------------------------------------
// Detection
// ---------------------------------------------------------------------------

/// Detect which AI coding tool is installed by checking `which` for known CLI
/// binary names. Returns the first match in priority order.
pub fn detect_ai_tool() -> AiTool {
    if which_exists("claude") {
        AiTool::ClaudeCode
    } else if which_exists("codex") {
        AiTool::Codex
    } else if which_exists("gemini") {
        AiTool::GeminiCli
    } else {
        AiTool::Unknown
    }
}

/// Detect ALL installed AI coding tools by checking `which` for each known CLI
/// binary name. Returns a Vec of all tools that are found on the system.
pub fn detect_all_ai_tools() -> Vec<AiTool> {
    let mut tools = Vec::new();
    if which_exists("claude") {
        tools.push(AiTool::ClaudeCode);
    }
    if which_exists("codex") {
        tools.push(AiTool::Codex);
    }
    if which_exists("gemini") {
        tools.push(AiTool::GeminiCli);
    }
    tools
}

/// Install the AgentCoffeeChat plugin/guidelines for ALL detected AI tools.
/// Returns a Vec of (tool, result) pairs so the caller can report per-tool
/// success or failure.
pub fn install_all_plugins() -> Vec<(AiTool, Result<()>)> {
    let tools = detect_all_ai_tools();
    tools
        .into_iter()
        .map(|tool| {
            let result = install_plugin(&tool);
            (tool, result)
        })
        .collect()
}

/// Check whether a binary is reachable via `which`.
fn which_exists(name: &str) -> bool {
    Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Home directory helper
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf> {
    dirs_path().context("could not determine home directory")
}

/// Resolve $HOME portably.
fn dirs_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

// ---------------------------------------------------------------------------
// Path helpers per tool
// ---------------------------------------------------------------------------

fn claude_md_path() -> Result<PathBuf> {
    Ok(home_dir()?.join(".claude").join("CLAUDE.md"))
}

fn claude_skill_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".claude")
        .join("skills")
        .join("agentcoffeechat.md"))
}

fn codex_instructions_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".codex")
        .join("instructions")
        .join("agentcoffeechat.md"))
}

fn gemini_instructions_path() -> Result<PathBuf> {
    Ok(home_dir()?
        .join(".gemini")
        .join("instructions")
        .join("agentcoffeechat.md"))
}

// ---------------------------------------------------------------------------
// Install
// ---------------------------------------------------------------------------

/// Install the AgentCoffeeChat plugin/guidelines for the given AI tool.
pub fn install_plugin(tool: &AiTool) -> Result<()> {
    match tool {
        AiTool::ClaudeCode => install_claude_code(),
        AiTool::Codex => install_codex(),
        AiTool::GeminiCli => install_gemini_cli(),
        AiTool::Unknown => {
            // Nothing to install — no known tool detected.
            Ok(())
        }
    }
}

fn install_claude_code() -> Result<()> {
    // 1. Append the CLAUDE.md snippet (with markers) if not already present.
    let claude_md = claude_md_path()?;
    ensure_parent_dir(&claude_md)?;

    let marked_snippet = format!(
        "\n{MARKER_START}\n{CLAUDE_MD_SNIPPET}\n{MARKER_END}\n"
    );

    if claude_md.exists() {
        let contents = fs::read_to_string(&claude_md)
            .with_context(|| format!("failed to read {}", claude_md.display()))?;
        if !contents.contains(MARKER_START) {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(&claude_md)
                .with_context(|| format!("failed to open {} for appending", claude_md.display()))?;
            std::io::Write::write_all(&mut file, marked_snippet.as_bytes())
                .with_context(|| format!("failed to append to {}", claude_md.display()))?;
        }
    } else {
        fs::write(&claude_md, marked_snippet)
            .with_context(|| format!("failed to write {}", claude_md.display()))?;
    }

    // 2. Write the full skill file.
    let skill_path = claude_skill_path()?;
    ensure_parent_dir(&skill_path)?;

    let skill_content = format!(
        "{MARKER_START}\n{GUIDELINES}\n{MARKER_END}\n"
    );
    fs::write(&skill_path, skill_content)
        .with_context(|| format!("failed to write {}", skill_path.display()))?;

    Ok(())
}

fn install_codex() -> Result<()> {
    let path = codex_instructions_path()?;
    ensure_parent_dir(&path)?;

    let content = format!("{MARKER_START}\n{GUIDELINES}\n{MARKER_END}\n");
    fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

fn install_gemini_cli() -> Result<()> {
    let path = gemini_instructions_path()?;
    ensure_parent_dir(&path)?;

    let content = format!("{MARKER_START}\n{GUIDELINES}\n{MARKER_END}\n");
    fs::write(&path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Uninstall
// ---------------------------------------------------------------------------

/// Remove AgentCoffeeChat plugin/guidelines for the given AI tool.
pub fn uninstall_plugin(tool: &AiTool) -> Result<()> {
    match tool {
        AiTool::ClaudeCode => uninstall_claude_code(),
        AiTool::Codex => uninstall_codex(),
        AiTool::GeminiCli => uninstall_gemini_cli(),
        AiTool::Unknown => Ok(()),
    }
}

fn uninstall_claude_code() -> Result<()> {
    // Remove the marked section from CLAUDE.md.
    let claude_md = claude_md_path()?;
    if claude_md.exists() {
        remove_marked_section(&claude_md)?;
    }

    // Remove the skill file entirely.
    let skill_path = claude_skill_path()?;
    if skill_path.exists() {
        fs::remove_file(&skill_path)
            .with_context(|| format!("failed to remove {}", skill_path.display()))?;
    }

    Ok(())
}

fn uninstall_codex() -> Result<()> {
    let path = codex_instructions_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

fn uninstall_gemini_cli() -> Result<()> {
    let path = gemini_instructions_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    }
    Ok(())
}

/// Remove everything between (and including) MARKER_START and MARKER_END in a
/// file. Preserves the rest of the file content.
fn remove_marked_section(path: &Path) -> Result<()> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;

    if let (Some(start), Some(end)) = (contents.find(MARKER_START), contents.find(MARKER_END)) {
        let end = end + MARKER_END.len();
        // Also consume a trailing newline if present.
        let end = if contents[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        // Also consume a leading newline if present (from the \n before MARKER_START).
        let start = if start > 0 && contents.as_bytes()[start - 1] == b'\n' {
            start - 1
        } else {
            start
        };
        let cleaned = format!("{}{}", &contents[..start], &contents[end..]);
        fs::write(path, cleaned)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Query
// ---------------------------------------------------------------------------

/// Check whether the plugin is already installed for the given AI tool.
pub fn is_plugin_installed(tool: &AiTool) -> bool {
    match tool {
        AiTool::ClaudeCode => {
            // Check that both the CLAUDE.md marker and skill file exist.
            let has_marker = claude_md_path()
                .ok()
                .and_then(|p| fs::read_to_string(p).ok())
                .map(|c| c.contains(MARKER_START))
                .unwrap_or(false);
            let has_skill = claude_skill_path()
                .ok()
                .map(|p| p.exists())
                .unwrap_or(false);
            has_marker && has_skill
        }
        AiTool::Codex => codex_instructions_path()
            .ok()
            .map(|p| p.exists())
            .unwrap_or(false),
        AiTool::GeminiCli => gemini_instructions_path()
            .ok()
            .map(|p| p.exists())
            .unwrap_or(false),
        AiTool::Unknown => false,
    }
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Ensure the parent directory of `path` exists, creating it recursively if
/// necessary.
fn ensure_parent_dir(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory {}", parent.display()))?;
        }
    }
    Ok(())
}

/// Return the raw guidelines text. Useful for testing.
pub fn guidelines_content() -> &'static str {
    GUIDELINES
}

/// Return the marker start tag.
pub fn marker_start() -> &'static str {
    MARKER_START
}

/// Return the marker end tag.
pub fn marker_end() -> &'static str {
    MARKER_END
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_ai_tool_returns_a_variant() {
        // In CI/test environments none of the tools may be installed.
        // The important thing is that the function doesn't panic.
        let tool = detect_ai_tool();
        // We can't assert which variant (depends on the machine), but we can
        // assert it's a valid enum variant.
        match tool {
            AiTool::ClaudeCode | AiTool::Codex | AiTool::GeminiCli | AiTool::Unknown => {}
        }
    }

    #[test]
    fn detect_ai_tool_unknown_when_no_tools_on_path() {
        // If we override PATH to something empty, no tool should be found.
        // Use a guard so PATH is restored even on panic.
        struct PathGuard(String);
        impl Drop for PathGuard {
            fn drop(&mut self) {
                unsafe { std::env::set_var("PATH", &self.0); }
            }
        }
        let _guard = PathGuard(std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", "/nonexistent_dir_for_test"); }
        let tool = detect_ai_tool();
        // PATH is restored automatically by _guard on drop.
        assert_eq!(tool, AiTool::Unknown, "expected Unknown when PATH has no tools");
    }

    #[test]
    fn guidelines_contains_key_phrases() {
        let content = guidelines_content();
        assert!(
            content.contains("acc connect"),
            "guidelines should mention 'acc connect'"
        );
        assert!(
            content.to_lowercase().contains("coffee chat"),
            "guidelines should mention 'coffee chat' (case-insensitive)"
        );
        assert!(
            content.contains("acc peers"),
            "guidelines should mention 'acc peers'"
        );
        assert!(
            content.contains("acc start"),
            "guidelines should mention 'acc start'"
        );
        assert!(
            content.contains("Connection flow"),
            "guidelines should include the connection flow section"
        );
        assert!(
            content.contains("3-word code"),
            "guidelines should mention the 3-word code exchange"
        );
    }

    #[test]
    fn marker_tags_present_in_formatted_content() {
        let start = marker_start();
        let end = marker_end();
        assert!(
            start.contains("agentcoffeechat-start"),
            "start marker should contain 'agentcoffeechat-start'"
        );
        assert!(
            end.contains("agentcoffeechat-end"),
            "end marker should contain 'agentcoffeechat-end'"
        );
        // Markers should be valid HTML comments for clean embedding in .md files.
        assert!(start.starts_with("<!--"), "start marker should be an HTML comment");
        assert!(end.starts_with("<!--"), "end marker should be an HTML comment");
        assert!(start.ends_with("-->"), "start marker should be an HTML comment");
        assert!(end.ends_with("-->"), "end marker should be an HTML comment");
    }

    /// Combined test for all three tools' install/uninstall. Uses a single
    /// HOME override to avoid env-var races between parallel tests.
    #[test]
    fn install_and_uninstall_all_tools_in_temp_dir() {
        let tmp = std::env::temp_dir().join("acc_plugin_test_all");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // Override HOME so all path helpers resolve into our temp dir.
        // Use a struct guard so HOME is restored even if the test panics.
        struct HomeGuard(String);
        impl Drop for HomeGuard {
            fn drop(&mut self) {
                unsafe { std::env::set_var("HOME", &self.0); }
            }
        }
        let _guard = HomeGuard(std::env::var("HOME").unwrap_or_default());
        unsafe { std::env::set_var("HOME", tmp.to_str().unwrap()); }

        // --- Claude Code ---
        install_plugin(&AiTool::ClaudeCode).expect("claude install should succeed");
        assert!(is_plugin_installed(&AiTool::ClaudeCode), "claude plugin should be installed");

        // Verify CLAUDE.md has the marker.
        let claude_md = tmp.join(".claude").join("CLAUDE.md");
        let contents = fs::read_to_string(&claude_md).unwrap();
        assert!(contents.contains(MARKER_START));
        assert!(contents.contains(MARKER_END));
        assert!(contents.contains("AgentCoffeeChat is installed"));

        // Verify skill file has guidelines.
        let skill = tmp.join(".claude").join("skills").join("agentcoffeechat.md");
        let skill_contents = fs::read_to_string(&skill).unwrap();
        assert!(skill_contents.contains("acc connect"));
        assert!(skill_contents.contains(MARKER_START));

        // Idempotent install — should not duplicate.
        install_plugin(&AiTool::ClaudeCode).expect("second claude install should succeed");
        let contents2 = fs::read_to_string(&claude_md).unwrap();
        assert_eq!(
            contents2.matches(MARKER_START).count(),
            1,
            "marker should appear only once after re-install"
        );

        // Uninstall Claude Code.
        uninstall_plugin(&AiTool::ClaudeCode).expect("claude uninstall should succeed");
        assert!(!is_plugin_installed(&AiTool::ClaudeCode), "claude plugin should be uninstalled");
        let after = fs::read_to_string(&claude_md).unwrap();
        assert!(!after.contains(MARKER_START));
        assert!(!skill.exists());

        // --- Codex ---
        install_plugin(&AiTool::Codex).expect("codex install should succeed");
        assert!(is_plugin_installed(&AiTool::Codex));

        let codex_path = tmp.join(".codex").join("instructions").join("agentcoffeechat.md");
        let codex_contents = fs::read_to_string(&codex_path).unwrap();
        assert!(codex_contents.contains("acc connect"));
        assert!(codex_contents.contains(MARKER_START));

        uninstall_plugin(&AiTool::Codex).expect("codex uninstall should succeed");
        assert!(!is_plugin_installed(&AiTool::Codex));
        assert!(!codex_path.exists());

        // --- Gemini CLI ---
        install_plugin(&AiTool::GeminiCli).expect("gemini install should succeed");
        assert!(is_plugin_installed(&AiTool::GeminiCli));

        let gemini_path = tmp.join(".gemini").join("instructions").join("agentcoffeechat.md");
        let gemini_contents = fs::read_to_string(&gemini_path).unwrap();
        assert!(gemini_contents.contains("acc connect"));
        assert!(gemini_contents.contains(MARKER_START));

        uninstall_plugin(&AiTool::GeminiCli).expect("gemini uninstall should succeed");
        assert!(!is_plugin_installed(&AiTool::GeminiCli));
        assert!(!gemini_path.exists());

        // HOME is restored automatically by _guard on drop.
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn unknown_tool_is_not_installed() {
        assert!(!is_plugin_installed(&AiTool::Unknown));
    }

    #[test]
    fn install_unknown_is_noop() {
        // Should succeed without doing anything.
        install_plugin(&AiTool::Unknown).expect("installing Unknown should be a no-op");
    }

    #[test]
    fn remove_marked_section_preserves_surrounding_content() {
        let tmp = std::env::temp_dir().join("acc_marker_test.md");
        let content = format!(
            "# My Notes\nSome content here.\n{MARKER_START}\nPlugin stuff\n{MARKER_END}\nMore content below.\n"
        );
        fs::write(&tmp, &content).unwrap();

        remove_marked_section(&tmp).unwrap();

        let after = fs::read_to_string(&tmp).unwrap();
        assert!(!after.contains(MARKER_START));
        assert!(!after.contains("Plugin stuff"));
        assert!(after.contains("# My Notes"));
        assert!(after.contains("More content below."));

        let _ = fs::remove_file(&tmp);
    }
}
