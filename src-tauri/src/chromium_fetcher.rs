//! 通过独立的无界面 Chromium 会话查询 Apple 库存。
//!
//! Apple 当前会在商品页执行 `shop/shld/v2_1/verify.js`，完成浏览器环境校验后才
//! 接受库存请求。普通 HTTP 客户端或 WKWebView 即便拿到了部分 Cookie，仍会收到
//! HTTP 541；真正的 Chromium 会话则能得到正常 JSON。这里启动一个使用临时资料
//! 目录的后台浏览器，通过 DevTools 协议复用同一会话查询所有门店。
//!
//! 这个实现不会读取用户现有 Chrome 的个人资料、Cookie 或浏览记录。临时目录随
//! 会话销毁，浏览器进程也由应用持有并在退出时终止。

use std::collections::HashMap;
#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apw_core::apple::{
    ApiError, CycleStats, Fetcher, ScheduleHint, StoreAvailability, parse_pickup_message,
};
use apw_core::model::{DeliveryRegion, Region, Target};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CHROME_START_TIMEOUT: Duration = Duration::from_secs(12);
const DEVTOOLS_HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const DEVTOOLS_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_READY_TIMEOUT: Duration = Duration::from_secs(50);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 4 << 20;
const FALLBACK_CHROMIUM_MAJOR: u32 = 152;
const MAX_EXCEPTION_SUMMARY_CHARS: usize = 160;
const DELIVERY_CACHE_TTL: Duration = Duration::from_secs(60);
const REQUEST_BUDGET_CAPACITY: u32 = 20;
const REQUEST_BUDGET_REFILL: Duration = Duration::from_secs(60);
const PROBE_RETRY_AFTER_TRANSIENT_ERROR: Duration = Duration::from_secs(60);
const COOLDOWN_STEPS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DebugTarget {
    #[serde(rename = "type")]
    kind: String,
    web_socket_debugger_url: Option<String>,
}

#[derive(Debug)]
struct ChromiumSession {
    child: Child,
    _profile: TempDir,
    socket: Socket,
    next_command_id: u64,
    locale: Option<&'static str>,
    delivery_cache: HashMap<String, (Instant, Option<String>)>,
}

impl Drop for ChromiumSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl ChromiumSession {
    async fn start() -> Result<Self, ApiError> {
        let chrome = find_chromium().ok_or_else(|| {
            ApiError::Transport(
                "未找到 Google Chrome 或 Microsoft Edge；Apple 当前库存接口要求完整 Chromium 浏览器会话"
                    .into(),
            )
        })?;
        // Windows 上直接执行 `chrome.exe --version` 可能不会退出。旧实现用
        // `Command::output()` 同步等待，整个查询任务因此永久卡在会话启动阶段。
        // 版本号只用于隐藏 HeadlessChrome 标记，不值得为它启动第二个浏览器进程；
        // 使用随版本维护的兼容 UA，彻底移除这个无界等待点。
        let user_agent = chromium_user_agent();
        let profile = tempfile::Builder::new()
            .prefix("apple-store-inventory-monitor-chromium-")
            .tempdir()
            .map_err(|e| ApiError::Transport(format!("无法创建 Chromium 临时目录：{e}")))?;
        let profile_arg = format!("--user-data-dir={}", profile.path().display());
        let user_agent_arg = format!("--user-agent={user_agent}");
        let mut child = Command::new(chrome)
            .args([
                "--headless=new",
                "--remote-debugging-port=0",
                profile_arg.as_str(),
                user_agent_arg.as_str(),
                "--disable-blink-features=AutomationControlled",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-sync",
                "--disable-default-apps",
                "--disable-extensions",
                "about:blank",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ApiError::Transport(format!("无法启动 Chromium：{e}")))?;

        let port_file = profile.path().join("DevToolsActivePort");
        let deadline = Instant::now() + CHROME_START_TIMEOUT;
        let port = loop {
            if let Ok(contents) = std::fs::read_to_string(&port_file)
                && let Some(line) = contents.lines().next()
                && let Ok(port) = line.parse::<u16>()
            {
                break port;
            }
            if let Some(status) = child
                .try_wait()
                .map_err(|e| ApiError::Transport(format!("无法检查 Chromium 状态：{e}")))?
            {
                return Err(ApiError::Transport(format!(
                    "Chromium 启动后立即退出：{status}"
                )));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                return Err(ApiError::Transport("等待 Chromium 启动超时".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };

        let targets_url = format!("http://127.0.0.1:{port}/json/list");
        let targets: Vec<DebugTarget> = tokio::time::timeout(DEVTOOLS_HTTP_TIMEOUT, async {
            reqwest::get(&targets_url)
                .await
                .map_err(|e| ApiError::Transport(format!("无法连接 Chromium 调试端口：{e}")))?
                .json()
                .await
                .map_err(|e| ApiError::Transport(format!("Chromium 目标列表无法解析：{e}")))
        })
        .await
        .map_err(|_| ApiError::Transport("读取 Chromium 页面目标超时".into()))??;
        let socket_url = targets
            .into_iter()
            .find(|target| target.kind == "page")
            .and_then(|target| target.web_socket_debugger_url)
            .ok_or_else(|| ApiError::Transport("Chromium 没有可用页面目标".into()))?;
        let (socket, _) = tokio::time::timeout(DEVTOOLS_SOCKET_TIMEOUT, connect_async(&socket_url))
            .await
            .map_err(|_| ApiError::Transport("连接 Chromium 调试 WebSocket 超时".into()))?
            .map_err(|e| ApiError::Transport(format!("无法连接 Chromium 页面：{e}")))?;

        Ok(Self {
            child,
            _profile: profile,
            socket,
            next_command_id: 1,
            locale: None,
            delivery_cache: HashMap::new(),
        })
    }

    async fn command(&mut self, method: &str, params: Value) -> Result<Value, ApiError> {
        let id = self.next_command_id;
        self.next_command_id = self.next_command_id.wrapping_add(1).max(1);
        let request = json!({ "id": id, "method": method, "params": params });
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            self.socket.send(Message::Text(request.to_string().into())),
        )
        .await
        .map_err(|_| ApiError::Transport(format!("发送 Chromium 命令 {method} 超时")))?
        .map_err(|e| ApiError::Transport(format!("发送 Chromium 命令失败：{e}")))?;

        let wait = async {
            while let Some(message) = self.socket.next().await {
                let message = message
                    .map_err(|e| ApiError::Transport(format!("读取 Chromium 响应失败：{e}")))?;
                let Message::Text(text) = message else {
                    continue;
                };
                let response: Value = serde_json::from_str(&text)
                    .map_err(|e| ApiError::Transport(format!("Chromium 响应无法解析：{e}")))?;
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = response.get("error") {
                    return Err(ApiError::Transport(format!(
                        "Chromium 命令 {method} 失败：{error}"
                    )));
                }
                return Ok(response.get("result").cloned().unwrap_or(Value::Null));
            }
            Err(ApiError::Transport("Chromium 调试连接意外关闭".into()))
        };

        tokio::time::timeout(COMMAND_TIMEOUT, wait)
            .await
            .map_err(|_| ApiError::Transport(format!("Chromium 命令 {method} 超时")))?
    }

    async fn evaluate(&mut self, expression: &str, await_promise: bool) -> Result<Value, ApiError> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": await_promise,
                    "returnByValue": true
                }),
            )
            .await?;
        if let Some(details) = result.get("exceptionDetails") {
            // DevTools 的 exceptionDetails 带着 className、objectId 和整段 stack。
            // 这些内容适合开发诊断，不适合直接铺到用户的活动日志里。
            eprintln!("Chromium Runtime.evaluate exceptionDetails: {details}");
            return Err(ApiError::Transport(chromium_exception_summary(details)));
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn ensure_region(&mut self, region: &'static Region) -> Result<(), ApiError> {
        if self.locale == Some(region.locale) {
            return Ok(());
        }

        // 不能用 `/shop/product/{part}` 暖场：Apple Watch 的配置零件号并不一定
        // 有独立商品详情页，例如 MFA04CH/B 当前直接返回 404。旧实现随后仍会等满
        // 50 秒，并且每家门店各等一次，界面看起来就像点击后完全没反应。
        //
        // 改用内置目录里的正式购买页。它与库存接口属于同一个在线商店会话，且
        // URL 会随在售产品目录一起维护；会话一旦建立，同地区的 iPhone、Watch、
        // iPad 和 Mac 库存请求都可以复用。
        let family = region
            .families
            .first()
            .ok_or_else(|| ApiError::Transport("当前地区没有可用于建立会话的购买页".into()))?;
        let page_url = region.buy_page_url(family);
        self.command("Page.navigate", json!({ "url": page_url }))
            .await?;
        let deadline = Instant::now() + SESSION_READY_TIMEOUT;
        loop {
            let state = self
                .evaluate(
                    r#"JSON.stringify({readyState:document.readyState,cookies:document.cookie.split(';').map(x=>x.trim().split('=')[0]).filter(Boolean)})"#,
                    false,
                )
                .await?;
            if let Some(raw) = state.as_str()
                && let Ok(state) = serde_json::from_str::<ReadyState>(raw)
                && state.ready_state != "loading"
                && state.cookies.iter().any(|name| name == "shld_bt_ck")
                && state.cookies.iter().any(|name| name == "as_atb")
            {
                self.locale = Some(region.locale);
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ApiError::Blocked("Apple 页面未能完成 shld 风控握手".into()));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn fetch(
        &mut self,
        region: &'static Region,
        store_number: &str,
        parts: &[String],
        delivery_region: Option<&DeliveryRegion>,
        gate: &mut RequestGate,
    ) -> Result<BrowserPayload, ApiError> {
        self.ensure_region(region).await?;
        gate.acquire().await;

        let mut pairs = vec![
            ("fae".to_string(), "true".to_string()),
            ("pl".to_string(), "true".to_string()),
            ("mts.0".to_string(), "regular".to_string()),
            ("searchNearby".to_string(), "true".to_string()),
        ];
        pairs.extend(
            parts
                .iter()
                .enumerate()
                .map(|(index, part)| (format!("parts.{index}"), part.clone())),
        );
        pairs.push(("store".to_string(), store_number.to_string()));
        if let Some(location) = delivery_region {
            pairs.extend([
                ("state".to_string(), location.state.clone()),
                ("city".to_string(), location.city.clone()),
                ("district".to_string(), location.district.clone()),
            ]);
        }

        #[derive(Serialize)]
        struct BrowserRequest<'a> {
            url: String,
            pairs: &'a [(String, String)],
            max_bytes: usize,
        }
        let request = serde_json::to_string(&BrowserRequest {
            url: region.pickup_message_url(),
            pairs: &pairs,
            max_bytes: MAX_RESPONSE_BYTES,
        })
        .map_err(|e| ApiError::Transport(format!("无法编码库存请求：{e}")))?;
        let expression = format!(
            r#"(async()=>{{
                const request={request};
                const url=new URL(request.url);
                for(const [key,value] of request.pairs) url.searchParams.append(key,value);
                const response=await fetch(url.toString(),{{
                    credentials:'same-origin',
                    headers:{{
                        'Accept':'application/json, text/javascript, */*; q=0.01',
                        'X-Requested-With':'XMLHttpRequest'
                    }}
                }});
                const body=await response.text();
                const bytes=new TextEncoder().encode(body).length;
                if(bytes>request.max_bytes) return {{status:0,body:'Apple 响应超过 4 MiB 安全上限'}};
                return {{status:response.status,body}};
            }})()"#
        );
        let value = self.evaluate(&expression, true).await?;
        serde_json::from_value(value)
            .map_err(|e| ApiError::Transport(format!("Chromium 库存结果无法解析：{e}")))
    }

    async fn fetch_watch_delivery(
        &mut self,
        region: &'static Region,
        target: &Target,
        location: &DeliveryRegion,
        gate: &mut RequestGate,
    ) -> Result<Option<String>, ApiError> {
        let (Some(kit), Some(companion)) = (
            target
                .kit_part
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
            target
                .companion_part
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ) else {
            return Ok(None);
        };
        let key = format!(
            "{}|{}|{}|{}|{}|{}|{}",
            region.locale,
            kit,
            target.part_number,
            companion,
            location.state,
            location.city,
            location.district
        );
        if let Some((at, message)) = self.delivery_cache.get(&key)
            && at.elapsed() < DELIVERY_CACHE_TTL
        {
            return Ok(message.clone());
        }

        gate.acquire().await;

        let pairs = vec![
            ("fae".to_string(), "true".to_string()),
            ("pl".to_string(), "true".to_string()),
            ("fts".to_string(), "true".to_string()),
            ("mts.0".to_string(), "expanded".to_string()),
            ("parts.0".to_string(), kit.to_string()),
            (
                "option.0".to_string(),
                format!("{},{}", target.part_number, companion),
            ),
            ("state".to_string(), location.state.clone()),
            ("city".to_string(), location.city.clone()),
            ("district".to_string(), location.district.clone()),
        ];

        #[derive(Serialize)]
        struct BrowserRequest<'a> {
            url: String,
            pairs: &'a [(String, String)],
            max_bytes: usize,
        }
        let request = serde_json::to_string(&BrowserRequest {
            url: region.pickup_message_url(),
            pairs: &pairs,
            max_bytes: MAX_RESPONSE_BYTES,
        })
        .map_err(|e| ApiError::Transport(format!("无法编码 Watch 送货请求：{e}")))?;
        let expression = format!(
            r#"(async()=>{{
                const request={request};
                const url=new URL(request.url);
                for(const [key,value] of request.pairs) url.searchParams.append(key,value);
                const response=await fetch(url.toString(),{{
                    credentials:'same-origin',
                    headers:{{'Accept':'application/json, text/javascript, */*; q=0.01','X-Requested-With':'XMLHttpRequest'}}
                }});
                const body=await response.text();
                const bytes=new TextEncoder().encode(body).length;
                if(bytes>request.max_bytes) return {{status:0,body:'Apple 响应超过 4 MiB 安全上限'}};
                return {{status:response.status,body}};
            }})()"#
        );
        let value = self.evaluate(&expression, true).await?;
        let payload: BrowserPayload = serde_json::from_value(value)
            .map_err(|e| ApiError::Transport(format!("Chromium Watch 送货结果无法解析：{e}")))?;
        let message = match payload.status {
            200 => parse_delivery_display_name(payload.body.as_bytes())?,
            403 | 541 => return Err(ApiError::Blocked(format!("HTTP {}", payload.status))),
            429 => return Err(ApiError::RateLimited("HTTP 429".into())),
            status if status >= 500 => return Err(ApiError::RateLimited(format!("HTTP {status}"))),
            0 => {
                return Err(ApiError::Transport(
                    payload.body.chars().take(300).collect(),
                ));
            }
            status => return Err(ApiError::Transport(format!("HTTP {status}"))),
        };
        self.delivery_cache
            .insert(key, (Instant::now(), message.clone()));
        Ok(message)
    }
}

fn parse_delivery_display_name(raw: &[u8]) -> Result<Option<String>, ApiError> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| ApiError::SchemaDrift {
        field: "body.content.deliveryMessage".into(),
        raw: format!("Watch 送货响应不是 JSON：{e}"),
    })?;
    fn find(value: &Value) -> Option<String> {
        if let Some(message) = value
            .get("deliveryOptionMessages")
            .and_then(Value::as_array)
            .and_then(|messages| messages.first())
            .and_then(|message| message.get("displayName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|message| !message.is_empty())
        {
            return Some(message.to_string());
        }
        match value {
            Value::Array(items) => items.iter().find_map(find),
            Value::Object(fields) => fields.values().find_map(find),
            _ => None,
        }
    }
    Ok(value
        .pointer("/body/content/deliveryMessage")
        .and_then(find))
}

fn chromium_exception_summary(details: &Value) -> String {
    let raw = details
        .pointer("/exception/description")
        .and_then(Value::as_str)
        .or_else(|| details.pointer("/exception/value").and_then(Value::as_str))
        .or_else(|| details.get("text").and_then(Value::as_str))
        .unwrap_or_default();
    let first_line = raw
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let normalized = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = normalized.to_ascii_lowercase();

    if lower.contains("access is denied")
        || lower.contains("blocked a frame with origin")
        || lower.contains("permission denied to access property")
    {
        return "浏览器安全限制阻止了本次 Apple 页面访问".into();
    }

    if normalized.is_empty() {
        return "浏览器会话未能完成本次 Apple 查询".into();
    }

    let mut summary: String = normalized
        .chars()
        .take(MAX_EXCEPTION_SUMMARY_CHARS)
        .collect();
    if normalized.chars().count() > MAX_EXCEPTION_SUMMARY_CHARS {
        summary.push('…');
    }
    format!("浏览器页面执行失败：{summary}")
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadyState {
    ready_state: String,
    cookies: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct BrowserPayload {
    status: u16,
    body: String,
}

fn find_chromium() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    const ABSOLUTE_CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ];

    #[cfg(target_os = "windows")]
    const EXECUTABLE_NAMES: &[&str] = &["chrome.exe", "msedge.exe"];
    #[cfg(target_os = "macos")]
    const EXECUTABLE_NAMES: &[&str] = &["google-chrome", "microsoft-edge"];
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    const EXECUTABLE_NAMES: &[&str] = &[
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
        "microsoft-edge",
    ];

    #[cfg(target_os = "macos")]
    if let Some(path) = ABSOLUTE_CANDIDATES
        .iter()
        .map(Path::new)
        .find(|path| path.is_file())
        .map(Path::to_path_buf)
    {
        return Some(path);
    }

    #[cfg(target_os = "windows")]
    {
        const WINDOWS_LOCATIONS: &[(&str, &str)] = &[
            ("PROGRAMFILES", "Google/Chrome/Application/chrome.exe"),
            ("PROGRAMFILES", "Microsoft/Edge/Application/msedge.exe"),
            ("PROGRAMFILES(X86)", "Google/Chrome/Application/chrome.exe"),
            ("PROGRAMFILES(X86)", "Microsoft/Edge/Application/msedge.exe"),
            ("LOCALAPPDATA", "Google/Chrome/Application/chrome.exe"),
            ("LOCALAPPDATA", "Microsoft/Edge/Application/msedge.exe"),
        ];
        if let Some(path) = WINDOWS_LOCATIONS.iter().find_map(|(variable, suffix)| {
            let root = std::env::var_os(variable)?;
            let path = PathBuf::from(root).join(suffix);
            path.is_file().then_some(path)
        }) {
            return Some(path);
        }
    }

    let search_path = std::env::var_os("PATH")?;
    std::env::split_paths(&search_path).find_map(|directory| {
        EXECUTABLE_NAMES
            .iter()
            .map(|name| directory.join(name))
            .find(|path| path.is_file())
    })
}

fn chromium_user_agent() -> String {
    format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{FALLBACK_CHROMIUM_MAJOR}.0.0.0 Safari/537.36"
    )
}

/// 可交给核心监控引擎的 Chromium 查询器。
#[derive(Debug, Clone)]
pub struct AppleChromiumFetcher {
    state: Arc<Mutex<QueryState>>,
}

type PickupCacheKey = (&'static str, Vec<String>, Option<DeliveryRegion>);

#[derive(Debug)]
struct RequestBudget {
    tokens: f64,
    updated: Instant,
}

impl RequestBudget {
    fn new(now: Instant) -> Self {
        Self {
            tokens: f64::from(REQUEST_BUDGET_CAPACITY),
            updated: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated);
        let gained = elapsed.as_secs_f64() / REQUEST_BUDGET_REFILL.as_secs_f64();
        self.tokens = (self.tokens + gained).min(f64::from(REQUEST_BUDGET_CAPACITY));
        self.updated = now;
    }

    /// 只计算等待时间，不预约、不扣额度。等待 future 被取消时不会留下虚假欠账。
    fn available_in(&mut self, now: Instant) -> Duration {
        self.available_for_in(now, 1)
    }

    /// 准备好一整轮预计请求数还要多久。这里只做估算，不提前扣额度。
    fn available_for_in(&mut self, now: Instant, requests: u32) -> Duration {
        self.refill(now);
        let shortfall = f64::from(requests) - self.tokens;
        if shortfall <= 0.0 {
            Duration::ZERO
        } else {
            REQUEST_BUDGET_REFILL.mul_f64(shortfall)
        }
    }

    fn take(&mut self, now: Instant) -> bool {
        self.refill(now);
        if self.tokens < 1.0 {
            return false;
        }
        self.tokens -= 1.0;
        true
    }
}

#[derive(Debug)]
struct RequestGate {
    budget: RequestBudget,
    last_sent: Option<Instant>,
    cycle_request_count: u32,
}

impl Default for RequestGate {
    fn default() -> Self {
        Self {
            budget: RequestBudget::new(Instant::now()),
            last_sent: None,
            cycle_request_count: 0,
        }
    }
}

impl RequestGate {
    fn next_delay(&mut self, now: Instant) -> Duration {
        let interval = self
            .last_sent
            .map(|last| (last + MIN_REQUEST_INTERVAL).saturating_duration_since(now))
            .unwrap_or_default();
        interval.max(self.budget.available_in(now))
    }

    fn next_cycle_delay(&mut self, now: Instant) -> Duration {
        let expected_requests = self.cycle_request_count.max(1);
        let interval = self
            .last_sent
            .map(|last| (last + MIN_REQUEST_INTERVAL).saturating_duration_since(now))
            .unwrap_or_default();
        interval.max(self.budget.available_for_in(now, expected_requests))
    }

    async fn acquire(&mut self) {
        loop {
            let now = Instant::now();
            let delay = self.next_delay(now);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
                continue;
            }

            // 额度只在即将进入真实 fetch 前扣除。若监控在上面的等待期间暂停，
            // future 会直接被丢弃，这里不会运行，也就不会产生上游实现的虚假欠账。
            let now = Instant::now();
            if !self.budget.take(now) {
                continue;
            }
            self.last_sent = Some(now);
            self.cycle_request_count = self.cycle_request_count.saturating_add(1);
            return;
        }
    }
}

#[derive(Debug, Clone)]
struct CooldownState {
    until: Instant,
    level: usize,
    detail: String,
    announce: bool,
}

#[derive(Debug, Default)]
struct QueryState {
    session: Option<ChromiumSession>,
    pickup_cache: HashMap<PickupCacheKey, String>,
    cooldowns: HashMap<&'static str, CooldownState>,
    gate: RequestGate,
    cycle_reused_response_count: u32,
}

impl QueryState {
    fn begin_cycle(&mut self) {
        self.pickup_cache.clear();
        self.gate.cycle_request_count = 0;
        self.cycle_reused_response_count = 0;
    }

    fn active_cooldown_error(&mut self, region: &'static Region) -> Option<ApiError> {
        let now = Instant::now();
        let state = self.cooldowns.get_mut(region.locale)?;
        let remaining = state.until.saturating_duration_since(now);
        if remaining.is_zero() {
            return None;
        }
        let newly_started = std::mem::take(&mut state.announce);
        Some(ApiError::CoolingDown {
            remaining_seconds: remaining
                .as_secs()
                .saturating_add(u64::from(remaining.subsec_nanos() > 0)),
            detail: state.detail.clone(),
            newly_started,
        })
    }

    fn start_or_escalate_cooldown(
        &mut self,
        region: &'static Region,
        detail: impl Into<String>,
    ) -> ApiError {
        let now = Instant::now();
        let level = self
            .cooldowns
            .get(region.locale)
            .map(|state| {
                if now >= state.until {
                    state.level.saturating_add(1)
                } else {
                    state.level
                }
            })
            .unwrap_or(0)
            .min(COOLDOWN_STEPS.len() - 1);
        self.cooldowns.insert(
            region.locale,
            CooldownState {
                until: now + COOLDOWN_STEPS[level],
                level,
                detail: detail.into(),
                announce: true,
            },
        );
        self.pickup_cache.retain(|key, _| key.0 != region.locale);
        self.active_cooldown_error(region)
            .expect("刚建立的冷却应当立即生效")
    }

    /// 冷却结束后的唯一探测若遇到普通网络错误，不升档，但短暂挡住本轮其余门店。
    fn defer_after_transient_probe_error(&mut self, region: &'static Region, detail: &str) {
        let now = Instant::now();
        if let Some(state) = self.cooldowns.get_mut(region.locale)
            && now >= state.until
        {
            state.until = now + PROBE_RETRY_AFTER_TRANSIENT_ERROR;
            state.detail = format!("恢复探测暂时失败：{detail}");
            state.announce = false;
        }
    }

    fn record_success(&mut self, region: &'static Region) {
        self.cooldowns.remove(region.locale);
    }

    fn schedule_hint(&mut self, locales: &[String]) -> ScheduleHint {
        let now = Instant::now();
        // 以上一轮实际产生的网络请求数估算下一轮，而不是只等到够发第一条就开跑。
        // 否则预算不足时，界面会长时间停在“正在查询”，其余请求仍在轮内排队。
        let mut delay = self.gate.next_cycle_delay(now);

        if !locales.is_empty() {
            let mut all_cooling = true;
            let mut earliest = None::<Duration>;
            for locale in locales {
                let remaining = self
                    .cooldowns
                    .get(locale.as_str())
                    .map(|state| state.until.saturating_duration_since(now))
                    .filter(|remaining| !remaining.is_zero());
                match remaining {
                    Some(remaining) => {
                        earliest = Some(earliest.map_or(remaining, |old| old.min(remaining)));
                    }
                    None => all_cooling = false,
                }
            }
            if all_cooling && let Some(cooldown) = earliest {
                delay = delay.max(cooldown);
            }
        }

        ScheduleHint { delay }
    }
}

impl AppleChromiumFetcher {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueryState::default())),
        }
    }

    async fn pickup(
        &self,
        region: &'static Region,
        store_number: &str,
        targets: &[Target],
        delivery_region: Option<&DeliveryRegion>,
    ) -> Result<StoreAvailability, ApiError> {
        if store_number.is_empty() {
            return Err(ApiError::Transport("门店编号为空".into()));
        }
        if targets.is_empty() {
            return Err(ApiError::Transport("零件号列表为空".into()));
        }
        let mut parts: Vec<String> = targets
            .iter()
            .map(|target| target.part_number.clone())
            .collect();
        for companion in targets
            .iter()
            .filter_map(|target| target.companion_part.as_ref())
        {
            if !parts.contains(companion) {
                parts.push(companion.clone());
            }
        }

        parts.sort_unstable();
        parts.dedup();
        let can_reuse_nearby = targets
            .iter()
            .all(|target| target.companion_part.is_none() && target.kit_part.is_none());
        let cache_key = (region.locale, parts.clone(), delivery_region.cloned());

        // 一把锁覆盖整个浏览器命令往返。监控引擎可以并发调多个门店，但同一个
        // DevTools 连接与 Apple 会话必须串行使用，避免请求突发再次触发 541。
        let mut guard = self.state.lock().await;
        if let Some(error) = guard.active_cooldown_error(region) {
            return Err(error);
        }
        if can_reuse_nearby
            && let Some(body) = guard.pickup_cache.get(&cache_key)
            && let Ok(availability) = parse_pickup_message(body.as_bytes(), store_number)
            && parts
                .iter()
                .all(|part| availability.parts.contains_key(part))
        {
            guard.cycle_reused_response_count = guard.cycle_reused_response_count.saturating_add(1);
            return Ok(availability);
        }
        if guard.session.is_none() {
            guard.session = Some(ChromiumSession::start().await?);
        }
        let fetched = {
            let QueryState { session, gate, .. } = &mut *guard;
            session
                .as_mut()
                .expect("刚初始化的 Chromium 会话应当存在")
                .fetch(region, store_number, &parts, delivery_region, gate)
                .await
        };
        let payload = match fetched {
            Ok(payload) => payload,
            Err(error) => {
                // CDP 断连、命令超时或页面执行失败后，不再缓存这个失效会话。
                // 保留原错误交给引擎处理，后续查询才重建，不在失败请求内重试。
                // Apple 的 HTTP 限流和库存数据仍走下面原有的分类逻辑。
                if matches!(error, ApiError::Transport(_)) {
                    guard.session = None;
                    guard.defer_after_transient_probe_error(region, &error.to_string());
                } else if matches!(error, ApiError::Blocked(_) | ApiError::RateLimited(_)) {
                    guard.session = None;
                    return Err(guard.start_or_escalate_cooldown(region, error.to_string()));
                }
                return Err(error);
            }
        };

        match payload.status {
            200 => {
                let bytes = payload.body.as_bytes();
                if bytes
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    .is_some_and(|byte| *byte != b'{' && *byte != b'[')
                {
                    guard.session = None;
                    return Err(
                        guard.start_or_escalate_cooldown(region, "HTTP 200 但响应不是 JSON")
                    );
                }
                let mut availability = match parse_pickup_message(bytes, store_number) {
                    Ok(availability) => availability,
                    Err(error @ (ApiError::Blocked(_) | ApiError::RateLimited(_))) => {
                        guard.session = None;
                        return Err(guard.start_or_escalate_cooldown(region, error.to_string()));
                    }
                    Err(error) => {
                        guard.defer_after_transient_probe_error(region, &error.to_string());
                        return Err(error);
                    }
                };
                guard.record_success(region);
                if can_reuse_nearby {
                    guard.pickup_cache.insert(cache_key, payload.body.clone());
                }
                if let Some(location) = delivery_region {
                    for target in targets.iter().filter(|target| target.kit_part.is_some()) {
                        let delivery = {
                            let QueryState { session, gate, .. } = &mut *guard;
                            session
                                .as_mut()
                                .expect("查询期间 Chromium 会话应当存在")
                                .fetch_watch_delivery(region, target, location, gate)
                                .await
                        };
                        match delivery {
                            Ok(Some(message)) => {
                                if let Some(status) =
                                    availability.parts.get_mut(&target.part_number)
                                    && let Some(details) = status.pickup_details.as_mut()
                                {
                                    details.sale_message = Some(message);
                                }
                            }
                            Ok(None) => {}
                            Err(error) => {
                                // 送货是附加信息，失败不能抹掉已经拿到的门店库存结论。
                                // 保留取货响应里的粗略文案，下轮自动重试精确送货。
                                eprintln!("Watch 送货查询失败（{}）：{error}", target.part_number);
                                match error {
                                    ApiError::Blocked(_) | ApiError::RateLimited(_) => {
                                        guard.start_or_escalate_cooldown(region, error.to_string());
                                    }
                                    ApiError::Transport(_) => {
                                        guard.session = None;
                                        guard.defer_after_transient_probe_error(
                                            region,
                                            &error.to_string(),
                                        );
                                    }
                                    _ => {}
                                }
                                break;
                            }
                        }
                    }
                }
                Ok(availability)
            }
            403 | 541 => {
                // 当前浏览器会话已经被拒绝。直接销毁临时资料与进程；下一轮会用
                // 全新会话重新握手，不拿失效 Cookie 反复撞接口。
                guard.session = None;
                Err(guard.start_or_escalate_cooldown(region, format!("HTTP {}", payload.status)))
            }
            429 => Err(guard.start_or_escalate_cooldown(region, "HTTP 429")),
            status if status >= 500 => {
                let error = ApiError::Transport(format!("Apple 服务暂时异常：HTTP {status}"));
                guard.defer_after_transient_probe_error(region, &error.to_string());
                Err(error)
            }
            0 => {
                let error = ApiError::Transport(payload.body.chars().take(300).collect());
                guard.session = None;
                guard.defer_after_transient_probe_error(region, &error.to_string());
                Err(error)
            }
            status => Err(ApiError::Transport(format!("HTTP {status}"))),
        }
    }
}

impl Fetcher for AppleChromiumFetcher {
    async fn begin_cycle(&self) {
        self.state.lock().await.begin_cycle();
    }

    async fn cycle_stats(&self) -> CycleStats {
        let state = self.state.lock().await;
        CycleStats {
            request_count: state.gate.cycle_request_count,
            reused_response_count: state.cycle_reused_response_count,
        }
    }

    async fn schedule_hint(&self, locales: &[String]) -> ScheduleHint {
        self.state.lock().await.schedule_hint(locales)
    }

    async fn pickup_message(
        &self,
        region: &'static Region,
        store_number: &str,
        targets: &[Target],
        delivery_region: Option<&DeliveryRegion>,
    ) -> Result<StoreAvailability, ApiError> {
        self.pickup(region, store_number, targets, delivery_region)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apw_core::model::region_by_locale;

    fn test_target(part: &str) -> Target {
        Target {
            locale: "zh_CN".into(),
            store_number: "R390".into(),
            store_title: "上海-香港广场".into(),
            part_number: part.into(),
            product_name: part.into(),
            companion_part: None,
            companion_name: None,
            kit_part: None,
        }
    }

    /// 模拟浏览器的 CDP 边界，不启动 Chromium，也不访问 Apple。
    async fn cdp_fixture(response: Value) -> (AppleChromiumFetcher, tokio::task::JoinHandle<bool>) {
        cdp_responses(vec![response]).await
    }

    async fn cdp_responses(
        responses: Vec<Value>,
    ) -> (AppleChromiumFetcher, tokio::task::JoinHandle<bool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            for mut response in responses {
                let request = tokio::time::timeout(Duration::from_secs(5), socket.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
                response["id"] = request["id"].clone();
                socket
                    .send(Message::Text(response.to_string().into()))
                    .await
                    .unwrap();
            }
            // 观察浏览器一端的连接生命周期，不依赖查询器内部的 Option 状态。
            matches!(
                tokio::time::timeout(Duration::from_millis(500), socket.next()).await,
                Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_))))
            )
        });
        let (socket, _) = connect_async(format!("ws://{address}")).await.unwrap();
        let session = ChromiumSession {
            child: Command::new("rustc")
                .arg("--version")
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
            _profile: tempfile::tempdir().unwrap(),
            socket,
            next_command_id: 1,
            locale: Some("zh_CN"),
            delivery_cache: HashMap::new(),
        };
        (
            AppleChromiumFetcher {
                state: Arc::new(Mutex::new(QueryState {
                    session: Some(session),
                    ..QueryState::default()
                })),
            },
            peer,
        )
    }

    fn pickup_response(stores: &[&str], part: &str, display: &str) -> Value {
        let stores: Vec<_> = stores
            .iter()
            .map(|store| {
                json!({
                    "storeNumber": store,
                    "partsAvailability": {
                        part: {"partNumber": part, "pickupDisplay": display}
                    }
                })
            })
            .collect();
        json!({"result":{"result":{"value":{
            "status": 200,
            "body": json!({"body":{"stores":stores}}).to_string()
        }}}})
    }

    #[tokio::test]
    async fn 同轮附近门店复用一次响应且下一轮重新查询() {
        let stores = ["R428", "R673", "R610", "R409", "R499", "R485"];
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&stores, "MJXQ4ZA/A", "available"),
            pickup_response(&stores, "MJXQ4ZA/A", "unavailable"),
        ])
        .await;
        {
            let mut state = fetcher.state.lock().await;
            state.session.as_mut().unwrap().locale = Some("zh_HK");
        }
        let region = region_by_locale("zh_HK").unwrap();

        for expected_in_stock in [true, false] {
            fetcher.begin_cycle().await;
            // 测试不需要真的等生产环境的两秒最小间隔。
            fetcher.state.lock().await.gate.last_sent = None;
            for store in stores {
                let result = fetcher
                    .pickup_message(region, store, &[test_target("MJXQ4ZA/A")], None)
                    .await
                    .unwrap();
                assert_eq!(result.store_number, store);
                assert_eq!(
                    result.parts["MJXQ4ZA/A"].availability.is_in_stock(),
                    expected_in_stock
                );
            }
            assert_eq!(
                fetcher.cycle_stats().await,
                CycleStats {
                    request_count: 1,
                    reused_response_count: 5,
                }
            );
        }
        assert!(!peer.await.unwrap(), "有效会话不应被提前关闭");
    }

    #[tokio::test]
    async fn 缓存缺门店时回退真实请求() {
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&["R390"], "one", "available"),
            pickup_response(&["R683"], "one", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        fetcher.begin_cycle().await;

        let first = fetcher
            .pickup_message(region, "R390", &[test_target("one")], None)
            .await
            .unwrap();
        assert!(first.parts["one"].availability.is_in_stock());
        fetcher.state.lock().await.gate.last_sent = None;
        let second = fetcher
            .pickup_message(region, "R683", &[test_target("one")], None)
            .await
            .unwrap();
        assert!(!second.parts["one"].availability.is_in_stock());
        assert_eq!(fetcher.cycle_stats().await.request_count, 2);
        assert_eq!(fetcher.cycle_stats().await.reused_response_count, 0);
        assert!(!peer.await.unwrap());
    }

    #[test]
    fn 请求预算等待不会预扣额度() {
        let start = Instant::now();
        let mut budget = RequestBudget::new(start);
        for _ in 0..REQUEST_BUDGET_CAPACITY {
            assert!(budget.take(start));
        }
        assert!(!budget.take(start));
        let before = budget.tokens;
        assert_eq!(budget.available_in(start), REQUEST_BUDGET_REFILL);
        assert_eq!(budget.available_in(start), REQUEST_BUDGET_REFILL);
        assert_eq!(budget.tokens, before, "只查看等待时间不应欠下未来额度");
        assert!(budget.take(start + REQUEST_BUDGET_REFILL));
    }

    #[test]
    fn 下一轮预算会等待整轮额度而不是只等第一条() {
        let start = Instant::now();
        let mut gate = RequestGate {
            budget: RequestBudget::new(start),
            ..RequestGate::default()
        };
        for _ in 0..REQUEST_BUDGET_CAPACITY {
            assert!(gate.budget.take(start));
        }
        gate.cycle_request_count = 3;
        assert_eq!(gate.next_cycle_delay(start), REQUEST_BUDGET_REFILL * 3,);
        assert_eq!(gate.budget.tokens, 0.0, "调度估算不应提前扣额度");
    }

    #[test]
    fn 冷却只在恢复探测再次被拒时升档() {
        let region = region_by_locale("zh_CN").unwrap();
        let mut state = QueryState::default();
        let first = state.start_or_escalate_cooldown(region, "HTTP 541");
        assert!(matches!(
            first,
            ApiError::CoolingDown {
                newly_started: true,
                ..
            }
        ));
        let level = state.cooldowns[region.locale].level;
        for _ in 0..6 {
            assert!(matches!(
                state.active_cooldown_error(region),
                Some(ApiError::CoolingDown {
                    newly_started: false,
                    ..
                })
            ));
        }
        assert_eq!(state.cooldowns[region.locale].level, level);

        state.cooldowns.get_mut(region.locale).unwrap().until =
            Instant::now() - Duration::from_secs(1);
        let second = state.start_or_escalate_cooldown(region, "HTTP 541");
        assert!(matches!(
            second,
            ApiError::CoolingDown {
                newly_started: true,
                ..
            }
        ));
        assert_eq!(state.cooldowns[region.locale].level, level + 1);
    }

    #[tokio::test]
    async fn 服务端五百错误不进入固定地区冷却() {
        let (fetcher, peer) = cdp_fixture(json!({
            "result": {"result": {"value": {"status": 503, "body": "{}"}}}
        }))
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let result = fetcher
            .pickup_message(region, "R390", &[test_target("MG6X4CH/A")], None)
            .await;
        assert!(matches!(result, Err(ApiError::Transport(_))));
        assert!(
            !fetcher
                .state
                .lock()
                .await
                .cooldowns
                .contains_key(region.locale)
        );
        assert!(!peer.await.unwrap(), "普通 5xx 不应销毁仍连接的浏览器会话");
    }

    #[tokio::test]
    async fn 浏览器会话错误后释放失效连接供下轮重建() {
        let (fetcher, peer) = cdp_fixture(json!({
            "error": {"code": -32000, "message": "Target closed"}
        }))
        .await;
        let result = fetcher
            .pickup_message(
                region_by_locale("zh_CN").unwrap(),
                "R390",
                &[test_target("MG6X4CH/A")],
                None,
            )
            .await;
        assert!(matches!(result, Err(ApiError::Transport(_))));
        assert!(
            peer.await.unwrap(),
            "失效 CDP 连接仍被缓存，下轮会继续使用坏会话"
        );
    }

    #[tokio::test]
    async fn 限流保留会话而明确拦截仍清理会话() {
        for (status, should_close) in [(429, false), (403, true), (541, true)] {
            let (fetcher, peer) = cdp_fixture(json!({
                "result": {"result": {"value": {"status": status, "body": "{}"}}}
            }))
            .await;
            let result = fetcher
                .pickup_message(
                    region_by_locale("zh_CN").unwrap(),
                    "R390",
                    &[test_target("MG6X4CH/A")],
                    None,
                )
                .await;
            assert!(matches!(result, Err(ApiError::CoolingDown { .. })));
            assert_eq!(peer.await.unwrap(), should_close, "HTTP {status}");
        }
    }

    #[test]
    fn 能找到本机chromium浏览器() {
        assert!(find_chromium().is_some());
    }

    #[test]
    fn 浏览器响应结构只接受状态与正文() {
        let payload: BrowserPayload =
            serde_json::from_value(json!({"status": 200, "body": "{}"})).unwrap();
        assert_eq!(payload.status, 200);
        assert_eq!(payload.body, "{}");
    }

    #[test]
    fn watch整表送货响应提取精确日期() {
        let raw = r#"{"body":{"content":{"deliveryMessage":{"Z0YQ":{"regular":{"deliveryOptionMessages":[{"displayName":"2026/09/25 – 2026/09/30 — 免费"}]}}}}}}"#;
        assert_eq!(
            parse_delivery_display_name(raw.as_bytes())
                .unwrap()
                .as_deref(),
            Some("2026/09/25 – 2026/09/30 — 免费")
        );
    }

    #[test]
    fn 兼容ua不暴露headless标记() {
        let user_agent = chromium_user_agent();
        assert!(user_agent.contains(&format!("Chrome/{FALLBACK_CHROMIUM_MAJOR}.0.0.0")));
        assert!(!user_agent.contains("HeadlessChrome"));
    }

    #[test]
    fn 会话暖场使用正式购买页而不是sku详情页() {
        let region = region_by_locale("zh_CN").expect("应当有中国大陆地区配置");
        let family = region.families.first().expect("地区应当至少有一个购买页");
        let url = region.buy_page_url(family);

        assert!(url.starts_with("https://www.apple.com.cn/shop/buy-"));
        assert!(!url.contains("/shop/product/"));
    }

    #[test]
    fn devtools异常只向界面返回简短摘要() {
        let details = json!({
            "text": "Uncaught",
            "exception": {
                "className": "DOMException",
                "description": "DOMException: Failed to read a named property from 'Document': Access is denied for this document.\n    at <anonymous>:1:65",
                "objectId": "123456.1.2"
            },
            "stackTrace": {"callFrames": [{"functionName": "", "url": "https://example.invalid"}]}
        });

        let summary = chromium_exception_summary(&details);
        assert_eq!(summary, "浏览器安全限制阻止了本次 Apple 页面访问");
        assert!(!summary.contains("objectId"));
        assert!(!summary.contains("stackTrace"));
        assert!(!summary.contains("Document"));
    }

    #[test]
    fn 未知devtools异常也不会携带堆栈() {
        let details = json!({
            "exception": {
                "description": "TypeError: unexpected value\n    at fetchInventory (<anonymous>:10:2)"
            }
        });

        assert_eq!(
            chromium_exception_summary(&details),
            "浏览器页面执行失败：TypeError: unexpected value"
        );
    }

    /// 真实网络回归：复用同一个浏览器会话连续检查四家门店两轮。
    ///
    /// 第一家门店有货也不能使后续门店短路；第二轮还能成功则同时证明 Cookie
    /// 会话可复用。测试默认忽略，避免普通 `cargo test` 访问外网。
    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实chromium会话连续检查四家门店两轮() {
        let region = region_by_locale("zh_CN").expect("应当有中国大陆地区配置");
        let fetcher = AppleChromiumFetcher::new();
        let part = vec![test_target("MG6X4CH/A")];
        let stores = ["R390", "R401", "R581", "R683"];

        for round in 1..=2 {
            for store in stores {
                let result = fetcher
                    .pickup(region, store, &part, None)
                    .await
                    .unwrap_or_else(|error| panic!("第 {round} 轮门店 {store} 查询失败：{error}"));
                let status = result.parts.get(&part[0].part_number).unwrap_or_else(|| {
                    panic!("第 {round} 轮门店 {store} 响应缺少 {}", part[0].part_number)
                });
                assert_eq!(result.store_number, store);
                assert!(
                    !status.availability.is_unknown(),
                    "第 {round} 轮门店 {store} 未得到明确库存：{:?}",
                    status.availability
                );
                println!(
                    "round={round} store={store} name={} availability={:?}",
                    result.store_name, status.availability
                );
            }
        }
    }

    /// 用户现场回归：Apple Watch 的配置零件号没有 `/shop/product/{part}` 页面，
    /// 但它本身仍可通过库存接口查询。首轮不应再卡到 50 秒暖场超时。
    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实apple_watch配置型号首轮可以查询() {
        let region = region_by_locale("zh_CN").expect("应当有中国大陆地区配置");
        let fetcher = AppleChromiumFetcher::new();
        let part = vec![test_target("MEP24CH/B")];

        let result = tokio::time::timeout(
            Duration::from_secs(35),
            fetcher.pickup(region, "R390", &part, None),
        )
        .await
        .expect("首轮查询不应再等待 50 秒握手超时")
        .expect("Apple Watch 配置型号应当得到库存响应");

        let status = result
            .parts
            .get(&part[0].part_number)
            .expect("响应应包含请求的 Apple Watch 零件号");
        assert!(!status.availability.is_unknown());
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实apple_watch套件能按省市区查询精确送货日期() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        let mut target = test_target("MJCX4CH/B");
        target.companion_part = Some("MKDY4FE/A".into());
        target.kit_part = Some("Z0YQ".into());
        let destination = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        let result = fetcher
            .pickup(region, "R390", &[target.clone()], Some(&destination))
            .await
            .expect("Watch 套件查询应成功");
        let message = result
            .parts
            .get(&target.part_number)
            .and_then(|status| status.pickup_details.as_ref())
            .and_then(|details| details.sale_message.as_deref())
            .expect("Watch 套件应返回送货日期");
        assert!(
            message.contains("2026/"),
            "应为精确日期而不是周范围：{message}"
        );
    }

    /// 用户界面回归：Series 12 表壳必须和页面默认表带组成套件后再查送货，
    /// 否则取货接口只会留下“2-3 周”这种不精确的通用文案。
    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实series_12默认表带能按浦东新区查询精确送货日期() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        let mut target = test_target("MJK44CH/B");
        target.companion_part = Some("MJUY4FE/A".into());
        target.kit_part = Some("Z0YQ".into());
        let destination = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "浦东新区".into(),
        };
        let result = fetcher
            .pickup(region, "R683", &[target.clone()], Some(&destination))
            .await
            .expect("Series 12 套件查询应成功");
        let message = result
            .parts
            .get(&target.part_number)
            .and_then(|status| status.pickup_details.as_ref())
            .and_then(|details| details.sale_message.as_deref())
            .expect("Series 12 套件应返回送货日期");
        eprintln!("Series 12 浦东新区送货：{message}");
        assert!(
            message.contains("2026/"),
            "应为精确日期而不是周范围：{message}"
        );
    }

    /// 现场契约：香港六店同一型号应由首个 searchNearby 响应覆盖，后五店不再出站。
    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实香港六店同轮只发一次取货请求() {
        let region = region_by_locale("zh_HK").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        let stores = ["R428", "R673", "R610", "R409", "R499", "R485"];
        fetcher.begin_cycle().await;

        for store in stores {
            let availability = fetcher
                .pickup(region, store, &[test_target("MJXQ4ZA/A")], None)
                .await
                .expect("香港门店取货响应应可解析");
            assert_eq!(availability.store_number, store);
        }

        assert_eq!(
            fetcher.cycle_stats().await,
            CycleStats {
                request_count: 1,
                reused_response_count: 5,
            }
        );
    }

    #[tokio::test]
    #[ignore = "现场只读诊断，需要本机 Chromium 与 Apple 官网网络"]
    async fn diagnose_missing_store_response() {
        let region = region_by_locale("zh_CN").unwrap();
        let mut session = ChromiumSession::start().await.unwrap();
        let mut gate = RequestGate::default();
        for (store, part) in [
            ("R581", "MFA04CH/B"),
            ("R683", "MG8X4CH/A"),
            ("R581", "MG6W4CH/A"),
            ("R683", "MJYH4CH/A"),
        ] {
            let payload = session
                .fetch(region, store, &[part.to_string()], None, &mut gate)
                .await
                .unwrap();
            println!("store={store} part={part} http={}", payload.status);
            if let Ok(value) = serde_json::from_str::<Value>(&payload.body) {
                let stores = value
                    .pointer("/body/content/pickupMessage/stores")
                    .or_else(|| value.pointer("/body/stores"));
                let ids: Vec<_> = stores
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|store| store.get("storeNumber").and_then(Value::as_str))
                    .collect();
                println!(
                    "returned_stores={ids:?} parsed={:?}",
                    parse_pickup_message(payload.body.as_bytes(), store)
                );
            } else {
                println!("non_json_body bytes={}", payload.body.len());
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    #[tokio::test]
    #[ignore = "用户现场批量隔离对照，只读库存，需要网络"]
    async fn diagnose_old_products_without_new_iphone() {
        let region = region_by_locale("zh_CN").unwrap();
        let mut session = ChromiumSession::start().await.unwrap();
        let mut gate = RequestGate::default();
        let batches: &[(&str, &[&str])] = &[
            ("watch_only", &["MF9T4CH/B"]),
            ("iphone17pro_only", &["MG0G4CH/A"]),
            ("old_only", &["MF9T4CH/B", "MG0G4CH/A"]),
            ("old_and_new", &["MF9T4CH/B", "MG0G4CH/A", "MJTJ4CH/A"]),
            ("old_and_control", &["MF9T4CH/B", "MG0G4CH/A", "MG6W4CH/A"]),
        ];
        for (name, parts) in batches {
            let parts: Vec<_> = parts.iter().map(|p| p.to_string()).collect();
            let payload = session
                .fetch(region, "R359", &parts, None, &mut gate)
                .await
                .unwrap();
            println!(
                "case={name} requested={parts:?} status={} parsed={:?}",
                payload.status,
                parse_pickup_message(payload.body.as_bytes(), "R359")
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
