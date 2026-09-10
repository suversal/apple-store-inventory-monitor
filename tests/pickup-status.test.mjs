import test from "node:test";
import assert from "node:assert/strict";
import { describeAvailability, describeAdvice, isUntrusted } from "../src/lib/types.ts";

test("空取货数据保持不可信并指导核对型号，不要求等待程序更新", () => {
  const state = { kind: "unknown", reason: "no_pickup_data", store_number: "R581" };
  assert.equal(describeAvailability(state).label, "暂无数据");
  assert.equal(isUntrusted(state), true);
  assert.match(describeAdvice("check_product"), /刷新型号目录/);
  assert.doesNotMatch(describeAdvice("check_product"), /等程序更新/);
});
