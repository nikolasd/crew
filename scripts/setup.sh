#!/bin/bash
set -e

# Crew contributor setup.
# Bootstraps both halves of the workspace: JS (bun workspaces) and Rust
# (crewd runtime). Safe to re-run. Does not assume rustup -- Rust may be
# installed via rustup, Homebrew, or a system package manager.

cd "$(dirname "$0")/.."

REQUIRED_RUST_VERSION=$(grep '^channel' rust-toolchain.toml | sed -E 's/.*"([^"]+)".*/\1/')

echo "== Crew contributor setup =="

if ! command -v cargo &> /dev/null; then
  echo "Error: cargo not found on PATH." >&2
  echo "Install Rust ${REQUIRED_RUST_VERSION} via rustup (recommended -- respects" >&2
  echo "rust-toolchain.toml's pinned version automatically):" >&2
  echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh" >&2
  echo "Or via Homebrew (no automatic version pinning -- verify the version yourself):" >&2
  echo "  brew install rust" >&2
  exit 1
fi

if command -v rustup &> /dev/null; then
  echo "rustup detected -- rust-toolchain.toml (${REQUIRED_RUST_VERSION}) is respected automatically."
else
  ACTUAL_RUST_VERSION=$(rustc --version | awk '{print $2}')
  if [ "$ACTUAL_RUST_VERSION" != "$REQUIRED_RUST_VERSION" ]; then
    echo "Warning: no rustup on PATH to enforce the pinned toolchain." >&2
    echo "  rust-toolchain.toml requires rustc ${REQUIRED_RUST_VERSION}, found ${ACTUAL_RUST_VERSION}." >&2
    echo "  Continuing anyway -- install rustup for automatic version pinning if you hit build issues." >&2
  fi
fi

if ! command -v bun &> /dev/null; then
  echo "Error: bun not found on PATH. Install it: https://bun.sh" >&2
  exit 1
fi

# Warn (not fail) if bun is older than the pinned version. The committed extension bundle
# (packages/extension/dist/index.js) is verified in CI against a linux-x64 build at the pinned
# Bun version; an older bun can rebuild a bundle CI rejects as stale.
REQUIRED_BUN_VERSION="1.3.14"
bun_version_ge() {
  # $1 = have, $2 = want; returns 0 if have >= want
  awk -v have="$1" -v want="$2" 'BEGIN{
    split(have, a, "."); split(want, b, ".");
    for (i = 1; i <= 3; i++) { x = a[i] + 0; y = b[i] + 0; if (x > y) exit 0; if (x < y) exit 1 }
    exit 0
  }'
}
BUN_VERSION="$(bun --version)"
if ! bun_version_ge "$BUN_VERSION" "$REQUIRED_BUN_VERSION"; then
  echo "Warning: bun ${BUN_VERSION} found, but the project pins bun >= ${REQUIRED_BUN_VERSION} (packageManager field)." >&2
  echo "  The committed bundle is verified in CI against a linux-x64 build at ${REQUIRED_BUN_VERSION};" >&2
  echo "  an older bun may produce a bundle CI rejects. Install ${REQUIRED_BUN_VERSION} or newer and re-run." >&2
fi

echo "Installing JS workspace dependencies..."
bun install

echo "Building crewd runtime..."
cargo build -p crew-runtime

echo ""
echo "Setup complete."
echo "  Run 'bun run check' before opening a PR (schema drift + build + all tests)."
echo "  See docs/development.md for environment variables and workflows."
