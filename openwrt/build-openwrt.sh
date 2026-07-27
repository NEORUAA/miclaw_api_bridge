#!/usr/bin/env bash
# Cross-compile the headless miclaw_api_bridge server for OpenWrt / mt7981
# (aarch64-unknown-linux-musl, fully static, no C deps).
#
# Strategy: build the Vue WebUI on the host (so rust-embed has dist/), then
# cross-compile the Rust `server` binary inside a `cross` Docker container so
# you don't need a local musl toolchain.
#
# Usage:
#   bash openwrt/build-openwrt.sh
#
# Output:
#   openwrt/out/miclaw_api_bridge   (aarch64 static, stripped)
set -euo pipefail

TARGET="aarch64-unknown-linux-musl"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT_DIR="$ROOT/openwrt/out"

echo "==> repo root: $ROOT"
mkdir -p "$OUT_DIR"

# --- 1. Build the WebUI so rust-embed can inline dist/ -----------------------
echo "==> building Vue WebUI (pnpm build)"
cd "$ROOT"
if ! command -v pnpm >/dev/null 2>&1; then
  echo "!! pnpm not found. Install Node 20+ and 'corepack enable && corepack prepare pnpm@latest --activate'." >&2
  exit 1
fi
pnpm install --frozen-lockfile
pnpm build
test -f "$ROOT/dist/index.html" || { echo "!! dist/ not produced" >&2; exit 1; }

# --- 2. Cross-compile the Rust server binary --------------------------------
cd "$ROOT/src-tauri"

# `cross` builds inside a container that only mounts this src-tauri workspace;
# expose the host-built ../dist (inlined by rust-embed) via the MICLAW_DIST_DIR
# volume declared in src-tauri/Cross.toml.
export MICLAW_DIST_DIR="$ROOT/dist"

build_with_cross() {
  echo "==> cross build for $TARGET (docker)"
  cross build --release --no-default-features --target "$TARGET" --bin miclaw_api_bridge
}

build_with_rustup() {
  echo "==> native rustup cross build for $TARGET"
  rustup target add "$TARGET"
  : "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:?set this to your musl linker, e.g. aarch64-linux-musl-gcc}"
  cargo build --release --no-default-features --target "$TARGET" --bin miclaw_api_bridge
}

if command -v cross >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  build_with_cross
elif command -v cross >/dev/null 2>&1; then
  echo "!! 'cross' found but Docker daemon not reachable; falling back to rustup." >&2
  build_with_rustup
else
  echo "!! 'cross' not found. Install it for the easiest path:" >&2
  echo "     cargo install cross --git https://github.com/cross-rs/cross" >&2
  echo "   ...or set CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER and rerun for a manual build." >&2
  build_with_rustup
fi

BIN="$ROOT/src-tauri/target/$TARGET/release/miclaw_api_bridge"
test -f "$BIN" || { echo "!! build did not produce $BIN" >&2; exit 1; }

# --- 3. Strip + stage -------------------------------------------------------
cp -f "$BIN" "$OUT_DIR/miclaw_api_bridge"
if command -v aarch64-linux-musl-strip >/dev/null 2>&1; then
  aarch64-linux-musl-strip "$OUT_DIR/miclaw_api_bridge" || true
elif command -v llvm-strip >/dev/null 2>&1; then
  llvm-strip "$OUT_DIR/miclaw_api_bridge" || true
fi

echo "==> binary: $OUT_DIR/miclaw_api_bridge"
file "$OUT_DIR/miclaw_api_bridge" 2>/dev/null || true
ls -lh "$OUT_DIR/miclaw_api_bridge"

# --- 4. Assemble deployable bundle -----------------------------------------
echo "==> packing deployable bundle (tar.gz)"
bash "$ROOT/openwrt/pack-bundle.sh" "$OUT_DIR/miclaw_api_bridge"

# --- 5. Build opkg-installable ipk (OpenWrt 24.10 and older) ----------------
echo "==> building ipk"
bash "$ROOT/openwrt/build-ipk.sh" "$OUT_DIR/miclaw_api_bridge"

# --- 6. Build apk-installable apk (OpenWrt 25.12+ / main) -------------------
echo "==> building apk"
bash "$ROOT/openwrt/build-apk.sh" "$OUT_DIR/miclaw_api_bridge"

echo
echo "==> done. Ways to deploy:"
echo
echo "  A) opkg package (OpenWrt 24.10 / ImmortalWrt 24.10):"
echo "     scp $OUT_DIR/luci-app-miclaw_*_aarch64_cortex-a53.ipk root@192.168.1.1:/tmp/"
echo "     ssh root@192.168.1.1 'opkg install /tmp/luci-app-miclaw_*_aarch64_cortex-a53.ipk'"
echo
echo "  B) apk package (OpenWrt 25.12+):"
echo "     scp $OUT_DIR/luci-app-miclaw-*.apk root@192.168.1.1:/tmp/"
echo "     ssh root@192.168.1.1 'apk add --allow-untrusted /tmp/luci-app-miclaw-*.apk'"
echo
echo "  C) tarball + installer script:"
echo "     scp $OUT_DIR/miclaw_api_bridge_openwrt_aarch64.tar.gz root@192.168.1.1:/tmp/"
echo "     ssh root@192.168.1.1"
echo "     cd /tmp && tar -xzf miclaw_api_bridge_openwrt_aarch64.tar.gz && cd miclaw_api_bridge_openwrt && sh install.sh"
