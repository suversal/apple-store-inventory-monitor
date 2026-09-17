//! Tauri 应用外壳。
//!
//! 这一层只做装配和转译：起引擎、把命令转成引擎消息、把引擎事件转发给前端、
//! 在有货时发提醒。**所有业务判断都在 `apw-core` 里**，这里不许出现任何
//! 「什么算有货」之类的逻辑 —— 一旦让界面层参与判断，那条核心不变量就多了
//! 一处可以被绕开的地方。
//!
//! 提醒也刻意放在这一层而不是前端：托盘模式下窗口是隐藏的，WebView 可能被
//! 系统节流甚至挂起，把「及时提醒」挂在一个会被挂起的执行环境上是不能接受的。

use std::sync::RwLock;
use std::time::Duration;

use apw_core::catalog::Catalog;
use apw_core::config::{MIN_INTERVAL_SECONDS, OpenOnHit, Settings, SettingsStore};
use apw_core::model::{Category, Product, REGIONS, Store, Target, region_by_locale};
use apw_core::notify::{Bark, Multi, Notification, Notifier, Sound};
use apw_core::watcher::{Event, TargetState, Watcher, WatcherConfig};
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, WindowEvent};
use tauri_plugin_updater::UpdaterExt;

mod chromium_fetcher;
use chromium_fetcher::AppleChromiumFetcher;

/// 前端事件通道名。前端用 `listen("watcher://event", ...)` 订阅。
const EVENT_CHANNEL: &str = "watcher://event";
/// 启动过程中的降级说明通道：配置读不出来之类的事必须让用户看见。
const NOTICE_CHANNEL: &str = "watcher://notice";

/// 地区的可序列化形式。
///
/// `model::Region` 的字段都是 `&'static str`，而且界面不需要知道 `base_url`
/// 这类内部细节。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct RegionDto {
    title: &'static str,
    locale: &'static str,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DeliveryLocalityOption {
    text: String,
    value: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DeliveryLocalitiesDto {
    states: Vec<DeliveryLocalityOption>,
    cities: Vec<DeliveryLocalityOption>,
    districts: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WatchBandChoiceDto {
    style_key: String,
    style_name: String,
    color_key: String,
    color_name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct WatchBandSizeDto {
    part_number: String,
    text: String,
}

#[derive(Debug, Deserialize, Default)]
struct DeliveryLocalityField {
    #[serde(default)]
    data: Vec<DeliveryLocalityOption>,
}

#[derive(Debug, Deserialize, Default)]
struct DeliveryLocalityBody {
    #[serde(default)]
    state: DeliveryLocalityField,
    #[serde(default)]
    city: DeliveryLocalityField,
    #[serde(default)]
    district: DeliveryLocalityField,
}

#[derive(Debug, Deserialize)]
struct DeliveryLocalityResponse {
    body: DeliveryLocalityBody,
}

fn usable_localities(field: DeliveryLocalityField) -> Vec<DeliveryLocalityOption> {
    field
        .data
        .into_iter()
        .filter(|option| !option.value.trim().is_empty())
        .collect()
}

fn parse_delivery_localities(body: &[u8]) -> Result<DeliveryLocalitiesDto, serde_json::Error> {
    let parsed: DeliveryLocalityResponse = serde_json::from_slice(body)?;
    Ok(DeliveryLocalitiesDto {
        states: usable_localities(parsed.body.state),
        cities: usable_localities(parsed.body.city),
        districts: usable_localities(parsed.body.district),
    })
}

fn plain_text(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn parse_watch_band_choices(body: &[u8]) -> Result<Vec<WatchBandChoiceDto>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple 表带响应不是 JSON：{error}"))?;
    let items = value
        .pointer("/body/items")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Apple 表带响应缺少 body.items".to_string())?;
    let mut choices = Vec::new();
    for (style_key, style) in items {
        let style_name = plain_text(
            style
                .get("sectionHeader")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(style_key),
        );
        let style_order = style
            .get("sortOrder")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(i64::MAX);
        let Some(colors) = style
            .get("subDimensionValue")
            .and_then(serde_json::Value::as_array)
        else {
            continue;
        };
        for color in colors {
            let Some(color_key) = color
                .get("dimensionValue")
                .and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let color_name = color
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(plain_text)
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| color_key.to_string());
            let color_order = color
                .get("sortOrder")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(i64::MAX);
            choices.push((
                style_order,
                color_order,
                WatchBandChoiceDto {
                    style_key: style_key.clone(),
                    style_name: style_name.clone(),
                    color_key: color_key.to_string(),
                    color_name,
                },
            ));
        }
    }
    choices.sort_by_key(|(style_order, color_order, choice)| {
        (
            *style_order,
            *color_order,
            choice.style_key.clone(),
            choice.color_key.clone(),
        )
    });
    Ok(choices.into_iter().map(|(_, _, choice)| choice).collect())
}

fn parse_watch_band_sizes(body: &[u8]) -> Result<Vec<WatchBandSizeDto>, String> {
    let value: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Apple 表带尺码响应不是 JSON：{error}"))?;
    let options = value
        .pointer("/body/options")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "Apple 表带尺码响应缺少 body.options".to_string())?;
    let mut sizes = Vec::new();
    for option in options {
        let Some(part_number) = option
            .pointer("/product/options/watch_bands")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| value.contains('/'))
        else {
            continue;
        };
        let text = option
            .get("text")
            .and_then(serde_json::Value::as_str)
            .map(plain_text)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                option
                    .get("key")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| part_number.to_string());
        sizes.push(WatchBandSizeDto {
            part_number: part_number.to_string(),
            text,
        });
    }
    Ok(sizes)
}

/// 品类的可序列化形式。
///
/// 界面上的品类下拉框由这里驱动，而不是在前端另抄一份常量：抄一份就迟早会有
/// 一边先加了品类、另一边还蒙在鼓里。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CategoryDto {
    value: Category,
    title: &'static str,
}

struct AppState {
    watcher: Watcher,
    catalog: Catalog,
    http: reqwest::Client,
    /// 设置的内存副本。写盘失败不该让界面卡住，所以内存副本是权威的展示来源。
    settings: RwLock<Settings>,
    /// 为 `None` 表示配置不可持久化（目录不可写，或上次读取失败已放弃写盘）。
    store: Option<SettingsStore>,
}

impl AppState {
    fn settings_snapshot(&self) -> Settings {
        self.settings
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    /// 更新内存副本并尝试落盘。落盘失败只报错，不回滚内存 ——
    /// 用户的操作已经生效了，没道理因为磁盘问题把界面弹回去。
    fn put_settings(&self, next: Settings) -> Result<(), String> {
        let mut guard = self.settings.write().unwrap_or_else(|e| e.into_inner());
        *guard = next;
        let to_save = guard.clone();
        drop(guard);

        match &self.store {
            Some(store) => store.save(&to_save).map_err(|e| e.to_string()),
            None => Ok(()),
        }
    }
}

#[tauri::command]
fn list_regions() -> Vec<RegionDto> {
    REGIONS
        .iter()
        .map(|r| RegionDto {
            title: r.title,
            locale: r.locale,
        })
        .collect()
}

#[tauri::command]
fn list_categories() -> Vec<CategoryDto> {
    Category::ALL
        .iter()
        .map(|c| CategoryDto {
            value: *c,
            title: c.title(),
        })
        .collect()
}

/// 读取 Apple 官网“送货选项”使用的省、市、区三级联动数据。
///
/// 中国大陆官网当前明确返回三个 select；其他地区多为邮编或定位输入，不能硬套
/// 省市区模型，因此先拒绝而不是给用户一组看似可选、实际查询无效的值。
#[tauri::command]
async fn list_delivery_localities(
    state: tauri::State<'_, AppState>,
    locale: String,
    state_name: String,
    city_name: String,
) -> Result<DeliveryLocalitiesDto, String> {
    if locale != "zh_CN" {
        return Err("Apple 官网当前仅在中国大陆站提供省、市、区三级选择".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let mut query = vec![("fae", "true")];
    let state_name = state_name.trim();
    let city_name = city_name.trim();
    if !state_name.is_empty() {
        query.push(("state", state_name));
    }
    if !city_name.is_empty() {
        query.push(("city", city_name));
    }

    let response = state
        .http
        .get(format!("{}/shop/address/locality-lookup", region.base_url))
        .query(&query)
        .timeout(Duration::from_secs(20))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/javascript, */*; q=0.01",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .map_err(|error| format!("读取 Apple 送货地区失败：{error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Apple 送货地区响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("Apple 送货地区接口返回 HTTP {}", status.as_u16()));
    }
    if body.len() > 512 * 1024 {
        return Err("Apple 送货地区响应超过 512 KiB 安全上限".into());
    }
    parse_delivery_localities(&body).map_err(|error| format!("Apple 送货地区响应无法解析：{error}"))
}

fn watch_band_product(state: &AppState, locale: &str, case_part: &str) -> Result<Product, String> {
    let mut product = state
        .catalog
        .product_by_part(locale, case_part)
        .ok_or_else(|| format!("型号目录中找不到 Apple Watch 表壳 {case_part}"))?;
    if product.category != Category::Watch {
        return Err(format!("{case_part} 不是 Apple Watch 表壳"));
    }
    if product.kit_part.as_deref().is_none_or(str::is_empty) {
        return Err("这条 Apple Watch 型号缺少整表套件号，请先刷新型号目录".into());
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        let words: Vec<_> = product.title.split_whitespace().collect();
        product.watch_case_size = words.windows(2).find_map(|pair| {
            (pair[1] == "毫米" && pair[0].chars().all(|ch| ch.is_ascii_digit()))
                .then(|| format!("{}mm", pair[0]))
        });
    }
    if product.watch_case_size.as_deref().is_none_or(str::is_empty) {
        return Err("这条 Apple Watch 型号缺少表壳尺寸，请先刷新型号目录".into());
    }
    Ok(product)
}

async fn fetch_watch_band_api(
    state: &AppState,
    url: reqwest::Url,
    label: &str,
) -> Result<Vec<u8>, String> {
    let response = state
        .http
        .get(url)
        .timeout(Duration::from_secs(20))
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/javascript, */*; q=0.01",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9")
        .header("X-Requested-With", "XMLHttpRequest")
        .send()
        .await
        .map_err(|error| format!("读取 Apple {label}失败：{error}"))?;
    let status = response.status();
    let body = response
        .bytes()
        .await
        .map_err(|error| format!("读取 Apple {label}响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("Apple {label}接口返回 HTTP {}", status.as_u16()));
    }
    if body.len() > 2 * 1024 * 1024 {
        return Err(format!("Apple {label}响应超过 2 MiB 安全上限"));
    }
    Ok(body.to_vec())
}

/// 按表壳读取 Apple 官网当前允许搭配的表带款式和颜色。
#[tauri::command]
async fn list_watch_band_choices(
    state: tauri::State<'_, AppState>,
    locale: String,
    case_part: String,
) -> Result<Vec<WatchBandChoiceDto>, String> {
    if locale != "zh_CN" {
        return Err("Apple Watch 精确送货表带选择目前仅支持中国大陆站".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let product = watch_band_product(&state, &locale, &case_part)?;
    let kit = product
        .kit_part
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let case_size = product.watch_case_size.as_deref().unwrap_or_default();
    let mut url = reqwest::Url::parse(&format!("{}/shop/api/band-selection", region.base_url))
        .map_err(|error| format!("无法构造 Apple 表带接口：{error}"))?;
    url.query_pairs_mut()
        .append_pair("fae", "true")
        .append_pair("product", &kit)
        .append_pair("option.watch_cases", &product.part_number)
        .append_pair("dm.watch_cases-dimensionCaseSize", case_size);
    let body = fetch_watch_band_api(&state, url, "表带选项").await?;
    parse_watch_band_choices(&body)
}

/// 读取某一款式、颜色下的尺码；每个选项直接携带查询送货所需的表带零件号。
#[tauri::command]
async fn list_watch_band_sizes(
    state: tauri::State<'_, AppState>,
    locale: String,
    case_part: String,
    style_key: String,
    color_key: String,
) -> Result<Vec<WatchBandSizeDto>, String> {
    if locale != "zh_CN" {
        return Err("Apple Watch 精确送货表带选择目前仅支持中国大陆站".into());
    }
    if style_key.len() > 80 || color_key.len() > 80 {
        return Err("Apple 表带选项值过长".into());
    }
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let product = watch_band_product(&state, &locale, &case_part)?;
    let kit = product
        .kit_part
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let case_size = product.watch_case_size.as_deref().unwrap_or_default();
    let mut url = reqwest::Url::parse(&format!("{}/shop/api/band_sizes", region.base_url))
        .map_err(|error| format!("无法构造 Apple 表带尺码接口：{error}"))?;
    url.query_pairs_mut()
        .append_pair("fae", "true")
        .append_pair("product", &kit)
        .append_pair("option.watch_cases", &product.part_number)
        .append_pair("dm.watch_bands-dimensionBandStyle", style_key.trim())
        .append_pair("dm.watch_bands-dimensionColor", color_key.trim())
        .append_pair("dm.watch_cases-dimensionCaseSize", case_size);
    let body = fetch_watch_band_api(&state, url, "表带尺码").await?;
    parse_watch_band_sizes(&body)
}

#[tauri::command]
fn list_stores(state: tauri::State<'_, AppState>, locale: String) -> Result<Vec<Store>, String> {
    state.catalog.stores(&locale).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_products(
    state: tauri::State<'_, AppState>,
    locale: String,
) -> Result<Vec<Product>, String> {
    state.catalog.products(&locale).map_err(|e| e.to_string())
}

/// 从 Apple 官网抓最新型号，替换该地区该品类的内存副本，返回抓到的型号数。
///
/// `category` 为 `None` 时抓该地区的全部购买页。界面传的是当前选中的品类：
/// 一次只抓那几页，用户想看新出的 Mac 不必等 iPhone、iPad、Watch 一起抓完。
#[tauri::command]
async fn refresh_products(
    state: tauri::State<'_, AppState>,
    locale: String,
    category: Option<Category>,
) -> Result<usize, String> {
    let region = region_by_locale(&locale).ok_or_else(|| format!("认不出地区 {locale}"))?;
    let count = state
        .catalog
        .refresh_products(region, category, &state.http)
        .await
        .map_err(|e| e.to_string())?;

    // 目录更新后同步修复已有 Watch 目标的展示元数据。目标键只由地区、门店和
    // 零件号决定，因此这里不会增加、删除或换掉用户正在监控的 SKU。
    let mut next = state.settings_snapshot();
    let before = next.targets.clone();
    state.catalog.hydrate_watch_targets(&mut next.targets);
    if next.targets != before {
        state.watcher.set_targets(next.targets.clone()).await;
        state.put_settings(next)?;
    }
    Ok(count)
}

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings_snapshot()
}

#[tauri::command]
async fn save_settings(
    state: tauri::State<'_, AppState>,
    settings: Settings,
) -> Result<Settings, String> {
    let mut next = settings;
    next.normalize();
    state.catalog.hydrate_watch_targets(&mut next.targets);

    // 设置里的目标列表和查询间隔要同步给引擎，否则改完设置监控还按旧的跑。
    state.watcher.set_targets(next.targets.clone()).await;
    state.watcher.set_interval(next.interval()).await;
    state
        .watcher
        .set_delivery_region(next.delivery_region.clone())
        .await;

    state.put_settings(next.clone())?;
    Ok(next)
}

#[tauri::command]
async fn get_snapshot(state: tauri::State<'_, AppState>) -> Result<Vec<TargetState>, String> {
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_targets(
    state: tauri::State<'_, AppState>,
    targets: Vec<Target>,
) -> Result<Vec<TargetState>, String> {
    let mut next = state.settings_snapshot();
    next.targets = targets;
    next.normalize();
    state.catalog.hydrate_watch_targets(&mut next.targets);
    state.watcher.set_targets(next.targets.clone()).await;
    state.put_settings(next)?;
    Ok(state.watcher.snapshot().await)
}

#[tauri::command]
async fn set_interval(state: tauri::State<'_, AppState>, seconds: u64) -> Result<u64, String> {
    let secs = seconds.max(MIN_INTERVAL_SECONDS);
    state.watcher.set_interval(Duration::from_secs(secs)).await;
    let mut next = state.settings_snapshot();
    next.interval_seconds = secs;
    state.put_settings(next)?;
    Ok(secs)
}

#[tauri::command]
async fn start_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.watcher.start().await;
    Ok(())
}

#[tauri::command]
async fn stop_watching(state: tauri::State<'_, AppState>) -> Result<(), String> {
    state.watcher.stop().await;
    Ok(())
}

#[tauri::command]
async fn retry_watching_now(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.watcher.retry_now().await)
}

#[tauri::command]
async fn is_running(state: tauri::State<'_, AppState>) -> Result<bool, String> {
    Ok(state.watcher.is_running().await)
}

/// 一个待安装的更新。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateInfo {
    version: String,
    current_version: String,
    notes: Option<String>,
}

/// 查询有没有新版本。返回 `None` 表示已经是最新。
///
/// **刻意不做静默自动安装**：给用户装东西这件事应该由用户点头。何况这是个会在
/// 抢购当口挂着的程序，自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
#[tauri::command]
async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    match tokio::time::timeout(Duration::from_secs(20), updater.check())
        .await
        .map_err(|_| "检查更新超时，请稍后重试".to_string())?
    {
        Ok(Some(update)) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        Ok(None) => Ok(None),
        // 检查更新失败不是错误状态，只是这次没查到 —— 网络不通、GitHub 抽风都
        // 会走到这里，没必要弹给用户看，写进日志即可。
        Err(err) => Err(err.to_string()),
    }
}

/// 下载并安装更新。安装完成后需要重启应用才生效。
#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app
        .updater_builder()
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;
    let update = tokio::time::timeout(Duration::from_secs(20), updater.check())
        .await
        .map_err(|_| "检查更新超时，请稍后重试".to_string())?
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "已经是最新版本".to_string())?;

    let handle = app.clone();
    let finished = app.clone();
    let mut downloaded = 0_u64;
    let mut last_emit = std::time::Instant::now() - Duration::from_secs(1);
    let _ = app.emit(
        "watcher://update-progress",
        serde_json::json!({
            "phase": "downloading", "downloaded": 0, "total": null
        }),
    );
    let bytes = update
        .download(
            move |chunk, total| {
                // 插件回调给的是本次数据块大小，界面需要累计字节数。
                downloaded = downloaded.saturating_add(chunk as u64);
                if last_emit.elapsed() >= Duration::from_millis(100) || total == Some(downloaded) {
                    let _ = handle.emit(
                        "watcher://update-progress",
                        serde_json::json!({
                            "phase": "downloading", "downloaded": downloaded, "total": total
                        }),
                    );
                    last_emit = std::time::Instant::now();
                }
            },
            move || {
                let _ = finished.emit(
                    "watcher://update-progress",
                    serde_json::json!({
                        "phase": "verifying", "downloaded": 0, "total": null
                    }),
                );
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    // download 完成签名验证后才允许安装，校验失败绝不进入此分支。
    let _ = app.emit(
        "watcher://update-progress",
        serde_json::json!({
            "phase": "installing", "downloaded": 0, "total": null
        }),
    );
    tauri::async_runtime::spawn_blocking(move || update.install(bytes))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 与库存目标共用目录信息，避免 Apple Watch 的表壳料号跳到不存在的详情页。
fn target_purchase_url(app: &AppHandle, target: &Target) -> Option<String> {
    let product = app.try_state::<AppState>().and_then(|state| {
        state
            .catalog
            .product_by_part(&target.locale, &target.part_number)
    });
    target.purchase_url(product.as_ref())
}

/// 按用户选择返回这个监控目标对应的跳转地址。
fn target_open_url(app: &AppHandle, target: &Target, destination: OpenOnHit) -> Option<String> {
    match destination {
        OpenOnHit::None => None,
        OpenOnHit::Bag => region_by_locale(&target.locale).map(|region| region.bag_url()),
        OpenOnHit::Product => target_purchase_url(app, target),
    }
}

fn destination_title(destination: OpenOnHit) -> &'static str {
    match destination {
        OpenOnHit::None => "",
        OpenOnHit::Bag => "购物袋",
        OpenOnHit::Product => "商品页",
    }
}

#[tauri::command]
fn open_target_product(app: AppHandle, target: Target) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    let url = target_purchase_url(&app, &target).ok_or("无法识别目标地区")?;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 手动测试提醒和首个目标的跳转，便于提前验证实际操作链路。
#[tauri::command]
async fn test_notify(app: AppHandle) -> Result<(), String> {
    let settings = app.state::<AppState>().settings_snapshot();
    let bark_url = settings.targets.first().map_or_else(
        || settings.bark_url.as_str(),
        |target| settings.bark_url_for(target),
    );
    if !settings.sound_enabled
        && bark_url.trim().is_empty()
        && settings.open_on_hit == OpenOnHit::None
    {
        return Err("请先开启提示音、页面跳转或配置 Bark，再测试提醒".into());
    }
    let mut notification = Notification::new(
        "提醒测试（不代表有货）",
        "请确认已开启的提醒是否收到；Apple 页面仍需自行结账",
    );
    if settings.open_on_hit == OpenOnHit::Product
        && settings.targets.is_empty()
        && !settings.sound_enabled
        && bark_url.trim().is_empty()
    {
        return Err("请先添加监控目标，再测试商品页跳转".into());
    }

    let jump_url = match settings.open_on_hit {
        OpenOnHit::None => None,
        OpenOnHit::Bag => {
            let locale = settings
                .targets
                .first()
                .map_or(settings.locale.as_str(), |target| target.locale.as_str());
            region_by_locale(locale).map(|region| region.bag_url())
        }
        OpenOnHit::Product => settings
            .targets
            .first()
            .and_then(|target| target_purchase_url(&app, target)),
    };

    if settings.open_on_hit != OpenOnHit::None
        && jump_url.is_none()
        && !settings.sound_enabled
        && bark_url.trim().is_empty()
    {
        return Err(format!(
            "无法生成{}跳转地址",
            destination_title(settings.open_on_hit)
        ));
    }

    if let Some(url) = jump_url {
        notification = notification.with_url(url.clone());
        use tauri_plugin_opener::OpenerExt;
        app.opener()
            .open_url(url, None::<&str>)
            .map_err(|e| e.to_string())?;
    }
    dispatch_notification(&app, notification, bark_url)
        .await
        .map_err(|e| e.to_string())
}

/// 按用户设置发送提示音和 Bark 提醒。
async fn dispatch_notification(
    app: &AppHandle,
    notification: Notification,
    bark_url: &str,
) -> Result<(), apw_core::notify::NotifyError> {
    let settings = match app.try_state::<AppState>() {
        Some(state) => state.settings_snapshot(),
        None => return Ok(()),
    };

    let mut channels = Multi::new();
    if settings.sound_enabled {
        channels.push(Sound::embedded());
    }
    if !bark_url.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // Bark 每次现构造：地址是用户随时可改的设置项，缓存实例会在改完地址后
        // 继续往旧地址推。共享的 http 客户端一并传进去，连接池仍然复用。
        channels.push(Bark::new(bark_url.to_owned(), http));
    }
    if channels.is_empty() {
        return Ok(());
    }
    channels.notify(&notification).await
}

fn in_stock_notification_body(target: &Target) -> String {
    let mut body = format!("{} {}", target.store_title, target.product_name);
    if let Some(companion) = target
        .companion_part
        .as_deref()
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let label = target
            .companion_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty());
        if let Some(label) = label {
            body.push_str(&format!("\nWatch 送货表带 {label} [{companion}]"));
        } else {
            body.push_str(&format!("\nWatch 送货搭配表带 {companion}"));
        }
    }
    body
}

/// 消费引擎事件：转发给前端，并在有货时发提醒。
async fn pump_events(app: AppHandle, mut events: tokio::sync::mpsc::Receiver<Event>) {
    while let Some(event) = events.recv().await {
        // 先原样转发。前端拿到的事件流应当与引擎发出的完全一致，
        // 中间少一层可能出错的翻译。
        let _ = app.emit(EVENT_CHANNEL, &event);

        if let Event::InStock { state } = &event {
            let target = &state.target;
            let settings = app
                .try_state::<AppState>()
                .map(|s| s.settings_snapshot())
                .unwrap_or_default();
            let destination_url = target_open_url(&app, target, settings.open_on_hit);
            let bark_url = settings.bark_url_for(target).to_owned();
            let has_product_bark = settings.product_bark_urls.contains_key(&target.part_number);
            let mut notification = Notification::new("有货了", in_stock_notification_body(target));
            if let Some(url) = &destination_url {
                notification = notification.with_url(url.clone());
            }

            let mut opened_destination = None;
            if settings.open_on_hit != OpenOnHit::None {
                use tauri_plugin_opener::OpenerExt;
                match destination_url {
                    Some(url) => match app.opener().open_url(url, None::<&str>) {
                        Ok(()) => opened_destination = Some(settings.open_on_hit),
                        Err(err) => {
                            let _ = app.emit(
                                NOTICE_CHANNEL,
                                format!(
                                    "自动打开{}失败：{err}",
                                    destination_title(settings.open_on_hit)
                                ),
                            );
                        }
                    },
                    None => {
                        let _ = app.emit(
                            NOTICE_CHANNEL,
                            format!(
                                "自动打开{}失败：无法生成跳转地址",
                                destination_title(settings.open_on_hit)
                            ),
                        );
                    }
                }
            }

            if let Err(err) = dispatch_notification(&app, notification, &bark_url).await {
                // 提醒没发出去是遗憾，但绝不能让监控本身停下来。
                let _ = app.emit(NOTICE_CHANNEL, format!("发送提醒时出错：{err}"));
            } else {
                let mut actions = Vec::new();
                if settings.sound_enabled {
                    actions.push("提示音");
                }
                if !bark_url.trim().is_empty() {
                    actions.push(if has_product_bark {
                        "Bark（型号专属）"
                    } else {
                        "Bark"
                    });
                }
                if let Some(destination) = opened_destination {
                    match destination {
                        OpenOnHit::Bag => actions.push("已打开购物袋"),
                        OpenOnHit::Product => actions.push("已打开商品页"),
                        OpenOnHit::None => {}
                    }
                }
                if actions.is_empty() {
                    continue;
                }
                let _ = app.emit(
                    NOTICE_CHANNEL,
                    format!(
                        "到货提醒已执行：{} {}（{}）",
                        target.store_title,
                        target.product_name,
                        actions.join("、")
                    ),
                );
            }
        }
    }
}

/// 建系统托盘。
///
/// 关窗口时不退出而是收进托盘：这个工具的正常用法就是挂上几个小时等发售，
/// 让它一直占着一个窗口和 Dock 图标没有道理。
fn setup_tray(app: &AppHandle) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("main")
        .icon(
            app.default_window_icon().cloned().ok_or_else(|| {
                tauri::Error::AssetNotFound("默认窗口图标缺失，无法建立托盘".into())
            })?,
        )
        .tooltip("果到雷达")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => reveal_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                reveal_window(tray.app_handle());
            }
        })
        .build(app)?;
    Ok(())
}

fn reveal_window(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// 载入设置。读不出来时放弃写盘并留档原文件。
///
/// 这一条是刻意的：读不到旧配置**不等于**用户没有配置。若照常写盘，界面初始化
/// 时的几次控件赋值就会把仅存的那份原子替换成一份空的默认配置，监控列表再也
/// 找不回来。Go 版正是这么丢过数据。
fn load_settings(notices: &mut Vec<String>) -> (Settings, Option<SettingsStore>) {
    let store = match SettingsStore::new() {
        Ok(s) => s,
        Err(err) => {
            notices.push(format!("配置目录不可用，本次运行的设置不会被保存：{err}"));
            return (Settings::default(), None);
        }
    };

    // 「新版配置文件还不存在」才是首次运行的判据。
    //
    // 不能用「目标列表为空」代替：用户删光目标后保存的是一份合法的空目标配置，
    // 而旧版 settings.json 是刻意保留不删的（为了能回退），于是下次启动会把他
    // 亲手删掉的目标连同 locale、Bark 地址一起原样倒回来。
    let first_run = store.path().symlink_metadata().is_err();

    match store.load() {
        Ok(settings) => {
            if first_run && let Some(previous) = store.import_previous_version() {
                notices.push(format!(
                    "已从改名前版本迁移了 {} 条监控目标。",
                    previous.targets.len()
                ));
                if let Err(err) = store.save(&previous) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (previous, Some(store));
            }
            if first_run
                && let Some(legacy) = store.import_legacy()
                && !legacy.targets.is_empty()
            {
                notices.push(format!(
                    "已从旧版设置迁移了 {} 条监控目标。",
                    legacy.targets.len()
                ));
                // 立刻落盘。否则用户不改任何设置时新版文件一直不存在，
                // 每次启动都要重迁一遍，用户删掉的目标也会一直复活。
                if let Err(err) = store.save(&legacy) {
                    notices.push(format!("迁移结果暂时没能保存：{err}"));
                }
                return (legacy, Some(store));
            }
            (settings, Some(store))
        }
        Err(err) => {
            match store.preserve_corrupted() {
                Ok(Some(path)) => {
                    notices.push(format!("读取设置失败，原文件已备份到 {}", path.display()))
                }
                Ok(None) => {}
                Err(backup_err) => {
                    notices.push(format!("读取设置失败，且无法备份原文件：{backup_err}"));
                }
            }
            notices.push(format!(
                "读取设置失败，已回退到默认设置，并且本次运行不会覆盖磁盘上的配置：{err}"
            ));
            (Settings::default(), None)
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .setup(|app| {
            let mut notices = Vec::new();
            let (mut settings, store) = load_settings(&mut notices);
            let catalog = Catalog::new();
            let saved_targets = settings.targets.clone();
            catalog.hydrate_watch_targets(&mut settings.targets);
            if settings.targets != saved_targets
                && let Some(store) = store.as_ref()
                && let Err(error) = store.save(&settings)
            {
                notices.push(format!(
                    "Watch 型号名称已在本次运行修复，但暂时无法保存：{error}"
                ));
            }

            // 用 Watcher::new 而不是 Watcher::spawn：setup 回调跑在主线程上，
            // 并不处在 tokio 运行时上下文里，在这里 tokio::spawn 会 panic，
            // 而且因为发生在不可展开的回调中，进程会直接 abort。
            // 引擎任务交给 Tauri 自己的运行时去驱动。
            let watcher_config = WatcherConfig {
                delivery_region: settings.delivery_region.clone(),
                ..WatcherConfig::default()
            };
            let (watcher, events, engine) =
                Watcher::new(AppleChromiumFetcher::new(), watcher_config);
            tauri::async_runtime::spawn(engine);

            {
                let watcher = watcher.clone();
                let targets = settings.targets.clone();
                let interval = settings.interval();
                tauri::async_runtime::spawn(async move {
                    watcher.set_targets(targets).await;
                    watcher.set_interval(interval).await;
                });
            }

            app.manage(AppState {
                watcher,
                catalog,
                http: reqwest::Client::new(),
                settings: RwLock::new(settings),
                store,
            });

            let handle: AppHandle = app.handle().clone();
            tauri::async_runtime::spawn(pump_events(handle.clone(), events));

            if let Err(err) = setup_tray(&handle) {
                // 托盘建不起来只是少一项能力，不该让程序起不来。
                notices.push(format!("系统托盘不可用：{err}"));
            }

            if !notices.is_empty() {
                // 界面还没订阅上，稍等一下再发。这些是降级说明，用户必须看见 ——
                // 只写到 stderr 是没用的，用户是双击图标启动的。
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    for notice in notices {
                        let _ = handle.emit(NOTICE_CHANNEL, notice);
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                // 收进托盘而不是退出。真要退出走托盘菜单里的「退出」。
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            list_regions,
            list_categories,
            list_delivery_localities,
            list_watch_band_choices,
            list_watch_band_sizes,
            list_stores,
            list_products,
            refresh_products,
            get_settings,
            save_settings,
            get_snapshot,
            set_targets,
            set_interval,
            start_watching,
            stop_watching,
            retry_watching_now,
            is_running,
            test_notify,
            open_target_product,
            check_for_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}

#[cfg(test)]
mod tests {
    use super::{
        in_stock_notification_body, parse_delivery_localities, parse_watch_band_choices,
        parse_watch_band_sizes,
    };
    use apw_core::model::Target;

    #[test]
    fn apple地区响应过滤占位项并保留三级选项() {
        let response = r#"{
          "body": {
            "state": {"data":[{"text":"省份","value":""},{"text":"江苏","value":"江苏"}]},
            "city": {"data":[{"text":"城镇/城市","value":""},{"text":"苏州","value":"苏州"}]},
            "district": {"data":[{"text":"区","value":""},{"text":"吴江区","value":"吴江区"}]}
          }
        }"#;
        let parsed =
            parse_delivery_localities(response.as_bytes()).expect("应当解析 Apple 地区响应");
        assert_eq!(parsed.states[0].value, "江苏");
        assert_eq!(parsed.cities[0].value, "苏州");
        assert_eq!(parsed.districts[0].value, "吴江区");
        assert_eq!(parsed.states.len(), 1);
    }

    #[test]
    fn watch到货提醒明确标出送货查询使用的表带() {
        let target = Target {
            locale: "zh_CN".into(),
            store_number: "R390".into(),
            store_title: "上海-香港广场".into(),
            part_number: "MJCX4CH/B".into(),
            product_name: "Apple Watch Ultra 4 49 毫米 黑色".into(),
            companion_part: Some("MKDY4FE/A".into()),
            companion_name: None,
            kit_part: Some("Z0YQ".into()),
        };
        let body = in_stock_notification_body(&target);
        assert!(body.contains("Apple Watch Ultra 4"));
        assert!(body.contains("Watch 送货搭配表带 MKDY4FE/A"));
    }

    #[test]
    fn apple表带接口保留官网款式颜色和精确尺码零件号() {
        let choices = r#"{"body":{"items":{"sololoop":{"sortOrder":20,"sectionHeader":"<span>新配色</span> 单圈表带","subDimensionValue":[{"dimensionValue":"burgundy","text":"勃艮第酒红色","sortOrder":10}]}}}}"#;
        let parsed = parse_watch_band_choices(choices.as_bytes()).expect("应当解析表带选项");
        assert_eq!(parsed[0].style_key, "sololoop");
        assert_eq!(parsed[0].style_name, "新配色 单圈表带");
        assert_eq!(parsed[0].color_name, "勃艮第酒红色");

        let sizes = r#"{"body":{"options":[{"key":"6","text":"6","product":{"options":{"watch_bands":"MKJP4FE/A"}}}]}}"#;
        let parsed = parse_watch_band_sizes(sizes.as_bytes()).expect("应当解析表带尺码");
        assert_eq!(parsed[0].text, "6");
        assert_eq!(parsed[0].part_number, "MKJP4FE/A");
    }
}
