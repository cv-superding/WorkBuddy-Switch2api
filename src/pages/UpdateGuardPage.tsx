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
        {/* 这个开关才是真正决定「启动时会不会被静默升级」的那一个 */}
        {info.startupUpdate !== null && (
          <span
            className={cn(
              "flex items-center gap-1",
              info.startupUpdate === false ? "text-emerald-700" : "text-amber-700",
            )}
          >
            {info.startupUpdate === false ? (
              <CheckCircle2 className="size-3" />
            ) : (
              <AlertTriangle className="size-3" />
            )}
            启动静默更新：
            {info.startupUpdate === false ? "已关闭" : "开着"}
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
      const bits: string[] = [];
      if (disabledNext) bits.push("更新源已指向黑洞");
      if (r.patched?.length) bits.push(`已关掉 ${r.patched.join(" / ")} 的启动静默更新`);
      if (r.quarantined.length) bits.push(`隔离了 ${r.quarantined.length} 个已下载的包（${r.freedText}）`);
      if (r.patchErrors?.length) {
        toast.warning("有一处没改成", { description: r.patchErrors.join("；") });
      }
      if (disabledNext) {
        toast.success("已开启更新防护", {
          description: `${bits.join("，") || "无需改动"}。重启两个应用后彻底生效。`,
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
      toast.message("演示模式不打开目录", { description: status.cacheDirs.join("\n") });
      return;
    }
    try {
      // 两个暂存目录都打开 —— 哪个里面有包就打开哪个最省事。
      for (const d of status.cacheDirs) {
        await api.cacheMoveOpen(d);
      }
    } catch (e) {
      toast.error("打不开这个目录", { description: e instanceof Error ? e.message : String(e) });
    }
  }

  const header = (
    <header>
      <h1 className="text-[28px] font-semibold tracking-tight">更新防护</h1>
      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        WorkBuddy 的更新缓存是<span className="font-medium text-foreground">按用户放的、不分产品</span>
        ，国内版和国际版共用同一套暂存目录。任一版本下载的安装包，另一个版本启动时看到
        「版本号更高」就会直接套用，
        <span className="font-medium text-foreground">不校验包属于哪个产品</span>
        ，于是把对方的程序目录整个覆盖掉。
      </p>
      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        真正把包装上去的那一步叫
        <span className="font-medium text-foreground">「启动静默更新」</span>
        （<code className="rounded bg-muted px-1">updates.startupForceAutoUpdate</code>），
        开关在每份安装的 <code className="rounded bg-muted px-1">cli/product.json</code> 里 ——
        它<span className="font-medium text-foreground">不看更新源地址</span>
        ，所以光设环境变量挡不住，必须一起关掉。
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

      {/*
        一眼给结论。防护要三件事都成立才算数，缺一件都会漏：
        ① 更新源黑洞  ② 每份安装都关掉「启动静默更新」  ③ 暂存目录里没有活包。
        2026-10-08 实测漏了 ②③，结果 5.7.6 被静默装上了。
      */}
      {status?.supported && (
        disabled ? (
          <Alert className="border-emerald-200 bg-emerald-50/60">
            <CheckCircle2 className="size-4 text-emerald-600" />
            <AlertDescription className="text-emerald-800">
              <span className="font-medium">防护已完全生效。</span>
              更新源指向黑洞、{status.installs.length} 个安装都关掉了启动静默更新、
              暂存目录里也没有待处理的包。重启两个应用后彻底生效。
            </AlertDescription>
          </Alert>
        ) : (
          <Alert variant={cacheActive || status.installsNeedingPatch > 0 ? "destructive" : undefined}>
            <AlertTriangle className="size-4" />
            <AlertDescription>
              <span className="font-medium">防护还没完全生效</span>
              —— 下面这几项没做到就会有更新溜进来：
              <ul className="mt-1.5 space-y-1">
                <li>
                  {status.envValue === status.blackhole ? "✅" : "❌"} 更新源
                  {status.envValue === status.blackhole
                    ? "已指向黑洞"
                    : "没指向黑洞（新进程仍会去查更新）"}
                </li>
                <li>
                  {status.installsNeedingPatch === 0 ? "✅" : "❌"}{" "}
                  {status.installsNeedingPatch === 0 ? (
                    "所有安装都关掉了启动静默更新"
                  ) : (
                    <span>
                      还有 <span className="font-medium">{status.installsNeedingPatch}</span>{" "}
                      个安装开着「启动静默更新」—— 已下载的包会在启动时被静默装掉，
                      这条路<b>不看更新源地址</b>
                    </span>
                  )}
                </li>
                <li>
                  {cacheActive ? "❌" : "✅"}{" "}
                  {cacheActive ? "暂存目录里还有会被应用的包" : "暂存目录是干净的"}
                </li>
              </ul>
              {!disabled && (
                <span className="mt-2 flex">
                  <Button size="sm" onClick={() => setConfirmOpen(true)} disabled={busy || demo}>
                    <ShieldCheck className="size-3.5" />
                    一键补齐
                  </Button>
                </span>
              )}
            </AlertDescription>
          </Alert>
        )
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
                  打开后做三件事，<span className="font-medium text-foreground">少一件都挡不住</span>：
                  ① 把更新源指到黑洞（用户级环境变量{" "}
                  <code className="rounded bg-muted px-1">{status?.envName ?? "WORKBUDDY_UPDATE_URL"}</code>
                  ）；② 关掉每份安装的「启动静默更新」；③ 隔离暂存目录里已下载的包。
                </p>
                <p className="mt-1 text-[11px] leading-5 text-muted-foreground/80">
                  ⚠️ 环境变量对<span className="font-medium">已经在跑</span>的进程无效
                  （进程环境在创建时就固定了），所以它是全局的、也是不完整的 ——
                  真正管住「已下载的包被静默装掉」的是第 ②项，那一项是
                  <span className="font-medium">按版本</span>生效的。
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
              <div className="mt-1 space-y-0.5">
                {(status?.cacheDirs ?? []).map((d) => (
                  <div key={d} className="break-all font-mono text-[11px] text-muted-foreground">
                    {d}
                  </div>
                ))}
                {!status?.cacheDirs.length && (
                  <div className="font-mono text-[11px] text-muted-foreground">—</div>
                )}
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
                disabled={busy || demo || !status?.supported || status.cacheFiles.length === 0}
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
                // 只显示最后两级，够区分是哪个暂存目录了
                const dirTail = f.dir.split("\\").slice(-2).join("\\");
                return (
                  <div key={`${f.dir}|${f.name}`} className="flex items-center gap-3 px-3 py-2">
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
                      <div className="truncate font-mono text-[11px] text-muted-foreground/70">
                        {dirTail}
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
              暂存目录是干净的 —— 没有下载好等着被应用的包。
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
            <DialogTitle>开启更新防护？</DialogTitle>
            <DialogDescription>
              会改每份安装的 <code className="rounded bg-muted px-1">cli/product.json</code>
              （把 <code className="rounded bg-muted px-1">startupForceAutoUpdate</code> 置为 false），
              写一个用户级环境变量，并隔离暂存目录里已下载的安装包。随时可以关掉恢复。
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
