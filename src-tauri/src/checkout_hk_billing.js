(values) => {
  const result = (ready = false, error = null) => ({ ready, error });
  // 先核对地区和官方安全结账域名，再读取或填写任何付款资料。
  if (location.protocol !== 'https:' || location.port || !/^secure\d*\.store\.apple\.com$/.test(location.hostname)
      || location.pathname !== '/hk-zh/shop/checkout'
      || !new URLSearchParams(location.search).get('_s')?.startsWith('Billing')) {
    return result(false, '未在 Apple 香港官方付款页，未填写信用卡资料');
  }
  try {
    const credit = document.getElementById('checkout.billing.billingoptions.credit');
    if (!credit || credit.disabled) return result();
    if (!credit.checked) {
      if (!window.__apwHkCreditClicked) {
        window.__apwHkCreditClicked = true;
        (credit.labels?.[0] || credit).click();
      }
      return result();
    }
    const root = 'checkout.billing.billingOptions.selectedBillingOptions.creditCard.';
    const fields = {
      cardNumber: root + 'cardInputs.cardInput-0.cardNumber',
      expiration: root + 'cardInputs.cardInput-0.expiration',
      securityCode: root + 'cardInputs.cardInput-0.securityCode',
      billingName: root + 'billingAddress.address.firstName',
      street: root + 'billingAddress.address.street',
      street2: root + 'billingAddress.address.street2',
    };
    const normalize = (key, value) => ['cardNumber', 'expiration'].includes(key)
      ? value.replace(/[\s-]/g, '') : value.trim();
    for (const [key, id] of Object.entries(fields)) {
      const input = document.getElementById(id);
      if (!input && key === 'street2' && !values[key]) continue;
      if (!input || input.disabled || input.readOnly) return result();
      if (normalize(key, input.value) !== normalize(key, values[key])) {
        input.focus();
        Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, values[key]);
        input.dispatchEvent(new Event('input', { bubbles: true }));
        input.dispatchEvent(new Event('change', { bubbles: true }));
        input.blur();
      }
      if (normalize(key, input.value) !== normalize(key, values[key])) {
        return result(false, '香港付款资料未能填写或核对，请在下单浏览器里检查');
      }
    }
    return result(true);
  } catch {
    return result(false, '无法填写香港付款表单，请在下单浏览器里检查');
  }
}
