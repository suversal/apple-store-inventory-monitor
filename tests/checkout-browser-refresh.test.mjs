import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// Exercise the exact script included by the Rust runner, with no Apple requests.
const source = readFileSync(new URL("../src-tauri/src/checkout_browser_refresh.js", import.meta.url), "utf8");
function page({ href = "https://www.apple.com.cn/shop/bag", ready = "complete", editing = false, fields = [] } = {}) {
  const actions = [];
  const context = vm.createContext({
    URL,
    location: { href, reload: () => actions.push("reload"), replace: (url) => actions.push(url) },
    document: { readyState: ready, activeElement: { matches: () => editing }, querySelectorAll: () => fields },
  });
  const refresh = vm.compileFunction(`return (${source})(url);`, ["url"], { parsingContext: context });
  return { actions, refresh: (url = "") => refresh(url) };
}

test("current bag reloads, while fixed URL always navigates to that URL", () => {
  const current = page();
  assert.match(current.refresh(), /已请求刷新/);
  assert.deepEqual(current.actions, ["reload"]);
  const fixed = page();
  assert.match(fixed.refresh("https://www.apple.com.cn/shop/bag"), /已请求打开固定/);
  assert.deepEqual(fixed.actions, ["https://www.apple.com.cn/shop/bag"]);
});

test("login, checkout, order, payment and unrelated tabs are never changed", () => {
  for (const href of [
    "https://secure.store.apple.com.cn/shop/signIn", "https://www.apple.com.cn/shop/checkout",
    "https://www.apple.com.cn/shop/checkout/review", "https://www.apple.com.cn/shop/order/list",
    "https://www.apple.com.cn/shop/bag?checkout=true", "https://apple.com.cn.evil.test/shop/bag",
    "https://example.com/", "about:blank",
  ]) {
    const p = page({ href });
    assert.match(p.refresh("https://www.apple.com.cn/shop/bag"), /已跳过/, href);
    assert.deepEqual(p.actions, [], href);
  }
});

test("fixed URLs reject unsafe protocols, lookalikes, parameters and non-bag destinations", () => {
  for (const url of ["javascript:alert(1)", "http://www.apple.com.cn/shop/bag", "https://www.apple.com.cn.evil.test/shop/bag",
    "https://user:pass@www.apple.com.cn/shop/bag", "https://www.apple.com.cn:8443/shop/bag",
    "https://www.apple.com.cn/shop/checkout", "https://www.apple.com.cn/shop/bag?x=1", "https://www.apple.com.cn/shop/bag#checkout"]) {
    const p = page();
    assert.match(p.refresh(url), /已跳过/, url);
    assert.deepEqual(p.actions, []);
  }
});

test("regional and education bags work without taking foreground focus", () => {
  for (const href of ["https://www.apple.com/hk/shop/bag", "https://www.apple.com/jp/shop/bag/", "https://www.apple.com.cn/cn-k12/shop/bag"]) {
    const p = page({ href });
    p.refresh();
    assert.deepEqual(p.actions, ["reload"]);
  }
});

test("loading pages, focused editors and unsaved changes are skipped", () => {
  for (const options of [
    { ready: "loading" }, { ready: "interactive" }, { editing: true },
    { fields: [{ tagName: "INPUT", type: "text", value: "new", defaultValue: "old" }] },
    { fields: [{ tagName: "INPUT", type: "checkbox", checked: true, defaultChecked: false }] },
    { fields: [{ tagName: "SELECT", selectedIndex: 1, options: [{ selected: false, defaultSelected: false }, { selected: true, defaultSelected: false }] }] },
  ]) {
    const p = page(options);
    assert.match(p.refresh(), /已跳过/);
    assert.deepEqual(p.actions, []);
  }
  const unchanged = page({ fields: [{ tagName: "INPUT", type: "text", value: "old", defaultValue: "old" }] });
  unchanged.refresh();
  assert.deepEqual(unchanged.actions, ["reload"]);
  const defaultSelect = page({ fields: [{ tagName: "SELECT", selectedIndex: 0, options: [{ selected: true, defaultSelected: false }] }] });
  defaultSelect.refresh();
  assert.deepEqual(defaultSelect.actions, ["reload"]);
});
