(expected, action = 'inspect') => {
  // 脚本只能在已校验的 Apple 官方登录 iframe 内使用。
  if (location.origin !== (expected.locale === 'zh_HK' ? 'https://idmsa.apple.com' : 'https://idmsa.apple.com.cn')) return 'untrusted';
  const visible = e => !!e && !!e.getClientRects().length;
  const account = document.getElementById('account_name_text_field');
  const password = document.getElementById('password_text_field');
  const submit = document.getElementById('sign-in');
  if (visible(document.querySelector('input[autocomplete="one-time-code"], input[id^="char"], #captcha, input[name="captcha"]'))) return 'verification';
  if ([...document.querySelectorAll('[role="alert"], .signin-error')].some(e => visible(e) && e.innerText.trim())) return 'error';
  if (!visible(account) || !visible(submit)) return 'waiting';
  if (account.value && account.value.trim().toLowerCase() !== expected.account.toLowerCase()) return 'accountMismatch';
  const set = (input, value) => {
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, value);
    input.dispatchEvent(new Event('input', { bubbles: true }));
    input.dispatchEvent(new Event('change', { bubbles: true }));
  };
  if (visible(password)) {
    if (action !== 'password') return 'password';
    set(account, expected.account);
    set(password, expected.password);
    if (submit.disabled) return 'waiting';
    submit.click();
    return 'passwordSubmitted';
  }
  if (action !== 'account') return 'account';
  set(account, expected.account);
  if (submit.disabled) return 'waiting';
  submit.click();
  return 'accountSubmitted';
}
