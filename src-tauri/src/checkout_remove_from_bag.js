(expected, action = 'ready') => {
  const hk = expected.locale === 'zh_HK';
  if (location.protocol !== 'https:' || location.hostname !== (hk ? 'www.apple.com' : 'www.apple.com.cn') || location.pathname !== (hk ? '/hk-zh/shop/bag' : '/shop/bag')) return false;
  if (!expected.removeId || !['ready', 'remove'].includes(action)) return false;
  const normalize = s => (s || '').replace(/\s+/g, '').toLowerCase();
  let rows = [...document.querySelectorAll('[data-autom^="bag-item-"]')].filter(e => /^bag-item-\d+$/.test(e.getAttribute('data-autom')));
  if (hk && !rows.length) {
    rows = [...document.querySelectorAll('.rs-bag-item, .rs-iteminfo')]
      .filter((row, _, all) => !row.closest('.rs-savedbyyou') && !all.some(other => other !== row && other.contains(row)));
  }
  const matches = rows.filter(row => row.querySelector('button[data-autom="bag-item-remove-button"]')?.id === expected.removeId);
  if (matches.length !== 1) return false;
  const row = matches[0];
  const title = row.querySelector('[data-autom="bag-item-name"], h2, h3') || (hk ? row.querySelector('.rs-iteminfo-title') : null);
  const link = row.querySelector('a[data-autom="bag-item-name"]');
  const part = link && (link.getAttribute('href') || '').match(/\/shop\/product\/([^/?#]+\/[^/?#]+)/i);
  const sku = hk ? row.getAttribute('data-part-number') || row.querySelector('[data-part-number]')?.getAttribute('data-part-number') || row.querySelector('[itemprop="sku"]')?.getAttribute('content') : '';
  const actualPart = part ? decodeURIComponent(part[1]).toUpperCase() : (sku || '').toUpperCase();
  const select = row.querySelector('select[data-autom="item-quantity-dropdown"]') || (hk ? row.querySelector('[id$="-itemQuantity"]') : null);
  const counted = (row.innerText || '').replace(/\s+/g, ' ').match(/數量\s*[:：]?\s*(\d+)|数量\s*[:：]?\s*(\d+)/);
  const quantity = select ? select.value : counted ? counted[1] || counted[2] : '';
  const remove = row.querySelector('button[data-autom="bag-item-remove-button"]');
  if (!title || actualPart !== expected.partNumber.toUpperCase() ||
      normalize(title.innerText) !== normalize(expected.productName) || quantity !== expected.quantity ||
      !remove || remove.disabled) return false;
  if (action === 'ready') return true;
  remove.click();
  return true;
}
