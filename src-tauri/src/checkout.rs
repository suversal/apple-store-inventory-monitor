//! 到货后自动结账（中国大陆和中国香港直营店取货）。
//!
//! 使用一个带窗口的专属 Chromium 会话。用户先登录，商品可预先放入购物袋，或
//! 开启按到货型号自动加购；自动加购在提交前失败时先移除本次商品并确认空袋。
//! 再依次选择门店、取货信息、付款方式和查看订单。页面元素按 `data-autom` 定位。
//!
//! 这个会话与库存查询的无头会话完全分开：资料目录不同，也不走线路轮换。登录
//! 账户的会话频繁更换出口 IP 容易触发 Apple 的账户风控。
//!
//! 大陆付款由用户完成，香港信用卡提交可能触发支付授权。默认停在「查看订单」页；只有用户明确开启时才点
//! 「立即下单」，并且只点一次，无论结果如何都不重试。

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use apw_core::catalog::Catalog;
use apw_core::config::{CheckoutPayment, Settings, checkout_product_key};
use apw_core::model::Target;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::Mutex;

use crate::checkout_hk::HkPayment;
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
    BagCleanup,
    SignIn,
    SignInLoading,
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
            Stage::BagCleanup => "清理购物袋",
            Stage::SignIn => "登录",
            Stage::SignInLoading => "登录页面加载",
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

    /// 提交按钮可能已经送出请求，此时关闭页面并重试会有重复下单的风险。
    fn can_retry(&self) -> bool {
        !matches!(self.stage, Stage::PlaceOrder | Stage::SignIn)
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
    pub locale: String,
    pub auto_sign_in: bool,
    pub auto_add_to_bag: bool,
    pub part_number: String,
    pub product_url: Option<String>,
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
                (
                    if target.locale == "zh_HK" {
                        "mobilePhone"
                    } else {
                        "fullDaytimePhone"
                    },
                    &settings.contact_phone,
                ),
            ]
            .into_iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(field, value)| (field, value.clone()))
            .collect()
        } else {
            Vec::new()
        };
        Self {
            locale: target.locale.clone(),
            auto_sign_in: target.locale == "zh_CN" || settings.hk_auto_sign_in,
            auto_add_to_bag: settings.auto_add_to_bag,
            part_number: target.part_number.clone(),
            product_url: if target.companion_part.is_none() && target.kit_part.is_none() {
                target.purchase_url(None)
            } else {
                None
            },
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

fn bag_url(locale: &str) -> &'static str {
    if locale == "zh_HK" {
        "https://www.apple.com/hk-zh/shop/bag"
    } else {
        BAG_URL
    }
}

fn pickup_region_js(plan: &CheckoutPlan) -> String {
    format!(
        "JSON.stringify(({})({}))",
        include_str!("checkout_pickup_region.js"),
        json!({
            "locale": plan.locale,
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
    #[serde(default, rename = "partNumber")]
    part_number: String,
    #[serde(default, rename = "removeId")]
    remove_id: String,
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
    [
        "不可取货",
        "无法取货",
        "暂无供应",
        "不可取貨",
        "無法使用",
        "暫無供應",
    ]
    .iter()
    .any(|text| compact.contains(text))
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
const PAGE_ERRORS_JS: &str = concat!(
    "JSON.stringify((",
    include_str!("checkout_page_errors.js"),
    ")())"
);

/// 购物袋页和查看订单页都用同一组 `bag-item-N` 标识列出商品。
const LINE_ITEMS_JS: &str = concat!(
    "JSON.stringify((",
    include_str!("checkout_line_items.js"),
    ")((",
    include_str!("checkout_page_errors.js"),
    ")()))"
);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lines {
    items: Vec<LineItem>,
    total: String,
    can_checkout: bool,
    can_place_order: bool,
    #[serde(default)]
    bag_ready: bool,
    #[serde(default)]
    bag_empty: bool,
    #[serde(default)]
    errors: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AddState {
    ready: bool,
    error: String,
    point: Option<ClickPoint>,
}

#[derive(Debug, Deserialize)]
struct ClickPoint {
    x: f64,
    y: f64,
}

fn bag_action_js(plan: &CheckoutPlan, source: &str, action: &str, remove_id: &str) -> String {
    format!(
        "JSON.stringify(({})({},{}))",
        source,
        json!({"locale": plan.locale, "partNumber": plan.part_number, "productName": plan.product_name, "removeId": remove_id}),
        js_string(action),
    )
}

impl Lines {
    fn verify(&self, stage: Stage, product_name: &str) -> Result<(), CheckoutError> {
        if !self.errors.is_empty() {
            return Err(CheckoutError::new(
                stage,
                format!("页面提示：{}", self.errors.join("；")),
            ));
        }
        verify_items(&self.items, product_name)
            .map_err(|message| CheckoutError::new(stage, message))
    }
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
  const next = document.querySelector('button[data-autom="fulfillment-continue-button"]') || (location.pathname.startsWith('/hk-zh/') ? document.getElementById('rs-checkout-continue-button-bottom') : null);
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
  const field = (name) => document.querySelector('input[data-autom="form-field-' + name + '"]') || document.getElementById('checkout.pickupContact.selfPickupContact.selfContact.address.' + name);
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

fn trusted_checkout_login_url(value: &str, locale: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let (suffix, path) = match locale {
        "zh_CN" => (".www.apple.com.cn", "/shop/"),
        "zh_HK" => (".store.apple.com", "/hk-zh/shop/"),
        _ => return false,
    };
    let secure_number = host
        .strip_prefix("secure")
        .and_then(|s| s.strip_suffix(suffix));
    url.scheme() == "https"
        && url.port().is_none()
        && secure_number.is_some_and(|s| {
            (locale == "zh_HK" || !s.is_empty()) && s.bytes().all(|b| b.is_ascii_digit())
        })
        && (url.path() == format!("{path}signIn")
            || (url.path() == format!("{path}checkout")
                && url
                    .query_pairs()
                    .any(|(k, v)| k == "_s" && v.starts_with("SignIn"))))
}

struct Flow<'a> {
    session: &'a mut ChromiumSession,
    plan: &'a CheckoutPlan,
    progress: &'a (dyn Fn(String) + Send + Sync),
    /// 实际取货的门店。默认是到货的那一家，沿用结账页默认选中的门店时会变。
    store_number: String,
    store_title: String,
    /// 只在初始空袋准备加购，或袋中已恰好为本次型号时接管清理。
    owns_bag: bool,
    hk_payment: Option<HkPayment>,
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
        let selector = if self.plan.locale == "zh_HK"
            && selector == r#"button[data-autom="checkout"]"# {
            format!(
                r#"{selector}, button[id="shoppingCart.actions.checkoutOtherPayments"], button[id="shoppingCart.actions.navCheckout"]"#
            )
        } else if self.plan.locale == "zh_HK"
            && [
                r#"button[data-autom="fulfillment-continue-button"]"#,
                r#"button[data-autom="continue-button-label"]"#,
                r#"button[data-autom="continue-button-review"]"#,
                r#"button[data-autom="continue-button-placeOrder"]"#,
            ]
            .contains(&selector)
        {
            format!("{selector}, button#rs-checkout-continue-button-bottom")
        } else {
            selector.to_owned()
        };
        let script = if self.plan.locale == "zh_HK"
            && selector == r#"button[data-autom="show-more-stores-button"]"# {
            r#"(() => {
  const e = document.querySelector('button[data-autom="show-more-stores-button"]') || [...document.querySelectorAll('button')].find(e => ['顯示所有取貨地點', '顯示更多取貨地點'].includes((e.innerText || '').replace(/\s+/g, '')));
  if (!e || e.disabled) return false;
  e.click();
  return true;
})()"#.to_owned()
        } else {
            click_js(&selector)
        };
        let clicked = self
            .session
            .evaluate(&script, false)
            .await
            .map_err(|e| CheckoutError::new(stage, e.to_string()))?;
        if clicked.as_bool() == Some(true) {
            Ok(())
        } else {
            // 只有脚本明确返回 false 才能确认没有点击。连接报错或没有读到布尔结果
            // 时仍按 PlaceOrder 处理，因为无法知道请求是否已经送出。
            let stage = if stage == Stage::PlaceOrder && clicked.as_bool() == Some(false) {
                Stage::Review
            } else {
                stage
            };
            Err(CheckoutError::new(stage, format!("「{name}」按钮不可点击")))
        }
    }

    /// 等结账流程进入下一页。重新登录成功后继续本次流程。
    async fn wait_for_step(
        &mut self,
        stage: Stage,
        step_prefix: &str,
    ) -> Result<(), CheckoutError> {
        let mut deadline = Instant::now() + NAVIGATION_TIMEOUT;
        let mut signed_in = false;
        loop {
            if let Ok(state) = self.read::<PageState>(PAGE_STATE_JS).await {
                if state.sign_in {
                    if signed_in {
                        return Err(CheckoutError::new(
                            Stage::SignIn,
                            "登录后又返回登录页，请检查账户验证",
                        ));
                    }
                    self.sign_in().await?;
                    signed_in = true;
                    deadline = Instant::now() + NAVIGATION_TIMEOUT;
                    continue;
                }
                let errors: Vec<String> = self.read(PAGE_ERRORS_JS).await.unwrap_or_default();
                if !errors.is_empty() {
                    return Err(CheckoutError::new(
                        stage,
                        format!("页面提示：{}", errors.join("；")),
                    ));
                }
                if state.step.starts_with(step_prefix) {
                    #[cfg(test)]
                    eprintln!(
                        "    进入 {step_prefix}：{:.1}s",
                        (NAVIGATION_TIMEOUT - (deadline - Instant::now())).as_secs_f32()
                    );
                    return Ok(());
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

    async fn sign_in(&mut self) -> Result<(), CheckoutError> {
        if self.plan.locale == "zh_HK" {
            return Err(CheckoutError::new(
                Stage::SignIn,
                "香港结账仅使用手动登录。请在当前下单浏览器完成 Apple 登录和验证，并停在取货方式页；后续到货将自动继续",
            ));
        }
        let fail = |message| CheckoutError::new(Stage::SignIn, message);
        let login = crate::checkout_login::load()
            .map_err(fail)?
            .ok_or_else(|| {
                fail(
                    "Apple 要求登录，请在自动结账设置中保存登录信息，或在下单浏览器中手动登录"
                        .into(),
                )
            })?;
        self.report("自动结账：Apple 要求重新登录，正在使用软件配置中的账户登录");
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut account_sent = false;
        let mut password_sent = false;
        while Instant::now() < deadline {
            let state = self.read::<PageState>(PAGE_STATE_JS).await;
            if state
                .as_ref()
                .is_ok_and(|s| !s.sign_in && s.step.starts_with("Fulfillment"))
            {
                self.report("自动结账：Apple 登录成功，继续本次结账");
                return Ok(());
            }
            if let Some(context) = self.login_context().await? {
                let status = self
                    .login_action(context, &login.account, "", "inspect")
                    .await?;
                let action = match status.as_str() {
                    "account" if !account_sent && !password_sent => Some("account"),
                    "password" if !password_sent => Some("password"),
                    "verification" => {
                        return Err(fail(
                            "Apple 要求验证码或二次验证，已保留页面，请手动完成验证".into(),
                        ));
                    }
                    "error" => {
                        return Err(fail(
                            "Apple 登录未通过，已保留页面，请检查账户或密码；不会重复尝试密码"
                                .into(),
                        ));
                    }
                    "accountMismatch" => {
                        return Err(fail(
                            "登录页账户与配置账户不一致，未填写密码，请手动确认".into(),
                        ));
                    }
                    "untrusted" => return Err(fail("登录页面来源不符合预期，未填写密码".into())),
                    _ => None,
                };
                if let Some(action) = action {
                    let status = self
                        .login_action(
                            context,
                            &login.account,
                            if action == "password" {
                                &login.password
                            } else {
                                ""
                            },
                            action,
                        )
                        .await?;
                    account_sent |= status == "accountSubmitted";
                    password_sent |= status == "passwordSubmitted";
                }
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Err(CheckoutError::new(
            Stage::SignInLoading,
            "等待 Apple 登录完成超时，将清理购物袋后等待下次到货重试",
        ))
    }

    async fn login_context(&mut self) -> Result<Option<u64>, CheckoutError> {
        let fail = || CheckoutError::new(Stage::SignInLoading, "暂时无法读取 Apple 登录页面");
        let tree = self
            .session
            .command("Page.getFrameTree", json!({}))
            .await
            .map_err(|_| fail())?;
        let Some(tree) = tree.get("frameTree") else {
            return Ok(None);
        };
        let top = tree
            .pointer("/frame/url")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !trusted_checkout_login_url(top, &self.plan.locale) {
            return Ok(None);
        }
        let frame = tree
            .get("childFrames")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .find(|child| {
                child
                    .pointer("/frame/securityOrigin")
                    .and_then(|v| v.as_str())
                    == Some(if self.plan.locale == "zh_HK" {
                        "https://idmsa.apple.com"
                    } else {
                        "https://idmsa.apple.com.cn"
                    })
            });
        let Some(frame_id) = frame
            .and_then(|f| f.pointer("/frame/id"))
            .and_then(|v| v.as_str())
        else {
            return Ok(None);
        };
        let world = self
            .session
            .command(
                "Page.createIsolatedWorld",
                json!({"frameId":frame_id,"worldName":"apw-checkout-login"}),
            )
            .await
            .map_err(|_| fail())?;
        Ok(world.get("executionContextId").and_then(|v| v.as_u64()))
    }

    async fn login_action(
        &mut self,
        context: u64,
        account: &str,
        password: &str,
        action: &str,
    ) -> Result<String, CheckoutError> {
        let result = self.session.command("Runtime.callFunctionOn", json!({
            "executionContextId":context,
            "functionDeclaration":format!("function(expected, action) {{ return ({})(expected, action); }}", include_str!("checkout_sign_in.js")),
            "arguments":[{"value":{"account":account,"password":password,"locale":self.plan.locale}},{"value":action}],
            "returnByValue":true,
        })).await.map_err(|_| CheckoutError::new(Stage::SignInLoading, "登录页面发生变化，将重新确认页面状态"))?;
        if result.get("exceptionDetails").is_some() {
            return Err(CheckoutError::new(
                Stage::SignInLoading,
                "登录控件暂时无法操作",
            ));
        }
        Ok(result
            .pointer("/result/value")
            .and_then(|v| v.as_str())
            .unwrap_or("waiting")
            .to_owned())
    }

    async fn navigate(&mut self, url: &str, stage: Stage) -> Result<(), CheckoutError> {
        if self.plan.auto_add_to_bag {
            self.session
                .evaluate("window.__apwCheckoutNavigating = true", false)
                .await
                .map_err(|e| CheckoutError::new(stage, e.to_string()))?;
        }
        let navigation = self
            .session
            .command("Page.navigate", json!({ "url": url }))
            .await
            .map_err(|e| CheckoutError::new(stage, e.to_string()))?;
        if let Some(error) = navigation
            .get("errorText")
            .and_then(serde_json::Value::as_str)
            .filter(|error| !error.trim().is_empty())
        {
            return Err(CheckoutError::new(stage, format!("页面打不开：{error}")));
        }
        if self.plan.auto_add_to_bag {
            // 同一购物袋 URL 的刷新也必须等到新文档，不能把旧页面的空袋动画
            // 当成服务器已经移除商品的证据。
            self.wait(
                stage,
                "JSON.stringify(!window.__apwCheckoutNavigating)",
                NAVIGATION_TIMEOUT,
                "新页面加载",
                |ready: &bool| *ready,
            )
            .await?;
        }
        Ok(())
    }

    fn verify_auto_bag(&self, lines: &Lines) -> Result<(), CheckoutError> {
        verify_items(&lines.items, &self.plan.product_name)
            .map_err(|message| CheckoutError::new(Stage::Bag, message))?;
        let item = &lines.items[0];
        if item.quantity != "1"
            || !item
                .part_number
                .eq_ignore_ascii_case(&self.plan.part_number)
        {
            return Err(CheckoutError::new(
                Stage::Bag,
                "购物袋的零件号或数量与本次到货型号不一致",
            ));
        }
        Ok(())
    }

    async fn read_bag(&mut self, stage: Stage) -> Result<Lines, CheckoutError> {
        self.navigate(bag_url(&self.plan.locale), stage).await?;
        self.wait(
            stage,
            LINE_ITEMS_JS,
            ELEMENT_TIMEOUT,
            "购物袋加载完成",
            |s: &Lines| s.bag_ready,
        )
        .await
    }

    async fn prepare_bag(&mut self) -> Result<(), CheckoutError> {
        if self.plan.product_url.is_none() {
            return Err(CheckoutError::new(
                Stage::Bag,
                "此型号需要额外配置，暂不支持自动加购，请使用预先准备购物袋的方式",
            ));
        }
        self.report("自动加购：检查购物袋");
        let initial = self.read_bag(Stage::Bag).await?;
        if !initial.items.is_empty() {
            // 用户本来就放了同型号的一件时可直接使用；从不叠加数量或清除其他型号。
            self.verify_auto_bag(&initial).map_err(|error| {
                CheckoutError::new(
                    Stage::BagCleanup,
                    format!(
                        "购物袋已有其他商品或数量不符，未加购也未删除：{}。请先整理专属购物袋",
                        error.message
                    ),
                )
            })?;
            self.owns_bag = true;
            self.report("自动加购：购物袋已恰好为本次到货型号，沿用 1 件");
            return Ok(());
        }
        if !initial.bag_empty {
            return Err(CheckoutError::new(
                Stage::Bag,
                "尚未确认购物袋为空，未执行加购",
            ));
        }
        self.report(format!("自动加购：打开 {} 商品页", self.plan.product_name));
        let url = self
            .plan
            .product_url
            .as_deref()
            .unwrap_or(bag_url(&self.plan.locale))
            .to_owned();
        self.navigate(&url, Stage::Bag).await?;
        let prepare = bag_action_js(
            self.plan,
            include_str!("checkout_add_to_bag.js"),
            "prepare",
            "",
        );
        let add = bag_action_js(self.plan, include_str!("checkout_add_to_bag.js"), "add", "");
        let deadline = Instant::now() + ELEMENT_TIMEOUT;
        loop {
            let ready: AddState = self
                .wait(
                    Stage::Bag,
                    &prepare,
                    deadline.saturating_duration_since(Instant::now()),
                    "到货型号的加购按钮",
                    |s: &AddState| s.ready || !s.error.is_empty(),
                )
                .await?;
            if !ready.error.is_empty() {
                return Err(CheckoutError::new(Stage::Bag, ready.error));
            }
            // 从真正点击前接管清理。请求出错或结果丢失立即退出，绝不重发加购。
            self.owns_bag = true;
            let added: AddState = self
                .read(&add)
                .await
                .map_err(|e| CheckoutError::new(Stage::Bag, e))?;
            if let Some(point) = added.point {
                #[cfg(test)]
                if std::env::var_os("APW_DIAG_AUTO_BAG").is_some() {
                    eprintln!("加购按钮诊断：{:?}", self.session.evaluate(r#"JSON.stringify((()=>{const b=document.querySelector('button[data-autom="add-to-cart"]');const f=b?.form;return {url:location.href, ready:document.readyState, button:b?.outerHTML, form:f?{action:f.getAttribute('action'),method:f.method,products:[...f.querySelectorAll('input[name="product"]')].map(e=>e.value)}:null};})())"#, false).await);
                }
                for kind in ["mousePressed", "mouseReleased"] {
                    self.session.command("Input.dispatchMouseEvent", json!({
                        "type": kind, "x": point.x, "y": point.y, "button": "left", "clickCount": 1,
                    })).await.map_err(|e| CheckoutError::new(Stage::Bag, e.to_string()))?;
                }
                break;
            }
            if !added.error.is_empty() || Instant::now() >= deadline {
                return Err(CheckoutError::new(
                    Stage::Bag,
                    if added.error.is_empty() {
                        "加购按钮不可点击".into()
                    } else {
                        added.error
                    },
                ));
            }
            // 仅脚本明确返回未点击时才重新等准备状态。React 的异步重绘会短暂禁用按钮。
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        // Apple 会先跳转配件推荐页；等加购完成后直接进购物袋，跳过全部推荐配件。
        let added: String = self.wait(Stage::Bag, r#"JSON.stringify(['/shop/bag', '/hk-zh/shop/bag'].includes(location.pathname) || new URLSearchParams(location.search).get('step') === 'attach' ? 'complete' : /Page Not Found/i.test(document.title) ? 'failed' : 'waiting')"#,
            NAVIGATION_TIMEOUT, "加购完成", |state: &String| state != "waiting").await?;
        if added == "failed" {
            return Err(CheckoutError::new(Stage::Bag, "Apple 加购页面返回 404"));
        }
        let lines = self.read_bag(Stage::Bag).await?;
        self.verify_auto_bag(&lines)?;
        self.report(format!(
            "自动加购：已确认 {} 1 件，购物袋无其他商品",
            self.plan.product_name
        ));
        Ok(())
    }

    async fn cleanup_bag(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::BagCleanup;
        self.report("自动加购：本次结账失败，清理本次购物袋");
        let mut lines = self.read_bag(stage).await?;
        let deadline = Instant::now() + NAVIGATION_TIMEOUT;
        let mut changed = false;
        loop {
            if let Some(item) = lines.items.first() {
                if item.remove_id.is_empty() || Instant::now() >= deadline {
                    return Err(CheckoutError::new(
                        stage,
                        "购物袋条目尚不可移除，下次先继续清理",
                    ));
                }
                let previous_count = lines.items.len();
                let item = item.clone();
                let action = |action: &str| {
                    format!(
                        "JSON.stringify(({})({},{}))",
                        include_str!("checkout_remove_from_bag.js"),
                        json!({"locale": self.plan.locale, "partNumber": item.part_number, "productName": item.name,
                        "quantity": item.quantity, "removeId": item.remove_id}),
                        js_string(action),
                    )
                };
                self.wait(
                    stage,
                    &action("ready"),
                    deadline.saturating_duration_since(Instant::now()),
                    "购物袋条目可移除",
                    |ready: &bool| *ready,
                )
                .await?;
                self.report(format!(
                    "自动加购：移除失败购物袋中的「{}」（数量 {}）",
                    item.name, item.quantity
                ));
                let removed: bool = self
                    .read(&action("remove"))
                    .await
                    .map_err(|e| CheckoutError::new(stage, e))?;
                if !removed {
                    return Err(CheckoutError::new(
                        stage,
                        "购物袋条目发生变化，下次重新读取后继续清理",
                    ));
                }
                // Apple 先隐藏删除按钮，再移除整行，必须等条目数减少。
                lines = self
                    .wait(
                        stage,
                        LINE_ITEMS_JS,
                        deadline.saturating_duration_since(Instant::now()),
                        "购物袋条目移除完成",
                        |s: &Lines| s.bag_ready && s.items.len() < previous_count,
                    )
                    .await?;
                changed = true;
            } else if changed {
                lines = self.read_bag(stage).await?;
                changed = false;
            } else if lines.bag_empty {
                break;
            } else {
                return Err(CheckoutError::new(
                    stage,
                    "未能确认购物袋为空，下次先继续清理",
                ));
            }
        }
        self.report("自动加购：本次商品已移除，已确认购物袋为空");
        Ok(())
    }

    async fn bag(&mut self) -> Result<(), CheckoutError> {
        if self.plan.auto_add_to_bag {
            self.prepare_bag().await?;
        }
        self.report("自动结账：打开购物袋");
        self.navigate(bag_url(&self.plan.locale), Stage::Bag)
            .await?;
        let lines: Lines = self
            .wait(
                Stage::Bag,
                LINE_ITEMS_JS,
                ELEMENT_TIMEOUT,
                "购物袋出现可结账的商品",
                |lines: &Lines| {
                    !lines.errors.is_empty() || (lines.can_checkout && !lines.items.is_empty())
                },
            )
            .await
            .map_err(|_| {
                CheckoutError::new(
                    Stage::Bag,
                    "购物袋里没有可结账的商品，请先在下单浏览器里登录并把商品加入购物袋",
                )
            })?;
        lines.verify(Stage::Bag, &self.plan.product_name)?;
        if self.plan.auto_add_to_bag {
            self.verify_auto_bag(&lines)?;
        }
        self.click(Stage::Bag, r#"button[data-autom="checkout"]"#, "结账")
            .await?;
        self.wait_for_step(Stage::Bag, "Fulfillment").await
    }

    async fn fulfillment(&mut self) -> Result<(), CheckoutError> {
        let stage = Stage::Fulfillment;
        self.report(format!("自动结账：选择 {} 取货", self.plan.store_title));

        // 结账默认停在「为我送货」，取货门店列表要切到「我要取货」后才加载。
        const PICKUP_TAB_JS: &str = r#"(() => {
  const tab = [...document.querySelectorAll('button')].find((e) => ['我要取货', '我會前來取貨'].includes((e.innerText || '').trim()));
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
        let contact_script = if self.plan.locale == "zh_HK" {
            CONTACT_STATE_JS.replace("fullDaytimePhone", "mobilePhone")
        } else {
            CONTACT_STATE_JS.to_owned()
        };
        self.report("自动结账：填写取货信息");
        let state: ContactState = self
            .wait(
                stage,
                &contact_script,
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
    const input = document.querySelector('input[data-autom="form-field-' + name + '"]') || document.getElementById('checkout.pickupContact.selfPickupContact.selfContact.address.' + name);
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
            self.read(&contact_script)
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
            if self.plan.locale != "zh_CN" {
                return Err(CheckoutError::new(
                    stage,
                    "香港取货页出现额外证件要求，请在下单浏览器里手动确认",
                ));
            }
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
        if self.plan.locale == "zh_HK" {
            let payment = self.hk_payment.as_ref().ok_or_else(|| {
                CheckoutError::new(stage, "请先在设置里填写本次运行的香港信用卡资料")
            })?;
            let script = payment.script();
            let state: PickupRegionState = self
                .wait(
                    stage,
                    &script,
                    ELEMENT_TIMEOUT,
                    "香港信用卡和账单地址填写完成",
                    |s: &PickupRegionState| s.ready || s.error.is_some(),
                )
                .await?;
            if let Some(error) = state.error {
                return Err(CheckoutError::new(stage, error));
            }
            self.click(
                stage,
                r#"button[data-autom="continue-button-review"]"#,
                "检查订单",
            )
            .await?;
            return self.wait_for_step(stage, "Review").await;
        }
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
                |lines: &Lines| {
                    !lines.errors.is_empty() || (lines.can_place_order && !lines.items.is_empty())
                },
            )
            .await?;
        lines.verify(stage, &self.plan.product_name)?;
        if self.plan.locale == "zh_HK" && lines.items.iter().any(|item| item.quantity.trim() != "1")
        {
            return Err(CheckoutError::new(
                stage,
                "无法确认香港订单的商品数量为 1，未提交订单",
            ));
        }
        if self.plan.auto_add_to_bag
            && lines.items.iter().any(|item| {
                !item.part_number.is_empty()
                    && !item
                        .part_number
                        .eq_ignore_ascii_case(&self.plan.part_number)
            })
        {
            return Err(CheckoutError::new(
                stage,
                "查看订单页的零件号与本次到货型号不一致",
            ));
        }
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
    reserved: Arc<AtomicBool>,
    /// 清理结果未确认前，下一次必须先清理，不能沿用失败商品或叠加加购。
    cleanup_pending: Arc<AtomicBool>,
    /// 账户验证失败后只观察保留页面，不跨轮次重复提交密码。
    login_blocked: Arc<AtomicBool>,
    active_locale: Arc<Mutex<String>>,
    hk_payment: Arc<Mutex<Option<HkPayment>>>,
}

/// 从到货事件分派直到结果写回设置都占住名额，避免多型号同时到货时排队下两单。
pub struct CheckoutReservation(Arc<AtomicBool>);

impl Drop for CheckoutReservation {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl CheckoutRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn has_hk_payment(&self) -> bool {
        self.hk_payment.lock().await.is_some()
    }

    pub async fn set_hk_payment(&self, payment: Option<HkPayment>) -> Result<(), String> {
        if self.is_busy() {
            return Err("结账正在进行，请结束后再更新付款资料".into());
        }
        let payment = payment.map(HkPayment::validate).transpose()?;
        *self.hk_payment.lock().await = payment;
        Ok(())
    }

    pub fn try_reserve(&self) -> Option<CheckoutReservation> {
        if self.is_busy() {
            return None;
        }
        let _session = self.session.try_lock().ok()?;
        self.reserved
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(CheckoutReservation(self.reserved.clone()))
    }

    /// 确保下单浏览器开着并且连接可用；用户关掉窗口后会重新启动。
    async fn ensure<'a>(
        &self,
        slot: &'a mut Option<ChromiumSession>,
        locale: &str,
    ) -> Result<&'a mut ChromiumSession, CheckoutError> {
        if !matches!(locale, "zh_CN" | "zh_HK") {
            return Err(CheckoutError::new(Stage::Browser, "此地区暂不支持自动结账"));
        }
        let mut active_locale = self.active_locale.lock().await;
        if !active_locale.is_empty() && *active_locale != locale {
            if self.cleanup_pending.load(Ordering::Acquire)
                || self.login_blocked.load(Ordering::Acquire)
            {
                return Err(CheckoutError::new(
                    if self.login_blocked.load(Ordering::Acquire) {
                        Stage::SignIn
                    } else {
                        Stage::BagCleanup
                    },
                    "请先切回原地区完成登录验证或购物袋清理，再切换结账地区",
                ));
            }
            if let Some(mut session) = slot.take() {
                session.close().await;
            }
        }
        *active_locale = locale.to_owned();
        let alive = match slot.as_mut() {
            Some(session) => session.evaluate("1", false).await.is_ok(),
            None => false,
        };
        if !alive {
            // 先释放旧会话，否则新进程会因为资料目录被占用而启动失败。
            *slot = None;
            let session = ChromiumSession::start_visible(app_profile_dir(if locale == "zh_HK" {
                "checkout-profile-hk-v1"
            } else {
                PROFILE_DIR
            }))
            .await
            .map_err(|e| CheckoutError::new(Stage::Browser, e.to_string()))?;
            *slot = Some(session);
        }
        slot.as_mut()
            .ok_or_else(|| CheckoutError::new(Stage::Browser, "下单浏览器没有启动"))
    }

    /// 打开下单浏览器并显示购物袋，供用户登录、加购和预热结账。
    pub async fn open(&self, locale: &str) -> Result<(), CheckoutError> {
        let mut slot = self.session.lock().await;
        let session = self.ensure(&mut slot, locale).await?;
        let browser_error =
            |e: apw_core::apple::ApiError| CheckoutError::new(Stage::Browser, e.to_string());
        // 等待验证码时保留当前页面，打开按钮只把它带到前台。
        if !self.login_blocked.load(Ordering::Acquire) {
            session
                .command("Page.navigate", json!({ "url": bag_url(locale) }))
                .await
                .map_err(browser_error)?;
        }
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
        self.run_inner(plan, progress, false).await
    }

    /// 到货后的自动结账：提交前失败就关闭专属窗口，保留资料目录供下一轮重开。
    /// 试跑仍使用 `run`，留下页面供用户排查。
    pub async fn run_on_hit(
        &self,
        plan: &CheckoutPlan,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<CheckoutOutcome, CheckoutError> {
        self.run_inner(plan, progress, true).await
    }

    async fn run_inner(
        &self,
        plan: &CheckoutPlan,
        progress: &(dyn Fn(String) + Send + Sync),
        close_on_failure: bool,
    ) -> Result<CheckoutOutcome, CheckoutError> {
        if !close_on_failure && self.reserved.load(Ordering::Acquire) {
            return Err(CheckoutError::new(
                Stage::Browser,
                "已有一次自动结账正在进行",
            ));
        }
        let Ok(mut slot) = self.session.try_lock() else {
            return Err(CheckoutError::new(
                Stage::Browser,
                "已有一次结账正在进行，本次到货没有重复触发",
            ));
        };
        let result = async {
            let session = self.ensure(&mut slot, &plan.locale).await?;
            let _ = session.command("Page.bringToFront", json!({})).await;
            let hk_payment = if plan.locale == "zh_HK" {
                Some(self.hk_payment.lock().await.clone().ok_or_else(|| {
                    CheckoutError::new(
                        Stage::Billing,
                        "请先填写本次运行的香港信用卡资料；应用重启后需重新填写",
                    )
                })?)
            } else {
                None
            };
            let mut flow = Flow {
                session,
                plan,
                progress,
                store_number: plan.store_number.clone(),
                store_title: plan.store_title.clone(),
                owns_bag: false,
                hk_payment,
            };
            if self.login_blocked.load(Ordering::Acquire) {
                let resumed = flow
                    .read::<PageState>(PAGE_STATE_JS)
                    .await
                    .is_ok_and(|state| !state.sign_in && state.step.starts_with("Fulfillment"));
                if !resumed {
                    return Err(CheckoutError::new(
                        Stage::SignIn,
                        "正在等待当前页面完成 Apple 登录或验证；自动结账保持开启，不重复提交密码",
                    ));
                }
                self.login_blocked.store(false, Ordering::Release);
                progress("自动结账：已确认登录恢复，继续清理和重试".into());
            }
            if self.cleanup_pending.load(Ordering::Acquire) {
                progress("自动加购：上次清理尚未完成，先确认空袋再处理本次到货".into());
                flow.cleanup_bag().await?;
                self.cleanup_pending.store(false, Ordering::Release);
            }
            let result = flow.run().await;
            if close_on_failure && let Err(original) = &result {
                if original.stage == Stage::SignIn {
                    self.login_blocked.store(true, Ordering::Release);
                }
                if plan.auto_add_to_bag && original.stage != Stage::PlaceOrder {
                    self.cleanup_pending.store(true, Ordering::Release);
                }
                if (plan.auto_add_to_bag || flow.owns_bag) && original.can_retry() {
                    if let Err(cleanup) = flow.cleanup_bag().await {
                        return Err(CheckoutError::new(
                            Stage::BagCleanup,
                            format!(
                                "{original}；清理未完成：{}；自动结账保持开启，下次先继续清理",
                                cleanup.message
                            ),
                        ));
                    }
                    self.cleanup_pending.store(false, Ordering::Release);
                }
            }
            result
        }
        .await;
        if close_on_failure
            && result.as_ref().is_err_and(|error| error.can_retry())
            && let Some(mut session) = slot.take()
        {
            // 清理完成前一直持有锁，避免下一次结账与旧浏览器争用资料目录。
            session.close().await;
            drop(session);
            progress("自动结账：已关闭本次下单页面，等待下一次有货".into());
        }
        result
    }

    /// 是否有一次结账正在进行。
    pub fn is_busy(&self) -> bool {
        self.reserved.load(Ordering::Acquire) || self.session.try_lock().is_err()
    }

    pub async fn shutdown(&self) {
        self.session.lock().await.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 登录只允许官方安全结账入口() {
        assert!(trusted_checkout_login_url(
            "https://secure6.www.apple.com.cn/shop/signIn?ssi=test",
            "zh_CN"
        ));
        assert!(trusted_checkout_login_url(
            "https://secure12.www.apple.com.cn/shop/checkout?_s=SignIn",
            "zh_CN"
        ));
        for url in [
            "http://secure6.www.apple.com.cn/shop/signIn",
            "https://secure6.www.apple.com.cn.example.test/shop/signIn",
            "https://secure.www.apple.com.cn/shop/signIn",
            "https://secure6.www.apple.com.cn/shop/checkout?_s=Review",
            "https://example.test/shop/signIn",
        ] {
            assert!(!trusted_checkout_login_url(url, "zh_CN"));
        }
    }

    fn test_plan(place_order: bool) -> CheckoutPlan {
        CheckoutPlan {
            locale: "zh_CN".into(),
            auto_sign_in: true,
            auto_add_to_bag: false,
            part_number: "MK2Q4CH/A".into(),
            product_url: Some("https://www.apple.com.cn/shop/product/MK2Q4CH/A".into()),
            store_number: "R678".into(),
            store_title: "Apple 静安".into(),
            product_name: "要买的商品".into(),
            pickup_region: None,
            national_id_suffix: "0000".into(),
            payment: CheckoutPayment::Alipay,
            installment_bank: String::new(),
            installment_periods: 24,
            place_order,
            monitored_stores: Vec::new(),
            accept_nearest_store: false,
            contact: Vec::new(),
        }
    }

    fn cdp_page(value: serde_json::Value) -> serde_json::Value {
        json!({"result": {"result": {"value": value.to_string()}}})
    }

    async fn review_result(
        responses: Vec<serde_json::Value>,
        place_order: bool,
    ) -> (
        Result<CheckoutOutcome, CheckoutError>,
        Vec<serde_json::Value>,
    ) {
        let (mut session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(responses).await;
        let plan = test_plan(place_order);
        let result = Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        }
        .review()
        .await;
        drop(session);
        assert!(peer.await.unwrap());
        let requests = requests.lock().await.clone();
        (result, requests)
    }

    #[tokio::test]
    async fn 核对页出现售罄提示不会当作结账完成或继续提交() {
        for place_order in [false, true] {
            let (result, requests) = review_result(
                vec![cdp_page(json!({
                    "items": [{"name": "要买的商品", "quantity": "1"}],
                    "total": "100",
                    "canCheckout": false,
                    "canPlaceOrder": true,
                    "errors": ["商品已售罄，请稍后重试"],
                }))],
                place_order,
            )
            .await;
            let error = result.unwrap_err();
            assert_eq!(error.stage, Stage::Review);
            assert!(error.can_retry());
            assert!(error.message.contains("售罄"));
            assert_eq!(requests.len(), 1, "只读取核对页，不能点击提交按钮");
        }
    }

    #[tokio::test]
    async fn 明确未点击提交可以重试而提交连接报错必须人工确认() {
        for (click_response, expected_stage) in [
            (
                json!({"result": {"result": {"value": false}}}),
                Stage::Review,
            ),
            (
                json!({"error": {"message": "connection lost"}}),
                Stage::PlaceOrder,
            ),
            (json!({"result": {"result": {}}}), Stage::PlaceOrder),
        ] {
            let (result, requests) = review_result(
                vec![
                    cdp_page(json!({
                        "items": [{"name": "要买的商品", "quantity": "1"}],
                        "total": "100", "canCheckout": false,
                        "canPlaceOrder": true, "errors": [],
                    })),
                    click_response,
                ],
                true,
            )
            .await;
            let error = result.unwrap_err();
            assert_eq!(error.stage, expected_stage);
            assert_eq!(error.can_retry(), expected_stage == Stage::Review);
            assert_eq!(requests.len(), 2, "提交按钮只尝试点击一次");
        }
    }

    #[tokio::test]
    async fn 进入门店页同时出现售罄错误不能当作成功前进() {
        let (mut session, peer, _) = crate::chromium_fetcher::tests::checkout_session(vec![
            cdp_page(json!({"step": "Fulfillment", "signIn": false})),
            cdp_page(json!(["商品已售罄"])),
        ])
        .await;
        let plan = test_plan(true);
        let error = Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        }
        .wait_for_step(Stage::Bag, "Fulfillment")
        .await
        .unwrap_err();
        assert!(error.can_retry());
        assert!(error.message.contains("售罄"));
        drop(session);
        assert!(peer.await.unwrap());
    }

    #[test]
    fn 提交订单结果不明时不能关闭页面重试() {
        for stage in [
            Stage::Browser,
            Stage::Bag,
            Stage::BagCleanup,
            Stage::SignInLoading,
            Stage::Fulfillment,
            Stage::PickupContact,
            Stage::Billing,
            Stage::Review,
        ] {
            assert!(CheckoutError::new(stage, "test").can_retry());
        }
        assert!(!CheckoutError::new(Stage::PlaceOrder, "test").can_retry());
        assert!(!CheckoutError::new(Stage::SignIn, "test").can_retry());
    }

    #[tokio::test]
    async fn 自动结账失败会关闭窗口而试跑保留失败现场() {
        for (automatic, sold_out) in [(true, false), (false, false), (true, true), (false, true)] {
            let mut responses = vec![
                json!({"result": {"result": {"value": 1}}}),
                json!({"result": {}}),
                json!({"result": {"frameId": "bag"}}),
                json!({"result": {"result": {"value": json!({
                    "items": if sold_out { json!([]) } else { json!([{"name": "另一件商品", "quantity": "1"}]) },
                    "total": "100",
                    "canCheckout": true,
                    "canPlaceOrder": false,
                    "errors": if sold_out { vec!["商品已售罄"] } else { Vec::<&str>::new() },
                }).to_string()}}}),
            ];
            if automatic {
                responses.push(json!({"result": {}}));
            }
            let (session, peer, requests) =
                crate::chromium_fetcher::tests::checkout_session(responses).await;
            let runner = CheckoutRunner {
                session: Arc::new(Mutex::new(Some(session))),
                ..Default::default()
            };
            let plan = CheckoutPlan {
                locale: "zh_CN".into(),
                auto_sign_in: true,
                auto_add_to_bag: false,
                part_number: "MK2Q4CH/A".into(),
                product_url: Some("https://www.apple.com.cn/shop/product/MK2Q4CH/A".into()),
                store_number: "R678".into(),
                store_title: "Apple 静安".into(),
                product_name: "要买的商品".into(),
                pickup_region: None,
                national_id_suffix: "0000".into(),
                payment: CheckoutPayment::Alipay,
                installment_bank: String::new(),
                installment_periods: 24,
                place_order: true,
                monitored_stores: Vec::new(),
                accept_nearest_store: false,
                contact: Vec::new(),
            };
            let result = if automatic {
                runner.run_on_hit(&plan, &|_| {}).await
            } else {
                runner.run(&plan, &|_| {}).await
            };
            let error = result.unwrap_err();
            assert_eq!(error.stage, Stage::Bag);
            if sold_out {
                assert!(error.message.contains("售罄"));
            }
            assert_eq!(peer.await.unwrap(), automatic, "CDP 连接是否已关闭");
            assert_eq!(runner.session.lock().await.is_none(), automatic);
            let requests = requests.lock().await;
            assert_eq!(requests[2]["params"]["url"], BAG_URL);
            assert_eq!(
                requests.iter().any(|r| r["method"] == "Browser.close"),
                automatic,
            );
            assert!(!runner.is_busy());
            runner.shutdown().await;
        }
    }

    #[tokio::test]
    async fn 自动加购失败先移除并刷新确认空袋再关闭窗口() {
        for removed in [true, false] {
            let mut plan = test_plan(true);
            plan.auto_add_to_bag = true;
            let empty = cdp_page(json!({
                "items": [], "total": "", "canCheckout": false, "canPlaceOrder": false,
                "bagReady": true, "bagEmpty": true, "errors": [],
            }));
            let full = cdp_page(json!({
                "items": [{"name": plan.product_name, "quantity": "1", "partNumber": plan.part_number, "removeId": "owned-item.delete"}],
                "total": "100", "canCheckout": true, "canPlaceOrder": false,
                "bagReady": true, "bagEmpty": false, "errors": [],
            }));
            let mut sold_out = full.clone();
            let mut data: serde_json::Value =
                serde_json::from_str(sold_out["result"]["result"]["value"].as_str().unwrap())
                    .unwrap();
            data["errors"] = json!(["商品已售罄"]);
            sold_out["result"]["result"]["value"] = json!(data.to_string());
            let navigate = json!({"result": {"frameId": "page"}});
            let mut responses = vec![
                json!({"result": {"result": {"value": 1}}}), // ensure
                json!({"result": {}}),                       // bring to front
                navigate.clone(),
                empty.clone(), // 加购前确认空袋
                navigate.clone(),
                cdp_page(json!({"ready": true, "clicked": false, "error": ""})),
                cdp_page(json!({"ready": true, "point": {"x": 10, "y": 20}, "error": ""})),
                json!({"result": {}}),
                json!({"result": {}}),       // 真实鼠标按下、释放各一次
                cdp_page(json!("complete")), // attach 已完成
                navigate.clone(),
                full.clone(), // 加购后的 SKU/数量核对
                navigate.clone(),
                sold_out, // 准备结账时库存已消失
                navigate.clone(),
                full,
                cdp_page(json!(true)),    // 只读等待条目可删除
                cdp_page(json!(removed)), // 清理前再次核对，移除只点一次
            ];
            if removed {
                responses.extend([empty.clone(), navigate, empty, json!({"result": {}})]);
            } else {
                responses.push(json!({"result": {}})); // 清理失败也关闭本次窗口，下轮先继续清理
            }
            let responses = responses
                .into_iter()
                .flat_map(|response| {
                    if response["result"]["frameId"].is_string() {
                        vec![
                            json!({"result": {"result": {"value": true}}}),
                            response,
                            cdp_page(json!(true)),
                        ]
                    } else {
                        vec![response]
                    }
                })
                .collect();
            let (session, peer, requests) =
                crate::chromium_fetcher::tests::checkout_session(responses).await;
            let runner = CheckoutRunner {
                session: Arc::new(Mutex::new(Some(session))),
                ..Default::default()
            };
            let error = runner.run_on_hit(&plan, &|_| {}).await.unwrap_err();
            assert_eq!(
                error.stage,
                if removed {
                    Stage::Bag
                } else {
                    Stage::BagCleanup
                }
            );
            assert!(error.can_retry());
            assert!(peer.await.unwrap());
            assert_eq!(runner.cleanup_pending.load(Ordering::Acquire), !removed);
            let requests = requests.lock().await;
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r["method"] == "Browser.close")
                    .count(),
                1
            );
            assert!(requests.iter().any(|r| {
                r["method"] == "Page.navigate"
                    && r["params"]["url"]
                        .as_str()
                        .is_some_and(|url| url.contains("MK2Q4CH/A"))
            }));
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r["method"] == "Input.dispatchMouseEvent")
                    .count(),
                2,
                "加购只按下、释放各一次"
            );
            let remove = requests
                .iter()
                .filter_map(|r| r["params"]["expression"].as_str())
                .find(|script| script.contains("matches.length !== 1"))
                .unwrap();
            assert!(remove.contains("owned-item.delete"));
            assert!(remove.contains("MK2Q4CH/A"));
            if removed {
                assert_eq!(
                    requests[requests.len() - 2]["method"],
                    "Runtime.evaluate",
                    "关闭前已重新打开购物袋核实空袋"
                );
                assert_eq!(requests.last().unwrap()["method"], "Browser.close");
            }
            runner.shutdown().await;
        }
    }

    #[test]
    fn 多型号同时到货不会排队启动第二次结账() {
        let runner = CheckoutRunner::new();
        let reservation = runner.try_reserve().unwrap();
        assert!(runner.is_busy());
        assert!(runner.try_reserve().is_none());
        drop(reservation);
        assert!(!runner.is_busy());
        assert!(runner.try_reserve().is_some());
    }

    fn bag_snapshot(items: Vec<serde_json::Value>) -> serde_json::Value {
        cdp_page(
            json!({"bagEmpty": items.is_empty(), "canCheckout": !items.is_empty(),
            "items": items, "total": "", "canPlaceOrder": false, "bagReady": true, "errors": []}),
        )
    }

    fn bag_target(plan: &CheckoutPlan) -> serde_json::Value {
        json!({"name": plan.product_name, "quantity": "1", "partNumber": plan.part_number,
            "removeId": "target.delete"})
    }

    fn bag_navigation() -> serde_json::Value {
        json!({"result": {"frameId": "page"}})
    }

    fn with_navigation_checks(responses: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        responses
            .into_iter()
            .flat_map(|response| {
                if response["result"]["frameId"].is_string() {
                    vec![
                        json!({"result": {"result": {"value": true}}}),
                        response,
                        cdp_page(json!(true)),
                    ]
                } else {
                    vec![response]
                }
            })
            .collect()
    }

    #[tokio::test]
    async fn 清理失败保留待清理状态下次先清空再按新型号加购() {
        let mut plan = test_plan(true);
        plan.auto_add_to_bag = true;
        let old = json!({"name": "上次失败的旧型号", "quantity": "2", "partNumber": "OLDCH/A", "removeId": "old.delete"});
        let runner = CheckoutRunner::new();
        let (session, peer, first_requests) =
            crate::chromium_fetcher::tests::checkout_session(with_navigation_checks(vec![
                json!({"result": {"result": {"value": 1}}}),
                json!({"result": {}}),
                bag_navigation(),
                bag_snapshot(vec![old.clone()]), // 本次检查发现残留而结束结账
                bag_navigation(),
                bag_snapshot(vec![old.clone()]),
                cdp_page(json!(true)),
                cdp_page(json!(false)),
                json!({"result": {}}),
            ]))
            .await;
        *runner.session.lock().await = Some(session);
        let error = runner.run_on_hit(&plan, &|_| {}).await.unwrap_err();
        assert_eq!(error.stage, Stage::BagCleanup);
        assert!(error.can_retry());
        assert!(runner.cleanup_pending.load(Ordering::Acquire));
        assert!(peer.await.unwrap());
        assert!(
            !first_requests
                .lock()
                .await
                .iter()
                .any(|r| r["method"] == "Input.dispatchMouseEvent")
        );

        plan.part_number = "MK2U4CH/A".into();
        plan.product_name = "iPhone Duo 1TB 夜空色".into();
        plan.product_url = Some("https://www.apple.com.cn/shop/product/MK2U4CH/A".into());
        let full = bag_snapshot(vec![bag_target(&plan)]);
        let sold_out = cdp_page(json!({
            "items": [bag_target(&plan)], "canCheckout": true, "bagReady": true,
            "errors": ["商品已售罄"],
        }));
        let mut removing = old.clone();
        removing["removeId"] = json!("");
        let (session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(with_navigation_checks(vec![
                json!({"result": {"result": {"value": 1}}}),
                json!({"result": {}}),
                bag_navigation(),
                bag_snapshot(vec![old]), // 上次未清完，先处理
                cdp_page(json!(true)),
                cdp_page(json!(true)),
                bag_snapshot(vec![removing]),
                bag_snapshot(vec![]),
                bag_navigation(),
                bag_snapshot(vec![]),
                bag_navigation(),
                bag_snapshot(vec![]), // 新尝试从空袋开始
                bag_navigation(),
                cdp_page(json!({"ready": true, "error": ""})),
                cdp_page(json!({"ready": true, "point": {"x": 10, "y": 20}, "error": ""})),
                json!({"result": {}}),
                json!({"result": {}}),
                cdp_page(json!("complete")),
                bag_navigation(),
                full.clone(),
                bag_navigation(),
                sold_out,
                bag_navigation(),
                full,
                cdp_page(json!(true)),
                cdp_page(json!(true)),
                bag_snapshot(vec![]),
                bag_navigation(),
                bag_snapshot(vec![]),
                json!({"result": {}}),
            ]))
            .await;
        *runner.session.lock().await = Some(session);
        let error = runner.run_on_hit(&plan, &|_| {}).await.unwrap_err();
        assert_eq!(error.stage, Stage::Bag);
        assert!(!runner.cleanup_pending.load(Ordering::Acquire));
        assert!(peer.await.unwrap());
        let requests = requests.lock().await;
        let first_remove = requests
            .iter()
            .position(|r| {
                r["params"]["expression"]
                    .as_str()
                    .is_some_and(|s| s.contains("old.delete") && s.ends_with("\"remove\"))"))
            })
            .unwrap();
        let product = requests
            .iter()
            .position(|r| {
                r["method"] == "Page.navigate"
                    && r["params"]["url"] == plan.product_url.as_deref().unwrap()
            })
            .unwrap();
        assert!(first_remove < product);
        assert_eq!(
            requests
                .iter()
                .filter(|r| r["method"] == "Input.dispatchMouseEvent")
                .count(),
            2
        );
        assert!(!requests.iter().any(|r| {
            r["params"]["url"]
                .as_str()
                .is_some_and(|s| s.contains("MK2Q4CH"))
        }));
    }

    #[tokio::test]
    async fn 登录验证等待期间不重复输密码完成后自动恢复清理() {
        let runner = CheckoutRunner::new();
        runner.login_blocked.store(true, Ordering::Release);
        runner.cleanup_pending.store(true, Ordering::Release);
        let mut plan = test_plan(false);
        plan.auto_add_to_bag = true;
        plan.product_url = None; // 恢复验证后结束于准备阶段，不发起实际加购
        let (session, peer, requests) = crate::chromium_fetcher::tests::checkout_session(vec![
            json!({"result": {"result": {"value": 1}}}),
            json!({"result": {}}),
            cdp_page(json!({"step": "SignIn", "signIn": true})),
        ])
        .await;
        *runner.session.lock().await = Some(session);
        assert_eq!(
            runner.run_on_hit(&plan, &|_| {}).await.unwrap_err().stage,
            Stage::SignIn
        );
        assert!(!peer.await.unwrap(), "保留验证页");
        assert_eq!(
            requests.lock().await.len(),
            3,
            "不再提交密码或导航离开验证页"
        );
        assert!(runner.login_blocked.load(Ordering::Acquire));
        runner.shutdown().await;

        let (session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(with_navigation_checks(vec![
                json!({"result": {"result": {"value": 1}}}),
                json!({"result": {}}),
                cdp_page(json!({"step": "Fulfillment", "signIn": false})),
                bag_navigation(),
                bag_snapshot(vec![]), // 恢复后先完成待清理
                bag_navigation(),
                bag_snapshot(vec![]), // 准备失败收尾仍确认空袋
                json!({"result": {}}),
            ]))
            .await;
        *runner.session.lock().await = Some(session);
        assert_eq!(
            runner.run_on_hit(&plan, &|_| {}).await.unwrap_err().stage,
            Stage::Bag
        );
        assert!(peer.await.unwrap());
        assert!(!runner.login_blocked.load(Ordering::Acquire));
        assert!(!runner.cleanup_pending.load(Ordering::Acquire));
        assert!(
            !requests
                .lock()
                .await
                .iter()
                .any(|r| r["method"] == "Runtime.callFunctionOn")
        );
    }

    #[tokio::test]
    async fn 登录页面临时读取失败可以重试() {
        let (mut session, peer, _) = crate::chromium_fetcher::tests::checkout_session(vec![
            json!({"error": {"message": "context temporarily unavailable"}}),
        ])
        .await;
        let plan = test_plan(false);
        let error = Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        }
        .login_context()
        .await
        .unwrap_err();
        assert_eq!(error.stage, Stage::SignInLoading);
        assert!(error.can_retry());
        drop(session);
        assert!(peer.await.unwrap());
    }

    #[tokio::test]
    async fn 打开浏览器不会冲掉正在等待的登录验证页() {
        let (session, peer, requests) = crate::chromium_fetcher::tests::checkout_session(vec![
            json!({"result": {"result": {"value": 1}}}),
            json!({"result": {}}),
        ])
        .await;
        let runner = CheckoutRunner {
            session: Arc::new(Mutex::new(Some(session))),
            ..Default::default()
        };
        runner.login_blocked.store(true, Ordering::Release);
        runner.open("zh_CN").await.unwrap();
        assert!(!peer.await.unwrap());
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["method"], "Page.bringToFront");
        runner.shutdown().await;
    }

    #[tokio::test]
    async fn 失败购物袋清理多个条目并刷新确认不会沿用残留() {
        let mut plan = test_plan(false);
        plan.auto_add_to_bag = true;
        let target = bag_target(&plan);
        let other = json!({"name": "其他残留商品", "quantity": "2", "partNumber": "OTHERCH/A", "removeId": "other.delete"});
        let mut remaining = other.clone();
        remaining["removeId"] = json!("target.delete"); // 重绘可能复用旧索引 ID，仍按新快照清理
        let (mut session, peer, _) =
            crate::chromium_fetcher::tests::checkout_session(with_navigation_checks(vec![
                bag_navigation(),
                bag_snapshot(vec![target, other.clone()]),
                cdp_page(json!(true)),
                cdp_page(json!(true)),
                bag_snapshot(vec![remaining]),
                cdp_page(json!(true)),
                cdp_page(json!(true)),
                bag_snapshot(vec![]),
                bag_navigation(),
                bag_snapshot(vec![]),
            ]))
            .await;
        Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: true,
            hk_payment: None,
        }
        .cleanup_bag()
        .await
        .unwrap();
        drop(session);
        assert!(peer.await.unwrap());
    }

    /// 使用临时、未登录的购物袋，不点击结账，也不接触用户的购物袋和付款信息。
    #[tokio::test]
    #[ignore = "访问 Apple，独立 Pro 购物袋两轮加购与失败清空，不结账"]
    async fn 真实失败购物袋清空后可重新加购() {
        let profile = tempfile::tempdir().unwrap();
        let mut session = ChromiumSession::start_visible(profile.path().to_path_buf())
            .await
            .unwrap();
        let mut plan = test_plan(false);
        plan.auto_add_to_bag = true;
        plan.part_number = "MJTA4CH/A".into();
        plan.product_name = "iPhone 18 Pro 256GB 冰川蓝色".into();
        plan.product_url = Some("https://www.apple.com.cn/shop/product/MJTA4CH/A".into());
        let progress = |message: String| eprintln!("{message}");
        let mut results = Vec::new();
        let mut failure = None;
        let mut flow = Flow {
            session: &mut session,
            plan: &plan,
            progress: &progress,
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        };
        for round in 1..=2 {
            let start = Instant::now();
            let result = flow.prepare_bag().await;
            let add_ms = start.elapsed().as_millis();
            let start = Instant::now();
            let cleanup = flow.cleanup_bag().await;
            results.push(json!({"round": round, "addMs": add_ms, "clearAndVerifyMs": start.elapsed().as_millis(),
                "added": result.is_ok(), "emptyVerified": cleanup.is_ok()}));
            if let Err(error) = result.and(cleanup) {
                failure = Some(error);
                break;
            }
        }
        eprintln!(
            "REAL_CONTINUOUS_BAG {}",
            json!({"sku": plan.part_number, "isolatedProfile": true,
            "checkoutAttempted": false, "results": results, "error": failure.as_ref().map(ToString::to_string)})
        );
        session.close().await;
        if let Some(error) = failure {
            panic!("真实空袋重试验证失败：{error}");
        }
    }

    /// 使用临时、未登录的购物袋，不点击结账，也不接触用户的购物袋和付款信息。
    #[tokio::test]
    #[ignore = "访问 Apple 并真实加购后移除临时购物袋中的商品，不结账"]
    async fn 真实自动加购能核对型号并移除至空袋() {
        let profile = tempfile::tempdir().unwrap();
        for (part, name) in [
            ("MK2Q4CH/A", "iPhone Duo 512GB 夜空色"),
            ("MK2U4CH/A", "iPhone Duo 1TB 夜空色"),
        ] {
            let mut session = ChromiumSession::start_visible(profile.path().to_path_buf())
                .await
                .unwrap();
            let mut plan = test_plan(false);
            plan.auto_add_to_bag = true;
            plan.part_number = part.into();
            plan.product_name = name.into();
            plan.product_url = Some(format!("https://www.apple.com.cn/shop/product/{part}"));
            let progress = |message: String| eprintln!("{message}");
            let mut flow = Flow {
                session: &mut session,
                plan: &plan,
                progress: &progress,
                store_number: plan.store_number.clone(),
                store_title: plan.store_title.clone(),
                owns_bag: false,
                hk_payment: None,
            };
            let added = flow.prepare_bag().await;
            if added.is_err() {
                eprintln!("加购页诊断：{:?}", flow.session.evaluate(r#"JSON.stringify({path: location.pathname, product: new URLSearchParams(location.search).get('product'), summary: document.querySelector('[data-autom="summary-productName"]')?.innerText, title: document.title})"#, false).await);
            }
            let cleaned = if flow.owns_bag {
                flow.cleanup_bag().await
            } else {
                Ok(())
            };
            session.close().await;
            added.expect("真实加购失败");
            cleaned.expect("真实清理失败");
        }
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
            part_number: String::new(),
            remove_id: String::new(),
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
            locale: "zh_CN".into(),
            auto_sign_in: true,
            auto_add_to_bag: false,
            part_number: "MK2Q4CH/A".into(),
            product_url: Some("https://www.apple.com.cn/shop/product/MK2Q4CH/A".into()),
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
            locale: "zh_CN".into(),
            auto_sign_in: true,
            auto_add_to_bag: false,
            part_number: "MK2Q4CH/A".into(),
            product_url: Some("https://www.apple.com.cn/shop/product/MK2Q4CH/A".into()),
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
            owns_bag: false,
            hk_payment: None,
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
            locale: "zh_CN".into(),
            auto_sign_in: true,
            auto_add_to_bag: false,
            part_number: "MK2Q4CH/A".into(),
            product_url: Some("https://www.apple.com.cn/shop/product/MK2Q4CH/A".into()),
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
            owns_bag: false,
            hk_payment: None,
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

    fn hk_plan(place_order: bool) -> CheckoutPlan {
        let mut plan = test_plan(place_order);
        plan.locale = "zh_HK".into();
        plan.auto_sign_in = false;
        plan.part_number = "MG724ZA/A".into();
        plan.product_url = Some("https://www.apple.com/hk-zh/shop/product/MG724ZA/A".into());
        plan.store_number = "R428".into();
        plan.store_title = "Apple ifc mall".into();
        plan
    }

    fn hk_payment() -> HkPayment {
        serde_json::from_value::<HkPayment>(json!({
            "cardNumber": "4111111111111111", "expiration": "12/30", "securityCode": "123",
            "billingName": "Test Buyer", "street": "Test Street",
        }))
        .unwrap()
        .validate()
        .unwrap()
    }

    #[test]
    fn 香港登录仅接受所选地区官方结账地址() {
        for url in [
            "https://secure9.store.apple.com/hk-zh/shop/signIn",
            "https://secure.store.apple.com/hk-zh/shop/checkout?_s=SignIn-init",
        ] {
            assert!(trusted_checkout_login_url(url, "zh_HK"));
            assert!(!trusted_checkout_login_url(url, "zh_CN"));
        }
        for url in [
            "https://secure9.store.apple.com.example.test/hk-zh/shop/signIn",
            "https://secure9.store.apple.com/jp/shop/signIn",
            "https://secure9.www.apple.com.cn/shop/signIn",
            "https://secure9.store.apple.com/hk-zh/shop/checkout?_s=Review",
        ] {
            assert!(!trusted_checkout_login_url(url, "zh_HK"));
        }
    }

    #[tokio::test]
    async fn 香港默认手动登录不读取账户也不向页面提交密码() {
        let (mut session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(vec![]).await;
        let plan = hk_plan(false);
        let error = Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        }
        .sign_in()
        .await
        .unwrap_err();
        assert_eq!(error.stage, Stage::SignIn);
        assert!(!error.can_retry());
        assert!(error.message.contains("手动登录"));
        drop(session);
        let _ = peer.await;
        assert!(requests.lock().await.is_empty());
    }

    #[test]
    fn 香港结账计划使用香港地址手机号且自动重新登录须单独开启() {
        let mut target = apw_core::model::Target {
            locale: "zh_HK".into(),
            store_number: "R428".into(),
            store_title: "Apple ifc mall".into(),
            part_number: "MG724ZA/A".into(),
            product_name: "iPhone 17".into(),
            companion_part: None,
            companion_name: None,
            kit_part: None,
            pickup_location: None,
        };
        let mut settings = Settings::default();
        settings.checkout.locale = "zh_HK".into();
        settings.checkout.fill_contact = true;
        settings.checkout.contact_phone = "91234567".into();
        let plan = CheckoutPlan::new(&target, &settings, &Catalog::new(), false);
        assert!(!plan.auto_sign_in);
        assert_eq!(plan.contact, vec![("mobilePhone", "91234567".into())]);
        assert_eq!(
            bag_url(&plan.locale),
            "https://www.apple.com/hk-zh/shop/bag"
        );
        assert!(
            plan.product_url
                .unwrap()
                .starts_with("https://www.apple.com/hk-zh/")
        );
        settings.checkout.hk_auto_sign_in = true;
        assert!(CheckoutPlan::new(&target, &settings, &Catalog::new(), false).auto_sign_in);
        target.locale = "zh_CN".into();
        settings.checkout.hk_auto_sign_in = false;
        assert!(CheckoutPlan::new(&target, &settings, &Catalog::new(), false).auto_sign_in);
    }

    #[tokio::test]
    async fn 香港付款资料只保留在当前运行并可清除且结账期间不可更换() {
        let runner = CheckoutRunner::new();
        assert!(!runner.has_hk_payment().await);
        runner.set_hk_payment(Some(hk_payment())).await.unwrap();
        assert!(runner.has_hk_payment().await);
        assert!(!CheckoutRunner::new().has_hk_payment().await);
        let reserved = runner.try_reserve().unwrap();
        assert!(runner.set_hk_payment(None).await.is_err());
        drop(reserved);
        runner.set_hk_payment(None).await.unwrap();
        assert!(!runner.has_hk_payment().await);
    }

    #[tokio::test]
    async fn 香港付款核对后默认停在查看订单明确开启才提交一次() {
        for place_order in [false, true] {
            let mut responses = vec![
                cdp_page(json!({"ready": true, "error": null})),
                json!({"result": {"result": {"value": true}}}),
                cdp_page(json!({"step": "Review", "signIn": false})),
                cdp_page(json!([])),
                cdp_page(
                    json!({"items": [{"name": "要买的商品", "quantity": "1", "partNumber": "MG724ZA/A"}],
                    "total": "HK$100", "canCheckout": false, "canPlaceOrder": true}),
                ),
            ];
            if place_order {
                responses.extend([
                    json!({"result": {"result": {"value": true}}}),
                    cdp_page(json!({"step": "ThankYou", "order": "W123456789"})),
                ]);
            }
            let (mut session, peer, requests) =
                crate::chromium_fetcher::tests::checkout_session(responses).await;
            let plan = hk_plan(place_order);
            let mut flow = Flow {
                session: &mut session,
                plan: &plan,
                progress: &|_| {},
                store_number: plan.store_number.clone(),
                store_title: plan.store_title.clone(),
                owns_bag: false,
                hk_payment: Some(hk_payment()),
            };
            flow.billing().await.unwrap();
            let result = flow.review().await.unwrap();
            assert_eq!(
                matches!(result, CheckoutOutcome::OrderSubmitted { .. }),
                place_order
            );
            drop(session);
            let _ = peer.await;
            let requests = requests.lock().await;
            let submits = requests
                .iter()
                .filter(|r| {
                    r["params"]["expression"].as_str().is_some_and(|e| {
                        e.contains("const e = document.querySelector")
                            && e.contains("continue-button-placeOrder")
                    })
                })
                .count();
            assert_eq!(submits, usize::from(place_order));
        }
    }

    #[tokio::test]
    async fn 原地区等待登录时切换地区不会冲掉验证页面() {
        let runner = CheckoutRunner::new();
        *runner.active_locale.lock().await = "zh_CN".into();
        runner.login_blocked.store(true, Ordering::Release);
        let (session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(vec![]).await;
        *runner.session.lock().await = Some(session);
        let result = runner.run_on_hit(&hk_plan(false), &|_| {}).await;
        assert_eq!(result.unwrap_err().stage, Stage::SignIn);
        assert!(runner.session.lock().await.is_some());
        runner.shutdown().await;
        let _ = peer.await;
        assert!(requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn 香港订单数量不明确时不点击提交() {
        let plan = hk_plan(true);
        let (mut session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(vec![cdp_page(
                json!({"items": [{"name": plan.product_name, "quantity": ""}],
                "total": "HK$100", "canCheckout": false, "canPlaceOrder": true}),
            )])
            .await;
        let error = Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: false,
            hk_payment: None,
        }
        .review()
        .await
        .unwrap_err();
        assert_eq!(error.stage, Stage::Review);
        drop(session);
        let _ = peer.await;
        assert_eq!(requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn 香港清袋始终使用香港地址并传入地区() {
        let mut plan = hk_plan(false);
        plan.auto_add_to_bag = true;
        let (mut session, peer, requests) =
            crate::chromium_fetcher::tests::checkout_session(with_navigation_checks(vec![
                bag_navigation(),
                bag_snapshot(vec![bag_target(&plan)]),
                cdp_page(json!(true)),
                cdp_page(json!(true)),
                bag_snapshot(vec![]),
                bag_navigation(),
                bag_snapshot(vec![]),
            ]))
            .await;
        Flow {
            session: &mut session,
            plan: &plan,
            progress: &|_| {},
            store_number: plan.store_number.clone(),
            store_title: plan.store_title.clone(),
            owns_bag: true,
            hk_payment: None,
        }
        .cleanup_bag()
        .await
        .unwrap();
        drop(session);
        let _ = peer.await;
        let requests = requests.lock().await;
        assert!(
            requests
                .iter()
                .filter(|r| r["method"] == "Page.navigate")
                .all(|r| r["params"]["url"] == "https://www.apple.com/hk-zh/shop/bag")
        );
        assert!(requests.iter().any(|r| {
            r["params"]["expression"]
                .as_str()
                .is_some_and(|e| e.contains(r#""locale":"zh_HK""#) && e.contains("removeId"))
        }));
    }
}
