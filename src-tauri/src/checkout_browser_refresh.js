(fixedUrl) => {
  const isBag = (value) => {
    try {
      const url = new URL(value);
      return url.protocol === 'https:'
        && ['www.apple.com', 'www.apple.com.cn'].includes(url.hostname)
        && !url.username && !url.password && !url.port && !url.search && !url.hash
        && /^\/(?:[a-z0-9-]+\/)?shop\/bag\/?$/.test(url.pathname);
    } catch { return false; }
  };
  // Check and act in the same browser task: no stale URL check followed by a reload
  // of a page the user has since navigated to. Never touch a login/checkout tab.
  if (!isBag(location.href)) return '已跳过：当前标签页不是 Apple 购物袋（登录、结账和其他页面不刷新）';
  if (fixedUrl && !isBag(fixedUrl)) return '已跳过：固定网址不是有效的 Apple 购物袋';
  if (document.readyState !== 'complete') return '已跳过：页面正在加载';
  if (document.activeElement?.matches('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) {
    return '已跳过：正在输入或编辑页面';
  }
  const dirty = [...document.querySelectorAll('input:not([type="hidden"]):not([type="submit"]):not([type="button"]), textarea, select')]
    .some((element) => {
      if (element.tagName === 'SELECT') {
        const options = [...element.options];
        if (!element.multiple && !options.some((option) => option.defaultSelected)) return element.selectedIndex > 0;
        return options.some((option) => option.selected !== option.defaultSelected);
      }
      if (['checkbox', 'radio'].includes(element.type)) return element.checked !== element.defaultChecked;
      return element.value !== element.defaultValue;
    });
  if (dirty) return '已跳过：页面有尚未提交的修改';
  // Never click a form or an order button, or bring the browser to the foreground.
  if (fixedUrl) location.replace(fixedUrl);
  else location.reload();
  return fixedUrl ? '已请求打开固定购物袋网址（不代表登录仍有效）' : '已请求刷新购物袋（不代表登录仍有效）';
}
