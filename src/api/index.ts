export interface AuthSnapshot {
  authenticated: boolean;
  nick: string | null;
  user_id: string | null;
  refreshed_at: number | null;
}

export type LoginOutcome =
  | { outcome: "authenticated"; nick: string | null }
  | { outcome: "two_factor_required"; options: number[] }
  | { outcome: "captcha_required"; captcha_url: string }
  | { outcome: "failed"; code: number; description: string };

export interface ProxySnapshot {
  running: boolean;
  addr: string | null;
  port: number;
  active_port: number | null;
  restart_required: boolean;
}

export interface ModelInfo {
  id: string;
  object: string;
  owned_by: string;
  family: string;
}

export interface QuotaSnapshot {
  membership_level: number;
  membership_level_name: string | null;
  membership_type: string | null;
  membership_expire_at: number | null;
  auto_renewal: boolean | null;
  points_limit: number;
  points_used: number;
  points_remaining: number;
  usage_ratio: number;
  quota_reset_at: number;
  can_upgrade: boolean;
  abnormal: boolean;
  status: "available" | "low" | "exhausted";
  observed_at: number;
}

export interface AdminSession {
  configured: boolean;
  authenticated: boolean;
}

export interface ApiKeyView {
  id: string;
  name: string;
  prefix: string;
  created_at: number;
  last_used: number | null;
}

export interface UsageModelBreakdown {
  prompt: number;
  completion: number;
  total: number;
}

export interface UsageBucket {
  t: number;
  total: number;
  models: Record<string, UsageModelBreakdown>;
}

export interface UsageReport {
  window: string;
  bucket_seconds: number;
  start: number;
  end: number;
  grand_total: number;
  model_totals: Record<string, number>;
  buckets: UsageBucket[];
}

async function request<T>(path: string, init?: RequestInit): Promise<T> {
  const resp = await fetch(path, {
    credentials: "same-origin",
    ...init,
    headers: {
      "content-type": "application/json",
      ...(init?.headers ?? {}),
    },
  });
  const text = await resp.text();
  const data = text ? JSON.parse(text) : null;
  if (!resp.ok) {
    const message = data?.error?.message ?? `${resp.status} ${resp.statusText}`;
    const err = new Error(message) as Error & { status?: number };
    err.status = resp.status;
    throw err;
  }
  return data as T;
}

function post<T>(path: string, body?: unknown) {
  return request<T>(path, {
    method: "POST",
    body: JSON.stringify(body ?? {}),
  });
}

export const api = {
  authStatus: () => request<AuthSnapshot>("/api/auth/status"),
  login: (account: string, password: string, captcha?: string) =>
    post<LoginOutcome>("/api/auth/login", { account, password, captcha }),
  sendTicket: (flag: number) => post<boolean>("/api/auth/two-factor/send", { flag }),
  verifyTicket: (flag: number, ticket: string) =>
    post<void>("/api/auth/two-factor/verify", { flag, ticket }),
  refreshSession: () => post<AuthSnapshot>("/api/auth/refresh"),
  logout: () => post<void>("/api/auth/logout"),
  proxyStatus: () => request<ProxySnapshot>("/api/proxy/status"),
  setProxyPort: (port: number) => post<ProxySnapshot>("/api/settings/port", { port }),
  listModels: () => request<ModelInfo[]>("/api/models"),
  quota: () => request<QuotaSnapshot>("/api/quota"),
  getVerboseLogs: () => request<{ enabled: boolean }>("/api/logs/verbose"),
  setVerboseLogs: (enabled: boolean) =>
    post<{ enabled: boolean }>("/api/logs/verbose", { enabled }),

  // admin password auth
  adminSession: () => request<AdminSession>("/api/admin/session"),
  adminSetup: (password: string) => post<{ ok: boolean }>("/api/admin/setup", { password }),
  adminLogin: (password: string) => post<{ ok: boolean }>("/api/admin/login", { password }),
  adminLogout: () => post<{ ok: boolean }>("/api/admin/logout"),
  changePassword: (old_password: string, new_password: string) =>
    post<{ ok: boolean }>("/api/admin/password", { old_password, new_password }),

  // api keys
  listKeys: () => request<ApiKeyView[]>("/api/keys"),
  createKey: (name: string) =>
    post<{ key: ApiKeyView; secret: string }>("/api/keys", { name }),
  deleteKey: (id: string) => request<{ ok: boolean }>(`/api/keys/${id}`, { method: "DELETE" }),
  getApiKeyRequired: () => request<{ required: boolean }>("/api/settings/api-key-required"),
  setApiKeyRequired: (required: boolean) =>
    post<{ required: boolean }>("/api/settings/api-key-required", { required }),

  // usage stats
  usage: (window: string) => request<UsageReport>(`/api/usage?window=${window}`),
};
