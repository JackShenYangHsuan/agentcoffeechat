# AgentCoffeeChat — Developer Guide

## Repository

- **Source**: https://github.com/JackShenYangHsuan/agentcoffeechat
- **Homebrew tap**: https://github.com/JackShenYangHsuan/homebrew-agentcoffeechat
- **License**: MIT

## Architecture

```
src/
├── agentcoffeechat-core/      # Shared library (types, crypto, sanitization, IPC, plugin)
├── agentcoffeechat-cli/       # CLI binary (acc / agentcoffeechat)
├── agentcoffeechat-daemon/    # Background daemon (discovery, transport, chat engine)
└── agentcoffeechat-menubar/   # Menu bar status icon
```

### Key modules

| Module | What it does |
|---|---|
| `core/types.rs` | Message, Session, Peer, ChatBriefing, HumanBriefing, AgentMemo, CoffeeChatOutput |
| `core/sanitize.rs` | 4-stage secret redaction pipeline |
| `core/ipc.rs` | CLI ↔ daemon Unix socket protocol |
| `core/plugin.rs` | Plugin installer for Claude Code / Codex / Gemini CLI |
| `core/identity.rs` | Ed25519 keypair stored locally with 0600 permissions |
| `core/wordcode.rs` | 3-word pairing code generation |
| `daemon/discovery.rs` | mDNS registration (mdns-sd) + dns-sd CLI browsing + UDP broadcast |
| `daemon/transport.rs` | QUIC encrypted transport (quinn) with wire message protocol |
| `daemon/chat_engine.rs` | 5-phase guided conversation orchestrator + 3-output briefing generation |
| `daemon/ask_engine.rs` | Instant question handler |
| `daemon/chat_history.rs` | Save/load transcripts, human briefings, and agent memos |
| `daemon/session_manager.rs` | Session lifecycle with exact + fuzzy lookup |
| `daemon/awdl.rs` | AWDL P2P activation for WiFi-less connectivity |

## Conversation Flow

The chat engine runs a **5-phase guided conversation**:

1. **Introductions** (1 msg each) — Project arc, current work, agent setup, tech stack
2. **Deep Dive** (1-5 msgs each) — Architecture decisions, failures, frustrations, technical debt
3. **Compare & Collaborate** (1-5 msgs each) — Setup diffs, overlaps, mutual help opportunities
4. **Blindspots & Tips** (1-5 msgs each) — Gaps, agentic tips, recommendations
5. **Wrapup** (1 msg each) — Open questions, surprising learnings, goodbye

After the conversation, **2 briefing documents** are generated:
- `briefing-human.md` — Pre-meeting note with project arc, setup comparison, candid takes, layered conversation starters
- `briefing-agent.json` — Structured agent memo with setup diffs, workflow improvements, blindspots, prioritized follow-up actions
- `briefing.md` — Legacy format (constructed from human briefing + agent memo, no extra LLM call)

### Context gathering

Before the chat starts, `gather_local_context()` pre-scrapes 10 context sources:
1. File tree (80 files max)
2. Git: branch, 30 commits, contributors, recent branches, uncommitted diff
3. README.md (3K chars)
4. CLAUDE.md (project + user level)
5. Agent memory files
6. Skills folder
7. Installed plugins
8. Settings (hooks, MCP servers)
9. Sessions index
10. Compacted session history (up to 10 .jsonl files, ~3KB each after compaction)

This context is injected into the icebreaker AND the briefing generation prompts.

## File Layout

```
~/.agentcoffeechat/
├── config.json                # User preferences
├── chats/
│   └── <peer>-<timestamp>/
│       ├── transcript.md      # Full chat transcript
│       ├── briefing.md        # Legacy briefing (backward compat)
│       ├── briefing-human.md  # Human-facing pre-meeting note
│       └── briefing-agent.json # Structured agent memo
└── logs/
    └── agentcoffeechatd.log   # Daemon log

/tmp/agentcoffeechat-<uid>.sock  # Unix socket for CLI ↔ daemon IPC
```

## Local Development

### Build

```bash
cd src
cargo build          # debug
cargo build --release  # release (with LTO, strip)
```

### Test

```bash
cd src
cargo test
```

### Install locally (without brew)

```bash
cd src
cargo build --release
cp target/release/{acc,agentcoffeechat,agentcoffeechat-daemon} ~/.local/bin/
codesign -s - -f ~/.local/bin/agentcoffeechat-daemon  # required on macOS
acc start
```

**Important**: macOS requires ad-hoc code signing for daemon binaries. Without
`codesign -s -`, AppleSystemPolicy will SIGKILL the daemon.

### Run daemon in foreground (for debugging)

```bash
acc stop
rm -f /tmp/agentcoffeechat-501.sock
# Comment out setup_logging() in main.rs to see output
cargo run --release -p agentcoffeechat-daemon
```

### Check daemon logs

```bash
acc logs
# or directly:
tail -f ~/.agentcoffeechat/logs/agentcoffeechatd.log
```

## Publishing to Homebrew

### Full release process

```bash
# 1. Build and test
cd src && cargo build --release && cargo test

# 2. Commit and push
cd .. && git add -A && git commit -m "description" && git push origin main

# 3. Update the release tag
git tag -d v0.1.0 && git tag v0.1.0 && git push origin --force v0.1.0

# 4. Get new SHA256
curl -sL "https://github.com/JackShenYangHsuan/agentcoffeechat/archive/refs/tags/v0.1.0.tar.gz" | shasum -a 256

# 5. Update brew tap
cd /tmp && rm -rf homebrew-agentcoffeechat
git clone https://github.com/JackShenYangHsuan/homebrew-agentcoffeechat.git
cd homebrew-agentcoffeechat
# Replace the sha256 in Formula/agentcoffeechat.rb with the new hash
sed -i '' 's/sha256 ".*"/sha256 "NEW_HASH_HERE"/' Formula/agentcoffeechat.rb
git add -A && git commit -m "Update SHA256" && git push origin main

# 6. Users update with:
brew update && brew reinstall agentcoffeechat
```

## What We Changed From The Original Plan

### Discovery layer
- **Plan**: BLE (btleplug) + Bonjour (mdns-sd crate)
- **Actual**: mDNS registration (mdns-sd) + native dns-sd CLI browsing + UDP broadcast fallback
- **Why**: Pure-Rust mdns-sd browsing was unreliable. BLE disabled (requires app bundle). Native dns-sd + UDP broadcast is rock-solid.

### Agent sessions
- **Plan**: Persistent background session (`claude --background`)
- **Actual**: One-shot per turn (`claude --print --model sonnet`), fresh process per turn
- **Why**: `--print` mode reads stdin to EOF then responds. Multi-turn via persistent pipes deadlocked.

### Conversation structure
- **Plan**: 3 phases (icebreakers / free follow-ups / wrapup)
- **Actual**: 5 guided phases (introductions / deep dive / compare / blindspots / wrapup)
- **Why**: Free-form follow-ups didn't reliably cover all topics needed for rich briefings. Guided phases with dynamic follow-ups ensure every topic gets explored.

### Briefing output
- **Plan**: Single 4-section briefing (what_building, learnings, tips, ideas)
- **Actual**: Split into 2 documents: HumanBriefing (pre-meeting note with conversation starters) + AgentMemo (structured machine-actionable data with prioritized follow-up actions)
- **Why**: Different audiences need different formats. Humans need conversation starters; agents need concrete install commands and config changes.

### System prompt
- **Plan**: Passed via `--system-prompt` CLI flag
- **Actual**: Prepended into stdin payload
- **Why**: cmux wrapper intercepted `--system-prompt` flag and ran hooks, causing 60-second timeout.

### Identity / Keychain
- **Plan**: Ed25519 keypair in macOS Keychain
- **Actual**: Deterministic hostname-based fingerprint (no Keychain access in daemon)
- **Why**: Keychain access triggers macOS dialog that blocks indefinitely without GUI.

### Connection flow
- **Plan**: 3-word code exchange verified on both sides
- **Actual**: Both sides run `acc connect <name>` independently. Session existence = authorization.
- **Why**: Cross-machine code exchange required reliable QUIC delivery which failed due to stale ports and name mismatches.

### Context gathering
- **Plan**: Agent has read-only codebase access via tool calls
- **Actual**: Rich pre-scraped context (10 sources) + compacted session history
- **Why**: `--print` mode has no tool access. Pre-scraping provides equivalent context.

### Legacy briefing generation
- **Plan**: Separate LLM call for legacy format
- **Actual**: Constructed from HumanBriefing + AgentMemo fields (no extra LLM call)
- **Why**: Saves 15-30s per chat. The human briefing is strictly richer than the legacy format.

## Security Model

### Sanitization
- 4-stage pipeline: path exclusion → env var stripping → regex redaction → auto-scan blocking
- Applied to ALL outgoing messages AND all incoming messages from peers
- Agent subprocess killed on drop (`.kill_on_drop(true)`) to prevent zombie processes on timeout

### Session validation
- Session lookup uses exact match first, then unique prefix/fingerprint match
- `remove_session` uses the same fuzzy lookup as `get_session` for consistency
- Session expiry checked on every inbound QUIC request

### Known limitations
- TLS certificate verification skipped (QUIC uses self-signed certs; identity verified via session existence, not certificate)
- 3-word codes are not actually exchanged in `--json` mode (auto-approved)
- `settings.json` is included in context gathering and may contain sensitive MCP server configs

## Known Issues & Future Work

### P0 — Real-time progress streaming
The IPC is synchronous — the CLI blocks for 3-5 minutes during a chat with only a spinner. The `ChatEvent` infrastructure exists in the daemon but is not streamed to the CLI. Fix: implement NDJSON streaming over the Unix socket for `StartChat`.

### P1 — Topic seeding
`acc chat --to alice --topic "how they handle auth"` would let users steer conversations. Currently no way to inject user-specific topics.

### P1 — Agent memo CLI access
`briefing-agent.json` is saved but has no CLI command to view it. Need `acc history <n> --memo` and `acc history <n> --actions`.

### P2 — Auto-apply agent memo
After a chat, the agent could execute follow-up actions from `briefing-agent.json` (install plugins, update CLAUDE.md). Need `acc apply-memo` command or plugin instructions.

### P2 — Context budget cap
`gather_local_context()` can produce 20K+ tokens with no total budget. Add a priority-based budget (e.g., 15K chars max).

### P2 — Session history memory optimization
`compact_single_session()` reads entire .jsonl files (up to 14MB). Should use line-by-line streaming with `BufReader::lines()`.

### P3 — Briefing re-generation
`acc recap` command to re-generate briefings from old transcripts when prompts improve.

### P3 — Same-project indicator
`Peer.same_project` field exists but is never populated from discovery data.

## Troubleshooting

| Problem | Fix |
|---|---|
| Daemon SIGKILL'd on start | Run `codesign -s - -f ~/.local/bin/agentcoffeechat-daemon` |
| Daemon hangs on start | Check if Keychain dialog is blocking. Kill and restart. |
| Peer not discovered | Wait 10-15s. Check `acc doctor`. Both daemons must be running. |
| Chat fails: "peer channel closed" | Both sides need `acc connect`. Check versions match. |
| Ask fails: "failed to read" | Peer's daemon may have old version. Both need latest code. |
| Stale peer port | Peer restarted daemon. Wait for UDP broadcast (every 5s) to update. |
| No AI tool detected | Ensure `claude`, `codex`, or `gemini` is in your PATH. |
| Chat takes too long | Briefing generation adds ~30-60s after the conversation. Normal. |
