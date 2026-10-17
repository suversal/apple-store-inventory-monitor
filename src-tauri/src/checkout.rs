//! 到货后自动结账（中国大陆直营店取货）。
//!
//! 使用一个带窗口的专属 Chromium 会话：用户在里面登录 Apple 账户、把商品放进
//! 购物袋；到货时由这里依次操作购物袋、选择门店、取货信息、付款方式和查看订单
//! 五个页面。页面元素按 Apple 结账页的 `data-autom` 标识定位。
//!
//! 这个会话与库存查询的无头会话完全分开：资料目录不同，也不走线路轮换。登录
//! 账户的会话频繁更换出口 IP 容易触发 Apple 的账户风控。
//!
//! 付款始终由用户完成。默认停在「查看订单」页；只有用户明确开启时才点
//! 「立即下单」，并且只点一次，无论结果如何都不重试。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use apw_core::catalog::Catalog;
use apw_core::config::{BrowserRefreshSettings, CheckoutPayment, Settings, checkout_product_key};
use apw_core::model::Target;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::Mutex;

use crate::chromium_fetcher::{ChromiumSession, app_profile_dir};

const PROFILE_DIR: &str = "checkout-profile-v1";
const BAG_URL: &str = "https://www.apple.com.cn/shop/bag";
const POLL_INTERVAL: Duration = Duration::from_millis(150);
// Apple 的结账接口实测每次要 9 到 17 秒才返回，超时必须明显长于这个数，
// 否则会在请求还没回来时重复点击，反而更慢。
const ELEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const NAVIGATION_TIMEOUT: Duration = Duration::from_secs(45);
/// 门店切换后日期和时段是异步加载的；超过这个时间仍没有时段，就按无需时段处理。
const SLOT_TIMEOUT: Duration = Duration::from_secs(8);
/// 门店页的状态连续这么久没有变化，才认为异步加载结束了。
const SETTLE_WINDOW: Duration = Duration::from_millis(1200);
const ORDER_TIMEOUT: Duration = Duration::from_secs(45);

/// 结账流程停下时所在的页面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Browser,
    Bag,
    SignIn,
    Fulfillment,
    PickupContact,
    Billing,
    Review,
    PlaceOrder,
}

impl Stage {
    fn title(self) -> &'static str {
        match self {
            Stage::Browser => "下单浏览器",
            Stage::Bag => "购物袋",
            Stage::SignIn => "登录",
            Stage::Fulfillment => "选择取货门店",
            Stage::PickupContact => "取货信息",
            Stage::Billing => "付款方式",
            Stage::Review => "查看订单",
            Stage::PlaceOrder => "提交订单",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutError {
    pub stage: Stage,
    pub message: String,
}

impl CheckoutError {
    fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
        }
    }
}

impl fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "在「{}」停下：{}", self.stage.title(), self.message)
    }
}

/// 一次自动结账要完成的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutPlan {
    pub store_number: String,
    pub store_title: String,
    pub product_name: String,
    /// 从目标门店目录读取，不沿用送货地址或浏览器记住的其他城市。
    pub pickup_region: Option<PickupRegion>,
    pub national_id_suffix: String,
    pub payment: CheckoutPayment,
    pub installment_bank: String,
    pub installment_periods: u8,
    pub place_order: bool,
    /// 这件商品还在监控的其他门店。结账页默认选中其中一家时可以直接用。
    pub monitored_stores: Vec<String>,
    pub accept_nearest_store: bool,
    /// 要填进取货信息页的联系信息：(结账页字段名, 值)。为空表示只核对 Apple 预填的内容。
    pub contact: Vec<(&'static str, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct PickupRegion {
    state: String,
    city: String,
    address: String,
}

impl PickupRegion {
    fn for_store(catalog: &Catalog, locale: &str, number: &str) -> Option<Self> {
        let store = catalog.store_by_number(locale, number)?;
        if locale != "zh_CN" || store.state.is_empty() || store.city.is_empty() {
            return None;
        }
        Some(Self {
            state: store.state,
            city: store.city,
            address: store.address,
        })
    }
}

impl CheckoutPlan {
    pub fn new(target: &Target, all: &Settings, catalog: &Catalog, place_order: bool) -> Self {
        let settings = &all.checkout;
        let monitored_stores = all
            .targets
            .iter()
            .filter(|other| {
                other.locale == target.locale
                    && checkout_product_key(other) == checkout_product_key(target)
            })
            .map(|other| other.store_number.clone())
            .collect();
        let contact = if settings.fill_contact {
            [
                ("lastName", &settings.contact_last_name),
                ("firstName", &settings.contact_first_name),
                ("emailAddress", &settings.contact_email),
                ("fullDaytimePhone", &settings.contact_phone),
            ]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(field, value)| (field, value.clone()))
            .collect()
        } else {
            Vec::new()
        };
        Self {
            contact,
            monitored_stores,
            accept_nearest_store: settings.accept_nearest_store,
            store_number: target.store_number.clone(),
            store_title: target.store_title.clone(),
            product_name: target.product_name.clone(),
            pickup_region: PickupRegion::for_store(catalog, &target.locale, &target.store_number),
            national_id_suffix: settings.national_id_suffix.clone(),
            payment: settings.payment,
            installment_bank: settings.installment_bank.clone(),
            installment_periods: settings.installment_periods,
            place_order,
        }
    }
}

fn pickup_region_js(plan: &CheckoutPlan) -> String {
    format!(
        "JSON.stringify(({})({}))",
        include_str!("checkout_pickup_region.js"),
        json!({
            "store": plan.store_number,
            "region": plan.pickup_region,
            "acceptNearest": plan.accept_nearest_store,
            "monitoredStores": plan.monitored_stores,
        })
    )
}

#[derive(Debug, Deserialize)]
struct PickupRegionState {
    ready: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutOutcome {
    /// 已停在「查看订单」页，等用户自己点「立即下单」。
    ReachedReview { store: String, total: String },
    /// 已点「立即下单」。`order_number` 为空表示页面上没有读到订单号，
    /// 订单是否成立需要用户在浏览器窗口里确认。
    OrderSubmitted {
        store: String,
        order_number: Option<String>,
        total: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct LineItem {
    name: String,
    quantity: String,
}

/// 型号名称只比较去掉空白后的文字，大小写不敏感。
fn normalized_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// 购物袋必须恰好是到货的那一件商品。
///
/// 宁可拒绝也不放宽：多一件商品、数量不是 1、或型号对不上，继续下去买到的
/// 就不是用户监控的东西。
fn verify_items(items: &[LineItem], product_name: &str) -> Result<(), String> {
    let [item] = items else {
        return Err(if items.is_empty() {
            "购物袋是空的，请先在下单浏览器里把要买的商品加入购物袋".into()
        } else {
            format!(
                "购物袋里有 {} 件商品，自动结账只处理购物袋里只有到货型号的情况",
                items.len()
            )
        });
    };
    if normalized_name(&item.name) != normalized_name(product_name) {
        return Err(format!(
            "购物袋里是「{}」，到货的是「{}」",
            item.name.trim(),
            product_name.trim()
        ));
    }
    let quantity = item.quantity.trim();
    if !quantity.is_empty() && quantity != "1" {
        return Err(format!("购物袋里的数量是 {quantity}，自动结账只买 1 件"));
    }
    Ok(())
}

/// 结账页的门店标签形如「2 Apple 静安 明天 可取货 店内取货」或「… 目前不可取货 …」。
fn store_label_unavailable(label: &str) -> bool {
    let compact: String = label.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("不可取货") || compact.contains("无法取货") || compact.contains("暂无供应")
}

fn js_string(value: &str) -> String {
    // JSON 字符串字面量同时是合法的 JavaScript 字符串字面量。
    serde_json::Value::String(value.to_owned()).to_string()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageState {
    step: String,
    sign_in: bool,
}

const PAGE_STATE_JS: &str = r#"(() => {
  const step = new URLSearchParams(location.search).get('_s') || '';
  const signIn = /signin/i.test(location.pathname) || /signin/i.test(step) || /^idmsa\./.test(location.host);
  return JSON.stringify({ step, signIn });
})()"#;

/// 页面上当前可见的错误提示，用于解释为什么点了继续却没有前进。
const PAGE_ERRORS_JS: &str = r#"(() => {
  const nodes = [...document.querySelectorAll('[role="alert"], .rs-form-error, .form-message-wrapper, .rs-error-message')];
  const texts = nodes.filter((e) => e.offsetParent).map((e) => (e.innerText || '').replace(/\s+/g, ' ').trim()).filter(Boolean);
  return JSON.stringify([...new Set(texts)].slice(0, 3));
})()"#;

/// 购物袋页和查看订单页都用同一组 `bag-item-N` 标识列出商品。
const LINE_ITEMS_JS: &str = r#"(() => {
  const rows = [...document.querySelectorAll('[data-autom^="bag-item-"]')].filter((e) => /^bag-item-\d+$/.test(e.getAttribute('data-autom')));
  const items = rows.map((row) => {
    const title = row.querySelector('[data-autom="bag-item-name"], h2, h3');
    const select = row.querySelector('select[data-autom="item-quantity-dropdown"]');
    const text = (row.innerText || '').replace(/\s+/g, ' ');
    const counted = text.match(/数量\s*(\d+)/);
    return { name: (title ? title.innerText : '').replace(/\s+/g, ' ').trim(), quantity: select ? select.value : counted ? counted[1] : '' };
  });
  const total = document.querySelector('[data-autom="bagtotalvalue"]');
  return JSON.stringify({
    items,
    total: total ? total.innerText.replace(/\s+/g, ' ').trim() : '',
    canCheckout: !!document.querySelector('button[data-autom="checkout"]'),
    canPlaceOrder: !!document.querySelector('button[data-autom="continue-button-placeOrder"]'),
  });
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lines {
    items: Vec<LineItem>,
    total: String,
    can_checkout: bool,
    can_place_order: bool,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PickupDate {
    value: String,
    checked: bool,
    disabled: bool,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreState {
    count: usize,
    found: bool,
    disabled: bool,
    checked: bool,
    visible: bool,
    label: String,
    /// 页面当前选中的门店，没有选中时为空。
    selected: String,
    /// 「查附近门店」的请求已发出但还没有返回。
    search_pending: bool,
    selected_title: String,
    selected_label: String,
    /// `None` 表示页面上没有时段下拉框。
    slots: Option<Vec<String>>,
    slot: String,
    dates: Vec<PickupDate>,
    can_continue: bool,
}

fn store_state_js(store_number: &str) -> String {
    format!(
        r#"(() => {{
  const radios = [...document.querySelectorAll('input[name="store-locator-result"]')];
  const radio = radios.find((e) => e.value === {store});
  const selected = radios.find((e) => e.checked && !e.disabled);
  const text = (e) => e && e.labels[0] ? e.labels[0].innerText.replace(/\s+/g, ' ').trim() : '';
  const title = (e) => {{ const t = e && e.labels[0] && e.labels[0].querySelector('.form-selector-title'); return t ? t.innerText.trim() : ''; }};
  const select = document.querySelector('select[data-autom="pickup-availablewindow-dropdown"]');
  const next = document.querySelector('button[data-autom="fulfillment-continue-button"]');
  return JSON.stringify({{
    count: radios.length,
    found: !!radio,
    disabled: !!radio && radio.disabled,
    checked: !!radio && radio.checked,
    visible: !!radio && !!radio.offsetParent,
    label: text(radio),
    selected: selected ? selected.value : '',
    searchPending: !!window.__apwPickupClicked && !performance.getEntriesByType('resource').some((e) => e.name.includes('/checkoutx/fulfillment') && e.name.includes('_a=search')),
    selectedTitle: title(selected),
    selectedLabel: text(selected),
    slots: select ? [...select.options].map((o) => o.value).filter(Boolean) : null,
    slot: select ? select.value : '',
    dates: [...document.querySelectorAll('input[name="bartPickupDateSelectorButtonGroup"]')].map((e) => ({{ value: e.value, checked: e.checked, disabled: e.disabled }})),
    canContinue: !!next && !next.disabled,
  }});
}})()"#,
        store = js_string(store_number)
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContactState {
    ready: bool,
    missing: Vec<String>,
    needs_id: bool,
}

const CONTACT_STATE_JS: &str = r#"(() => {
  const field = (name) => document.querySelector('input[data-autom="form-field-' + name + '"]');
  const labels = { lastName: '姓氏', firstName: '名字', emailAddress: '电子邮件地址', fullDaytimePhone: '手机号码' };
  const missing = Object.keys(labels).filter((name) => { const e = field(name); return !e || !e.value.trim(); }).map((name) => labels[name]);
  return JSON.stringify({ ready: !!field('lastName'), missing, needsId: !!field('nationalIdSelf') });
})()"#;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BillingChoice {
    count: usize,
    found: bool,
    id: String,
    checked: bool,
    options: Vec<String>,
}

fn billing_choice_js(plan: &CheckoutPlan) -> String {
    let (kind, bank) = match plan.payment {
        CheckoutPayment::Alipay => ("ALIPAY", ""),
        CheckoutPayment::Wechat => ("WECHAT", ""),
        CheckoutPayment::Installment => ("INSTALLMENT", plan.installment_bank.as_str()),
    };
    format!(
        r#"(() => {{
  const kind = {kind};
  const bank = {bank}.replace(/\s+/g, '');
  const radios = [...document.querySelectorAll('input[data-autom^="checkout-billingOptions-"]')];
  const text = (e) => {{
    const label = e.labels[0];
    if (!label) return e.value;
    const alts = [...label.querySelectorAll('img')].map((img) => img.alt).join(' ');
    return ((label.innerText || '') + ' ' + alts).replace(/\s+/g, '') || e.value;
  }};
  const radio = kind === 'INSTALLMENT'
    ? radios.find((e) => e.value.startsWith('installments') && bank && text(e).includes(bank))
    : radios.find((e) => e.value === kind);
  // 选中状态要等服务器返回才更新。这段脚本会被反复执行，
  // 所以两次点击之间留出间隔，免得每次轮询都重新发一遍请求。
  const now = Date.now();
  if (radio && !radio.checked && now - (window.__apwLastOptionPick || 0) > 20000) {{
    window.__apwLastOptionPick = now;
    radio.click();
    if (!radio.checked && radio.labels[0]) radio.labels[0].click();
  }}
  return JSON.stringify({{
    count: radios.length,
    found: !!radio,
    id: radio ? radio.value : '',
    checked: !!radio && radio.checked,
    options: radios.map(text),
  }});
}})()"#,
        kind = js_string(kind),
        bank = js_string(bank)
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeriodChoice {
    found: bool,
    checked: bool,
    periods: Vec<String>,
}

fn period_choice_js(option_id: &str, periods: u8) -> String {
    format!(
        r#"(() => {{
  const prefix = {id} + '-';
  const radios = [...document.querySelectorAll('input[type="radio"]')].filter((e) => (e.getAttribute('data-autom') || '').startsWith(prefix) && e.offsetParent);
  const radio = radios.find((e) => e.getAttribute('data-autom') === prefix + {periods});
  // 选中状态要等服务器返回才更新。这段脚本会被反复执行，
  // 所以两次点击之间留出间隔，免得每次轮询都重新发一遍请求。
  const now = Date.now();
  if (radio && !radio.checked && now - (window.__apwLastPeriodPick || 0) > 20000) {{
    window.__apwLastPeriodPick = now;
    radio.click();
    if (!radio.checked && radio.labels[0]) radio.labels[0].click();
  }}
  return JSON.stringify({{
    found: !!radio,
    checked: !!radio && radio.checked,
    periods: radios.map((e) => e.value),
  }});
}})()"#,
        id = js_string(option_id),
        periods = js_string(&periods.to_string())
    )
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderState {
    step: String,
    order: String,
}

const ORDER_STATE_JS: &str = r#"(() => {
  const found = (document.body ? document.body.innerText : '').match(/W\d{8,12}/);
  return JSON.stringify({ step: new URLSearchParams(location.search).get('_s') || '', order: found ? found[0] : '' });
})()"#;

fn click_js(selector: &str) -> String {
    format!(
        r#"(() => {{
  const e = document.querySelector({selector});
  if (!e || e.disabled) return false;
  e.click();
  return true;
}})()"#,
        selector = js_string(selector)
    )
}

struct Flow<'a> {
    session: &'a mut ChromiumSession,
    plan: &'a CheckoutPlan,
    progress: &'a (dyn Fn(String) + Send + Sync),
    /// 实际取货的门店。默认是到货的那一家，沿用结账页默认选中的门店时会变。
    store_number: String,
    store_title: String,
}

impl Flow<'_> {
    fn report(&self, message: impl Into<String>) {
        (self.progress)(message.into());
    }

    async fn read<T: DeserializeOwned>(&mut self, script: &str) -> Result<T, String> {
        let value = self
            .session
            .evaluate(script, false)
            .await
            .map_err(|e| e.to_string())?;
        let raw = value
            .as_str()
            .ok_or_else(|| "页面脚本没有返回结果".to_string())?;
        serde_json::from_str(raw).map_err(|e| format!("页面返回的数据无法解析：{e}"))
    }

    /// 反复读取页面状态直到满足条件。
    ///
    /// 页面跳转瞬间执行脚本会失败（执行上下文被销毁），这属于正常现象，
    /// 所以读取失败只记下来，继续等到超时。
    async fn wait<T: DeserializeOwned>(
        &mut self,
        stage: Stage,
        script: &str,
        timeout: Duration,
        waiting_for: &str,
        done: impl Fn(&T) -> bool,
    ) -> Result<T, CheckoutError> {
        let deadline = Instant::now() + timeout;
        loop {
            let last_error = match self.read::<T>(script).await {
                Ok(state) if done(&state) => {
                    #[cfg(test)]
                    eprintln!(
                        "    等到{waiting_for}：{:.1}s",
                        (timeout - (deadline - Instant::now())).as_secs_f32()
                    );
                    return Ok(state);
                }
                Ok(_) => None,
                Err(error) => Some(error),
            };
            if Instant::now() >= deadline {
                #[cfg(test)]
                eprintln!("    等待{waiting_for}超时：{:.1}s", timeout.as_secs_f32());
                let detail = last_error.map(|e| format!("（{e}）")).unwrap_or_default();
                return Err(CheckoutError::new(
                    stage,
                    format!("等待{waiting_for}超时{detail}"),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn click(
        &mut self,
        stage: Stage,
        selector: &str,
        name: &str,
    ) -> Result<(), CheckoutError> {
        let clicked = self
            .session
            .evaluate(&click_js(selector), false)
            .await
            .map_err(|e| CheckoutError::new(stage, e.to_string()))?;
        if clicked.as_bool() == Some(true) {
            Ok(())
        } else {
            Err(CheckoutError::new(stage, format!("「{name}」按钮不可点击")))
        }
    }

    /// 等结账流程进入下一页。途中出现登录页就立即停下，交给用户处理。
    async fn wait_for_step(
        &mut self,
        stage: Stage,
        step_prefix: &str,
    ) -> Result<(), CheckoutError> {
        let deadline = Instant::now() + NAVIGATION_TIMEOUT;
        loop {
            if let Ok(state) = self.read::<PageState>(PAGE_STATE_JS).await {
                if state.step.starts_with(step_prefix) {
                    #[cfg(test)]
                    eprintln!(
                        "    进入 {step_prefix}：{:.1}s",
                        (NAVIGATION_TIMEOUT - (deadline - Instant::now())).as_secs_f32()
                    );
                    return Ok(());
                }
                if state.sign_in {
                    return Err(CheckoutError::new(
                        Stage::SignIn,
                        "Apple 要求重新登录。请在下单浏览器里登录并手动走一次结账，之后再开启自动结账",
                    ));
                }
            }
            if Instant::now() >= deadline {
                let errors: Vec<String> = self.read(PAGE_ERRORS_JS).await.unwrap_or_default();
                let detail = if errors.is_empty() {
                    String::new()
                } else {
                    format!("，页面提示：{}", errors.join("；"))
                };
                return Err(CheckoutError::new(
                    stage,
                    format!("点击继续后页面没有前进{detail}"),
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn bag(&mut self) -> Result<(), CheckoutError> {
        self.report("自动结账：打开购物袋");
        let navigation = self
            .session
            .command("Page.navigate", json!({ "url": BAG_URL }))
            .await
            .map_err(|e| CheckoutError::new(Stage::Bag, e.to_string()))?;
        if let Some(error) = navigation
            .get("errorText")
            .and_then(serde_json::Value::as_str)
            .filter(|error| !error.trim().is_empty())
        {
            return Err(CheckoutError::new(
                Stage::Bag,
                format!("购物袋页面打不开：{error}"),
            ));
        }
        let lines: Lines = self
            .wait(
                Stage::Bag,
                LINE_ITEMS_JS,
                ELEMENT_TIMEOUT,
                "购物袋出现可结账的商品",
                |lines: &Lines| lines.can_checkout && !lines.items.is_empty(),
            )
            .await
            .map_err(|_| {
                CheckoutError::new(
                    Stage::Bag,
                    "购物袋里没有可结账的商品，请先在下单浏览器里登录并把商品加入购物袋",
                )
            })?;
        verify_items(&lines.items, &self.plan.product_name)
            .map_err(|message| CheckoutError::new(Stage::Bag, message))?;
        self.click(Stage::Bag, r#"button[data-autom="checkout"]"#, "结账")
            .await?;
        self.wait_for_step(Stage::Bag, "Fulfillment").await
    }

    async fn fulfillment(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::Fulfillment;
        self.report(format!("自动结账：选择 {} 取货", self.plan.store_title));

        // 结账默认停在「为我送货」，取货门店列表要切到「我要取货」后才加载。
        const PICKUP_TAB_JS: &str = r#"(() => {
  const tab = [...document.querySelectorAll('button')].find((e) => (e.innerText || '').trim() === '我要取货');
  if (!tab) return JSON.stringify('missing');
  // 这是一组 role="radio" 的按钮，选中状态写在 aria-checked 上。
  if (tab.getAttribute('aria-checked') === 'true' || tab.getAttribute('aria-pressed') === 'true') return JSON.stringify('selected');
  // 切过去会触发一次「查附近门店」的请求，后面要靠资源计时记录判断它是否返回，
  // 所以先清空记录并放大缓冲区，免得记录因为缓冲区满了而丢失。
  if (!window.__apwPickupClicked) {
    performance.clearResourceTimings();
    performance.setResourceTimingBufferSize(2000);
    window.__apwPickupClicked = true;
  }
  tab.click();
  return JSON.stringify('clicked');
})()"#;
        self.wait(
            stage,
            PICKUP_TAB_JS,
            ELEMENT_TIMEOUT,
            "「我要取货」选项",
            |state: &String| state == "selected",
        )
        .await?;

        self.prepare_pickup_region().await?;
        let mut script = store_state_js(&self.plan.store_number);
        self.wait(
            stage,
            &script,
            ELEMENT_TIMEOUT,
            "取货门店列表",
            |s: &StoreState| s.count > 0,
        )
        .await?;
        // 门店列表会先用页面自带的数据显示出来，但「查附近门店」的请求实测要十秒左右
        // 才返回，返回时页面会重画一遍，把这期间选的门店和时段清掉。Apple 的服务器
        // 又是按顺序处理请求的，抢在它前面操作并不会更快，所以等它回来再动手。
        let _ = self
            .wait(
                stage,
                &script,
                ELEMENT_TIMEOUT,
                "附近门店查询返回",
                |s: &StoreState| !s.search_pending,
            )
            .await;
        let mut state = self.settle(&script).await?;

        // Apple 会默认选中离取货地区最近的有货门店。换门店要再等它加载一轮（实测
        // 十几秒），所以默认选中的门店可以接受时直接用，不去换。
        let acceptable =
            self.plan.accept_nearest_store || self.plan.monitored_stores.contains(&state.selected);
        if !state.selected.is_empty()
            && state.selected != self.plan.store_number
            && acceptable
            && !store_label_unavailable(&state.selected_label)
        {
            self.store_number = state.selected.clone();
            self.store_title = if state.selected_title.is_empty() {
                state.selected.clone()
            } else {
                state.selected_title.clone()
            };
            self.report(format!(
                "自动结账：结账页已默认选中 {}，可取货，直接在这家取货",
                self.store_title
            ));
            script = store_state_js(&self.store_number);
            // 页面刚刚已经稳定，这里只是换成以这家门店为准重新读一次。
            state = self
                .read(&script)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
        }
        if !state.found {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "结账页的门店列表里没有 {}（{}）。请在下单浏览器的购物袋里把取货地区改到这家门店附近",
                    self.plan.store_title, self.plan.store_number
                ),
            ));
        }
        if state.disabled || store_label_unavailable(&state.label) {
            return Err(CheckoutError::new(
                stage,
                format!("结账页显示这家门店目前不可取货：{}", state.label),
            ));
        }
        // 换门店会触发一轮异步加载：门店先短暂变回未选中，上一家门店的时段列表
        // 也会多留一会儿才被替换。所以每一步都等页面稳定下来再读，
        // 选完时段还要确认门店和时段都保持住，被重置了就重来。
        let mut confirmed = false;
        let mut last_seen = String::new();
        for _ in 0..3 {
            let state = self.settle(&script).await?;
            if !state.checked {
                last_seen = "门店未选中".into();
                self.select_store(&script, &state).await?;
                // 门店加载期间继续按钮是禁用的，它恢复可用才说明这家门店的日期和时段到位了。
                // 过早去选时段，选中的值会在加载完成时被清空。
                let _ = self
                    .wait(
                        stage,
                        &script,
                        ELEMENT_TIMEOUT,
                        "门店加载完成",
                        |s: &StoreState| s.checked && s.can_continue,
                    )
                    .await;
                continue;
            }
            let slot = self.pick_slot(&script, &state).await?;
            // 选完时段后 Apple 要向服务器校验，这期间继续按钮是禁用的，要等它恢复。
            let wanted = slot.clone().unwrap_or_default();
            let _ = self
                .wait(
                    stage,
                    &script,
                    ELEMENT_TIMEOUT,
                    "继续按钮可用",
                    // 时段被页面清掉时也立刻返回，交给下一轮重选，不用干等到超时。
                    |s: &StoreState| (s.checked && s.can_continue) || s.slot != wanted,
                )
                .await;
            let state = self.settle(&script).await?;
            // 「查看订单」页不显示门店，这里是确认门店选对的最后机会。
            if state.checked && state.can_continue && state.slot == wanted {
                if let Some(slot) = slot {
                    self.report(format!("自动结账：取货时段 {slot}"));
                }
                confirmed = true;
                break;
            }
            last_seen = format!(
                "门店{}，要选的时段 {}，页面上的时段 {}，可选 {} 个，继续按钮{}",
                if state.checked {
                    "已选中"
                } else {
                    "未选中"
                },
                slot.as_deref().unwrap_or("无"),
                if state.slot.is_empty() {
                    "空"
                } else {
                    &state.slot
                },
                state.slots.as_ref().map_or(0, Vec::len),
                if state.can_continue {
                    "可用"
                } else {
                    "不可用"
                },
            );
        }
        if !confirmed {
            return Err(CheckoutError::new(
                stage,
                format!("门店或取货时段没有保持选中，没有继续（{last_seen}）"),
            ));
        }
        self.click(
            stage,
            r#"button[data-autom="fulfillment-continue-button"]"#,
            "继续填写取货详情",
        )
        .await?;
        self.wait_for_step(stage, "PickupContact").await
    }

    /// 列表为空时可能是官网还没选地区，必须先完成目标门店所在省市区的选择。
    async fn prepare_pickup_region(&mut self) -> Result<(), CheckoutError> {
        let script = pickup_region_js(self.plan);
        let state: PickupRegionState = self
            .wait(
                Stage::Fulfillment,
                &script,
                NAVIGATION_TIMEOUT,
                "目标门店的取货地区选择完成",
                |state: &PickupRegionState| state.ready || state.error.is_some(),
            )
            .await?;
        if let Some(error) = state.error {
            return Err(CheckoutError::new(Stage::Fulfillment, error));
        }
        Ok(())
    }

    /// 等门店页稳定：连续一段时间读到的状态完全相同。
    async fn settle(&mut self, script: &str) -> Result<StoreState, CheckoutError> {
        let stage = Stage::Fulfillment;
        let deadline = Instant::now() + ELEMENT_TIMEOUT;
        let mut last: Option<(StoreState, Instant)> = None;
        loop {
            if let Ok(state) = self.read::<StoreState>(script).await {
                match &last {
                    Some((previous, since)) if *previous == state => {
                        if since.elapsed() >= SETTLE_WINDOW {
                            #[cfg(test)]
                            eprintln!(
                                "    页面稳定：{:.1}s",
                                (ELEMENT_TIMEOUT - (deadline - Instant::now())).as_secs_f32()
                            );
                            return Ok(state);
                        }
                    }
                    _ => last = Some((state, Instant::now())),
                }
            }
            if Instant::now() >= deadline {
                return Err(CheckoutError::new(
                    stage,
                    "门店页面一直在变化，没有稳定下来",
                ));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn select_store(
        &mut self,
        script: &str,
        state: &StoreState,
    ) -> Result<(), CheckoutError> {
        let stage = Stage::Fulfillment;
        // 列表默认只展开前几家，其余收在「显示更多取货地点」里。
        // 收起状态下的单选项点了不会生效，还会把已选的门店清掉，所以先展开。
        if !state.visible {
            self.click(
                stage,
                r#"button[data-autom="show-more-stores-button"]"#,
                "显示更多取货地点",
            )
            .await?;
            self.wait(
                stage,
                script,
                ELEMENT_TIMEOUT,
                "门店列表展开",
                |s: &StoreState| s.visible,
            )
            .await?;
            // 展开动画期间点门店不会生效。
            self.settle(script).await?;
        }
        let selector = format!(
            r#"input[name="store-locator-result"][value={}]"#,
            js_string(&self.store_number)
        );
        self.click(stage, &selector, "门店").await
    }

    /// 选中最早可用的取货时段并返回它；这件商品不需要时段时返回 `None`。
    ///
    /// 日期默认选中最早的一天。那一天没有时段时，依次试后面的日期。
    async fn pick_slot(
        &mut self,
        script: &str,
        current: &StoreState,
    ) -> Result<Option<String>, CheckoutError> {
        // 时段已经选好就不要再动它：每改一次都会触发一轮服务器校验。
        if !current.slot.is_empty()
            && current
                .slots
                .as_ref()
                .is_some_and(|slots| slots.contains(&current.slot))
        {
            return Ok(Some(current.slot.clone()));
        }
        let stage = Stage::Fulfillment;
        let mut tried_dates: Vec<String> = Vec::new();
        let slot = loop {
            let loaded = self
                .wait(
                    stage,
                    script,
                    SLOT_TIMEOUT,
                    "取货时段",
                    |s: &StoreState| {
                        s.checked && s.slots.as_ref().is_some_and(|slots| !slots.is_empty())
                    },
                )
                .await;
            match loaded {
                Ok(state) => break state.slots.and_then(|slots| slots.into_iter().next()),
                Err(error) => {
                    let state: StoreState = self
                        .read(script)
                        .await
                        .map_err(|e| CheckoutError::new(stage, e))?;
                    if !state.checked {
                        return Err(CheckoutError::new(stage, "门店没有选中"));
                    }
                    if state.slots.is_none() {
                        // 这件商品不需要预约签到时段。
                        break None;
                    }
                    if let Some(current) = state.dates.iter().find(|date| date.checked) {
                        tried_dates.push(current.value.clone());
                    }
                    let Some(next) = state
                        .dates
                        .iter()
                        .find(|date| !date.disabled && !tried_dates.contains(&date.value))
                    else {
                        return Err(CheckoutError::new(
                            stage,
                            format!("没有可选的取货时段（{}）", error.message),
                        ));
                    };
                    let selector = format!(
                        r#"input[name="bartPickupDateSelectorButtonGroup"][value={}]"#,
                        js_string(&next.value)
                    );
                    self.click(stage, &selector, "取货日期").await?;
                    self.settle(script).await?;
                }
            }
        };
        let Some(slot) = slot else {
            return Ok(None);
        };

        // 受控的下拉框只认原生 setter 加事件，直接赋值不会更新页面状态。
        let set_slot = format!(
            r#"(() => {{
  const select = document.querySelector('select[data-autom="pickup-availablewindow-dropdown"]');
  if (!select) return JSON.stringify('');
  Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(select, {slot});
  select.dispatchEvent(new Event('input', {{ bubbles: true }}));
  select.dispatchEvent(new Event('change', {{ bubbles: true }}));
  return JSON.stringify(select.value);
}})()"#,
            slot = js_string(&slot)
        );
        let applied: String = self
            .read(&set_slot)
            .await
            .map_err(|e| CheckoutError::new(stage, e))?;
        if applied != slot {
            return Err(CheckoutError::new(stage, "取货时段没有选中"));
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        Ok(Some(slot))
    }

    async fn pickup_contact(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::PickupContact;
        self.report("自动结账：填写取货信息");
        let state: ContactState = self
            .wait(
                stage,
                CONTACT_STATE_JS,
                ELEMENT_TIMEOUT,
                "取货信息表单",
                |s: &ContactState| s.ready,
            )
            .await?;
        // 设置里开了「填写联系信息」就用设置里的值覆盖，否则沿用 Apple 预填的内容。
        let state = if self.plan.contact.is_empty() {
            state
        } else {
            let fields: serde_json::Map<String, serde_json::Value> = self
                .plan
                .contact
                .iter()
                .map(|(field, value)| ((*field).to_owned(), json!(value)))
                .collect();
            let fill = format!(
                r#"(() => {{
  const values = {values};
  const failed = [];
  for (const [name, value] of Object.entries(values)) {{
    const input = document.querySelector('input[data-autom="form-field-' + name + '"]');
    if (!input) {{ failed.push(name); continue; }}
    input.focus();
    Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, value);
    input.dispatchEvent(new Event('input', {{ bubbles: true }}));
    input.dispatchEvent(new Event('change', {{ bubbles: true }}));
    input.blur();
    if (input.value !== value) failed.push(name);
  }}
  return JSON.stringify(failed);
}})()"#,
                values = serde_json::Value::Object(fields)
            );
            let failed: Vec<String> = self
                .read(&fill)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
            if !failed.is_empty() {
                return Err(CheckoutError::new(
                    stage,
                    format!("联系信息没有填进去：{}", failed.join("、")),
                ));
            }
            self.read(CONTACT_STATE_JS)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?
        };
        // 下单后取货信息无法更改，缺了任何一项都停下，不带着空项往下走。
        if !state.missing.is_empty() {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "取货联系人缺少{}。请在设置里填好联系信息，或在下单浏览器里手动走一次结账",
                    state.missing.join("、")
                ),
            ));
        }
        if state.needs_id {
            let fill = format!(
                r#"(() => {{
  const input = document.querySelector('input[data-autom="form-field-nationalIdSelf"]');
  if (!input) return JSON.stringify(false);
  input.focus();
  Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, {suffix});
  input.dispatchEvent(new Event('input', {{ bubbles: true }}));
  input.dispatchEvent(new Event('change', {{ bubbles: true }}));
  input.blur();
  return JSON.stringify(input.value === {suffix});
}})()"#,
                suffix = js_string(&self.plan.national_id_suffix)
            );
            let filled: bool = self
                .read(&fill)
                .await
                .map_err(|e| CheckoutError::new(stage, e))?;
            if !filled {
                return Err(CheckoutError::new(stage, "证件号码后四位没有填进去"));
            }
        }
        self.click(
            stage,
            r#"button[data-autom="continue-button-label"]"#,
            "继续选择付款方式",
        )
        .await?;
        self.wait_for_step(stage, "Billing").await
    }

    async fn billing(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::Billing;
        self.report("自动结账：选择付款方式");
        let script = billing_choice_js(self.plan);
        let choice: BillingChoice = self
            .wait(
                stage,
                &script,
                ELEMENT_TIMEOUT,
                "付款方式列表",
                |c: &BillingChoice| c.count > 0 && (!c.found || c.checked),
            )
            .await?;
        if !choice.found {
            return Err(CheckoutError::new(
                stage,
                format!(
                    "结账页没有设置里选的付款方式，当前可选：{}",
                    choice.options.join("、")
                ),
            ));
        }
        if self.plan.payment == CheckoutPayment::Installment {
            // 期数选项在选中分期方案后才异步加载。
            let script = period_choice_js(&choice.id, self.plan.installment_periods);
            let period = self
                .wait(
                    stage,
                    &script,
                    ELEMENT_TIMEOUT,
                    "分期期数",
                    |p: &PeriodChoice| p.found && p.checked,
                )
                .await;
            if period.is_err() {
                let available: PeriodChoice = self
                    .read(&script)
                    .await
                    .map_err(|e| CheckoutError::new(stage, e))?;
                return Err(CheckoutError::new(
                    stage,
                    format!(
                        "{} 没有 {} 期，当前可选：{}",
                        self.plan.installment_bank,
                        self.plan.installment_periods,
                        available.periods.join("、")
                    ),
                ));
            }
        }
        self.click(
            stage,
            r#"button[data-autom="continue-button-review"]"#,
            "检查订单",
        )
        .await?;
        self.wait_for_step(stage, "Review").await
    }

    async fn review(&mut self) -> Result<CheckoutOutcome, CheckoutError> {
        let stage = Stage::Review;
        let lines: Lines = self
            .wait(
                stage,
                LINE_ITEMS_JS,
                ELEMENT_TIMEOUT,
                "订单确认内容",
                |lines: &Lines| lines.can_place_order && !lines.items.is_empty(),
            )
            .await?;
        verify_items(&lines.items, &self.plan.product_name)
            .map_err(|message| CheckoutError::new(stage, message))?;
        if !self.plan.place_order {
            return Ok(CheckoutOutcome::ReachedReview {
                store: self.store_title.clone(),
                total: lines.total,
            });
        }

        self.report("自动结账：核对通过，提交订单");
        // 这个按钮只点一次。之后无论页面怎样，都不重试，避免重复下单。
        self.click(
            Stage::PlaceOrder,
            r#"button[data-autom="continue-button-placeOrder"]"#,
            "立即下单",
        )
        .await?;
        let order = self
            .wait(
                Stage::PlaceOrder,
                ORDER_STATE_JS,
                ORDER_TIMEOUT,
                "订单号",
                |state: &OrderState| !state.order.is_empty() && !state.step.starts_with("Review"),
            )
            .await
            .ok()
            .map(|state| state.order);
        Ok(CheckoutOutcome::OrderSubmitted {
            store: self.store_title.clone(),
            order_number: order,
            total: lines.total,
        })
    }

    async fn run(&mut self) -> Result<CheckoutOutcome, CheckoutError> {
        self.bag().await?;
        self.fulfillment().await?;
        self.pickup_contact().await?;
        self.billing().await?;
        self.review().await
    }
}

/// 下单浏览器会话的持有者。同一时间只运行一次结账。
#[derive(Clone, Default)]
pub struct CheckoutRunner {
    session: Arc<Mutex<Option<ChromiumSession>>>,
    active_checkout: Arc<AtomicBool>,
}

struct CheckoutActivity<'a>(&'a AtomicBool);

impl Drop for CheckoutActivity<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CheckoutRunner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 确保下单浏览器开着并且连接可用；用户关掉窗口后会重新启动。
    async fn ensure(
        slot: &mut Option<ChromiumSession>,
    ) -> Result<&mut ChromiumSession, CheckoutError> {
        let alive = match slot.as_mut() {
            Some(session) => session.evaluate("1", false).await.is_ok(),
            None => false,
        };
        if !alive {
            // 先释放旧会话，否则新进程会因为资料目录被占用而启动失败。
            *slot = None;
            let session = ChromiumSession::start_visible(app_profile_dir(PROFILE_DIR))
                .await
                .map_err(|e| CheckoutError::new(Stage::Browser, e.to_string()))?;
            *slot = Some(session);
        }
        slot.as_mut()
            .ok_or_else(|| CheckoutError::new(Stage::Browser, "下单浏览器没有启动"))
    }

    /// 打开下单浏览器并显示购物袋，供用户登录、加购和预热结账。
    pub async fn open(&self) -> Result<(), CheckoutError> {
        let mut slot = self.session.lock().await;
        let session = Self::ensure(&mut slot).await?;
        let browser_error =
            |e: apw_core::apple::ApiError| CheckoutError::new(Stage::Browser, e.to_string());
        session
            .command("Page.navigate", json!({ "url": BAG_URL }))
            .await
            .map_err(browser_error)?;
        session
            .command("Page.bringToFront", json!({}))
            .await
            .map_err(browser_error)?;
        Ok(())
    }

    /// 执行一次结账。已有结账在进行时直接返回错误，不排队。
    pub async fn run(
        &self,
        plan: &CheckoutPlan,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<CheckoutOutcome, CheckoutError> {
        if self
            .active_checkout
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(CheckoutError::new(
                Stage::Browser,
                "已有一次结账正在进行，本次到货没有重复触发",
            ));
        }
        let _activity = CheckoutActivity(&self.active_checkout);
        // A short refresh must never make a stock hit look like a duplicate
        // checkout. Reserve the checkout first, then wait for its session lock.
        let mut slot = self.session.lock().await;
        let session = Self::ensure(&mut slot).await?;
        // 把窗口带到前台：流程停下时用户需要立刻看到页面。
        let _ = session.command("Page.bringToFront", json!({})).await;
        Flow {
            session,
            plan,
            progress,
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
        }
        .run()
        .await
    }

    /// 是否有一次结账正在进行。
    pub fn is_busy(&self) -> bool {
        self.active_checkout.load(Ordering::Acquire)
    }

    /// Only use the existing managed tab; never launch/reconnect a closed browser
    /// or borrow a user's ordinary Chrome profile for a background refresh.
    pub async fn refresh_browser(
        &self,
        settings: &BrowserRefreshSettings,
    ) -> Result<String, String> {
        settings.validate()?;
        if self.is_busy() {
            return Ok("已跳过：自动结账正在进行".into());
        }
        let Ok(mut slot) = self.session.try_lock() else {
            return Ok("已跳过：下单浏览器正在使用".into());
        };
        let Some(session) = slot.as_mut() else {
            return Ok("已跳过：请先手动打开下单浏览器".into());
        };
        let script = format!(
            "({})({})",
            include_str!("checkout_browser_refresh.js"),
            js_string(&settings.url)
        );
        // Bound the session lock so a stalled refresh cannot hold up a stock hit
        // for the normal CDP command timeout. Later commands ignore old reply IDs.
        let result = tokio::time::timeout(Duration::from_secs(3), session.evaluate(&script, false))
            .await
            .map_err(|_| "刷新等待超过 3 秒，已释放下单浏览器供结账使用".to_string())?
            .map_err(|_| {
                "浏览器连接已断开或页面正在跳转，请检查下单浏览器；不会自动重新打开窗口".to_string()
            })?;
        result
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "浏览器未返回定时刷新结果".into())
    }

    pub async fn shutdown(&self) {
        self.session.lock().await.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn browser_refresh_never_starts_a_browser_or_blocks_a_checkout_hit() {
        let runner = CheckoutRunner::new();
        let settings = BrowserRefreshSettings::default();
        assert!(
            runner
                .refresh_browser(&settings)
                .await
                .unwrap()
                .contains("请先手动打开")
        );
        assert!(runner.session.lock().await.is_none());
        let slot = runner.session.lock().await;
        assert!(
            !runner.is_busy(),
            "a refresh/session lock must not suppress checkout hits"
        );
        assert!(
            runner
                .refresh_browser(&settings)
                .await
                .unwrap()
                .contains("正在使用")
        );
        drop(slot);
        runner.active_checkout.store(true, Ordering::Release);
        assert!(
            runner
                .refresh_browser(&settings)
                .await
                .unwrap()
                .contains("自动结账正在进行")
        );
        let activity = CheckoutActivity(&runner.active_checkout);
        drop(activity);
        assert!(!runner.is_busy());
    }

    #[test]
    fn 结账地区按目标门店编号从目录读取() {
        let catalog = Catalog::new();
        assert_eq!(
            PickupRegion::for_store(&catalog, "zh_CN", "R577"),
            Some(PickupRegion {
                state: "广东".into(),
                city: "广州".into(),
                address: "广州市天河区天河路 218 号".into(),
            })
        );
        assert_eq!(
            PickupRegion::for_store(&catalog, "zh_CN", "R683"),
            Some(PickupRegion {
                state: "上海".into(),
                city: "上海".into(),
                address: "上海市普陀区中山北路 3300 号".into(),
            })
        );
        assert_eq!(PickupRegion::for_store(&catalog, "zh_CN", "R000"), None);
    }

    fn item(name: &str, quantity: &str) -> LineItem {
        LineItem {
            name: name.into(),
            quantity: quantity.into(),
        }
    }

    #[test]
    fn 购物袋恰好是到货型号时通过核对() {
        let items = [item("iPhone 18 Pro 256GB 冰川蓝色", "1")];
        assert_eq!(verify_items(&items, "iPhone 18 Pro 256GB 冰川蓝色"), Ok(()));
        // 空白和大小写的差异不算不一致。
        assert_eq!(
            verify_items(&items, "iphone 18 Pro  256gb 冰川蓝色"),
            Ok(())
        );
        // 查看订单页可能读不到数量下拉框。
        let unknown_quantity = [item("iPhone 18 Pro 256GB 冰川蓝色", "")];
        assert_eq!(
            verify_items(&unknown_quantity, "iPhone 18 Pro 256GB 冰川蓝色"),
            Ok(())
        );
    }

    #[test]
    fn 型号不一致或数量不是一件时拒绝继续() {
        let other = [item("iPhone 18 Pro Max 256GB 冰川蓝色", "1")];
        assert!(verify_items(&other, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
        let two = [item("iPhone 18 Pro 256GB 冰川蓝色", "2")];
        assert!(verify_items(&two, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
    }

    #[test]
    fn 购物袋为空或有多件商品时拒绝继续() {
        assert!(verify_items(&[], "iPhone 18 Pro 256GB 冰川蓝色").is_err());
        let many = [
            item("iPhone 18 Pro 256GB 冰川蓝色", "1"),
            item("腕带 – 清透蓝色", "1"),
        ];
        assert!(verify_items(&many, "iPhone 18 Pro 256GB 冰川蓝色").is_err());
    }

    #[test]
    fn 门店标签能区分可取货与不可取货() {
        assert!(!store_label_unavailable(
            "2 Apple 静安 明天 可取货 店内取货"
        ));
        assert!(store_label_unavailable(
            "4 Apple 七宝 目前不可取货 店内取货"
        ));
        assert!(store_label_unavailable("4Apple 七宝目前不可取货店内取货"));
    }

    #[test]
    fn 写进页面脚本的文字不会破坏脚本结构() {
        let script = store_state_js("R1\"); alert(1); (\"");
        assert!(script.contains(r#"e.value === "R1\"); alert(1); (\"""#));
        let script = billing_choice_js(&CheckoutPlan {
            store_number: "R678".into(),
            store_title: "Apple 静安".into(),
            product_name: "x".into(),
            pickup_region: None,
            national_id_suffix: "123X".into(),
            payment: CheckoutPayment::Installment,
            installment_bank: "招商'银行".into(),
            installment_periods: 24,
            place_order: false,
            monitored_stores: Vec::new(),
            contact: Vec::new(),
            accept_nearest_store: false,
        });
        assert!(script.contains(r#"const kind = "INSTALLMENT";"#));
        assert!(script.contains(r#"const bank = "招商'银行""#));
    }

    /// 用应用自己的下单资料目录把结账走到「查看订单」页，不会提交订单。
    ///
    /// 运行前要先退出应用（资料目录不能被两个浏览器同时使用），并且已经在
    /// 下单浏览器里登录、购物袋里只有 `APW_LIVE_PRODUCT` 这一件商品。
    #[tokio::test]
    #[ignore = "会用已登录的下单浏览器真实走一遍结账页面"]
    async fn 真实结账能走到查看订单页() {
        let env =
            |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
        let plan = CheckoutPlan {
            store_number: env("APW_LIVE_STORE", "R678"),
            store_title: env("APW_LIVE_STORE_TITLE", "Apple 静安"),
            product_name: std::env::var("APW_LIVE_PRODUCT").expect("请设置 APW_LIVE_PRODUCT"),
            pickup_region: PickupRegion::for_store(
                &Catalog::new(),
                "zh_CN",
                &env("APW_LIVE_STORE", "R678"),
            ),
            national_id_suffix: env("APW_LIVE_ID_SUFFIX", "0000"),
            payment: CheckoutPayment::Installment,
            installment_bank: env("APW_LIVE_BANK", "招商银行"),
            installment_periods: 24,
            place_order: false,
            monitored_stores: Vec::new(),
            contact: [
                ("lastName", "APW_LIVE_LAST_NAME"),
                ("firstName", "APW_LIVE_FIRST_NAME"),
                ("emailAddress", "APW_LIVE_EMAIL"),
                ("fullDaytimePhone", "APW_LIVE_PHONE"),
            ]
            .into_iter()
            .filter_map(|(field, name)| Some((field, std::env::var(name).ok()?)))
            .collect(),
            accept_nearest_store: std::env::var("APW_LIVE_NEAREST").is_ok(),
        };
        let mut session = ChromiumSession::start_visible(app_profile_dir(PROFILE_DIR))
            .await
            .expect("下单浏览器启动失败");
        let started = Instant::now();
        let progress = move |message: String| {
            eprintln!("[{:>5.1}s] {message}", started.elapsed().as_secs_f32())
        };
        let outcome = Flow {
            session: &mut session,
            plan: &plan,
            progress: &progress,
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
        }
        .run()
        .await;
        eprintln!(
            "[{:>5.1}s] 结果：{outcome:?}",
            started.elapsed().as_secs_f32()
        );
        if let Ok(script) = std::env::var("APW_LIVE_DUMP_JS") {
            eprintln!("页面：{:?}", session.evaluate(&script, false).await);
        }
        // 留一点时间让人看清停在哪一页。
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(matches!(outcome, Ok(CheckoutOutcome::ReachedReview { .. })));
    }

    /// 需要本机装有 Chrome 或 Edge，并且会弹出一个浏览器窗口，所以默认不跑。
    #[tokio::test]
    #[ignore = "会打开真实浏览器窗口并访问 Apple 官网"]
    async fn 真实下单浏览器能打开购物袋且不带自动化标记() {
        let profile = tempfile::tempdir().expect("无法创建临时资料目录");
        let mut session = ChromiumSession::start_visible(profile.path().to_path_buf())
            .await
            .expect("下单浏览器启动失败");
        session
            .command("Page.navigate", json!({ "url": BAG_URL }))
            .await
            .expect("购物袋页面导航失败");
        let progress = |_: String| {};
        let plan = CheckoutPlan {
            store_number: "R678".into(),
            store_title: "Apple 静安".into(),
            product_name: "x".into(),
            pickup_region: None,
            national_id_suffix: "0000".into(),
            payment: CheckoutPayment::Alipay,
            installment_bank: String::new(),
            installment_periods: 24,
            place_order: false,
            monitored_stores: Vec::new(),
            contact: Vec::new(),
            accept_nearest_store: std::env::var("APW_LIVE_NEAREST").is_ok(),
        };
        let mut flow = Flow {
            session: &mut session,
            plan: &plan,
            progress: &progress,
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
        };
        let state: PageState = flow
            .wait(
                Stage::Bag,
                PAGE_STATE_JS,
                ELEMENT_TIMEOUT,
                "页面状态",
                |_: &PageState| true,
            )
            .await
            .expect("读取页面状态失败");
        assert!(!state.sign_in);
        // 未登录的全新资料目录里购物袋是空的：脚本应当正常返回空列表而不是报错。
        tokio::time::sleep(Duration::from_secs(4)).await;
        let lines: Lines = flow.read(LINE_ITEMS_JS).await.expect("读取购物袋失败");
        assert!(lines.items.is_empty());
        assert!(!lines.can_place_order);
        let webdriver = flow
            .session
            .evaluate("String(navigator.webdriver)", false)
            .await
            .expect("读取 webdriver 标记失败");
        assert_eq!(webdriver.as_str(), Some("false"));
    }

    #[test]
    fn 错误信息带上停下的页面() {
        let error = CheckoutError::new(Stage::Fulfillment, "门店没有选中");
        assert_eq!(error.to_string(), "在「选择取货门店」停下：门店没有选中");
    }
}
