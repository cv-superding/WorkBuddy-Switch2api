import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { listen } from "@tauri-apps/api/event";
import {
  AlertTriangle,
  ArrowRight,
  CheckCircle2,
  HardDrive,
  Link2,
  Loader2,
  Play,
  RefreshCw,
  RotateCcw,
  Trash2,
  XCircle,
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
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import * as api from "@/lib/api";
import type {
  CacheBackupItem,
  CacheMovePlan,
  CacheMoveProgress,
  CacheMoveResult,
  CacheMoveStep,
  CacheVerifyResult,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 迁移阶段 → 中文。 */
const PHASE_LABEL: Record<string, string> = {
  scan: "统计源目录",
  copy: "复制文件",
  verify: "校验副本",
  link: "建立联接",
  rollback: "回滚中",
  done: "完成",
};

function ProgressBar({ percent }: { percent: number }) {
  return (
    <div className="h-1.5 w-full overflow-hidden rounded-full bg-muted">
      <div
        className="h-full rounded-full bg-primary transition-[width] duration-300 ease-out"
        style={{ width: `${Math.min(100, Math.max(0, percent))}%` }}
      />
    </div>
  );
}

function StepRow({ step }: { step: CacheMoveStep }) {
  return (
    <div className="flex items-start gap-2 py-1.5">
      {step.ok ? (
        <CheckCircle2 className="mt-0.5 size-4 shrink-0 text-emerald-600" />
      ) : (
        <XCircle className="mt-0.5 size-4 shrink-0 text-destructive" />
      )}
      <div className="min-w-0">
        <div className="text-[13px] font-medium leading-5">{step.name}</div>
        <div className="text-xs leading-5 text-muted-foreground break-all">{step.message}</div>
      </div>
    </div>
  );
}

type Stage = "idle" | "running" | "done";

export default function CacheMovePage() {
  // 演示模式走的是假数据，服务端与浏览器都没有真实的目录可扫：
  // 前者没有系统命令，后者没有文件系统权限。演示模式下照常渲染，方便截图与预览。
  const demo = api.isDemoMode();
  const desktop = demo || (api.isDesktop() && !api.isWebui());
  const [plan, setPlan] = useState<CacheMovePlan | null>(null);
  const [dest, setDest] = useState("");
  const [loading, setLoading] = useState(false);
  const [verifying, setVerifying] = useState(false);
  const [verify, setVerify] = useState<CacheVerifyResult | null>(null);
  const [stage, setStage] = useState<Stage>("idle");
  const [progress, setProgress] = useState<CacheMoveProgress | null>(null);
  const [result, setResult] = useState<CacheMoveResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [cleanupOpen, setCleanupOpen] = useState(false);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const destTouched = useRef(false);

  /** 只读体检。`keepDest` 为真时不覆盖用户已经改过的目标路径。 */
  const load = useCallback(async (keepDest = false) => {
    setLoading(true);
    setError(null);
    try {
      const p = await api.cacheMovePlan(keepDest && destTouched.current ? dest : undefined);
      setPlan(p);
      if (!keepDest || !destTouched.current) setDest(p.destDefault);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [dest]);

  const loadVerify = useCallback(async () => {
    setVerifying(true);
    try {
      const v = await api.cacheMoveVerify();
      setVerify(v);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setVerifying(false);
    }
  }, []);

  useEffect(() => {
    if (!desktop) return;
    void load();
    void loadVerify();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [desktop]);

  // 迁移进度事件。
  useEffect(() => {
    // 演示 / 浏览器里没有 Tauri 事件通道，监听会直接抛。
    if (!desktop || demo) return;
    let unlisten: (() => void) | undefined;
    void listen<CacheMoveProgress>("cache-move-progress", (ev) => {
      setProgress(ev.payload);
    }).then((fn) => {
      unlisten = fn;
    });
    return () => unlisten?.();
  }, [desktop, demo]);

  const plans = useMemo(() => plan?.dirs ?? [], [plan]);
  const movable = useMemo(() => plans.filter((d) => d.movable && d.exists && !d.isLink), [plans]);
  const backups = verify?.backups ?? [];

  async function runMigration() {
    setConfirmOpen(false);
    setStage("running");
    setResult(null);
    setError(null);
    setProgress(null);
    try {
      const r = await api.cacheMoveRun(dest, movable.map((d) => d.name));
      setResult(r);
      setStage("done");
      if (r.ok) {
        toast.success("迁移完成", { description: `已迁移 ${r.moved.length} 个目录到 ${r.dest}` });
      } else {
        toast.error("迁移未全部完成", { description: "看下面的步骤日志" });
      }
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setStage("idle");
    } finally {
      void load(true);
      void loadVerify();
    }
  }

  async function doRollback() {
    setBusy(true);
    setError(null);
    try {
      const r = await api.cacheMoveRollback();
      toast.success("已回滚", { description: r.restored.length ? `恢复：${r.restored.join("、")}` : "没有需要回滚的条目" });
      setResult({ ok: true, moved: [], backups: [], logs: r.logs, dest: "" });
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      void load(true);
      void loadVerify();
    }
  }

  async function doCleanup() {
    setCleanupOpen(false);
    setBusy(true);
    setError(null);
    try {
      const r = await api.cacheMoveCleanup(selected);
      const okCount = r.removed.length;
      if (okCount) toast.success(`已删除 ${okCount} 个备份`);
      else toast.error("没有删除任何东西", { description: r.logs.find((l) => !l.ok)?.message });
      setSelected([]);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
      void loadVerify();
    }
  }

  const header = (
    <header>
      <h1 className="text-[28px] font-semibold tracking-tight">缓存迁移</h1>
      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        WorkBuddy 把运行数据全放在家目录（国内版{" "}
        <code className="rounded bg-muted px-1 py-0.5">~/.workbuddy</code>、国际版{" "}
        <code className="rounded bg-muted px-1 py-0.5">~/.workbuddy-ai</code>），时间一长能涨到二十多 GB、全压在系统盘。
        这里把数据搬到别的盘，原路径改建 <span className="font-medium">NTFS 目录联接</span>，客户端完全感知不到，也不需要改任何配置。
      </p>
    </header>
  );

  if (!desktop) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-[1180px] px-4 py-6 sm:px-8 sm:py-9">
        {header}
        <Alert className="mt-6">
          <AlertDescription>
            缓存迁移要在<span className="font-medium">桌面应用</span>里使用 —— 它要跑 robocopy
            复制十几 GB、再调用系统命令建目录联接，浏览器和服务端模式都做不到。
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  const blocked = plan && !plan.canRun;
  const destDrive = plan?.drives.find((d) => dest.toUpperCase().startsWith(d.letter.toUpperCase()));
  const notEnough = Boolean(destDrive && plan && destDrive.free < plan.totalBytes * 1.05);

  return (
    <div className="mx-auto min-w-0 w-full max-w-[1180px] space-y-4 px-4 py-6 sm:px-8 sm:py-9">
      {header}

      {error && (
        <Alert variant="destructive">
          <AlertDescription className="break-all">{error}</AlertDescription>
        </Alert>
      )}

      {/* ---------------------------------------------------------- 现状 */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="p-0">
          <div className="flex items-center justify-between gap-3 border-b border-border/50 px-5 py-3">
            <div className="flex items-center gap-2">
              <HardDrive className="size-4 text-muted-foreground" />
              <h2 className="text-base font-semibold tracking-tight">当前占用</h2>
              {plan && (
                <Badge variant="secondary" className="h-5 border-0 px-1.5 text-[11px]">
                  合计 {plan.totalText} · {plan.totalFiles.toLocaleString("zh-CN")} 个文件
                </Badge>
              )}
            </div>
            <Button
              size="sm"
              variant="ghost"
              onClick={() => {
                void load(true);
                void loadVerify();
              }}
              disabled={loading || verifying || stage === "running"}
            >
              {loading || verifying ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <RefreshCw className="size-3.5" />
              )}
              刷新
            </Button>
          </div>

          <div className="divide-y divide-border/50">
            {plans.map((d) => (
              <div key={d.name} className="flex items-center gap-3 px-5 py-3">
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="text-[13px] font-medium leading-5">{d.label}</span>
                    <code className="text-[11px] text-muted-foreground">{d.name}</code>
                  </div>
                  <div className="mt-0.5 flex items-center gap-1.5 text-xs leading-5 text-muted-foreground">
                    {d.isLink ? (
                      <>
                        <Link2 className="size-3.5 shrink-0" />
                        <span className="break-all">{d.linkTarget ?? "?"}</span>
                      </>
                    ) : (
                      <span className="break-all">{d.exists ? d.path : "不存在"}</span>
                    )}
                  </div>
                </div>
                <div className="shrink-0 text-right">
                  {d.isLink ? (
                    <Badge variant="secondary" className="border-0 bg-emerald-50 text-emerald-700">
                      已是联接
                    </Badge>
                  ) : d.exists ? (
                    <span className="text-[13px] tabular-nums">{d.sizeText}</span>
                  ) : (
                    <span className="text-xs text-muted-foreground">—</span>
                  )}
                </div>
              </div>
            ))}
          </div>
        </CardContent>
      </Card>

      {/* ---------------------------------------------------------- 阻塞提示 */}
      {plan?.platformNote && (
        <Alert>
          <AlertDescription>{plan.platformNote}</AlertDescription>
        </Alert>
      )}

      {plan && plan.blocking.length > 0 && (
        <Alert variant="destructive">
          <AlertTriangle className="size-4" />
          <AlertDescription>
            {plan.blockedReason}
            <br />
            检测到正在运行：<span className="font-medium">{plan.blocking.join(" / ")}</span>
          </AlertDescription>
        </Alert>
      )}

      {plan && plan.blocking.length === 0 && plan.warnings.length > 0 && (
        <Alert>
          <AlertDescription>
            检测到 {plan.warnings.join(" / ")} 在运行。它不直接占用这些目录，但如果迁移中途报「改名失败」，先把它也退掉再重试。
          </AlertDescription>
        </Alert>
      )}

      {/* ---------------------------------------------------------- 目标位置 */}
      <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
        <CardContent className="space-y-4 p-5">
          <div>
            <h2 className="text-base font-semibold tracking-tight">目标位置</h2>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              数据会整份复制到这里，原位置只留一个零占用的联接。迁移期间源数据一个字节都不会删。
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="cache-dest" className="text-[13px]">
              目标目录
            </Label>
            <Input
              id="cache-dest"
              value={dest}
              spellCheck={false}
              onChange={(e) => {
                destTouched.current = true;
                setDest(e.target.value);
              }}
              placeholder={"E:\\WorkBuddyData"}
              className="font-mono text-[13px]"
            />
            <div className="flex flex-wrap gap-1.5 pt-0.5">
              {(plan?.drives ?? []).map((d) => (
                <button
                  key={d.letter}
                  type="button"
                  onClick={() => {
                    destTouched.current = true;
                    setDest(`${d.letter}\\WorkBuddyData`);
                  }}
                  className={cn(
                    "rounded-md border border-border/60 px-2 py-1 text-[11px] leading-4 transition-colors hover:bg-muted",
                    d.system && "opacity-60",
                  )}
                  title={`${d.letter} 可用 ${d.freeText} / 共 ${d.totalText}`}
                >
                  {d.letter} 可用 {d.freeText}
                  {d.system ? "（系统盘）" : ""}
                </button>
              ))}
            </div>
          </div>

          {notEnough && destDrive && plan && (
            <Alert variant="destructive">
              <AlertTriangle className="size-4" />
              <AlertDescription>
                {destDrive.letter} 只剩 {destDrive.freeText}，装不下本次需要搬运的 {plan.totalText}（建议留 5% 余量）。
              </AlertDescription>
            </Alert>
          )}

          <div className="rounded-lg bg-muted/50 p-3 text-xs leading-5 text-muted-foreground">
            <div className="flex items-center gap-1.5">
              <ArrowRight className="size-3.5 shrink-0" />
              <span className="break-all">
                {plans
                  .filter((d) => d.exists && !d.isLink && d.movable)
                  .map((d) => d.path)
                  .join("、") || "（没有可迁移的目录）"}
              </span>
            </div>
            <div className="mt-1 flex items-center gap-1.5">
              <ArrowRight className="size-3.5 shrink-0" />
              <span className="break-all">
                {movable.length
                  ? movable.map((d) => `${dest}\\${d.name}`).join("、")
                  : "—"}
              </span>
            </div>
          </div>

          <div className="flex items-center gap-2">
            <Button
              onClick={() => setConfirmOpen(true)}
              disabled={Boolean(blocked) || movable.length === 0 || stage === "running" || !dest.trim()}
            >
              {stage === "running" ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Play className="size-3.5" />
              )}
              {stage === "running" ? "迁移中…" : "开始迁移"}
            </Button>
            <span className="text-xs text-muted-foreground">
              {movable.length
                ? `将搬运 ${plan?.totalText ?? ""}`
                : "所有目录都已经是联接，无需迁移"}
            </span>
          </div>

          {stage === "running" && (
            <div className="space-y-2 pt-1">
              <div className="flex items-center justify-between text-xs">
                <span className="text-muted-foreground">
                  {progress ? PHASE_LABEL[progress.phase] ?? progress.phase : "准备中"}
                  {progress ? ` · ${progress.detail}` : ""}
                </span>
                <span className="tabular-nums text-muted-foreground">
                  {progress ? `${progress.percent}%` : "0%"}
                </span>
              </div>
              <ProgressBar percent={progress?.percent ?? 0} />
              <p className="text-[11px] leading-4 text-muted-foreground/80">
                复制期间请不要打开 WorkBuddy。几十 GB 可能要几分钟。
              </p>
            </div>
          )}
        </CardContent>
      </Card>

      {/* ---------------------------------------------------------- 结果日志 */}
      {result && result.logs.length > 0 && (
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-5">
            <h2 className="text-base font-semibold tracking-tight">
              {result.ok ? "迁移结果" : "没有全部完成"}
            </h2>
            <div className="mt-2 divide-y divide-border/40">
              {result.logs.map((s, i) => (
                <StepRow key={`${s.name}-${i}`} step={s} />
              ))}
            </div>
            {result.ok && result.backups.length > 0 && (
              <p className="mt-3 rounded-lg bg-muted/50 p-3 text-xs leading-5 text-muted-foreground">
                源目录没有被删除，只是改名保留了：
                <br />
                {result.backups.map((b) => (
                  <code key={b} className="break-all">
                    {b}
                  </code>
                ))}
                <br />
                启动一次 WorkBuddy 确认记忆 / 技能 / 会话都在，正常用几天之后再到下面清理，系统盘的空间才会真正释放。
              </p>
            )}
          </CardContent>
        </Card>
      )}

      {/* ---------------------------------------------------------- 备份清理 */}
      {(backups.length > 0 || plan?.dirs.some((d) => d.backup)) && (
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="p-5">
            <div className="flex items-center justify-between gap-3">
              <h2 className="text-base font-semibold tracking-tight">遗留备份</h2>
              <Button
                size="sm"
                variant="outline"
                disabled={selected.length === 0 || busy}
                onClick={() => setCleanupOpen(true)}
              >
                <Trash2 className="size-3.5" />
                删除选中（{selected.length}）
              </Button>
            </div>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              这些是迁移时改名留下的源目录。确认客户端一切正常之后再删，删掉才会真正腾出系统盘空间。
            </p>

            <div className="mt-3 divide-y divide-border/40">
              {backups.map((b: CacheBackupItem) => (
                <label key={b.path} className="flex cursor-pointer items-center gap-3 py-2.5">
                  <input
                    type="checkbox"
                    className="size-3.5 shrink-0 accent-current"
                    checked={selected.includes(b.path)}
                    onChange={(e) =>
                      setSelected((prev) =>
                        e.target.checked ? [...prev, b.path] : prev.filter((p) => p !== b.path),
                      )
                    }
                  />
                  <div className="min-w-0 flex-1">
                    <div className="truncate text-[13px] leading-5">{b.name}</div>
                    <div className="truncate text-xs leading-5 text-muted-foreground">{b.path}</div>
                  </div>
                  <div className="shrink-0 text-xs tabular-nums text-muted-foreground">
                    {b.sizeText} · {b.files.toLocaleString("zh-CN")} 个文件
                  </div>
                </label>
              ))}
              {backups.length === 0 && (
                <p className="py-3 text-xs text-muted-foreground">正在读取备份体积…</p>
              )}
            </div>
          </CardContent>
        </Card>
      )}

      {/* ---------------------------------------------------------- 回滚 */}
      {plans.some((d) => d.isLink) && (
        <Card className="min-w-0 gap-0 overflow-hidden rounded-xl py-0 shadow-none">
          <CardContent className="flex items-start justify-between gap-4 p-5">
            <div className="min-w-0">
              <h2 className="text-base font-semibold tracking-tight">回滚</h2>
              <p className="mt-1 text-xs leading-5 text-muted-foreground">
                删掉联接、把备份改名回到原位。只在客户端打不开或数据看着不对时用。
              </p>
            </div>
            <Button
              variant="outline"
              size="sm"
              disabled={busy || stage === "running" || plans.some((d) => d.isLink && !d.backup)}
              onClick={() => void doRollback()}
            >
              {busy ? <Loader2 className="size-3.5 animate-spin" /> : <RotateCcw className="size-3.5" />}
              回滚到迁移前
            </Button>
          </CardContent>
        </Card>
      )}

      {/* ---------------------------------------------------------- 确认弹窗 */}
      <Dialog open={confirmOpen} onOpenChange={setConfirmOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>确认开始迁移？</DialogTitle>
            <DialogDescription>
              先把数据整份复制到目标盘并逐项校验，全部一致之后才会改名 + 建联接。中途任何一步失败都会自动改回原样。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-[13px] leading-6">
            <div className="text-muted-foreground">
              目标：<code className="break-all">{dest}</code>
            </div>
            <div className="rounded-lg bg-muted/50 p-3">
              {movable.map((d) => (
                <div key={d.name} className="flex items-center justify-between gap-2">
                  <span className="truncate">{d.label}</span>
                  <span className="shrink-0 tabular-nums text-muted-foreground">{d.sizeText}</span>
                </div>
              ))}
              <div className="mt-1 flex items-center justify-between gap-2 border-t border-border/50 pt-1 font-medium">
                <span>合计</span>
                <span className="tabular-nums">{plan?.totalText}</span>
              </div>
            </div>
            <div className="flex items-start gap-2 text-xs leading-5 text-muted-foreground">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                迁移期间确保 WorkBuddy 已完全退出，并且不要中途启动它。源目录不会删除，只是改名成 <code>.moved-*</code>{" "}
                保留，之后可以清理或回滚。
              </span>
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setConfirmOpen(false)}>
              取消
            </Button>
            <Button onClick={() => void runMigration()} disabled={notEnough}>
              <Play className="size-3.5" />
              开始迁移
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      {/* ---------------------------------------------------------- 清理确认 */}
      <Dialog open={cleanupOpen} onOpenChange={setCleanupOpen}>
        <DialogContent className="sm:max-w-lg">
          <DialogHeader>
            <DialogTitle>删除备份目录</DialogTitle>
            <DialogDescription>删除后无法从备份恢复，只能从目标盘的联接目录里访问数据。</DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-[13px] leading-6">
            <div className="rounded-lg bg-muted/50 p-3">
              {selected.map((p) => (
                <div key={p} className="break-all font-mono text-xs">
                  {p}
                </div>
              ))}
            </div>
            <div className="flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-xs leading-5 text-destructive">
              <AlertTriangle className="mt-0.5 size-3.5 shrink-0" />
              <span>
                <span className="font-semibold">此操作不可逆。</span>
                请先确认客户端能正常打开、记忆与历史会话都在，并且已经正常使用过一段时间。
              </span>
            </div>
          </div>
          <DialogFooter>
            <Button variant="outline" onClick={() => setCleanupOpen(false)}>
              取消
            </Button>
            <Button variant="destructive" onClick={() => void doCleanup()} disabled={busy}>
              <Trash2 className="size-3.5" />
              确认删除
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </div>
  );
}
