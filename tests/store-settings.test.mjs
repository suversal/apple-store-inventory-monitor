import test from "node:test";
import assert from "node:assert/strict";
import { registerHooks } from "node:module";

// Exercise the real store with a controllable IPC boundary, without a desktop app.
let invoke;
globalThis.__settingsTestInvoke = (...args) => invoke(...args);
registerHooks({
  resolve(specifier, context, nextResolve) {
    const stubs = {
      "@tauri-apps/api/core": "export const invoke = (...args) => globalThis.__settingsTestInvoke(...args);",
      "@tauri-apps/api/event": "export const listen = async () => () => {};",
      "@tauri-apps/plugin-opener": "export const openUrl = async () => {};",
    };
    if (specifier in stubs) {
      return { url: `data:text/javascript,${encodeURIComponent(stubs[specifier])}`, shortCircuit: true };
    }
    if (specifier.startsWith("./") && !specifier.endsWith(".ts") && context.parentURL?.includes("/src/lib/")) {
      return nextResolve(`${specifier}.ts`, context);
    }
    return nextResolve(specifier, context);
  },
});

const defaults = {
  locale: "zh_CN", targets: [], intervalSeconds: 30,
  barkUrl: "https://example.invalid/old", soundEnabled: true, openBagOnHit: true,
};
let generation = 0;
async function setup() {
  const store = await import(`../src/lib/store.ts?case=${++generation}`);
  let persisted = { ...defaults };
  let release;
  let blockNext = false;
  const calls = [];
  invoke = async (command, args) => {
    calls.push(command);
    if (blockNext) {
      blockNext = false;
      await new Promise((resolve) => { release = resolve; });
    }
    if (command === "save_settings") persisted = { ...args.settings };
    if (command === "set_interval") persisted.intervalSeconds = Math.max(5, args.seconds);
    if (command === "set_targets") persisted.targets = args.targets;
    if (command === "set_targets") return [];
    if (command === "set_interval") return persisted.intervalSeconds;
    return { ...persisted };
  };
  await store.saveSettings(defaults);
  calls.length = 0;
  return {
    store, calls, persisted: () => persisted,
    block: () => { blockNext = true; },
    release: () => release(),
  };
}
const tick = () => new Promise((resolve) => setImmediate(resolve));

test("overlapping edits merge with the last saved settings", async () => {
  const ctx = await setup();
  ctx.block();
  const first = ctx.store.saveSettings({ barkUrl: "" });
  await tick();
  const second = ctx.store.saveSettings({ soundEnabled: false });
  await tick();
  assert.deepEqual(ctx.calls, ["save_settings"]);
  ctx.release();
  await Promise.all([first, second]);
  assert.deepEqual(ctx.persisted(), { ...defaults, barkUrl: "", soundEnabled: false });
});

test("interval and target commands cannot be overwritten by queued settings", async () => {
  const ctx = await setup();
  ctx.block();
  const interval = ctx.store.setIntervalSeconds(15);
  await tick();
  const targets = [{ locale: "zh_CN", storeNumber: "R390", storeTitle: "Test", partNumber: "TEST/A", productName: "Test" }];
  const updated = ctx.store.setTargets(targets);
  const saved = ctx.store.saveSettings({ soundEnabled: false });
  await tick();
  assert.deepEqual(ctx.calls, ["set_interval"]);
  ctx.release();
  await Promise.all([interval, updated, saved]);
  assert.deepEqual(ctx.persisted(), { ...defaults, intervalSeconds: 15, targets, soundEnabled: false });
});

test("test notification waits for the cleared Bark address to finish saving", async () => {
  const ctx = await setup();
  ctx.block();
  const save = ctx.store.saveSettings({ barkUrl: "" });
  await tick();
  const notification = ctx.store.testNotify();
  await tick();
  assert.deepEqual(ctx.calls, ["save_settings"]);
  ctx.release();
  await Promise.all([save, notification]);
  assert.deepEqual(ctx.calls, ["save_settings", "test_notify"]);
  assert.equal(ctx.persisted().barkUrl, "");
});

test("a failed write does not prevent subsequent edits", async () => {
  const ctx = await setup();
  const originalInvoke = invoke;
  invoke = async () => { throw new Error("disk unavailable"); };
  await ctx.store.saveSettings({ soundEnabled: false });
  invoke = originalInvoke;
  await ctx.store.saveSettings({ barkUrl: "" });
  assert.deepEqual(ctx.persisted(), { ...defaults, barkUrl: "" });
});
