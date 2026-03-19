// Chat engine — orchestrates multi-turn conversations between agents.
//
// Each agent "turn" spawns a fresh `claude --print` (or codex/gemini)
// subprocess with the full conversation history on stdin.  `--print` is
// one-shot: it reads stdin to EOF, produces a single response on stdout,
// and exits.  This avoids the deadlock that would occur if we tried to
// keep a persistent process and do multi-turn I/O on its pipes.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::mpsc;

use agentcoffeechat_core::types::{ChatBriefing, Message, MessagePhase, MessageSender, MessageType};
use agentcoffeechat_core::SanitizationPipeline;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default word limit per message.
const DEFAULT_MAX_MESSAGE_WORDS: usize = 200;

/// Default maximum messages each side may send before the chat wraps up.
const DEFAULT_MAX_MESSAGES_PER_SIDE: usize = 30;

/// Timeout (seconds) waiting for a single agent response.
const AGENT_RESPONSE_TIMEOUT_SECS: u64 = 60;

/// Timeout (seconds) waiting for a peer message over QUIC.
const PEER_MESSAGE_TIMEOUT_SECS: u64 = 180;

/// Sentinel the agent may include in its message to signal it wants to wrap up.
const DONE_SENTINEL: &str = "[DONE]";

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = r#"You are participating in an AgentCoffeeChat — a casual, curiosity-driven conversation between two AI coding agents. Your partner is another AI agent working on a different (or possibly the same) project nearby.

Guidelines:
- Be curious like a friend at a coffee shop. Ask genuine follow-up questions.
- Keep every message under 200 words. Be concise but warm.
- Share what you're working on, what tools you use, interesting design decisions, and things you're stuck on.
- Learn from the other agent: ask about their setup, prompting strategies, project architecture, and lessons learned.
- When you feel the conversation has reached a natural stopping point, include [DONE] at the very end of your message.

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
// Icebreaker prompt
// ---------------------------------------------------------------------------

const ICEBREAKER_PROMPT: &str = r#"This is the start of your coffee chat! Introduce yourself and answer these questions about your current project and agent setup:
- What tools/MCP servers are you using?
- What prompting strategies work well for you?
- What are you working on and why?
- What design decisions have you made?
- What are you stuck on?
- Why might any of this be interesting to the other person?

Keep it under 200 words. Be friendly and genuine."#;

// ---------------------------------------------------------------------------
// Follow-up prompt
// ---------------------------------------------------------------------------

const FOLLOWUP_PROMPT_PREFIX: &str = "PEER: ";

// ---------------------------------------------------------------------------
// Wrap-up prompt
// ---------------------------------------------------------------------------

const WRAPUP_PROMPT: &str = r#"It's time to wrap up the coffee chat. Send a brief closing message:
- Summarize what you found most interesting or useful from the conversation.
- Mention anything you plan to try or explore based on what you learned.
- Say goodbye warmly.

Keep it under 200 words."#;

// ---------------------------------------------------------------------------
// Briefing prompt
// ---------------------------------------------------------------------------

const BRIEFING_PROMPT: &str = r#"The coffee chat is over. Based on the full transcript above, produce a briefing in exactly this JSON format (no markdown fences, just raw JSON):
{
  "what_building": "A one-sentence summary of what the other agent is building",
  "learnings": ["Key insight 1", "Key insight 2", "..."],
  "tips": ["Actionable tip 1", "Actionable tip 2", "..."],
  "ideas_to_explore": ["Idea 1", "Idea 2", "..."]
}

Be specific and concise. Focus on genuinely useful information."#;

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
    /// Maximum messages each side may send (default 30).
    pub max_messages_per_side: usize,
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
            max_messages_per_side: DEFAULT_MAX_MESSAGES_PER_SIDE,
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
    /// Structured briefing produced by the agent.
    pub briefing: ChatBriefing,
    /// Wall-clock duration of the chat in seconds.
    pub duration_secs: u64,
    /// Total number of messages exchanged (both sides).
    pub message_count: usize,
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
            .stderr(std::process::Stdio::null());

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
            let suffix = if tree.lines().count() > 80 {
                format!("\n  ... and {} more files", tree.lines().count() - 80)
            } else {
                String::new()
            };
            sections.push(format!("Project files:\n{}{}", limited, suffix));
        }
    }

    // -----------------------------------------------------------------
    // 2. Git branch + recent commits
    // -----------------------------------------------------------------
    if let Some(branch) = run_git_command(project_root, &["branch", "--show-current"]).await {
        let branch = branch.trim().to_string();
        if !branch.is_empty() {
            sections.push(format!("Git branch: {}", branch));
        }
    }

    if let Some(log) = run_git_command(project_root, &["log", "--oneline", "-10"]).await {
        let log = log.trim().to_string();
        if !log.is_empty() {
            sections.push(format!("Recent commits:\n{}", log));
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

/// Read the first `max_chars` characters of a file. Returns `None` on any
/// I/O error (including the file not existing).
async fn read_file_prefix(path: &Path, max_chars: usize) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    if content.len() <= max_chars {
        Some(content)
    } else {
        // Truncate at a char boundary.
        let truncated: String = content.chars().take(max_chars).collect();
        Some(truncated)
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

    /// Run a complete coffee chat.
    ///
    /// - `send_tx`: channel for outgoing messages to the peer (via QUIC).
    /// - `recv_rx`: channel for incoming messages from the peer.
    /// - `transcript_tx`: event stream for the CLI to display progress.
    ///
    /// Returns a `ChatResult` with the full transcript and briefing.
    pub async fn run_chat(
        &self,
        send_tx: mpsc::Sender<String>,
        mut recv_rx: mpsc::Receiver<String>,
        transcript_tx: mpsc::Sender<ChatEvent>,
    ) -> Result<ChatResult> {
        let start = Instant::now();
        let mut transcript: Vec<Message> = Vec::new();
        let mut turn: u32 = 0;
        let mut local_message_count: usize = 0;

        // --- Create agent session (one-shot-per-turn) ---
        let mut session = AgentSession::new(&self.config, SYSTEM_PROMPT);
        // Accumulated conversation history fed to the agent each turn.
        let mut conversation_history = String::new();

        let _ = transcript_tx
            .send(ChatEvent::Status(format!(
                "Agent session ready ({})",
                self.config.ai_tool
            )))
            .await;

        // ---------------------------------------------------------------
        // Phase 1: Icebreaker
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("icebreaker".into()))
            .await;

        // Load past briefings for this peer (if known) and build the
        // icebreaker prompt with historical context.
        let icebreaker_prompt = if let Some(ref peer) = self.config.peer_name {
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
                    format!(
                        "Previous conversations with this peer:\n{}\n\n{}",
                        joined, ICEBREAKER_PROMPT
                    )
                }
                _ => ICEBREAKER_PROMPT.to_string(),
            }
        } else {
            ICEBREAKER_PROMPT.to_string()
        };

        // Gather local project context and prepend it to the icebreaker.
        let context = gather_local_context(&self.config.project_root).await;
        let full_icebreaker_prompt = if context.is_empty() {
            icebreaker_prompt
        } else {
            format!("{}\n\n{}", context, icebreaker_prompt)
        };

        // Send icebreaker prompt (with local context) to our agent.
        conversation_history.push_str(&full_icebreaker_prompt);
        let raw_icebreaker = session.query(&conversation_history).await?;
        let icebreaker = self.sanitize_message(&raw_icebreaker, &transcript_tx).await;
        conversation_history.push_str(&format!("\n\nYOU: {}", icebreaker));

        turn += 1;
        let local_sender = MessageSender::new(
            &self.config.display_name,
            "",
            &self.config.ai_tool,
        );
        transcript.push(Message::new(
            MessageType::Chat,
            MessagePhase::Opening,
            local_sender,
            &icebreaker,
            turn,
        ));
        local_message_count += 1;

        let _ = transcript_tx
            .send(ChatEvent::LocalMessage(icebreaker.clone()))
            .await;
        send_tx
            .send(icebreaker)
            .await
            .context("failed to send icebreaker to peer")?;

        // Wait for peer's icebreaker.
        let peer_icebreaker = wait_for_peer(&mut recv_rx).await?;
        turn += 1;
        let peer_sender = MessageSender::new("peer", "", "unknown");
        transcript.push(Message::new(
            MessageType::Chat,
            MessagePhase::Opening,
            peer_sender,
            &peer_icebreaker,
            turn,
        ));
        let _ = transcript_tx
            .send(ChatEvent::RemoteMessage(peer_icebreaker.clone()))
            .await;

        // Record peer's icebreaker in conversation history.
        conversation_history.push_str(&format!(
            "\n\n{}{}", FOLLOWUP_PROMPT_PREFIX, peer_icebreaker
        ));

        // ---------------------------------------------------------------
        // Phase 2: Follow-ups
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("followup".into()))
            .await;

        loop {
            if local_message_count >= self.config.max_messages_per_side {
                let _ = transcript_tx
                    .send(ChatEvent::Status(
                        "Maximum message count reached, moving to wrap-up.".into(),
                    ))
                    .await;
                break;
            }

            // Get agent's follow-up response (one-shot with full history).
            conversation_history.push_str("\n\nRespond with your next message:");
            let raw_response = match session.query(&conversation_history).await {
                Ok(r) => r,
                Err(_) => break, // Agent failed — move to wrap-up.
            };
            if raw_response.is_empty() {
                break;
            }

            let done = raw_response.contains(DONE_SENTINEL);
            let cleaned = raw_response.replace(DONE_SENTINEL, "").trim().to_string();
            let response = self.sanitize_message(&cleaned, &transcript_tx).await;
            conversation_history.push_str(&format!("\n\nYOU: {}", response));

            turn += 1;
            let local_sender = MessageSender::new(
                &self.config.display_name,
                "",
                &self.config.ai_tool,
            );
            transcript.push(Message::new(
                MessageType::Chat,
                MessagePhase::Exchange,
                local_sender,
                &response,
                turn,
            ));
            local_message_count += 1;

            let _ = transcript_tx
                .send(ChatEvent::LocalMessage(response.clone()))
                .await;
            send_tx
                .send(response)
                .await
                .context("failed to send follow-up to peer")?;

            if done {
                let _ = transcript_tx
                    .send(ChatEvent::Status(
                        "Agent signaled conversation complete.".into(),
                    ))
                    .await;
                break;
            }

            // Wait for peer's follow-up.
            let peer_msg = match tokio::time::timeout(
                std::time::Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
                recv_rx.recv(),
            )
            .await
            {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    let _ = transcript_tx
                        .send(ChatEvent::Status("Peer disconnected.".into()))
                        .await;
                    break;
                }
                Err(_) => {
                    let _ = transcript_tx
                        .send(ChatEvent::Status(
                            "Timed out waiting for peer, moving to wrap-up.".into(),
                        ))
                        .await;
                    break;
                }
            };

            turn += 1;
            let peer_sender = MessageSender::new("peer", "", "unknown");
            transcript.push(Message::new(
                MessageType::Chat,
                MessagePhase::Exchange,
                peer_sender,
                &peer_msg,
                turn,
            ));
            let _ = transcript_tx
                .send(ChatEvent::RemoteMessage(peer_msg.clone()))
                .await;

            // Record peer's message in conversation history.
            conversation_history.push_str(&format!(
                "\n\n{}{}", FOLLOWUP_PROMPT_PREFIX, peer_msg
            ));
        }

        // ---------------------------------------------------------------
        // Phase 3: Wrap-up
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("wrapup".into()))
            .await;

        conversation_history.push_str(&format!("\n\n{}", WRAPUP_PROMPT));
        let raw_wrapup = session.query(&conversation_history).await
            .unwrap_or_else(|_| "Thanks for the chat! It was great connecting.".to_string());
        let wrapup = self
            .sanitize_message(&raw_wrapup, &transcript_tx)
            .await;
        conversation_history.push_str(&format!("\n\nYOU: {}", wrapup));

        turn += 1;
        let local_sender = MessageSender::new(
            &self.config.display_name,
            "",
            &self.config.ai_tool,
        );
        transcript.push(Message::new(
            MessageType::Chat,
            MessagePhase::Closing,
            local_sender,
            &wrapup,
            turn,
        ));

        let _ = transcript_tx
            .send(ChatEvent::LocalMessage(wrapup.clone()))
            .await;
        send_tx
            .send(wrapup)
            .await
            .context("failed to send wrap-up to peer")?;

        // Wait for peer's wrap-up (best-effort; don't fail if they disconnect).
        if let Ok(Some(peer_wrapup)) = tokio::time::timeout(
            std::time::Duration::from_secs(PEER_MESSAGE_TIMEOUT_SECS),
            recv_rx.recv(),
        )
        .await
        {
            turn += 1;
            let peer_sender = MessageSender::new("peer", "", "unknown");
            transcript.push(Message::new(
                MessageType::Chat,
                MessagePhase::Closing,
                peer_sender,
                &peer_wrapup,
                turn,
            ));
            let _ = transcript_tx
                .send(ChatEvent::RemoteMessage(peer_wrapup))
                .await;
        }

        // ---------------------------------------------------------------
        // Phase 4: Briefing
        // ---------------------------------------------------------------
        let _ = transcript_tx
            .send(ChatEvent::Phase("briefing".into()))
            .await;

        let briefing = self
            .generate_briefing(&mut session, &transcript)
            .await
            .unwrap_or_else(|e| {
                eprintln!("[chat_engine] Briefing generation failed: {:#}", e);
                ChatBriefing::default()
            });

        let briefing_text = format_briefing(&briefing);
        let _ = transcript_tx
            .send(ChatEvent::Briefing(briefing_text))
            .await;

        // ---------------------------------------------------------------
        // Cleanup
        // ---------------------------------------------------------------
        session.kill().await;

        let duration_secs = start.elapsed().as_secs();
        let message_count = transcript.len();

        let _ = transcript_tx.send(ChatEvent::Complete).await;

        Ok(ChatResult {
            transcript,
            briefing,
            duration_secs,
            message_count,
        })
    }

    // -------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------

    /// Run the message through the sanitization pipeline. If the pipeline
    /// blocks the message entirely, substitute a safe fallback.
    async fn sanitize_message(
        &self,
        raw: &str,
        transcript_tx: &mpsc::Sender<ChatEvent>,
    ) -> String {
        let truncated = truncate_to_words(raw, self.config.max_message_words);
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

    /// Ask the agent to produce a structured briefing from the transcript.
    async fn generate_briefing(
        &self,
        session: &mut AgentSession,
        transcript: &[Message],
    ) -> Result<ChatBriefing> {
        // Format the transcript for the agent.
        let mut transcript_text = String::from("=== FULL TRANSCRIPT ===\n");
        for msg in transcript {
            let speaker = if msg.from.name == "peer" {
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
}

// ---------------------------------------------------------------------------
// Free functions
// ---------------------------------------------------------------------------

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

/// Format a `ChatBriefing` into a human-readable string.
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
        assert_eq!(cfg.max_messages_per_side, 30);
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
    fn system_prompt_has_safety_rules() {
        assert!(SYSTEM_PROMPT.contains("NEVER include credentials"));
        assert!(SYSTEM_PROMPT.contains("200 words"));
        assert!(SYSTEM_PROMPT.contains("PEER: "));
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
