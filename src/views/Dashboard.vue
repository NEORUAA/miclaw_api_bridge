<script setup lang="ts">
import { computed, onMounted, ref } from "vue";
import { RouterLink } from "vue-router";
import { api, AuthSnapshot, ModelInfo, ProxySnapshot, QuotaSnapshot } from "../api";

const auth = ref<AuthSnapshot | null>(null);
const proxy = ref<ProxySnapshot | null>(null);
const models = ref<ModelInfo[]>([]);
const quota = ref<QuotaSnapshot | null>(null);
const portInput = ref<number>(8765);
const busy = ref(false);
const err = ref("");

const proxyBase = computed(() => {
  const port = proxy.value?.active_port ?? proxy.value?.port ?? 8765;
  return `${window.location.protocol}//${window.location.hostname}:${port}`;
});
const health = computed(() => {
  if (!auth.value?.authenticated) return { label: "账号未登录", tone: "bad" };
  if (!proxy.value?.running) return { label: "服务未运行", tone: "warn" };
  if (proxy.value.restart_required) return { label: "需重启生效", tone: "warn" };
  return { label: "Ready", tone: "ok" };
});
const quotaTone = computed(() => {
  if (!auth.value?.authenticated || !quota.value) return "warn";
  if (quota.value.status === "exhausted") return "bad";
  if (quota.value.status === "low") return "warn";
  return "ok";
});
const quotaRemaining = computed(() =>
  quota.value ? new Intl.NumberFormat("zh-CN").format(quota.value.points_remaining) : "—",
);
const quotaMeta = computed(() => {
  if (!auth.value?.authenticated) return "登录后显示";
  if (!quota.value) return "暂时无法获取";
  const resetAt = new Date(quota.value.quota_reset_at).toLocaleString("zh-CN", {
    month: "numeric",
    day: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
  return `${quota.value.points_used.toLocaleString("zh-CN")} / ${quota.value.points_limit.toLocaleString("zh-CN")} 已用 · ${resetAt} 重置`;
});

async function loadQuota() {
  quota.value = auth.value?.authenticated ? await api.quota() : null;
}

async function refreshAll() {
  err.value = "";
  try {
    auth.value = await api.authStatus();
    proxy.value = await api.proxyStatus();
    models.value = await api.listModels();
    portInput.value = proxy.value.port;
    await loadQuota().catch(() => {
      quota.value = null;
    });
  } catch (e: any) {
    err.value = String(e);
  }
}

async function applyPort() {
  busy.value = true;
  try {
    proxy.value = await api.setProxyPort(portInput.value);
  } catch (e: any) {
    err.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function refreshAuth() {
  busy.value = true;
  try {
    auth.value = await api.refreshSession();
    await loadQuota();
  } catch (e: any) {
    err.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function refreshQuota() {
  busy.value = true;
  try {
    await loadQuota();
  } catch (e: any) {
    err.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function logout() {
  await api.logout();
  quota.value = null;
  await refreshAll();
}

onMounted(refreshAll);
</script>

<template>
  <p v-if="err" class="notice bad">{{ err }}</p>

  <section class="status-strip" aria-label="运行状态">
    <div>
      <span class="label">Bridge</span>
      <strong :class="['signal', health.tone]">{{ health.label }}</strong>
    </div>
    <div>
      <span class="label">Account</span>
      <strong>{{ auth?.authenticated ? auth.nick ?? auth.user_id ?? "已登录" : "未登录" }}</strong>
    </div>
    <div>
      <span class="label">剩余积分</span>
      <strong :class="quotaTone">{{ quotaRemaining }}</strong>
      <small class="status-meta">{{ quotaMeta }}</small>
    </div>
    <div>
      <span class="label">Port</span>
      <strong>{{ proxy?.active_port ?? proxy?.port ?? 8765 }}</strong>
    </div>
    <div>
      <span class="label">Models</span>
      <strong>{{ models.length || "—" }}</strong>
    </div>
  </section>

  <section class="panel proxy-panel">
    <div class="panel-heading">
      <p class="section-number">02</p>
      <div>
        <h2>本地代理</h2>
        <p>启动后，任何 OpenAI / Claude 兼容客户端都可以连到本机。</p>
      </div>
    </div>

    <div class="proxy-actions">
      <button class="primary-action" disabled>服务运行中</button>
      <div class="port-control">
        <label for="proxy-port">
          <span>监听端口</span>
          <input id="proxy-port" type="number" v-model.number="portInput" min="1024" max="65535" />
        </label>
        <button class="line-action" :disabled="busy" @click="applyPort">应用</button>
      </div>
    </div>
    <p v-if="proxy?.restart_required" class="notice warn">
      新端口 {{ proxy.port }} 已保存，重启服务后生效。当前仍在 {{ proxy.active_port }} 端口运行。
    </p>

    <div class="endpoint-grid">
      <div>
        <span class="label">OpenAI</span>
        <code>{{ proxyBase }}/v1</code>
      </div>
      <div>
        <span class="label">Responses</span>
        <code>{{ proxyBase }}/v1/responses</code>
      </div>
      <div>
        <span class="label">Anthropic</span>
        <code>{{ proxyBase }}</code>
      </div>
      <div>
        <span class="label">API Key</span>
        <!-- <RouterLink class="arrow-link" to="/keys">管理密钥</RouterLink> -->
        <code>
          <a href="keys#/keys">管理密钥 ></a>
        </code>
      </div>
    </div>
  </section>

  <section class="split-panels">
    <article class="panel">
      <div class="panel-heading compact">
        <p class="section-number">03</p>
        <div>
          <h2>账号</h2>
          <p>serviceToken 过期时会自动刷新，也可以手动触发。</p>
        </div>
      </div>
      <div class="account-row">
        <span :class="['state-line', auth?.authenticated ? 'ok' : 'bad']">
          {{ auth?.authenticated ? "已认证" : "未认证" }}
        </span>
        <button class="line-action" :disabled="busy || !auth?.authenticated" @click="refreshAuth">
          刷新令牌
        </button>
        <button class="line-action" :disabled="busy || !auth?.authenticated" @click="refreshQuota">
          刷新额度
        </button>
        <button class="line-action danger" :disabled="busy || !auth?.authenticated" @click="logout">
          退出
        </button>
      </div>
      <RouterLink v-if="!auth?.authenticated" class="arrow-link" to="/login">去登录</RouterLink>
    </article>

    <article class="panel">
      <div class="panel-heading compact">
        <p class="section-number">04</p>
        <div>
          <h2>协议</h2>
          <p>Chat Completions 透传，Messages 和 Responses 做兼容转换。</p>
        </div>
      </div>
      <ul class="protocol-list">
        <li><span>Chat</span><code>/v1/chat/completions</code></li>
        <li><span>Responses</span><code>/v1/responses</code></li>
        <li><span>Anthropic</span><code>/v1/messages</code></li>
      </ul>
    </article>
  </section>

  <section class="panel">
    <div class="panel-heading">
      <p class="section-number">05</p>
      <div>
        <h2>可用模型</h2>
        <p>以下模型已在超级小爱 PC v2 通道验证。</p>
      </div>
    </div>
    <div class="model-table">
      <div v-for="(m, index) in models" :key="m.id" class="model-row">
        <span class="row-index">{{ String(index + 1).padStart(2, "0") }}</span>
        <code>{{ m.id }}</code>
        <span>{{ m.family }}</span>
      </div>
    </div>
  </section>
</template>
