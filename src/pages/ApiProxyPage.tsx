import { useEffect, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { Check, Copy, ListPlus, Loader2, RefreshCw, Save, Trash2, Users } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import {
  accountGroupLabel,
  EDITIONS,
  type AccountMeta,
  type ProxyConfig,
  type ProxyEndpoint,
  type ProxyEndpointStatus,
  type ProxyModels,
  type ProxyStatus,
  type ProxyUsage,
  type UsageBucket,
} from "@/lib/types";
import { cn } from "@/lib/utils";

type AccountGroup = {
  key: string;
  label: string;
  items: AccountMeta[];
  /** 这一组本身就是按版本拆出来的，行内不用再标版本。 */
  byEdition: boolean;
};

/**
 * 账号分组。
 *
 * 「反代API」这一组**必须按版本再拆开**：国内版和国际版是两套上游，
 * 模型列表不一样，混在一起既看不出谁是谁，也说不清某个模型该由谁来接。
 * 其它组不拆，只在每行标一个版本徽标。
 *
 * 反代这两节只要有一档有人就都显示（另一档显示 0 个）——
 * 这样「我没国际版账号」这件事是看得见的，而不是悄悄少一截。
 */
function groupAccounts(accounts: AccountMeta[]): AccountGroup[] {
  const pick = (group: string, edition?: string) =>
    accounts.filter(
      (a) =>
        (a.group ?? "") === group &&
        (edition === undefined || (a.edition ?? "domestic") === edition),
    );

  const out: AccountGroup[] = [];
  const proxyTotal = pick("proxy").length;
  for (const e of EDITIONS) {
    out.push({
      key: `proxy|${e.value}`,
      label: `反代API · ${e.label}`,
      items: pick("proxy", e.value),
      byEdition: true,
    });
  }
  if (proxyTotal === 0) {
    out.splice(0, out.length);
  }
  out.push({
    key: "desktop",
    label: accountGroupLabel("desktop"),
    items: pick("desktop"),
    byEdition: false,
  });
  out.push({
    key: "ungrouped",
    label: accountGroupLabel(""),
    items: pick(""),
    byEdition: false,
  });
  return out.filter((g) => g.byEdition || g.items.length > 0);
}

/** 一行里显示的版本徽标（分组本身已经按版本拆了就不用再标）。 */
/** 本页那个入口的配置区：版本标识 + 开关、监听地址、API Key。
 *
 * 刻意**不带自己的外框** —— 它渲染在页面那张 `Card` 里面，再套一层边框只会
 * 显得嵌套。分隔线由各行自己给，和页面里其它设置行保持一致。
 */
function EndpointSection({
  edition,
  endpoint,
  status,
  onChange,
}: {
  edition: "domestic" | "international";
  endpoint: ProxyEndpoint;
  status?: ProxyEndpointStatus;
  onChange: (next: ProxyEndpoint) => void;
}) {
  const label = edition === "domestic" ? "国内版" : "国际版";
  const hint =
    edition === "domestic"
      ? "使用 .workbuddy 的账号 · www.codebuddy.cn"
      : "使用 .workbuddy-ai 的账号 · www.workbuddy.ai";
  const listenPlaceholder = edition === "domestic" ? "127.0.0.1:7863" : "127.0.0.1:7864";

  return (
    <>
      <div className="mx-4 flex items-center justify-between gap-3 border-b border-border/50 py-3 sm:mx-5">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <Label htmlFor={`proxy-enabled-${edition}`} className="text-[13px] leading-4">
              启用{label}接口
            </Label>
            {status?.running ? (
              <Badge
                variant="secondary"
                className="h-5 shrink-0 border-0 px-1.5 text-[10px] leading-5 text-emerald-600"
              >
                监听中
              </Badge>
            ) : null}
          </div>
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">{hint}</p>
        </div>
        <Switch
          id={`proxy-enabled-${edition}`}
          checked={endpoint.enabled}
          onCheckedChange={(v) => onChange({ ...endpoint, enabled: v })}
          aria-label={`启用${label}反代`}
        />
      </div>

      <div className="mx-4 flex flex-col items-stretch justify-between gap-2 border-b border-border/50 py-3 sm:mx-5 sm:flex-row sm:items-center">
        <div className="min-w-0 flex-1">
          <Label htmlFor={`proxy-listen-${edition}`} className="text-[13px] leading-4">
            监听地址
          </Label>
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
            建议保持 127.0.0.1；只有受信任的内网才改成 0.0.0.0
          </p>
        </div>
        <Input
          id={`proxy-listen-${edition}`}
          value={endpoint.listen}
          onChange={(e) => onChange({ ...endpoint, listen: e.target.value })}
          className="h-8 w-full sm:w-56"
          placeholder={listenPlaceholder}
        />
      </div>

      <div className="mx-4 flex flex-col items-stretch justify-between gap-2 border-b border-border/50 py-3 sm:mx-5 sm:flex-row sm:items-center">
        <div className="min-w-0 flex-1">
          <Label htmlFor={`proxy-key-${edition}`} className="text-[13px] leading-4">
            API Key
          </Label>
          <p className="mt-0.5 text-xs leading-4 text-muted-foreground/75">
            留空 = 不鉴权；设置后客户端需带 Authorization: Bearer &lt;key&gt;
          </p>
        </div>
        <Input
          id={`proxy-key-${edition}`}
          type="password"
          value={endpoint.api_key}
          onChange={(e) => onChange({ ...endpoint, api_key: e.target.value })}
          className="h-8 w-full sm:w-56"
          placeholder="留空 = 不鉴权"
        />
      </div>
    </>
  );
}

function editionLabelOf(a: AccountMeta): string {
  return EDITIONS.find((e) => e.value === (a.edition ?? "domestic"))?.label ?? "国内版";
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

export default function ApiProxyPage({
  edition,
}: {
  /** 本页服务哪一版。国内版与国际版是**两个独立页面**，各有自己的监听地址与 API Key。 */
  edition: "domestic" | "international";
}) {
  const [params] = useSearchParams();
  const [cfg, setCfg] = useState<ProxyConfig | null>(null);
  const [st, setSt] = useState<ProxyStatus | null>(null);
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [usage, setUsage] = useState<ProxyUsage | null>(null);
  const [saving, setSaving] = useState(false);
  const [resettingUsage, setResettingUsage] = useState(false);
  const [msg, setMsg] = useState<{ type: "ok" | "err"; text: string } | null>(null);
  // 「获取模型ID」：拉一次模型列表，弹窗展示 + 一键复制
  const [models, setModels] = useState<ProxyModels | null>(null);
  const [modelsOpen, setModelsOpen] = useState(false);
  const [loadingModels, setLoadingModels] = useState(false);
  const [copied, setCopied] = useState<string | null>(null);

  useEffect(() => {
    void load();
    // 截图/演示用：?models=1 直接展开模型列表（和迁移页的 ?tab=import 一个路子）
    if (params.get("models") === "1") void fetchModels();
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
      // 本页只管自己这一版 —— 另一版的结果不该在这里报（那边有它自己的页面）。
      const mine = res.endpoints.find((e) => e.edition === edition);
      if (mine && !mine.ok) {
        setMsg({ type: "err", text: `已保存，但${mine.label}入口未启动：${mine.error}` });
      } else if (mine?.ok) {
        setMsg({ type: "ok", text: `已保存，${mine.label}入口已启动` });
      } else {
        setMsg({ type: "ok", text: "已保存（本入口未启用）" });
      }
      setSt(await api.getProxyStatus());
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setSaving(false);
    }
  }

  function toggleAccount(uid: string, on: boolean) {
    const next = new Set(ep?.accounts ?? []);
    if (on) next.add(uid);
    else next.delete(uid);
    patchEndpoint({ accounts: Array.from(next) });
  }

  /** 拉一次可用模型列表：优先问本机反代，拿不到再直连上游。 */
  async function fetchModels() {
    setLoadingModels(true);
    setMsg(null);
    try {
      // 只拉本页这一版的模型 —— 另一版的模型在这个入口上用不了，列出来只会误导。
      const res = await api.getProxyModels(edition);
      setModels(res);
      setModelsOpen(true);
    } catch (e) {
      setMsg({ type: "err", text: api.asError(e) });
    } finally {
      setLoadingModels(false);
    }
  }

  async function copyText(text: string, key: string) {
    try {
      await navigator.clipboard.writeText(text);
      setCopied(key);
      setTimeout(() => setCopied((cur) => (cur === key ? null : cur)), 1600);
    } catch {
      toast.error("复制失败", { description: "可以手动选中后 Ctrl+C" });
    }
  }

  const label = edition === "domestic" ? "国内版" : "国际版";
  /** 本页服务的那一个入口。 */
  const ep = cfg?.[edition];
  const epStatus = st?.endpoints.find((e) => e.edition === edition);
  /** 本页只关心**这一版**的账号 —— 另一版的号在这个入口上根本用不了。 */
  const editionAccounts = accounts.filter((a) => (a.edition ?? "domestic") === edition);
  const allAccounts = (ep?.accounts.length ?? 0) === 0;
  const groups = groupAccounts(editionAccounts);

  /** 改本页那个入口的字段。
   *
   * 两个入口是独立的，写回时必须指明改的是哪一边。用 `{ ...cfg, [edition]: ... }`
   * 这种计算属性在 TS 里会退化成索引签名、丢掉类型，所以显式分两支写。
   */
  function patchEndpoint(patch: Partial<ProxyEndpoint>) {
    setCfg((prev) =>
      prev
        ? {
            ...prev,
            domestic: edition === "domestic" ? { ...prev.domestic, ...patch } : prev.domestic,
            international:
              edition === "international"
                ? { ...prev.international, ...patch }
                : prev.international,
          }
        : prev,
    );
  }

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
        <h1 className="text-[28px] font-semibold tracking-tight">{label} API 反代</h1>
        <p className="mt-2 text-sm leading-6 text-muted-foreground">
          把 Switch 里的{label}账号包装成本机 OpenAI 兼容接口（<code className="rounded bg-muted px-1 py-0.5">/v1/chat/completions</code>），任何支持 OpenAI SDK 的客户端都能用。
        </p>
      </header>

      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="space-y-0 p-0">
          {cfg ? (
            <>
              <EndpointSection
                edition={edition}
                endpoint={cfg[edition]}
                status={epStatus}
                onChange={patchEndpoint}
              />

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
                    patchEndpoint({
                      accounts: v ? [] : editionAccounts.map((a) => a.uid ?? "").filter(Boolean),
                    })
                  }
                  aria-label="使用全部账号"
                />
              </div>

              {!allAccounts ? (
                <div className="mx-4 my-3 space-y-3 sm:mx-5">
                  <div className="flex items-center justify-between gap-3">
                    <div className="text-xs text-muted-foreground">
                      参与轮转 <span className="font-medium text-foreground">{ep?.accounts.length ?? 0}</span> / {editionAccounts.length} 个账号
                    </div>
                    <Button
                      variant="outline"
                      size="sm"
                      className="h-8"
                      onClick={() =>
                        patchEndpoint({
                          accounts: editionAccounts
                            .filter((a) => a.group === "proxy")
                            .map((a) => a.uid ?? "")
                            .filter(Boolean),
                        })
                      }
                      disabled={!editionAccounts.some((a) => a.group === "proxy")}
                    >
                      <Users className="size-3.5" />
                      只选「反代API」分组
                    </Button>
                  </div>

                  {groups.map((g) => (
                    <div key={g.key} className="overflow-hidden rounded-lg border border-border/60">
                      <div className="flex items-center justify-between gap-2 border-b border-border/40 bg-muted/40 px-3 py-1.5">
                        <span className="text-xs font-medium text-muted-foreground">{g.label}</span>
                        <span className="text-xs text-muted-foreground/60">{g.items.length} 个</span>
                      </div>
                      <div className="divide-y divide-border/40">
                        {g.items.map((a) => {
                          const uid = a.uid ?? "";
                          return (
                            <div key={a.id} className="flex items-center justify-between gap-3 px-3 py-2">
                              <div className="min-w-0 flex-1">
                                <div className="flex items-center gap-1.5">
                                  <span className="truncate text-[13px] leading-4">
                                    {a.nickname || a.email || uid}
                                  </span>
                                  {!g.byEdition ? (
                                    <Badge
                                      variant="secondary"
                                      className="h-4 shrink-0 border-0 px-1 text-[10px] leading-4"
                                    >
                                      {editionLabelOf(a)}
                                    </Badge>
                                  ) : null}
                                </div>
                                {a.needsRelogin ? (
                                  <div className="text-xs text-amber-600">需要重新登录，不会参与</div>
                                ) : null}
                              </div>
                              <Switch
                                checked={(ep?.accounts ?? []).includes(uid)}
                                onCheckedChange={(v) => toggleAccount(uid, v)}
                                disabled={!uid}
                                aria-label="参与反代"
                              />
                            </div>
                          );
                        })}
                        {g.items.length === 0 ? (
                          <div className="px-3 py-2 text-xs text-muted-foreground/70">
                            {g.byEdition
                              ? `账号库里没有这一档的号 —— 它那套模型也拉不到`
                              : "没有账号"}
                          </div>
                        ) : null}
                      </div>
                    </div>
                  ))}
                </div>
              ) : null}

              <div className={cn("mx-4 flex items-center justify-between gap-3 py-3 sm:mx-5", allAccounts && "border-t border-border/50")}>
                <div className="min-w-0 flex-1 text-xs leading-5 text-muted-foreground/80">
                  {epStatus?.running ? (
                    <>
                      运行中 · OpenAI 基址{" "}
                      <code className="rounded bg-muted px-1 py-0.5">http://{epStatus.listen}/v1</code>
                    </>
                  ) : (
                    <>未运行{ep?.enabled ? "（点击保存后启动）" : ""}</>
                  )}
                </div>
                <div className="flex shrink-0 items-center gap-2">
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => void fetchModels()}
                    disabled={loadingModels}
                    title="拉一次可用模型列表，客户端 model 字段照着填"
                  >
                    {loadingModels ? (
                      <Loader2 className="size-3.5 animate-spin" />
                    ) : (
                      <ListPlus className="size-3.5" />
                    )}
                    获取模型ID
                  </Button>
                  <Button size="sm" onClick={() => void save()} disabled={saving}>
                    {saving ? <Loader2 className="size-3.5 animate-spin" /> : <Save className="size-3.5" />}
                    保存
                  </Button>
                </div>
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

      <Dialog open={modelsOpen} onOpenChange={setModelsOpen}>
        <DialogContent className="max-w-2xl">
          <DialogHeader>
            <DialogTitle>可用模型 ID（按版本分开）</DialogTitle>
            <DialogDescription>
              国内版和国际版是两套上游，模型列表不一样，所以分档列。客户端里的{" "}
              <code className="rounded bg-muted px-1">model</code> 字段填下面任意一个 ——
              反代会按这个 ID 自动挑对应版本的账号。
            </DialogDescription>
          </DialogHeader>

          <div className="max-h-[52vh] space-y-3 overflow-y-auto">
            {(models?.editions ?? []).map((g) => (
              <div key={g.edition} className="overflow-hidden rounded-lg border border-border/60">
                <div className="flex items-center justify-between gap-2 border-b border-border/40 bg-muted/40 px-3 py-1.5">
                  <span className="text-xs font-medium text-muted-foreground">
                    {g.label}
                    {g.source ? ` · ${g.source}` : ""}
                  </span>
                  <div className="flex items-center gap-2">
                    <span className="text-xs text-muted-foreground/60">{g.models.length} 个</span>
                    {g.models.length > 0 ? (
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-1.5 text-[11px]"
                        onClick={() => void copyText(g.models.join("\n"), `__all__${g.edition}`)}
                      >
                        {copied === `__all__${g.edition}` ? (
                          <Check className="size-3" />
                        ) : (
                          <Copy className="size-3" />
                        )}
                        复制
                      </Button>
                    ) : null}
                  </div>
                </div>
                {g.models.length > 0 ? (
                  <div className="divide-y divide-border/40">
                    {g.models.map((id) => (
                      <div key={`${g.edition}-${id}`} className="flex items-center justify-between gap-3 px-3 py-2">
                        <code className="min-w-0 flex-1 truncate text-[13px]">{id}</code>
                        <Button
                          variant="ghost"
                          size="sm"
                          className="h-7 shrink-0 px-2"
                          onClick={() => void copyText(id, `${g.edition}-${id}`)}
                          aria-label={`复制 ${id}`}
                        >
                          {copied === `${g.edition}-${id}` ? (
                            <Check className="size-3.5" />
                          ) : (
                            <Copy className="size-3.5" />
                          )}
                        </Button>
                      </div>
                    ))}
                  </div>
                ) : (
                  <div className="px-3 py-2.5 text-xs leading-5 text-amber-600">
                    {g.error ?? "没拉到"}
                  </div>
                )}
                {g.sourceUrl ? (
                  <div className="border-t border-border/40 px-3 py-1.5 text-[11px] text-muted-foreground/70">
                    数据来自 <code className="break-all">{g.sourceUrl}</code>
                  </div>
                ) : null}
              </div>
            ))}
            {models && (models.editions?.length ?? 0) === 0 ? (
              <div className="px-3 py-2 text-xs text-muted-foreground">没有拉到任何模型。</div>
            ) : null}
          </div>

          <p className="text-xs leading-5 text-muted-foreground">
            列表外的 ID 也能直接发（上游支持就行）。两版都会各问一次上游，所以国际版账号缺失时那一档会标出原因。
          </p>

          <DialogFooter>
            <Button
              variant="outline"
              size="sm"
              onClick={() => void copyText((models?.models ?? []).join("\n"), "__all__")}
            >
              {copied === "__all__" ? <Check className="size-3.5" /> : <Copy className="size-3.5" />}
              复制全部（两版并集）
            </Button>
            <Button size="sm" onClick={() => setModelsOpen(false)}>
              关闭
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
