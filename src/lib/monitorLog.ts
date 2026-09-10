import { describeAvailability, type TargetState, type StatusTone } from "./types.ts";

/** 只解释本轮业务字段；库存判定与到货通知仍以引擎结果为准。 */
export function describeMonitorStatus(row: Pick<TargetState, "availability" | "pickupDetails">): {
  label: string; tone: StatusTone; detail: string | null;
} {
  const a = row.availability;
  const base = describeAvailability(a);
  if (a.kind === "unknown") {
    const labels: Record<string, string> = {
      no_pickup_data: "暂无取货数据", product_not_returned: "未返回型号",
      blocked: "请求被拦截", rate_limited: "请求被限流",
      transport: "查询失败", schema_drift: "响应解析失败", apple_error: "Apple 返回错误",
    };
    return { ...base, label: labels[a.reason] ?? base.label };
  }
  const d = row.pickupDetails;
  if (!d) return base;
  const evidence = [
    `取货=${short(d.pickupDisplay)}`,
    d.pickupQuote ? `Apple：${short(d.pickupQuote)}` : null,
    d.saleReason ? `购买=${short(d.saleReason)}` : null,
    d.saleMessage ? `送货：${short(d.saleMessage)}` : null,
  ].filter(Boolean).join("；");
  // 已确认有货时绝不被送货描述覆盖；暂未开售必须有明确业务证据。
  if (a.kind === "out_of_stock") {
    if (d.saleReason === "COMING_SOON") {
      return { ...base, label: "即将发售", tone: "comingSoon", detail: evidence };
    }
    if (d.saleReason === "NOT_FOR_SALE") {
      const presale = /暂未发售|尚未发售|not yet available/i.test(d.saleMessage ?? "");
      return { ...base, label: presale ? "暂未开售" : "暂不可购买", tone: presale ? "presale" : "notForSale", detail: evidence };
    }
    if (d.pickupDisplay === "ineligible") return { ...base, label: "不支持取货", tone: "pickupUnsupported", detail: evidence };
  }
  return { ...base, detail: evidence };
}

function short(raw: string): string {
  const text = raw.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
  return text.length > 160 ? `${text.slice(0, 160)}…` : text;
}

export function describeCycleRow(cycle: number, row: TargetState): string {
  const { label, detail } = describeMonitorStatus(row);
  const t = row.target;
  return `第 ${cycle} 轮 · ${label}：${t.storeTitle} [${t.storeNumber}] ${t.productName} [${t.partNumber}]${detail ? `（${detail}）` : ""}`;
}

export function describeCycleSummary(rows: TargetState[]): string {
  const counts = new Map<string, number>();
  for (const row of rows) {
    const label = describeMonitorStatus(row).label;
    counts.set(label, (counts.get(label) ?? 0) + 1);
  }
  return [...counts].map(([label, count]) => `${label} ${count} 项`).join("、") || "无监控项";
}
