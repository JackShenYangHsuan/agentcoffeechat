#!/bin/bash
set -e

VERSION="${1:-0.1.0}"
REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$REPO_ROOT/dist"

echo "=== AgentCoffeeChat Release v${VERSION} ==="

# 1. Clean and build
echo ""
echo "--- Building release binaries ---"
cd "$REPO_ROOT/src"
cargo build --release

# 2. Run tests
echo ""
echo "--- Running tests ---"
cargo test

# 3. Create dist directory
echo ""
echo "--- Packaging ---"
rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

# 4. Create tarball (source archive, excluding target/)
cd "$REPO_ROOT"
tar czf "$DIST_DIR/agentcoffeechat-${VERSION}.tar.gz" \
    --exclude='src/target' \
    --exclude='.git' \
    --exclude='dist' \
    --exclude='.DS_Store' \
    .

# 5. Copy release binaries
mkdir -p "$DIST_DIR/bin"
cp src/target/release/acc "$DIST_DIR/bin/"
cp src/target/release/agentcoffeechat "$DIST_DIR/bin/"
cp src/target/release/agentcoffeechat-daemon "$DIST_DIR/bin/"
cp src/target/release/agentcoffeechat-menubar "$DIST_DIR/bin/" 2>/dev/null || true

# 6. Generate SHA256
echo ""
echo "--- SHA256 ---"
TARBALL_SHA=$(shasum -a 256 "$DIST_DIR/agentcoffeechat-${VERSION}.tar.gz" | awk '{print $1}')
echo "Tarball: $TARBALL_SHA"

# 7. Update formula with real SHA256
FORMULA="$REPO_ROOT/Formula/agentcoffeechat.rb"
if [ -f "$FORMULA" ]; then
    sed -i '' "s/PLACEHOLDER_SHA256/${TARBALL_SHA}/" "$FORMULA"
    sed -i '' "s|refs/tags/v[0-9.]*\.tar\.gz|refs/tags/v${VERSION}.tar.gz|" "$FORMULA"
    echo "Formula updated with SHA256"
fi

# 8. Summary
echo ""
echo "=== Release v${VERSION} Ready ==="
echo ""
echo "Artifacts in $DIST_DIR/:"
ls -lh "$DIST_DIR/agentcoffeechat-${VERSION}.tar.gz"
echo ""
echo "Binary sizes:"
ls -lh "$DIST_DIR/bin/" | grep -v total
echo ""
echo "Next steps:"
echo "  1. Create GitHub repo: gh repo create agentcoffeechat/agentcoffeechat --public"
echo "  2. Push code:          git push origin main"
echo "  3. Create release:     gh release create v${VERSION} dist/agentcoffeechat-${VERSION}.tar.gz dist/bin/* --title 'v${VERSION}' --notes 'Initial release'"
echo "  4. Create tap repo:    gh repo create agentcoffeechat/homebrew-tap --public"
echo "  5. Copy formula:       cp Formula/agentcoffeechat.rb <tap-repo>/"
echo "  6. Users install:      brew tap agentcoffeechat/tap && brew install agentcoffeechat"
