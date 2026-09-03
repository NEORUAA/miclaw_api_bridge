<script setup lang="ts">
import { onMounted, ref } from "vue";
import { api, AuthSnapshot } from "../api";

const auth = ref<AuthSnapshot | null>(null);
const account = ref("");
const password = ref("");
const captcha = ref("");
const captchaUrl = ref<string | null>(null);
const flow = ref<"idle" | "captcha" | "two_factor" | "done" | "fail">("idle");
const options = ref<number[]>([]);
const flag = ref<number>(8);
const ticket = ref("");
const busy = ref(false);
const message = ref("");
const error = ref("");

async function refreshAuth() {
  try {
    auth.value = await api.authStatus();
    if (auth.value?.authenticated) {
      flow.value = "done";
      message.value = "";
    }
  } catch (e: any) {
    error.value = String(e);
  }
}

async function logout() {
  busy.value = true;
  try {
    await api.logout();
    auth.value = await api.authStatus();
    flow.value = "idle";
    message.value = "";
    error.value = "";
    options.value = [];
    ticket.value = "";
  } catch (e: any) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function doLogin() {
  busy.value = true;
  error.value = "";
  message.value = "";
  try {
    const res = await api.login(account.value, password.value, captcha.value || undefined);
    if (res.outcome === "authenticated") {
      flow.value = "done";
      auth.value = await api.authStatus();
      message.value = "";
    } else if (res.outcome === "two_factor_required") {
      flow.value = "two_factor";
      options.value = res.options;
      flag.value = res.options.includes(8) ? 8 : res.options[0];
      message.value = "需要二步验证。";
    } else if (res.outcome === "captcha_required") {
      flow.value = "captcha";
      captchaUrl.value = res.captcha_url;
      message.value = "需要图形验证码，请输入后重试。";
    } else {
      flow.value = "fail";
      error.value = `登录失败 (${res.code})：${res.description}`;
    }
  } catch (e: any) {
    error.value = String(e);
    flow.value = "fail";
  } finally {
    busy.value = false;
  }
}

async function sendTicket() {
  busy.value = true;
  error.value = "";
  message.value = "";
  try {
    await api.sendTicket(flag.value);
    message.value = flag.value === 4 ? "已发送短信验证码。" : "已发送邮箱验证码。";
  } catch (e: any) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

async function verify() {
  busy.value = true;
  error.value = "";
  try {
    await api.verifyTicket(flag.value, ticket.value);
    flow.value = "done";
    auth.value = await api.authStatus();
    message.value = "";
  } catch (e: any) {
    error.value = String(e);
  } finally {
    busy.value = false;
  }
}

onMounted(refreshAuth);
</script>

<template>
  <section class="login-grid">
    <div class="panel login-form">
      <div class="panel-heading">
        <p class="section-number">02</p>
        <div>
          <h2>OAuth</h2>
          <p>使用 miclaw 权限小米账号登录，凭证只写入系统 Keychain。</p>
        </div>
      </div>

      <div v-if="auth?.authenticated" class="signed-in">
        <span class="state-line ok">已登录 {{ auth.nick ?? auth.user_id ?? "" }}</span>
        <p v-if="auth.refreshed_at">最后刷新：{{ new Date(auth.refreshed_at).toLocaleString() }}</p>
        <button class="primary-action danger" :disabled="busy" @click="logout">退出登录</button>
      </div>

      <form v-else class="form-stack" @submit.prevent="doLogin">
        <label>
          <span>账号 / 邮箱 / 手机号</span>
          <input v-model="account" autocomplete="username" placeholder="user@example.com" />
        </label>
        <label>
          <span>密码</span>
          <input type="password" v-model="password" autocomplete="current-password" />
        </label>
        <label v-if="flow === 'captcha'">
          <span>图形验证码</span>
          <img v-if="captchaUrl" class="captcha" :src="captchaUrl" alt="captcha" />
          <input v-model="captcha" />
        </label>
        <button class="primary-action" :disabled="busy || !account || !password" type="submit">
          登录
        </button>
      </form>

      <p v-if="message" class="notice ok">{{ message }}</p>
      <p v-if="error" class="notice bad">{{ error }}</p>
    </div>

    <aside class="panel auth-steps">
      <div class="panel-heading compact">
        <p class="section-number">03</p>
        <div>
          <h2>令牌流程</h2>
          <p>桥接器会自动换取超级小爱使用的 miclaw serviceToken。</p>
        </div>
      </div>
      <ol>
        <li><span>01</span><strong>账号认证</strong><p>密码登录与二步验证。</p></li>
        <li><span>02</span><strong>sid=miclaw</strong><p>用 passToken 换服务令牌。</p></li>
        <li><span>03</span><strong>超级小爱 v2</strong><p>本地代理携带 serviceToken 请求。</p></li>
      </ol>
    </aside>
  </section>

  <section class="panel" v-if="!auth?.authenticated && flow === 'two_factor'">
    <div class="panel-heading">
      <p class="section-number">04</p>
      <div>
        <h2>二步验证</h2>
        <p>选择验证码通道，收到验证码后完成登录。</p>
      </div>
    </div>
    <div class="two-factor-row">
      <label>
        <span>验证方式</span>
        <select v-model.number="flag">
          <option v-for="o in options" :key="o" :value="o">
            {{ o === 4 ? "短信" : o === 8 ? "邮箱" : `flag=${o}` }}
          </option>
        </select>
      </label>
      <button class="line-action" :disabled="busy" @click="sendTicket">发送验证码</button>
      <label>
        <span>验证码</span>
        <input v-model="ticket" inputmode="numeric" />
      </label>
      <button class="primary-action" :disabled="busy || !ticket" @click="verify">完成验证</button>
    </div>
  </section>
</template>
