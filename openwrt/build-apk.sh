#!/usr/bin/env bash
# Build a real, apk-installable .apk for OpenWrt 25.12+ — WITHOUT the full SDK.
#
# OpenWrt 25.12 and newer use Alpine's apk-tools v3 as the package manager
# (see https://openwrt.org/docs/guide-user/additional-software/apk).
# Packages must be APKv3; legacy APKv2 (two concatenated gzip tars) is rejected
# with "v2 package format error".
#
# Official packaging (include/package-pack.mk) calls host `apk mkpkg` with
# --info / --script / --files. We mirror that layout here:
#   - package data tree under --files
#   - conffiles at lib/apk/packages/<name>.conffiles
#   - file inventory at lib/apk/packages/<name>.list
#   - lifecycle scripts: post-install, post-upgrade, pre-deinstall, post-deinstall
#
# Filename convention (OpenWrt): <name>-<version>.apk
# Version convention:          <pkgver>-r<release>   (e.g. 0.1.0-r1)
#
# Usage:
#   openwrt/build-apk.sh <path-to-binary> [output.apk]
#
# Env overrides:
#   PKG_VERSION   default 0.1.0
#   PKG_RELEASE   default 1
#   PKG_ARCH      default aarch64_cortex-a53  (mt7981 / mediatek-filogic)
#   APK_BIN       path to apk binary that supports `mkpkg` (optional)
#   APK_IMAGE     docker image used when host apk is missing (default alpine:edge)
set -euo pipefail

BIN="${1:?usage: build-apk.sh <binary> [output.apk]}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OWRT="$ROOT/openwrt"

PKG_NAME="luci-app-miclaw"
PKG_VERSION="${PKG_VERSION:-0.1.0}"
PKG_RELEASE="${PKG_RELEASE:-1}"
PKG_ARCH="${PKG_ARCH:-aarch64_cortex-a53}"
# OpenWrt APK version field is "<ver>-r<release>" (not the ipk's "<ver>-<release>").
PKG_VER_FULL="${PKG_VERSION}-r${PKG_RELEASE}"

OUT="${2:-$OWRT/out/${PKG_NAME}-${PKG_VER_FULL}.apk}"

mkdir -p "$(dirname "$OUT")"
OUT="$(cd "$(dirname "$OUT")" && pwd)/$(basename "$OUT")"

test -f "$BIN" || { echo "!! binary not found: $BIN" >&2; exit 1; }

# macOS tar must not inject AppleDouble (._*) metadata into any intermediate archives.
export COPYFILE_DISABLE=1

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT

# --- data tree: files at their final install paths -------------------------
DATA="$STAGE/data"
mkdir -p \
  "$DATA/usr/bin" \
  "$DATA/etc/init.d" \
  "$DATA/etc/config" \
  "$DATA/www/luci-static/resources/view/miclaw" \
  "$DATA/usr/share/luci/menu.d" \
  "$DATA/usr/share/rpcd/acl.d" \
  "$DATA/lib/apk/packages"

install -m 0755 "$BIN"                                   "$DATA/usr/bin/miclaw_api_bridge"
install -m 0755 "$OWRT/files/etc/init.d/miclaw_api_bridge" "$DATA/etc/init.d/miclaw_api_bridge"
install -m 0644 "$OWRT/files/etc/config/miclaw_api_bridge" "$DATA/etc/config/miclaw_api_bridge"
install -m 0644 "$OWRT/luci/htdocs/luci-static/resources/view/miclaw/overview.js" \
  "$DATA/www/luci-static/resources/view/miclaw/overview.js"
install -m 0644 "$OWRT/luci/root/usr/share/luci/menu.d/luci-app-miclaw.json" \
  "$DATA/usr/share/luci/menu.d/luci-app-miclaw.json"
install -m 0644 "$OWRT/luci/root/usr/share/rpcd/acl.d/luci-app-miclaw.json" \
  "$DATA/usr/share/rpcd/acl.d/luci-app-miclaw.json"

# APK protected-paths / conffiles (OpenWrt package-pack.mk layout)
cat > "$DATA/lib/apk/packages/${PKG_NAME}.conffiles" <<EOF
/etc/config/miclaw_api_bridge
EOF

# Optional static checksum list used by OpenWrt for conffile upgrade handling.
if command -v sha256sum >/dev/null 2>&1; then
  csum=$(sha256sum "$DATA/etc/config/miclaw_api_bridge" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  csum=$(shasum -a 256 "$DATA/etc/config/miclaw_api_bridge" | awk '{print $1}')
else
  csum=""
fi
if [ -n "$csum" ]; then
  echo "/etc/config/miclaw_api_bridge $csum" \
    > "$DATA/lib/apk/packages/${PKG_NAME}.conffiles_static"
fi

# File inventory (paths relative to /), same as package-pack.mk.
(
  cd "$DATA"
  # portable find (no -printf): list regular files + symlinks
  find . \( -type f -o -type l \) | sed 's|^\./|/|' | sort \
    > "lib/apk/packages/${PKG_NAME}.list"
)

# --- lifecycle scripts (apk script types, not opkg postinst/prerm names) ---
SCRIPTS="$STAGE/scripts"
mkdir -p "$SCRIPTS"

# post-install / post-upgrade: enable + start service, refresh LuCI caches.
# IPKG_INSTROOT is still set by OpenWrt's apk wrappers during image builds.
cat > "$SCRIPTS/post-install" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT}" ] && exit 0
if [ -x /etc/init.d/miclaw_api_bridge ]; then
	/etc/init.d/miclaw_api_bridge enable 2>/dev/null
	/etc/init.d/miclaw_api_bridge start 2>/dev/null
fi
rm -f /tmp/luci-indexcache
rm -rf /tmp/luci-modulecache
killall -HUP rpcd 2>/dev/null
exit 0
EOF
cp "$SCRIPTS/post-install" "$SCRIPTS/post-upgrade"

cat > "$SCRIPTS/pre-deinstall" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT}" ] && exit 0
if [ -x /etc/init.d/miclaw_api_bridge ]; then
	/etc/init.d/miclaw_api_bridge stop 2>/dev/null
	/etc/init.d/miclaw_api_bridge disable 2>/dev/null
fi
exit 0
EOF

cat > "$SCRIPTS/post-deinstall" <<'EOF'
#!/bin/sh
rm -f /tmp/luci-indexcache
rm -rf /tmp/luci-modulecache
killall -HUP rpcd 2>/dev/null
exit 0
EOF

chmod 0755 "$SCRIPTS"/*

# --- locate / obtain `apk mkpkg` (APKv3 only) ------------------------------
# Prefer a host binary; otherwise run Alpine's apk-tools inside Docker.
# Do NOT fall back to hand-rolled APKv2 — OpenWrt 25.12+ rejects it.
#
# Detection cannot rely on `apk mkpkg --help`: apk-tools may be built with
# -Dhelp=disabled (as in our CI). Probe the applet by invoking it bare —
# a real mkpkg replies with "required info field 'name'".
apk_has_mkpkg() {
  local bin="$1" out
  [ -x "$bin" ] || return 1
  out="$("$bin" mkpkg 2>&1 || true)"
  case "$out" in
    *"required info field"*|*"--info"*) return 0 ;;
  esac
  return 1
}

resolve_apk() {
  if [ -n "${APK_BIN:-}" ]; then
    if apk_has_mkpkg "$APK_BIN"; then
      echo "$APK_BIN"
      return 0
    fi
    echo "!! APK_BIN=$APK_BIN does not support 'mkpkg'" >&2
    return 1
  fi
  if command -v apk >/dev/null 2>&1 && apk_has_mkpkg "$(command -v apk)"; then
    command -v apk
    return 0
  fi
  return 1
}

# Drop host xattrs (macOS com.apple.provenance etc.) before packaging.
strip_xattrs() {
  if command -v xattr >/dev/null 2>&1; then
    xattr -cr "$1" 2>/dev/null || true
  fi
  # Also remove any AppleDouble sidecar files that slipped in.
  find "$1" -name '._*' -delete 2>/dev/null || true
}

run_mkpkg_host() {
  local apk_bin="$1"
  strip_xattrs "$DATA"
  strip_xattrs "$SCRIPTS"

  # Write a small runner so fakeroot doesn't choke on complex argv / parentheses
  # in --info description (macOS fakeroot eval is fragile).
  # OpenWrt package-pack.mk likewise wraps apk mkpkg in fakeroot so ownership
  # is recorded as root:root inside the package metadata.
  local runner="$STAGE/run-mkpkg.sh"
  cat > "$runner" <<EOF
#!/bin/sh
set -e
# Best-effort root ownership. Under Linux fakeroot this is virtual and succeeds;
# under real root it is real. On plain macOS without working fakeroot, chown
# may fail — package still builds, with a note below.
chown -R 0:0 "$DATA" "$SCRIPTS" 2>/dev/null || true
exec "$apk_bin" mkpkg \\
  --xattrs=no \\
  --info "name:${PKG_NAME}" \\
  --info "version:${PKG_VER_FULL}" \\
  --info "description:LuCI app + headless server for miclaw_api_bridge. Runs Xiaomi mimo as a local OpenAI/Anthropic-compatible endpoint and embeds its WebUI into LuCI via an iframe." \\
  --info "arch:${PKG_ARCH}" \\
  --info "license:MIT" \\
  --info "origin:openwrt/" \\
  --info "url:https://github.com/neoruaa/mimo-bridge" \\
  --info "maintainer:neoruaa" \\
  --info "depends:luci-base" \\
  --info "tags:openwrt:section=luci" \\
  --script "post-install:${SCRIPTS}/post-install" \\
  --script "post-upgrade:${SCRIPTS}/post-upgrade" \\
  --script "pre-deinstall:${SCRIPTS}/pre-deinstall" \\
  --script "post-deinstall:${SCRIPTS}/post-deinstall" \\
  --files "$DATA" \\
  --output "$OUT"
EOF
  chmod 0755 "$runner"

  if [ "$(id -u)" -eq 0 ]; then
    "$runner"
  elif command -v fakeroot >/dev/null 2>&1; then
    if ! fakeroot -- "$runner"; then
      echo "==> fakeroot mkpkg failed; retrying without fakeroot" >&2
      "$runner"
    fi
  else
    echo "==> note: no root/fakeroot; package metadata may keep host uid/gid" >&2
    "$runner"
  fi
}

run_mkpkg_docker() {
  local image="${APK_IMAGE:-alpine:edge}"
  if ! command -v docker >/dev/null 2>&1; then
    return 1
  fi
  if ! docker info >/dev/null 2>&1; then
    return 1
  fi

  echo "==> host apk mkpkg not found; using docker image ${image}"
  # Mount STAGE so scripts + data are visible; write output into STAGE then copy.
  local docker_out="/work/out/${PKG_NAME}-${PKG_VER_FULL}.apk"
  mkdir -p "$STAGE/out"
  strip_xattrs "$DATA"
  strip_xattrs "$SCRIPTS"
  docker run --rm \
    -v "$STAGE:/work" \
    -w /work \
    "$image" \
    sh -c "
      set -e
      # edge ships apk-tools v3 with mkpkg; ensure present
      apk add --no-cache apk-tools >/dev/null
      # Host-mounted tree keeps host uids — force root for installable package.
      chown -R 0:0 /work/data /work/scripts
      apk mkpkg \
        --xattrs=no \
        --info 'name:${PKG_NAME}' \
        --info 'version:${PKG_VER_FULL}' \
        --info 'description:LuCI app + headless server for miclaw_api_bridge. Runs Xiaomi mimo as a local OpenAI/Anthropic-compatible endpoint and embeds its WebUI into LuCI via an iframe.' \
        --info 'arch:${PKG_ARCH}' \
        --info 'license:MIT' \
        --info 'origin:openwrt/' \
        --info 'url:https://github.com/neoruaa/mimo-bridge' \
        --info 'maintainer:neoruaa' \
        --info 'depends:luci-base' \
        --info 'tags:openwrt:section=luci' \
        --script 'post-install:/work/scripts/post-install' \
        --script 'post-upgrade:/work/scripts/post-upgrade' \
        --script 'pre-deinstall:/work/scripts/pre-deinstall' \
        --script 'post-deinstall:/work/scripts/post-deinstall' \
        --files /work/data \
        --output '${docker_out}'
    "
  cp -f "$STAGE/out/${PKG_NAME}-${PKG_VER_FULL}.apk" "$OUT"
}

if APK_RESOLVED="$(resolve_apk)"; then
  run_mkpkg_host "$APK_RESOLVED"
elif run_mkpkg_docker; then
  :
else
  cat >&2 <<'ERR'
!! cannot build APKv3: no `apk mkpkg` available.

OpenWrt 25.12+ requires APKv3 packages produced by apk-tools' `apk mkpkg`
(see openwrt include/package-pack.mk). Options:

  1) Install apk-tools v3 with mkpkg on the host and re-run, e.g. on Alpine:
       apk add apk-tools
  2) Build from source (meson -Dminimal=false), then set APK_BIN=
       https://gitlab.alpinelinux.org/alpine/apk-tools
  3) Install Docker so this script can use alpine:edge's apk mkpkg
       (set APK_IMAGE to override the image)

Do NOT hand-craft APKv2 (concatenated control+data tars) — OpenWrt rejects them
with "v2 package format error".
ERR
  exit 1
fi

test -f "$OUT" || { echo "!! apk not produced: $OUT" >&2; exit 1; }

echo "==> apk: $OUT"
ls -lh "$OUT"
echo "    arch=${PKG_ARCH}  version=${PKG_VER_FULL}"
echo "    install: apk add --allow-untrusted $(basename "$OUT")"
