(target) => {
  const result = (ready = false, error = null) => ({ ready, error });
  const visible = (element) => element && element.getClientRects().length > 0;
  const text = (element) => (element?.innerText || '').replace(/\s+/g, '').trim();
  const name = (value) => value.replace(/\s+/g, '').replace(/(省|市)$/, '');
  const stores = [...document.querySelectorAll('input[name="store-locator-result"]')];
  // 保留已有列表及默认门店策略。是否可取货仍由后面的门店核对负责。
  if (stores.some((store) => store.value === target.store)
      || stores.some((store) => store.checked && !store.disabled
        && (target.acceptNearest || target.monitoredStores.includes(store.value)))) {
    return result(true);
  }
  if (target.locale === 'zh_HK') {
    const progress = window.__apwHkPickup ||= { activated: false, searched: false, expanded: false };
    if (stores.length) {
      const more = document.querySelector('button[data-autom="show-more-stores-button"]') || [...document.querySelectorAll('button')].find(e => ['顯示所有取貨地點', '顯示更多取貨地點'].includes(text(e)));
      if (more && !more.disabled && !progress.expanded) {
        progress.expanded = true;
        more.click();
        return result();
      }
      return result(true);
    }
    const input = document.querySelector('input[id$="fulfillment.pickupTab.pickup.storeLocator.searchInput"]');
    if (!visible(input) || input.disabled) return result();
    if (!progress.activated) {
      progress.activated = true;
      input.click();
      input.focus();
      return result();
    }
    if (progress.searched || input.readOnly) return result();
    if (input.value !== '中環') {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, '中環');
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.dispatchEvent(new Event('change', { bubbles: true }));
      return result();
    }
    const search = document.querySelector('button[id$="storeLocator.search"]');
    if (search && !search.disabled) {
      progress.searched = true;
      performance.clearResourceTimings();
      window.__apwPickupClicked = true;
      search.click();
    }
    return result();
  }
  const region = target.region;
  if (!region) return result(false, '目标门店缺少省市信息，请更新门店目录后重试，或先手动设置取货地区');
  const picker = [...document.querySelectorAll('button[data-autom="fulfillment-pickup-store-search-button"]')].find(visible);
  if (!picker) return result();
  const panel = (part) => [...document.querySelectorAll(`[data-autom="address-field-provinceselector-${part}-panel"]`)].find((element) => visible(element) && element.getAttribute('aria-hidden') !== 'true');
  const statePanel = panel('state');
  const cityPanel = panel('city');
  const districtPanel = panel('district');
  const open = statePanel || cityPanel || districtPanel;
  if (!open && text(picker).includes(name(region.state)) && text(picker).includes(name(region.city))) {
    return result(true);
  }
  // 轮询只观察异步进度，每一级只点一次，避免重发搜索或把下拉框关掉。
  const key = JSON.stringify(region);
  if (window.__apwPickupRegion?.key !== key) {
    window.__apwPickupRegion = { key, opened: false, clicked: {} };
  }
  const progress = window.__apwPickupRegion;
  if (!open) {
    if (!progress.opened) {
      progress.opened = true;
      performance.clearResourceTimings();
      performance.setResourceTimingBufferSize(2000);
      window.__apwPickupClicked = true;
      picker.click();
    }
    return result();
  }
  const choose = (part, container, wanted) => {
    if (progress.clicked[part]) return result();
    const options = [...container.querySelectorAll('button')].filter((button) => visible(button) && !button.disabled && text(button));
    if (!options.length) return result(); // 省市级联选项尚在加载。
    const matches = options.filter((button) => wanted(text(button)));
    if (matches.length !== 1) return result(false, `无法在取货地区中唯一确定${part === 'state' ? '省份' : part === 'city' ? '城市' : '区县'}（${region.state} ${region.city}），请在下单浏览器里手动选择`);
    progress.clicked[part] = true;
    matches[0].click();
    return result();
  };
  if (statePanel) return choose('state', statePanel, (value) => name(value) === name(region.state));
  if (cityPanel) return choose('city', cityPanel, (value) => name(value) === name(region.city));
  // 直辖市可能直接从省份进入区县，不猜测、不随便选择第一个区县。
  return choose('district', districtPanel, (value) => region.address.replace(/\s+/g, '').includes(value));
}
