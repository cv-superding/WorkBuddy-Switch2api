import { useCallback, useEffect, useMemo, useState } from "react";
import { useSearchParams } from "react-router-dom";
import {
  AlertTriangle,
  ArrowDownToLine,
  ArrowUpFromLine,
  CheckCircle2,
  FolderOpen,
  Loader2,
  RefreshCw,
  Search,
} from "lucide-react";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import * as api from "@/lib/api";
import type {
  TransferExportResult,
  TransferImportResult,
  TransferPreview,
  TransferScan,
} from "@/lib/types";
import { cn } from "@/lib/utils";

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

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

function fmtTime(ms?: number): string {
  if (!ms) return "—";
  const d = new Date(ms);
  const p = (n: number) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
}

/** 工作区显示名：优先用原始 cwd，回退把 slug 粗略还原成路径。 */
function wsLabel(w: { cwd?: string; slug: string }): string {
  if (w.cwd) return w.cwd;
  const parts = w.slug.split("-");
  if (parts.length > 1 && parts[0].length === 1) {
    return `${parts[0].toUpperCase()}:\\${parts.slice(1).join("\\")}`;
  }
  return w.slug;
}

const EDITIONS = [
  { key: "domestic", label: "国内版" },
  { key: "international", label: "国际版" },
] as const;

function EditionPicker({
  value,
  onChange,
}: {
  value: string;
  onChange: (v: string) => void;
}) {
  return (
    <div className="inline-flex rounded-lg border border-border bg-muted/30 p-0.5">
      {EDITIONS.map((e) => (
        <button
          key={e.key}
          type="button"
          onClick={() => onChange(e.key)}
          className={cn(
            "rounded-md px-3 py-1 text-xs transition-colors",
            value === e.key
              ? "bg-background font-medium text-foreground shadow-sm"
              : "text-muted-foreground hover:text-foreground",
          )}
        >
          {e.label}
        </button>
      ))}
    </div>
  );
}

function Row({
  checked,
  onToggle,
  disabled,
  children,
}: {
  checked: boolean;
  onToggle: () => void;
  disabled?: boolean;
  children: React.ReactNode;
}) {
  return (
    <label
      className={cn(
        "flex cursor-pointer items-center gap-2.5 rounded-lg px-2.5 py-2 text-sm transition-colors",
        disabled ? "cursor-not-allowed opacity-50" : "hover:bg-foreground/[0.04]",
      )}
    >
      <input
        type="checkbox"
        className="size-3.5 accent-foreground"
        checked={checked}
        disabled={disabled}
        onChange={onToggle}
      />
      {children}
    </label>
  );
}

function Field({
  label,
  desc,
  checked,
  onChange,
  disabled,
}: {
  label: string;
  desc?: string;
  checked: boolean;
  onChange: (v: boolean) => void;
  disabled?: boolean;
}) {
  return (
    <div className="flex items-start justify-between gap-3 py-2">
      <div className="min-w-0">
        <div className="text-sm">{label}</div>
        {desc ? (
          <div className="mt-0.5 text-xs text-muted-foreground">{desc}</div>
        ) : null}
      </div>
      <Switch checked={checked} onCheckedChange={onChange} disabled={disabled} />
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border bg-muted/25 px-3 py-2">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div className="mt-0.5 text-sm font-medium tabular-nums">{value}</div>
    </div>
  );
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

function ExportPanel() {
  const [edition, setEdition] = useState<string>("domestic");
  const [scan, setScan] = useState<TransferScan | null>(null);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<TransferExportResult | null>(null);

  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [query, setQuery] = useState("");
  const [opt, setOpt] = useState({
    config: true,
    plugins: false,
    fileHistory: false,
    snapshots: false,
    credentials: false,
  });

  const load = useCallback(async (ed: string) => {
    setLoading(true);
    setError(null);
    setResult(null);
    try {
      const s = await api.transferScan(ed);
      setScan(s);
      setPicked(new Set());
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void load(edition);
  }, [edition, load]);

  const list = useMemo(() => {
    const ws = scan?.workspaces ?? [];
    const q = query.trim().toLowerCase();
    const filtered = q
      ? ws.filter(
          (w) =>
            w.slug.toLowerCase().includes(q) ||
            (w.cwd ?? "").toLowerCase().includes(q) ||
            w.title.toLowerCase().includes(q),
        )
      : ws;
    return [...filtered].sort((a, b) => b.bytes - a.bytes);
  }, [scan, query]);

  const totalSessions = useMemo(
    () =>
      (scan?.workspaces ?? [])
        .filter((w) => picked.has(w.slug))
        .reduce((n, w) => n + w.sessions, 0),
    [scan, picked],
  );

  const totalBytes = useMemo(() => {
    const ws = (scan?.workspaces ?? []).filter((w) => picked.has(w.slug));
    let n = ws.reduce((a, w) => a + w.bytes, 0);
    if (opt.config) {
      n += (scan?.config ?? [])
        .filter((c) => !["plugins", "connectors_marketplace"].includes(c.key))
        .reduce((a, c) => a + c.bytes, 0);
    }
    if (opt.plugins) {
      n += (scan?.config ?? [])
        .filter((c) => ["plugins", "connectors_marketplace"].includes(c.key))
        .reduce((a, c) => a + c.bytes, 0);
    }
    if (opt.fileHistory) n += scan?.extras.fileHistory.bytes ?? 0;
    if (opt.snapshots) n += scan?.extras.workspaceSnapshots.bytes ?? 0;
    return n;
  }, [scan, picked, opt]);

  const toggle = (slug: string) =>
    setPicked((prev) => {
      const next = new Set(prev);
      if (next.has(slug)) next.delete(slug);
      else next.add(slug);
      return next;
    });

  async function doExport() {
    if (picked.size === 0) return;
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const { save } = await import("@tauri-apps/plugin-dialog");
      const d = new Date();
      const p = (n: number) => String(n).padStart(2, "0");
      const stamp = `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}`;
      const path = await save({
        title: "导出迁移包",
        defaultPath: `workbuddy-迁移包-${edition}-${stamp}.zip`,
        filters: [{ name: "Zip", extensions: ["zip"] }],
      });
      if (!path) return;
      const res = await api.transferExport(
        path,
        {
          slugs: [...picked],
          includeConfig: opt.config,
          includePlugins: opt.plugins,
          includeFileHistory: opt.fileHistory,
          includeWorkspaceSnapshots: opt.snapshots,
          includeCredentials: opt.credentials,
        },
        edition,
      );
      setResult(res);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="flex items-center gap-3">
          <EditionPicker value={edition} onChange={setEdition} />
          <Button
            variant="outline"
            size="sm"
            onClick={() => void load(edition)}
            disabled={loading}
          >
            {loading ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <RefreshCw className="size-3.5" />
            )}
            重新扫描
          </Button>
        </div>
        {scan ? (
          <div className="min-w-0 truncate text-xs text-muted-foreground">
            {scan.workspaceCount} 个工作区 · {scan.sessionFiles} 个文件 ·{" "}
            {fmtBytes(scan.extras.blobs.bytes)} 附件
          </div>
        ) : null}
      </div>

      {error ? (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      ) : null}

      {loading && !scan ? (
        <div className="flex items-center gap-2 py-10 text-sm text-muted-foreground">
          <Loader2 className="size-4 animate-spin" />
          正在扫描数据目录…
        </div>
      ) : null}

      {scan ? (
        <>
          <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_300px]">
            {/* 工作区列表 */}
            <Card>
              <CardContent className="p-3">
                <div className="mb-2 flex items-center gap-2">
                  <div className="relative min-w-0 flex-1">
                    <Search className="pointer-events-none absolute left-2.5 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
                    <Input
                      className="h-8 pl-8 text-sm"
                      placeholder="搜索项目路径或标题"
                      value={query}
                      onChange={(e) => setQuery(e.target.value)}
                    />
                  </div>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() =>
                      setPicked(
                        picked.size === list.length
                          ? new Set()
                          : new Set(list.map((w) => w.slug)),
                      )
                    }
                  >
                    {picked.size === list.length && list.length > 0 ? "全不选" : "全选"}
                  </Button>
                </div>
                <div className="max-h-[26rem] overflow-y-auto pr-1">
                  {list.length === 0 ? (
                    <div className="px-2 py-6 text-center text-sm text-muted-foreground">
                      没有匹配的项目
                    </div>
                  ) : (
                    list.map((w) => (
                      <Row
                        key={w.slug}
                        checked={picked.has(w.slug)}
                        onToggle={() => toggle(w.slug)}
                      >
                        <div className="min-w-0 flex-1">
                          <div className="truncate" title={wsLabel(w)}>
                            {wsLabel(w)}
                          </div>
                          <div className="mt-0.5 text-xs text-muted-foreground">
                            {w.sessions} 个会话 · {fmtBytes(w.bytes)}
                          </div>
                        </div>
                      </Row>
                    ))
                  )}
                </div>
              </CardContent>
            </Card>

            {/* 选项 */}
            <div className="space-y-3">
              <Card>
                <CardContent className="p-4">
                  <div className="mb-1 text-sm font-medium">包内容</div>
                  <div className="divide-y divide-border">
                    <Field
                      label="配置"
                      desc="技能 / MCP / 连接器 / 记忆 / 设置"
                      checked={opt.config}
                      onChange={(v) => setOpt((o) => ({ ...o, config: v }))}
                    />
                    <Field
                      label="插件缓存"
                      desc="新机器免重新下载，体积较大"
                      checked={opt.plugins}
                      onChange={(v) => setOpt((o) => ({ ...o, plugins: v }))}
                    />
                    <Field
                      label="文件改动历史"
                      desc="file-history"
                      checked={opt.fileHistory}
                      onChange={(v) => setOpt((o) => ({ ...o, fileHistory: v }))}
                    />
                    <Field
                      label="工作区快照"
                      desc={`体积最大（${fmtBytes(scan.extras.workspaceSnapshots.bytes)}）`}
                      checked={opt.snapshots}
                      onChange={(v) => setOpt((o) => ({ ...o, snapshots: v }))}
                    />
                    <Field
                      label="账号凭证"
                      desc="含明文令牌，导入后免登录。别放网盘"
                      checked={opt.credentials}
                      onChange={(v) => setOpt((o) => ({ ...o, credentials: v }))}
                    />
                  </div>
                </CardContent>
              </Card>

              <Card>
                <CardContent className="space-y-3 p-4">
                  <div className="grid grid-cols-2 gap-2">
                    <Stat label="工作区" value={String(picked.size)} />
                    <Stat label="会话" value={String(totalSessions)} />
                  </div>
                  <div className="text-xs text-muted-foreground">
                    预计 {fmtBytes(totalBytes)}（附件按实际引用统计，可能更少）
                  </div>
                  <Button
                    className="w-full"
                    disabled={picked.size === 0 || busy}
                    onClick={() => void doExport()}
                  >
                    {busy ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <ArrowUpFromLine className="size-4" />
                    )}
                    导出为 zip
                  </Button>
                  {busy ? (
                    <div className="text-center text-xs text-muted-foreground">
                      正在打包，大包可能要几十秒…
                    </div>
                  ) : null}
                </CardContent>
              </Card>
            </div>
          </div>

          {result ? (
            <Card>
              <CardContent className="p-4">
                <div className="flex items-center gap-2 text-sm font-medium">
                  <CheckCircle2 className="size-4 text-emerald-600" />
                  导出完成
                </div>
                <div className="mt-2 space-y-1 text-xs text-muted-foreground">
                  <div className="break-all font-mono">{result.path}</div>
                  <div>
                    {fmtBytes(result.bytes)}（原始 {fmtBytes(result.rawBytes)}）·{" "}
                    {result.workspaces} 个工作区 · {result.sessions} 个会话 ·{" "}
                    {result.blobs} 个附件 · {result.files} 个文件
                    {result.credentials ? " · 含凭证" : ""}
                  </div>
                  {result.skipped.length > 0 ? (
                    <div className="text-amber-600">
                      跳过 {result.skipped.length} 项（读不到或过大）
                    </div>
                  ) : null}
                </div>
              </CardContent>
            </Card>
          ) : null}
        </>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

function ImportPanel() {
  const [edition, setEdition] = useState<string>("domestic");
  const [zipPath, setZipPath] = useState<string>("");
  const [preview, setPreview] = useState<TransferPreview | null>(null);
  const [picked, setPicked] = useState<Set<string>>(new Set());
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<TransferImportResult | null>(null);
  const [opt, setOpt] = useState({ overwrite: false, credentials: false, config: true, db: true });

  async function chooseAndPreview() {
    setError(null);
    setResult(null);
    try {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const path = await open({
        title: "选择迁移包",
        multiple: false,
        filters: [{ name: "迁移包", extensions: ["zip"] }],
      });
      if (!path || typeof path !== "string") return;
      setZipPath(path);
      setLoading(true);
      const p = await api.transferPreview(path, edition);
      setPreview(p);
      setPicked(new Set(p.sessions.map((s) => s.sid)));
    } catch (e) {
      setError(api.asError(e));
      setPreview(null);
    } finally {
      setLoading(false);
    }
  }

  async function doImport() {
    if (!zipPath) return;
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const res = await api.transferImport(
        zipPath,
        {
          sessionIds: [...picked],
          applySessions: true,
          applyBlobs: true,
          applyConfig: opt.config,
          applyDb: opt.db,
          applyCredentials: opt.credentials,
          overwrite: opt.overwrite,
          shareSessions: true,
        },
        edition,
      );
      setResult(res);
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  const s = preview?.summary;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center gap-3">
        <EditionPicker value={edition} onChange={setEdition} />
        <Button size="sm" onClick={() => void chooseAndPreview()} disabled={loading}>
          {loading ? (
            <Loader2 className="size-3.5 animate-spin" />
          ) : (
            <FolderOpen className="size-3.5" />
          )}
          选择迁移包
        </Button>
        {zipPath ? (
          <span className="min-w-0 truncate font-mono text-xs text-muted-foreground">
            {zipPath}
          </span>
        ) : null}
      </div>

      {error ? (
        <Alert variant="destructive">
          <AlertDescription>{error}</AlertDescription>
        </Alert>
      ) : null}

      {!zipPath && !loading ? (
        <Card>
          <CardContent className="p-10 text-center text-sm text-muted-foreground">
            从另一台机器导出的 zip 包在这里导入。会先比对再让你挑，不会直接改数据。
          </CardContent>
        </Card>
      ) : null}

      {preview && s ? (
        <>
          {preview.warnings.length > 0 ? (
            <Alert>
              <AlertDescription className="space-y-1">
                {preview.warnings.map((w) => (
                  <div key={w} className="flex items-start gap-2">
                    <AlertTriangle className="mt-0.5 size-3.5 shrink-0 text-amber-600" />
                    <span>{w}</span>
                  </div>
                ))}
              </AlertDescription>
            </Alert>
          ) : null}

          <Card>
            <CardContent className="p-4">
              <div className="flex flex-wrap items-center gap-2 text-sm">
                <Badge variant="secondary">
                  {preview.source?.editionLabel ?? preview.source?.edition ?? "未知档位"}
                </Badge>
                <span className="text-muted-foreground">
                  来自 {preview.source?.hostname ?? "未知机器"} ·{" "}
                  {fmtTime(
                    (preview.manifest?.createdAt as number | undefined) ?? undefined,
                  )}
                </span>
                <span className="text-muted-foreground">
                  → 导入到 {preview.target.editionLabel}
                </span>
              </div>
              <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-4">
                <Stat
                  label="会话"
                  value={`${s.sessions.new} 新 / ${s.sessions.existing} 已有`}
                />
                <Stat
                  label="附件"
                  value={`缺 ${s.blobs.missing} / 共 ${s.blobs.total}`}
                />
                <Stat label="配置项" value={String(s.config.length)} />
                <Stat
                  label="数据库"
                  value={s.dbNew > 0 ? `新增 ${s.dbNew} 行` : "无新增"}
                />
              </div>
              {s.credentials ? (
                <div className="mt-3 rounded-lg border border-amber-300/60 bg-amber-50/60 px-3 py-2 text-xs text-amber-800">
                  包里含账号凭证：{s.credentials.nickname ?? ""}{" "}
                  <span className="font-mono">
                    {String(s.credentials.uid ?? "").slice(0, 8)}
                  </span>
                  {s.credentials.hasAccessToken ? "（含 accessToken）" : ""}
                </div>
              ) : null}
            </CardContent>
          </Card>

          <div className="grid gap-4 lg:grid-cols-[minmax(0,1fr)_300px]">
            <Card>
              <CardContent className="p-3">
                <div className="mb-2 flex items-center justify-between px-1">
                  <div className="text-sm font-medium">
                    会话（{picked.size}/{preview.sessions.length}）
                  </div>
                  <Button
                    variant="ghost"
                    size="sm"
                    onClick={() =>
                      setPicked(
                        picked.size === preview.sessions.length
                          ? new Set()
                          : new Set(preview.sessions.map((x) => x.sid)),
                      )
                    }
                  >
                    {picked.size === preview.sessions.length ? "全不选" : "全选"}
                  </Button>
                </div>
                <div className="max-h-[24rem] overflow-y-auto pr-1">
                  {preview.sessions.map((x) => (
                    <Row
                      key={x.sid}
                      checked={picked.has(x.sid)}
                      onToggle={() =>
                        setPicked((prev) => {
                          const next = new Set(prev);
                          if (next.has(x.sid)) next.delete(x.sid);
                          else next.add(x.sid);
                          return next;
                        })
                      }
                    >
                      <div className="min-w-0 flex-1">
                        <div className="flex items-center gap-2">
                          <span className="truncate" title={x.title}>
                            {x.title || "(未命名会话)"}
                          </span>
                          {x.exists ? (
                            <Badge variant="outline" className="shrink-0 text-[10px]">
                              已有
                            </Badge>
                          ) : null}
                        </div>
                        <div className="mt-0.5 truncate text-xs text-muted-foreground">
                          {x.files} 个文件 · {fmtBytes(x.bytes)} ·{" "}
                          {fmtTime(x.updatedAt)}
                        </div>
                      </div>
                    </Row>
                  ))}
                </div>
              </CardContent>
            </Card>

            <div className="space-y-3">
              <Card>
                <CardContent className="p-4">
                  <div className="mb-1 text-sm font-medium">导入选项</div>
                  <div className="divide-y divide-border">
                    <Field
                      label="配置"
                      desc={`${s.config.length} 类，逐文件补缺、JSON 递归合并（本地已有的键保留）`}
                      checked={opt.config}
                      onChange={(v) => setOpt((o) => ({ ...o, config: v }))}
                    />
                    <Field
                      label="数据库"
                      desc="会话 / 定时任务 / 工作区索引"
                      checked={opt.db}
                      onChange={(v) => setOpt((o) => ({ ...o, db: v }))}
                    />
                    <Field
                      label="覆盖已存在"
                      desc="默认关：只补本地缺的，已有的不动"
                      checked={opt.overwrite}
                      onChange={(v) => setOpt((o) => ({ ...o, overwrite: v }))}
                    />
                    <Field
                      label="写入账号凭证"
                      desc="会覆盖当前档位的登录状态"
                      checked={opt.credentials}
                      onChange={(v) => setOpt((o) => ({ ...o, credentials: v }))}
                    />
                  </div>
                </CardContent>
              </Card>

              <Card>
                <CardContent className="space-y-3 p-4">
                  <Button
                    className="w-full"
                    disabled={busy || (!picked.size && !opt.config)}
                    onClick={() => void doImport()}
                  >
                    {busy ? (
                      <Loader2 className="size-4 animate-spin" />
                    ) : (
                      <ArrowDownToLine className="size-4" />
                    )}
                    开始导入
                  </Button>
                  <div className="text-xs text-muted-foreground">
                    导入前会自动备份将被覆盖的文件。会话会被置为「共享」，换账号也能看到。
                  </div>
                </CardContent>
              </Card>
            </div>
          </div>

          {result ? (
            <Card>
              <CardContent className="p-4">
                <div className="flex items-center gap-2 text-sm font-medium">
                  {result.errors.length === 0 ? (
                    <CheckCircle2 className="size-4 text-emerald-600" />
                  ) : (
                    <AlertTriangle className="size-4 text-amber-600" />
                  )}
                  导入完成
                  {result.errors.length > 0 ? `（${result.errors.length} 个问题）` : ""}
                </div>
                <div className="mt-3 grid grid-cols-2 gap-2 sm:grid-cols-4">
                  <Stat
                    label="会话文件"
                    value={`${result.sessions.files} 个`}
                  />
                  <Stat label="附件" value={`${result.blobs.files} 个`} />
                  <Stat
                    label="配置"
                    value={`新 ${result.config.newFiles} / 合并 ${result.config.mergedLeaves}`}
                  />
                  <Stat
                    label="数据库"
                    value={`${Object.values(result.db).reduce((n, v) => n + v.inserted, 0)} 行`}
                  />
                </div>
                <div className="mt-3 space-y-1 text-xs text-muted-foreground">
                  <div className="break-all">
                    备份：<span className="font-mono">{result.backup.dir}</span>（
                    {result.backup.files} 个文件，{fmtBytes(result.backup.bytes)}）
                  </div>
                  {result.notes.map((n) => (
                    <div key={n}>· {n}</div>
                  ))}
                  {result.errors.slice(0, 5).map((n) => (
                    <div key={n} className="text-amber-600">
                      · {n}
                    </div>
                  ))}
                  {result.errors.length > 5 ? (
                    <div className="text-amber-600">
                      … 另有 {result.errors.length - 5} 条
                    </div>
                  ) : null}
                </div>
              </CardContent>
            </Card>
          ) : null}
        </>
      ) : null}
    </div>
  );
}

// ---------------------------------------------------------------------------

export default function TransferPage() {
  // Tab 由 URL 决定：/transfer?tab=import 可以直接落到导入页（也方便排查问题）。
  const [params, setParams] = useSearchParams();
  const tab = params.get("tab") === "import" ? "import" : "export";

  const title = (
    <header>
      <h1 className="text-[28px] font-semibold tracking-tight">迁移</h1>
      <p className="mt-2 text-sm leading-6 text-muted-foreground">
        把会话与全部配置（技能 / MCP / 连接器 / 记忆 / 定时任务）打包带走，在另一台机器上增量合并进来。
      </p>
    </header>
  );

  // webui（浏览器）模式下没有系统文件对话框，选不了迁移包的保存 / 打开位置。
  // 与其让按钮点下去报「暂不支持该操作」，不如直接说清楚。
  if (api.isWebui()) {
    return (
      <div className="mx-auto min-w-0 w-full max-w-[1180px] px-4 py-6 sm:px-8 sm:py-9">
        {title}
        <Alert>
          <AlertDescription>
            迁移需要在 <span className="font-medium">桌面应用</span> 里使用 —— 打包和导入都要通过系统文件对话框选择迁移包的位置，
            浏览器做不到。请在另一台机器上装好桌面端，从「迁移」页导出 / 导入。
            <br />
            只是想在<span className="font-medium">同一台机器的两个账号之间</span>共享会话？那不需要迁移包，
            用账号管理里的「共享会话」即可（不产生副本）。
          </AlertDescription>
        </Alert>
      </div>
    );
  }

  return (
    <div className="mx-auto min-w-0 w-full max-w-[1180px] space-y-4 px-4 py-6 sm:px-8 sm:py-9">
      {title}

      <Tabs
        value={tab}
        onValueChange={(v) => setParams(v === "import" ? { tab: "import" } : {})}
      >
        <TabsList>
          <TabsTrigger value="export">导出</TabsTrigger>
          <TabsTrigger value="import">导入</TabsTrigger>
        </TabsList>
        <TabsContent value="export" className="mt-4">
          <ExportPanel />
        </TabsContent>
        <TabsContent value="import" className="mt-4">
          <ImportPanel />
        </TabsContent>
      </Tabs>
    </div>
  );
}
