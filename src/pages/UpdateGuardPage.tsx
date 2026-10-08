import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  Ban,
  CheckCircle2,
  FolderOpen,
  Loader2,
  Package,
  RefreshCw,
  ShieldCheck,
  Trash2,
} from "lucide-react";
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
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import type { UpdateGuardInstall, UpdateGuardStatus } from "@/lib/types";
import { cn } from "@/lib/utils";

/** 一个安装目录的卡片：身份 / 版本 / 目录里的 exe / 身份不符的提醒。 */
function InstallRow({ info }: { info: UpdateGuardInstall }) {
  const overseas = info.isOversea === true;
  return (
    <div className="py-3">
      <div className="flex items-center gap-2">
        <span className="text-[13px] font-medium leading-5">{info.label}</span>
        <Badge
          variant="secondary"
          className={cn(
            "h-5 border-0 px-1.5 text-[11px]",
            overseas ? "bg-violet-50 text-violet-700" : "bg-sky-50 text-sky-700",
          )}
        >
          {overseas ? "国际版身份" : "国内版身份"}
        </Badge>
        {info.version && (
          <span className="text-[11px] tabular-nums text-muted-foreground">{info.version}</span>
        )}
      </div>
      <div className="mt-1 break-all font-mono text-[11px] text-muted-foreground">{info.dir}</div>
      <div className="mt-1 flex flex-wrap items-center gap-x-3 gap-y-1 text-[11px] text-muted-foreground">
        {info.dataDirName && (
          <span>
            数据目录 <code className="rounded bg-muted px-1">{info.dataDirName}</code>
          </span>
        )}
        {info.exes.length > 0 && (
          <span>
            启动器 <code className="rounded bg-muted px-1">{info.exes.join(" / ")}</code>
          </span>
        )}
      </div>
      {info.warning && (
        <div className="mt-2 flex items-start gap-1.5 rounded-md border border-amber-200 bg-amber-50/70 px-2.5 py-2 text-[11px] leading-5 text-amber-800">
          <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
          <span>{info.warning}</span>
        </div>
      )}
    </div>
  );
}

export default function UpdateGuardPage() {
  const demo = api.isDemoMode();
  const [status, setStatus] = useState<UpdateGuardStatus | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 开启前的确认弹窗（要改注册表，值得先讲清楚）。 */
  const [confirmOpen, setConfirmOpen] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      setStatus(await api.updateGuardStatus());
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load();
  }, [load]);

  const disabled = status?.disabled ?? false;
  const cacheActive = status?.cacheActive ?? false;
  const installs = useMemo(() => status?.installs ?? [], [status]);
  /** 身份与目录名对不上的安装 —— 也就是「国内版快捷方式跑出国际版」这种。 */
  const suspicious = useMemo(() => installs.filter((i) => i.warning), [installs]);

  async function apply(disabledNext: boolean, clearCache: boolean) {
    setBusy(true);
    setError(null);
    try {
      const r = await api.updateGuardSet(disabledNext, clearCache);
      setStatus(r.status);
      if (disabledNext) {
        const extra = r.quarantined.length
          ? `，并隔离了 ${r.quarantined.length} 个已下载的包（${r.freedText}）`
          : "";
        toast.success("已禁止自动更新", {
          description: `更新源已指向黑洞${extra}。重启两个应用后生效。`,
        });
      } else {
        toast.success("已恢复自动更新", { description: "重启两个应用后生效。" });
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function clearCache() {
    setBusy(true);
    setError(null);
    try {
      const r = await api.updateGuardClearCache();
      setStatus(r.status);
      if (r.quarantined.length) {
        toast.success(`已隔离 ${r.quarantined.length} 个更新包`, {
          description: `释放 ${r.freedText}。文件只是改了名，没有删除。`,
        });
      } else {
        toast.message("缓存里没有待处理的更新包");
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  }

  async function openCacheDir() {
    if (!status) return;
    if (demo) {
      toast.message("演示模式不打开目录", { description: status.cacheDir });
      return;
    }
    try {
      await api.cacheMoveOpen(status.cacheDir);
    } catch (e) {
      toast.error("打不开这个目录", { description: e instanceof Error ? e.message : String(e) });
    }
  }

  const header = (
    <header>
      <h1 className="text-[28px] font-semibold tracking-tight">更新防护</h1>
      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        WorkBuddy 的更新缓存是<span className="font-medium text-foreground">按用户放的、不分产品</span>
        —— 国内版和国际版共用同一个{" "}
        <code className="rounded bg-muted px-1 py-0.5">
          %LOCALAPPDATA%\@genieworkbuddy-desktop-updater
        </code>
        。任一版本下载的安装包，另一个版本启动时看到「版本号更高」就会直接套用，
        <span className="font-medium text-foreground">不校验包属于哪个产品</span>
        ，于是把对方的程序目录整个覆盖掉。
      </p>
    </header>
  );

  return (
    <div className="mx-auto min-w-0 w-full max-w-[1180px] space-y-4 px-4 py-6 sm:px-8 sm:py-9">
      {header}

      {error && (
        <Alert variant="destructive">
          <AlertDescription className="break-all">{error}</AlertDescription>
        </Alert>
      )}

      {status?.platformNote && (
        <Alert>
          <AlertDescription>{status.platformNote}</AlertDescription>
        </Alert>
      )}

      {/* ---------------------------------------------------------- 总开关 */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="p-5">
          <div className="flex items-start justify-between gap-4">
            <div className="flex min-w-0 items-start gap-3">
              {disabled ? (
                <ShieldCheck className="mt-0.5 size-5 shrink-0 text-emerald-600" />
              ) : (
                <Ban className="mt-0.5 size-5 shrink-0 text-muted-foreground" />
              )}
              <div className="min-w-0">
                <div className="flex items-center gap-2">
                  <h2 className="text-base font-semibold tracking-tight">禁止 WorkBuddy 自动更新</h2>
                  <Badge
                    variant="secondary"
                    className={cn(
                      "h-5 border-0 px-1.5 text-[11px]",
                      disabled ? "bg-emerald-50 text-emerald-700" : "bg-muted text-muted-foreground",
                    )}
                  >
                    {disabled ? "已启用防护" : "未启用"}
                  </Badge>
                </div>
                <p className="mt-1 text-xs leading-5 text-muted-foreground">
                  打开后做两件事：把更新源指到黑洞（用户级环境变量{" "}
                  <code className="rounded bg-muted px-1">{status?.envName ?? "WORKBUDDY_UPDATE_URL"}</code>
                  ），并隔离缓存里已下载的包 —— 只改环境变量拦不住已经下好的包。
                </p>
                <p className="mt-1 text-[11px] leading-5 text-muted-foreground/80">
                  ⚠️ 环境变量是<span className="font-medium">用户级</span>的，两个版本读的是同一个名字，
                  所以这是<span className="font-medium">一个总开关</span>：开了就是两个版本都不更新。
                  风险本来就来自两者共用缓存，一起关掉才是对的。
                </p>
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-3">
              {loading && <Loader2 className="size-3.5 animate-spin text-muted-foreground" />}
              <Switch
                checked={disabled}
                disabled={busy || demo || !status?.supported}
                onCheckedChange={(next) => {
                  if (next) setConfirmOpen(true);
                  else void apply(false, false);
                }}
                aria-label="禁止 WorkBuddy 自动更新"
              />
            </div>
          </div>

          {status && (
            <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1.5 border-t border-border/50 pt-3 text-[11px] text-muted-foreground">
              <span>
                当前值：
                <code className="ml-1 break-all rounded bg-muted px-1">
                  {status.envValue ?? "（未设置）"}
                </code>
              </span>
              {disabled && (
                <span className="flex items-center gap-1 text-emerald-700">
                  <CheckCircle2 className="size-3.5" />
                  更新源已指向黑洞，不会再下载新包
                </span>
              )}
            </div>
          )}

          {status?.running.length ? (
            <p className="mt-3 rounded-lg bg-muted/50 px-3 py-2 text-[11px] leading-5 text-muted-foreground">
              检测到 <span className="font-medium text-foreground">{status.running.join(" / ")}</span>{" "}
              正在运行 —— 环境变量要<span className="font-medium text-foreground">重启应用</span>才生效；
              如果重启后仍不生效，注销并重新登录一次（资源管理器要重新读环境变量）。
            </p>
          ) : null}
        </CardContent>
      </Card>

      {/* ---------------------------------------------------------- 更新缓存 */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="p-5">
          <div className="flex flex-wrap items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <Package className="size-4 text-muted-foreground" />
                <h2 className="text-base font-semibold tracking-tight">共用的更新缓存</h2>
                {status && (
                  <Badge
                    variant="secondary"
                    className={cn(
                      "h-5 border-0 px-1.5 text-[11px]",
                      cacheActive ? "bg-amber-50 text-amber-700" : "bg-muted text-muted-foreground",
                    )}
                  >
                    {cacheActive ? "有待处理的包" : "干净"}
                  </Badge>
                )}
              </div>
              <div className="mt-1 break-all font-mono text-[11px] text-muted-foreground">
                {status?.cacheDir ?? "—"}
              </div>
            </div>
            <div className="flex shrink-0 items-center gap-2">
              <Button
                size="sm"
                variant="ghost"
                className="text-[11px] font-normal text-muted-foreground"
                onClick={() => void openCacheDir()}
                disabled={!status}
              >
                <FolderOpen className="size-3.5" />
                打开
              </Button>
              <Button
                size="sm"
                variant="outline"
                onClick={() => void clearCache()}
                disabled={busy || demo || !status?.supported || !status?.cacheExists}
                title="把已下载的安装包改名隔离，不会删除文件"
              >
                {busy ? <Loader2 className="size-3.5 animate-spin" /> : <Trash2 className="size-3.5" />}
                隔离已下载的包
              </Button>
            </div>
          </div>

          <p className="mt-2 text-xs leading-5 text-muted-foreground">
            这里的包<span className="font-medium text-foreground">不看更新源地址</span>
            ，应用启动时照样会被应用 —— 所以开启防护时必须把它们隔离掉。
            隔离只是<span className="font-medium text-foreground">改名</span>（加{" "}
            <code className="rounded bg-muted px-1">.disabled-&lt;时间&gt;</code> 后缀），
            文件还在原处，随时可以改回去。
          </p>

          {status && status.cacheFiles.length > 0 && (
            <div className="mt-3 divide-y divide-border/40 rounded-lg border border-border/60">
              {status.cacheFiles.map((f) => {
                const quarantined = f.name.includes(".disabled-") || f.name.includes(".quarantine-");
                return (
                  <div key={f.name} className="flex items-center gap-3 px-3 py-2">
                    <span
                      className={cn(
                        "size-1.5 shrink-0 rounded-full",
                        quarantined ? "bg-muted-foreground/40" : "bg-amber-500",
                      )}
                      aria-hidden="true"
                    />
                    <div className="min-w-0 flex-1">
                      <div
                        className={cn(
                          "truncate font-mono text-[11px]",
                          quarantined && "text-muted-foreground line-through",
                        )}
                      >
                        {f.name}
                      </div>
                      <div className="text-[11px] text-muted-foreground">{f.modified}</div>
                    </div>
                    <span className="shrink-0 text-[11px] tabular-nums text-muted-foreground">
                      {f.sizeText}
                    </span>
                  </div>
                );
              })}
              <div className="flex items-center justify-between px-3 py-2 text-[11px] text-muted-foreground">
                <span>合计</span>
                <span className="tabular-nums">{status.cacheText}</span>
              </div>
            </div>
          )}

          {status && status.cacheFiles.length === 0 && (
            <p className="mt-3 rounded-lg bg-muted/50 px-3 py-2 text-[11px] text-muted-foreground">
              {status.cacheExists ? "缓存目录是空的。" : "缓存目录还不存在 —— 说明还没下载过更新包。"}
            </p>
          )}
        </CardContent>
      </Card>

      {/* ---------------------------------------------------------- 安装状态 */}
      {installs.length > 0 && (
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-5">
            <div className="flex items-center gap-2">
              <h2 className="text-base font-semibold tracking-tight">安装目录的身份</h2>
              {suspicious.length > 0 && (
                <Badge variant="secondary" className="h-5 border-0 bg-amber-50 px-1.5 text-[11px] text-amber-700">
                  {suspicious.length} 处可疑
                </Badge>
              )}
            </div>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              身份由目录里的 <code className="rounded bg-muted px-1">cli/product.json</code> 决定，
              和目录名、和 exe 叫什么名字都无关。
              <span className="font-medium text-foreground">两个启动器 exe 同时存在但身份只有一个</span>
              时，用另一个名字启动也会按那个身份跑 —— 这正是被更新包覆盖后的样子。
            </p>
            <div className="mt-2 divide-y divide-border/40">
              {installs.map((i) => (
                <InstallRow key={i.dir} info={i} />
              ))}
            </div>
          </CardContent>
        </Card>
      )}

      {/* ---------------------------------------------------------- 开启确认 */}
      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>禁止两个版本自动更新？</DialogTitle>
            <DialogDescription>
              会写一个用户级环境变量，并把缓存里已下载的安装包改名隔离。随时可以关掉恢复。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-[13px] leading-6">
            <div className="rounded-lg bg-muted/50 p-3 font-mono text-[11px] leading-5">
              {status?.envName} = {status?.blackhole}
            </div>
            <ul className="space-y-1.5 text-xs leading-5 text-muted-foreground">
              <li>
                <span className="font-medium text-foreground">会影响什么：</span>
                WorkBuddy 不再自动检查/下载新版本，国内版和国际版都一样。手动装新版本不受影响。
              </li>
              <li>
                <span className="font-medium text-foreground">不会影响什么：</span>
                账号切换、签到、反代、缓存迁移这些都不依赖更新通道。
              </li>
              <li>
                <span className="font-medium text-foreground">什么时候生效：</span>
                重启两个应用之后；若仍不生效，注销重新登录一次。
              </li>
            </ul>
            {status && status.cacheFiles.some((f) => !f.name.includes(".disabled-") && !f.name.includes(".quarantine-")) && (
              <div className="flex items-start gap-2 rounded-lg border border-amber-200 bg-amber-50/70 p-3 text-xs leading-5 text-amber-800">
                <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
                <span>
                  缓存里现在就有已下载的包，它们不会看更新源地址 —— 这次会一并隔离掉，否则防护等于没开。
                </span>
              </div>
            )}
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOpen(false)}>
              取消
            </Button>
            <Button
              onClick={() => {
                setConfirmOpen(false);
                void apply(true, true);
              }}
            >
              <ShieldCheck className="size-3.5" />
              启用防护
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {!status && loading && (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <RefreshCw className="size-3.5 animate-spin" />
          正在读取状态…
        </div>
      )}
    </div>
  );
}
