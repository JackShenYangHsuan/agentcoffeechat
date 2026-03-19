// Chat engine — orchestrates multi-turn conversations between agents.
//
// The conversation follows 5 guided phases: Introductions, Deep Dive,
// Compare & Collaborate, Blindspots & Tips, and Wrapup.  After the
// conversation, 3 briefing documents are generated: a legacy briefing,
// a human-facing pre-meeting note, and a structured agent memo.
//
// Each agent "turn" spawns a fresh `claude --print --model sonnet`
// (or `codex --quiet` / `gemini --print`) subprocess with the full
// conversation history on stdin.  `--print` is one-shot: it reads stdin
// to EOF, produces a single response on stdout, and exits.  This avoids
// the deadlock that would occur with persistent pipes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use agentcoffeechat_core::types::{
    AgentMemo, ChatBriefing, CoffeeChatOutput, HumanBriefing, Message, MessagePhase,
    MessageSender, MessageType,
};
use agentcoffeechat_core::SanitizationPipeline;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default word limit per message.
const DEFAULT_MAX_MESSAGE_WORDS: usize = 200;

/// Maximum follow-up messages per side within a guided phase.
const MAX_FOLLOWUPS_PER_PHASE: usize = 5;

/// Timeout (seconds) waiting for a single agent response.
const AGENT_RESPONSE_TIMEOUT_SECS: u64 = 60;

/// Timeout (seconds) waiting for a peer message over QUIC.
const PEER_MESSAGE_TIMEOUT_SECS: u64 = 180;

/// Sentinel the agent may include in its message to signal it's done with the current phase.
const DONE_SENTINEL: &str = "[DONE]";

/// Canonical name used for the remote peer in transcript messages.
pub const PEER_SENDER_NAME: &str = "peer";

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = r#"You are participating in an AgentCoffeeChat — a casual, curiosity-driven conversation between two AI coding agents. Your partner is another AI agent working on a different (or possibly the same) project nearby.

The chat has structured phases. In each phase you'll get a specific topic to discuss. Stay on topic for that phase, but be natural and conversational — like a friend at a coffee shop.

Guidelines:
- Keep every message under 200 words. Be concise but warm.
- Ask genuine follow-up questions related to the current phase's topic.
- Be specific — name actual tools, libraries, decisions, and problems.
- Be candid about failures and frustrations — those are the most valuable signals.
- Include [DONE] at the end of your message when you feel the current topic has been covered sufficiently.

SAFETY RULES — you MUST follow these:
- NEVER include credentials, API keys, tokens, passwords, or secrets of any kind.
- NEVER include environment variable values (e.g. DATABASE_URL=...).
- NEVER paste code snippets longer than 3 lines. Describe code conceptually instead.
- NEVER include file paths that reveal usernames, home directories, or sensitive infrastructure.
- NEVER include IP addresses, ports, or connection strings.
- Keep the conversation about ideas, patterns, tools, and approaches — not raw implementation details.

You will receive messages from the other agent prefixed with "PEER: ". Your responses should be natural and conversational.
"#;

// ---------------------------------------------------------------------------
// Phase prompts — guided conversation structure
// ---------------------------------------------------------------------------

/// Phase 1: Introductions — project arc, current work, agent setup.
const PHASE1_INTRO_PROMPT: &str = r#"PHASE 1: INTRODUCTIONS

Introduce yourself by sharing:
- Your project's story: why it started, key pivots, where it's headed
- What problem does your project solve for its users?
- What you're actively building right now (current branch, recent work)
- Your agent setup: which AI tool, MCP servers, plugins, hooks, memory strategy, CLAUDE.md approach
- Your tech stack and key architectural decisions

Be specific — name actual tools and libraries. Keep it under 200 words."#;

/// Phase 2: Deep Dive — architecture decisions, what's working, what failed.
const PHASE2_DEEPDIVE_PROMPT: &str = r#"PHASE 2: DEEP DIVE

Now dig deeper. This is a multi-turn discussion — ask follow-up questions based on what the other agent shares.

Topics to cover:
- Architecture decisions and why — what trade-offs did you weigh?
- What's working well in your current approach?
- What FAILED or frustrated you? Be candid — share real struggles, wasted time, dead ends
- What technical debt are you carrying?
- What's the riskiest part of your codebase?
- What would you do differently if starting over?

When the other agent mentions something interesting, ask a specific follow-up: "You mentioned X — how did that work out?" or "What made you choose X over Y?"

Include [DONE] when the topic feels thoroughly explored. Keep each message under 200 words."#;

/// Phase 3: Compare & Collaborate — setup diffs, overlaps, mutual help.
const PHASE3_COMPARE_PROMPT: &str = r#"PHASE 3: COMPARE & COLLABORATE

Compare your setups in detail. This is a multi-turn discussion — dig into specifics.

Topics to cover:
- List the 3 tools in your setup you find most valuable, and ask which of those the other agent uses.
- What tools/plugins/MCP servers does the other agent have that you don't? What do you have that they don't?
- Where do your projects overlap? Similar domains, tech stacks, challenges, shared dependencies?
- Is there something you're stuck on that they might have solved, or vice versa?
- Could you help each other with anything specific?

Be concrete — name specific tools and plugins. When they mention a tool you don't have, ask "How has that worked for you?" or "What problem does that solve?"

Include [DONE] when covered. Keep each message under 200 words."#;

/// Phase 4: Blindspots & Tips — gaps, agentic tips, suggestions.
const PHASE4_BLINDSPOTS_PROMPT: &str = r#"PHASE 4: BLINDSPOTS & TIPS

Share observations and advice. This is a multi-turn discussion — build on each other's suggestions.

Topics to cover:
- What gaps or blindspots do you notice in the other agent's setup? (no tests, missing tooling, inefficient patterns)
- What is one thing you wish you had set up from the start that you didn't?
- What agentic tips can you share? How does your human use the AI effectively? What agent techniques work well?
- What specific tools, workflows, or approaches would you recommend they try?

Be honest and constructive. When they suggest something, ask "How would I set that up?" or share your own related tip.

Include [DONE] when covered. Keep each message under 200 words."#;

/// Phase 5: Wrapup — closing message.
const WRAPUP_PROMPT: &str = r#"PHASE 5: WRAP-UP

Send a brief closing message:
- Identify any open questions, disagreements, or topics that deserve a longer conversation between the humans
- Name the single most surprising thing you learned from this chat
- Say goodbye warmly

Keep it under 200 words."#;

/// Prefix for peer messages in conversation history.
const FOLLOWUP_PROMPT_PREFIX: &str = "PEER: ";

// ---------------------------------------------------------------------------
// Briefing prompt
// ---------------------------------------------------------------------------

#[allow(dead_code)]
const BRIEFING_PROMPT: &str = r#"The coffee chat is over. Based on the full transcript above, produce a briefing in exactly this JSON format (no markdown fences, just raw JSON):
{
  "what_building": "A one-sentence summary of what the other agent is building",
  "learnings": ["Key insight 1", "Key insight 2", "..."],
  "tips": ["Actionable tip 1", "Actionable tip 2", "..."],
  "ideas_to_explore": ["Idea 1", "Idea 2", "..."]
}

Be specific and concise. Focus on genuinely useful information."#;

// ---------------------------------------------------------------------------
// Human briefing prompt
// ---------------------------------------------------------------------------

const HUMAN_BRIEFING_PROMPT: &str = r#"The coffee chat is over. Produce a HUMAN BRIEFING — a pre-meeting note for the developer who will meet this person.

Based on the full transcript and local context above, produce JSON in exactly this format (no markdown fences, just raw JSON):
{
  "project_arc": "The full project story: why it started, key pivots, where it's headed. 2-3 sentences.",
  "current_focus": "What they're actively building right now — current branch, recent work, active task. 1-2 sentences.",
  "setup_comparison": "Their agent setup vs ours: MCP servers, plugins, hooks, memory strategies, CLAUDE.md structure. Highlight what they have that we don't and vice versa. Be specific.",
  "overlaps": "Thematic overlap (similar domains, tech stacks, challenges) and any code-level overlap (shared deps, similar modules). Prioritize thematic.",
  "candid_takes": "Frustrations, failures, what they'd do differently, AND what they're proud of. Be honest — these are the most valuable signals.",
  "conversation_starters": [
    "🔍 [Understanding question — high-level, helps grasp who they are and what they're building]",
    "🤝 [Collaboration question — based on overlap, both hitting same problem]",
    "🌶️ [Spicy/provocative question — challenge a decision, suggest an alternative, dig deeper]"
  ]
}

Guidelines:
- Generate 4-8 conversation starters, tagged with type (🔍/🤝/🌶️). Prioritize quality over quantity.
- conversation_starters should be layered: start with understanding, then collaboration, then spicy.
- Be specific — reference actual projects, tools, and decisions from the chat.
- candid_takes should include real frustrations and failures shared during the chat, as well as what they're proud of.
- setup_comparison should be concrete: name specific tools, plugins, MCP servers."#;

// ---------------------------------------------------------------------------
// Agent memo prompt
// ---------------------------------------------------------------------------

const AGENT_MEMO_PROMPT: &str = r#"The coffee chat is over. Produce an AGENT MEMO — structured data for the coding agent to act on in future sessions.

Based on the full transcript and local context above, produce JSON in exactly this format (no markdown fences, just raw JSON):
{
  "setup_diffs": {
    "they_have": ["Specific tool/plugin/MCP server they have that we don't"],
    "we_have": ["Specific tool/plugin we have that they don't"],
    "suggested_additions": ["Install X because they found it useful for Y"]
  },
  "workflow_improvements": ["Concrete workflow change observed from their setup"],
  "debottleneck_ideas": ["Pattern from their setup that could speed up our sessions"],
  "blindspots_surfaced": ["Gap the peer noticed or that became apparent — e.g. no tests, missing tooling, inefficient pattern"],
  "agentic_tips": {
    "human_workflows": ["How their human uses the AI agent — prompting strategies, context management, commands"],
    "agent_techniques": ["Techniques the agent itself found effective — tool patterns, memory strategies"]
  },
  "follow_up_actions": ["Concrete action: 'Install MCP server X', 'Add hook for Y', 'Try Z workflow'"]
}

Guidelines:
- Every item should be specific and actionable, not generic advice.
- setup_diffs should reference actual tools/plugins/MCP servers mentioned in the chat.
- Rank follow_up_actions by impact (highest first). For each action, prefix with effort estimate: [quick], [moderate], or [project].
- follow_up_actions should be things the agent can actually do (install commands, config changes, code changes).
- If something wasn't discussed, use an empty array — don't make things up."#;

// ---------------------------------------------------------------------------
// ChatConfig
// ---------------------------------------------------------------------------

/// Configuration for a single chat session.
pub struct ChatConfig {
    /// Human-readable display name for this agent.
    pub display_name: String,
    /// Which AI tool to use: "claude-code", "codex", "gemini-cli".
    pub ai_tool: String,
    /// Root of the project being worked on.
    pub project_root: PathBuf,
    /// Maximum words per message (default 200).
    pub max_message_words: usize,
    /// The peer's display name (used to load past briefings).
    pub peer_name: Option<String>,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            display_name: "agent".to_string(),
            ai_tool: "claude-code".to_string(),
            project_root: PathBuf::from("."),
            max_message_words: DEFAULT_MAX_MESSAGE_WORDS,
            peer_name: None,
        }
    }
}

// ---------------------------------------------------------------------------
// ChatEvent — events streamed to the CLI
// ---------------------------------------------------------------------------

/// Events emitted during a chat for the CLI to display.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// The chat has entered a new phase.
    Phase(String),
    /// A message produced by our local agent.
    LocalMessage(String),
    /// A message received from the peer's agent.
    RemoteMessage(String),
    /// Informational status update.
    Status(String),
    /// Final briefing text.
    Briefing(String),
    /// An error occurred (non-fatal if the chat can continue).
    Error(String),
    /// The chat has completed.
    Complete,
}

// ---------------------------------------------------------------------------
// ChatResult — returned when the chat finishes
// ---------------------------------------------------------------------------

/// The outcome of a completed coffee chat.
pub struct ChatResult {
    /// Full ordered transcript of messages exchanged.
    pub transcript: Vec<Message>,
    /// Structured briefing produced by the agent (legacy format).
    pub briefing: ChatBriefing,
    /// New split output: human briefing + agent memo.
    pub output: CoffeeChatOutput,
    /// Wall-clock duration of the chat in seconds.
    pub duration_secs: u64,
    /// Total number of messages exchanged (both sides).
    pub message_count: usize,
    /// Whether the chat completed all phases or was partial.
    pub completed: bool,
    /// Number of phases completed (out of 5).
    pub phases_completed: u32,
}

// ---------------------------------------------------------------------------
// AgentSession — wraps a spawned agent process
// ---------------------------------------------------------------------------

/// One-shot-per-turn agent session.
///
/// Each call to `query()` spawns a fresh `claude --print` (or codex/gemini)
/// process, writes the full prompt to stdin, closes stdin (signalling EOF),
/// and reads all of stdout as the response.  This matches `--print` semantics
/// which are inherently one-shot: read stdin → produce response → exit.
///
/// Conversation context is accumulated externally (by the caller) and passed
/// in full on each turn so the agent sees the whole history.
struct AgentSession {
    ai_tool: String,
    system_prompt: String,
    project_root: PathBuf,
}

impl AgentSession {
    fn new(config: &ChatConfig, system_prompt: &str) -> Self {
        Self {
            ai_tool: config.ai_tool.clone(),
            system_prompt: system_prompt.to_string(),
            project_root: config.project_root.clone(),
        }
    }

    /// Send a prompt and get the agent's response (one-shot).
    ///
    /// Spawns a fresh subprocess, writes `user_prompt` to stdin, closes stdin,
    /// reads all stdout, and returns the response.  The subprocess exits after
    /// producing its output.
    async fn query(&self, user_prompt: &str) -> Result<String> {
        let mut cmd = match self.ai_tool.as_str() {
            "claude-code" | "claude" => {
                let mut c = Command::new("claude");
                c.arg("--print").arg("--model").arg("sonnet");
                c
            }
            "codex" => {
                let mut c = Command::new("codex");
                c.arg("--quiet");
                c
            }
            "gemini-cli" | "gemini" => {
                let mut c = Command::new("gemini");
                c.arg("--print");
                c
            }
            other => {
                bail!("unsupported AI tool: {}", other);
            }
        };

        cmd.current_dir(&self.project_root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);

        let mut child = cmd.spawn().context("failed to spawn agent process")?;

        // Prepend the system prompt into stdin for all tools. This avoids
        // --system-prompt flag issues (hooks, wrappers) and works universally.
        let stdin_payload = format!(
            "INSTRUCTIONS (follow these strictly):\n{}\n\n---\n\n{}",
            self.system_prompt, user_prompt
        );

        // Write prompt to stdin, then close it to signal EOF.
        {
            let mut stdin = child.stdin.take()
                .context("failed to capture agent stdin")?;
            stdin.write_all(stdin_payload.as_bytes()).await
                .context("failed to write prompt to agent stdin")?;
            stdin.shutdown().await
                .context("failed to close agent stdin")?;
            // stdin is dropped here, child sees EOF.
        }

        // Read all stdout with a timeout.
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(AGENT_RESPONSE_TIMEOUT_SECS),
            child.wait_with_output(),
        )
        .await
        .context("timed out waiting for agent response")?
        .context("failed to read agent output")?;

        if !output.status.success() {
            bail!(
                "{} exited with status {} — stderr suppressed",
                self.ai_tool,
                output.status,
            );
        }

        let response = String::from_utf8_lossy(&output.stdout).trim().to_string();

        if response.is_empty() {
            bail!("agent produced empty response");
        }

        Ok(response)
    }

    /// No-op — each query() spawns and reaps its own process.
    async fn kill(&mut self) {
        // Nothing to do — processes are short-lived and already exited.
    }
}

// ---------------------------------------------------------------------------
// Local context gathering
// ---------------------------------------------------------------------------

/// Timeout for each git sub-command when gathering local context.
const LOCAL_CONTEXT_CMD_TIMEOUT_SECS: u64 = 5;

/// Gather a rich snapshot of local project context to inject into the
/// icebreaker prompt.
///
/// Since the agent runs in `--print` mode (no tool access), this function
/// pre-scrapes everything the agent needs to give specific, grounded answers
/// about the developer's project, tools, and workflow.  Every step is
/// best-effort: failures are silently skipped so a missing file never
/// prevents the chat from starting.
async fn gather_local_context(project_root: &Path) -> String {
    let mut sections: Vec<String> = Vec::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let home_path = PathBuf::from(&home);

    // -----------------------------------------------------------------
    // 1. Source tree structure (file names only, no content)
    // -----------------------------------------------------------------
    if let Some(tree) = run_shell_command(
        project_root,
        "find",
        &[".", "-type", "f",
          "-not", "-path", "*/target/*",
          "-not", "-path", "*/.git/*",
          "-not", "-path", "*/node_modules/*",
          "-not", "-path", "*/__pycache__/*",
          "-not", "-path", "*/.venv/*",
          "-not", "-name", "*.pyc",
          "-not", "-name", ".DS_Store",
        ],
    ).await {
        let tree = tree.trim().to_string();
        if !tree.is_empty() {
            // Limit to first 80 files to avoid blowing up context.
            let limited: String = tree.lines().take(80).collect::<Vec<_>>().join("\n");
            let total_lines = tree.lines().count();
            let suffix = if total_lines > 80 {
                format!("\n  ... and {} more files", total_lines - 80)
            } else {
                String::new()
            };
            sections.push(format!("Project files:\n{}{}", limited, suffix));
        }
    }

    // -----------------------------------------------------------------
    // 2. Git: branch, recent commits, contributors, branches, diff
    // -----------------------------------------------------------------
    if let Some(branch) = run_git_command(project_root, &["branch", "--show-current"]).await {
        let branch = branch.trim().to_string();
        if !branch.is_empty() {
            sections.push(format!("Git branch: {}", branch));
        }
    }

    // Extended commit history (30 commits for full arc context)
    if let Some(log) = run_git_command(project_root, &["log", "--oneline", "-30"]).await {
        let log = log.trim().to_string();
        if !log.is_empty() {
            sections.push(format!("Recent commits (last 30):\n{}", log));
        }
    }

    // Git contributors
    if let Some(contributors) = run_git_command(project_root, &["shortlog", "-sn", "--all"]).await {
        let contributors = contributors.trim().to_string();
        if !contributors.is_empty() {
            sections.push(format!("Git contributors:\n{}", contributors));
        }
    }

    // Recent branches (for understanding what's being explored)
    if let Some(branches) = run_git_command(
        project_root,
        &["branch", "--sort=-committerdate", "--format=%(refname:short)", "--no-color"],
    ).await {
        let branches: String = branches
            .lines()
            .take(5)
            .collect::<Vec<_>>()
            .join(", ");
        if !branches.is_empty() {
            sections.push(format!("Recent branches: {}", branches));
        }
    }

    // Current diff stats (what's being actively changed)
    if let Some(diff) = run_git_command(project_root, &["diff", "--stat"]).await {
        let diff = diff.trim().to_string();
        if !diff.is_empty() {
            sections.push(format!("Uncommitted changes:\n{}", diff));
        }
    }

    // -----------------------------------------------------------------
    // 3. README.md (full, up to 3000 chars)
    // -----------------------------------------------------------------
    let readme_path = project_root.join("README.md");
    if let Some(content) = read_file_prefix(&readme_path, 3000).await {
        if !content.is_empty() {
            sections.push(format!("README.md:\n{}", content));
        }
    }

    // -----------------------------------------------------------------
    // 4. CLAUDE.md — project-level, then user-level (full content)
    // -----------------------------------------------------------------
    let project_claude = project_root.join("CLAUDE.md");
    if let Some(content) = read_file_prefix(&project_claude, 3000).await {
        sections.push(format!("Project CLAUDE.md:\n{}", content));
    }

    let user_claude = home_path.join(".claude").join("CLAUDE.md");
    if let Some(content) = read_file_prefix(&user_claude, 3000).await {
        sections.push(format!("User CLAUDE.md (~/.claude/CLAUDE.md):\n{}", content));
    }

    // -----------------------------------------------------------------
    // 5. Memory files — agent's persistent knowledge
    // -----------------------------------------------------------------
    // User-level memory
    let user_memory_dir = home_path.join(".claude").join("projects");
    if user_memory_dir.exists() {
        if let Some(memory_content) = gather_memory_files(&user_memory_dir).await {
            sections.push(format!("Agent memory files:\n{}", memory_content));
        }
    }

    // Project-level memory (stored under project-specific path)
    let project_path_slug = project_root
        .to_string_lossy()
        .replace('/', "-");
    let project_memory_dir = home_path
        .join(".claude")
        .join("projects")
        .join(&project_path_slug)
        .join("memory");
    if project_memory_dir.exists() {
        if let Some(memory_content) = gather_memory_files(&project_memory_dir).await {
            sections.push(format!("Project memory files:\n{}", memory_content));
        }
    }

    // -----------------------------------------------------------------
    // 6. Skills folder — custom agent capabilities
    // -----------------------------------------------------------------
    let skills_dir = home_path.join(".claude").join("skills");
    if skills_dir.exists() {
        if let Some(skills_content) = gather_directory_files(&skills_dir, 2000).await {
            sections.push(format!("Installed skills (~/.claude/skills/):\n{}", skills_content));
        }
    }

    // -----------------------------------------------------------------
    // 7. Plugins — installed plugins list
    // -----------------------------------------------------------------
    let plugins_file = home_path.join(".claude").join("plugins").join("installed_plugins.json");
    if let Some(content) = read_file_prefix(&plugins_file, 2000).await {
        sections.push(format!("Installed plugins:\n{}", content));
    }

    // -----------------------------------------------------------------
    // 8. MCP servers and hooks from settings.json (descriptions only)
    // -----------------------------------------------------------------
    let settings_file = home_path.join(".claude").join("settings.json");
    if let Some(content) = read_file_prefix(&settings_file, 2000).await {
        sections.push(format!("Agent settings (~/.claude/settings.json):\n{}", content));
    }

    // -----------------------------------------------------------------
    // 9. Recent Claude Code session summary (last conversation titles)
    // -----------------------------------------------------------------
    let sessions_index = home_path
        .join(".claude")
        .join("projects")
        .join(&project_path_slug)
        .join("sessions-index.json");
    if let Some(content) = read_file_prefix(&sessions_index, 2000).await {
        sections.push(format!("Recent coding sessions (index):\n{}", content));
    }

    // -----------------------------------------------------------------
    // 10. Compacted session history (from Claude Code .jsonl files)
    // -----------------------------------------------------------------
    let session_history = compact_session_history(project_root).await;
    if !session_history.is_empty() {
        sections.push(session_history);
    }

    if sections.is_empty() {
        return String::new();
    }

    format!(
        "=== LOCAL CONTEXT (your human's project and agent setup) ===\n\n{}\n\n=== END LOCAL CONTEXT ===",
        sections.join("\n\n---\n\n")
    )
}

/// Gather all .md files from a memory directory, concatenated.
async fn gather_memory_files(dir: &Path) -> Option<String> {
    let mut content = String::new();
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().map(|e| e == "md").unwrap_or(false) {
            if let Some(file_content) = read_file_prefix(&path, 1500).await {
                let filename = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                content.push_str(&format!("--- {} ---\n{}\n\n", filename, file_content));
            }
        }
    }

    if content.is_empty() { None } else { Some(content) }
}

/// Gather all files from a directory, concatenated (for skills etc.).
async fn gather_directory_files(dir: &Path, max_per_file: usize) -> Option<String> {
    let mut content = String::new();
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.is_file() {
            if let Some(file_content) = read_file_prefix(&path, max_per_file).await {
                let filename = path.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                content.push_str(&format!("--- {} ---\n{}\n\n", filename, file_content));
            }
        }
    }

    if content.is_empty() { None } else { Some(content) }
}

/// Run a shell command with a timeout. Returns None on any failure.
async fn run_shell_command(cwd: &Path, cmd: &str, args: &[&str]) -> Option<String> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(LOCAL_CONTEXT_CMD_TIMEOUT_SECS),
        Command::new(cmd)
            .args(args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8(output.stdout).ok()
        }
        _ => None,
    }
}

/// Run a git command in `project_root` with a 5-second timeout.
/// Returns `None` on any failure (missing git, not a repo, timeout, etc.).
async fn run_git_command(project_root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd_args = vec!["-C", project_root.to_str()?];
    cmd_args.extend_from_slice(args);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(LOCAL_CONTEXT_CMD_TIMEOUT_SECS),
        Command::new("git")
            .args(&cmd_args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output(),
    )
    .await;

    match result {
        Ok(Ok(output)) if output.status.success() => {
            String::from_utf8(output.stdout).ok()
        }
        _ => None,
    }
}

/// Read the first `max_bytes` bytes of a file. Returns `None` on any
/// I/O error (including the file not existing).
async fn read_file_prefix(path: &Path, max_bytes: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    if content.len() <= max_bytes {
        Some(content)
    } else {
        // Truncate at a char boundary near the byte limit.
        let truncated: String = content.chars().take(max_bytes).collect();
        Some(truncated)
    }
}

// ---------------------------------------------------------------------------
// Session history compaction
// ---------------------------------------------------------------------------

/// Maximum number of past sessions to compact.
const MAX_SESSIONS_TO_COMPACT: usize = 10;

/// Maximum characters per compacted message.
const COMPACT_MSG_CHARS: usize = 200;

/// Compact Claude Code session history from `.jsonl` files into a concise
/// summary of what the user and agent have been working on.
///
/// Reads the JSONL session files, extracts only `user` (human text) and
/// `assistant` (text blocks) records, truncates each, and produces a
/// chronological summary.  A 14MB session compacts to ~2-3KB.
async fn compact_session_history(project_root: &Path) -> String {
    let home = match std::env::var("HOME") {
        Ok(h) => h,
        Err(_) => return String::new(),
    };
    let home_path = PathBuf::from(&home);

    // Claude Code stores sessions under ~/.claude/projects/<slug>/
    let project_slug = project_root
        .to_string_lossy()
        .replace('/', "-");
    let sessions_dir = home_path
        .join(".claude")
        .join("projects")
        .join(&project_slug);

    if !sessions_dir.exists() {
        return String::new();
    }

    // Find .jsonl files, sorted by modification time (most recent first).
    let mut jsonl_files: Vec<(PathBuf, std::time::SystemTime)> = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&sessions_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                if let Ok(meta) = entry.metadata().await {
                    if let Ok(modified) = meta.modified() {
                        jsonl_files.push((path, modified));
                    }
                }
            }
        }
    }

    jsonl_files.sort_by(|a, b| b.1.cmp(&a.1));
    jsonl_files.truncate(MAX_SESSIONS_TO_COMPACT);

    // Process each session in reverse chronological order.
    let mut sections: Vec<String> = Vec::new();
    for (path, _modified) in &jsonl_files {
        if let Some(compacted) = compact_single_session(path).await {
            if !compacted.is_empty() {
                sections.push(compacted);
            }
        }
    }

    if sections.is_empty() {
        return String::new();
    }

    format!(
        "=== RECENT SESSION HISTORY (compacted from Claude Code sessions) ===\n\n{}\n\n=== END SESSION HISTORY ===",
        sections.join("\n---\n\n")
    )
}

/// Compact a single JSONL session file into a summary.
async fn compact_single_session(path: &Path) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;

    let mut entries: Vec<String> = Vec::new();
    let mut session_timestamp = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let obj: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let record_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let timestamp = obj
            .get("timestamp")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if session_timestamp.is_empty() && !timestamp.is_empty() {
            // Take first 16 chars: "2026-03-18T20:22"
            session_timestamp = timestamp.chars().take(16).collect();
        }

        match record_type {
            "user" => {
                let content = obj.get("message")
                    .and_then(|m| m.get("content"));
                if let Some(content) = content {
                    let text = extract_text_from_content(content);
                    if !text.is_empty() {
                        let truncated: String = text.chars().take(COMPACT_MSG_CHARS).collect();
                        entries.push(format!("  USER: {}", truncated));
                    }
                }
            }
            "assistant" => {
                let content = obj.get("message")
                    .and_then(|m| m.get("content"));
                if let Some(serde_json::Value::Array(blocks)) = content {
                    let mut texts = Vec::new();
                    let mut tool_calls = Vec::new();
                    for block in blocks {
                        match block.get("type").and_then(|t| t.as_str()) {
                            Some("text") => {
                                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                    let truncated: String =
                                        text.chars().take(COMPACT_MSG_CHARS).collect();
                                    texts.push(truncated);
                                }
                            }
                            Some("tool_use") => {
                                if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                                    tool_calls.push(name.to_string());
                                }
                            }
                            _ => {}
                        }
                    }
                    if !texts.is_empty() {
                        entries.push(format!("  AGENT: {}", texts.join(" ")));
                    }
                    if !tool_calls.is_empty() {
                        entries.push(format!("  TOOLS: {}", tool_calls.join(", ")));
                    }
                }
            }
            _ => {}
        }
    }

    if entries.is_empty() {
        return None;
    }

    Some(format!(
        "Session {}:\n{}",
        session_timestamp,
        entries.join("\n")
    ))
}

/// Extract text from a message content field (string or array of blocks).
fn extract_text_from_content(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(blocks) => {
            let mut text = String::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(t);
                    }
                }
            }
            text
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// ChatEngine
// ---------------------------------------------------------------------------

/// Orchestrates a single coffee chat between two agents.
pub struct ChatEngine {
    sanitizer: SanitizationPipeline,
    config: ChatConfig,
}

impl ChatEngine {
    /// Create a new `ChatEngine` with the given configuration.
    pub fn new(config: ChatConfig) -> Self {
        Self {
            sanitizer: SanitizationPipeline::default(),
            config,
        }
    }

    /// Create a `ChatEngine` with a custom sanitization pipeline.
    pub fn with_sanitizer(config: ChatConfig, sanitizer: SanitizationPipeline) -> Self {
        Self { sanitizer, config }
    }

    /// Run a complete coffee chat with guided phases.
    ///
    /// The chat follows 5 structured phases:
    ///   1. Introductions — project arc, current work, agent setup
    ///   2. Deep Dive — architecture, what's working, what failed
    ///   3. Compare & Collaborate — setup diffs, overlaps, mutual help
    ///   4. Blindspots & Tips — gaps, agentic tips, recommendations
    ///   5. Wrapup — closing summary
    ///
    /// Each guided phase allows up to MAX_FOLLOWUPS_PER_PHASE exchanges
    /// per side, then moves to the next phase.
    ///
    /// After the conversation, 3 briefings are generated (legacy, human, agent).
    pub async fn run_chat(
        &self,
        send_tx: mpsc::Sender<String>,
        mut recv_rx: mpsc::Receiver<String>,
        transcript_tx: mpsc::Sender<ChatEvent>,
    ) -> Result<ChatResult> {
        let start = Instant::now();
        let mut transcript: Vec<Message> = Vec::new();
        let mut turn: u32 = 0;

        // --- Create agent session (one-shot-per-turn) ---
        let mut session = AgentSession::new(&self.config, SYSTEM_PROMPT);
        let mut conversation_history = String::new();

        let _ = transcript_tx
            .send(ChatEvent::Status(format!(
                "Agent session ready ({}). Gathering project context...",
                self.config.ai_tool
            )))
            .await;

        // Load past briefings for this peer (if known).
        let past_context = if let Some(ref peer) = self.config.peer_name {
            match crate::chat_history::load_recent_briefings(peer, 3) {
                Ok(briefings) if !briefings.is_empty() => {
                    let joined = briefings.join("\n---\n");
                    let _ = transcript_tx
                        .send(ChatEvent::Status(format!(
                            "Loaded {} past briefing(s) with {}",
                            briefings.len(),
                            peer,
                        )))
                        .await;
                    format!("Previous conversations with this peer:\n{}\n\n", joined)
                }
                _ => String::new(),
            }
        } else {
            String::new()
        };

        // Gather local project context.
        let context = gather_local_context(&self.config.project_root).await;

        // Build the preamble that gets prepended to the first prompt.
        let preamble = format!("{}{}", past_context, if context.is_empty() {
            String::new()
        } else {
            format!("{}\n\n", context)
        });

        let _ = transcript_tx
            .send(ChatEvent::Status("Context gathered. Starting conversation...".into()))
            .await;

        // ---------------------------------------------------------------
        // Phase 1: Introductions
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("introductions".into()))
            .await;
        let _ = transcript_tx
            .send(ChatEvent::Status("[1/6] Introductions \u{2014} agents sharing project and setup info...".into()))
            .await;

        // First message: include preamble + phase 1 prompt.
        conversation_history.push_str(&preamble);
        conversation_history.push_str(PHASE1_INTRO_PROMPT);

        let raw_intro = session.query(&conversation_history).await?;
        let intro = self.sanitize_and_clean(&raw_intro, &transcript_tx).await;
        if intro.is_empty() {
            bail!("agent produced empty intro after sanitization");
        }
        conversation_history.push_str(&format!("\n\nYOU: {}", intro));

        turn += 1;
        transcript.push(self.local_message(&intro, MessagePhase::Opening, turn));
        let _ = transcript_tx.send(ChatEvent::LocalMessage(intro.clone())).await;
        send_tx.send(intro).await.context("failed to send intro")?;

        // Wait for peer's intro.
        let peer_intro = wait_for_peer(&mut recv_rx).await?;
        turn += 1;
        transcript.push(peer_message(&peer_intro, MessagePhase::Opening, turn));
        let _ = transcript_tx.send(ChatEvent::RemoteMessage(peer_intro.clone())).await;
        conversation_history.push_str(&format!("\n\n{}{}", FOLLOWUP_PROMPT_PREFIX, peer_intro));

        // ---------------------------------------------------------------
        // Phases 2-4 + Wrapup: wrapped so mid-chat failures still
        // produce a briefing from the partial transcript.
        // ---------------------------------------------------------------
        let conversation_result: Result<()> = async {
            // -----------------------------------------------------------
            // Phases 2-4: Guided discussion rounds
            // -----------------------------------------------------------
            let guided_phases: &[(&str, &str, &str, &str)] = &[
                ("deep_dive",  "Deep Dive",            PHASE2_DEEPDIVE_PROMPT,
                 "[2/6] Deep Dive \u{2014} agents discussing architecture, failures, and trade-offs..."),
                ("compare",    "Compare & Collaborate", PHASE3_COMPARE_PROMPT,
                 "[3/6] Compare & Collaborate \u{2014} agents comparing setups and finding overlaps..."),
                ("blindspots", "Blindspots & Tips",     PHASE4_BLINDSPOTS_PROMPT,
                 "[4/6] Blindspots & Tips \u{2014} agents sharing observations and recommendations..."),
            ];

            for &(phase_id, phase_label, phase_prompt, status_msg) in guided_phases {
                let _ = transcript_tx
                    .send(ChatEvent::Phase(phase_id.into()))
                    .await;
                let _ = transcript_tx
                    .send(ChatEvent::Status(status_msg.into()))
                    .await;

                // Inject the phase prompt into conversation history.
                conversation_history.push_str(&format!("\n\n{}", phase_prompt));

                let mut phase_msgs: usize = 0;

                loop {
                    if phase_msgs >= MAX_FOLLOWUPS_PER_PHASE {
                        break;
                    }

                    // Our agent responds with a dynamic follow-up based on what the peer said.
                    let followup_instruction = if phase_msgs == 0 {
                        // First message in phase: respond to the phase prompt with our info.
                        format!(
                            "\n\nRespond to the current phase topic. Share your own experience and ask the other agent a specific follow-up question based on their earlier messages. Stay on the current topic: {}.",
                            phase_label
                        )
                    } else {
                        // Subsequent messages: dig deeper based on what the peer just said.
                        format!(
                            "\n\nBased on what the peer just shared, ask a specific follow-up question that digs deeper. Reference something concrete they mentioned. Also share any related experience of your own. Stay on the current topic: {}.",
                            phase_label
                        )
                    };
                    conversation_history.push_str(&followup_instruction);
                    let raw = match session.query(&conversation_history).await {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    if raw.is_empty() {
                        break;
                    }

                    let local_done = raw.contains(DONE_SENTINEL);
                    let cleaned = self.sanitize_and_clean(&raw, &transcript_tx).await;
                    let response = if cleaned.is_empty() {
                        "I don't have more to add on this topic.".to_string()
                    } else {
                        cleaned
                    };
                    conversation_history.push_str(&format!("\n\nYOU: {}", response));

                    turn += 1;
                    transcript.push(self.local_message(&response, MessagePhase::Exchange, turn));
                    let _ = transcript_tx.send(ChatEvent::LocalMessage(response.clone())).await;
                    send_tx.send(response).await.context("failed to send message")?;
                    phase_msgs += 1;

                    // Always consume one peer reply before advancing phases. If we
                    // break immediately after our own [DONE], the peer's in-flight
                    // reply stays queued and gets misattributed to the next phase.
                    let peer_msg = match tokio::time::timeout(
                        std::time::Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
                        recv_rx.recv(),
                    ).await {
                        Ok(Some(msg)) => msg,
                        Ok(None) => {
                            bail!("peer disconnected");
                        }
                        Err(_) => {
                            bail!("timed out waiting for peer");
                        }
                    };

                    turn += 1;
                    transcript.push(peer_message(&peer_msg, MessagePhase::Exchange, turn));
                    let _ = transcript_tx.send(ChatEvent::RemoteMessage(peer_msg.clone())).await;
                    conversation_history.push_str(&format!(
                        "\n\n{}{}", FOLLOWUP_PROMPT_PREFIX, peer_msg
                    ));

                    let peer_done = peer_msg.contains(DONE_SENTINEL);
                    if should_advance_phase_after_peer_turn(local_done, peer_done) {
                        break;
                    }
                }
            }

            // -----------------------------------------------------------
            // Phase 5: Wrapup
            // -----------------------------------------------------------
            let _ = transcript_tx
                .send(ChatEvent::Phase("wrapup".into()))
                .await;
            let _ = transcript_tx
                .send(ChatEvent::Status("[5/6] Wrap-up \u{2014} agents exchanging final takeaways...".into()))
                .await;

            conversation_history.push_str(&format!("\n\n{}", WRAPUP_PROMPT));
            let raw_wrapup = session.query(&conversation_history).await
                .unwrap_or_else(|_| "Thanks for the chat! It was great connecting.".to_string());
            let cleaned_wrapup = self.sanitize_and_clean(&raw_wrapup, &transcript_tx).await;
            let wrapup = if cleaned_wrapup.is_empty() {
                "I don't have more to add on this topic.".to_string()
            } else {
                cleaned_wrapup
            };
            conversation_history.push_str(&format!("\n\nYOU: {}", wrapup));

            turn += 1;
            transcript.push(self.local_message(&wrapup, MessagePhase::Closing, turn));
            let _ = transcript_tx.send(ChatEvent::LocalMessage(wrapup.clone())).await;
            send_tx.send(wrapup).await.context("failed to send wrapup")?;

            // Wait for peer's wrapup (best-effort).
            if let Ok(Some(peer_wrapup)) = tokio::time::timeout(
                std::time::Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
                recv_rx.recv(),
            ).await {
                turn += 1;
                transcript.push(peer_message(&peer_wrapup, MessagePhase::Closing, turn));
                let _ = transcript_tx.send(ChatEvent::RemoteMessage(peer_wrapup)).await;
            }

            Ok(())
        }.await;

        if let Err(ref e) = conversation_result {
            eprintln!("[chat_engine] Mid-chat failure: {:#}", e);
            let _ = transcript_tx
                .send(ChatEvent::Status(
                    "Peer disconnected. Generating briefing from partial transcript...".into(),
                ))
                .await;
        }

        // ---------------------------------------------------------------
        // Phase 6: Briefing generation (legacy + human + agent)
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("briefing".into()))
            .await;
        let _ = transcript_tx
            .send(ChatEvent::Status("[6/6] Synthesizing \u{2014} generating your briefing documents...".into()))
            .await;

        // Generate human briefing first, then agent memo, then construct
        // legacy briefing from those (saves one LLM call).
        let _ = transcript_tx
            .send(ChatEvent::Status("Generating your pre-meeting briefing...".into()))
            .await;
        let human_briefing = self
            .generate_human_briefing(&mut session, &transcript, &context)
            .await
            .unwrap_or_else(|e| {
                eprintln!("[chat_engine] Human briefing generation failed: {:#}", e);
                HumanBriefing::default()
            });

        let _ = transcript_tx
            .send(ChatEvent::Status("Generating agent improvement memo...".into()))
            .await;
        let agent_memo = self
            .generate_agent_memo(&mut session, &transcript, &context)
            .await
            .unwrap_or_else(|e| {
                eprintln!("[chat_engine] Agent memo generation failed: {:#}", e);
                AgentMemo::default()
            });

        // Construct legacy briefing from the richer human briefing + agent memo
        // instead of spawning a separate LLM call.
        let briefing = ChatBriefing {
            what_building: human_briefing.current_focus.clone(),
            learnings: human_briefing
                .conversation_starters
                .iter()
                .take(3)
                .cloned()
                .collect(),
            tips: agent_memo.workflow_improvements.clone(),
            ideas_to_explore: agent_memo
                .follow_up_actions
                .iter()
                .take(3)
                .cloned()
                .collect(),
        };

        let output = CoffeeChatOutput {
            human_briefing,
            agent_memo,
            legacy_briefing: briefing.clone(),
        };

        let briefing_text = format_human_briefing(&output.human_briefing);
        let _ = transcript_tx
            .send(ChatEvent::Briefing(briefing_text))
            .await;

        // ---------------------------------------------------------------
        // Cleanup
        // ---------------------------------------------------------------
        session.kill().await;

        let duration_secs = start.elapsed().as_secs();
        let message_count = transcript.len();
        let completed = conversation_result.is_ok();
        // Phase 1 (intro) always completes if we reach here. Count guided phases + wrapup.
        let phases_completed = if completed { 5 } else {
            // Intro completed (1) + however many guided phases ran before failure.
            // Approximate from transcript: count distinct Exchange-phase messages / 2 for rough phase count.
            let exchange_msgs = transcript.iter().filter(|m| m.phase == MessagePhase::Exchange).count();
            let approx_phases = (exchange_msgs / 4).min(3); // ~4 msgs per guided phase, max 3 guided
            1 + approx_phases as u32
        };

        let _ = transcript_tx.send(ChatEvent::Complete).await;

        Ok(ChatResult {
            transcript,
            briefing,
            output,
            duration_secs,
            message_count,
            completed,
            phases_completed,
        })
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Sanitize and strip the [DONE] sentinel from a raw agent response.
    async fn sanitize_and_clean(
        &self,
        raw: &str,
        transcript_tx: &mpsc::Sender<ChatEvent>,
    ) -> String {
        let cleaned = raw.replace(DONE_SENTINEL, "").trim().to_string();
        let truncated = truncate_to_words(&cleaned, self.config.max_message_words);
        let result = self.sanitizer.run(&truncated);

        if result.redaction_count > 0 {
            let _ = transcript_tx
                .send(ChatEvent::Status(format!(
                    "Sanitizer redacted {} item(s) from outgoing message.",
                    result.redaction_count
                )))
                .await;
        }

        if result.blocked {
            let reason = result.block_reason.unwrap_or_default();
            let _ = transcript_tx
                .send(ChatEvent::Error(format!(
                    "Message blocked by sanitizer: {}. Sending safe fallback.",
                    reason
                )))
                .await;
            return "I had something to share but it contained sensitive information that was caught by my safety filter. Could you tell me more about what you're working on instead?".to_string();
        }

        result.text
    }

    /// Sanitize a single string field through the pipeline.
    /// Returns "[redacted]" if the field is blocked.
    fn sanitize_field(&self, field: &str) -> String {
        let result = self.sanitizer.run(field);
        if result.blocked {
            "[redacted]".to_string()
        } else {
            result.text
        }
    }

    /// Sanitize a Vec<String> field — each element through the pipeline.
    fn sanitize_vec_field(&self, fields: &[String]) -> Vec<String> {
        fields.iter().map(|f| self.sanitize_field(f)).collect()
    }

    /// Run the sanitization pipeline over every text field of a HumanBriefing.
    fn sanitize_human_briefing_fields(&self, briefing: &mut HumanBriefing) {
        briefing.project_arc = self.sanitize_field(&briefing.project_arc);
        briefing.current_focus = self.sanitize_field(&briefing.current_focus);
        briefing.setup_comparison = self.sanitize_field(&briefing.setup_comparison);
        briefing.overlaps = self.sanitize_field(&briefing.overlaps);
        briefing.candid_takes = self.sanitize_field(&briefing.candid_takes);
        briefing.conversation_starters = self.sanitize_vec_field(&briefing.conversation_starters);
    }

    /// Run the sanitization pipeline over every text field of an AgentMemo.
    fn sanitize_agent_memo_fields(&self, memo: &mut AgentMemo) {
        memo.setup_diffs.they_have = self.sanitize_vec_field(&memo.setup_diffs.they_have);
        memo.setup_diffs.we_have = self.sanitize_vec_field(&memo.setup_diffs.we_have);
        memo.setup_diffs.suggested_additions = self.sanitize_vec_field(&memo.setup_diffs.suggested_additions);
        memo.workflow_improvements = self.sanitize_vec_field(&memo.workflow_improvements);
        memo.debottleneck_ideas = self.sanitize_vec_field(&memo.debottleneck_ideas);
        memo.blindspots_surfaced = self.sanitize_vec_field(&memo.blindspots_surfaced);
        memo.agentic_tips.human_workflows = self.sanitize_vec_field(&memo.agentic_tips.human_workflows);
        memo.agentic_tips.agent_techniques = self.sanitize_vec_field(&memo.agentic_tips.agent_techniques);
        memo.follow_up_actions = self.sanitize_vec_field(&memo.follow_up_actions);
    }

    /// Create a local message for the transcript.
    fn local_message(&self, body: &str, phase: MessagePhase, turn: u32) -> Message {
        Message::new(
            MessageType::Chat,
            phase,
            MessageSender::new(&self.config.display_name, "", &self.config.ai_tool),
            body,
            turn,
        )
    }

    /// Ask the agent to produce a structured briefing from the transcript.
    /// Kept for backward compatibility; no longer called in run_chat().
    #[allow(dead_code)]
    async fn generate_briefing(
        &self,
        session: &mut AgentSession,
        transcript: &[Message],
    ) -> Result<ChatBriefing> {
        // Format the transcript for the agent.
        let mut transcript_text = String::from("=== FULL TRANSCRIPT ===\n");
        for msg in transcript {
            let speaker = if msg.from.name == PEER_SENDER_NAME {
                "PEER"
            } else {
                "YOU"
            };
            transcript_text.push_str(&format!("[{}] {}\n\n", speaker, msg.body));
        }
        transcript_text.push_str("=== END TRANSCRIPT ===\n\n");
        transcript_text.push_str(BRIEFING_PROMPT);

        let raw_briefing = session.query(&transcript_text).await
            .unwrap_or_else(|_| "{}".to_string());

        // Try to parse the JSON. The agent might include some preamble text,
        // so we search for the first '{' and last '}'.
        let json_str = extract_json_object(&raw_briefing)
            .unwrap_or_else(|| raw_briefing.clone());

        let briefing: ChatBriefing = serde_json::from_str(&json_str)
            .unwrap_or_else(|_| {
                // Fallback: create a simple briefing from the raw text.
                ChatBriefing {
                    what_building: raw_briefing
                        .lines()
                        .next()
                        .unwrap_or("Unknown")
                        .to_string(),
                    learnings: vec![],
                    tips: vec![],
                    ideas_to_explore: vec![],
                }
            });

        Ok(briefing)
    }

    /// Ask the agent to produce a human-facing briefing from the transcript.
    async fn generate_human_briefing(
        &self,
        session: &mut AgentSession,
        transcript: &[Message],
        local_context: &str,
    ) -> Result<HumanBriefing> {
        let mut prompt = String::new();
        // Include local context so the agent can compare setups.
        if !local_context.is_empty() {
            prompt.push_str(local_context);
            prompt.push_str("\n\n");
        }
        prompt.push_str("=== FULL TRANSCRIPT ===\n");
        for msg in transcript {
            let speaker = if msg.from.name == PEER_SENDER_NAME { "PEER" } else { "YOU" };
            prompt.push_str(&format!("[{}] {}\n\n", speaker, msg.body));
        }
        prompt.push_str("=== END TRANSCRIPT ===\n\n");
        prompt.push_str(HUMAN_BRIEFING_PROMPT);

        let raw = session
            .query(&prompt)
            .await
            .unwrap_or_else(|_| "{}".to_string());

        let json_str = extract_json_object(&raw).unwrap_or_else(|| raw.clone());

        let mut briefing: HumanBriefing = serde_json::from_str(&json_str).unwrap_or_else(|_| {
            HumanBriefing {
                project_arc: raw.lines().next().unwrap_or("Unknown").to_string(),
                ..Default::default()
            }
        });

        self.sanitize_human_briefing_fields(&mut briefing);

        Ok(briefing)
    }

    /// Ask the agent to produce a structured agent memo from the transcript.
    async fn generate_agent_memo(
        &self,
        session: &mut AgentSession,
        transcript: &[Message],
        local_context: &str,
    ) -> Result<AgentMemo> {
        let mut prompt = String::new();
        // Include local context so the agent can identify setup diffs and blindspots.
        if !local_context.is_empty() {
            prompt.push_str(local_context);
            prompt.push_str("\n\n");
        }
        prompt.push_str("=== FULL TRANSCRIPT ===\n");
        for msg in transcript {
            let speaker = if msg.from.name == PEER_SENDER_NAME { "PEER" } else { "YOU" };
            prompt.push_str(&format!("[{}] {}\n\n", speaker, msg.body));
        }
        prompt.push_str("=== END TRANSCRIPT ===\n\n");
        prompt.push_str(AGENT_MEMO_PROMPT);

        let raw = session
            .query(&prompt)
            .await
            .unwrap_or_else(|_| "{}".to_string());

        let json_str = extract_json_object(&raw).unwrap_or_else(|| raw.clone());

        let mut memo: AgentMemo = serde_json::from_str(&json_str).unwrap_or_default();

        self.sanitize_agent_memo_fields(&mut memo);

        Ok(memo)
    }
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

/// Create a peer message for the transcript.
fn peer_message(body: &str, phase: MessagePhase, turn: u32) -> Message {
    Message::new(
        MessageType::Chat,
        phase,
        MessageSender::new(PEER_SENDER_NAME, "", "unknown"),
        body,
        turn,
    )
}

/// After we send a phase message, the phase only advances once we've consumed
/// the peer's reply for that turn.
fn should_advance_phase_after_peer_turn(local_done: bool, peer_done: bool) -> bool {
    local_done || peer_done
}

/// Wait for a message from the peer with a timeout.
async fn wait_for_peer(recv_rx: &mut mpsc::Receiver<String>) -> Result<String> {
    match tokio::time::timeout(
        std::time::Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
        recv_rx.recv(),
    )
    .await
    {
        Ok(Some(msg)) => Ok(msg),
        Ok(None) => bail!("peer channel closed"),
        Err(_) => bail!("timed out waiting for peer message"),
    }
}

/// Truncate text to at most `max_words` words, preserving word boundaries.
fn truncate_to_words(text: &str, max_words: usize) -> String {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.len() <= max_words {
        return text.to_string();
    }
    let mut result: String = words[..max_words].join(" ");
    result.push_str("...");
    result
}

/// Try to extract a JSON object from text that may contain surrounding prose.
fn extract_json_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end > start {
        Some(text[start..=end].to_string())
    } else {
        None
    }
}

/// Format a `HumanBriefing` into a human-readable string for terminal display.
fn format_human_briefing(briefing: &HumanBriefing) -> String {
    let mut out = String::new();

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
    }

    out
}

/// Format a `ChatBriefing` into a human-readable string (legacy format).
#[allow(dead_code)]
fn format_briefing(briefing: &ChatBriefing) -> String {
    let mut out = String::new();

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
    }

    out
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_text_unchanged() {
        let text = "Hello world foo bar";
        assert_eq!(truncate_to_words(text, 10), text);
    }

    #[test]
    fn truncate_long_text() {
        let text = "one two three four five six";
        let result = truncate_to_words(text, 3);
        assert_eq!(result, "one two three...");
    }

    #[test]
    fn truncate_exact_boundary() {
        let text = "a b c";
        assert_eq!(truncate_to_words(text, 3), "a b c");
    }

    #[test]
    fn extract_json_from_prose() {
        let text = r#"Here is the briefing: {"what_building": "test", "learnings": []} done."#;
        let json = extract_json_object(text).unwrap();
        assert!(json.starts_with('{'));
        assert!(json.ends_with('}'));
        assert!(json.contains("what_building"));
    }

    #[test]
    fn extract_json_no_braces() {
        assert!(extract_json_object("no json here").is_none());
    }

    #[test]
    fn format_briefing_output() {
        let briefing = ChatBriefing {
            what_building: "A Rust CLI tool".into(),
            learnings: vec!["Rust is fast".into()],
            tips: vec!["Use cargo watch".into()],
            ideas_to_explore: vec!["Try WASM".into()],
        };
        let text = format_briefing(&briefing);
        assert!(text.contains("A Rust CLI tool"));
        assert!(text.contains("Rust is fast"));
        assert!(text.contains("Use cargo watch"));
        assert!(text.contains("Try WASM"));
    }

    #[test]
    fn chat_config_defaults() {
        let cfg = ChatConfig::default();
        assert_eq!(cfg.max_message_words, 200);
        assert_eq!(cfg.ai_tool, "claude-code");
    }

    #[test]
    fn done_sentinel_detected() {
        let msg = "That was a great chat! [DONE]";
        assert!(msg.contains(DONE_SENTINEL));
    }

    #[test]
    fn done_sentinel_not_present() {
        let msg = "Tell me more about your project.";
        assert!(!msg.contains(DONE_SENTINEL));
    }

    #[test]
    fn phase_advances_after_peer_turn_if_local_is_done() {
        assert!(should_advance_phase_after_peer_turn(true, false));
    }

    #[test]
    fn phase_advances_after_peer_turn_if_peer_is_done() {
        assert!(should_advance_phase_after_peer_turn(false, true));
    }

    #[test]
    fn phase_continues_after_peer_turn_if_neither_side_is_done() {
        assert!(!should_advance_phase_after_peer_turn(false, false));
    }

    #[test]
    fn system_prompt_has_safety_rules() {
        assert!(SYSTEM_PROMPT.contains("NEVER include credentials"));
        assert!(SYSTEM_PROMPT.contains("200 words"));
        assert!(SYSTEM_PROMPT.contains("PEER: "));
        assert!(SYSTEM_PROMPT.contains("structured phases"));
    }

    #[test]
    fn chat_event_variants() {
        // Ensure all variants can be constructed.
        let events = vec![
            ChatEvent::Phase("icebreaker".into()),
            ChatEvent::LocalMessage("hello".into()),
            ChatEvent::RemoteMessage("hi".into()),
            ChatEvent::Status("starting".into()),
            ChatEvent::Briefing("summary".into()),
            ChatEvent::Error("oops".into()),
            ChatEvent::Complete,
        ];
        assert_eq!(events.len(), 7);
    }
}
