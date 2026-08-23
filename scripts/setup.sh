#!/bin/bash
set -e

# Crew contributor setup.
# Bootstraps both halves of the workspace: JS (bun workspaces) and Rust
# (batcave runtime). Safe to re-run. Does not assume rustup -- Rust may be
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

echo "Installing JS workspace dependencies..."
bun install

echo "Building batcave runtime..."
cargo build -p crew-runtime

echo ""
echo "Setup complete."
echo "  Run 'bun run check' before opening a PR (schema drift + build + all tests)."
echo "  See docs/getting-started.md for environment variables and workflows."
