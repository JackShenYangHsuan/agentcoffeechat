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
| `core/types.rs` | Message, Session, Peer, ChatBriefing, Config types |
| `core/sanitize.rs` | 4-stage secret redaction pipeline |
| `core/ipc.rs` | CLI ↔ daemon Unix socket protocol |
| `core/plugin.rs` | Plugin installer for Claude Code / Codex / Gemini CLI |
| `core/identity.rs` | Ed25519 keypair in macOS Keychain |
| `core/wordcode.rs` | 3-word pairing code generation |
| `daemon/discovery.rs` | mDNS registration (mdns-sd) + dns-sd CLI browsing + UDP broadcast |
| `daemon/transport.rs` | QUIC encrypted transport (quinn) with wire message protocol |
| `daemon/chat_engine.rs` | Multi-turn agent conversation orchestrator |
| `daemon/ask_engine.rs` | Instant question handler |
| `daemon/chat_history.rs` | Save/load transcripts and briefings |
| `daemon/session_manager.rs` | Session lifecycle management |
| `daemon/awdl.rs` | AWDL P2P activation for WiFi-less connectivity |
| `daemon/auth.rs` | Session validation for inbound connections |

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

### Quick update (after pushing code)

```bash
# One-liner: push code, update tag, update brew tap
git push origin main && \
git tag -d v0.1.0 && git tag v0.1.0 && git push origin --force v0.1.0 && \
NEW_SHA=$(curl -sL "https://github.com/JackShenYangHsuan/agentcoffeechat/archive/refs/tags/v0.1.0.tar.gz" | shasum -a 256 | awk '{print $1}') && \
cd /tmp && rm -rf homebrew-agentcoffeechat && \
git clone https://github.com/JackShenYangHsuan/homebrew-agentcoffeechat.git && \
cd homebrew-agentcoffeechat && \
sed -i '' "s/sha256 \".*\"/sha256 \"${NEW_SHA}\"/" Formula/agentcoffeechat.rb && \
git add -A && git commit -m "Update SHA256" && git push origin main
```

## What We Changed From The Original Plan

### Discovery layer
- **Plan**: BLE (btleplug) + Bonjour (mdns-sd crate)
- **Actual**: mDNS registration (mdns-sd) + native dns-sd CLI browsing + UDP broadcast fallback
- **Why**: Pure-Rust mdns-sd browsing was unreliable (competed with macOS mDNSResponder). BLE disabled (requires app bundle with Info.plist). zeroconf crate caused SIGKILL by AppleSystemPolicy. Native dns-sd + UDP broadcast is rock-solid.

### Agent sessions
- **Plan**: Persistent background session (`claude --background`) with stdin/stdout multi-turn
- **Actual**: One-shot per turn (`claude --print`), spawning a fresh process for each conversation turn
- **Why**: `--print` mode reads stdin to EOF then responds. Multi-turn via persistent pipes deadlocked. One-shot with accumulated conversation history works reliably.

### System prompt
- **Plan**: Passed via `--system-prompt` CLI flag
- **Actual**: Prepended into stdin payload
- **Why**: cmux wrapper intercepted `--system-prompt` flag and ran hooks (SessionStart, PreToolUse etc.), causing 60-second timeout. Inlining into stdin avoids all wrapper/hook interference.

### Identity / Keychain
- **Plan**: Ed25519 keypair in macOS Keychain for identity
- **Actual**: Deterministic hostname-based fingerprint (no Keychain access in daemon)
- **Why**: Keychain access triggers a macOS dialog that blocks indefinitely when daemon runs without GUI (after dup2 redirects stdout/stderr to log file).

### Connection flow
- **Plan**: 3-word code exchange verified on both sides
- **Actual**: Both sides run `acc connect <name>` independently. Session existence = authorization.
- **Why**: Cross-machine code exchange required reliable QUIC delivery of ConnectionRequest, which failed due to stale ports, name mismatches, and version skew. Local-only sessions are simpler and work.

### Context gathering
- **Plan**: Agent has full read-only codebase access via tool calls
- **Actual**: Rich pre-scraped context injected into prompt (file tree, git history, README, CLAUDE.md, memory files, skills, plugins, settings, session index)
- **Why**: `--print` mode has no tool access. Pre-scraping provides equivalent context without requiring a persistent agent session.

## File Layout

```
~/.agentcoffeechat/
├── config.json            # User preferences (created on first acc start)
├── chats/
│   └── <peer>-<timestamp>/
│       ├── transcript.md  # Full chat transcript
│       └── briefing.md    # 4-section summary
└── logs/
    └── agentcoffeechatd.log  # Daemon log

/tmp/agentcoffeechat-<uid>.sock  # Unix socket for CLI ↔ daemon IPC
```

## Troubleshooting

| Problem | Fix |
|---|---|
| Daemon SIGKILL'd on start | Run `codesign -s - -f ~/.local/bin/agentcoffeechat-daemon` |
| Daemon hangs on start | Check if Keychain dialog is blocking. Kill and restart. |
| Peer not discovered | Wait 10-15s. Check `acc doctor`. Both daemons must be running. |
| Chat fails: "peer channel closed" | Both sides need `acc connect`. Check versions match. |
| Ask fails: "failed to read" | Peer's daemon may have old version. Both need latest code. |
| Stale peer port | Peer restarted daemon. Wait for UDP broadcast (every 5s) to update. |
