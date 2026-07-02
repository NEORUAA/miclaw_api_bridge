<div align="center">

# miclaw_api_bridge

**Run Xiaomi miclaw's models locally as an OpenAI / Anthropic-compatible endpoint.**

Sign in with a miclaw-permissioned Xiaomi account, then hit `http://127.0.0.1:8765` from any browser, OpenAI client, or Claude-compatible client.

[![Rust](https://img.shields.io/badge/Rust-1.77+-orange?logo=rust)](https://www.rust-lang.org/)
[![Vue 3](https://img.shields.io/badge/Vue-3-42b883?logo=vue.js)](https://vuejs.org/)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey)]()
[![License](https://img.shields.io/badge/license-MIT-green)](#license)

</div>

---

## What it does

miclaw_api_bridge logs into your Xiaomi account the same way the official **Xiaomi miclaw** desktop client does, then exposes the resulting `mimo` LLM through familiar local HTTP endpoints:

- `POST /v1/chat/completions` — OpenAI Chat Completions (drop-in for Cline, Cherry Studio, OpenAI SDKs, …)
- `POST /v1/responses` — OpenAI Responses API (native passthrough when available, Chat Completions compatibility fallback otherwise)
- `POST /v1/messages` — Anthropic Messages, with full SSE event translation (drop-in for Claude Code and any client honoring `ANTHROPIC_BASE_URL`)
- `GET /v1/models` — the eight verified model ids

> ⚠️ **Account requirement**: your Xiaomi account must already be approved for miclaw access. If the WebUI shows "需要 miclaw 内测权限" or the proxy returns 401 right after login, the account isn't allowlisted — apply through the official miclaw channel first.

Eight model ids are exposed, all routed through the official Xiaomi PC channel. The first six are cloud models; the last two are short back-compat aliases. All models verified via live `/v1/chat/completions` and `/v1/responses` tests:

| Model id | Upstream | Capabilities | Context | Max Output | Notes |
|---|---|---|---|---|---|
| `xiaomi/mimo` | `mimo` | text, vision, audio, video, tools, thinking | 64K | — | Multimodal (default) |
| `xiaomi/mimo-pro` | `mimo-pro` | text, tools, thinking | 256K | 128K | Reasoning model with `thinking` traces |
| `xiaomi/mimo-claw-0301` | `mimo-pro` | text, tools, thinking | 256K | 128K | Claw 0301 reasoning snapshot |
| `xiaomi/MiniMax-M2.5` | `MiniMax-M2.5` | text, tools | 128K | 8K | MiniMax M2.5 |
| `xiaomi/kimi-k2.5` | `kimi-k2.5` | text, tools, thinking | 128K | 8K | Kimi K2.5 reasoning |
| `xiaomi/glm-5` | `glm-5` | text, tools | 128K | 8K | GLM-5 |
| `mimo-omni` | `mimo` | — | — | — | Alias → `xiaomi/mimo` |
| `mimo-pro` | `mimo-pro` | — | — | — | Alias → `xiaomi/mimo-pro` |

> **Upstream mapping**: The "Upstream" column shows the canonical model name returned by the mify backend. For example, `xiaomi/mimo-claw-0301` is routed to `mimo-pro` upstream. The bridge passes `model` through verbatim; the upstream router handles canonicalization.

> **Note**: The client also defines two on-device models (`mimo_vlm`, `vlm` / Qwen3 0.7B) that run locally via the 太一 SDK. These are not exposed through the cloud API bridge.

## Features

- 🔐 **Real Xiaomi OAuth** — username + password + SMS / email 2FA, exactly like the desktop client
- 🔄 **Auto token refresh** — `serviceToken` rotated transparently on 401
- 🔑 **Keychain-backed storage** — credentials live in macOS Keychain / Windows DPAPI / Linux Secret Service, never on disk in plaintext
- 🔌 **Two protocols, one bridge** — speaks both OpenAI Chat Completions and Anthropic Messages
- 🔒 **Optional HTTPS** — serve the WebUI + proxy over TLS; a self-signed cert is auto-generated, or bring your own PEM. Toggle it at runtime from the desktop tray.
- 🛡 **Admin password** — a first-run setup guards the WebUI control plane (`/api/*`) behind an Argon2-hashed password and cookie session
- 🪪 **API-key auth** — optionally require `Authorization: Bearer` keys on `/v1`; create and revoke them from the dashboard (only the hash + prefix are stored)
- 📊 **Per-model usage** — token accounting per model, with windowed (`1h` / `1d` / `7d` / `30d`) charts in the dashboard
- 🌐 **Browser WebUI** — the former desktop UI is served at `http://127.0.0.1:8765`
- 📡 **Live request log** — WebUI streams every proxy hit in real time
- 🖥 **Optional desktop tray** — no embedded webview window; tray menu only opens WebUI or exits
- 🧩 **Headless deployment** — server binary runs on machines without a graphical session

## Quick start

### From a release

1. Grab the binary archive for your platform from the [Releases](../../releases) page.
2. Start the headless server:

   ```bash
   ./miclaw_api_bridge server
   ```

3. Open `http://127.0.0.1:8765` in a browser and sign in with your miclaw-permissioned Xiaomi account.
4. OpenAI / Responses / Anthropic endpoints are available immediately on the same port.

Desktop users can launch `miclaw_api_bridge_desktop` instead. It starts the same local service, opens the WebUI in your default browser, and adds a tray icon with **打开webui** / **退出**.
The tray also exposes a **启用 HTTPS / 关闭 HTTPS** toggle that flips TLS at runtime and restarts the service; **打开webui** then follows the active `http`/`https` scheme automatically.
On Linux, the tray launcher uses the desktop session's StatusNotifierItem/D-Bus tray; headless machines should run `miclaw_api_bridge server`.

For remote/headless servers, keep the default localhost binding and use an SSH tunnel:

```bash
ssh -L 8765:127.0.0.1:8765 user@server
```

### HTTPS

Serve the WebUI and proxy over TLS. With `--tls` and no cert supplied, a self-signed certificate is generated under the config directory on first start:

```bash
./miclaw_api_bridge server --tls
```

The auto-generated cert lists these names in its SAN, so any of them validate once the cert is trusted:

- `localhost`, `127.0.0.1`, `::1`
- `local.miclawbridge.com` — a placeholder hostname for access by name. Map it to wherever the bridge runs (hosts file or local DNS) and you get a warning-free HTTPS origin without regenerating a per-IP cert:

  ```
  # macOS/Linux: /etc/hosts        Windows: C:\Windows\System32\drivers\etc\hosts
  127.0.0.1       local.miclawbridge.com     # bridge on this machine
  192.168.2.1     local.miclawbridge.com     # bridge on a router/NAS at 192.168.2.1
  ```

  Then use `https://local.miclawbridge.com:8765/v1`. Trust the cert once (see below); the hostname matches the SAN, so there's no name-mismatch warning.

**Trusting the self-signed cert**

- macOS: `sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain <config-dir>/tls-cert.pem`
- SDK clients that ship their own CA store ignore the OS keychain — point them at the PEM directly: Node `NODE_EXTRA_CA_CERTS=<path>/tls-cert.pem`, Python `SSL_CERT_FILE=<path>/tls-cert.pem`, curl `--cacert <path>/tls-cert.pem`.

> The cert is generated once and cached. If you upgrade from an older build (whose cert predates `local.miclawbridge.com`), delete `tls-cert.pem` and `tls-key.pem` in the config directory and restart so a cert with the new SAN is regenerated. Config dir: macOS `~/Library/Application Support/com.neoruaa.miclaw_api_bridge/`, OpenWrt `/etc/miclaw_api_bridge/config/miclaw_api_bridge/`.

Bring your own PEM cert/key (either flag implies `--tls`):

```bash
./miclaw_api_bridge server --tls-cert /path/cert.pem --tls-key /path/key.pem
```

OpenWrt exposes the same switch through uci (`tls` / `tls_cert` / `tls_key`); see [openwrt/README.md](openwrt/README.md).

### Admin password & API keys

On first visit the WebUI prompts you to set an admin password; afterwards every control-plane call under `/api/*` requires the cookie session. The proxy endpoints under `/v1` are open by default — turn on **API key required** in the dashboard to enforce `Authorization: Bearer <key>`. Keys are created and revoked from the dashboard, and only their sha256 hash plus a short display prefix are persisted (the raw key is shown once at creation).

### Docker / NAS

Build and run the headless WebUI/proxy image:

```bash
docker compose up -d --build
```

Then open `http://<nas-ip>:8765` and sign in from the browser. The OpenAI-compatible base URL is:

```
http://<nas-ip>:8765/v1
```

If you publish the repository's Docker workflow, tags are pushed to GitHub Container Registry as `ghcr.io/<owner>/<repo>:latest` and `ghcr.io/<owner>/<repo>:vX.Y.Z`. Pulling a published image looks like:

```bash
docker run -d \
  --name miclaw_api_bridge \
  --restart unless-stopped \
  -p 8765:8765 \
  -v "$PWD/data:/data" \
  ghcr.io/<owner>/<repo>:latest
```

Local build equivalent:

```bash
docker build -t miclaw_api_bridge:latest .
docker run -d \
  --name miclaw_api_bridge \
  --restart unless-stopped \
  -p 8765:8765 \
  -v "$PWD/data:/data" \
  miclaw_api_bridge:latest
```

The Docker image binds `0.0.0.0:8765` inside the container and disables OS keyring access by default because NAS/container environments usually do not provide macOS Keychain, Windows DPAPI, or Linux Secret Service. Session data is persisted under the mounted `/data` volume; keep that directory private.

### Hooking up a client

**OpenAI-compatible (Cline, Cherry Studio, OpenAI SDK, …)**

```
Base URL: http://127.0.0.1:8765/v1
API key:  anything
Model:    mimo-pro      # or any model from /v1/models
```

**Anthropic-compatible (Claude Code, etc.)**

```
ANTHROPIC_BASE_URL=http://127.0.0.1:8765
ANTHROPIC_API_KEY=anything
Model: mimo-pro
```

When **API key required** is enabled in the dashboard, replace `anything` with a key minted there. Otherwise any non-empty value is accepted.

**curl smoke test**

```bash
# OpenAI
curl -N http://127.0.0.1:8765/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"mimo-pro","stream":true,"messages":[{"role":"user","content":"hi"}]}'

# OpenAI Responses
curl -N http://127.0.0.1:8765/v1/responses \
  -H 'content-type: application/json' \
  -d '{"model":"mimo-pro","stream":true,"input":"hi"}'

# Anthropic
curl -N http://127.0.0.1:8765/v1/messages \
  -H 'content-type: application/json' \
  -H 'anthropic-version: 2023-06-01' \
  -d '{"model":"mimo-pro","max_tokens":256,"stream":true,"messages":[{"role":"user","content":"hi"}]}'
```

## Build from source

Prerequisites: Rust 1.77+, Node.js 20+, pnpm 9+.

```bash
git clone <this-repo> miclaw_api_bridge && cd miclaw_api_bridge
pnpm install
pnpm build              # build the browser WebUI into dist/
cd src-tauri
cargo build --release --bin miclaw_api_bridge
cargo build --release --features desktop --bin miclaw_api_bridge_desktop
```

The binaries end up in `src-tauri/target/release/`. You can also build both for the current platform with:

```bash
pnpm build:binaries
```

### Release automation

Pushing a version tag builds a draft GitHub Release with platform binary archives:

```bash
git tag v0.1.0
git push origin v0.1.0
```

The release workflow produces:

- macOS universal zip: headless server + desktop tray binaries
- Windows x64 zip: headless server + desktop tray `.exe` binaries
- Windows ARM64 zip: headless server + desktop tray `.exe` binaries
- Linux x64 tar.gz: headless server + desktop tray binaries
- Linux ARM64 tar.gz: headless server + desktop tray binaries
- Docker image: `linux/amd64` + `linux/arm64` pushed to GHCR by `.github/workflows/docker.yml`

Linux and Windows ARM64 releases are built on native GitHub-hosted ARM runners, not emulated cross-builds.

### Tests

```bash
cd src-tauri
cargo test --lib                           # unit tests (incl. Anthropic SSE translator)

# Real-account end-to-end smoke test (manual; never runs in CI)
MIMO_BRIDGE_SMOKE_ACCOUNT=user@example.com \
MIMO_BRIDGE_SMOKE_PASSWORD='secret' \
cargo test --test smoke_login -- --ignored --nocapture
# Then resolve the SMS / email challenge it prints and re-run with:
#   MIMO_BRIDGE_SMOKE_2FA_FLAG=4 MIMO_BRIDGE_SMOKE_2FA_TICKET=123456 MIMO_BRIDGE_SMOKE_CHAT=1
```

## Architecture

```
┌─────────────────┐
│ Browser WebUI   │     /api/*     ┌──────────────────────────┐
│ Login / Status  │◄──────────────►│ Rust headless server     │
│ Logs panel      │    SSE logs    │ optional desktop tray    │
└─────────────────┘                └────────────┬─────────────┘
                                                │ axum on 127.0.0.1:8765
                                                ▼
                       ┌────────────────────────────────────────────┐
                       │ /v1/chat/completions  OpenAI passthrough   │
                       │ /v1/responses         OpenAI compat layer  │
                       │ /v1/messages          Anthropic ⇆ OpenAI   │
                       │ /v1/models            Static manifest      │
                       └────────────────────────┬───────────────────┘
                                                │
                                                ▼
                          api.miclaw.xiaomi.net /osbot/pc/llm/v1/...
                          (Cookie: serviceToken+cUserId, UA: node)
```

### Authentication flow

```
1. POST account.xiaomi.com/pass/serviceLoginAuth2 (sid=miclaw, MD5-upper of password)
   → 2FA challenge if required → identity/list, sendTicket, verifyTicket
   ⇒ passToken + cUserId + ssecurity

2. GET  account.xiaomi.com/pass/serviceLogin?sid=osbotapi
        UA = "miNative PC/...", Cookie = passToken+userId+cUserId+deviceId+uDevId+uLocale+pass_ua
   ⇒ loc + nonce + ssecurity   (nonce extracted from raw JSON to avoid f64 precision loss)

3. GET  <loc>&clientSign=<sig>          (no cookies)
        sig = url_encode( base64( sha1("nonce=N&ssecurity") ) )
   ⇒ Set-Cookie: serviceToken=...   (the token mimo accepts)

4. POST api.miclaw.xiaomi.net/osbot/pc/llm/v1/chat/completions
        Cookie: serviceToken+cUserId, UA: node
   ⇒ OpenAI-compatible Chat Completions (SSE when stream=true)
```

`deviceId` and `uDevId` are derived locally:

```
deviceId = "pc_" + md5_hex( IOPlatformUUID.toLowerCase() )           // matches macOS miclaw verbatim
uDevId   = base64( sha1( userId + deviceId ) )
```

A 401 from any mimo call triggers a transparent re-run of steps 2–3.

## Configuration

| Setting | Where | Default |
|---|---|---|
| Listen port | Dashboard ▸ "Listen port" | `8765` |
| Listen host | CLI `server --host` | `127.0.0.1` |
| HTTPS | CLI `server --tls` / dashboard ▸ HTTPS / tray ▸ 启用 HTTPS | off |
| Custom TLS cert | CLI `server --tls-cert <pem>` (implies `--tls`) | self-signed (auto) |
| Custom TLS key | CLI `server --tls-key <pem>` (implies `--tls`) | self-signed (auto) |
| Admin password | Dashboard ▸ first-run setup | unset (open) |
| API key auth on `/v1` | Dashboard ▸ "API key required" | off |
| OAuth `sid` | env `MIMO_BRIDGE_SID` | `miclaw` |
| Disable OS keyring | env `MICLAW_API_BRIDGE_DISABLE_KEYRING=1` | off |
| Session storage | OS keyring (auto) | n/a |

## FAQ

**Is this a fork of miclaw?**
No — miclaw_api_bridge is an independent client that speaks the same protocol. No code is copied from the official client.

**Does it work without the official miclaw app installed?**
Yes. miclaw_api_bridge talks directly to Xiaomi's account and inference endpoints; the desktop client doesn't need to be installed. **Your account does, however, need miclaw access** — without it the inference API returns 401 even with a valid serviceToken.

**Why does the OAuth flow run twice?**
Xiaomi mints `serviceToken`s scoped to a specific `sid`. Password login uses `sid=miclaw`, but mimo only accepts tokens minted under `sid=osbotapi`. The second leg swaps the former for the latter using the long-lived `passToken`.

**Where are my credentials stored?**
Encrypted in your OS keyring (macOS Keychain / Windows DPAPI / Linux Secret Service). Only the session blob — `passToken / serviceToken / userId / cUserId / ssecurity / nick` — is kept; your password is never persisted. Docker sets `MICLAW_API_BRIDGE_DISABLE_KEYRING=1`, so the session blob is stored in the mounted `/data` volume instead.

**Can I run multiple accounts?**
Not yet. Tracking under [#multi-account](../../issues).

## Roadmap

- [x] Vue WebUI + Rust server scaffolding
- [x] Xiaomi OAuth with 2FA
- [x] mimo PC client with auto token refresh
- [x] OpenAI Chat Completions passthrough
- [x] Anthropic Messages bridge with SSE translation
- [x] Live log panel
- [x] Keychain-backed credential storage
- [x] Headless CLI/server binary
- [x] Browser WebUI
- [x] Desktop tray launcher
- [x] Universal macOS binary (Intel + Apple Silicon)
- [ ] Code signing & notarization
- [x] Windows / Linux x64 + ARM64 release pipeline
- [x] HTTPS / TLS (self-signed auto-gen + custom cert/key)
- [x] Admin password auth for the WebUI control plane
- [x] API key auth (optional Bearer) on `/v1` endpoints
- [x] Per-model token usage dashboard
- [x] OpenWrt ipk packaging
- [ ] Multi-account support
- [ ] Optional rate-limit / quota dashboard

## Contributing

Issues and PRs are welcome. Useful starting points:

- **Bug reports** — please include the `RUST_LOG=miclaw_api_bridge_lib=debug` output for the failing flow.
- **Protocol changes** — Xiaomi tweaks the auth flow occasionally. If you have a fresh HAR capture of the official desktop client, attach it to the issue.

Before opening a PR:

```bash
cd src-tauri && cargo fmt && cargo clippy -- -D warnings && cargo test
```

## Disclaimer

This project is an independent reverse-engineering effort intended for **educational and personal use**. It is not affiliated with, endorsed by, or sponsored by Xiaomi. By using miclaw_api_bridge you accept full responsibility for compliance with the Xiaomi terms of service applicable to your account. The authors provide no warranty and accept no liability.

## License

[MIT](LICENSE)
