import type { TargetState } from "./types.ts";

const nameOrder = new Intl.Collator("zh-CN", { numeric: true });

/** 比较机型代际，不把容量、Watch 表径或 SKU 数字误当成发布年份。 */
function iphoneRank(title: string): [number, number] | null {
  const model = title.replace(/\u00a0/g, " ").trim();
  if (!/^iPhone\b/i.test(model)) return null;
  // 当前无数字名称的独立产品线，按其所属发布代际排列。
  if (/^iPhone\s+Duo\b/i.test(model)) return [18, 4];
  if (/^iPhone\s+Air\b/i.test(model)) return [17, -1];
  const match = /^iPhone\s*(\d+)(e)?(?:\s|$)/i.exec(model);
  if (!match) return [0, 0];
  const tier = /\bPro\s+Max\b/i.test(model) ? 3 : /\bPro\b/i.test(model) ? 2 : match[2] ? 0 : 1;
  return [Number(match[1]), tier];
}

/** 当前目录没有可靠发布日：iPhone 按代际倒序，其余品类保留原有目录顺序。 */
export function compareNewestProducts(a: { title: string }, b: { title: string }): number {
  const left = iphoneRank(a.title);
  const right = iphoneRank(b.title);
  if (!left || !right) return left ? -1 : right ? 1 : 0;
  return right[0] - left[0] || right[1] - left[1];
}

export function sortMonitorsNewestFirst(rows: readonly TargetState[]): TargetState[] {
  return [...rows].sort((a, b) => {
    const left = a.target.productName;
    const right = b.target.productName;
    const generation = compareNewestProducts({ title: left }, { title: right });
    if (generation) return generation;
    // 相同机型按商品、门店稳定排列，刷新状态不会让行来回跳动。
    return nameOrder.compare(left, right) || nameOrder.compare(a.target.storeTitle, b.target.storeTitle);
  });
}
