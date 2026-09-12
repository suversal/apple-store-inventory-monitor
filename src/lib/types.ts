/**
 * 与 Rust 侧一一对应的类型。
 *
 * 形状由 `crates/apw-core/tests/wire_format.rs` 钉住 —— 那些用例断言的就是这里
 * 写的结构。谁改了 Rust 的 serde 标注，那边会先红，而不是等界面上莫名少了一列
 * 才发现。
 *
 * 最关键的是 `Availability`：它在 Rust 里是「未知必然携带原因」的枚举，序列化
 * 后扁平成两层判别字段。配合下面的穷尽性检查，Rust 那边新增一个状态、这边漏了
 * 处理就直接编译不过 —— 这条不变量因此从 Rust 一路延伸到了界面上。
 */

export type UnknownReason =
  | { reason: "not_yet_checked" }
  | { reason: "blocked"; detail: string }
  | { reason: "rate_limited" }
  | { reason: "schema_drift"; field: string; raw: string }
  | { reason: "no_pickup_data"; store_number: string }
  | { reason: "product_not_returned"; part_number: string }
  | { reason: "apple_error"; message: string }
  | { reason: "transport"; detail: string };

export type Availability =
  | { kind: "in_stock" }
  | { kind: "out_of_stock" }
  | ({ kind: "unknown" } & UnknownReason);

export interface Target {
  locale: string;
  storeNumber: string;
  storeTitle: string;
  partNumber: string;
  productName: string;
}

export interface PickupDetails {
  pickupDisplay: string;
  pickupQuote: string | null;
  saleReason: string | null;
  saleMessage: string | null;
}

export interface TargetState {
  target: Target;
  availability: Availability;
  pickupDetails?: PickupDetails;
  lastCheckedMs: number | null;
  consecutiveFailures: number;
}

export interface Region {
  title: string;
  locale: string;
}

/** 与 Rust 侧 `model::Category` 一一对应。 */
export type Category = "iphone" | "ipad" | "mac" | "watch";

/**
 * 品类选项。
 *
 * 由 Rust 侧的 `list_categories` 提供，不在这里另抄一份常量 —— 抄一份就迟早会
 * 有一边先加了品类、另一边还蒙在鼓里。
 */
export interface CategoryOption {
  value: Category;
  title: string;
}

export interface Product {
  partNumber: string;
  category: Category;
  family: string;
  capacity: string;
  color: string;
  title: string;
}

export interface Store {
  number: string;
  name: string;
  title: string;
}

/** 检测到有货时自动打开的页面，与 Rust 侧 `OpenOnHit` 一一对应。 */
export type OpenOnHit = "none" | "bag" | "product";

export interface Settings {
  locale: string;
  targets: Target[];
  intervalSeconds: number;
  barkUrl: string;
  soundEnabled: boolean;
  openOnHit: OpenOnHit;
}

/** 监控目标的唯一键，与 Rust 侧 Target::key 的构成保持一致。 */
export function targetKey(t: Target): string {
  return `${t.locale}|${t.storeNumber}|${t.partNumber}`;
}

/** 一个待安装的更新。 */
export interface UpdateInfo {
  version: string;
  currentVersion: string;
  notes: string | null;
}

/**
 * 遇到故障时用户自己能做什么。与 Rust 侧 `watcher::TroubleAdvice` 一一对应。
 *
 * 由引擎判定并传过来，而不是让界面去匹配那句中文里有没有「拦截」两个字 ——
 * 那种写法在文案改动或加了别的语言之后会静默失效，而且不会有任何东西报错。
 */
export type TroubleAdvice = "try_another_network" | "wait_for_update" | "check_product";

export interface Trouble {
  reason: string;
  advice: TroubleAdvice | null;
}

export type WatcherEvent =
  | { type: "stateChanged"; state: TargetState }
  | { type: "inStock"; state: TargetState }
  | { type: "cycleStarted"; cycle: number; storeCount: number; targetCount: number }
  | { type: "cycleComplete"; cycle: number; elapsedMs: number; healthy: boolean; snapshot: TargetState[] }
  | { type: "trouble"; reason: string; advice: TroubleAdvice | null }
  | { type: "runStateChanged"; running: boolean };

/** 把建议翻译成一句能照着做的话。 */
export function describeAdvice(advice: TroubleAdvice): string {
  switch (advice) {
    case "try_another_network":
      return (
        "Apple 拒绝了这次查询，尚不能确定是会话、请求频率还是网络原因。" +
        "程序会延长重试间隔；若持续失败，可重启应用后重试，并对照官网或其他网络检查。"
      );
    case "wait_for_update":
      return "返回数据与当前解析规则不一致；如果持续出现，需要进一步排查或更新程序。";
    case "check_product":
      return "请刷新型号目录，并核对官网是否仍销售该型号、是否已开放取货。暂无数据不等于无货。";
    default:
      return assertNever(advice);
  }
}

/** 走到这里说明有分支没处理。放在 `default` 里，漏掉的分支会变成编译错误。 */
export function assertNever(x: never): never {
  throw new Error(`未处理的分支：${JSON.stringify(x)}`);
}

/** 界面上用于区分库存、购买限制及查询进度的语义配色。 */
export type StatusTone = "inStock" | "outOfStock" | "presale" | "comingSoon" | "pickupUnsupported" | "notForSale" | "unknown" | "pending";

/**
 * 把状态翻译成展示用的信息。
 *
 * 「未知（出故障）」和「待查询」必须分开：前者意味着这一行的数据不可信，
 * 要用醒目的方式呈现；后者只是还没轮到，弱化即可。上游把这两种和「无货」
 * 全糊成一种显示，用户无从分辨程序是在正常工作还是已经废了。
 */
export function describeAvailability(a: Availability): {
  label: string;
  tone: StatusTone;
  detail: string | null;
} {
  switch (a.kind) {
    case "in_stock":
      return { label: "有货", tone: "inStock", detail: null };
    case "out_of_stock":
      return { label: "无货", tone: "outOfStock", detail: null };
    case "unknown":
      return describeUnknown(a);
    default:
      return assertNever(a);
  }
}

function describeUnknown(a: { kind: "unknown" } & UnknownReason): {
  label: string;
  tone: StatusTone;
  detail: string;
} {
  switch (a.reason) {
    case "not_yet_checked":
      return { label: "待查询", tone: "pending", detail: "尚未轮到这一项" };
    case "blocked":
      return {
        label: "未知",
        tone: "unknown",
        detail: `请求被 Apple 拦截：${compactDiagnostic(a.detail)}`,
      };
    case "rate_limited":
      return {
        label: "未知",
        tone: "unknown",
        detail: "请求过于频繁被限流，正在退避",
      };
    case "schema_drift":
      return {
        label: "未知",
        tone: "unknown",
        detail: `接口返回结构与预期不符：${a.field} = ${compactDiagnostic(a.raw)}`,
      };
    case "product_not_returned":
      return { label: "未返回型号", tone: "unknown", detail: "Apple 本次门店响应未包含该 SKU；暂无库存结论，不能视为无货。" };
    case "no_pickup_data":
      return { label: "暂无数据", tone: "unknown", detail: "Apple 暂未返回该门店的取货数据；型号可能已下架、尚未开放取货或暂时不可查询，请刷新型号目录并核对官网。" };
    case "apple_error":
      return { label: "未知", tone: "unknown", detail: `Apple 返回错误：${compactDiagnostic(a.message)}` };
    case "transport":
      return { label: "未知", tone: "unknown", detail: describeTransportFailure(a.detail) };
    default:
      return assertNever(a);
  }
}

const MAX_DIAGNOSTIC_CHARS = 160;

/** 防止后端或第三方库把 JSON、对象 ID、堆栈等调试信息直接铺进用户界面。 */
function compactDiagnostic(raw: string): string {
  const normalized = raw.replace(/\s+/g, " ").trim();
  if (
    normalized.includes("exceptionDetails") ||
    normalized.includes('"className"') ||
    normalized.includes('"objectId"') ||
    normalized.includes('"stackTrace"')
  ) {
    return "返回了不可读的诊断信息";
  }
  if (normalized.length <= MAX_DIAGNOSTIC_CHARS) return normalized;
  return `${normalized.slice(0, MAX_DIAGNOSTIC_CHARS)}…`;
}

function describeTransportFailure(raw: string): string {
  const detail = compactDiagnostic(raw);
  const lower = raw.toLowerCase();
  if (
    detail.includes("浏览器安全限制") ||
    lower.includes("access is denied") ||
    lower.includes("domexception")
  ) {
    return "浏览器安全限制阻止了本次 Apple 页面访问，程序将在下一轮自动重试";
  }
  if (lower.includes("chromium") && lower.includes("超时")) {
    return "等待 Apple 页面响应超时，程序将在下一轮自动重试";
  }
  if (lower.includes("chromium") || lower.includes("浏览器")) {
    return "浏览器会话未能完成本次 Apple 查询，程序将在下一轮自动重试";
  }
  return `网络请求失败：${detail || "暂时无法连接 Apple"}，程序将在下一轮自动重试`;
}

/** 这一行的数据是否已经不可信。 */
export function isUntrusted(a: Availability): boolean {
  return a.kind === "unknown" && a.reason !== "not_yet_checked";
}

export function formatTime(ms: number | null): string {
  if (ms === null) return "—";
  const d = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
}
