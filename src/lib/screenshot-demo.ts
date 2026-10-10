import type {
  AccountMeta, AppStatus, AutoRotateConfig, CheckinConfig, CheckinLog,
  CodeBuddyCliStatus, CodeBuddyCliSwitchResult, CreditExpiry, CreditOfficialUsageModel, CreditStatistics,
  GithubConfig, ProxyConfig, ProxyModels, ProxyStatus, ProxyUsage, RotateLog, RotateStatus,
  TokenStatistics, TokenStatsGroup, TokenStatsSource, TokenStatsTotals, TravelConfig, TravelStatus,
  UsageBucket,
  CacheMovePlan, CacheVerifyResult,
  UpdateGuardStatus,
} from "./types";
import { demoModeEnabled } from "./demo-mode";

export const screenshotDemoEnabled = demoModeEnabled;

const MODEL_NAMES = ["deepseek-v4-flash", "kimi-k3-1", "deepseek-v4-pro", "glm-5.2", "hy3"] as const;

interface ModelSeed {
  model: (typeof MODEL_NAMES)[number];
  requestCount: number;
  credit: number;
}

interface AccountUsageSeed {
  requestCount: number;
  models: ModelSeed[];
}

const accounts: AccountMeta[] = [
  { id: "demo-account-a", uid: "demo-user-001", email: "test-a@example.com", nickname: "测试 A", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null, group: "proxy" },
  { id: "demo-account-b", uid: "demo-user-002", email: "test-b@example.com", nickname: "测试 B", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null, group: "desktop" },
  // 分组：A=反代API·国内版 / B=桌面端 / C=反代API·国际版 ——
// 反代页要按版本拆组，这里得两种版本都有人才看得出效果
  { id: "demo-account-c", uid: "demo-user-003", email: "test-c@example.com", nickname: "测试 C", enterpriseName: "Demo Workspace", expiresAt: 0, refreshExpiresAt: 0, refreshedAt: 0, createdAt: 0, needsRelogin: false, needsReloginReason: null, edition: "international", group: "proxy" },
];

/** 演示模式中的临时 CLI 当前账号，仅存在于本次页面会话。 */
let demoActiveCliAccountId = accounts[0].id;

// Counts and relative model roles follow anonymous aggregates from the sanitized local cache.
// No upstream request row or identifier is copied into this fixture.
const usageSeeds: AccountUsageSeed[] = [
  {
    requestCount: 2243,
    models: [
      { model: "deepseek-v4-flash", requestCount: 2133, credit: 1794.39 },
      { model: "kimi-k3-1", requestCount: 24, credit: 2497.16 },
      { model: "deepseek-v4-pro", requestCount: 23, credit: 3.63 },
      { model: "glm-5.2", requestCount: 1, credit: 33.63 },
      { model: "hy3", requestCount: 62, credit: 0 },
    ],
  },
  {
    requestCount: 679,
    models: [
      { model: "deepseek-v4-flash", requestCount: 659, credit: 1270.62 },
      { model: "hy3", requestCount: 20, credit: 0 },
    ],
  },
  {
    requestCount: 318,
    models: [
      { model: "deepseek-v4-flash", requestCount: 309, credit: 595.08 },
      { model: "hy3", requestCount: 9, credit: 0 },
    ],
  },
];

const creditPackages = [
  [
    ["CodeBuddy 个人版国内运营裂变包", 5000, 3186.4, 36],
    ["CodeBuddy 个人版积分包", 2400, 1180.75, 18],
    ["CodeBuddy 新用户体验包", 800, 386.4, 5],
    ["CodeBuddy 签到赠送积分", 300, 196.25, 11],
    ["CodeBuddy 活动奖励积分", 600, 428.6, 27],
  ],
  [
    ["CodeBuddy 个人版国内运营裂变包", 3600, 2468.2, 24],
    ["CodeBuddy 个人版积分包", 1800, 905.5, 42],
    ["CodeBuddy 新用户体验包", 500, 128.2, 7],
    ["CodeBuddy 签到赠送积分", 240, 174.35, 15],
    ["CodeBuddy 活动奖励积分", 400, 286.8, 31],
  ],
  [
    ["CodeBuddy 个人版国内运营裂变包", 2400, 1680.4, 29],
    ["CodeBuddy 个人版积分包", 1200, 748.6, 55],
    ["CodeBuddy 新用户体验包", 360, 214.5, 14],
    ["CodeBuddy 签到赠送积分", 180, 96.75, 21],
    ["CodeBuddy 活动奖励积分", 300, 207.9, 38],
  ],
] as const;

function startOfToday(): Date {
  const date = new Date();
  date.setHours(0, 0, 0, 0);
  return date;
}

function localDate(daysAgo: number): string {
  const date = startOfToday();
  date.setDate(date.getDate() - daysAgo);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;
}

function atLocalTime(daysAgo: number, hour: number, minute: number): number {
  const date = startOfToday();
  date.setDate(date.getDate() - daysAgo);
  date.setHours(hour, minute, 0, 0);
  return date.getTime();
}

function futureAt(daysAhead: number, hour = 23, minute = 59): number {
  const date = startOfToday();
  date.setDate(date.getDate() + daysAhead);
  date.setHours(hour, minute, 0, 0);
  return date.getTime();
}

function hydratedAccounts(): AccountMeta[] {
  return accounts.map((account, index) => ({
    ...account,
    expiresAt: futureAt(12 + index * 5, 18, 30),
    refreshExpiresAt: futureAt(40 + index * 7),
    refreshedAt: atLocalTime(0, 9, 12 + index * 7),
    createdAt: atLocalTime(45 + index * 19, 10, 0),
  }));
}

function creditExpiry(accountId: string): CreditExpiry {
  const index = Math.max(0, accounts.findIndex((account) => account.id === accountId));
  const account = accounts[index] ?? accounts[0];
  const resources = creditPackages[index].map(([packageName, total, remaining, expireDays], packageIndex) => ({
    packageCode: `demo-package-${index + 1}-${packageIndex + 1}`,
    packageName,
    total,
    remaining,
    used: Number((total - remaining).toFixed(2)),
    status: 1,
    expireAt: futureAt(expireDays),
    expired: false,
    expiringSoon: expireDays <= 7,
  }));
  const totalCapacity = resources.reduce((sum, resource) => sum + resource.total, 0);
  const totalRemaining = resources.reduce((sum, resource) => sum + resource.remaining, 0);
  const expiringSoonRemaining = resources.filter((resource) => resource.expiringSoon).reduce((sum, resource) => sum + resource.remaining, 0);

  return {
    ok: true,
    accountId: account.id,
    accountName: account.nickname ?? account.email ?? account.id,
    updatedAt: Date.now() - (index + 1) * 4 * 60 * 1000,
    totalCapacity,
    totalRemaining: Number(totalRemaining.toFixed(2)),
    expiringSoonRemaining: Number(expiringSoonRemaining.toFixed(2)),
    expiredRemaining: 0,
    soonestExpireAt: Math.min(...resources.map((resource) => resource.expireAt)),
    expiringSoon: expiringSoonRemaining > 0,
    expired: false,
    resources,
  };
}

function dailyWeight(accountIndex: number, dayIndex: number): number {
  const weekdayWave = [0.72, 1.08, 0.93, 1.22, 0.84, 1.16, 1.01][dayIndex % 7];
  const quiet = (dayIndex + accountIndex * 4) % 13 === 0 ? 0.16 : 1;
  return weekdayWave * quiet * (1 + accountIndex * 0.035);
}

function distributeModels(seed: AccountUsageSeed, accountIndex: number) {
  const weights = Array.from({ length: 30 }, (_, dayIndex) => dailyWeight(accountIndex, dayIndex));
  const weightTotal = weights.reduce((sum, weight) => sum + weight, 0);
  const countSeries = seed.models.map((model) => {
    const raw = weights.map((weight) => (model.requestCount * weight) / weightTotal);
    const values = raw.map(Math.floor);
    let remaining = model.requestCount - values.reduce((sum, value) => sum + value, 0);
    const byFraction = raw.map((value, index) => ({ index, fraction: value - Math.floor(value) })).sort((left, right) => right.fraction - left.fraction);
    for (let index = 0; index < remaining; index += 1) values[byFraction[index].index] += 1;
    return values;
  });
  const creditSeries = seed.models.map((model) => {
    const values = weights.map((weight) => Number(((model.credit * weight) / weightTotal).toFixed(2)));
    const drift = Number((model.credit - values.reduce((sum, value) => sum + value, 0)).toFixed(2));
    values[values.length - 1] = Number((values[values.length - 1] + drift).toFixed(2));
    return values;
  });
  return Array.from({ length: 30 }, (_, dayIndex) => {
    const models = seed.models.map((model, modelIndex) => ({
      model: model.model,
      requestCount: countSeries[modelIndex][dayIndex],
      credit: creditSeries[modelIndex][dayIndex],
    }));
    return {
      date: localDate(29 - dayIndex),
      usage: Number(models.reduce((sum, model) => sum + model.credit, 0).toFixed(2)),
      models,
    };
  });
}

function sumModels(rows: { models: CreditOfficialUsageModel[] }[]): CreditOfficialUsageModel[] {
  const totals = new Map<string, CreditOfficialUsageModel>();
  for (const row of rows) {
    for (const model of row.models) {
      const current = totals.get(model.model) ?? { model: model.model, requestCount: 0, credit: 0 };
      current.requestCount += model.requestCount;
      current.credit = Number((current.credit + model.credit).toFixed(2));
      totals.set(model.model, current);
    }
  }
  return [...totals.values()].sort((left, right) => right.credit - left.credit);
}

function visibleRequests(accountIndex: number) {
  const account = accounts[accountIndex];
  const seed = usageSeeds[accountIndex];
  const hours = [16, 15, 17, 14, 1, 0, 3];
  const flashCredits = [0.13, 0.04, 0.2, 1, 0.08, 3.99, 0.45, 8.5, 24.56];
  const kimiCredits = [86.4, 103.2, 112.8, 128.4, 74.6];
  const proCredits = [0.04, 0.13, 0.2, 0.45];
  const weightedModels = seed.models.flatMap((model) =>
    Array.from({ length: Math.max(1, Math.round((model.requestCount / seed.requestCount) * 100)) }, () => model.model),
  );

  return Array.from({ length: 100 }, (_, rowIndex) => {
    const daysAgo = Math.floor(rowIndex / 8);
    const hour = hours[(rowIndex + accountIndex * 2) % hours.length];
    const minute = (rowIndex * 7 + accountIndex * 11) % 60;
    const ts = new Date(atLocalTime(daysAgo, hour, minute));
    const model = rowIndex < seed.models.length
      ? seed.models[rowIndex].model
      : weightedModels[rowIndex % weightedModels.length];
    const credit = model === "hy3"
      ? 0
      : model === "kimi-k3-1"
        ? kimiCredits[(rowIndex + accountIndex) % kimiCredits.length]
        : model === "glm-5.2"
          ? 33.63
          : model === "deepseek-v4-pro"
            ? proCredits[(rowIndex + accountIndex) % proCredits.length]
            : flashCredits[(rowIndex + accountIndex * 3) % flashCredits.length];
    return {
      accountId: account.id,
      accountName: account.nickname ?? account.email ?? account.id,
      requestId: `demo-request-${String(accountIndex + 1).padStart(2, "0")}-${String(rowIndex + 1).padStart(4, "0")}`,
      credit,
      model,
      client: rowIndex % 50 === 0 ? "CodeBuddyIDE" : "CLI",
      requestTime: `${localDate(daysAgo)} ${String(ts.getHours()).padStart(2, "0")}:${String(ts.getMinutes()).padStart(2, "0")}:00`,
    };
  });
}

function buildStatistics(): CreditStatistics {
  const demoAccounts = hydratedAccounts();
  const accountDaily = usageSeeds.map((seed, index) => distributeModels(seed, index));
  const daily = accountDaily[0].map((_, dayIndex) => {
    const models = new Map<string, CreditOfficialUsageModel>();
    for (const rows of accountDaily) {
      for (const model of rows[dayIndex].models) {
        const current = models.get(model.model) ?? { model: model.model, requestCount: 0, credit: 0 };
        current.requestCount += model.requestCount;
        current.credit = Number((current.credit + model.credit).toFixed(2));
        models.set(model.model, current);
      }
    }
    const modelRows = [...models.values()];
    return { date: accountDaily[0][dayIndex].date, usage: Number(modelRows.reduce((sum, model) => sum + model.credit, 0).toFixed(2)), models: modelRows };
  });
  const sumRecent = (rows: { usage: number }[], count: number) => Number(rows.slice(-count).reduce((sum, row) => sum + row.usage, 0).toFixed(2));
  const monthPrefix = localDate(0).slice(0, 7);
  const sumMonth = (rows: { date: string; usage: number }[]) => Number(rows.filter((row) => row.date.startsWith(monthPrefix)).reduce((sum, row) => sum + row.usage, 0).toFixed(2));
  const generatedAt = Date.now() - 3 * 60 * 1000;
  const creditRows = accounts.map((account) => creditExpiry(account.id));
  const officialAccounts = demoAccounts.map((account, index) => ({
    accountId: account.id,
    accountName: account.nickname ?? account.email ?? account.id,
    ok: true,
    requestCount: usageSeeds[index].requestCount,
    detailTruncated: true,
    usageToday: accountDaily[index][accountDaily[index].length - 1]?.usage ?? 0,
    usage7Days: sumRecent(accountDaily[index], 7),
    usageThisMonth: sumMonth(accountDaily[index]),
    reportedTotal: usageSeeds[index].requestCount,
    fetchedCount: usageSeeds[index].requestCount,
    models: sumModels(accountDaily[index]),
    daily: accountDaily[index],
  }));
  const usageToday = daily[daily.length - 1]?.usage ?? 0;
  const usage7Days = sumRecent(daily, 7);
  const usageThisMonth = sumMonth(daily);
  const totalRemaining = creditRows.reduce((sum, credit) => sum + (credit.totalRemaining ?? 0), 0);
  const totalCapacity = creditRows.reduce((sum, credit) => sum + (credit.totalCapacity ?? 0), 0);

  return {
    generatedAt,
    retentionDays: 90,
    coverageStartAt: atLocalTime(29, 0, 0),
    summary: { currentRemaining: Number(totalRemaining.toFixed(2)), currentCapacity: totalCapacity, usageToday, usage7Days, usageThisMonth, todayCheckedInAccounts: 3, todaySuccess: 2, todayAlready: 1, todayFailed: 0 },
    daily,
    accounts: demoAccounts.map((account, index) => ({
      accountId: account.id,
      accountName: account.nickname ?? account.email ?? account.id,
      isCurrent: index === 0,
      currentRemaining: creditRows[index].totalRemaining ?? null,
      totalCapacity: creditRows[index].totalCapacity ?? null,
      lastSnapshotAt: generatedAt - index * 120_000,
      usageToday: officialAccounts[index].usageToday ?? 0,
      usage7Days: officialAccounts[index].usage7Days ?? 0,
      usageThisMonth: officialAccounts[index].usageThisMonth ?? 0,
      checkedInToday: true,
      checkinStatusToday: index === 1 ? "already" : "success",
      lastCheckinAt: atLocalTime(0, 8, 6 + index * 9),
      lastCheckinResult: index === 1 ? "already" : "success",
      daily: accountDaily[index],
    })),
    events: demoAccounts.map((account, index) => ({ kind: "checkin" as const, ts: atLocalTime(0, 8, 6 + index * 9), date: localDate(0), accountId: account.id, accountName: account.nickname ?? account.email ?? account.id, result: index === 1 ? "already" : "success" })),
    officialUsage: {
      status: "complete",
      rangeStart: localDate(29),
      rangeEnd: localDate(0),
      collectedAt: generatedAt,
      summary: { usageToday, usage7Days, usageThisMonth },
      daily,
      accounts: officialAccounts,
      requests: accounts.flatMap((_, index) => visibleRequests(index)),
      models: sumModels(daily),
      detailLimitPerAccount: 100,
      errors: [],
    },
  };
}

function checkinConfig(): CheckinConfig {
  return { enabled: true, keepalive_days: 7, lazy_refresh_hours: 12 };
}

function travelConfig(): TravelConfig {
  return { enabled: true };
}

function travelStatus(accountId: string): TravelStatus {
  const index = Math.max(0, accounts.findIndex((account) => account.id === accountId));
  const account = accounts[index];
  // 国际版没有成长中心，后端会返回 supported=false，演示数据保持一致
  if (account?.edition === "international") {
    return { label: "untraveled", rewardCredit: null, locationName: null, supported: false };
  }
  // 演示三种状态：旅行中 / 已结束 / 无 Buddy
  if (index % 3 === 0) return { label: "traveling", rewardCredit: 7, locationName: "咖啡馆", arriveAt: Math.floor(Date.now() / 1000) + 2 * 3600 + 40 * 60 };
  if (index % 3 === 1) return { label: "finished", rewardCredit: 20, locationName: "健身房" };
  return { label: "no-buddy", rewardCredit: null, locationName: null };
}

function rotateConfig(): AutoRotateConfig {
  return { enabled: true, check_interval_minutes: 15, cooldown_minutes: 120, min_gap_hours: 24, min_urgency_hours: 72, active_guard_minutes: 30, min_remaining_credits: 50 };
}

/** 反代页的演示数据（演示/Pages 构建里也要能完整渲染这一页）。 */
function proxyConfig(): ProxyConfig {
  // 故意不勾「使用全部账号」：分组列表只在按组挑选时才显示。
  // 账号**按版本各取一份** —— 国内版入口只用国内版的号，反之亦然。
  const uidsOf = (ed: string) =>
    accounts
      .filter((a) => a.group === "proxy" && (a.edition ?? "domestic") === ed)
      .map((a) => a.uid ?? "");
  return {
    // 两个入口都开着，且端口 / key / 账号名单各不相同 —— 演示「完全独立」。
    domestic: {
      enabled: true,
      listen: "127.0.0.1:7863",
      api_key: "",
      accounts: uidsOf("domestic"),
    },
    international: {
      enabled: true,
      listen: "127.0.0.1:7864",
      api_key: "sk-intl-demo",
      accounts: uidsOf("international"),
    },
  };
}

function proxyStatus(): ProxyStatus {
  const n = (ed: string) =>
    accounts.filter((a) => a.group === "proxy" && (a.edition ?? "domestic") === ed).length;
  return {
    running: true,
    endpoints: [
      {
        edition: "domestic",
        label: "国内版",
        enabled: true,
        listen: "127.0.0.1:7863",
        hasApiKey: false,
        accountCount: n("domestic"),
        running: true,
      },
      {
        edition: "international",
        label: "国际版",
        enabled: true,
        listen: "127.0.0.1:7864",
        hasApiKey: true,
        accountCount: n("international"),
        running: true,
      },
    ],
  };
}

function proxyUsage(): ProxyUsage {
  const accounts = hydratedAccounts();
  const byAccount: Record<string, UsageBucket> = {};
  const seeds: Array<[number, number, number, number, number]> = [
    [812, 2, 196_402_118, 384_512, 1_042.318],
    [640, 1, 154_988_204, 301_776, 823.104],
    [391, 0, 92_455_710, 180_244, 486.732],
  ];
  accounts.slice(0, seeds.length).forEach((a, i) => {
    const [requests, errors, promptTokens, completionTokens, credit] = seeds[i];
    byAccount[a.uid ?? a.id] = { nickname: a.nickname ?? a.uid ?? a.id, requests, errors, promptTokens, completionTokens, credit };
  });
  const byModel: Record<string, UsageBucket> = {
    "deepseek-v4.1-flash": { requests: 1_026, promptTokens: 248_120_442, completionTokens: 512_884, credit: 1_364.211 },
    "glm-5.3": { requests: 502, promptTokens: 121_884_006, completionTokens: 268_402, credit: 722.480 },
    "kimi-k3-1": { requests: 315, promptTokens: 73_841_584, completionTokens: 85_246, credit: 265.463 },
  };
  const total = Object.values(byAccount).reduce<UsageBucket>(
    (acc, b) => ({
      requests: (acc.requests ?? 0) + (b.requests ?? 0),
      errors: (acc.errors ?? 0) + (b.errors ?? 0),
      promptTokens: (acc.promptTokens ?? 0) + (b.promptTokens ?? 0),
      completionTokens: (acc.completionTokens ?? 0) + (b.completionTokens ?? 0),
      credit: Number(((acc.credit ?? 0) + (b.credit ?? 0)).toFixed(6)),
    }),
    { requests: 0, errors: 0, promptTokens: 0, completionTokens: 0, credit: 0 },
  );
  const now = Date.now();
  const recent = accounts.slice(0, 4).map((a, i) => ({
    ts: now - i * 137_000,
    uid: a.uid ?? a.id,
    nickname: a.nickname ?? a.uid ?? a.id,
    model: ["deepseek-v4.1-flash", "glm-5.3", "kimi-k3-1", "deepseek-v4.1-flash"][i],
    stream: true,
    ok: i !== 3,
    promptTokens: [8_442, 12_905, 3_118, 6_740][i],
    completionTokens: [1_208, 2_461, 640, 0][i],
    credit: [12.406, 24.118, 5.902, 0][i],
  }));
  return { updatedAt: now, total, byAccount, byModel, recent };
}

/**
 * 缓存迁移演示数据。
 *
 * 演示模式没有真实家目录可扫，所以给一份「已经迁过一半」的样子：
 * 国内版已建联接，国际版还是普通目录 —— 界面上能同时看到两种状态。
 */
function cacheMovePlan(): CacheMovePlan {
  const dirs = [
    {
      name: ".workbuddy",
      label: "国内版 WorkBuddy",
      path: "C:\\Users\\demo\\.workbuddy",
      exists: true,
      isLink: true,
      linkTarget: "E:\\WorkBuddyData\\.workbuddy",
      backup: null,
      files: 0,
      bytes: 0,
      movable: true,
      sizeText: "0 B",
      held: false,
      heldBy: [],
    },
    {
      name: ".workbuddy-ai",
      label: "国际版 WorkBuddy",
      path: "C:\\Users\\demo\\.workbuddy-ai",
      exists: true,
      isLink: false,
      linkTarget: null,
      backup: null,
      files: 286087,
      bytes: 6871947673,
      movable: true,
      sizeText: "6.4 GB",
      held: false,
      heldBy: [],
    },
    {
      name: ".workbuddy-key-fallback",
      label: "凭据回退目录",
      path: "C:\\Users\\demo\\.workbuddy-key-fallback",
      exists: true,
      isLink: false,
      linkTarget: null,
      backup: null,
      files: 10,
      bytes: 320,
      movable: true,
      sizeText: "320 B",
      held: false,
      heldBy: [],
    },
  ];
  return {
    supported: true,
    platformNote: null,
    home: "C:\\Users\\demo",
    destDefault: "E:\\WorkBuddyData",
    dirs,
    totalFiles: 286097,
    totalBytes: 6871947993,
    totalText: "6.4 GB",
    blocking: [],
    warnings: [],
    canRun: true,
    blockedReason: null,
    drives: [
      { letter: "E:", free: 324000000000, total: 500000000000, system: false, freeText: "301.7 GB", totalText: "465.7 GB" },
      { letter: "F:", free: 278000000000, total: 1000000000000, system: false, freeText: "258.9 GB", totalText: "931.3 GB" },
      { letter: "C:", free: 29000000000, total: 405000000000, system: true, freeText: "27.0 GB", totalText: "377.2 GB" },
    ],
    hasDest: true,
  };
}

function cacheVerify(): CacheVerifyResult {
  const plan = cacheMovePlan();
  return { dirs: plan.dirs, blocking: [], warnings: [], backups: [] };
}

/**
 * 更新防护演示数据。
 *
 * 刻意摆成「刚被更新包覆盖过」的样子：缓存里躺着一个已下载的包，
 * 而两个安装目录里都同时有两个启动器 exe、身份却都是国际版 ——
 * 这正是 2026-10-08 那次事故的指纹，截图里能一眼看出问题。
 */
function updateGuardStatus(): UpdateGuardStatus {
  return {
    supported: true,
    platformNote: null,
    disabled: true,
    envName: "WORKBUDDY_UPDATE_URL",
    envValue: "http://127.0.0.1:1",
    blackhole: "http://127.0.0.1:1",
    cacheDirs: [
      "C:\\Users\\demo\\AppData\\Local\\@genieworkbuddy-desktop-updater",
      "C:\\Users\\demo\\AppData\\Local\\Temp\\workbuddy-update-x64",
    ],
    cacheFiles: [
      {
        dir: "C:\\Users\\demo\\AppData\\Local\\@genieworkbuddy-desktop-updater",
        name: "installer.exe",
        bytes: 435534672,
        sizeText: "415.4 MB",
        modified: "2026-10-08 16:50",
      },
      {
        dir: "C:\\Users\\demo\\AppData\\Local\\@genieworkbuddy-desktop-updater",
        name: "installer.exe.quarantine-20261001-231621",
        bytes: 551277352,
        sizeText: "525.7 MB",
        modified: "2026-10-01 20:54",
      },
      {
        dir: "C:\\Users\\demo\\AppData\\Local\\Temp\\workbuddy-update-x64",
        name: "WorkBuddy-Setup-5.7.6.40409493.exe",
        bytes: 435530000,
        sizeText: "415.4 MB",
        modified: "2026-10-08 17:13",
      },
    ],
    cacheBytes: 1422342024,
    cacheText: "1.3 GB",
    cacheActive: true,
    cacheFrozen: true,
    installs: [
      {
        dir: "F:\\AdobeAll\\WorkBuddy",
        edition: "domestic",
        label: "国内版",
        dataDirName: ".workbuddy",
        endpoint: "https://www.workbuddy.cn",
        isOversea: false,
        version: "37.10.3-24",
        exes: ["WorkBuddy.exe"],
        startupUpdate: false,
        userFlag: false,
        warning: null,
      },
      {
        dir: "F:\\AdobeAll\\WorkBuddyAI",
        edition: "international",
        label: "国际版",
        dataDirName: ".workbuddy-ai",
        endpoint: "https://www.workbuddy.ai",
        isOversea: true,
        version: "37.10.3-24",
        exes: ["WorkBuddyAI.exe"],
        startupUpdate: true,
        userFlag: null,
        warning: null,
      },
    ],
    installsNeedingPatch: 1,
    running: [],
  };
}

function proxyModels(): ProxyModels {
  const cn = ["auto", "hy4-preview", "deepseek-v4-pro", "deepseek-v4.1-flash", "glm-5.3", "kimi-k3-1", "minimax-m3"];
  const intl = ["auto", "claude-sonnet-4.5", "gpt-5.2-codex", "gemini-3-pro", "deepseek-v4-pro"];
  const union = Array.from(new Set([...cn, ...intl]));
  return {
    editions: [
      { edition: "domestic", label: "国内版", models: cn, source: "上游", sourceUrl: "https://www.codebuddy.cn/v2/enterprises/personal/models", error: null },
      { edition: "international", label: "国际版", models: intl, source: "上游", sourceUrl: "https://www.workbuddy.ai/v2/enterprises/personal/models", error: null },
    ],
    models: union,
    source: "上游",
    sourceUrl: "https://www.codebuddy.cn/v2/enterprises/personal/models",
  };
}

function checkinLogs(): CheckinLog[] {
  return hydratedAccounts().flatMap((account, accountIndex) => [0, 1, 2].map((daysAgo) => ({ ts: atLocalTime(daysAgo, 8, 6 + accountIndex * 9), accountId: account.id, email: account.nickname ?? account.email ?? account.id, result: accountIndex === 1 && daysAgo === 0 ? "already" : "success" })));
}

function rotateLogs(): RotateLog[] {
  return [
    { ts: atLocalTime(0, 9, 30), action: "skipped", reason: "当前账号仍是积分到期最紧迫的可用账号", from: { id: accounts[0].id, name: accounts[0].nickname }, to: null },
    { ts: atLocalTime(1, 16, 20), action: "switched", reason: "目标账号积分将在 5 天内到期", from: { id: accounts[1].id, name: accounts[1].nickname }, to: { id: accounts[0].id, name: accounts[0].nickname } },
  ];
}

function demoTokenTotals(input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsTotals {
  return { total: input + output + cacheWrite, input, output, cacheRead, cacheWrite, uncachedInput: Math.max(0, input - cacheRead), records, cacheHitRate: input > 0 ? cacheRead / input : null };
}

function demoTokenGroup(key: string, input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsGroup {
  return { key, ...demoTokenTotals(input, output, cacheRead, cacheWrite, records) };
}

function demoTokenSession(key: string, title: string, project: string, input: number, output: number, cacheRead: number, cacheWrite: number, records: number): TokenStatsGroup {
  const keyParts = key.split(" · ");
  return { ...demoTokenGroup(key, input, output, cacheRead, cacheWrite, records), title, project, sessionId: keyParts[keyParts.length - 1] };
}

function demoTokenSource(source: TokenStatsSource["source"], scale: number): TokenStatsSource {
  const daily = Array.from({ length: 14 }, (_, index) => {
    const wave = [0.62, 0.86, 1.1, 0.72, 1.3, 0.94, 0.38][index % 7] * scale;
    return demoTokenGroup(localDate(13 - index), Math.round(7_600_000 * wave), Math.round(480_000 * wave), Math.round(6_650_000 * wave), Math.round(95_000 * wave), Math.round(24 * wave));
  });
  const summary = daily.reduce((sum, row) => demoTokenTotals(sum.input + row.input, sum.output + row.output, sum.cacheRead + row.cacheRead, sum.cacheWrite + row.cacheWrite, sum.records + row.records), demoTokenTotals(0, 0, 0, 0, 0));
  const hours = Array.from({ length: 7 * 24 }, (_, index) => {
    const day = Math.floor(index / 24); const hour = index % 24;
    const active = Math.max(0.02, Math.exp(-Math.pow(hour - (day >= 5 ? 22 : 15), 2) / 22));
    return demoTokenGroup(`${day}-${hour}`, Math.round(720_000 * active * scale), Math.round(41_000 * active * scale), Math.round(610_000 * active * scale), 0, Math.max(1, Math.round(6 * active * scale)));
  });
  const projects = [
    demoTokenGroup("wb-switch-rust", 42_800_000 * scale, 2_400_000 * scale, 37_100_000 * scale, 420_000 * scale, Math.round(148 * scale)),
    demoTokenGroup("my-code-teams", 25_600_000 * scale, 1_650_000 * scale, 21_900_000 * scale, 260_000 * scale, Math.round(96 * scale)),
    demoTokenGroup("LetterTotTown", 11_900_000 * scale, 920_000 * scale, 9_700_000 * scale, 110_000 * scale, Math.round(51 * scale)),
  ];
  const models = [
    demoTokenGroup("deepseek-v4-flash", 56_400_000 * scale, 3_200_000 * scale, 49_100_000 * scale, 530_000 * scale, Math.round(210 * scale)),
    demoTokenGroup("kimi-k3-1", 17_300_000 * scale, 1_140_000 * scale, 14_200_000 * scale, 180_000 * scale, Math.round(61 * scale)),
    demoTokenGroup("glm-5.2", 6_600_000 * scale, 630_000 * scale, 5_400_000 * scale, 80_000 * scale, Math.round(24 * scale)),
  ];
  const sessions = [
    demoTokenSession("wb-switch-rust · token-stats-dashboard", "完善 Token 统计仪表盘与本地用量分析", "wb-switch-rust", 18_700_000 * scale, 1_050_000 * scale, 16_100_000 * scale, 160_000 * scale, Math.round(72 * scale)),
    demoTokenSession("my-code-teams · settings-agent-acp", "设计 Agent 与 ACP 管理设置", "my-code-teams", 13_200_000 * scale, 890_000 * scale, 11_300_000 * scale, 120_000 * scale, Math.round(55 * scale)),
    demoTokenSession("LetterTotTown · character-audio", "补全角色成语双音频", "LetterTotTown", 8_600_000 * scale, 640_000 * scale, 7_200_000 * scale, 80_000 * scale, Math.round(38 * scale)),
    demoTokenSession("wb-switch-rust · account-card-redesign", "统一账号卡片视觉和交互", "wb-switch-rust", 6_300_000 * scale, 410_000 * scale, 5_400_000 * scale, 50_000 * scale, Math.round(29 * scale)),
  ];
  const now = Date.now();
  return { source, summary, models, projects, sessions, daily, hours, filesScanned: source === "workbuddy" ? 63 : source === "codebuddy-ide" ? 17 : 41, parseErrors: 0, coverageStartAt: now - 13 * 86_400_000, coverageEndAt: now };
}

function demoTokenStatistics(days?: number): TokenStatistics {
  return { generatedAt: Date.now(), rangeDays: days ?? null, sources: [demoTokenSource("workbuddy", 1), demoTokenSource("codebuddy-cli", 0.58), demoTokenSource("codebuddy-ide", 0.36)] };
}

/** Read-only demo response provider. It never reads or mutates real user data. */
export function screenshotDemoResponse(command: string, args?: Record<string, unknown>): unknown {
  const demoAccounts = hydratedAccounts();
  const demoDomesticCurrent = demoAccounts.find((account) => account.edition !== "international") ?? demoAccounts[0];
  const demoInternationalCurrent = demoAccounts.find((account) => account.edition === "international");
  const appStatus: AppStatus = { running: true, authFile: "/demo/workbuddy/auth.json", current: { uid: demoDomesticCurrent.uid, nickname: demoDomesticCurrent.nickname, email: demoDomesticCurrent.email }, currentInternational: demoInternationalCurrent ? { uid: demoInternationalCurrent.uid, nickname: demoInternationalCurrent.nickname, email: demoInternationalCurrent.email } : null, appPath: "/demo/WorkBuddy.app", version: "0.1.24" };
  const activeIndex = Math.max(0, demoAccounts.findIndex((account) => account.id === demoActiveCliAccountId));
  const activeAccount = demoAccounts[activeIndex] ?? demoAccounts[0];
  const cliStatus: CodeBuddyCliStatus = { configured: true, settingsPresent: true, helperPresent: true, helperSupportsAccountIds: true, activeIndex, activeAccountId: activeAccount.id, activeAccountName: activeAccount.nickname, accountCount: demoAccounts.length, statePath: "/demo/codebuddy-cli-state.json" };
  const config = rotateConfig();
  const rotateStatus: RotateStatus = { config, cliConfigured: true, activeAccountId: demoAccounts[0].id, activeAccountName: demoAccounts[0].nickname, lastCheckAt: atLocalTime(0, 9, 30), lastSwitchAt: atLocalTime(1, 16, 20) };
  const githubConfig: GithubConfig = { owner: "zhangjia", repo: "wb-switch", proxy: "" };
  switch (command) {
    case "get_status": return appStatus;
    case "get_accounts": return { accounts: demoAccounts };
    case "get_codebuddy_cli_status": return cliStatus;
    case "switch_codebuddy_cli_account": {
      const target = demoAccounts.find((account) => account.id === args?.accountId);
      if (!target) throw new Error("账号不存在");
      demoActiveCliAccountId = target.id;
      return { ok: true, configured: true, synced: true, verified: true, activeIndex: demoAccounts.indexOf(target), activeAccountId: target.id, message: "演示切换已完成" } satisfies CodeBuddyCliSwitchResult;
    }
    case "get_checkin_status": return { ok: true, todayCheckedIn: true };
    case "get_credit_expiry": return creditExpiry(String(args?.accountId ?? ""));
    case "get_credit_statistics": return buildStatistics();
    case "get_token_statistics": return demoTokenStatistics(typeof args?.days === "number" ? args.days : undefined);
    case "get_auto_checkin_config": return checkinConfig();
    case "get_checkin_logs": return { logs: checkinLogs() };
    case "get_travel_status": return travelStatus(String(args?.accountId ?? ""));
    case "get_auto_travel_config": return travelConfig();
    case "get_auto_rotate_config": return config;
    case "rotate_status": return rotateStatus;
    case "get_rotate_logs": return { logs: rotateLogs() };
    case "get_github_config": return githubConfig;
    case "check_update": return { ok: true, current: "0.1.24", latest: "0.1.25", latestTag: "v0.1.25", hasUpdate: true, releaseName: "更新提示演示", releaseUrl: "https://github.com/cv-superding/WorkBuddy-Switch2api/releases/tag/v0.1.25" };
    case "get_launch_at_login_enabled": return true;
    case "switch_progress": return { running: false, progress: null };
    case "get_proxy_config": return proxyConfig();
    case "get_proxy_status": return proxyStatus();
    case "get_proxy_usage": return proxyUsage();
    case "get_proxy_models": return proxyModels();
    case "cache_move_plan": return cacheMovePlan();
    case "cache_move_verify": return cacheVerify();
    case "cache_move_backups": return { backups: [] };
    case "update_guard_status": return updateGuardStatus();
    default: throw new Error(`演示模式缺少只读数据: ${command}`);
  }
}
