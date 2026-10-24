(expected, action = 'prepare') => {
  const normalize = (text) => (text || '').replace(/\s+/g, '').toLowerCase();
  const summary = document.querySelector('[data-autom="summary-productName"]');
  const button = document.querySelector('button[data-autom="add-to-cart"]');
  const path = decodeURIComponent(location.pathname).toLowerCase();
  const part = expected.partNumber.toLowerCase();
  const hk = expected.locale === 'zh_HK';
  // 同一个加购表单可能同时含空占位字段和实际 SKU，不能只读第一个同名字段。
  const inputs = button?.form ? [...button.form.querySelectorAll('input[name="product"]')]
    : [document.querySelector('input[name="product"]')].filter(Boolean);
  const skus = inputs.map(e => e.value?.toLowerCase()).filter(value => value && /^[a-z0-9]+\/[a-z]$/i.test(value));
  const sku = skus[0];
  const trustedPage = location.protocol === 'https:' && location.hostname === (hk ? 'www.apple.com' : 'www.apple.com.cn') &&
    (!hk || path.startsWith('/hk-zh/shop/'));
  const pageMatches = trustedPage &&
    (!sku || skus.every(value => value === part)) &&
    (path.endsWith('/' + part) || new URLSearchParams(location.search).get('product')?.toLowerCase() === part || (hk && sku === part));
  const nameMatches = !!summary && normalize(summary.innerText) === normalize(expected.productName);
  if ((!pageMatches || !nameMatches) && !(hk && action === 'prepare' && trustedPage && nameMatches && path.startsWith('/hk-zh/shop/buy-'))) {
    // 页面初次绘制可能先显示默认规格，再按 URL 恢复指定型号。只在准备阶段等待，
    // 真正点击前仍必须重新核对，不能把默认型号加入购物袋。
    return { ready: false, clicked: false, error: action === 'add' ? '商品页型号与本次到货型号不一致' : '' };
  }
  const options = [
    document.querySelector('input[data-autom="choose-noTradeIn"]'),
    document.querySelector('input[data-autom="noapplecare"]'),
  ].filter(Boolean);
  if (action === 'prepare') {
    // 只选择不带折抵、不加付费保障的普通购买；不猜测额外配置或付费服务。
    const option = options.find((e) => !e.checked && !e.disabled);
    if (option) {
      option.click();
      return { ready: false, clicked: false, error: '' };
    }
  }
  const ready = pageMatches && !!button && !button.disabled && options.every((e) => e.checked);
  if (action === 'add' && ready) {
    // 加购使用浏览器真实鼠标事件，避免表单把脚本 click 当成原生回退提交。
    button.scrollIntoView({ block: 'center' });
    const rect = button.getBoundingClientRect();
    return { ready: true, point: rect.width > 0 && rect.height > 0 ? { x: rect.x + rect.width / 2, y: rect.y + rect.height / 2 } : null, error: '' };
  }
  return { ready, clicked: false, error: '' };
}
