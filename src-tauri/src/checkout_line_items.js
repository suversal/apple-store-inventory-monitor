(errors) => {
  const hk = location.pathname.startsWith('/hk-zh/');
  let rows = [...document.querySelectorAll('[data-autom^="bag-item-"]')].filter((e) => /^bag-item-\d+$/.test(e.getAttribute('data-autom')));
  if (hk && !rows.length) {
    rows = [...document.querySelectorAll('.rs-bag-item, .rs-iteminfo')]
      .filter((row, _, all) => !row.closest('.rs-savedbyyou') && !all.some(other => other !== row && other.contains(row)));
  }
  const items = rows.map((row) => {
    const title = row.querySelector('[data-autom="bag-item-name"], h2, h3') || (hk ? row.querySelector('.rs-iteminfo-title') : null);
    const select = row.querySelector('select[data-autom="item-quantity-dropdown"]') || (hk ? row.querySelector('[id$="-itemQuantity"]') : null);
    const text = (row.innerText || '').replace(/\s+/g, ' ');
    const counted = text.match(/數量\s*[:：]?\s*(\d+)|数量\s*[:：]?\s*(\d+)/);
    const link = row.querySelector('a[data-autom="bag-item-name"]');
    const part = link && (link.getAttribute('href') || '').match(/\/shop\/product\/([^/?#]+\/[^/?#]+)/i);
    const sku = hk ? row.getAttribute('data-part-number') || row.querySelector('[data-part-number]')?.getAttribute('data-part-number') || row.querySelector('[itemprop="sku"]')?.getAttribute('content') : '';
    const remove = row.querySelector('button[data-autom="bag-item-remove-button"]');
    return {
      name: (title ? title.innerText : '').replace(/\s+/g, ' ').trim(),
      quantity: select ? select.value : counted ? counted[1] || counted[2] : '',
      partNumber: part ? decodeURIComponent(part[1]).toUpperCase() : (sku || '').toUpperCase(),
      removeId: remove ? remove.id : '',
    };
  });
  const total = document.querySelector('[data-autom="bagtotalvalue"]');
  const checkout = document.querySelector('button[data-autom="checkout"]') || (hk ? document.getElementById('shoppingCart.actions.checkoutOtherPayments') || document.getElementById('shoppingCart.actions.navCheckout') : null);
  const placeOrder = document.querySelector('button[data-autom="continue-button-placeOrder"]') || (hk && new URLSearchParams(location.search).get('_s')?.startsWith('Review') ? document.getElementById('rs-checkout-continue-button-bottom') : null);
  return {
    items,
    total: total ? total.innerText.replace(/\s+/g, ' ').trim() : '',
    canCheckout: !!checkout && !checkout.disabled,
    canPlaceOrder: !!placeOrder && !placeOrder.disabled,
    // 没有商品行不代表空袋：导航加载中也没有商品行，必须等 Apple 的空袋提示。
    bagReady: ['/shop/bag', '/hk-zh/shop/bag'].includes(location.pathname) && (!!document.querySelector('[data-autom="bag-header"]') || !!document.querySelector('[data-autom="bag-empty-continueshopping-button"]') || (hk && rows.length > 0 && !!document.getElementById('bag-content'))),
    bagEmpty: !!document.querySelector('[data-autom="bag-empty-continueshopping-button"]') && items.length === 0,
    errors,
  };
}
