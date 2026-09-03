# miclaw_api_bridge on OpenWrt (LuCI 插件)

把 `miclaw_api_bridge` 的 headless server 跑在路由器上，并在 LuCI 后台里通过
iframe 直接嵌入它自带的 WebUI（登录 / 状态 / 日志面板）。前端代码零改动。

目标设备：**mediatek/mt7981**（ARM Cortex-A53，64 位）
固件：
- **OpenWrt / ImmortalWrt 24.10** → `opkg` + `.ipk`
- **OpenWrt 25.12+ / main** → `apk` + `.apk`（官方已切换到 Alpine apk-tools v3）

```
┌──────────────────────────────────────────────┐
│ 浏览器                                          │
│  http://192.168.1.1/  →  LuCI (uhttpd, :80)     │
│    └─ 服务 ▸ Mimo Bridge                        │
│         <iframe src="http://192.168.1.1:8765/"> │
└───────────────┬────────────────────────────────┘
                │ 同源相对请求 /api /v1
                ▼
   miclaw_api_bridge server  (procd 常驻, :8765, 0.0.0.0)
                │
                ▼
   api.miclaw.xiaomi.net  (小米账号 + mimo 推理)
```

## 为什么可行

- 后端是纯 headless 的 axum HTTP 服务，`server --host 0.0.0.0` 即可被局域网访问。
- WebUI 用 hash 路由 + 相对资源路径（vite `base: "./"`），API 走同源 `/api`、`/v1`，
  iframe 指向 `:8765/` 就能加载完整界面，**前端一行都不用改**。
- server 二进制（不带 `desktop` feature）在 `aarch64-unknown-linux-musl` 下无任何
  C 库依赖（reqwest 用 rustls，keyring 用纯 Rust 的 zbus 后端），可静态交叉编译。
- OpenWrt 没有 Secret Service / keyring，用现成的环境变量
  `MICLAW_API_BRIDGE_DISABLE_KEYRING=1` 走磁盘存储，session 落在 `/etc/miclaw_api_bridge`。

## 安全提示

server 监听 `0.0.0.0:8765` 且 WebUI / 代理**没有任何鉴权**，局域网内任何人都能
访问你的登录态和模型代理。请确保：

- 仅在可信内网使用；不要把 8765 端口通过防火墙暴露到 WAN。
- 默认 init 脚本不开放防火墙端口，访问只走 LuCI 内嵌 iframe（同一台路由）。

## 步骤一：交叉编译 aarch64-musl 二进制

在开发机（macOS / Linux）上执行：

```bash
# 需要 docker（脚本用 cross 容器交叉编译，免去本地装 musl 工具链）
bash openwrt/build-openwrt.sh
```

产物：

- `openwrt/out/luci-app-miclaw_0.1.0-1_aarch64_cortex-a53.ipk` —— **OpenWrt 24.10 `opkg install`**
- `openwrt/out/luci-app-miclaw-0.1.0-r1.apk` —— **OpenWrt 25.12+ `apk add`**
- `openwrt/out/miclaw_api_bridge` —— aarch64 静态二进制（已 strip，约 9.5M）
- `openwrt/out/miclaw_api_bridge_openwrt_aarch64.tar.gz` —— tarball + `install.sh`（不想用包管理器时的备选）

> 手动方式见本文件末尾「附录：不用 cross 的手动交叉编译」。

## 步骤二：安装

按固件版本选一种即可。两种包内容相同（二进制 + init + UCI + LuCI），只是包格式不同。

### A) opkg（OpenWrt / ImmortalWrt 24.10）

`build-ipk.sh` 直接产出符合 24.10 格式（gzip-tar 容器）的 ipk，**不需要
OpenWrt SDK**，已在真实 OpenWrt 24.10 rootfs 中实测 `opkg install` 通过。

```bash
ROUTER=root@192.168.1.1

scp openwrt/out/luci-app-miclaw_0.1.0-1_aarch64_cortex-a53.ipk $ROUTER:/tmp/
ssh $ROUTER 'opkg install /tmp/luci-app-miclaw_0.1.0-1_aarch64_cortex-a53.ipk'
```

opkg 会自动：装好全部文件、跑 `postinst`（enable + start 服务、刷新 LuCI 缓存）。
`/etc/config/miclaw_api_bridge` 标记为 conffile，升级时不会覆盖你的改动。

卸载：`opkg remove luci-app-miclaw`（`prerm` 会先停服务）。

> 包架构是 `aarch64_cortex-a53`（mt7981 / mediatek-filogic）。如果 opkg 抱怨架构
> 不匹配，用 `opkg install --force-architecture ...`，或先确认固件包架构：
> `opkg print-architecture`。

### B) apk（OpenWrt 25.12+ / main）

OpenWrt 25.12 起默认包管理器是 [apk](https://openwrt.org/docs/guide-user/additional-software/apk)
（Alpine apk-tools v3，与 Android APK **无关**）。`build-apk.sh` 用官方同款
`apk mkpkg` 产出 **APKv3** 包（**不要**手搓 v2：会报 `v2 package format error`）。

```bash
ROUTER=root@192.168.1.1

scp openwrt/out/luci-app-miclaw-0.1.0-r1.apk $ROUTER:/tmp/
ssh $ROUTER 'apk add --allow-untrusted /tmp/luci-app-miclaw-0.1.0-r1.apk'
```

> 第三方未签名包必须加 `--allow-untrusted`（见官方文档）。若之后把公钥放进
> `/etc/apk/keys/`，可去掉该参数。

卸载：`apk del luci-app-miclaw`。

opkg ↔ apk 速查（官方 cheat sheet）：

| opkg | apk |
|------|-----|
| `opkg update` | `apk update` |
| `opkg install pkg` | `apk add pkg` |
| `opkg remove pkg` | `apk del pkg` |
| `opkg list-installed` | `apk list -I` |

验证（两种安装方式通用）：`curl http://192.168.1.1:8765/api/proxy/status` 返回 JSON；
浏览器打开 LuCI，**服务 (Services) ▸ Mimo Bridge** 即出现内嵌 WebUI。

### 让 session 在 sysupgrade 后保留

ipk / apk 都已把 `/etc/config/miclaw_api_bridge` 设为 conffile（固件升级保留）。登录态存在
`/etc/miclaw_api_bridge/`，要保留它，把这行加进 `/etc/sysupgrade.conf`：

```
/etc/miclaw_api_bridge/
```

## 开启 HTTPS

bridge 内置 TLS：开启后首次启动会在数据目录自动生成自签证书
（`/etc/miclaw_api_bridge/config/miclaw_api_bridge/tls-cert.pem`）。

```bash
uci set miclaw_api_bridge.main.tls=1
uci commit miclaw_api_bridge
/etc/init.d/miclaw_api_bridge restart
```

然后用 `https://<路由IP>:8765/` 访问。证书是自签的，浏览器会提示「不安全/证书无效」，
点继续访问即可。

**用域名 + 信任证书，去掉告警**

自签证书的 SAN 现在包含一个占位域名 `local.miclawbridge.com`（以及
`localhost`/`127.0.0.1`/`::1`）。在你的客户端机器上把它指到路由器 IP：

```
# /etc/hosts （Windows 为 C:\Windows\System32\drivers\etc\hosts）
192.168.2.1   local.miclawbridge.com
```

然后信任路由器上那张证书（从路由器取下 `tls-cert.pem`）：

```bash
scp root@192.168.2.1:/etc/miclaw_api_bridge/config/miclaw_api_bridge/tls-cert.pem .
# macOS：加入系统钥匙串并信任
sudo security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain tls-cert.pem
```

之后用 `https://local.miclawbridge.com:8765/`、`.../v1` 访问，主机名与 SAN 匹配，
不再有告警。SDK 客户端若自带 CA 库（Node / Python），分别用
`NODE_EXTRA_CA_CERTS` / `SSL_CERT_FILE` 指向这张 `tls-cert.pem`。

> 已装过旧版（证书早于 `local.miclawbridge.com`）？删掉路由器上
> `/etc/miclaw_api_bridge/config/miclaw_api_bridge/tls-cert.pem` 和 `tls-key.pem`
> 再 `/etc/init.d/miclaw_api_bridge restart`，会重新生成带新 SAN 的证书。

关闭 HTTPS：`uci set ...tls=0; uci commit; /etc/init.d/... restart`。

想用受信任证书（无浏览器告警），把自己的 PEM 证书/私钥路径填进 UCI：

```bash
uci set miclaw_api_bridge.main.tls=1
uci set miclaw_api_bridge.main.tls_cert='/etc/miclaw_api_bridge/cert.pem'
uci set miclaw_api_bridge.main.tls_key='/etc/miclaw_api_bridge/key.pem'
uci commit miclaw_api_bridge
/etc/init.d/miclaw_api_bridge restart
```

> 已经装了旧版（不带 tls 选项）？重新装新 ipk/apk 即可（`/etc/config`
> 是 conffile，会保留你的其它设置），再执行上面的 `uci set ...tls=1`。

## 步骤二（备选）：tarball + install.sh

不想用包管理器、或想看清每一步做了什么，可以用 tarball：

```bash
ROUTER=root@192.168.1.1

scp openwrt/out/miclaw_api_bridge_openwrt_aarch64.tar.gz $ROUTER:/tmp/
ssh $ROUTER
# --- 以下在路由器上 ---
cd /tmp
tar -xzf miclaw_api_bridge_openwrt_aarch64.tar.gz
cd miclaw_api_bridge_openwrt
sh install.sh
```

卸载：`sh uninstall.sh`（保留配置）或 `sh uninstall.sh --purge`（连登录态一起删）。

## 附录：手动分步部署（不想用 install.sh）

如果你想自己掌控每一步，bundle 解压后的文件就是按目标路径组织的：

```bash
ROUTER=root@192.168.1.1
cd miclaw_api_bridge_openwrt

# 1. 二进制
scp miclaw_api_bridge $ROUTER:/usr/bin/miclaw_api_bridge
ssh $ROUTER 'chmod +x /usr/bin/miclaw_api_bridge'

# 2. procd 服务 + UCI 配置
scp files/etc/init.d/miclaw_api_bridge $ROUTER:/etc/init.d/miclaw_api_bridge
ssh $ROUTER 'chmod +x /etc/init.d/miclaw_api_bridge'
scp files/etc/config/miclaw_api_bridge $ROUTER:/etc/config/miclaw_api_bridge

# 3. 启动并设开机自启
ssh $ROUTER '/etc/init.d/miclaw_api_bridge enable; /etc/init.d/miclaw_api_bridge start'
```

### 手动安装 LuCI 插件

`install.sh` 已经帮你做了这一步；只有在手动分步部署时才需要。LuCI openwrt-24.10
(25.087) 默认是**客户端 JS 框架**（不依赖 Lua 控制器），插件就是一个 view JS +
一个 menu 注册 + 一个 ACL 文件，三个文件直接铺到对应路径：

```bash
ROUTER=root@192.168.1.1

# 1. view（iframe 嵌入逻辑）
ssh $ROUTER 'mkdir -p /www/luci-static/resources/view/miclaw'
scp luci/htdocs/luci-static/resources/view/miclaw/overview.js \
    $ROUTER:/www/luci-static/resources/view/miclaw/overview.js

# 2. 菜单注册
ssh $ROUTER 'mkdir -p /usr/share/luci/menu.d'
scp luci/root/usr/share/luci/menu.d/luci-app-miclaw.json \
    $ROUTER:/usr/share/luci/menu.d/luci-app-miclaw.json

# 3. ACL（授予读 uci / 调 service list 权限）
ssh $ROUTER 'mkdir -p /usr/share/rpcd/acl.d'
scp luci/root/usr/share/rpcd/acl.d/luci-app-miclaw.json \
    $ROUTER:/usr/share/rpcd/acl.d/luci-app-miclaw.json

# 4. 刷新 LuCI / rpcd 缓存
ssh $ROUTER 'rm -f /tmp/luci-indexcache; rm -rf /tmp/luci-modulecache 2>/dev/null; \
             /etc/init.d/rpcd reload; /etc/init.d/uhttpd restart'
```

刷新浏览器后，LuCI 顶栏 **服务 (Services) ▸ Mimo Bridge** 即出现，点开就是内嵌的
WebUI。用具备 miclaw 内测权限的小米账号登录即可。

### 关于 HTTPS / 混合内容

view 会自动检测：如果你通过 `https://` 访问 LuCI，而 bridge 是明文 HTTP，浏览器
会拦截 iframe（混合内容），面板会空白。这时改用 `http://<路由IP>/` 访问 LuCI，
或点页面上的「在新标签打开」按钮。view 里已经有提示。

## CI：GitHub Actions 自动构建

`.github/workflows/openwrt.yml` 已接入：

- 手动触发（workflow_dispatch）或 push `v*` tag 时运行；
- 在 ubuntu runner 上构建 WebUI → 用 `cross` 交叉编译 aarch64-musl → 打 tarball bundle + **同时构建 `.ipk` 与 `.apk`**；
- CI 会从 Alpine 官方 GitHub 镜像下载经过 SHA-256 校验的固定提交，并编译带 `mkpkg` 的 apk-tools v3（OpenWrt 同款 APKv3 生成方式）；
- 产物上传为 workflow artifact `openwrt-aarch64`（含 `.tar.gz`、`.ipk`、`.apk`）；
- 若是 tag 触发，三个产物都会附加到对应的 draft Release（和现有 release.yml 同名同 tag，
  追加资产而非覆盖）。包版本号取自 tag（去掉前缀 `v`）。

打 tag 即可同时产出桌面/服务端二进制（release.yml）、Docker 镜像（docker.yml）和
OpenWrt tarball + ipk + apk（openwrt.yml）：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

## 附录：不用 cross 的手动交叉编译

如果你已经有 OpenWrt SDK 或 musl 工具链：

```bash
rustup target add aarch64-unknown-linux-musl
cd .. && pnpm install && pnpm build          # 先产出 dist/（会被 rust-embed 内嵌）
cd src-tauri

# 指定 musl 链接器（按你的工具链路径调整）
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc
cargo build --release --no-default-features \
  --target aarch64-unknown-linux-musl --bin miclaw_api_bridge

# 产物
ls -lh target/aarch64-unknown-linux-musl/release/miclaw_api_bridge

# 然后手动构建 ipk / apk（不依赖 SDK）
cd ..
bash openwrt/build-ipk.sh src-tauri/target/aarch64-unknown-linux-musl/release/miclaw_api_bridge
bash openwrt/build-apk.sh src-tauri/target/aarch64-unknown-linux-musl/release/miclaw_api_bridge
```

注意：**必须先 `pnpm build` 生成 `dist/`**，否则 `rust-embed` 找不到 WebUI 资源会编译报错。

## ipk / apk 是怎么构建的（无需 SDK）

两个脚本都是**离线打包**，不需要 OpenWrt SDK：

### ipk（`build-ipk.sh`，给 24.10 / opkg）

- 当前 OpenWrt（22.03+/24.10）的 ipk 其实是**一个 gzip 压缩的外层 tar**，里面是
  `./debian-binary`、`./data.tar.gz`、`./control.tar.gz` 三个成员（旧的 `ar` 归档格式
  已不再使用，这台固件的 opkg 会拒绝 `ar` 格式）。
- 脚本把二进制 + init + config + LuCI 文件铺到 `data.tar.gz`，把 control/postinst/
  prerm/postrm/conffiles 放进 `control.tar.gz`，再压成外层 tar。
- 已在官方 OpenWrt 24.10 arm64 rootfs 容器里实测 `opkg install` 通过。
- 文件名：`luci-app-miclaw_<ver>-<release>_<arch>.ipk`

### apk（`build-apk.sh`，给 25.12+ / apk-tools v3）

依据 OpenWrt 源码 `include/package-pack.mk` 与官方文档
[apk package manager](https://openwrt.org/docs/guide-user/additional-software/apk)：

- 调用 **`apk mkpkg`** 生成 **APKv3**（与 buildroot 里 host `apk mkpkg` 同一条路径）。
- 数据包内带 `lib/apk/packages/<name>.{list,conffiles,conffiles_static}`，lifecycle 脚本用
  apk 命名：`post-install` / `post-upgrade` / `pre-deinstall` / `post-deinstall`。
- 版本字段格式：`<ver>-r<release>`（例如 `0.1.0-r1`）；文件名：`luci-app-miclaw-<ver>-r<release>.apk`。
- 主机没有 `apk mkpkg` 时，脚本会尝试用 Docker 跑 `alpine:edge`；CI 则从
  Alpine 官方 [apk-tools GitHub 镜像](https://github.com/alpinelinux/apk-tools)
  下载固定、校验过的源码归档后编译安装。
- **不会**生成 APKv2 手工拼接包——OpenWrt 25.12 会直接报 `v2 package format error`。

环境变量可覆盖（两脚本通用）：`PKG_VERSION`（默认 0.1.0）、`PKG_RELEASE`（默认 1）、
`PKG_ARCH`（默认 `aarch64_cortex-a53`）。apk 还可设 `APK_BIN` / `APK_IMAGE`。

`openwrt/ipk/luci-app-miclaw/Makefile` 是一个**可选**的 OpenWrt SDK Makefile 骨架，
仅在你想用官方 SDK/buildroot 构建（比如发布到自定义 feed）时才需要——日常用
`build-ipk.sh` / `build-apk.sh` 即可。
