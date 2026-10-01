import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { listen } from "@tauri-apps/api/event";
import {
  AlertTriangle,
  ArrowRight,
  CheckCircle2,
  FolderOpen,
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
import { Switch } from "@/components/ui/switch";
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

/** 路径分隔符（模板串里写死会很难看，提出来）。 */
const SEP = "\\";

function fmtBytes(n: number): string {
  if (!n) return "0 B";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let v = n;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  return `${v < 10 && i > 0 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

type Stage = "idle" | "running" | "done";

/**
 * 把「目标根目录」末尾重复的目录名去掉。
 *
 * 字段收的是**根目录**，最终落点 = 根 + 目录名。但用户很容易直接填落点本身
 * （填 `F:\WBcache\.workbuddy-ai` 就变成了 `F:\WBcache\.workbuddy-ai\.workbuddy-ai`）。
 * 末尾已经是同名目录时按落点理解，不再重复拼一次。
 */
function stripTrailingName(root: string, name: string): string {
  const r = root.trim().replace(/[\\/]+$/, "");
  const i = Math.max(r.lastIndexOf(SEP), r.lastIndexOf("/"));
  if (i < 0) return r;
  return r.slice(i + 1).toLowerCase() === name.toLowerCase() ? r.slice(0, i) || r : r;
}

export default function CacheMovePage() {
  // 演示模式走的是假数据，服务端与浏览器都没有真实的目录可扫：
  // 前者没有系统命令，后者没有文件系统权限。演示模式下照常渲染，方便截图与预览。
  const [params] = useSearchParams();
  /** `?split=1`：把各国目录摊到不同的盘上（截图 / 演示用，和迁移页 ?tab= 一个约定）。 */
  const forceSplit = params.get("split") === "1";
  /**
   * `?only=.workbuddy-ai`：只勾选列出的目录（逗号分隔的目录名）。
   * `?custom=F:\WBcache\.workbuddy-ai`：给勾中的目录铺一个单独路径。
   * 两个都是截图 / 演示用 —— 要展示「只迁国际版、且单独指定位置」这种状态。
   */
  const onlyParam = params.get("only");
  const customParam = params.get("custom");
  const onlySet = useMemo(
    () =>
      onlyParam
        ? new Set(
            onlyParam
              .split(",")
              .map((s) => s.trim())
              .filter(Boolean),
          )
        : null,
    [onlyParam],
  );
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
  // `?confirm=1`：直接展开确认弹窗（截图 / 演示用）。
  const [confirmOpen, setConfirmOpen] = useState(params.get("confirm") === "1");
  const [cleanupOpen, setCleanupOpen] = useState(false);
  const [selected, setSelected] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  /**
   * 逐目录的迁移设置。
   *
   * - `on`     = 本次**是否迁移**这个目录。没勾的完全不碰 —— 这是关键，
   *              之前这个开关只影响「放哪儿」，结果想只迁国际版也把国内版一起搬了。
   * - `custom` = 单独指定的目标根目录；**留空表示用上面的「默认位置」**，
   *              这样改默认位置时没单独指定过的目录会跟着走。
   */
  const [perDest, setPerDest] = useState<Record<string, { on: boolean; custom: string }>>({});
  const destTouched = useRef(false);

  /** 只读体检。`keepDest` 为真时不覆盖用户已经改过的目标路径。 */
  const load = useCallback(async (keepDest = false) => {
    setLoading(true);
    setError(null);
    try {
      const p = await api.cacheMovePlan(keepDest && destTouched.current ? dest : undefined);
      setPlan(p);
      if (!keepDest || !destTouched.current) setDest(p.destDefault);
      // 首次进来每个目录都是「参与迁移 + 用默认位置」；目录各自留空表示沿用默认位置。
      // ?only= 只勾选列出的；?split=1 时改成「每个目录一个盘」并单独指定路径。
      const otherDrives = p.drives.filter((dr) => !dr.system);
      setPerDest((prev) =>
        Object.fromEntries(
          p.dirs.map((d, i) => {
            if (prev[d.name]) return [d.name, prev[d.name]];
            if (onlySet && !onlySet.has(d.name)) return [d.name, { on: false, custom: "" }];
            if (customParam) return [d.name, { on: true, custom: customParam }];
            if (forceSplit && otherDrives.length > 0) {
              const dr = otherDrives[i % otherDrives.length];
              return [d.name, { on: true, custom: `${dr.letter}${SEP}WorkBuddyData` }];
            }
            return [d.name, { on: true, custom: "" }];
          }),
        ),
      );
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setLoading(false);
    }
  }, [dest, onlySet]);

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

  /** 本次真正要迁移的目录。**没勾的完全不碰**。 */
  const included = useMemo(
    () => movable.filter((d) => perDest[d.name]?.on ?? false),
    [movable, perDest],
  );

  /** 改某个目录的设置（勾选 / 单独路径）。 */
  const setRow = useCallback((name: string, patch: Partial<{ on: boolean; custom: string }>) => {
    setPerDest((prev) => {
      const cur = prev[name] ?? { on: false, custom: "" };
      return { ...prev, [name]: { ...cur, ...patch } };
    });
  }, []);

  const selectAll = useCallback(() => {
    setPerDest((prev) => {
      const next = { ...prev };
      for (const d of movable) next[d.name] = { on: true, custom: next[d.name]?.custom ?? "" };
      return next;
    });
  }, [movable]);

  const selectNone = useCallback(() => {
    setPerDest((prev) => {
      const next = { ...prev };
      for (const d of movable) next[d.name] = { on: false, custom: next[d.name]?.custom ?? "" };
      return next;
    });
  }, [movable]);

  /** 某个目录最终用的目标根目录：单独指定优先，否则用默认位置。去掉末尾重复的目录名。 */
  const rootFor = useCallback(
    (name: string) => stripTrailingName(perDest[name]?.custom?.trim() || dest.trim(), name),
    [perDest, dest],
  );

  /** 某个目录最终落点（根目录 + 目录名）。 */
  const finalPathFor = useCallback(
    (name: string) => {
      const root = rootFor(name);
      return root ? `${root}${SEP}${name}` : "";
    },
    [rootFor],
  );

  /** 每个目标根目录 → 会被放进去的目录与合计体积（只统计本次要迁的）。 */
  const rootsUsed = useMemo(() => {
    const m = new Map<string, { labels: string[]; bytes: number }>();
    for (const d of included) {
      const r = rootFor(d.name);
      if (!r) continue;
      const cur = m.get(r) ?? { labels: [], bytes: 0 };
      cur.labels.push(d.label);
      cur.bytes += d.bytes;
      m.set(r, cur);
    }
    return Array.from(m.entries()).map(([root, v]) => ({ root, ...v }));
  }, [included, rootFor]);

  /** 分开放的目录数（>1 就是国内/国外分开）。 */
  const splitCount = rootsUsed.length;

  /** 本次要搬的合计体积。 */
  const includedBytes = useMemo(() => included.reduce((n, d) => n + d.bytes, 0), [included]);
  const includedText = fmtBytes(includedBytes);

  /** 哪个目标盘装不下 —— 按盘各算各的。 */
  const spaceIssues = useMemo(() => {
    const out: { letter: string; free: string; need: number }[] = [];
    for (const g of rootsUsed) {
      const letter = g.root.slice(0, 2).toUpperCase();
      const drive = plan?.drives.find((d) => d.letter.toUpperCase() === letter);
      if (!drive) continue;
      if (drive.free < g.bytes * 1.05) {
        out.push({ letter: drive.letter, free: drive.freeText, need: g.bytes });
      }
    }
    return out;
  }, [rootsUsed, plan]);

  async function runMigration() {
    setConfirmOpen(false);
    setStage("running");
    setResult(null);
    setError(null);
    setProgress(null);
    try {
      // 只传勾选的目录 —— 没勾的一个字节都不会动。
      const targets = included.map((d) => ({ name: d.name, dest: rootFor(d.name) }));
      const r = await api.cacheMoveRun(targets);
      setResult(r);
      setStage("done");
      if (r.ok) {
        toast.success("迁移完成", {
          description: `${r.moved.length} 个目录已搬到 ${rootsUsed.length > 1 ? `${rootsUsed.length} 个位置` : rootsUsed[0]?.root ?? ""}`,
        });
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

  /**
   * 在文件管理器里打开一个目录。
   *
   * 迁移后应用和资源管理器看到的仍是原家目录路径（联接对程序透明），
   * 所以需要一个口子把人直接带到数据真正所在的地方。
   */
  async function openDir(path: string | null) {
    if (!path) return;
    if (demo) {
      toast.message("演示模式不打开目录", { description: path });
      return;
    }
    try {
      await api.cacheMoveOpen(path);
    } catch (e) {
      toast.error("打不开这个目录", { description: e instanceof Error ? e.message : String(e) });
    }
  }

  async function doRollback() {
    setBusy(true);
    setError(null);
    try {
      const r = await api.cacheMoveRollback();
      toast.success("已回滚", { description: r.restored.length ? `恢复：${r.restored.join("、")}` : "没有需要回滚的条目" });
      setResult({ ok: true, moved: [], backups: [], logs: r.logs, placed: [] });
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
                <div className="flex shrink-0 items-center gap-2">
                  {d.isLink ? (
                    <>
                      <Badge variant="secondary" className="border-0 bg-emerald-50 text-emerald-700">
                        已是联接
                      </Badge>
                      <Button
                        variant="ghost"
                        size="sm"
                        className="h-6 px-1.5 text-[11px] font-normal text-muted-foreground"
                        onClick={() => void openDir(d.linkTarget)}
                        title={`打开数据真正所在的位置：${d.linkTarget ?? ""}`}
                      >
                        <FolderOpen className="size-3" />
                        打开
                      </Button>
                    </>
                  ) : d.exists ? (
                    <span className="text-[13px] tabular-nums">{d.sizeText}</span>
                  ) : (
                    <span className="text-xs text-muted-foreground">—</span>
                  )}
                </div>
              </div>
            ))}
          </div>

          {/*
            这块是给「迁完了但设置页还是老路径」这个疑问准备的。
            联接对应用透明是设计目标，不是没生效 —— 但不说清楚，谁都会以为失败。
          */}
          {plans.some((d) => d.isLink) && (
            <div className="border-t border-border/50 bg-muted/25 px-5 py-3">
              <p className="text-[11px] leading-5 text-muted-foreground">
                <span className="font-medium text-foreground">已经是联接的那几行，原路径不会变。</span>
                应用自己的「设置 → 系统缓存目录」里显示的仍是原来的家目录路径（例如{" "}
                <code className="rounded bg-muted px-1">
                  {plans.find((d) => d.isLink)?.path}
                </code>
                ），用资源管理器打开也是这个路径 —— 这是正常的，目录联接对程序完全透明，
                应用压根不知道数据换盘了。
                <span className="font-medium text-foreground">判断有没有真的搬走，看两处</span>
                ：这里的状态是「已是联接」并且指向目标盘；应用设置页里那条「磁盘」容量变成了目标盘的容量。
              </p>
            </div>
          )}
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
            <h2 className="text-base font-semibold tracking-tight">要迁移的目录与目标位置</h2>
            <p className="mt-1 text-xs leading-5 text-muted-foreground">
              数据会整份复制过去，原位置只留一个零占用的联接，源数据一个字节都不会删。
              <span className="font-medium">没勾的目录这次完全不碰</span>；
              国内版和国际版可以分开放到不同的盘，也可以都放同一个文件夹。
            </p>
          </div>

          <div className="space-y-2">
            <Label htmlFor="cache-dest" className="text-[13px]">
              默认位置
            </Label>
            <Input
              id="cache-dest"
              value={dest}
              spellCheck={false}
              onChange={(e) => {
                destTouched.current = true;
                setDest(e.target.value);
              }}
              placeholder={`E:${SEP}WorkBuddyData`}
              className="font-mono text-[13px]"
            />
            <div className="flex flex-wrap gap-1.5 pt-0.5">
              {(plan?.drives ?? []).map((d) => (
                <button
                  key={d.letter}
                  type="button"
                  onClick={() => {
                    destTouched.current = true;
                    setDest(`${d.letter}${SEP}WorkBuddyData`);
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
            <p className="text-[11px] leading-4 text-muted-foreground/80">
              下面路径留空的目录都放到这里；填了路径的走各自的。
              填的是<span className="font-medium">根目录</span>，若末尾已经是该目录名就不会重复拼。
            </p>
          </div>

          {/* 逐目录：勾选参与 + 可单独指定路径 */}
          {movable.length > 0 && (
            <div className="space-y-2 border-t border-border/50 pt-4">
              <div className="flex flex-wrap items-center justify-between gap-2">
                <div className="flex flex-wrap items-center gap-2">
                  <Label className="text-[13px]">要迁移哪些</Label>
                  <span className="text-[11px] text-muted-foreground">
                    勾了才会动，没勾的这次跳过
                    {plans.some((d) => d.isLink) && "；已经是联接的目录不在这里列（上面「当前占用」里能看）"}
                  </span>
                </div>
                <div className="flex items-center gap-1.5">
                  <Badge variant="secondary" className="h-5 border-0 px-1.5 text-[11px]">
                    已选 {included.length}/{movable.length}
                  </Badge>
                  {splitCount > 1 && (
                    <Badge variant="secondary" className="h-5 border-0 px-1.5 text-[11px]">
                      分放到 {splitCount} 个位置
                    </Badge>
                  )}
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-6 px-1.5 text-[11px] font-normal text-muted-foreground"
                    onClick={selectAll}
                    disabled={included.length === movable.length}
                  >
                    全选
                  </Button>
                  <Button
                    variant="ghost"
                    size="sm"
                    className="h-6 px-1.5 text-[11px] font-normal text-muted-foreground"
                    onClick={selectNone}
                    disabled={included.length === 0}
                  >
                    全不选
                  </Button>
                </div>
              </div>

              <div className="divide-y divide-border/40">
                {movable.map((d) => {
                  const o = perDest[d.name] ?? { on: false, custom: "" };
                  const resolved = finalPathFor(d.name);
                  const usingDefault = !o.custom.trim();
                  const id = `cache-dest-${d.name}`;
                  return (
                    <div key={d.name} className="space-y-1.5 py-2.5">
                      <div className="flex items-center gap-3">
                        <Switch
                          id={id}
                          checked={o.on}
                          onCheckedChange={(on) => setRow(d.name, { on })}
                        />
                        <label
                          htmlFor={id}
                          className="flex min-w-0 flex-1 cursor-pointer items-center gap-2"
                        >
                          <span className="text-[13px] font-medium leading-5">{d.label}</span>
                          <code className="text-[11px] text-muted-foreground">{d.name}</code>
                          <span className="text-[11px] tabular-nums text-muted-foreground">
                            {d.sizeText}
                          </span>
                        </label>
                        <span
                          className={cn(
                            "shrink-0 text-[11px]",
                            o.on ? "text-muted-foreground/70" : "text-muted-foreground/45",
                          )}
                        >
                          {!o.on ? "本次跳过" : usingDefault ? "用默认位置" : "单独位置"}
                        </span>
                      </div>

                      {o.on && (
                        <div className="space-y-1.5 pl-0.5">
                          <div className="flex items-center gap-1.5">
                            <Input
                              value={o.custom}
                              spellCheck={false}
                              onChange={(e) => setRow(d.name, { on: true, custom: e.target.value })}
                              placeholder={resolved || `填目标根目录，例如 F:${SEP}WorkBuddyAI-Data`}
                              className="font-mono text-[12px]"
                            />
                            {!usingDefault && (
                              <Button
                                variant="ghost"
                                size="sm"
                                className="h-8 shrink-0 px-2 text-[11px] font-normal text-muted-foreground"
                                onClick={() => setRow(d.name, { on: true, custom: "" })}
                              >
                                用默认
                              </Button>
                            )}
                          </div>
                          <div className="flex flex-wrap gap-1.5">
                            {(plan?.drives ?? []).map((dr) => (
                              <button
                                key={dr.letter}
                                type="button"
                                onClick={() =>
                                  setRow(d.name, { on: true, custom: `${dr.letter}${SEP}WorkBuddyData` })
                                }
                                className="rounded-md border border-border/60 px-2 py-0.5 text-[11px] leading-4 transition-colors hover:bg-muted"
                              >
                                {dr.letter} {dr.freeText}
                              </button>
                            ))}
                          </div>
                          <div className="flex items-center gap-1.5 text-[11px] text-muted-foreground">
                            <ArrowRight className="size-3 shrink-0" />
                            <span className="break-all">{resolved || "—"}</span>
                          </div>
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            </div>
          )}

          {spaceIssues.length > 0 && (
            <Alert variant="destructive">
              <AlertTriangle className="size-4" />
              <AlertDescription>
                {spaceIssues
                  .map(
                    (s) =>
                      `${s.letter} 只剩 ${s.free}，装不下要放进去的 ${fmtBytes(s.need)}（建议留 5% 余量）`,
                  )
                  .join("；")}
              </AlertDescription>
            </Alert>
          )}

          <div className="flex flex-wrap items-center gap-2">
            <Button
              onClick={() => setConfirmOpen(true)}
              disabled={
                Boolean(blocked) ||
                included.length === 0 ||
                stage === "running" ||
                rootsUsed.some((g) => !g.root) ||
                spaceIssues.length > 0
              }
            >
              {stage === "running" ? (
                <Loader2 className="size-3.5 animate-spin" />
              ) : (
                <Play className="size-3.5" />
              )}
              {stage === "running" ? "迁移中…" : "开始迁移"}
            </Button>
            <span className="text-xs text-muted-foreground">
              {movable.length === 0
                ? "所有目录都已经是联接，无需迁移"
                : included.length === 0
                  ? "先勾选要迁移的目录"
                  : `将迁移 ${included.length} 个目录 · ${includedText}`}
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
            {result.ok && result.placed.length > 0 && (
              <p className="mt-3 rounded-lg bg-muted/50 p-3 text-xs leading-5 text-muted-foreground">
                <span className="font-medium text-foreground">怎么确认真的迁好了：</span>
                往上翻到「当前占用」，被迁的那几行状态会变成
                <span className="font-medium text-foreground">「已是联接」</span>
                并指向目标盘。应用自己的设置页仍会显示原来的家目录路径 ——
                那是正常的（目录联接对程序透明），但它的「磁盘」容量会变成目标盘的。
              </p>
            )}
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
              本次只迁移下面列出的 {included.length} 个目录，其它目录不动。
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-2 text-[13px] leading-6">
            <div className="rounded-lg bg-muted/50 p-3">
              {rootsUsed.map((g, i) => (
                <div key={g.root} className={cn(i > 0 && "mt-2 border-t border-border/50 pt-2")}>
                  <div className="break-all font-mono text-[11px] text-muted-foreground">
                    {g.root}
                  </div>
                  {included
                    .filter((d) => rootFor(d.name) === g.root)
                    .map((d) => (
                      <div key={d.name} className="flex items-center justify-between gap-2">
                        <span className="truncate">{d.label}</span>
                        <span className="shrink-0 tabular-nums text-muted-foreground">
                          {d.sizeText}
                        </span>
                      </div>
                    ))}
                </div>
              ))}
              <div className="mt-1 flex items-center justify-between gap-2 border-t border-border/50 pt-1 font-medium">
                <span>合计</span>
                <span className="tabular-nums">{includedText}</span>
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
            <Button
              onClick={() => void runMigration()}
              disabled={spaceIssues.length > 0 || rootsUsed.some((g) => !g.root)}
            >
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
