# 给 #35 的评论草稿（直接复制粘贴）

> 目标：https://github.com/changexbc/workbuddy-switch/issues/35
> 为什么发这里而不是新开 issue：#35 就是作者/社区在征求"切号带会话选哪种语义"，
> 而且今天刚开、还没有人回复。**别再新开一条重复的 issue。**

---

先补一组实测数据，可能对判断有帮助。一台机器、6 个账号、反复切号之后：

| 指标 | 实测值 | 正常应 |
|---|---|---|
| `sessions` 表行数 | **4174** | 76 |
| 去重后真实对话数 | **349**（跨账号合计） | — |
| `projects/` 正文总体积 | **14.94 GB** | 约 0.6 GB |
| `.wb-switch/backups/sessions/` 目录数 | **130 个** | — |

也就是至少切过 130 次；单个对话在同一账号内最多被复制 **27 份**，跨账号最多 **59 份**。
按 `(账号, 工作目录, 标题)` 去重后剩 349 条，再按 `(工作目录, 标题)` 跨账号去重只剩 **76 条**，
正文 14.94 GB → **0.60 GB**。雪球效应确实和上面写的一致。

---

## 是否考虑第四种语义：共享（`user_id` 置空）

WorkBuddy 侧栏的过滤条件是

```js
or(eq(sessions.userId, filter.userId), isNull(sessions.userId), eq(sessions.userId, ""))
```

即 **`user_id` 为空 / NULL 的会话对所有账号可见**。所以除了表中三种方案，还有：

|  | 目标账号可见历史 | 源账号不受影响 | 无副本 | 归属可追溯 |
|---|---|---|---|---|
| 共享（`user_id = ''`） | ✅ | ✅ | ✅ | ❌ |

实现就是一条 SQL，

```sql
UPDATE sessions SET user_id = ''
WHERE deleted_at IS NULL AND user_id <> ''
  AND (LOWER(cwd) NOT LIKE '%\claw' AND LOWER(cwd) NOT LIKE '%/claw' AND LOWER(cwd) <> 'claw');
```

**不新增任何行**，所以可以每次切号都跑，不会滚雪球；磁盘上始终只有一份正文。

### 代价与边界（我认为需要你判断的地方）

1. **归属被抹平** —— 和「归属移动」同一个问题。但这是"共享"语义本身，不是副作用：
   这条会话不再属于任何人。
2. **云端**：`migrateOnStartup` 会把本地会话按 `msg_channel = convmsg:{当前uid}` 推给当前账号，
   所以每个登录过的账号云端都会各有一份 —— 就是 #15 里提到的「云端归属数据窜号」。
   **但在单机多账号、不依赖跨设备同步的场景下这个代价是零**：我这边 `edge-sync.log` 里
   `§3.2 LIST failed: Request failed (HTTP 404 Not Found)`，云端内容根本拉不下来，
   本地是权威。如果跨设备同步是主要使用路径，那这个顾虑成立，确实该走「项目占位」；
   如果它只是少数人的用法，也许值得分开对待（比如做成默认关闭的开关）。
3. **App 会把它改回去**：`SessionRuntimeEventService.resolveUserId()` 里，`user_id` 为空时
   会回落到 `getCurrentUserId()` —— 也就是**在某个账号下继续聊，这条就被该账号"领养"了**。
   对应办法二选一：每次切号重跑一次；或者在 `workbuddy.db` 上加个触发器钉住
   （`AFTER UPDATE ON sessions ... WHEN NEW.user_id <> ''` → 写回 `''`，实测能拦住 App 的 upsert）。

---

## 我可以提供的

`switch.rs` 里 `share_sessions` 目前还是占位（直接返回 `{"error": "share_sessions 兼容路径暂未在 Rust 版实现"}`），
但前后端管线都是通的（`api.ts` 的 `shareSessions?` → `commands.rs` → `api.rs`）。
我这边已经把它实现了：`session.rs` 新增 `share_sessions_for_switch()`（可单测、跳过 Claw）、
`switch.rs` 接上真实实现、前端弹窗加了个默认开启的开关，+144/−3 行、带 2 个单测。
**需要的话我整理成 PR。**

不过这是产品取舍，和「项目占位」怎么权衡以你的判断为准 —— 如果你已经有明确方向，我就按那个方向改。
