/**
 * 界面状态的唯一入口。
 *
 * 有一条铁律：**前端不持有真源**。这里存的每一样东西要么是 Rust 推过来的原样
 * 副本，要么是纯粹的展示状态（比如日志文本）。绝不在前端自己判断「这个型号
 * 到底有没有货」—— 一旦前端有了自己的一份判断，那条花大力气在 Rust 里守住的
 * 不变量就会从前门溜回来。
 *
 * 用 `useSyncExternalStore` 而不是 Context 或状态库：它就是为「订阅外部数据源」
 * 设计的，而我们的数据源正是 Rust。再套一层状态库只会制造「再存一份」的诱惑。
 */

import { openUrl } from "@tauri-apps/plugin-opener";
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

import type {
  Category,
  CategoryOption,
  Product,
  Region,
  Settings,
  Store,
  Target,
  TargetState,
  Trouble,
  UpdateInfo,
  WatcherEvent,
} from "./types";
import { assertNever } from "./types";
import { describeCycleRow, describeCycleSummary } from "./monitorLog";

import { describeUpdateError, type UpdateProgress } from "./updateStatus";

const EVENT_CHANNEL = "watcher://event";
const NOTICE_CHANNEL = "watcher://notice";
const MAX_LOG_LINES = 300;

export interface UiState {
  rows: TargetState[];
  running: boolean;
  /** 非 null 表示「当前的状态不可信」，界面要挂一条持续可见的告警。 */
  trouble: Trouble | null;
  logs: string[];
  regions: Region[];
  categories: CategoryOption[];
  stores: Store[];
  products: Product[];
  /**
   * 当前正在挑选的品类。
   *
   * 只是个筛选器，所以不进设置、不落盘：它决定型号下拉框里显示哪一批商品，
   * 以及刷新按钮去抓哪几页。已经加进监控列表的目标不受它影响 —— 列表里
   * 四个品类是混在一起的，不然用户切一下品类就以为自己的监控项没了。
   */
  category: Category;
  settings: Settings;
  /** 正在从 Apple 官网刷新型号列表。 */
  refreshing: boolean;
  ready: boolean;
  /** 检查到的新版本；null 表示已是最新或还没查。 */
  update: UpdateInfo | null;
  /** 正在下载安装更新。 */
  installing: boolean;
  updateProgress: UpdateProgress | null;
  updateError: string | null;
  updateInstalled: boolean;
}

const DEFAULT_SETTINGS: Settings = {
  locale: "zh_CN",
  targets: [],
  intervalSeconds: 30,
  barkUrl: "",
  soundEnabled: true,
  openBagOnHit: true,
};

let state: UiState = {
  rows: [],
  running: false,
  trouble: null,
  logs: [],
  regions: [],
  categories: [],
  stores: [],
  products: [],
  category: "iphone",
  settings: DEFAULT_SETTINGS,
  refreshing: false,
  ready: false,
  update: null,
  installing: false,
  updateProgress: null,
  updateError: null,
  updateInstalled: false,
};

const listeners = new Set<() => void>();

/**
 * 返回缓存的引用。
 *
 * `useSyncExternalStore` 用 `Object.is` 比较，每次返回新对象会导致无限重渲染，
 * 所以只在真正变更时才替换 `state`。
 */
function getSnapshot(): UiState {
  return state;
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function update(patch: Partial<UiState>): void {
  state = { ...state, ...patch };
  for (const cb of listeners) cb();
}

function pushLog(line: string): void {
  pushLogs([line]);
}

function pushLogs(lines: string[]): void {
  if (lines.length === 0) return;
  const stamp = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  const next = [...state.logs, ...lines.map((line) => `[${stamp}] ${line}`)];
  // 定长保留。上游把日志无限拼进一个字符串，跑一整天能有几 MB，
  // 每次刷新都要重新排版，界面越用越卡。
  update({ logs: next.length > MAX_LOG_LINES ? next.slice(-MAX_LOG_LINES) : next });
}

function formatElapsed(ms: number): string {
  if (ms < 1_000) return `${Math.max(0, Math.round(ms))} 毫秒`;
  return `${(ms / 1_000).toFixed(1)} 秒`;
}

function applyEvent(event: WatcherEvent): void {
  switch (event.type) {
    case "stateChanged": {
      // 列表本身以 cycleComplete 带来的快照为准，不拿这条事件去增量改 ——
      // 它是可丢弃的，用它做增量会让界面和引擎慢慢对不上。
      // 每轮逐项日志统一由 CycleComplete 的完整快照生成，避免首轮状态变化事件
      // 与轮次日志重复，后续库存不变时也仍能保持完全相同的展示格式。
      break;
    }

    case "inStock":
      // 到货动作由 Rust 宿主执行；逐项查询结果在 CycleComplete 时统一写日志。
      break;

    case "cycleStarted":
      pushLog(
        `第 ${event.cycle} 轮开始查询：${event.storeCount} 家门店、${event.targetCount} 项监控。${event.cycle === 1 ? "首次使用时会先建立 Apple 查询会话，通常需要几秒。" : ""}`,
      );
      break;

    case "cycleComplete": {
      const recovered = event.healthy && state.trouble !== null;
      update({
        rows: event.snapshot,
        // 只有引擎明说本轮健康，才收起告警。用「所有行都没错误」去反推是
        // 不可靠的：某些故障路径下状态压根没被更新。
        trouble: event.healthy ? null : state.trouble,
      });
      const lines = event.snapshot.map((row) => describeCycleRow(event.cycle, row));
      if (recovered) lines.unshift("查询已恢复正常。");
      lines.push(
        `第 ${event.cycle} 轮完成（${formatElapsed(event.elapsedMs)}）：${describeCycleSummary(event.snapshot)}。` +
        (event.healthy ? `约 ${state.settings.intervalSeconds} 秒后查询。` : "未取得结果的项目将自动重试；请按逐项原因核对。"),
      );
      pushLogs(lines);
      break;
    }

    case "trouble":
      update({ trouble: { reason: event.reason, advice: event.advice } });
      pushLog(`告警：${event.reason}`);
      break;

    case "runStateChanged":
      if (state.running !== event.running) {
        update({ running: event.running });
        pushLog(event.running ? "已开始监控。" : "已暂停监控。");
      }
      break;

    default:
      assertNever(event);
  }
}

let unlisteners: UnlistenFn[] = [];
let starting: Promise<void> | null = null;

/**
 * 连接后端。重复调用是安全的。
 *
 * React 开发模式下 StrictMode 会把 effect 执行两次，如果每次都注册监听器，
 * 会得到两份 —— 日志每行打两遍、提醒重复触发。这个问题只在 dev 复现、生产不会，
 * 最容易漏到很后面才发现，所以这里用一个进行中的 Promise 做去重。
 */
export function connect(): Promise<void> {
  if (starting) return starting;
  starting = (async () => {
    unlisteners = await Promise.all([
      listen<WatcherEvent>(EVENT_CHANNEL, (e) => applyEvent(e.payload)),
      listen<string>(NOTICE_CHANNEL, (e) => pushLog(e.payload)),
    ]);

    const [regions, categories, settings, rows, running] = await Promise.all([
      invoke<Region[]>("list_regions"),
      invoke<CategoryOption[]>("list_categories"),
      invoke<Settings>("get_settings"),
      invoke<TargetState[]>("get_snapshot"),
      invoke<boolean>("is_running"),
    ]);
    update({ regions, categories, settings, rows, running, ready: true });
    await loadCatalog(settings.locale);
    // 启动时静默查一次。查不到就算了，不打扰用户 —— 网络不通、GitHub 抽风
    // 都会走到这里，跟「有没有新版本」是两回事。
    void checkForUpdate({ quiet: true });
  })();
  return starting;
}

export function disconnect(): void {
  for (const un of unlisteners) un();
  unlisteners = [];
  starting = null;
}

export const watcherStore = { subscribe, getSnapshot };

// ---- 命令。全部只是转发，不含任何业务判断。

/** 载入某地区的门店与型号目录。 */
export async function loadCatalog(locale: string): Promise<void> {
  try {
    const [stores, products] = await Promise.all([
      invoke<Store[]>("list_stores", { locale }),
      invoke<Product[]>("list_products", { locale }),
    ]);
    update({ stores, products });
  } catch (err) {
    // 目录读不出来不该让整个界面挂掉，但必须让用户知道下拉为什么是空的。
    update({ stores: [], products: [] });
    pushLog(`载入 ${locale} 的门店与型号失败：${String(err)}`);
  }
}

// 所有修改设置的命令按顺序执行，查询间隔和目标列表也不能被旧设置覆盖。
let settingsWrite: Promise<unknown> = Promise.resolve();

function enqueueSettingsWrite<T>(operation: () => Promise<T>): Promise<T> {
  const next = settingsWrite.then(operation);
  settingsWrite = next.catch(() => undefined);
  return next;
}

export function saveSettings(patch: Partial<Settings>): Promise<void> {
  return enqueueSettingsWrite(async () => {
    try {
      const saved = await invoke<Settings>("save_settings", {
        settings: { ...state.settings, ...patch },
      });
      update({ settings: saved });
    } catch (err) {
      pushLog(`保存设置失败：${String(err)}`);
    }
  });
}

export function setCategory(category: Category): void {
  update({ category });
}

export async function changeLocale(locale: string): Promise<void> {
  await saveSettings({ locale });
  await loadCatalog(locale);
}

export async function setTargets(targets: Target[]): Promise<boolean> {
  return enqueueSettingsWrite(async () => {
    try {
      const rows = await invoke<TargetState[]>("set_targets", { targets });
      update({ rows, settings: { ...state.settings, targets } });
      return true;
    } catch (err) {
      pushLog(`更新监控列表失败：${String(err)}`);
      return false;
    }
  });
}

export async function startWatching(): Promise<void> {
  try {
    await invoke("start_watching");
    const running = await invoke<boolean>("is_running");
    if (state.running !== running) {
      update({ running });
      pushLog(running ? "已开始监控。" : "启动命令已返回，但监控引擎没有进入运行状态。");
    }
  } catch (err) {
    pushLog(`启动监控失败：${String(err)}`);
  }
}

export async function stopWatching(): Promise<void> {
  try {
    await invoke("stop_watching");
    const running = await invoke<boolean>("is_running");
    if (state.running !== running) {
      update({ running });
      pushLog(running ? "暂停命令已返回，但监控引擎仍在运行。" : "已暂停监控。");
    }
  } catch (err) {
    pushLog(`暂停监控失败：${String(err)}`);
  }
}

export async function setIntervalSeconds(seconds: number): Promise<void> {
  return enqueueSettingsWrite(async () => {
    try {
      const applied = await invoke<number>("set_interval", { seconds });
      update({ settings: { ...state.settings, intervalSeconds: applied } });
    } catch (err) {
      pushLog(`设置查询间隔失败：${String(err)}`);
    }
  });
}

/**
 * 从 Apple 官网抓最新型号。
 *
 * 只抓当前品类的那几页。全部品类加起来有二十页、几十兆 HTML，用户想看新出的
 * Mac 没有理由等着 iPhone、iPad、Watch 一起抓完。
 */
export async function refreshProducts(): Promise<void> {
  if (state.refreshing) return;
  update({ refreshing: true });
  const locale = state.settings.locale;
  const category = state.category;
  try {
    const count = await invoke<number>("refresh_products", { locale, category });
    // 说「抓到」而不是「更新」：这个数字是本轮成功抓下来的不同零件号数，
    // 不等于目录里真的多了或改了多少行。
    pushLog(`已从 Apple 官网抓到 ${count} 个型号。`);
  } catch (err) {
    // 抓取失败仍可继续用内嵌的离线目录，只是可能缺最新机型。
    pushLog(`更新型号列表失败（仍可使用内置目录）：${String(err)}`);
  } finally {
    // 无论成败都重载一次目录。**失败时也必须重载**：后端是一页一页安装的，
    // 一个品类有八页，其中几页成功、几页失败是常事，成功那几页的新数据此刻
    // 已经在后端生效了。不重载的话界面还停在旧目录上，等到下一次因为别的
    // 原因重载时，这批变更才悄悄冒出来 —— 那时候已经没有任何提示说明它们
    // 是哪来的了。
    await loadCatalog(locale);
    update({ refreshing: false });
  }
}

export async function testNotify(): Promise<void> {
  try {
    await settingsWrite;
    await invoke("test_notify");
    pushLog("已发出测试提醒。");
  } catch (err) {
    pushLog(`测试提醒失败：${String(err)}`);
  }
}

// ---- 应用更新。只提示，不静默安装。

export async function checkForUpdate(opts?: { quiet?: boolean }): Promise<void> {
  try {
    const info = await invoke<UpdateInfo | null>("check_for_update");
    update({ update: info });
    if (!opts?.quiet) {
      pushLog(info ? `发现新版本 ${info.version}。` : "已经是最新版本。");
    }
  } catch (err) {
    // 检查更新失败不是故障，只是这次没查到。静默模式下连日志都不写，
    // 免得每次断网启动都刷一条无用信息。
    if (!opts?.quiet) pushLog(`检查更新失败：${String(err)}`);
  }
}

/**
 * 下载并安装更新。
 *
 * 刻意由用户点击触发，绝不静默进行：这是个会在抢购当口挂着的程序，
 * 自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
 */
export async function installUpdate(): Promise<void> {
  if (state.installing || state.updateInstalled) return;
  update({ installing: true, updateError: null,
    updateProgress: { phase: "checking", downloaded: 0, total: null } });
  let unlisten: UnlistenFn | undefined;
  try {
    // 先监听再发起下载，保证首个进度事件不会丢失。
    unlisten = await listen<UpdateProgress>("watcher://update-progress", (event) => {
      update({ updateProgress: event.payload });
    });
    await invoke("install_update");
    update({ updateInstalled: true, updateProgress: null });
    pushLog("更新已安装，请退出并重新打开应用。");
  } catch (err) {
    const message = describeUpdateError(err);
    update({ updateError: message, updateProgress: null });
    pushLog(message);
  } finally {
    unlisten?.();
    update({ installing: false });
  }
}

export function dismissUpdate(): void {
  if (state.installing) return;
  update({ update: null, updateError: null });
}

/** 自动更新不可用时，打开本项目的完整安装包下载页。 */
export async function openReleasePage(): Promise<void> {
  try {
    await openUrl("https://github.com/suversal/apple-store-inventory-monitor/releases/latest");
  } catch (err) {
    update({ updateError: `无法打开下载页：${String(err)}` });
  }
}
