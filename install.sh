#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SRC_DIR="$SCRIPT_DIR/src"
BIN_DIR="$HOME/.local/bin"

echo "Installing AgentCoffeeChat..."

# Build if needed
if [ ! -f "$SRC_DIR/target/release/acc" ]; then
    echo "Building release binary..."
    cd "$SRC_DIR"
    cargo build --release
    cd -
fi

mkdir -p "$BIN_DIR"

# Copy binaries
cp "$SRC_DIR/target/release/acc" "$BIN_DIR/acc"
cp "$SRC_DIR/target/release/agentcoffeechat" "$BIN_DIR/agentcoffeechat"
cp "$SRC_DIR/target/release/agentcoffeechat-daemon" "$BIN_DIR/agentcoffeechat-daemon"
cp "$SRC_DIR/target/release/agentcoffeechat-menubar" "$BIN_DIR/agentcoffeechat-menubar" 2>/dev/null || true
chmod +x "$BIN_DIR/acc" "$BIN_DIR/agentcoffeechat" "$BIN_DIR/agentcoffeechat-daemon"
[ -f "$BIN_DIR/agentcoffeechat-menubar" ] && chmod +x "$BIN_DIR/agentcoffeechat-menubar"

echo "✓ Binaries installed to $BIN_DIR"

# Check PATH
if ! echo "$PATH" | grep -q "$BIN_DIR"; then
    echo ""
    echo "Add to your PATH (add to ~/.zshrc):"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
fi

# Run setup
export PATH="$BIN_DIR:$PATH"

echo ""
echo "Running first-start setup..."
acc start

echo ""
echo "✓ AgentCoffeeChat installed and running!"
echo "  Say 'connect to <name>' to chat with a nearby dev."
echo "  Next sanity check: acc doctor --json"
