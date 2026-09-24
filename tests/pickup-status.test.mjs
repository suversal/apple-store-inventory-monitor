import test from "node:test";
import assert from "node:assert/strict";
import { describeAvailability, describeAdvice, isUntrusted } from "../src/lib/types.ts";

test("门店未接入在线取货显示为暂停服务而不是查询故障", () => {
  const state = { kind: "unknown", reason: "store_pickup_unavailable", store_number: "R384" };
  assert.equal(describeAvailability(state).label, "暂停取货");
  assert.equal(describeAvailability(state).tone, "comingSoon");
  assert.equal(isUntrusted(state), false);
});

test("保护冷却明确显示等待时间且不建议用户重启", () => {
  const state = {
    kind: "unknown",
    reason: "cooling_down",
    remaining_seconds: 299,
    detail: "HTTP 541",
  };
  const presentation = describeAvailability(state);
  assert.equal(presentation.label, "冷却中");
  assert.match(presentation.detail, /299 秒后自动探测/);
  assert.match(describeAdvice("wait_for_retry"), /无需手动重启/);
});

test("新品 default 状态显示待开放取货且不计为故障", () => {
  const state = { kind: "unknown", reason: "pickup_pending" };
  const presentation = describeAvailability(state);
  assert.equal(presentation.label, "待开放取货");
  assert.equal(presentation.tone, "comingSoon");
  assert.equal(isUntrusted(state), false);
});
