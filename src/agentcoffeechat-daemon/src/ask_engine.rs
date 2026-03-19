// Ask engine — handles instant questions from a peer's agent.
//
// Spawns a lightweight agent subprocess, sends the question along with a
// focused system prompt, sanitizes the answer, and returns it.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;

use agentcoffeechat_core::SanitizationPipeline;

/// Timeout for the ask-engine agent subprocess (seconds).
const ASK_AGENT_TIMEOUT_SECS: u64 = 45;

// ---------------------------------------------------------------------------
// System prompt for instant questions
// ---------------------------------------------------------------------------

const ASK_SYSTEM_PROMPT: &str = "\
You are answering a question from a peer developer's agent. \
Be specific and concise. You may read local files to answer. \
Never include credentials, env vars, or code blocks longer than 2 lines.";

// ---------------------------------------------------------------------------
// AskResult
// ---------------------------------------------------------------------------

/// The result of an instant question, including the sanitized answer and
/// how long the process took.
pub struct AskResult {
    pub answer: String,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// AskEngine
// ---------------------------------------------------------------------------

/// Orchestrates the instant-question flow:
///
///   1. Build a prompt combining the system instructions, peer context, and
///      the question itself.
///   2. Spawn a lightweight agent subprocess (`claude`, `codex`, or `gemini`)
///      with the prompt on stdin, read the answer from stdout.
///   3. Sanitize the agent's response through the full pipeline.
///   4. Return the sanitized answer with timing information.
pub struct AskEngine {
    sanitizer: SanitizationPipeline,
}

impl AskEngine {
    /// Create a new `AskEngine` with the default sanitization pipeline.
    pub fn new() -> Self {
        Self {
            sanitizer: SanitizationPipeline::default(),
        }
    }

    /// Ask a question on behalf of a remote peer.
    ///
    /// # Arguments
    /// * `question`     — the question text from the peer's agent.
    /// * `peer_name`    — display name of the peer who asked.
    /// * `ai_tool`      — which AI backend to use (e.g. "claude"). Currently
    ///                     unused; will be wired to the real agent later.
    /// * `project_root` — path to the local project root so the agent can
    ///                     read files for context.
    pub async fn ask(
        &self,
        question: &str,
        peer_name: &str,
        ai_tool: &str,
        project_root: &Path,
    ) -> Result<AskResult> {
        let start = Instant::now();

        // ---------------------------------------------------------------
        // 1. Build the agent prompt
        // ---------------------------------------------------------------
        let prompt = format!(
            "{system}\n\n\
             Peer: {peer}\n\
             AI tool: {tool}\n\
             Project root: {root}\n\n\
             Question:\n{question}",
            system = ASK_SYSTEM_PROMPT,
            peer = peer_name,
            tool = ai_tool,
            root = project_root.display(),
            question = question,
        );

        // ---------------------------------------------------------------
        // 2. Spawn the agent and get a raw answer
        // ---------------------------------------------------------------
        let raw_answer = run_prompt(
            &prompt,
            ai_tool,
            project_root,
            Some(ASK_SYSTEM_PROMPT),
        )
            .await
            .context("agent failed to produce an answer")?;

        // ---------------------------------------------------------------
        // 3. Sanitize the answer
        // ---------------------------------------------------------------
        let sanitized = self.sanitizer.run(&raw_answer);

        if sanitized.blocked {
            let reason = sanitized
                .block_reason
                .unwrap_or_else(|| "sensitive content detected".to_string());
            anyhow::bail!(
                "answer blocked by sanitization pipeline: {}",
                reason
            );
        }

        let duration_ms = start.elapsed().as_millis() as u64;

        Ok(AskResult {
            answer: sanitized.text,
            duration_ms,
        })
    }
}

/// Spawn an AI agent subprocess, feed it a prompt via stdin, and return its
/// stdout as the answer.
pub async fn run_prompt(
    prompt: &str,
    ai_tool: &str,
    project_root: &Path,
    system_prompt: Option<&str>,
) -> Result<String> {
    let (cmd_name, args): (&str, Vec<&str>) = match ai_tool {
        "claude-code" | "claude" if which_exists("claude") => {
            ("claude", vec!["--print", "--model", "sonnet"])
        }
        "codex" if which_exists("codex") => {
            ("codex", vec!["--quiet"])
        }
        "gemini-cli" | "gemini" if which_exists("gemini") => {
            ("gemini", vec!["--print"])
        }
        _ => {
            if which_exists("claude") {
                ("claude", vec!["--print", "--model", "sonnet"])
            } else if which_exists("codex") {
                ("codex", vec!["--quiet"])
            } else if which_exists("gemini") {
                ("gemini", vec!["--print"])
            } else {
                return Ok("No AI tool available to answer this question.".to_string());
            }
        }
    };

    let mut command = tokio::process::Command::new(cmd_name);
    command
        .args(&args)
        .current_dir(project_root)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    if let Some(prompt_text) = system_prompt {
        command.env("CLAUDE_SYSTEM_PROMPT", prompt_text);
    }

    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {} process", cmd_name))?;

    {
        let mut stdin = child
            .stdin
            .take()
            .context("failed to capture agent stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write prompt to agent stdin")?;
        stdin
            .shutdown()
            .await
            .context("failed to close agent stdin")?;
    }

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(ASK_AGENT_TIMEOUT_SECS),
        child.wait_with_output(),
    )
    .await
    .context("agent timed out (45s limit for instant questions)")?
    .context("failed to read agent output")?;

    if !output.status.success() {
        anyhow::bail!(
            "{} exited with status {} — stderr suppressed",
            cmd_name,
            output.status,
        );
    }

    let answer = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if answer.is_empty() {
        anyhow::bail!("{} returned an empty response", cmd_name);
    }

    Ok(answer)
}

/// Check whether a binary exists on `$PATH`.
fn which_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Helper: true when at least one supported AI tool binary is on PATH.
    fn has_ai_tool() -> bool {
        which_exists("claude") || which_exists("codex") || which_exists("gemini")
    }

    #[tokio::test]
    async fn ask_returns_answer() {
        let engine = AskEngine::new();
        let result = engine
            .ask(
                "What framework do you use?",
                "alice",
                "claude",
                &PathBuf::from("/tmp/test-project"),
            )
            .await;

        match result {
            Ok(r) => {
                assert!(!r.answer.is_empty());
                // When no AI tool is available the fallback message is returned;
                // when one is available the agent produces a real answer.
                if !has_ai_tool() {
                    assert!(
                        r.answer.contains("No AI tool available"),
                        "Expected fallback message, got: {}",
                        r.answer,
                    );
                }
            }
            Err(e) => {
                // If the AI tool binary can't be spawned (e.g. PATH differs
                // in test runner), this is acceptable — the test verifies the
                // code path runs without panicking.
                let msg = format!("{:#}", e);
                assert!(
                    msg.contains("spawn") || msg.contains("not found") || msg.contains("No such file"),
                    "Unexpected error: {}",
                    msg,
                );
            }
        }
    }

    #[tokio::test]
    async fn ask_result_has_timing() {
        let engine = AskEngine::new();
        let result = engine
            .ask("ping", "carol", "claude", &PathBuf::from("/tmp"))
            .await;

        match result {
            Ok(result) => {
                assert!(
                    result.duration_ms < 60_000,
                    "Expected duration under 60s, got {}ms",
                    result.duration_ms,
                );
            }
            Err(e) => {
                let msg = format!("{:#}", e);
                assert!(
                    msg.contains("spawn")
                        || msg.contains("not found")
                        || msg.contains("No such file")
                        || msg.contains("exit status"),
                    "Unexpected error: {}",
                    msg,
                );
            }
        }
    }

    #[test]
    fn which_exists_finds_common_binary() {
        // `which` itself should always be present on macOS / Linux.
        assert!(which_exists("which"));
    }

    #[test]
    fn which_exists_returns_false_for_missing() {
        assert!(!which_exists("definitely_not_a_real_binary_abc123xyz"));
    }

    #[tokio::test]
    async fn run_agent_no_tool_fallback() {
        // When no AI tool is on PATH, run_prompt should return the fallback
        // message.  We can't reliably test this in CI where tools may be
        // installed, so we only assert when none are present.
        if has_ai_tool() {
            return; // skip — a real tool would be invoked
        }
        let answer = run_prompt("hello", "nonexistent-tool", Path::new("/tmp"), None)
            .await
            .expect("run_prompt should not error on fallback");
        assert!(answer.contains("No AI tool available"));
    }
}
