# 「共享全部会话」改动说明

针对 `switch_account()` 里那个**一直没实现的 `share_sessions` 占位**（原注释：「旧的『全体转移』兼容路径（默认关闭），Rust 版暂未实现」）。

改动见 `share-sessions.patch`（+144 / −3 行，4 个文件）。

---

## 为什么需要它

原工具的「**复制会话到目标账号**」是**复制**：

```rust
let new_cid = uuid::Uuid::new_v4().to_string();   // 每次都是全新 id
```

每切一次账号、每个勾选的会话就多一行新记录。来回切 N 次 → 会话被复制 N 份。
（实测某台机器 130 次切换，349 个真实对话膨胀成 4174 条、15 GB 正文。）

本改动走**路径 C：置空 `user_id`**：

```sql
UPDATE sessions SET user_id = '' WHERE deleted_at IS NULL AND user_id <> '' AND <不是 Claw>
```

WorkBuddy 的侧栏过滤条件是 `user_id = 当前账号 OR user_id 为空`，
所以置空之后**任何账号登录都能看到同一份会话，磁盘上仍然只有一份正文，不新增任何行**。
因此可以放心地在**每次**切换时执行。

---

## 具体改了哪 4 个地方

| 文件 | 改动 |
|---|---|
| `crates/wb-switch-core/src/modules/session.rs` | 新增 `share_sessions_for_switch()` / `share_sessions_in(db)`；跳过 Claw 工作区与已删除会话；返回 `{total, shared, skippedClaw}`。另加 2 个单测 |
| `crates/wb-switch-core/src/modules/switch.rs` | 把 `share_sessions` 的占位错误替换为真实实现；进度文案补「正在把会话设为多账号共享…」 |
| `src/components/switch-account-dialog.tsx` | 新增「共享全部会话（推荐）」开关，**默认开**；切换结果 toast 里报告共享了多少条 |
| `src/lib/types.ts` | `SwitchResult` 补 `sessionShare` 字段类型 |

前后端管线**原本就是通的**（`api.ts` 的 `shareSessions?`、Tauri 命令的 `share_sessions`、
HTTP API 的 `shareSessions` 都在），只缺核心实现和 UI 开关 —— 所以这次改动很小。

### 行为变化（注意）

- 弹窗现在多了一个**默认开启**的「共享全部会话」开关。不想要就关掉，行为回到原样。
- 后端 `share_sessions.unwrap_or(false)` **没改**（直接调 API 的老调用方行为不变），
  是前端默认传 `true`。

---

## 怎么用上它

改的是**源码**。`F:\AdobeAll\workbuddy-switch\wb-switch-rust.exe` 是 19.2 MB 的
Rust 单体程序，**前端也打包在里面**（`index.html`、`assets/` 都在 exe 内部），
磁盘上没有可改的 JS/HTML，所以**必须重新编译**才能生效：

```bash
npm install          # 装前端依赖
npm run build        # 产出 dist/
cd src-tauri && cargo build --release   # 或走仓库自带的 CI
```

需要：Rust 工具链 + MSVC 生成工具（Windows 上 Tauri 要 `link.exe`）+ Node。
Rust 已装到 `F:\Code\ENV\RUST`，这份改动已在本机编译验证过。

> ℹ️ 仓库归属已改为 `github.com/cv-superding/WorkBuddy-Switch2api`。
> 需要自动更新时把 `src-tauri/tauri.conf.json` 里的 `plugins.updater.endpoints`
> 指向自己的 releases（当前为空 = 不检查更新）。

---

## 其实……现在不用它也行

本机当前是用**数据库触发器**实现同样效果的（见 `../dedup/诊断报告.md` 第八节）：

```sql
CREATE TRIGGER wb_keep_shared_upd AFTER UPDATE ON sessions FOR EACH ROW
WHEN NEW.user_id <> '' AND <不是 Claw 工作区>
BEGIN UPDATE sessions SET user_id = '' WHERE id = NEW.id; END;
```

触发器比「每次切换时共享一次」更强：**聊过之后也不会被当前账号「领养」**。
而且它在 `workbuddy.db` 里，WorkBuddy 升级不影响。

所以这份源码改动更适合：**提给上游作者 / 你自己以后编译时带上**。
