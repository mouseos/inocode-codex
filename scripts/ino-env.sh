#!/bin/bash
# Shared build environment for every InoCode session (bash). Usage:
#   source scripts/ino-env.sh          # CLI (codex-rs)  -> then: cd codex-rs && cargo ... --profile dev-small -p <crate>
#   source scripts/ino-env.sh gui      # GUI (gui/inocode-desktop) -> then: cd gui/inocode-desktop && cargo build -j 1
# Only paths and cargo behaviour live here; nothing that changes cargo's artifact hashes
# (no RUSTFLAGS, no CARGO_PROFILE_*), so every session reuses the same artifacts.
_ino_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
mkdir -p "$_ino_root/.build/cargo-home" "$_ino_root/.build/tmp"
export CARGO_HOME="$_ino_root/.build/cargo-home"
export TMP="$_ino_root/.build/tmp" TEMP="$_ino_root/.build/tmp"
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_INCREMENTAL=0
export RUST_MIN_STACK=67108864   # 64MB; 256MB x many rustc threads crashed rustc (0xc0000409)
unset HTTPS_PROXY HTTP_PROXY ALL_PROXY NODE_EXTRA_CA_CERTS
if [ "$1" = "gui" ]; then
  export CARGO_TARGET_DIR="$_ino_root/../../gui/inocode-desktop/target"
  # The GUI has its own Cargo home semantics: keep the default one.
  unset CARGO_HOME
  export RUST_MIN_STACK=268435456   # GUI builds with -j 1
else
  export CARGO_TARGET_DIR="$_ino_root/codex-rs/target-win"
fi
unset _ino_root
