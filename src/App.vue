<script setup lang="ts">
import { computed, onMounted, ref, watch, watchEffect } from "vue";
import { RouterLink, RouterView, useRoute } from "vue-router";
import appIcon from "../src-tauri/icons/icon.png";

type Theme = "light" | "dark";

const route = useRoute();
const theme = ref<Theme>("light");
const patternLetters = ["M", "I", "M", "O"];
const patternRows = Array.from({ length: 8 }, (_, row) =>
  Array.from({ length: 18 }, (_, col) => patternLetters[(col + (row % 2)) % patternLetters.length]),
);

const pageTitle = computed(() => {
  if (route.path.includes("admin-login")) return "后台登录";
  if (route.path.includes("login")) return "小米账号";
  if (route.path.includes("logs")) return "代理日志";
  if (route.path.includes("keys")) return "API 密钥";
  if (route.path.includes("usage")) return "用量统计";
  return "本地代理";
});

const isAuthGate = computed(() => route.path.includes("admin-login"));

// Password-setup reminder banner. It appears once the user chose "暂不设置"
// on the setup page (skipPwSetup flag) and a password is still not set. The
// first close swaps the prompt for a how-to hint; the second close dismisses
// it for good (pwBannerDismissed flag).
const SKIP_KEY = "miclaw.skipPwSetup";
const DISMISS_KEY = "miclaw.pwBannerDismissed";
const skipActive = ref(localStorage.getItem(SKIP_KEY) === "1");
const bannerClosed = ref(localStorage.getItem(DISMISS_KEY) === "1");
const bannerStage = ref(0); // 0 = prompt, 1 = how-to hint

const showBanner = computed(() => skipActive.value && !bannerClosed.value && !isAuthGate.value);

function refreshBannerFlags() {
  skipActive.value = localStorage.getItem(SKIP_KEY) === "1";
  bannerClosed.value = localStorage.getItem(DISMISS_KEY) === "1";
}

function dismissBanner() {
  if (bannerStage.value === 0) {
    bannerStage.value = 1;
  } else {
    localStorage.setItem(DISMISS_KEY, "1");
    bannerClosed.value = true;
  }
}

// localStorage is not reactive; re-read whenever navigation happens (the skip
// action navigates to /dashboard, which triggers this).
watch(() => route.path, refreshBannerFlags);

function applyTheme(next: Theme) {
  theme.value = next;
  localStorage.setItem("miclaw-theme", next);
}

function toggleTheme() {
  applyTheme(theme.value === "dark" ? "light" : "dark");
}

onMounted(() => {
  const saved = localStorage.getItem("miclaw-theme") as Theme | null;
  if (saved === "light" || saved === "dark") {
    theme.value = saved;
    return;
  }
  theme.value = window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
});

watchEffect(() => {
  document.documentElement.dataset.theme = theme.value;
});
</script>

<template>
  <div class="app-shell" :class="{ 'has-banner': showBanner }">
    <div v-if="showBanner" class="pw-banner" role="status">
      <span class="pw-banner-text" v-if="bannerStage === 0">
        管理后台尚未设置密码，建议
        <RouterLink to="/admin-login">设置管理后台密码</RouterLink>
      </span>
      <span class="pw-banner-text" v-else>
        需要时在地址栏访问 <code>#/admin-login</code> 即可进入设置页。
      </span>
      <button
        class="pw-banner-close"
        type="button"
        :aria-label="bannerStage === 0 ? '关闭提示' : '彻底关闭提示'"
        @click="dismissBanner"
      >
        ×
      </button>
    </div>

    <header class="topbar">
      <RouterLink class="brand-lockup" to="/dashboard" aria-label="miclaw_api_bridge dashboard">
        <img :src="appIcon" alt="" />
        <span>miclaw_api_bridge</span>
      </RouterLink>

      <nav class="topnav" aria-label="主导航" v-if="!isAuthGate">
        <RouterLink to="/dashboard">Proxy</RouterLink>
        <RouterLink to="/login">Account</RouterLink>
        <RouterLink to="/keys">Keys</RouterLink>
        <RouterLink to="/usage">Usage</RouterLink>
        <RouterLink to="/logs">Logs</RouterLink>
      </nav>

      <a
        class="icon-button github-link"
        href="https://github.com/NEORUAA/miclaw_api_bridge"
        target="_blank"
        rel="noopener noreferrer"
        title="GitHub"
        aria-label="在 GitHub 上查看"
      >
        <svg class="github-mark" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M12 .5C5.7.5.5 5.7.5 12a11.5 11.5 0 0 0 7.9 10.9c.6.1.8-.2.8-.5v-1.8c-3.2.7-3.9-1.5-3.9-1.5-.5-1.3-1.3-1.7-1.3-1.7-1.1-.7.1-.7.1-.7 1.2.1 1.8 1.2 1.8 1.2 1 .1.8 1.7 2.6 1.2.1-.7.4-1.2.7-1.5-2.5-.3-5.2-1.3-5.2-5.7 0-1.3.5-2.3 1.2-3.1-.1-.3-.5-1.5.1-3.1 0 0 1-.3 3.3 1.2a11.5 11.5 0 0 1 6 0c2.3-1.5 3.3-1.2 3.3-1.2.6 1.6.2 2.8.1 3.1.8.8 1.2 1.8 1.2 3.1 0 4.4-2.7 5.4-5.2 5.7.4.3.8 1 .8 2.1v3.1c0 .3.2.6.8.5A11.5 11.5 0 0 0 23.5 12C23.5 5.7 18.3.5 12 .5Z" />
        </svg>
      </a>

      <button
        class="icon-button"
        type="button"
        :title="theme === 'dark' ? '切换浅色模式' : '切换深色模式'"
        :aria-label="theme === 'dark' ? '切换浅色模式' : '切换深色模式'"
        @click="toggleTheme"
      >
        <svg v-if="theme === 'dark'" viewBox="0 0 24 24" aria-hidden="true">
          <path d="M12 3v2.1M12 18.9V21M4.4 4.4l1.5 1.5M18.1 18.1l1.5 1.5M3 12h2.1M18.9 12H21M4.4 19.6l1.5-1.5M18.1 5.9l1.5-1.5" />
          <circle cx="12" cy="12" r="4.2" />
        </svg>
        <svg v-else viewBox="0 0 24 24" aria-hidden="true">
          <path d="M20.4 14.2A7.7 7.7 0 0 1 9.8 3.6 8.7 8.7 0 1 0 20.4 14.2Z" />
        </svg>
      </button>
    </header>

    <section class="page-hero" aria-label="当前页面">
      <div class="pattern" aria-hidden="true">
        <span v-for="(row, rowIndex) in patternRows" :key="rowIndex" class="pattern-row">
          <b v-for="(letter, colIndex) in row" :key="`${rowIndex}-${colIndex}`">{{ letter }}</b>
        </span>
      </div>
      <p class="section-number">01</p>
      <h1>{{ pageTitle }}</h1>
      <p class="hero-copy">将超级小爱中的 Xiaomi MiMo 模型转接成本地 OpenAI / Anthropic 兼容端点。</p>
    </section>

    <main class="content">
      <RouterView />
    </main>
  </div>
</template>
