import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import vm from 'node:vm';

const source = name => fs.readFileSync(new URL(`../src-tauri/src/checkout_${name}.js`, import.meta.url), 'utf8');
const billing = source('hk_billing');
const pickup = source('pickup_region');
const values = { cardNumber: '4111111111111111', expiration: '12/30', securityCode: '123', billingName: 'Test Buyer', street: 'Test Street', street2: '' };
const paymentUrl = 'https://secure9.store.apple.com/hk-zh/shop/checkout?_s=Billing-init';
class Input {
  set value(value) { this.saved = value; }
  get value() { return this.saved ?? ''; }
  focus() {} blur() {} dispatchEvent() {}
  getClientRects() { return [1]; }
}
function execute(script, args, document, url = paymentUrl, window = {}) {
  return JSON.parse(vm.runInNewContext(`JSON.stringify((${script})(...args))`, {
    document, location: new URL(url), window, args, HTMLInputElement: Input,
    URLSearchParams, Event: class {}, performance: { clearResourceTimings() {} },
  }));
}
function paymentPage() {
  let clicks = 0;
  const radio = { checked: false, click() { clicks++; this.checked = true; } };
  const inputs = Object.fromEntries(['cardNumber', 'expiration', 'securityCode', 'firstName', 'street', 'street2'].map(key => [key, new Input()]));
  const document = { getElementById(id) { return id.endsWith('billingoptions.credit') ? radio : inputs[id.split('.').at(-1)]; } };
  return { radio, inputs, document, clicks: () => clicks };
}

test('香港付款选择信用卡后核对全部资料，轮询不重复点击、不返回卡资料', () => {
  const page = paymentPage();
  const window = {};
  assert.equal(execute(billing, [values], page.document, paymentUrl, window).ready, false);
  assert.equal(execute(billing, [values], page.document, paymentUrl, window).ready, true);
  assert.equal(execute(billing, [values], page.document, paymentUrl, window).ready, true);
  assert.equal(page.clicks(), 1);
  assert.deepEqual(Object.values(page.inputs).map(e => e.value), Object.values(values));
  assert.deepEqual(execute(billing, [values], page.document, paymentUrl, window), { ready: true, error: null });
});

test('信用卡只填写香港官方付款页，错误地区、伪造域名、非付款步骤均不读取表单', () => {
  for (const url of [
    'http://secure9.store.apple.com/hk-zh/shop/checkout?_s=Billing',
    'https://secure9.store.apple.com.example.test/hk-zh/shop/checkout?_s=Billing',
    'https://secure9.store.apple.com:8443/hk-zh/shop/checkout?_s=Billing',
    'https://secure9.store.apple.com/shop/checkout?_s=Billing',
    'https://secure9.store.apple.com/hk-zh/shop/checkout?_s=Review',
    'https://secure9.www.apple.com.cn/shop/checkout?_s=Billing',
  ]) {
    let reads = 0;
    const state = execute(billing, [values], { getElementById() { reads++; } }, url);
    assert.equal(state.ready, false);
    assert.ok(state.error);
    assert.equal(reads, 0);
  }
});

test('香港付款缺字段或写入失败时不能进入订单核对，错误不带卡号或安全码', () => {
  const page = paymentPage();
  page.radio.checked = true;
  delete page.inputs.securityCode;
  assert.equal(execute(billing, [values], page.document).ready, false);
  const broken = paymentPage();
  broken.radio.checked = true;
  Object.defineProperty(broken.inputs.cardNumber, 'value', { get: () => '' });
  const state = execute(billing, [values], broken.document);
  assert.equal(state.ready, false);
  assert.ok(state.error);
  assert.ok(!state.error.includes(values.cardNumber));
  assert.ok(!state.error.includes(values.securityCode));
});

test('香港搜索地区只提交一次，随后展开门店列表再核对指定门店', () => {
  const input = new Input();
  input.click = () => {};
  let searches = 0;
  let expansions = 0;
  let stores = [];
  const document = {
    querySelectorAll: () => stores,
    querySelector(selector) {
      if (selector.includes('searchInput')) return input;
      if (selector.includes('storeLocator.search')) return { click() { searches++; } };
      if (selector.includes('show-more-stores')) return { click() { expansions++; stores = [{ value: 'R610' }]; } };
      return null;
    },
  };
  const target = { locale: 'zh_HK', store: 'R610', monitoredStores: [], acceptNearest: false };
  const window = {};
  for (let i = 0; i < 8; i++) execute(pickup, [target], document, paymentUrl, window);
  assert.equal(input.value, '中環');
  assert.equal(searches, 1);
  stores = [{ value: 'R428' }];
  assert.equal(execute(pickup, [target], document, paymentUrl, window).ready, false);
  assert.equal(execute(pickup, [target], document, paymentUrl, window).ready, true);
  assert.equal(expansions, 1);
});

test('香港商品 slug 页必须读取相同 SKU，繁体购物袋可核对数量并清理', () => {
  const expected = { locale: 'zh_HK', partNumber: 'MG724ZA/A', productName: 'iPhone 17 512GB 黑色', quantity: '1', removeId: 'owned.delete' };
  const document = { querySelector(selector) {
    if (selector.includes('summary-productName')) return { innerText: expected.productName };
    if (selector.includes('input[name="product"]')) return { value: expected.partNumber };
    if (selector.includes('add-to-cart')) return { disabled: false };
    return null;
  } };
  const url = 'https://www.apple.com/hk-zh/shop/buy-iphone/iphone-17/6.3-inch-display-512gb-black';
  assert.equal(execute(source('add_to_bag'), [expected], document, url).ready, true);
  assert.equal(execute(source('add_to_bag'), [{ ...expected, partNumber: 'WRONG/A' }], document, url).ready, false);
  assert.equal(execute(source('add_to_bag'), [expected], document, url.replace('/hk-zh/', '/jp/')).ready, false);
  let removals = 0;
  const title = { innerText: expected.productName, getAttribute: () => `/hk-zh/shop/product/${expected.partNumber}` };
  const remove = { id: expected.removeId, click() { removals++; } };
  const row = { innerText: `${expected.productName} 數量：1`, getAttribute: () => 'bag-item-1', querySelector(selector) {
    if (selector.includes('bag-item-name')) return title;
    if (selector.includes('remove-button')) return remove;
    return null;
  } };
  const bag = { getElementById: () => null, querySelectorAll: () => [row], querySelector: selector => selector.includes('bag-header') ? {} : null };
  const bagUrl = 'https://www.apple.com/hk-zh/shop/bag';
  const lines = execute(source('line_items'), [[]], bag, bagUrl);
  assert.equal(lines.bagReady, true);
  assert.equal(lines.items[0].quantity, '1');
  assert.equal(execute(source('remove_from_bag'), [expected, 'remove'], bag, bagUrl), true);
  assert.equal(execute(source('remove_from_bag'), [expected, 'remove'], bag, 'https://www.apple.com.cn/shop/bag'), false);
  assert.equal(removals, 1);
});


test('香港加购表单有多个 product 字段时跳过空占位，SKU 冲突则不加购', () => {
  const expected = { locale: 'zh_HK', partNumber: 'MG6J4ZA/A', productName: 'iPhone 17 256GB 黑色' };
  let products = [{ value: '' }, { value: expected.partNumber }];
  const button = { disabled: false, form: { querySelectorAll: () => products } };
  const document = { querySelector(selector) {
    if (selector.includes('summary-productName')) return { innerText: expected.productName };
    if (selector.includes('add-to-cart')) return button;
    return null;
  } };
  const url = 'https://www.apple.com/hk-zh/shop/buy-iphone/iphone-17/6.3-吋顯示器-256gb-黑色';
  assert.equal(execute(source('add_to_bag'), [expected], document, url).ready, true);
  products.push({ value: 'MG6K4ZA/A' });
  assert.equal(execute(source('add_to_bag'), [expected], document, url).ready, false);
});

test('香港旧版商品摘要只读取购物袋行，排除收藏与嵌套摘要并按 SKU 清理', () => {
  const expected = { locale: 'zh_HK', partNumber: 'MG6J4ZA/A', productName: 'iPhone 17 256GB 黑色', quantity: '1', removeId: 'owned.delete' };
  let removals = 0;
  const button = { id: expected.removeId, click() { removals++; } };
  const title = { innerText: expected.productName };
  const row = { innerText: `${expected.productName} 數量 1`, closest: () => null, contains: e => e === nested,
    getAttribute: name => name === 'data-part-number' ? expected.partNumber : null,
    querySelector(selector) {
      if (selector === '.rs-iteminfo-title') return title;
      if (selector.includes('remove-button')) return button;
      return null;
    } };
  const nested = { ...row, contains: () => false };
  const saved = { ...row, closest: () => ({}), contains: () => false };
  const document = {
    querySelectorAll: selector => selector === '.rs-bag-item, .rs-iteminfo' ? [row, nested, saved] : [],
    querySelector: () => null,
    getElementById: id => id === 'bag-content' ? {} : id === 'shoppingCart.actions.navCheckout' ? { disabled: false } : null,
  };
  const url = 'https://www.apple.com/hk-zh/shop/bag';
  const lines = execute(source('line_items'), [[]], document, url);
  assert.equal(lines.items.length, 1);
  assert.equal(lines.bagReady, true);
  assert.equal(lines.canCheckout, true);
  assert.equal(lines.items[0].partNumber, expected.partNumber);
  assert.equal(execute(source('remove_from_bag'), [expected, 'remove'], document, url), true);
  assert.equal(removals, 1);
});
