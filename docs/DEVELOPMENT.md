# 开发指南

## 环境要求

Node.js ≥ 20、Rust stable、macOS（或 Windows/Linux）。

## 开发命令

```bash
npm install
npm run tauri dev        # 开发模式
npm run build:app        # 构建 debug .app（含前端资源补丁）
npm run build:app:release  # 构建 release .app + 签名更新包
```

## 发布新版本

签名密钥（自动更新用）存放于 `~/.wb-switch/wb-switch-updater.key`，构建脚本通过
`TAURI_SIGNING_PRIVATE_KEY` 注入。

### 1. 改版本号（一条命令）

```bash
node scripts/version.mjs                 # 查看当前版本 + 18 个位置是否一致
node scripts/version.mjs set 0.6.1       # 全量改成 0.6.1，并自动刷新 Cargo.lock
node scripts/version.mjs check           # 不一致就 exit 1（CI 已接入，编译前先跑）
sh scripts/bump-version.sh 0.6.1         # 旧入口，等价于 set
```

版本号**唯一真源**是根 `Cargo.toml` 的 `[workspace.package] version`，
三个成员 crate 用 `version.workspace = true` 继承。其余必须同步的位置：

| 位置 | 说明 |
| --- | --- |
| `Cargo.toml` | **真源** |
| `package.json` / `package-lock.json` | 前端；lock 里有 2 处（顶层 + `packages[""]`） |
| `src-tauri/tauri.conf.json` | Tauri 打包用的版本 |
| `npm/package.json` | 主包 version **+ optionalDependencies 的 4 个平台包版本** |
| `npm/platform/*/package.json` | 5 个平台包 |
| `Cargo.lock` | 三个成员 crate 的 version（由脚本自动刷新） |

> ⚠️ 手改必漏。0.5.1 → 0.6.0 那次就漏了 `package-lock.json` 与 `Cargo.lock`。

**验证二进制里的版本**（UI 版本取自 `update::APP_VERSION = env!("CARGO_PKG_VERSION")`，
即 Cargo.toml，不是 `tauri.conf.json`）：

```bash
node scripts/version.mjs binary target/release/wb-switch-rust.exe
```
> PE 版本资源是 **UTF-16LE** 存的，直接 `grep 0.6.0` 搜不到；
> 反过来 `grep 0.5.1` 会命中 `crossbeam-channel-0.5.16` 之类的依赖路径，容易误判。

### 2. 打 tag 触发 CI

```bash
git add -A && git commit -m "版本 0.6.0 -> 0.6.1"
git push origin main
git tag -a v0.6.1 -m "v0.6.1" && git push origin v0.6.1
```

`.github/workflows/build.yml` 只认 `v*` tag。4 平台构建（约 8 分钟）→ `release` job
自动发 Release（安装包 + 签名更新包 + `latest*.json`）。
`latest*.json` 由 `scripts/merge-update-manifests.py` 合并，没配签名密钥时会跳过。

> **CI 挂了但本地能编过？** CI 用 `actions-rust-lang/setup-rust-toolchain`，默认带
> `RUSTFLAGS=-D warnings`。只在 `#[cfg(target_os = "windows")]` 分支里被调用的函数，
> 在 Linux/macOS 上会因 dead code 直接编译失败 → 三个平台全红 → `release` 被跳过。
> 判据：**定义在 cfg 门外的函数/常量，是否只在门内被调用？**
>
> **重打 tag**：Release 没发出来时 tag 是空转的，`git push --delete origin v0.6.1`
> 后重建即可，无副作用。

### 3. npm 版（webui）发布

包名 **`workbuddy-switch2api`**（`workbuddy-switch` 已被上游作者占用，用原名会 403）。
命令行名仍是 `workbuddy-switch`。

1. 平台二进制由 CI 的 `Publish platform package` 步骤打进
   `workbuddy-switch2api-<platform>-<arch>` 并发布（需配 `secrets.NPM_TOKEN`）
2. 主包由 `publish-main` 发布：`cd npm && npm publish --access public`
3. 没配 `NPM_TOKEN` 时这两步会优雅跳过（打印"未配置 → 跳过"），不影响 Release
4. `postinstall` 从**平台 npmmirror 包**复制二进制，不依赖 GitHub；
   覆盖本地二进制用 `WB_SWITCH_BINARY=<路径>`

平台包名一律从 `npm/package.json` 的 `name` 推导（`install.js` 与 CI 都不写死前缀），
以后改名不会再漏。

## 目录结构

```
src-tauri/
  src/
    commands.rs      # Tauri command 薄包装（对应 Python 版 HTTP API）
    modules/         # 已抽离到 crates/wb-switch-core（三宿主复用）
crates/
  wb-switch-core/    # 核心逻辑：account/auth_file/oauth/process/switch/session/checkin/refresh/update/config
  wb-switch-server/  # HTTP server + CLI：axum API + rust-embed 前端
src/                 # 前端：components/pages/lib（api.ts 双通道：Tauri invoke / HTTP fetch）
npm/                 # npm 包：package.json + bin + scripts/install.js
```

## 隐私注意事项

- 仓库不提交本地数据（accounts.json、认证文件、密钥、token 由 `.gitignore` 排除）
- 发布前用 `git grep` 扫描 token 模式（`ghp_`/`npm_`/`gho_` 等）
