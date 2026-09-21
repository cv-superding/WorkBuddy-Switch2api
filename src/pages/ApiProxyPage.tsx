import { useEffect, useState } from "react";
import { Loader2, RefreshCw, Save, Trash2, Users } from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import {
  accountGroupLabel,
  type AccountMeta,
  type ProxyConfig,
  type ProxyStatus,
  type ProxyUsage,
  type UsageBucket,
} from "@/lib/types";
import { cn } from "@/lib/utils";

const GROUP_ORDER = ["proxy", "desktop", ""] as const;

function groupAccounts(accounts: AccountMeta[]): Array<{ group: string; items: AccountMeta[] }> {
  return GROUP_ORDER.map((group) => ({
    group,
    items: accounts.filter((a) => (a.group ?? "") === group),
  })).filter((g) => g.items.length > 0);
}

function num(v?: number): string {
  return (v ?? 0).toLocaleString("zh-CN");
}

/** credit 是浮点累加值，直接 String() 会漏出 603.9699999999999 这种尾数，固定三位小数。 */
function num3(v?: number): string {
  return new Intl.NumberFormat("zh-CN", {
    minimumFractionDigits: 3,
    maximumFractionDigits: 3,
  }).format(v ?? 0);
}

function StatCard({ label, value, hint }: { label: string; value: string; hint?: string }) {
  return (
    <div className="rounded-xl border border-border bg-muted/25 px-4 py-3">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="mt-1 text-xl font-semibold tabular-nums tracking-tight">{value}</div>
      {hint ? <div className="mt-0.5 text-[11px] text-muted-foreground/70">{hint}</div> : null}
    </div>
  );
}

function bucketRow(name: string, b: UsageBucket) {
  return (
    <div key={name} className="flex items-center justify-between gap-3 px-3 py-2 text-[13px]">
      <span className="min-w-0 flex-1 truncate">{name}</span>
      <span className="shrink-0 tabular-nums text-muted-foreground">
        {num(b.requests)} 次
      </span>
      <span className="w-24 shrink-0 text-right tabular-nums text-muted-foreground">
        {num(b.promptTokens)} / {num(b.completionTokens)}
      </span>
    </div>
  );
}

export default function ApiProxyPage() {
  const [cfg, setCfg] = useState<ProxyConfig | null>(null);
  const [st, setSt] = useState<ProxyStatus | null>(null);
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [usage, setUsage] = useState<ProxyUsage | null>(null);
  const [saving, setSaving] = useState(false);
  const [resettingUsage, setResettingUsage] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);

  useEffect(() => {
    void load();
  }, []);

  async function load() {
    try {
      const [c, s, list, u] = await Promise.all([
        api.getProxyConfig(),
        api.getProxyStatus(),
        api.getAccounts(),
        api.getProxyUsage(),
      ]);
      setCfg(c);
      setSt(s);
      setAccounts(list.accounts);
      setUsage(u);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    }
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setMsg(null);
    try {
      const res = await api.saveProxyConfig(cfg);
      setMsg({ type: "ok", text: res.running ? "已保存，反代正在运行" : "已保存，反代当前未运行" });
      setSt(await api.getProxyStatus());
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  function toggleAccount(uid: string, on: boolean) {
    setCfg((prev) => {
      if (!prev) return prev;
      const next = new Set(prev.accounts);
      if (on) next.add(uid);
      else next.delete(uid);
      return { ...prev, accounts: Array.from(next) };
    });
  }

  const allAccounts = (cfg?.accounts.length ?? 0) === 0;
  const groups = groupAccounts(accounts);

  async function onResetUsage() {
    setResettingUsage(true);
    try {
      await api.resetProxyUsage();
      setUsage(await api.getProxyUsage());
      setMsg({ type: "ok", text: "用量统计已清空" });
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setResettingUsage(false);
    }
  }

  return (
    <div className="mx-auto min-w-0 w-full max-w-3xl px-4 py-6 sm:px-6 sm:py-8">
      <header className="mb-8 sm:mb-10">
        <h1 className="text-[28px] font-semibold tracking-tight">API 反代</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          把 Switch 里的账号包装成本机 OpenAI 兼容接口（<code className="rounded bg-muted px-1 py-0.5">/v1/chat/completions</code>），任何支持 OpenAI SDK 的客户端都能用。
        </p>
      </header>

      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="space-y-0 p-0">
          {cfg ? (
            <>
              <div className="mx-4 flex items-center justify-between gap-3 border-b border-border/50 py-3 sm:mx-5">
                <div className="min-w-0 flex-1">
                  <Label htmlFor="proxy-enabled" className="text-[13px] leading-4">启用 OpenAI 兼容接口</Label>
                  <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
                    保存后立即生效；Switch 启动时也会按配置自动拉起
                  </p>
                </div>
                <Switch
                  id="proxy-enabled"
                  checked={cfg.enabled}
                  onCheckedChange={(v) => setCfg({ ...cfg, enabled: v })}
                  aria-label="启用 OpenAI 兼容接口"
                />
              </div>

              <div className="mx-4 flex flex-col items-stretch justify-between gap-2 border-b border-border/50 py-3 sm:mx-5 sm:flex-row sm:items-center">
                <div className="min-w-0 flex-1">
                  <Label htmlFor="proxy-listen" className="text-[13px] leading-4">监听地址</Label>
                  <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
                    建议保持 127.0.0.1；只有受信任的内网才改成 0.0.0.0
                  </p>
                </div>
                <Input
                  id="proxy-listen"
                  value={cfg.listen}
                  onChange={(e) => setCfg({ ...cfg, listen: e.target.value })}
                  className="h-8 w-full sm:w-56"
                  placeholder="127.0.0.1:7863"
                />
              </div>

              <div className="mx-4 flex flex-col items-stretch justify-between gap-2 border-b border-border/50 py-3 sm:mx-5 sm:flex-row sm:items-center">
                <div className="min-w-0 flex-1">
                  <Label htmlFor="proxy-key" className="text-[13px] leading-4">API Key</Label>
                  <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
                    留空 = 不鉴权；设置后客户端需带 Authorization: Bearer &lt;key&gt;
                  </p>
                </div>
                <Input
                  id="proxy-key"
                  type="password"
                  value={cfg.api_key}
                  onChange={(e) => setCfg({ ...cfg, api_key: e.target.value })}
                  className="h-8 w-full sm:w-56"
                  placeholder="留空 = 不鉴权"
                />
              </div>

              <div className="mx-4 flex items-center justify-between gap-3 border-b border-border/50 py-3 sm:mx-5">
                <div className="min-w-0 flex-1">
                  <div className="text-[13px] font-medium leading-4">使用全部账号</div>
                  <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
                    关闭后按分组挑选；建议给桌面端留 1~2 个号不参与反代
                  </p>
                </div>
                <Switch
                  id="proxy-all"
                  checked={allAccounts}
                  onCheckedChange={(v) =>
                    setCfg({
                      ...cfg,
                      accounts: v ? [] : accounts.map((a) => a.uid ?? "").filter(Boolean),
                    })
                  }
                  aria-label="使用全部账号"
                />
              </div>

              {!allAccounts ? (
                <div className="mx-4 my-3 space-y-3 sm:mx-5">
                  <div className="flex items-center justify-between gap-3">
                    <div className="text-xs text-muted-foreground">
                      参与轮转 <span className="font-medium text-foreground">{cfg.accounts.length}</span> / {accounts.length} 个账号
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-8"
                      onClick={() =>
                        setCfg({ ...cfg, accounts: accounts.filter((a) => a.group === "proxy").map((a) => a.uid ?? "").filter(Boolean) })
                      }
                      disabled={!groups.some((g) => g.group === "proxy")}
                    >
                      <Users className="size-3.5" />
                      只选「反代API」分组
                    </Button>
                  </div>

                  {groups.map(({ group, items }) => (
                    <div key={group || "ungrouped"} className="overflow-hidden rounded-lg border border-border/60">
                      <div className="flex items-center justify-between gap-2 border-b border-border/40 bg-muted/40 px-3 py-1.5">
                        <span className="text-xs font-medium text-muted-foreground">
                          {accountGroupLabel(group)}
                        </span>
                        <span className="text-xs text-muted-foreground/60">{items.length} 个</span>
                      </div>
                      <div className="divide-y divide-border/40">
                        {items.map((a) => {
                          const uid = a.uid ?? "";
                          return (
                            <div key={a.id} className="flex items-center justify-between gap-3 px-3 py-2">
                              <div className="min-w-0 flex-1">
                                <div className="truncate text-[13px] leading-4">
                                  {a.nickname || a.email || uid}
                                </div>
                                {a.needsRelogin ? (
                                  <div className="text-xs text-amber-600">需要重新登录，不会参与</div>
                                ) : null}
                              </div>
                              <Switch
                                checked={cfg.accounts.includes(uid)}
                                onCheckedChange={(v) => toggleAccount(uid, v)}
                                disabled={!uid}
                                aria-label="参与反代"
                              />
                            </div>
                          );
                        })}
                      </div>
                    </div>
                  ))}
                </div>
              ) : null}

              <div className={cn("mx-4 flex items-center justify-between gap-3 py-3 sm:mx-5", allAccounts && "border-t border-border/50")}>
                <div className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground/80">
                  {st?.running ? (
                    <>
                      运行中 · OpenAI 基址{" "}
                      <code className="rounded bg-muted px-1 py-0.5">http://{st.listen}/v1</code>
                    </>
                  ) : (
                    <>未运行{cfg.enabled ? "（点击保存后启动）" : ""}</>
                  )}
                </div>
                <Button size="sm" onClick={() => void save()} disabled={saving}>
                  {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
                  保存
                </Button>
              </div>
            </>
          ) : (
            <div className="px-5 py-6 text-sm text-muted-foreground">加载中…</div>
          )}

          {msg ? (
            <Alert variant={msg.type === "err" ? "destructive" : "default"} className="!w-auto mx-4 my-4 sm:mx-5">
              <AlertDescription>{msg.text}</AlertDescription>
            </Alert>
          ) : null}
        </CardContent>
      </Card>

      <section className="mt-8" aria-labelledby="proxy-usage-title">
        <div className="mb-3 flex items-center justify-between gap-3">
          <div>
            <h2 id="proxy-usage-title" className="text-base font-semibold tracking-tight">反代用量</h2>
            <p className="mt-1 text-xs text-muted-foreground">只统计经过本接口的请求，存在本地 proxy-usage.json 里</p>
          </div>
          <div className="flex shrink-0 items-center gap-1.5">
            <Button
              variant="ghost"
              size="sm"
              className="h-8 px-2.5"
              onClick={() => void load()}
              aria-label="刷新用量"
            >
              <RefreshCw className="size-3.5" />
              刷新
            </Button>
            <Button
              variant="ghost"
              size="sm"
              className="h-8 px-2.5 text-destructive"
              onClick={() => void onResetUsage()}
              disabled={resettingUsage}
            >
              {resettingUsage ? <Loader2 className="size-3.5 animate-spin" /> : <Trash2 className="size-3.5" />}
              清空
            </Button>
          </div>
        </div>

        {usage ? (
          <>
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-4">
              <StatCard label="总请求" value={num(usage.total.requests)} hint={usage.total.errors ? `失败 ${num(usage.total.errors)}` : undefined} />
              <StatCard label="输入 tokens" value={num(usage.total.promptTokens)} />
              <StatCard label="输出 tokens" value={num(usage.total.completionTokens)} />
              <StatCard label="累计消耗" value={num3(usage.total.credit)} hint="上游返回的 credit" />
            </div>

            {Object.keys(usage.byAccount ?? {}).length > 0 ? (
              <div className="mt-4 overflow-hidden rounded-xl border border-border">
                <div className="flex items-center justify-between gap-3 border-b border-border/50 bg-muted/40 px-3 py-1.5 text-xs text-muted-foreground">
                  <span>按账号</span>
                  <span>请求数 · 输入/输出 tokens</span>
                </div>
                <div className="divide-y divide-border/40">
                  {Object.entries(usage.byAccount).map(([uid, b]) =>
                    bucketRow(b.nickname || uid.slice(0, 8), b),
                  )}
                </div>
              </div>
            ) : null}

            {Object.keys(usage.byModel ?? {}).length > 0 ? (
              <div className="mt-3 overflow-hidden rounded-xl border border-border">
                <div className="flex items-center justify-between gap-3 border-b border-border/50 bg-muted/40 px-3 py-1.5 text-xs text-muted-foreground">
                  <span>按模型</span>
                  <span>请求数 · 输入/输出 tokens</span>
                </div>
                <div className="divide-y divide-border/40">
                  {Object.entries(usage.byModel).map(([m, b]) => bucketRow(m, b))}
                </div>
              </div>
            ) : null}

            {(usage.recent ?? []).length > 0 ? (
              <div className="mt-3 overflow-hidden rounded-xl border border-border">
                <div className="border-b border-border/50 bg-muted/40 px-3 py-1.5 text-xs text-muted-foreground">
                  最近 {Math.min((usage.recent ?? []).length, 8)} 条
                </div>
                <div className="divide-y divide-border/40">
                  {(usage.recent ?? []).slice(0, 8).map((r, i) => (
                    <div key={`${r.ts}-${i}`} className="flex items-center justify-between gap-3 px-3 py-2 text-[13px]">
                      <div className="min-w-0 flex-1">
                        <div className="truncate">
                          {r.nickname || r.uid.slice(0, 8)} · {r.model}
                          {r.stream ? " · 流式" : ""}
                          {r.ok ? "" : " · 失败"}
                        </div>
                        <div className="text-xs text-muted-foreground/70">
                          {new Date(r.ts).toLocaleString("zh-CN")}
                        </div>
                      </div>
                      <span className="shrink-0 tabular-nums text-muted-foreground">
                        {num(r.promptTokens)} / {num(r.completionTokens)}
                      </span>
                    </div>
                  ))}
                </div>
              </div>
            ) : null}
          </>
        ) : (
          <div className="rounded-xl border border-dashed px-4 py-8 text-center text-sm text-muted-foreground">
            暂无用量数据。调用一次接口后这里就会有记录。
          </div>
        )}
      </section>

      <p className="mt-6 px-1 text-xs leading-5 text-muted-foreground/70">
        提示：如果开了梯子（系统代理），客户端连 127.0.0.1 失败时先设置 <code className="rounded bg-muted px-1">no_proxy=127.0.0.1,localhost</code>。
      </p>
    </div>
  );
}
