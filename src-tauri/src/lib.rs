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
use serde::Serialize;
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
    state
        .catalog
        .refresh_products(region, category, &state.http)
        .await
        .map_err(|e| e.to_string())
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

    // 设置里的目标列表和查询间隔要同步给引擎，否则改完设置监控还按旧的跑。
    state.watcher.set_targets(next.targets.clone()).await;
    state.watcher.set_interval(next.interval()).await;

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
    state.watcher.set_targets(targets.clone()).await;
    let mut next = state.settings_snapshot();
    next.targets = targets;
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
    if !settings.sound_enabled
        && settings.bark_url.trim().is_empty()
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
        && settings.bark_url.trim().is_empty()
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
        && settings.bark_url.trim().is_empty()
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
    dispatch_notification(&app, notification)
        .await
        .map_err(|e| e.to_string())
}

/// 按用户设置发送提示音和 Bark 提醒。
async fn dispatch_notification(
    app: &AppHandle,
    notification: Notification,
) -> Result<(), apw_core::notify::NotifyError> {
    let settings = match app.try_state::<AppState>() {
        Some(state) => state.settings_snapshot(),
        None => return Ok(()),
    };

    let mut channels = Multi::new();
    if settings.sound_enabled {
        channels.push(Sound::embedded());
    }
    if !settings.bark_url.trim().is_empty() {
        let http = app
            .try_state::<AppState>()
            .map(|s| s.http.clone())
            .unwrap_or_default();
        // Bark 每次现构造：地址是用户随时可改的设置项，缓存实例会在改完地址后
        // 继续往旧地址推。共享的 http 客户端一并传进去，连接池仍然复用。
        channels.push(Bark::new(settings.bark_url.clone(), http));
    }
    if channels.is_empty() {
        return Ok(());
    }
    channels.notify(&notification).await
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
            let mut notification = Notification::new(
                "有货了",
                format!("{} {}", target.store_title, target.product_name),
            );
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

            if let Err(err) = dispatch_notification(&app, notification).await {
                // 提醒没发出去是遗憾，但绝不能让监控本身停下来。
                let _ = app.emit(NOTICE_CHANNEL, format!("发送提醒时出错：{err}"));
            } else {
                let mut actions = Vec::new();
                if settings.sound_enabled {
                    actions.push("提示音");
                }
                if !settings.bark_url.trim().is_empty() {
                    actions.push("Bark");
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
            let (settings, store) = load_settings(&mut notices);

            // 用 Watcher::new 而不是 Watcher::spawn：setup 回调跑在主线程上，
            // 并不处在 tokio 运行时上下文里，在这里 tokio::spawn 会 panic，
            // 而且因为发生在不可展开的回调中，进程会直接 abort。
            // 引擎任务交给 Tauri 自己的运行时去驱动。
            let (watcher, events, engine) =
                Watcher::new(AppleChromiumFetcher::new(), WatcherConfig::default());
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
                catalog: Catalog::new(),
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
            is_running,
            test_notify,
            open_target_product,
            check_for_update,
            install_update,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri 应用启动失败");
}
