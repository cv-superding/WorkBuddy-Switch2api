#!/usr/bin/env node
/**
 * 版本号管理：一处修改，全仓库同步 + 一致性校验。
 *
 * 为什么需要它：版本号散在 6 类文件里（Rust workspace / 前端 / Tauri / 3 套 npm 包 /
 * 两个 lock 文件）。手改或半自动改一定会漏 —— 0.5.1 → 0.6.0 那次就漏了
 * package-lock.json 和 Cargo.lock，导致 `npm ci` / `cargo --locked` 报错。
 *
 * 用法：
 *   node scripts/version.mjs                  # 显示当前版本 + 各处一致性
 *   node scripts/version.mjs check            # 不一致则 exit 1（CI 用）
 *   node scripts/version.mjs set 0.6.1        # 全量改成 0.6.1，并刷新 Cargo.lock
 *   node scripts/version.mjs binary <exe路径> # 读已编译二进制里内嵌的版本
 *
 * CI 里还会额外校验 tag：GITHUB_REF_NAME=v0.6.0 必须与仓库内版本一致，
 * 否则说明 tag 打在了错误的提交上。
 */
import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const rel = (p) => join(ROOT, p);

/** 版本号的唯一真源（Rust workspace）。 */
const WORKSPACE_CARGO = "Cargo.toml";
/** 版本号由 workspace 继承的成员（这些文件里不该再出现具体版本号）。 */
const MEMBER_CARGOS = [
  "src-tauri/Cargo.toml",
  "crates/wb-switch-core/Cargo.toml",
  "crates/wb-switch-server/Cargo.toml",
];
/** 独立维护版本号的 JSON 文件。 */
const JSON_FILES = [
  "package.json",
  "package-lock.json", // 顶层 version + packages[""].version 两处
  "src-tauri/tauri.conf.json",
  "npm/package.json",
];
const PLATFORM_GLOB = "npm/platform";

const read = (p) => readFileSync(rel(p), "utf8");

/**
 * 按原文件的换行风格写回。
 *
 * 本机 `core.autocrlf=true` ⇒ 工作区是 CRLF、仓库里是 LF。若这里一律写 `\n`，
 * 工作区文件会变成混行（同一文件里 CRLF 与 LF 并存），diff 也会变得难以审阅。
 */
function writeText(p, text) {
  const abs = rel(p);
  const crlf = readFileSync(abs).includes(Buffer.from("\r\n"));
  writeFileSync(abs, crlf ? text.replace(/\r?\n/g, "\r\n") : text, "utf8");
}

const readJson = (p) => JSON.parse(read(p));
const writeJson = (p, obj) => writeText(p, `${JSON.stringify(obj, null, 2)}\n`);

function fail(msg) {
  console.error(`✗ ${msg}`);
  process.exit(1);
}

// ---------------------------------------------------------------- 读取

function workspaceVersion() {
  const m = read(WORKSPACE_CARGO).match(
    /\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m,
  );
  if (!m) fail(`${WORKSPACE_CARGO} 里找不到 [workspace.package] version`);
  return m[1];
}

function platformPkgs() {
  const base = rel(PLATFORM_GLOB);
  if (!existsSync(base)) return [];
  return readdirSync(base)
    .filter((d) => existsSync(join(base, d, "package.json")))
    .map((d) => `${PLATFORM_GLOB}/${d}/package.json`)
    .sort();
}

/** 收集「所有应该等于真源版本号」的位置 -> 实际值。 */
function collect() {
  const out = { [WORKSPACE_CARGO]: workspaceVersion() };

  for (const f of JSON_FILES) {
    const j = readJson(f);
    if (f === "package-lock.json") {
      out[`${f} (top-level)`] = j.version;
      out[`${f} (packages[""])`] = j.packages?.[""]?.version;
    } else {
      out[f] = j.version;
    }
  }

  // 主包的 optionalDependencies 必须指向同版本的平台包
  const mainNpm = readJson("npm/package.json");
  const od = mainNpm.optionalDependencies ?? {};
  for (const [name, ver] of Object.entries(od)) {
    out[`npm/package.json → ${name}`] = ver;
    if (!name.startsWith(mainNpm.name)) {
      out[`⚠️ ${name} 前缀不等于主包名 ${mainNpm.name}`] = "不匹配";
    }
  }

  for (const f of platformPkgs()) {
    const j = readJson(f);
    out[f] = j.version;
    if (!j.name.startsWith(mainNpm.name)) {
      out[`⚠️ ${f} name=${j.name} 前缀不对`] = "不匹配";
    }
  }

 // Cargo.lock 里三个成员 crate 的版本
  if (existsSync(rel("Cargo.lock"))) {
    const lock = read("Cargo.lock");
    for (const name of ["wb-switch-rust", "wb-switch-core", "wb-switch-server"]) {
      // ⚠️ 本机工作区的 Cargo.lock 是 CRLF，必须 \r?\n，否则永远匹配不到（会误报"缺失"）
      const m = lock.match(
        new RegExp(`name = "${name}"\\r?\\nversion = "([^"]+)"`),
      );
      out[`Cargo.lock → ${name}`] = m ? m[1] : "缺失";
    }
  }

  // workspace 成员的 Cargo.toml 不该再写死版本
  for (const f of MEMBER_CARGOS) {
    const t = read(f);
    if (/^version\s*=\s*"/m.test(t)) {
      out[`⚠️ ${f} 仍写死了 version`] = "应改为 version.workspace = true";
    }
  }

  return out;
}

// ---------------------------------------------------------------- 命令

function cmdShow() {
  const expect = workspaceVersion();
  const all = collect();
  const entries = Object.entries(all);
  const bad = entries.filter(([, v]) => v !== expect);
  const width = Math.max(...entries.map(([k]) => k.length));

  console.log(`版本真源 [workspace.package] = ${expect}\n`);
  for (const [k, v] of entries) {
    const mark = v === expect ? "✓" : "✗";
    console.log(`  ${mark} ${k.padEnd(width)}  ${v}`);
  }
  console.log(
    `\n${bad.length === 0 ? "全部一致" : `${bad.length} 处不一致`}` +
      `（共 ${entries.length} 个位置）`,
  );
  return bad.length === 0;
}

function cmdCheck() {
  const ok = cmdShow();
  // CI：tag 必须与仓库内版本一致，否则说明 tag 打在了错的提交上
  const ref = process.env.GITHUB_REF_NAME ?? "";
  if (ref.startsWith("v")) {
    const tagVer = ref.slice(1);
    const expect = workspaceVersion();
    if (tagVer !== expect) {
      console.error(`\n✗ tag ${ref} 与仓库版本 ${expect} 不一致`);
      console.error("  tag 打在了错误的提交上，或忘了跑 scripts/bump-version.sh");
      return false;
    }
    console.log(`✓ tag ${ref} 与仓库版本一致`);
  }
  return ok;
}

function cmdSet(next) {
  if (!/^\d+\.\d+\.\d+(-[\w.]+)?$/.test(next)) {
    fail(`版本格式不对: ${next}（期望 1.2.3 或 1.2.3-beta.1）`);
  }
  const prev = workspaceVersion();
  const changes = [];

  // 1) Rust workspace 真源
  let cargo = read(WORKSPACE_CARGO);
  cargo = cargo.replace(
    /(\[workspace\.package\][\s\S]*?^version\s*=\s*")[^"]+(")/m,
    `$1${next}$2`,
  );
  writeText(WORKSPACE_CARGO, cargo);
  changes.push(`${WORKSPACE_CARGO} (workspace 真源)`);

  // 2) JSON 文件
  for (const f of JSON_FILES) {
    const j = readJson(f);
    if (f === "package-lock.json") {
      j.version = next;
      if (j.packages?.[""]) j.packages[""].version = next;
      writeJson(f, j);
      changes.push(`${f} (2 处)`);
      continue;
    }
    if (f === "npm/package.json") {
      j.version = next;
      if (j.optionalDependencies) {
        for (const k of Object.keys(j.optionalDependencies)) {
          j.optionalDependencies[k] = next;
        }
      }
      writeJson(f, j);
      changes.push(`${f} (version + optionalDependencies)`);
      continue;
    }
    j.version = next;
    writeJson(f, j);
    changes.push(f);
  }

  // 3) 平台包
  for (const f of platformPkgs()) {
    const j = readJson(f);
    j.version = next;
    writeJson(f, j);
    changes.push(f);
  }

  console.log(`版本 ${prev} → ${next}，已更新 ${changes.length} 个文件：`);
  for (const c of changes) console.log(`  · ${c}`);

  // 4) 刷新 Cargo.lock（成员版本变了，lock 里的记录也要跟着变）
  if (refreshCargoLock()) {
    console.log("  · Cargo.lock 已刷新");
  } else {
    console.warn("⚠️ Cargo.lock 未刷新（没找到 cargo），构建前请手动跑一次 cargo metadata");
  }

  console.log("");
  if (!cmdShow()) {
    fail("仍有位置不一致，请检查上面的 ✗ 行");
  }
  console.log(`\n下一步：git add -A && git commit -m "版本 ${prev} -> ${next}"\n  然后 git tag -a v${next} && git push origin main v${next}`);
}

function refreshCargoLock() {
  const cargo = process.env.CARGO ?? "cargo";
  try {
    execFileSync(cargo, ["metadata", "--format-version", "1"], {
      cwd: ROOT,
      stdio: "ignore",
      env: { ...process.env, CARGO_HOME: process.env.CARGO_HOME ?? "" },
    });
    return true;
  } catch {
    return false;
  }
}

/**
 * 读已编译二进制里内嵌的版本号。
 *
 * ⚠️ 必须按 **UTF-16LE** 找：Windows PE 的版本资源是宽字符存的，
 * 直接搜 `Buffer.from("0.6.0")` 一个都搜不到（会和 crossbeam-channel-0.5.16
 * 之类的依赖路径混淆，得出"版本没更新"的错误结论）。
 */
function cmdBinary(path) {
  if (!path) fail("用法: node scripts/version.mjs binary <exe路径>");
  const buf = readFileSync(resolve(path));
  const expect = workspaceVersion();
  const ansi = Buffer.from(expect, "utf8");
  const wide = Buffer.from(expect, "utf16le");
  const nAnsi = buf.indexOf(ansi) >= 0 ? 1 : 0;
  const nWide = buf.includes(wide) ? 1 : 0;
  console.log(`二进制: ${path}  (${(buf.length / 1048576).toFixed(1)} MB)`);
  console.log(`  期望版本 ${expect}`);
  console.log(`  UTF-8    出现: ${nAnsi ? "是" : "否"}`);
  console.log(`  UTF-16LE 出现: ${nWide ? "是" : "否"}  ← PE 版本资源用这个`);
  if (!nWide) fail("二进制里没有找到期望版本（UTF-16LE），可能没重新编译/部署");
  console.log("✓ 二进制版本正确");
}

// ---------------------------------------------------------------- 入口

const [cmd = "show", arg] = process.argv.slice(2);
switch (cmd) {
  case "show":
    process.exit(cmdShow() ? 0 : 1);
  case "check":
    process.exit(cmdCheck() ? 0 : 1);
  case "set":
    if (!arg) fail("用法: node scripts/version.mjs set <新版本>");
    cmdSet(arg);
    break;
  case "binary":
    cmdBinary(arg);
    break;
  default:
    fail(`未知命令 ${cmd}（可用：show / check / set / binary）`);
}
