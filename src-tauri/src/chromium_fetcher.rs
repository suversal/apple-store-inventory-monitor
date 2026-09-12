//! 通过独立的无界面 Chromium 会话查询 Apple 库存。
//!
//! Apple 当前会在商品页执行 `shop/shld/v2_1/verify.js`，完成浏览器环境校验后才
//! 接受库存请求。普通 HTTP 客户端或 WKWebView 即便拿到了部分 Cookie，仍会收到
//! HTTP 541；真正的 Chromium 会话则能得到正常 JSON。这里启动一个使用临时资料
//! 目录的后台浏览器，通过 DevTools 协议复用同一会话查询所有门店。
//!
//! 这个实现不会读取用户现有 Chrome 的个人资料、Cookie 或浏览记录。临时目录随
//! 会话销毁，浏览器进程也由应用持有并在退出时终止。

#[cfg(target_os = "macos")]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apw_core::apple::{ApiError, Fetcher, StoreAvailability, parse_pickup_message};
use apw_core::model::Region;
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
    last_inventory_request: Option<Instant>,
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
            last_inventory_request: None,
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
    ) -> Result<BrowserPayload, ApiError> {
        self.ensure_region(region).await?;

        // 监控引擎会并发调度不同门店。虽然外层 Mutex 已把 DevTools 命令串行化，
        // 但“串行”仍可能是毫秒级连续请求；Apple 会把这种突发识别成自动化并
        // 返回 541。把节流放在共享浏览器会话里，确保跨门店也遵守最小间隔。
        if let Some(last) = self.last_inventory_request {
            let elapsed = last.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        self.last_inventory_request = Some(Instant::now());

        let mut pairs = vec![
            ("fae".to_string(), "true".to_string()),
            ("pl".to_string(), "true".to_string()),
            ("mts.0".to_string(), "regular".to_string()),
        ];
        pairs.extend(
            parts
                .iter()
                .enumerate()
                .map(|(index, part)| (format!("parts.{index}"), part.clone())),
        );
        pairs.push(("store".to_string(), store_number.to_string()));

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
    session: Arc<Mutex<Option<ChromiumSession>>>,
}

impl AppleChromiumFetcher {
    pub fn new() -> Self {
        Self {
            session: Arc::new(Mutex::new(None)),
        }
    }

    async fn pickup(
        &self,
        region: &'static Region,
        store_number: &str,
        parts: &[String],
    ) -> Result<StoreAvailability, ApiError> {
        if store_number.is_empty() {
            return Err(ApiError::Transport("门店编号为空".into()));
        }
        if parts.is_empty() {
            return Err(ApiError::Transport("零件号列表为空".into()));
        }

        // 一把锁覆盖整个浏览器命令往返。监控引擎可以并发调多个门店，但同一个
        // DevTools 连接与 Apple 会话必须串行使用，避免请求突发再次触发 541。
        let mut guard = self.session.lock().await;
        if guard.is_none() {
            *guard = Some(ChromiumSession::start().await?);
        }
        let fetched = guard
            .as_mut()
            .expect("刚初始化的 Chromium 会话应当存在")
            .fetch(region, store_number, parts)
            .await;
        let payload = match fetched {
            Ok(payload) => payload,
            Err(error) => {
                // CDP 断连、命令超时或页面执行失败后，不再缓存这个失效会话。
                // 保留原错误交给引擎处理，后续查询才重建，不在失败请求内重试。
                // Apple 的 HTTP 限流和库存数据仍走下面原有的分类逻辑。
                if matches!(error, ApiError::Transport(_)) {
                    *guard = None;
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
                    return Err(ApiError::Blocked("HTTP 200 但响应不是 JSON".into()));
                }
                parse_pickup_message(bytes, store_number)
            }
            403 | 541 => {
                // 当前浏览器会话已经被拒绝。直接销毁临时资料与进程；下一轮会用
                // 全新会话重新握手，不拿失效 Cookie 反复撞接口。
                *guard = None;
                Err(ApiError::Blocked(format!("HTTP {}", payload.status)))
            }
            429 => Err(ApiError::RateLimited("HTTP 429".into())),
            status if status >= 500 => Err(ApiError::RateLimited(format!("HTTP {status}"))),
            0 => Err(ApiError::Transport(
                payload.body.chars().take(300).collect(),
            )),
            status => Err(ApiError::Transport(format!("HTTP {status}"))),
        }
    }
}

impl Fetcher for AppleChromiumFetcher {
    async fn pickup_message(
        &self,
        region: &'static Region,
        store_number: &str,
        parts: &[String],
    ) -> Result<StoreAvailability, ApiError> {
        self.pickup(region, store_number, parts).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use apw_core::model::region_by_locale;

    /// 模拟浏览器的 CDP 边界，不启动 Chromium，也不访问 Apple。
    async fn cdp_fixture(response: Value) -> (AppleChromiumFetcher, tokio::task::JoinHandle<bool>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = socket.next().await.unwrap().unwrap();
            let request: Value = serde_json::from_str(request.to_text().unwrap()).unwrap();
            let mut response = response;
            response["id"] = request["id"].clone();
            socket
                .send(Message::Text(response.to_string().into()))
                .await
                .unwrap();
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
            last_inventory_request: None,
        };
        (
            AppleChromiumFetcher {
                session: Arc::new(Mutex::new(Some(session))),
            },
            peer,
        )
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
                &["MG6X4CH/A".to_string()],
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
                    &["MG6X4CH/A".to_string()],
                )
                .await;
            if status == 429 {
                assert!(matches!(result, Err(ApiError::RateLimited(_))));
            } else {
                assert!(matches!(result, Err(ApiError::Blocked(_))));
            }
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
        let part = vec!["MG6X4CH/A".to_string()];
        let stores = ["R390", "R401", "R581", "R683"];

        for round in 1..=2 {
            for store in stores {
                let result = fetcher
                    .pickup(region, store, &part)
                    .await
                    .unwrap_or_else(|error| panic!("第 {round} 轮门店 {store} 查询失败：{error}"));
                let status = result
                    .parts
                    .get(&part[0])
                    .unwrap_or_else(|| panic!("第 {round} 轮门店 {store} 响应缺少 {}", part[0]));
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
        let part = vec!["MEP24CH/B".to_string()];

        let result = tokio::time::timeout(
            Duration::from_secs(35),
            fetcher.pickup(region, "R390", &part),
        )
        .await
        .expect("首轮查询不应再等待 50 秒握手超时")
        .expect("Apple Watch 配置型号应当得到库存响应");

        let status = result
            .parts
            .get(&part[0])
            .expect("响应应包含请求的 Apple Watch 零件号");
        assert!(!status.availability.is_unknown());
    }
    #[tokio::test]
    #[ignore = "现场只读诊断，需要本机 Chromium 与 Apple 官网网络"]
    async fn diagnose_missing_store_response() {
        let region = region_by_locale("zh_CN").unwrap();
        let mut session = ChromiumSession::start().await.unwrap();
        for (store, part) in [
            ("R581", "MFA04CH/B"),
            ("R683", "MG8X4CH/A"),
            ("R581", "MG6W4CH/A"),
            ("R683", "MJYH4CH/A"),
        ] {
            let payload = session
                .fetch(region, store, &[part.to_string()])
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
        let batches: &[(&str, &[&str])] = &[
            ("watch_only", &["MF9T4CH/B"]),
            ("iphone17pro_only", &["MG0G4CH/A"]),
            ("old_only", &["MF9T4CH/B", "MG0G4CH/A"]),
            ("old_and_new", &["MF9T4CH/B", "MG0G4CH/A", "MJTJ4CH/A"]),
            ("old_and_control", &["MF9T4CH/B", "MG0G4CH/A", "MG6W4CH/A"]),
        ];
        for (name, parts) in batches {
            let parts: Vec<_> = parts.iter().map(|p| p.to_string()).collect();
            let payload = session.fetch(region, "R359", &parts).await.unwrap();
            println!(
                "case={name} requested={parts:?} status={} parsed={:?}",
                payload.status,
                parse_pickup_message(payload.body.as_bytes(), "R359")
            );
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
}
