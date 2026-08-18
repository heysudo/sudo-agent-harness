#!/usr/bin/env bash
# Build the aarch64 Pi binary from a dev machine. NEVER compile on the Pi.
#
# Two paths, chosen automatically:
#
#   Apple Silicon Mac (arm64 host)  -> native build inside an arm64 Debian
#                                      container. No emulation, no cross-linker,
#                                      because host and target are the same arch.
#                                      `cross` does NOT work here: its images for
#                                      this target are x86_64-only, so it tries to
#                                      install an x86_64 Rust toolchain in an arm64
#                                      container and dies.
#
#   x86_64 host (Linux or Intel Mac) -> `cross`, per deploy/Cross.toml.
#
# Both need a container runtime (Docker or colima). Output in both cases:
#   target/aarch64-unknown-linux-gnu/release/hermit
#
# Usage:  scripts/build-pi.sh            # release, --features pi
#         scripts/build-pi.sh --debug    # faster iteration build

set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE_FLAG="--release"
OUT_DIR="release"
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE_FLAG=""
  OUT_DIR="debug"
fi

TARGET="aarch64-unknown-linux-gnu"
IMAGE="rust:1-bookworm"   # glibc 2.36 = Raspberry Pi OS Bookworm

if ! command -v docker >/dev/null 2>&1; then
  echo "error: docker CLI not found. On macOS: brew install colima docker && colima start" >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  echo "error: docker daemon not reachable. On macOS: colima start" >&2
  exit 1
fi

HOST_ARCH="$(uname -m)"
echo "host: $(uname -s) $HOST_ARCH   target: $TARGET   profile: $OUT_DIR"

if [[ "$HOST_ARCH" == "arm64" || "$HOST_ARCH" == "aarch64" ]]; then
  echo "== native arm64 container build =="
  # A named volume for the cargo registry so dependency downloads are cached
  # across builds instead of re-fetched every time.
  docker run --rm --platform linux/arm64 \
    -v "$PWD":/work -w /work \
    -v hermit-cargo-registry:/usr/local/cargo/registry \
    -e CARGO_TERM_COLOR=always \
    "$IMAGE" bash -c "
      set -e
      apt-get update -qq
      apt-get install -y -qq --no-install-recommends pkg-config libasound2-dev >/dev/null
      cargo build $PROFILE_FLAG --target $TARGET --features pi
    "
else
  echo "== cross build =="
  if ! command -v cross >/dev/null 2>&1; then
    echo "error: cross not installed: cargo install cross --locked" >&2
    exit 1
  fi
  export PKG_CONFIG_ALLOW_CROSS=1
  export CROSS_CONFIG=deploy/Cross.toml
  cross build $PROFILE_FLAG --target "$TARGET" --features pi
fi

BIN="target/$TARGET/$OUT_DIR/hermit"
echo
echo "== result =="
ls -lh "$BIN"
file "$BIN" 2>/dev/null || true
echo
echo "deploy with:  scripts/deploy.sh <pi-host>"
