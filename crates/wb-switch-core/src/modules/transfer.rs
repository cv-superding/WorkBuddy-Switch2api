//! 跨机器迁移包：扫描 / 导出 / 预览 / 导入。
//!
//! # 要解决的问题
//!
//! WorkBuddy 的数据是「本地优先 + 账号隔离」，换台电脑就什么都没有：
//! 会话、长期记忆、MCP/连接器、自定义技能全在本机目录里。
//!
//! # 与已有开源项目的分工
//!
//! 参考（并在实现上对齐）了两个项目：
//! - `xiaoliuzhuan666/workbuddy-account-migrate`（MIT）：数据隔离全景图、单对话跨版本手法
//! - `Harvey-Will/workbuddy-tools`（MIT）：`core/migrate.py` 的可选迁移项、blobs 可达性反查
//!
//! **两者都只处理「同一台机器内」的账号/版本之间**。本模块补的是它们没有的那一层：
//! 把选中的工作区 + 全部配置**打成一个 zip 包**，在另一台机器上**增量合并**进来。
//!
//! # 包的形态
//!
//! ```text
//! manifest.json          版本 / 时间 / 来源 / 账号 / 清单 / 每个文件的 sha256
//! README.txt             人可读的说明（这个包是什么、怎么用、含哪些数据）
//! sessions/{sid}/
//!   row.json             workbuddy.db 里 sessions 表那一行
//!   projects/{slug}/…    会话正文 .jsonl + .meta.json + 回滚文件 + 同名目录
//!   tasks/…              历史任务
//!   artifact-index/…     产物索引
//!   blobs/{hh}/{hash}    仅本会话引用到的附件（SHA256 可达性反查）
//! config/
//!   skills/              自定义技能
//!   mcp.json / settings.json / models.json / mcp-approvals.json
//!   connectors/{uid}/    MCP 与连接器状态
//!   memory/{uid}_memory.md
//!   agents/ commands/    自定义 agent 与命令（存在才带）
//!   plugins/             插件缓存（可选）
//!   automations.json     定时任务（DB 行导出）
//! credentials/
//!   auth.json            认证文件原样（可选；含 accessToken/refreshToken）
//! ```
//!
//! # 增量语义
//!
//! 导入一律「**目标已有就不动**」：
//! - 会话：按 session id 判重，已存在则跳过
//! - blobs：文件名就是 SHA256，天然去重
//! - 技能：按技能目录名判重
//! - JSON 配置（mcp/settings/…）：递归合并，**目标已有的键保留**
//! - 记忆：追加去重

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::modules::config::now_ms;
use crate::modules::edition::Edition;
use crate::modules::session::workbuddy_db_path_for;
use crate::modules::zipreader::{safe_join, ZipReader};

/// 包格式标识与版本。导入侧据此拒绝不认识的包。
pub const BUNDLE_FORMAT: &str = "wb-switch-bundle";
pub const BUNDLE_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

fn projects_dir(edition: Edition) -> PathBuf {
    edition.projects_dir()
}

fn tasks_dir(edition: Edition) -> PathBuf {
    edition.tasks_dir()
}

fn blobs_dir(edition: Edition) -> PathBuf {
    edition.blobs_dir()
}

fn artifact_index_dir(edition: Edition) -> PathBuf {
    edition.artifact_index_dir()
}

fn file_history_dir(edition: Edition) -> PathBuf {
    edition.file_history_dir()
}

fn workspace_sessions_dir(edition: Edition) -> PathBuf {
    edition.workspace_sessions_dir()
}

fn config_dir(edition: Edition) -> PathBuf {
    edition.data_dir()
}

/// 配置文件清单（`key, 相对路径, 是否目录, 展示名`）。
const CONFIG_ITEMS: &[(&str, &str, bool, &str)] = &[
    ("skills", "skills", true, "自定义技能"),
    ("connectors", "connectors", true, "MCP / 连接器配置"),
    ("memory", "memory", true, "长期记忆"),
    ("agents", "agents", true, "自定义 Agent"),
    ("commands", "commands", true, "自定义命令"),
    ("mcp_json", "mcp.json", false, "MCP 服务清单"),
    ("mcp_approvals", "mcp-approvals.json", false, "MCP 授权记录"),
    ("settings", "settings.json", false, "全局设置"),
    ("models", "models.json", false, "自定义模型"),
    ("user_state", "user-state.json", false, "界面状态"),
    ("soul", "SOUL.md", false, "人格设定"),
    ("user_md", "USER.md", false, "用户画像"),
    ("identity", "IDENTITY.md", false, "身份记录"),
];

// ---------------------------------------------------------------------------
// 扫描
// ---------------------------------------------------------------------------

/// 目录的字节数与文件数（**流式**遍历，不把上万个路径收进内存）。
///
/// `max_files` 是安全上限 —— 扫描只为给 UI 估体积，没必要为精确卡住界面。
fn dir_stats(dir: &Path, max_files: u64) -> (u64, u64) {
    if !dir.is_dir() {
        return (0, 0);
    }
    let mut bytes = 0u64;
    let mut files = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                if let Ok(meta) = entry.metadata() {
                    bytes += meta.len();
                }
                files += 1;
                if files >= max_files {
                    return (bytes, files);
                }
            }
        }
    }
    (bytes, files)
}

/// 单个工作区（= `projects/` 下的一个目录）。
#[derive(Debug, Clone)]
struct WorkspaceInfo {
    slug: String,
    cwd: String,
    title: String,
    session_count: usize,
}

/// 列出可导出的工作区：以数据库里的 `sessions.cwd` 为准分组，
/// 再补上 `projects/` 下存在但数据库里没有 cwd 的目录。
fn list_workspaces(edition: Edition) -> Vec<WorkspaceInfo> {
    use rusqlite::Connection;
    let mut by_slug: BTreeMap<String, WorkspaceInfo> = BTreeMap::new();

    // 1) 数据库里的会话按 cwd 归组
    let db = workbuddy_db_path_for(edition);
    if db.is_file() {
        if let Ok(conn) = Connection::open_with_flags(
            &db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        ) {
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
            let sql = "SELECT cwd, COUNT(*) FROM sessions \
                       WHERE deleted_at IS NULL AND cwd IS NOT NULL AND cwd <> '' \
                       GROUP BY cwd";
            if let Ok(mut stmt) = conn.prepare(sql) {
                if let Ok(rows) = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
                }) {
                    for row in rows.flatten() {
                        let (cwd, count) = row;
                        let slug = slug_for_cwd(&cwd);
                        by_slug
                            .entry(slug.clone())
                            .and_modify(|w| w.session_count += count as usize)
                            .or_insert(WorkspaceInfo {
                                slug: slug.clone(),
                                cwd: cwd.clone(),
                                title: short_title(&cwd),
                                session_count: count as usize,
                            });
                    }
                }
            }
        }
    }

    // 2) projects/ 下的目录（可能包含数据库里没有的）
    let pd = projects_dir(edition);
    if pd.is_dir() {
        if let Ok(rd) = std::fs::read_dir(&pd) {
            for entry in rd.flatten() {
                let path = entry.path();
                if !path.is_dir() {
                    continue;
                }
                let Some(slug) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                by_slug.entry(slug.to_string()).or_insert_with(|| WorkspaceInfo {
                    slug: slug.to_string(),
                    cwd: String::new(),
                    title: short_title(slug),
                    session_count: 0,
                });
            }
        }
    }

    let mut list: Vec<_> = by_slug.into_values().collect();
    list.sort_by(|a, b| b.session_count.cmp(&a.session_count).then(a.slug.cmp(&b.slug)));
    list
}

/// `C:\Users\alice\WorkBuddy\2026-09-10-14-49-02` → `c-Users-alice-WorkBuddy-2026-09-10-14-49-02`
///
/// 与客户端一致：转小写、去掉 `:`、`\` 与 `/` 换成 `-`。
fn slug_for_cwd(cwd: &str) -> String {
    // 客户端规则：**盘符小写，其余原样保留**；去掉冒号，分隔符换成 `-`。
    // 实测目录名：`f-Code-Contest-LLM`、`c-Users-29436-WorkBuddy-2026-08-14-20-59-24`。
    // 两个坑都踩过：
    //   1. 一步替换 [':', '\\', '/'] 会让 `C:\` 变成 `c--`（双横线）；
    //   2. 整串 to_ascii_lowercase() 会得到 `f-code-contest-llm`，与磁盘目录名对不上 ——
    //      DB 分组与目录分组各成一份，工作区列表里会冒出重复项。
    let cleaned: String = cwd.trim().replace(':', "").replace(['\\', '/'], "-");
    let cleaned = cleaned.trim_start_matches('-');
    let mut it = cleaned.chars();
    match it.next() {
        Some(first) => format!("{}{}", first.to_ascii_lowercase(), it.as_str()),
        None => String::new(),
    }
}

/// 工作区展示名：取路径最后两段，太长则截断。
fn short_title(cwd_or_slug: &str) -> String {
    let s = cwd_or_slug.replace('\\', "/");
    let parts: Vec<&str> = s.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        s
    }
}

/// 扫描结果（给前端渲染导出选项）。
pub fn scan_exportable(edition: Edition) -> Value {
    let ws = list_workspaces(edition);
    let mut workspaces = Vec::new();
    let mut total_files = 0u64;
    for w in &ws {
        let dir = projects_dir(edition).join(&w.slug);
        let (bytes, files) = dir_stats(&dir, 200_000);
        total_files += files;
        workspaces.push(json!({
            "slug": w.slug,
            "cwd": w.cwd,
            "title": w.title,
            "sessions": w.session_count,
            "bytes": bytes,
            "files": files,
        }));
    }

    let cfg = config_dir(edition);
    let mut config = Vec::new();
    for (key, rel, is_dir, label) in CONFIG_ITEMS {
        let p = cfg.join(rel);
        if !p.exists() {
            continue;
        }
        let (bytes, files) = if *is_dir {
            dir_stats(&p, 200_000)
        } else {
            (p.metadata().map(|m| m.len()).unwrap_or(0), 1)
        };
        config.push(json!({
            "key": key,
            "label": label,
            "path": rel,
            "isDir": is_dir,
            "bytes": bytes,
            "files": files,
            "exists": true,
        }));
    }
    // 可选的大件
    for (key, rel, label) in [
        ("plugins", "plugins", "插件与市场缓存"),
        ("connectors_marketplace", "connectors-marketplace", "连接器市场缓存"),
    ] {
        let p = cfg.join(rel);
        if p.is_dir() {
            let (bytes, files) = dir_stats(&p, 200_000);
            config.push(json!({
                "key": key, "label": label, "path": rel, "isDir": true,
                "bytes": bytes, "files": files, "exists": true, "optional": true,
            }));
        }
    }

    let snap = workspace_sessions_dir(edition);
    let (snap_bytes, snap_files) = dir_stats(&snap, 200_000);
    let blobs = blobs_dir(edition);
    let (blob_bytes, blob_files) = dir_stats(&blobs, 200_000);
    let hist = file_history_dir(edition);
    let (hist_bytes, hist_files) = dir_stats(&hist, 200_000);
    let tasks = tasks_dir(edition);
    let (task_bytes, task_files) = dir_stats(&tasks, 50_000);
    let art = artifact_index_dir(edition);
    let (art_bytes, art_files) = dir_stats(&art, 50_000);
    let db = workbuddy_db_path_for(edition);

    json!({
        "edition": edition.key(),
        "editionLabel": edition.label(),
        "dataDir": edition.data_dir().to_string_lossy(),
        "uid": crate::modules::session::current_user_uid_for(edition),
        "workspaces": workspaces,
        "workspaceCount": ws.len(),
        "sessionFiles": total_files,
        "config": config,
        "extras": {
            "blobs": { "bytes": blob_bytes, "files": blob_files },
            "fileHistory": { "bytes": hist_bytes, "files": hist_files },
            "workspaceSnapshots": { "bytes": snap_bytes, "files": snap_files },
            "tasks": { "bytes": task_bytes, "files": task_files },
            "artifactIndex": { "bytes": art_bytes, "files": art_files },
            "database": { "bytes": db.metadata().map(|m| m.len()).unwrap_or(0) },
        },
        "scannedAt": now_ms(),
    })
}

// ---------------------------------------------------------------------------
// 导出
// ---------------------------------------------------------------------------

/// 导出选项。
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// 要导出的工作区 slug（取自 `scan_exportable().workspaces[].slug`）。
    pub slugs: Vec<String>,
    /// 带配置：skills / connectors / memory / agents / commands / mcp.json / settings.json …
    pub include_config: bool,
    /// 带插件与连接器市场缓存（大、可重新下载）。
    pub include_plugins: bool,
    /// 带文件改动历史（`file-history/`）。
    pub include_file_history: bool,
    /// 带工作区快照（`workspace/sessions/`，最大的一块，单会话可达几百 MB）。
    pub include_workspace_snapshots: bool,
    /// 带账号凭证（accessToken / refreshToken），导入后免登录。
    pub include_credentials: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            slugs: Vec::new(),
            include_config: true,
            include_plugins: false,
            include_file_history: false,
            include_workspace_snapshots: false,
            include_credentials: false,
        }
    }
}

/// 归档条目名 + 磁盘路径。
type FileEntry = (String, PathBuf);

/// 未启用 ZIP64，单包上限留些余量。
const MAX_BUNDLE_BYTES: u64 = 3_500_000_000;

/// 某工作区下的全部会话 id。
///
/// 直接看 `projects/{slug}/` 下的文件名：正文是 `{sid}.jsonl`，
/// 同一个 sid 还可能带 `.meta.json` / `.file-rollback.ndjson` / 同名目录。
/// 会话 id 是 UUID，取第一个点之前的部分并按这个形状筛掉杂项。
fn session_ids_in_slug(edition: Edition, slug: &str) -> Vec<String> {
    let dir = projects_dir(edition).join(slug);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut ids = BTreeSet::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(base) = name.split('.').next() else {
            continue;
        };
        if base.len() == 36 && base.matches('-').count() == 4 {
            ids.insert(base.to_string());
        }
    }
    ids.into_iter().collect()
}

/// 把 rusqlite 的一行转成 JSON 对象。
fn row_to_json(row: &rusqlite::Row<'_>, cols: &[String]) -> rusqlite::Result<Value> {
    let mut obj = serde_json::Map::new();
    for (i, name) in cols.iter().enumerate() {
        let v = match row.get_ref(i) {
            Ok(rusqlite::types::ValueRef::Null) => Value::Null,
            Ok(rusqlite::types::ValueRef::Integer(n)) => json!(n),
            Ok(rusqlite::types::ValueRef::Real(f)) => json!(f),
            Ok(rusqlite::types::ValueRef::Text(t)) => {
                Value::String(String::from_utf8_lossy(t).to_string())
            }
            _ => Value::Null,
        };
        obj.insert(name.clone(), v);
    }
    Ok(Value::Object(obj))
}

fn open_ro(edition: Edition) -> Option<rusqlite::Connection> {
    let db = workbuddy_db_path_for(edition);
    if !db.is_file() {
        return None;
    }
    let conn = rusqlite::Connection::open_with_flags(
        &db,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .ok()?;
    let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
    Some(conn)
}

/// 从数据库取这些会话的完整行（`SELECT *`，按列名转 JSON）。
fn session_rows(edition: Edition, ids: &BTreeSet<String>) -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    if ids.is_empty() {
        return out;
    }
    let Some(conn) = open_ro(edition) else {
        return out;
    };
    let Ok(mut stmt) = conn.prepare("SELECT * FROM sessions WHERE id = ?1") else {
        return out;
    };
    let cols: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for id in ids {
        if let Ok(v) = stmt.query_row([id], |r| row_to_json(r, &cols)) {
            out.insert(id.clone(), v);
        }
    }
    out
}

/// 把整张表导成 JSON 数组（用于 automations / workspaces 这类无 user_id 的全局表）。
fn dump_table(edition: Edition, table: &str) -> Vec<Value> {
    let mut out = Vec::new();
    let Some(conn) = open_ro(edition) else {
        return out;
    };
    let Ok(mut stmt) = conn.prepare(&format!("SELECT * FROM {table}")) else {
        return out;
    };
    let cols: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let Ok(rows) = stmt.query_map([], |r| row_to_json(r, &cols)) else {
        return out;
    };
    for row in rows.flatten() {
        out.push(row);
    }
    out
}

/// 递归收集目录下所有文件，归档名前缀为 `prefix`。
fn dir_files(dir: &Path, prefix: &str) -> Vec<FileEntry> {
    let mut out = Vec::new();
    let mut stack = vec![(dir.to_path_buf(), prefix.trim_end_matches('/').to_string())];
    while let Some((cur, pre)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&cur) else {
            continue;
        };
        for entry in rd.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let arch = format!("{pre}/{name}");
            let path = entry.path();
            if path.is_dir() {
                stack.push((path, arch));
            } else if path.is_file() {
                out.push((arch, path));
            }
        }
    }
    out
}

/// 收集一个会话在 `projects/{slug}/` 下的全部文件。
///
/// ⚠️ 同一个 sid 既可能是文件（`.jsonl`）也可能是**目录**（工具输出外溢处）。
/// 参考项目特别提醒：对目录用 `copy2` 会在 Windows 上抛 `IsADirectoryError`。
/// 这里显式分开处理。
fn session_project_files(edition: Edition, slug: &str, sid: &str) -> Vec<FileEntry> {
    let dir = projects_dir(edition).join(slug);
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.split('.').next() != Some(sid) {
            continue;
        }
        let path = entry.path();
        let arch = format!("sessions/{sid}/projects/{slug}/{name}");
        if path.is_file() {
            out.push((arch, path));
        } else if path.is_dir() {
            out.extend(dir_files(&path, &arch));
        }
    }
    out
}

/// 按会话 id 从「每会话一个同名子项」的目录里取文件（tasks / artifact-index / …）。
fn session_child_entry(root: &Path, sid: &str, kind: &str) -> Vec<FileEntry> {
    let mut out = Vec::new();
    for cand in [root.join(sid), root.join(format!("{sid}.json"))] {
        if !cand.exists() {
            continue;
        }
        let Some(name) = cand.file_name().map(|s| s.to_string_lossy().to_string()) else {
            continue;
        };
        let arch = format!("sessions/{sid}/{kind}/{name}");
        if cand.is_file() {
            out.push((arch, cand));
        } else {
            out.extend(dir_files(&cand, &arch));
        }
    }
    out
}

/// 从文本里抠出所有「恰好 64 位十六进制」的串（SHA256 形态）。
///
/// 手写而不是引 `regex`：少一个依赖就少一份离线构建风险（`zip` 那次已经吃过亏）。
fn hex64_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_hexdigit() {
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_hexdigit() {
                i += 1;
            }
            // 只有正好 64 位才算 —— 更长的串是别的编码，不是哈希引用
            if i - start == 64 {
                out.push(text[start..i].to_ascii_lowercase());
            }
        } else {
            i += 1;
        }
    }
    out
}

/// 反查这组会话引用到的 blobs 文件。
///
/// 思路同 `Harvey-Will/workbuddy-tools` 的 `find_reachable_blobs`：
/// 扫会话的 jsonl/tasks 抠出 SHA256，再到 `blobs/{前两位}/{hash}` 取文件。
/// 找不到引用的直接忽略（文件可能已被 GC）。
fn reachable_blobs(edition: Edition, files: &[FileEntry]) -> Vec<FileEntry> {
    let blobs = blobs_dir(edition);
    if !blobs.is_dir() {
        return Vec::new();
    }
    /// 单文件扫描上限：再大就不读内容了（避免为了反查把几 GB 读进内存）。
    const MAX_SCAN: u64 = 32 * 1024 * 1024;

    let mut hashes: BTreeSet<String> = BTreeSet::new();
    for (arch, path) in files {
        if !(arch.ends_with(".jsonl") || arch.ends_with(".json") || arch.ends_with(".ndjson")) {
            continue;
        }
        let Ok(meta) = path.metadata() else {
            continue;
        };
        if meta.len() > MAX_SCAN {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        hashes.extend(hex64_tokens(&text));
    }

    let mut out = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for h in hashes {
        let bucket = blobs.join(&h[0..2]);
        let exact = bucket.join(&h);
        if exact.is_file() {
            if seen.insert(h.clone()) {
                out.push((format!("blobs/{}/{h}", &h[0..2]), exact));
            }
            continue;
        }
        // 文件名可能是 `{hash}`，也可能后面还带后缀
        let Ok(rd) = std::fs::read_dir(&bucket) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with(&h) && e.path().is_file() {
                if seen.insert(h.clone()) {
                    out.push((format!("blobs/{}/{name}", &h[0..2]), e.path()));
                }
                break;
            }
        }
    }
    out
}

/// 往包里放条目的统一入口 —— 负责体积上限、跳过记录、校验表累积。
///
/// 一开始写成 `macro_rules!`，但宏里的 `continue` 在「不在循环里」的调用点会编译失败
/// （配置项那几处就是），所以改成显式结构。
struct BundleWriter<'a> {
    zip: &'a mut crate::modules::zipwriter::ZipWriter,
    checks: Vec<Value>,
    written: u64,
    skipped: Vec<String>,
}

impl BundleWriter<'_> {
    /// 放一个文件。返回 `Ok(false)` 表示**跳过**（读不到 / 太大 / 超包体上限前的不重要项）。
    fn put(&mut self, arch: &str, path: &Path, deflate: bool) -> Result<bool, String> {
        let Ok(meta) = path.metadata() else {
            self.skipped.push(format!("{arch}（读不到元信息）"));
            return Ok(false);
        };
        if meta.len() > u32::MAX as u64 {
            self.skipped
                .push(format!("{arch}（单文件超过 4 GiB，未启用 ZIP64）"));
            return Ok(false);
        }
        if self.written + meta.len() > MAX_BUNDLE_BYTES {
            return Err(format!(
                "包体积将超过 {:.1} GB（未启用 ZIP64）。请少选几个工作区，\
                 或关掉「工作区快照」「插件缓存」这类大件后重试。",
                MAX_BUNDLE_BYTES as f64 / 1e9
            ));
        }
        let Ok(bytes) = std::fs::read(path) else {
            self.skipped.push(format!("{arch}（读取失败）"));
            return Ok(false);
        };
        self.zip
            .add_file(arch, &bytes, deflate)
            .map_err(|e| format!("写入 {arch} 失败：{e}"))?;
        self.written += meta.len();
        self.checks.push(json!({
            "path": arch,
            "bytes": meta.len(),
            "crc32": format!("{:08x}", crc32fast::hash(&bytes)),
        }));
        Ok(true)
    }

    /// 放一段内存里的字节（manifest / checksums / README 这类由程序生成的条目）。
    fn put_bytes(&mut self, arch: &str, bytes: &[u8], deflate: bool) -> Result<(), String> {
        self.zip
            .add_file(arch, bytes, deflate)
            .map_err(|e| format!("写入 {arch} 失败：{e}"))
    }

    /// 放一个目录里的全部文件（先建目录条目）。
    fn put_dir(&mut self, dir: &Path, prefix: &str, deflate: bool) -> Result<usize, String> {
        self.zip
            .add_dir(prefix)
            .map_err(|e| format!("建目录条目 {prefix} 失败：{e}"))?;
        let mut n = 0;
        for (arch, path) in dir_files(dir, prefix) {
            if self.put(&arch, &path, deflate)? {
                n += 1;
            }
        }
        Ok(n)
    }
}

/// 导出：把选中的工作区 + 配置打成一个 zip。
pub fn export_bundle(
    edition: Edition,
    opts: &ExportOptions,
    out_path: &Path,
) -> Result<Value, String> {
    if opts.slugs.is_empty() {
        return Err("请至少选择一个工作区".to_string());
    }
    if out_path.exists() {
        return Err(format!("目标文件已存在，请换个路径：{}", out_path.display()));
    }

    // ---- 1) 定位会话 ----
    let mut slug_sessions: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut all_ids: BTreeSet<String> = BTreeSet::new();
    for slug in &opts.slugs {
        let ids = session_ids_in_slug(edition, slug);
        if ids.is_empty() {
            continue;
        }
        all_ids.extend(ids.iter().cloned());
        slug_sessions.insert(slug.clone(), ids);
    }
    if all_ids.is_empty() {
        return Err("选中的工作区里没有找到会话".to_string());
    }
    let rows = session_rows(edition, &all_ids);

    // ---- 2) 收集每个会话的文件 ----
    let mut per_session: BTreeMap<String, Vec<FileEntry>> = BTreeMap::new();
    for (slug, ids) in &slug_sessions {
        for sid in ids {
            let mut fs = session_project_files(edition, slug, sid);
            fs.extend(session_child_entry(&tasks_dir(edition), sid, "tasks"));
            fs.extend(session_child_entry(
                &artifact_index_dir(edition),
                sid,
                "artifact-index",
            ));
            if opts.include_file_history {
                fs.extend(session_child_entry(
                    &file_history_dir(edition),
                    sid,
                    "file-history",
                ));
            }
            if opts.include_workspace_snapshots {
                fs.extend(session_child_entry(
                    &workspace_sessions_dir(edition),
                    sid,
                    "workspace",
                ));
            }
            if !fs.is_empty() {
                per_session.insert(sid.clone(), fs);
            }
        }
    }

    // ---- 3) 附件（跨会话去重）----
    let flat: Vec<FileEntry> = per_session.values().flatten().cloned().collect();
    let blobs = reachable_blobs(edition, &flat);

    // ---- 4) 开写 ----
    let mut zip =
        crate::modules::zipwriter::ZipWriter::create(out_path).map_err(|e| e.to_string())?;
    let mut w = BundleWriter {
        zip: &mut zip,
        checks: Vec::new(),
        written: 0,
        skipped: Vec::new(),
    };

    // manifest 放最前面：读包时先看它就知道内容，不必解压全部。
    let ws_meta: Vec<Value> = slug_sessions
        .iter()
        .map(|(slug, ids)| json!({ "slug": slug, "sessions": ids.len(), "sessionIds": ids }))
        .collect();
    let manifest = json!({
        "format": BUNDLE_FORMAT,
        "version": BUNDLE_VERSION,
        "createdAt": now_ms(),
        "source": {
            "edition": edition.key(),
            "editionLabel": edition.label(),
            "hostname": hostname(),
            "os": std::env::consts::OS,
            "dataDir": edition.data_dir().to_string_lossy(),
            "uid": crate::modules::session::current_user_uid_for(edition),
        },
        "options": {
            "config": opts.include_config,
            "plugins": opts.include_plugins,
            "fileHistory": opts.include_file_history,
            "workspaceSnapshots": opts.include_workspace_snapshots,
            "credentials": opts.include_credentials,
        },
        "workspaces": ws_meta,
        "sessionCount": all_ids.len(),
        "blobCount": blobs.len(),
    });
    let manifest_text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    w.put_bytes("manifest.json", manifest_text.as_bytes(), true)?;

    // 会话：DB 行 + 文件
    for (sid, fs) in &per_session {
        if let Some(row) = rows.get(sid) {
            let text = serde_json::to_string_pretty(row).map_err(|e| e.to_string())?;
            w.put_bytes(&format!("sessions/{sid}/row.json"), text.as_bytes(), true)?;
        }
        for (arch, path) in fs {
            w.put(arch, path, true)?;
        }
    }

    // 附件：多为二进制，deflate 收益低，直接用 stored 换速度
    for (arch, path) in &blobs {
        w.put(arch, path, false)?;
    }

    // 配置
    if opts.include_config {
        let cfg = config_dir(edition);
        for (key, rel, is_dir, _label) in CONFIG_ITEMS {
            let p = cfg.join(rel);
            if !p.exists() {
                continue;
            }
            if *is_dir {
                w.put_dir(&p, &format!("config/{key}"), true)?;
            } else {
                w.put(&format!("config/{key}/{rel}"), &p, true)?;
            }
        }
        for table in ["automations", "workspaces"] {
            let rows = dump_table(edition, table);
            if rows.is_empty() {
                continue;
            }
            let text = serde_json::to_string_pretty(&json!(rows)).map_err(|e| e.to_string())?;
            w.put_bytes(&format!("config/db/{table}.json"), text.as_bytes(), true)?;
        }
    }
    if opts.include_plugins {
        for rel in ["plugins", "connectors-marketplace"] {
            let p = config_dir(edition).join(rel);
            if p.is_dir() {
                w.put_dir(&p, &format!("config/{rel}"), true)?;
            }
        }
    }

    // 凭证
    if opts.include_credentials {
        let auth = edition.auth_file_path();
        if auth.is_file() {
            w.put("credentials/auth.json", &auth, true)?;
        }
    }

    // 校验表 + 人可读说明
    let checks = std::mem::take(&mut w.checks);
    let raw = w.written;
    let skipped = std::mem::take(&mut w.skipped);
    let checks_text = serde_json::to_string_pretty(&json!({
        "generatedAt": now_ms(),
        "entries": checks,
    }))
    .map_err(|e| e.to_string())?;
    w.put_bytes("checksums.json", checks_text.as_bytes(), true)?;

    let readme = build_readme(edition, &manifest, raw, skipped.len());
    w.put_bytes("README.txt", readme.as_bytes(), true)?;

    let total = zip.finish().map_err(|e| e.to_string())?;

    Ok(json!({
        "path": out_path.to_string_lossy(),
        "bytes": total,
        "rawBytes": raw,
        "sessions": all_ids.len(),
        "workspaces": slug_sessions.len(),
        "blobs": blobs.len(),
        "files": checks.len(),
        "skipped": skipped,
        "credentials": opts.include_credentials,
    }))
}

/// 给人看的说明，放进包里。
fn build_readme(edition: Edition, manifest: &Value, raw: u64, skipped: usize) -> String {
    let mut s = String::new();
    s.push_str("WorkBuddy-Switch2api 迁移包\n");
    s.push_str("============================\n\n");
    s.push_str("这个包用于把 WorkBuddy 的会话与配置搬到另一台电脑（增量合并，\n");
    s.push_str("不会覆盖目标机器上已有的数据）。\n\n");
    s.push_str(&format!("来源版本：{}\n", edition.label()));
    s.push_str(&format!(
        "来源机器：{}\n",
        manifest["source"]["hostname"].as_str().unwrap_or("(未知)")
    ));
    s.push_str(&format!(
        "包内会话：{} 个\n",
        manifest["sessionCount"].as_u64().unwrap_or(0)
    ));
    s.push_str(&format!("原始体积：{:.1} MB\n", raw as f64 / 1e6));
    if skipped > 0 {
        s.push_str(&format!("跳过文件：{skipped} 个\n"));
    }
    s.push_str("\n怎么用\n------\n");
    s.push_str("打开 WorkBuddy-Switch2api → 迁移 → 导入，选择本文件。\n");
    s.push_str("导入前请完全退出 WorkBuddy 客户端；工具会自动备份本机数据。\n\n");
    s.push_str("目录说明\n--------\n");
    s.push_str("  manifest.json   包信息与清单（先读它就知道包里有什么）\n");
    s.push_str("  sessions/       每个会话一个目录：数据库行 + 正文 + 任务 + 附件\n");
    s.push_str("  config/         技能、MCP、连接器、设置、记忆等配置\n");
    s.push_str("  blobs/          会话引用到的附件（文件名即内容的 SHA256）\n");
    s.push_str("  credentials/    账号登录凭证（若导出时勾选）\n");
    s.push_str("  checksums.json  每个条目的 CRC32 与大小，可用于校验完整性\n");
    if manifest["options"]["credentials"].as_bool().unwrap_or(false) {
        s.push_str("\n⚠️ 本包含有账号登录凭证（accessToken / refreshToken）。\n");
        s.push_str("   拿到这个包的人就能以该账号身份使用 WorkBuddy，请妥善保管。\n");
    }
    s
}

/// 本机名（写进 manifest 便于识别来源；取不到就留空）。
fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 归档名 → 本地落点
// ---------------------------------------------------------------------------

/// 一个包内条目属于哪一类。
#[derive(Debug, Clone, PartialEq)]
enum Slot {
    /// `sessions/{sid}/row.json` —— 那一行进数据库，不落盘。
    SessionRow { sid: String },
    /// 会话附属文件（正文 / tasks / artifact-index / file-history / workspace）。
    SessionFile { sid: String },
    Blob,
    ConfigDir { key: String },
    ConfigFile { key: String },
    DbTable { table: String },
    Credential,
}

struct Dest {
    slot: Slot,
    path: PathBuf,
}

/// 解析归档名，得到「属于哪一类 + 落到本地哪个路径」。
///
/// 返回 `None` = 这个条目不需要落盘（目录条目、manifest 等元数据、或名字不合法）。
fn resolve_arch(edition: Edition, arch: &str) -> Option<Dest> {
    let arch = arch.trim_end_matches('/');

    // ---- 会话附属 ----
    if let Some(rest) = arch.strip_prefix("sessions/") {
        let mut it = rest.splitn(3, '/');
        let sid = it.next()?;
        let kind = it.next()?;
        let tail = it.next();
        if sid.is_empty() {
            return None;
        }
        // `sessions/{sid}/row.json` 是 DB 行，不走文件系统。
        if kind == "row.json" && tail.is_none() {
            return Some(Dest {
                slot: Slot::SessionRow { sid: sid.to_string() },
                path: PathBuf::new(),
            });
        }
        let tail = tail?;
        if tail.is_empty() {
            return None;
        }
        // 与导出侧一一对应（见 export_bundle 的 session_child_entry 调用）。
        // 🔴 只有 `projects` 的尾巴是 `{slug}/{name}`；
        // 其余四类的尾巴**已经带着 sid**（是 `{sid}` 目录或 `{sid}.json` 文件本身，
        // 见导出侧的 `session_child_entry`）。这里绝不能再 `.join(sid)` ——
        // 那会多套一层目录，客户端就找不到这些附属数据了。
        let base = match kind {
            "projects" => projects_dir(edition),
            "tasks" => tasks_dir(edition),
            "artifact-index" => artifact_index_dir(edition),
            "file-history" => file_history_dir(edition),
            "workspace" => workspace_sessions_dir(edition),
            _ => return None,
        };
        let path = safe_join(&base, tail).ok()?;
        return Some(Dest {
            slot: Slot::SessionFile {
                sid: sid.to_string(),
            },
            path,
        });
    }

    // ---- 附件 ----
    if let Some(rest) = arch.strip_prefix("blobs/") {
        if rest.is_empty() {
            return None;
        }
        let path = safe_join(&blobs_dir(edition), rest).ok()?;
        return Some(Dest {
            slot: Slot::Blob,
            path,
        });
    }

    // ---- 配置 ----
    if let Some(rest) = arch.strip_prefix("config/") {
        if let Some(t) = rest.strip_prefix("db/") {
            let table = t.strip_suffix(".json")?;
            return Some(Dest {
                slot: Slot::DbTable {
                    table: table.to_string(),
                },
                path: PathBuf::new(), // 走数据库，不落文件
            });
        }
        let (key, tail) = rest.split_once('/')?;
        if tail.is_empty() {
            return None;
        }
        // 目录型：`config/{key}/…rest` → `config/{rel}/…rest`
        // 文件型：`config/{key}/{rel}` → `config/{rel}`（key 已唯一决定目标文件）
        if let Some((_, rel, is_dir, _)) = CONFIG_ITEMS.iter().find(|(k, _, _, _)| *k == key) {
            if *is_dir {
                let path = safe_join(&config_dir(edition).join(rel), tail).ok()?;
                return Some(Dest {
                    slot: Slot::ConfigDir {
                        key: key.to_string(),
                    },
                    path,
                });
            }
            return Some(Dest {
                slot: Slot::ConfigFile {
                    key: key.to_string(),
                },
                path: config_dir(edition).join(rel),
            });
        }
        // 插件缓存这类不在 CONFIG_ITEMS 里的目录，key 就是目录名。
        if key == "plugins" || key == "connectors-marketplace" {
            let path = safe_join(&config_dir(edition).join(key), tail).ok()?;
            return Some(Dest {
                slot: Slot::ConfigDir {
                    key: key.to_string(),
                },
                path,
            });
        }
        return None;
    }

    // ---- 凭证 ----
    if arch == "credentials/auth.json" {
        return Some(Dest {
            slot: Slot::Credential,
            path: edition.auth_file_path(),
        });
    }

    None
}

/// 这个配置项的 markdown 文件该不该「行级追加合并」。
///
/// 只有**记忆类**才该合并 —— 两台机器各记了一些，合起来才对。
/// 技能目录里的 `SKILL.md` 是文档：目标已有就该原样不动，
/// 把两边的行拼在一起只会得到一份缝合怪。
fn is_mergeable_md(key: &str) -> bool {
    matches!(key, "memory" | "soul" | "user_md" | "identity")
}

/// 配置项 key → 人可读标签。
fn config_label(key: &str) -> String {
    if key == "plugins" {
        return "插件缓存".to_string();
    }
    if key == "connectors-marketplace" {
        return "连接器市场缓存".to_string();
    }
    CONFIG_ITEMS
        .iter()
        .find(|(k, _, _, _)| *k == key)
        .map(|(_, _, _, label)| (*label).to_string())
        .unwrap_or_else(|| key.to_string())
}

// ---------------------------------------------------------------------------
// 增量合并的两个基础动作
// ---------------------------------------------------------------------------

/// 递归合并 JSON：**目标已有的键一律保留**，只补进来的。
///
/// 返回新增的叶子数（标量或新键的个数），用于给用户报告「实际新增了多少」。
fn deep_merge(dst: &mut Value, src: &Value) -> usize {
    match (dst, src) {
        (Value::Object(d), Value::Object(s)) => {
            let mut n = 0;
            for (k, v) in s {
                match d.get_mut(k) {
                    Some(dv) => n += deep_merge(dv, v),
                    None => {
                        d.insert(k.clone(), v.clone());
                        n += 1;
                    }
                }
            }
            n
        }
        (Value::Array(d), Value::Array(s)) => {
            // 大数组不做 O(n²) 去重 —— 收益远小于代价。
            if d.len() > 2000 || s.len() > 2000 {
                return 0;
            }
            let mut n = 0;
            for v in s {
                if !d.contains(v) {
                    d.push(v.clone());
                    n += 1;
                }
            }
            n
        }
        // 标量冲突：目标赢。
        _ => 0,
    }
}

/// 行级合并 markdown：保留目标现有内容，把源里**没有的行**追加到末尾。
///
/// 长期记忆就是这样 —— 只增不减，两边各记了一些，合起来才对。
fn merge_markdown_lines(dst: &str, src: &str) -> (String, usize) {
    let have: BTreeSet<&str> = dst.lines().map(|l| l.trim_end()).collect();
    let mut added = Vec::new();
    for line in src.lines() {
        let key = line.trim_end();
        if key.trim().is_empty() || have.contains(key) {
            continue;
        }
        added.push(line);
    }
    if added.is_empty() {
        return (dst.to_string(), 0);
    }
    let mut out = dst.trim_end().to_string();
    out.push('\n');
    for l in &added {
        out.push_str(l);
        out.push('\n');
    }
    (out, added.len())
}

// ---------------------------------------------------------------------------
// 备份
// ---------------------------------------------------------------------------

/// 导入前的自动备份：把将被覆盖的每个文件原样复制一份。
///
/// 落点：`~/.wb-switch/transfer-backups/{时间戳}/`，**镜像原绝对路径**
/// （`C:\Users\x\a.txt` → `{备份}/C/Users/x/a.txt`），需要回滚时直接拷回去即可。
struct Backup {
    root: PathBuf,
    saved: BTreeSet<PathBuf>,
    files: u64,
    bytes: u64,
}

impl Backup {
    fn new() -> Result<Self, String> {
        let stamp = now_ms();
        let root = crate::modules::config::store_dir()
            .join("transfer-backups")
            .join(stamp.to_string());
        std::fs::create_dir_all(&root).map_err(|e| format!("建备份目录失败：{e}"))?;
        Ok(Self {
            root,
            saved: BTreeSet::new(),
            files: 0,
            bytes: 0,
        })
    }

    /// 备份一个**已存在**的文件。不存在 / 已备份过 → 什么都不做。
    fn save(&mut self, path: &Path) -> Result<(), String> {
        if !path.is_file() || self.saved.contains(path) {
            return Ok(());
        }
        let rel = flatten_path(path);
        let dst = self.root.join(&rel);
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("建备份子目录失败：{e}"))?;
        }
        let n = std::fs::copy(path, &dst)
            .map_err(|e| format!("备份 {} 失败：{e}", path.display()))?;
        self.saved.insert(path.to_path_buf());
        self.files += 1;
        self.bytes += n;
        Ok(())
    }

    fn report(&self) -> Value {
        json!({
            "dir": self.root.to_string_lossy(),
            "files": self.files,
            "bytes": self.bytes,
        })
    }
}

/// 绝对路径 → 备份目录内的相对路径（去掉盘符冒号，把分隔符统一成 `/`）。
fn flatten_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches('/').replacen(':', "", 1);
    PathBuf::from(s)
}

// ---------------------------------------------------------------------------
// 预览
// ---------------------------------------------------------------------------

/// 会话在包里的清单项（sid 是 map 的 key，不重复存）。
struct SessionBrief {
    title: String,
    slug: String,
    updated_at: i64,
    files: u64,
    bytes: u64,
}

/// 本地全部会话 id（扫一遍 `projects/*/`，用于判断「已存在」）。
fn local_session_ids(edition: Edition) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let root = projects_dir(edition);
    let Ok(rd) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in rd.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(entry.path()) else {
            continue;
        };
        for f in inner.flatten() {
            let name = f.file_name().to_string_lossy().to_string();
            let Some(base) = name.split('.').next() else {
                continue;
            };
            if base.len() == 36 && base.matches('-').count() == 4 {
                out.insert(base.to_string());
            }
        }
    }
    out
}

/// 解析包的基本头信息，并校验格式/版本。预览与导入共用。
fn read_bundle_header(z: &ZipReader, zip_path: &Path) -> Result<Value, String> {
    let manifest = z
        .read_json("manifest.json")
        .map_err(|e| format!("读 manifest.json 失败：{e}"))?;
    let fmt = manifest
        .get("format")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if fmt != BUNDLE_FORMAT {
        return Err(format!(
            "这不是本工具产出的迁移包（format = {fmt:?}，期望 {BUNDLE_FORMAT:?}）。\
             如果你拿到的是一份手工压缩的目录，请用「导出」重新生成。"
        ));
    }
    let ver = manifest.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
    if ver == 0 {
        return Err("包缺少 version 字段，无法判断兼容性".to_string());
    }
    if ver > BUNDLE_VERSION as u64 {
        return Err(format!(
            "包是新版本（v{ver}）导出的，当前程序只认到 v{BUNDLE_VERSION}。请先升级本工具。"
        ));
    }
    let _ = zip_path;
    Ok(manifest)
}

/// 预览导入：只读，不动任何数据。返回「将要发生什么」的完整清单。
pub fn preview_bundle(edition: Edition, zip_path: &Path) -> Result<Value, String> {
    let z = ZipReader::open(zip_path).map_err(|e| format!("打开迁移包失败：{e}"))?;
    let manifest = read_bundle_header(&z, zip_path)?;
    let local_ids = local_session_ids(edition);

    // ---- 逐条目分类统计 ----
    let mut sessions: BTreeMap<String, SessionBrief> = BTreeMap::new();
    let mut blob_total = 0u64;
    let mut blob_missing = 0u64;
    let mut blob_bytes_missing = 0u64;
    let mut blob_bytes_total = 0u64;
    // key → (条目数, 字节数, 本地已有数, 一级子项集合)
    let mut cfg: BTreeMap<String, (u64, u64, u64, BTreeSet<String>)> = BTreeMap::new();
    let mut db_files: BTreeMap<String, String> = BTreeMap::new();
    // 包里有 row.json 的会话 id（这些行会进 sessions 表）。
    let mut session_row_ids: BTreeSet<String> = BTreeSet::new();
    let mut credential: Option<Value> = None;
    let mut unknown: Vec<String> = Vec::new();

    for e in z.entries() {
        let arch = e.name.as_str();
        if arch.ends_with('/') {
            continue;
        }
        match resolve_arch(edition, arch) {
            None => {
                if arch != "manifest.json" && arch != "checksums.json" && arch != "README.txt" {
                    unknown.push(arch.to_string());
                }
            }
            Some(d) => match d.slot {
                Slot::SessionRow { sid } => {
                    session_row_ids.insert(sid);
                }
                Slot::SessionFile { sid } => {
                    let entry = sessions.entry(sid.clone()).or_insert(SessionBrief {
                        title: String::new(),
                        slug: String::new(),
                        updated_at: 0,
                        files: 0,
                        bytes: 0,
                    });
                    entry.files += 1;
                    entry.bytes += e.uncomp_size;
                }
                Slot::Blob => {
                    blob_total += 1;
                    blob_bytes_total += e.uncomp_size;
                    if !d.path.is_file() {
                        blob_missing += 1;
                        blob_bytes_missing += e.uncomp_size;
                    }
                }
                Slot::ConfigDir { key } => {
                    let top = arch
                        .split('/')
                        .nth(2)
                        .unwrap_or("")
                        .to_string();
                    let rec = cfg.entry(key).or_insert((0, 0, 0, BTreeSet::new()));
                    rec.0 += 1;
                    rec.1 += e.uncomp_size;
                    if !d.path.is_file() {
                        rec.2 += 1;
                    }
                    if !top.is_empty() {
                        rec.3.insert(top);
                    }
                }
                Slot::ConfigFile { key } => {
                    let rec = cfg.entry(key).or_insert((0, 0, 0, BTreeSet::new()));
                    rec.0 += 1;
                    rec.1 += e.uncomp_size;
                    if !d.path.is_file() {
                        rec.2 += 1;
                    }
                }
                Slot::DbTable { table } => {
                    db_files.insert(table, arch.to_string());
                }
                Slot::Credential => {
                    // 只读出来看 uid / nickname，不落盘。
                    if let Ok(v) = z.read_json(arch) {
                        credential = Some(json!({
                            "uid": v.get("account").and_then(|a| a.get("uid")).cloned().unwrap_or(Value::Null),
                            "nickname": v.get("account").and_then(|a| a.get("nickname")).cloned().unwrap_or(Value::Null),
                            "hasAccessToken": v.get("auth").and_then(|a| a.get("accessToken")).is_some(),
                        }));
                    }
                }
            },
        }
    }

    // ---- 会话的标题等信息（读 row.json） ----
    let mut session_list: Vec<Value> = Vec::new();
    let mut sess_new = 0u64;
    let mut sess_existing = 0u64;
    for (sid, mut s) in sessions {
        if let Ok(row) = z.read_json(&format!("sessions/{sid}/row.json")) {
            s.title = row
                .get("title")
                .or_else(|| row.get("custom_title"))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let cwd = row.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
            if !cwd.is_empty() {
                s.slug = slug_for_cwd(cwd);
            }
            s.updated_at = row.get("updated_at").and_then(|v| v.as_i64()).unwrap_or(0);
        }
        // 本地已有？按 sid 全局判断（cwd 可能为空，不能只看 slug）。
        let exists = local_ids.contains(&sid)
            || (!s.slug.is_empty() && projects_dir(edition).join(&s.slug).join(format!("{sid}.jsonl")).is_file());
        if exists {
            sess_existing += 1;
        } else {
            sess_new += 1;
        }
        session_list.push(json!({
            "sid": sid,
            "title": if s.title.is_empty() { short_title(&s.slug) } else { s.title.clone() },
            "slug": s.slug,
            "updatedAt": s.updated_at,
            "files": s.files,
            "bytes": s.bytes,
            "exists": exists,
        }));
    }
    session_list.sort_by(|a, b| {
        let av = a.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        let bv = b.get("updatedAt").and_then(|v| v.as_i64()).unwrap_or(0);
        bv.cmp(&av)
    });

    // ---- 配置预览 ----
    let mut cfg_list: Vec<Value> = Vec::new();
    for (key, (count, bytes, missing, tops)) in &cfg {
        let is_dir = CONFIG_ITEMS
            .iter()
            .find(|(k, _, _, _)| k == key)
            .map(|(_, _, d, _)| *d)
            .unwrap_or(true);
        let sample: Vec<String> = tops.iter().take(8).cloned().collect();
        cfg_list.push(json!({
            "key": key,
            "label": config_label(key),
            "isDir": is_dir,
            "incoming": count,
            "incomingBytes": bytes,
            "missingLocally": missing,
            "topItems": tops.len(),
            "sample": sample,
        }));
    }
    cfg_list.sort_by(|a, b| {
        let av = a.get("incomingBytes").and_then(|v| v.as_u64()).unwrap_or(0);
        let bv = b.get("incomingBytes").and_then(|v| v.as_u64()).unwrap_or(0);
        bv.cmp(&av)
    });

    // ---- DB 行预览 ----
    let mut db_preview = serde_json::Map::new();
    let mut db_total_new = 0u64;
    for (table, arch) in &db_files {
        let Ok(rows) = z.read_json(arch) else { continue };
        let Some(arr) = rows.as_array() else { continue };
        let incoming = arr.len() as u64;
        let existing = match table.as_str() {
            "sessions" => arr
                .iter()
                .filter(|r| {
                    r.get("id")
                        .and_then(|v| v.as_str())
                        .map(|id| local_ids.contains(id))
                        .unwrap_or(false)
                })
                .count() as u64,
            _ => {
                let local = dump_table(edition, table);
                let local_ids_set: BTreeSet<String> = local
                    .iter()
                    .filter_map(|r| row_identity(r))
                    .collect();
                arr.iter()
                    .filter(|r| row_identity(r).map(|i| local_ids_set.contains(&i)).unwrap_or(false))
                    .count() as u64
            }
        };
        db_total_new += incoming.saturating_sub(existing);
        db_preview.insert(
            table.clone(),
            json!({ "incoming": incoming, "existing": existing, "new": incoming.saturating_sub(existing) }),
        );
    }
    // sessions 表：行来自 `sessions/{sid}/row.json`，不是 config/db。
    if !session_row_ids.is_empty() {
        let existing = session_row_ids
            .iter()
            .filter(|sid| local_ids.contains(*sid))
            .count() as u64;
        let incoming = session_row_ids.len() as u64;
        db_total_new += incoming.saturating_sub(existing);
        db_preview.insert(
            "sessions".to_string(),
            json!({ "incoming": incoming, "existing": existing,
                    "new": incoming.saturating_sub(existing) }),
        );
    }

    // ---- 警告 ----
    let mut warnings: Vec<String> = Vec::new();
    let src_edition = manifest
        .get("source")
        .and_then(|s| s.get("edition"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !src_edition.is_empty() && src_edition != edition.key() {
        warnings.push(format!(
            "包来自{}，正在导入到{}。两边的插件/连接器状态不能通用，但会话与 skills 可以。",
            edition_label_of(src_edition),
            edition.label()
        ));
    }
    if !unknown.is_empty() {
        warnings.push(format!(
            "包里有 {} 个条目不认识（已跳过），可能来自更新的版本：{}",
            unknown.len(),
            unknown.iter().take(3).cloned().collect::<Vec<_>>().join("、")
        ));
    }
    let target_uid = crate::modules::session::current_user_uid_for(edition);
    if target_uid.is_none() {
        warnings.push(
            "当前档位没有登录账号。导入的会话会被置为「共享」（对任意账号可见），\
             但客户端里可能要在切换账号后才显示。"
                .to_string(),
        );
    }
    if sess_existing > 0 {
        warnings.push(format!(
            "有 {sess_existing} 个会话在本地已存在，默认不覆盖（只补缺失的附属文件）。"
        ));
    }

    Ok(json!({
        "ok": true,
        "path": zip_path.to_string_lossy(),
        "manifest": manifest,
        "source": manifest.get("source").cloned().unwrap_or(Value::Null),
        "options": manifest.get("options").cloned().unwrap_or(Value::Null),
        "target": {
            "edition": edition.key(),
            "editionLabel": edition.label(),
            "dataDir": edition.data_dir().to_string_lossy(),
            "uid": target_uid,
        },
        "sessions": session_list,
        "summary": {
            "sessions": { "total": sess_new + sess_existing, "new": sess_new, "existing": sess_existing },
            "blobs": { "total": blob_total, "missing": blob_missing,
                       "bytesTotal": blob_bytes_total, "bytesMissing": blob_bytes_missing },
            "config": cfg_list,
            "db": db_preview,
            "dbNew": db_total_new,
            "credentials": credential,
        },
        "warnings": warnings,
    }))
}

/// 取一行记录的稳定身份（用于判重）。
fn row_identity(row: &Value) -> Option<String> {
    for k in ["id", "key", "name", "slug"] {
        if let Some(v) = row.get(k).and_then(|v| v.as_str()) {
            if !v.is_empty() {
                return Some(format!("{k}:{v}"));
            }
        }
    }
    None
}

fn edition_label_of(key: &str) -> String {
    // 取值见 `Edition::key()`：domestic / international。
    match key {
        "domestic" => "国内版".to_string(),
        "international" => "国际版".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 导入
// ---------------------------------------------------------------------------

/// 导入选项。
#[derive(Debug, Clone)]
pub struct ImportOptions {
    /// 要导入的会话 id；空 = 包里全部。
    pub session_ids: Vec<String>,
    pub apply_sessions: bool,
    pub apply_blobs: bool,
    pub apply_config: bool,
    /// 只导入这些配置项 key；空 = 全部。
    pub config_keys: Vec<String>,
    pub apply_db: bool,
    pub apply_credentials: bool,
    /// 目标已有同名文件时是否覆盖。`false` = 只补缺失（默认）。
    pub overwrite: bool,
    /// 把会话的 `user_id` 置空 —— 变成「共享会话」，任意账号都可见。
    ///
    /// 不这么做的话，导入的会话带着**源机器**的 uid，在目标机器上根本不会被列出来。
    pub share_sessions: bool,
}

impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            session_ids: Vec::new(),
            apply_sessions: true,
            apply_blobs: true,
            apply_config: true,
            config_keys: Vec::new(),
            apply_db: true,
            apply_credentials: false,
            overwrite: false,
            share_sessions: true,
        }
    }
}

/// 往表里补行。返回 (新增, 跳过)。
fn insert_rows(
    conn: &rusqlite::Connection,
    table: &str,
    rows: &[Value],
    replace: bool,
) -> Result<(usize, usize), String> {
    if rows.is_empty() {
        return Ok((0, 0));
    }
    // 目标表有哪些列 —— 包里多出来的键直接忽略（可能是更高版本加的）。
    let cols: Vec<String> = {
        let stmt = conn
            .prepare(&format!("SELECT * FROM \"{table}\" LIMIT 0"))
            .map_err(|e| format!("表 {table} 不可读：{e}"))?;
        stmt.column_names().iter().map(|s| (*s).to_string()).collect()
    };
    if cols.is_empty() {
        return Ok((0, 0));
    }

    let keeplist: Vec<&String> = cols.iter().collect();
    let placeholders = vec!["?"; keeplist.len()].join(",");
    let quoted = keeplist
        .iter()
        .map(|c| format!("\"{c}\""))
        .collect::<Vec<_>>()
        .join(",");
    let verb = if replace { "INSERT OR REPLACE" } else { "INSERT OR IGNORE" };
    let sql = format!("{verb} INTO \"{table}\" ({quoted}) VALUES ({placeholders})");

    let mut inserted = 0usize;
    let mut skipped = 0usize;
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| format!("准备写入 {table} 失败：{e}"))?;
    for row in rows {
        let obj = match row.as_object() {
            Some(o) => o,
            None => continue,
        };
        let params: Vec<rusqlite::types::Value> = keeplist
            .iter()
            .map(|c| match obj.get(*c) {
                Some(Value::Null) | None => rusqlite::types::Value::Null,
                Some(Value::Bool(b)) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
                Some(Value::Number(n)) => {
                    if let Some(i) = n.as_i64() {
                        rusqlite::types::Value::Integer(i)
                    } else {
                        rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
                    }
                }
                Some(Value::String(s)) => rusqlite::types::Value::Text(s.clone()),
                // 嵌套结构存成 JSON 字符串 —— 客户端就是这么存 JSON 列的。
                Some(other) => rusqlite::types::Value::Text(other.to_string()),
            })
            .collect();
        match stmt.execute(rusqlite::params_from_iter(params.iter())) {
            Ok(0) => skipped += 1,
            Ok(_) => inserted += 1,
            Err(e) => return Err(format!("写入 {table} 失败：{e}")),
        }
    }
    Ok((inserted, skipped))
}

/// 导入：按选项把包里的内容**增量合并**进来。
pub fn import_bundle(
    edition: Edition,
    zip_path: &Path,
    opts: &ImportOptions,
) -> Result<Value, String> {
    let z = ZipReader::open(zip_path).map_err(|e| format!("打开迁移包失败：{e}"))?;
    let manifest = read_bundle_header(&z, zip_path)?;
    let _ = manifest;

    let picked: BTreeSet<String> = opts.session_ids.iter().cloned().collect();
    let cfg_pick: BTreeSet<String> = opts.config_keys.iter().cloned().collect();

    let mut backup = Backup::new()?;
    let mut errors: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    let mut n_session_files = 0u64;
    let mut n_session_bytes = 0u64;
    let mut n_session_skipped = 0u64;
    let mut n_blobs = 0u64;
    let mut n_blob_bytes = 0u64;
    let mut n_blob_skipped = 0u64;
    let mut n_cfg_new = 0u64;
    let mut n_cfg_merged = 0u64;
    let mut n_cfg_bytes = 0u64;
    let mut n_cfg_skipped = 0u64;
    let mut n_cfg_keys_merged: BTreeSet<String> = BTreeSet::new();
    let mut done_sessions: BTreeSet<String> = BTreeSet::new();
    let mut credential_written = false;

    // ---- 1) 会话 + 附件 + 配置：逐条目落盘 ----
    let entries: Vec<(String, Slot, PathBuf)> = z
        .entries()
        .iter()
        .filter(|e| !e.is_dir())
        .filter_map(|e| {
            resolve_arch(edition, &e.name).map(|d| (e.name.clone(), d.slot, d.path))
        })
        .collect();

    for (arch, slot, dest) in entries {
        match slot {
            Slot::SessionFile { sid } => {
                if !opts.apply_sessions {
                    continue;
                }
                if !picked.is_empty() && !picked.contains(&sid) {
                    continue;
                }
                // 会话已存在且不覆盖 → 只补缺失的附属文件。
                if dest.is_file() && !opts.overwrite {
                    n_session_skipped += 1;
                    continue;
                }
                if let Err(e) = backup.save(&dest) {
                    errors.push(e);
                    continue;
                }
                match z.extract_to(&arch, &dest) {
                    Ok(n) => {
                        n_session_files += 1;
                        n_session_bytes += n;
                        done_sessions.insert(sid);
                    }
                    Err(e) => errors.push(format!("{arch}: {e}")),
                }
            }

            Slot::Blob => {
                if !opts.apply_blobs {
                    continue;
                }
                // 文件名就是 SHA256 —— 存在即同一个文件，跳过。
                if dest.is_file() {
                    n_blob_skipped += 1;
                    continue;
                }
                match z.extract_to(&arch, &dest) {
                    Ok(n) => {
                        n_blobs += 1;
                        n_blob_bytes += n;
                    }
                    Err(e) => errors.push(format!("{arch}: {e}")),
                }
            }

            Slot::ConfigDir { key } => {
                if !opts.apply_config || (!cfg_pick.is_empty() && !cfg_pick.contains(&key)) {
                    continue;
                }
                n_cfg_keys_merged.insert(key.clone());

                // 长期记忆（markdown）走「行级追加」，其余按文件补缺 / 覆盖。
                let is_md = dest
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("md"))
                    .unwrap_or(false);
                if is_md && is_mergeable_md(&key) && dest.is_file() {
                    match (z.read_text(&arch), std::fs::read_to_string(&dest)) {
                        (Ok(src), Ok(dst)) => {
                            let (merged, added) = merge_markdown_lines(&dst, &src);
                            if added > 0 {
                                if let Err(e) = backup.save(&dest) {
                                    errors.push(e);
                                } else if let Err(e) = std::fs::write(&dest, merged.as_bytes()) {
                                    errors.push(format!("合并记忆 {} 失败：{e}", dest.display()));
                                } else {
                                    n_cfg_merged += added as u64;
                                    n_cfg_bytes += merged.len() as u64;
                                }
                            }
                        }
                        (Err(e), _) => errors.push(format!("{arch}: {e}")),
                        (_, Err(e)) => errors.push(format!("读 {} 失败：{e}", dest.display())),
                    }
                    continue;
                }

                if dest.is_file() && !opts.overwrite {
                    n_cfg_skipped += 1;
                    continue;
                }
                if let Err(e) = backup.save(&dest) {
                    errors.push(e);
                    continue;
                }
                match z.extract_to(&arch, &dest) {
                    Ok(n) => {
                        n_cfg_new += 1;
                        n_cfg_bytes += n;
                    }
                    Err(e) => errors.push(format!("{arch}: {e}")),
                }
            }

            Slot::ConfigFile { key } => {
                if !opts.apply_config || (!cfg_pick.is_empty() && !cfg_pick.contains(&key)) {
                    continue;
                }
                n_cfg_keys_merged.insert(key.clone());

                // SOUL.md / USER.md / IDENTITY.md 这类是 markdown，不是 JSON ——
                // 按行级追加合并（与长期记忆同一套语义）。
                let is_json = dest
                    .extension()
                    .map(|e| e.eq_ignore_ascii_case("json"))
                    .unwrap_or(false);
                if !is_json {
                    let text = match z.read_text(&arch) {
                        Ok(t) => t,
                        Err(e) => {
                            errors.push(format!("{arch}: {e}"));
                            continue;
                        }
                    };
                    if dest.is_file() {
                        let dst = std::fs::read_to_string(&dest).unwrap_or_default();
                        let (merged, added) = merge_markdown_lines(&dst, &text);
                        if added == 0 {
                            n_cfg_skipped += 1;
                            continue;
                        }
                        if let Err(e) = backup.save(&dest) {
                            errors.push(e);
                            continue;
                        }
                        std::fs::write(&dest, merged.as_bytes())
                            .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
                        n_cfg_merged += added as u64;
                        n_cfg_bytes += merged.len() as u64;
                    } else {
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent).ok();
                        }
                        std::fs::write(&dest, text.as_bytes())
                            .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
                        n_cfg_new += 1;
                        n_cfg_bytes += text.len() as u64;
                    }
                    continue;
                }

                let Ok(src) = z.read_json(&arch) else {
                    errors.push(format!("{arch}: 不是合法 JSON，已跳过"));
                    continue;
                };
                if dest.is_file() {
                    let local = std::fs::read_to_string(&dest)
                        .ok()
                        .and_then(|t| serde_json::from_str::<Value>(&t).ok());
                    match local {
                        Some(mut lv) => {
                            let added = deep_merge(&mut lv, &src);
                            if added == 0 {
                                n_cfg_skipped += 1;
                                continue;
                            }
                            let text = serde_json::to_string_pretty(&lv)
                                .map_err(|e| e.to_string())?;
                            if let Err(e) = backup.save(&dest) {
                                errors.push(e);
                                continue;
                            }
                            std::fs::write(&dest, text.as_bytes())
                                .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
                            n_cfg_merged += added as u64;
                            n_cfg_bytes += text.len() as u64;
                        }
                        None => {
                            // 本地这份不是 JSON（或被写坏了）—— 备份后整体替换。
                            notes.push(format!(
                                "{} 不是合法 JSON，已按整文件覆盖",
                                dest.display()
                            ));
                            if let Err(e) = backup.save(&dest) {
                                errors.push(e);
                                continue;
                            }
                            let text =
                                serde_json::to_string_pretty(&src).map_err(|e| e.to_string())?;
                            std::fs::write(&dest, text.as_bytes())
                                .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
                            n_cfg_new += 1;
                        }
                    }
                } else {
                    let text = serde_json::to_string_pretty(&src).map_err(|e| e.to_string())?;
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    std::fs::write(&dest, text.as_bytes())
                        .map_err(|e| format!("写 {} 失败：{e}", dest.display()))?;
                    n_cfg_new += 1;
                    n_cfg_bytes += text.len() as u64;
                }
            }

            // 这两类不落文件：DB 行与凭证都在后面的专门阶段处理。
            Slot::SessionRow { .. } | Slot::DbTable { .. } | Slot::Credential => {}
        }
    }

    // ---- 2) DB 行 ----
    let mut db_report = serde_json::Map::new();
    if opts.apply_db {
        let db = workbuddy_db_path_for(edition);
        if db.is_file() {
            if let Err(e) = backup.save(&db) {
                errors.push(e);
            }
            let conn = rusqlite::Connection::open(&db)
                .map_err(|e| format!("打开数据库失败（客户端是否在运行？）：{e}"))?;
            let _ = conn.busy_timeout(std::time::Duration::from_secs(10));
            // WAL 下把已提交内容并回主库，缩短后续写入的占用时间。
            let _ = conn.pragma_update(None, "journal_mode", "WAL");

            // sessions：按 id 补，可选把 user_id 置空（共享会话）。
            let mut session_rows: Vec<Value> = Vec::new();
            for sid in z
                .names_with_prefix("sessions/")
                .iter()
                .filter_map(|n| n.strip_prefix("sessions/"))
                .filter_map(|r| r.split('/').next())
                .collect::<BTreeSet<_>>()
            {
                if !picked.is_empty() && !picked.contains(sid) {
                    continue;
                }
                if let Ok(mut row) = z.read_json(&format!("sessions/{sid}/row.json")) {
                    if opts.share_sessions {
                        if let Some(o) = row.as_object_mut() {
                            o.insert("user_id".to_string(), Value::String(String::new()));
                        }
                    }
                    session_rows.push(row);
                }
            }
            match insert_rows(&conn, "sessions", &session_rows, opts.overwrite) {
                Ok((i, s)) => {
                    db_report.insert(
                        "sessions".to_string(),
                        json!({ "inserted": i, "skipped": s, "total": session_rows.len() }),
                    );
                }
                Err(e) => errors.push(e),
            }

            for table in ["automations", "workspaces"] {
                let arch = format!("config/db/{table}.json");
                let Ok(rows) = z.read_json(&arch) else { continue };
                let Some(arr) = rows.as_array() else { continue };
                match insert_rows(&conn, table, arr, false) {
                    Ok((i, s)) => {
                        db_report.insert(
                            table.to_string(),
                            json!({ "inserted": i, "skipped": s, "total": arr.len() }),
                        );
                    }
                    Err(e) => errors.push(e),
                }
            }
        } else {
            notes.push("目标机器还没有数据库（客户端从没启动过？），这次的 DB 行没导入".to_string());
        }
    }

    // ---- 3) 凭证 ----
    if opts.apply_credentials && z.contains("credentials/auth.json") {
        let dest = edition.auth_file_path();
        match z.read("credentials/auth.json") {
            Ok(bytes) => {
                if let Err(e) = backup.save(&dest) {
                    errors.push(e);
                } else {
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    match std::fs::write(&dest, &bytes) {
                        Ok(()) => credential_written = true,
                        Err(e) => errors.push(format!("写认证文件失败：{e}")),
                    }
                }
            }
            Err(e) => errors.push(format!("读包内凭证失败：{e}")),
        }
    }

    if done_sessions.is_empty() && opts.apply_sessions && !picked.is_empty() {
        notes.push("选中的会话在包里没有对应文件，什么都没导入".to_string());
    }
    if n_session_skipped > 0 {
        notes.push(format!(
            "{n_session_skipped} 个会话文件在本地已存在且未勾选「覆盖」，已跳过（只补了缺失的）"
        ));
    }

    Ok(json!({
        "ok": errors.is_empty(),
        "path": zip_path.to_string_lossy(),
        "sessions": {
            "files": n_session_files,
            "bytes": n_session_bytes,
            "skipped": n_session_skipped,
            "ids": done_sessions.len(),
            "shared": opts.share_sessions,
        },
        "blobs": { "files": n_blobs, "bytes": n_blob_bytes, "skipped": n_blob_skipped },
        "config": {
            "newFiles": n_cfg_new,
            "mergedLeaves": n_cfg_merged,
            "skipped": n_cfg_skipped,
            "bytes": n_cfg_bytes,
            "keys": n_cfg_keys_merged.into_iter().collect::<Vec<_>>(),
        },
        "db": Value::Object(db_report),
        "credentials": { "written": credential_written },
        "backup": backup.report(),
        "errors": errors,
        "notes": notes,
    }))
}

// ---------------------------------------------------------------------------
// 账号间复制会话时的附属数据
// ---------------------------------------------------------------------------

/// 一次附属数据复制的统计。
///
/// 存在的意义：`copy_session_to_user` 以前只搬正文，用户看到的后果是
/// 「会话在、工具输出和产物索引没了」。现在把这件事量化报出来，缺什么一眼能看见。
#[derive(Debug, Default, Clone)]
pub(crate) struct SideCopyReport {
    pub files: usize,
    pub bytes: u64,
    /// 正文里引用到的 SHA256 个数。
    pub blobs_referenced: usize,
    /// 引用了、但附件仓库里找不到的个数（正常应该是 0；非 0 说明附件被 GC 过）。
    pub blobs_missing: usize,
    /// 源会话有没有工作区快照（有也不搬，见函数头注释）。
    pub snapshot_present: bool,
    pub errors: Vec<String>,
}

impl SideCopyReport {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "sideFiles": self.files,
            "sideBytes": self.bytes,
            "blobsReferenced": self.blobs_referenced,
            "blobsMissing": self.blobs_missing,
            "snapshotNotCopied": self.snapshot_present,
            "errors": self.errors,
        })
    }
}

/// 复制时要用的目录集合（抽出来是为了能用临时目录做单测，不必碰真实数据目录）。
pub(crate) struct SideDirs {
    /// `projects/{slug}` —— 里面有 `{sid}.jsonl`、`.meta.json`、`{sid}/` 等。
    pub project_dir: Option<PathBuf>,
    pub tasks: PathBuf,
    pub artifact_index: PathBuf,
    pub file_history: PathBuf,
    pub workspace: PathBuf,
    pub blobs: PathBuf,
}

/// 把一个会话的**附属数据**复制到新会话 id 之下。
///
/// # 为什么需要它
///
/// `session::copy_session_to_user_for` 以前只复制 `{sid}.jsonl`。新会话 id 一变，
/// 那些**按旧 id 命名**的附属数据就全对不上了 —— 工具输出外溢目录、产物索引、
/// 历史任务、文件改动历史，目标账号看到的会话是"内容在、东西没了"。
///
/// # 为什么不复制 blobs
///
/// `blobs/` 是**内容寻址的全局仓库**（`blobs/{sha256 前两位}/{sha256}`），
/// **不分账号**：新会话引用的是同样的 SHA256，文件本来就在原地。
/// 复制只会白抄几百 MB —— 这里只做一次引用完整性检查。
///
/// # 不搬的
///
/// `workspace/sessions/{sid}`（文件快照）：单会话可达数百 MB，账号间复制收益低、
/// 代价高 ⇒ 明确不搬，但在报告里指出源里到底有没有。
pub(crate) fn copy_session_side_data(
    edition: Edition,
    src_sid: &str,
    dst_sid: &str,
    src_project_dir: Option<&Path>,
) -> Value {
    let dirs = SideDirs {
        project_dir: src_project_dir.map(|p| p.to_path_buf()),
        tasks: tasks_dir(edition),
        artifact_index: artifact_index_dir(edition),
        file_history: file_history_dir(edition),
        workspace: workspace_sessions_dir(edition),
        blobs: blobs_dir(edition),
    };
    copy_side_data_in(&dirs, src_sid, dst_sid).to_json()
}

/// 复制主体（目录全部由调用方给定，便于单测）。
fn copy_side_data_in(dirs: &SideDirs, src_sid: &str, dst_sid: &str) -> SideCopyReport {
    let mut r = SideCopyReport::default();
    // 复制的文本文件（用于稍后抠 SHA256）；正文也算进去。
    let mut texts: Vec<String> = Vec::new();
    let mut copied_text_files: Vec<PathBuf> = Vec::new();

    // ---- 1) projects/{slug}/ 下同 id 的附属（正文由调用方搬，这里跳过）----
    if let Some(dir) = &dirs.project_dir {
        let src_jsonl = dir.join(format!("{src_sid}.jsonl"));
        if let Ok(t) = std::fs::read_to_string(&src_jsonl) {
            texts.push(t);
        }
        if let Ok(rd) = std::fs::read_dir(dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.split('.').next() != Some(src_sid) {
                    continue;
                }
                if name == format!("{src_sid}.jsonl") {
                    continue;
                }
                let dst = dir.join(name.replacen(src_sid, dst_sid, 1));
                copy_into(
                    &entry.path(),
                    &dst,
                    src_sid,
                    dst_sid,
                    &mut r,
                    &mut copied_text_files,
                );
            }
        }
    }

    // ---- 2) tasks / artifact-index / file-history：候选项是 `{sid}` 目录或 `{sid}.json` ----
    for root in [&dirs.tasks, &dirs.artifact_index, &dirs.file_history] {
        for cand in [root.join(src_sid), root.join(format!("{src_sid}.json"))] {
            if !cand.exists() {
                continue;
            }
            let Some(fname) = cand.file_name() else {
                continue;
            };
            let dst = root.join(fname.to_string_lossy().replacen(src_sid, dst_sid, 1));
            copy_into(&cand, &dst, src_sid, dst_sid, &mut r, &mut copied_text_files);
        }
    }

    // ---- 3) 工作区快照：只报告，不搬（见函数头注释）----
    r.snapshot_present = dirs.workspace.join(src_sid).exists()
        || dirs.workspace.join(format!("{src_sid}.json")).exists();

    // ---- 4) 附件完整性检查（不复制 —— 同仓共享，见函数头注释）----
    for p in &copied_text_files {
        if let Ok(t) = std::fs::read_to_string(p) {
            texts.push(t);
        }
    }
    let mut hashes: BTreeSet<String> = BTreeSet::new();
    for t in &texts {
        hashes.extend(hex64_tokens(t));
    }
    r.blobs_referenced = hashes.len();
    if dirs.blobs.is_dir() {
        for h in &hashes {
            if !blob_exists(&dirs.blobs, h) {
                r.blobs_missing += 1;
            }
        }
    }

    r
}

/// 复制一个条目（文件或目录）。
fn copy_into(
    src: &Path,
    dst: &Path,
    src_sid: &str,
    dst_sid: &str,
    r: &mut SideCopyReport,
    copied_text_files: &mut Vec<PathBuf>,
) {
    if src.is_dir() {
        copy_tree(src, dst, src_sid, dst_sid, r, copied_text_files);
        return;
    }
    if !src.is_file() {
        return;
    }
    match copy_one(src, dst, src_sid, dst_sid) {
        Ok((n, textual)) => {
            r.files += 1;
            r.bytes += n;
            if textual {
                copied_text_files.push(dst.to_path_buf());
            }
        }
        Err(e) => r.errors.push(e),
    }
}

/// 递归复制目录，路径名里的旧会话 id 一并替换。
fn copy_tree(
    src: &Path,
    dst: &Path,
    src_sid: &str,
    dst_sid: &str,
    r: &mut SideCopyReport,
    copied_text_files: &mut Vec<PathBuf>,
) {
    let mut stack = vec![(src.to_path_buf(), dst.to_path_buf())];
    while let Some((s, d)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&s) else {
            continue;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().replacen(src_sid, dst_sid, 1);
            let target = d.join(name);
            if p.is_dir() {
                stack.push((p, target));
            } else if p.is_file() {
                match copy_one(&p, &target, src_sid, dst_sid) {
                    Ok((n, textual)) => {
                        r.files += 1;
                        r.bytes += n;
                        if textual {
                            copied_text_files.push(target);
                        }
                    }
                    Err(e) => r.errors.push(e),
                }
            }
        }
    }
}

/// 复制单个文件。返回 `(字节数, 是否文本)`。
///
/// 文本类（≤ 4 MiB 的 json/jsonl/ndjson/txt/md/log）会把里面的**旧会话 id 替换成新 id** ——
/// 否则新会话的元数据仍指向旧 id。二进制原样复制（改字节会损坏文件）。
fn copy_one(src: &Path, dst: &Path, src_sid: &str, dst_sid: &str) -> Result<(u64, bool), String> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建目录失败：{e}"))?;
    }
    const MAX_REWRITE: u64 = 4 * 1024 * 1024;
    let textual = src
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            matches!(
                e.to_ascii_lowercase().as_str(),
                "json" | "jsonl" | "ndjson" | "txt" | "md" | "log"
            )
        })
        .unwrap_or(false);
    if textual {
        if let Ok(meta) = src.metadata() {
            if meta.len() <= MAX_REWRITE {
                if let Ok(text) = std::fs::read_to_string(src) {
                    let out = text.replace(src_sid, dst_sid);
                    std::fs::write(dst, out.as_bytes())
                        .map_err(|e| format!("写 {} 失败：{e}", dst.display()))?;
                    return Ok((out.len() as u64, true));
                }
            }
        }
    }
    let n = std::fs::copy(src, dst).map_err(|e| format!("复制 {} 失败：{e}", src.display()))?;
    Ok((n, textual))
}

/// 附件仓库里有没有这个 hash。
///
/// 文件名可能是 `{hash}`，也可能后面带后缀（客户端某些版本会加），
/// 所以先精确匹配、再按前缀找。
fn blob_exists(blobs: &Path, hash: &str) -> bool {
    if hash.len() < 2 {
        return false;
    }
    let bucket = blobs.join(&hash[0..2]);
    if bucket.join(hash).is_file() {
        return true;
    }
    let Ok(rd) = std::fs::read_dir(&bucket) else {
        return false;
    };
    rd.flatten()
        .any(|e| e.file_name().to_string_lossy().starts_with(hash) && e.path().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 会话附属数据复制：新 id、内容替换、三类目录、附件只查不搬。
    #[test]
    fn copies_session_side_data_under_new_id() {
        const SRC: &str = "11111111-1111-1111-1111-111111111111";
        const DST: &str = "22222222-2222-2222-2222-222222222222";
        // 一个「仓库里有」的 sha256，一个「没有」的
        const HAVE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        const GONE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

        let base = std::env::temp_dir().join(format!("wb-side-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);

        let proj = base.join("projects").join("f-Code-x");
        std::fs::create_dir_all(proj.join(SRC)).unwrap(); // 工具输出外溢目录
        std::fs::write(
            proj.join(format!("{SRC}.jsonl")),
            format!(r#"{{"sessionId":"{SRC}","a":"{HAVE}","b":"{GONE}"}}"#),
        )
        .unwrap();
        std::fs::write(
            proj.join(format!("{SRC}.meta.json")),
            format!(r#"{{"id":"{SRC}"}}"#),
        )
        .unwrap();
        std::fs::write(proj.join(SRC).join("tool.txt"), format!("session {SRC} output")).unwrap();

        let tasks = base.join("tasks");
        std::fs::create_dir_all(tasks.join(SRC)).unwrap();
        std::fs::write(tasks.join(SRC).join("t.json"), "{}").unwrap();

        let art = base.join("artifact-index");
        std::fs::create_dir_all(&art).unwrap();
        std::fs::write(art.join(format!("{SRC}.json")), "[]").unwrap();

        let blobs = base.join("blobs");
        std::fs::create_dir_all(blobs.join(&HAVE[0..2])).unwrap();
        std::fs::write(blobs.join(&HAVE[0..2]).join(HAVE), b"png-bytes").unwrap();

        let dirs = SideDirs {
            project_dir: Some(proj.clone()),
            tasks: tasks.clone(),
            artifact_index: art.clone(),
            file_history: base.join("file-history"),
            workspace: base.join("workspace").join("sessions"),
            blobs: blobs.clone(),
        };
        let r = copy_side_data_in(&dirs, SRC, DST);

        // 四个附属全到位：meta + {sid}/tool.txt + tasks/{sid}/t.json + artifact-index/{sid}.json
        assert_eq!(r.files, 4, "复制文件数 {}，错误 {:?}", r.files, r.errors);
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert!(proj.join(format!("{DST}.meta.json")).is_file());
        assert!(proj.join(DST).join("tool.txt").is_file());
        assert!(tasks.join(DST).join("t.json").is_file());
        assert!(art.join(format!("{DST}.json")).is_file());

        // 正文归调用方搬，源文件一个都不动
        assert!(!proj.join(format!("{DST}.jsonl")).exists());
        assert!(proj.join(format!("{SRC}.meta.json")).is_file());

        // 文本里的旧会话 id 已被替换
        let meta = std::fs::read_to_string(proj.join(format!("{DST}.meta.json"))).unwrap();
        assert!(meta.contains(DST) && !meta.contains(SRC), "{meta}");
        let tool = std::fs::read_to_string(proj.join(DST).join("tool.txt")).unwrap();
        assert!(tool.contains(DST) && !tool.contains(SRC), "{tool}");

        // 附件：只统计不复制 —— 仓库里仍然只有那 1 个文件
        assert_eq!(r.blobs_referenced, 2, "正文引用了 2 个 hash");
        assert_eq!(r.blobs_missing, 1, "其中一个在仓库里不存在");
        assert_eq!(
            std::fs::read_dir(blobs.join(&HAVE[0..2])).unwrap().count(),
            1,
            "附件不该被复制（内容寻址的全局仓库，不分账号）"
        );
        assert!(!r.snapshot_present);

        let _ = std::fs::remove_dir_all(&base);
    }


    #[test]
    fn slug_matches_client_convention() {
        assert_eq!(
            slug_for_cwd(r"C:\Users\alice\WorkBuddy\2026-09-10-14-49-02"),
            "c-Users-alice-WorkBuddy-2026-09-10-14-49-02"
        );
        assert_eq!(slug_for_cwd("/home/alice/proj"), "home-alice-proj");
    }

    #[test]
    fn short_title_uses_last_two_segments() {
        assert_eq!(short_title(r"C:\Users\alice\Code\demo"), "Code/demo");
        assert_eq!(short_title("single"), "single");
    }

    #[test]
    fn scan_reports_workspaces_for_current_edition() {
        // 本机国内版有 projects/ 目录，应该能列出工作区；国际版可能为空。
        let v = scan_exportable(Edition::Domestic);
        assert_eq!(v["edition"], "domestic");
        assert!(v["workspaces"].is_array());
        assert!(v["config"].is_array());
        assert!(v["extras"]["blobs"]["files"].is_u64());
    }
}
