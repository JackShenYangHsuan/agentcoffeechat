#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BIN_DIR="$HOME/.local/bin"

echo "Installing AgentCoffeeChat (dev build)..."

mkdir -p "$BIN_DIR"

# Copy debug binaries
cp "$SCRIPT_DIR/src/target/debug/acc" "$BIN_DIR/acc"
cp "$SCRIPT_DIR/src/target/debug/agentcoffeechat" "$BIN_DIR/agentcoffeechat"
cp "$SCRIPT_DIR/src/target/debug/agentcoffeechat-daemon" "$BIN_DIR/agentcoffeechat-daemon"
cp "$SCRIPT_DIR/src/target/debug/agentcoffeechat-menubar" "$BIN_DIR/agentcoffeechat-menubar" 2>/dev/null || true
chmod +x "$BIN_DIR/acc" "$BIN_DIR/agentcoffeechat" "$BIN_DIR/agentcoffeechat-daemon"
[ -f "$BIN_DIR/agentcoffeechat-menubar" ] && chmod +x "$BIN_DIR/agentcoffeechat-menubar"

echo "✓ Binaries installed to $BIN_DIR"

# Ensure PATH
export PATH="$BIN_DIR:$PATH"

# Run setup
echo ""
echo "Running first-start setup..."
acc start

echo ""
echo "✓ AgentCoffeeChat installed and running!"
echo "  Next sanity check: acc doctor --json"
