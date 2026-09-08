//! 通过独立的无界面 Chromium 会话查询 Apple 库存。
//!
//! Apple 当前会在商品页执行 `shop/shld/v2_1/verify.js`，完成浏览器环境校验后才
//! 接受库存请求。普通 HTTP 客户端或 WKWebView 即便拿到了部分 Cookie，仍会收到
//! HTTP 541；真正的 Chromium 会话则能得到正常 JSON。这里启动一个使用临时资料
//! 目录的后台浏览器，通过 DevTools 协议复用同一会话查询所有门店。
//!
//! 这个实现不会读取用户现有 Chrome 的个人资料、Cookie 或浏览记录。临时目录随
//! 会话销毁，浏览器进程也由应用持有并在退出时终止。

use std::path::{Path, PathBuf};
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
const SESSION_READY_TIMEOUT: Duration = Duration::from_secs(50);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
const MAX_RESPONSE_BYTES: usize = 4 << 20;
const FALLBACK_CHROMIUM_MAJOR: u32 = 152;

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
        let user_agent = chromium_user_agent(&chrome);
        let profile = tempfile::Builder::new()
            .prefix("apple-pickup-watcher-chromium-")
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
        let targets: Vec<DebugTarget> = reqwest::get(&targets_url)
            .await
            .map_err(|e| ApiError::Transport(format!("无法连接 Chromium 调试端口：{e}")))?
            .json()
            .await
            .map_err(|e| ApiError::Transport(format!("Chromium 目标列表无法解析：{e}")))?;
        let socket_url = targets
            .into_iter()
            .find(|target| target.kind == "page")
            .and_then(|target| target.web_socket_debugger_url)
            .ok_or_else(|| ApiError::Transport("Chromium 没有可用页面目标".into()))?;
        let (socket, _) = connect_async(&socket_url)
            .await
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
        self.socket
            .send(Message::Text(request.to_string().into()))
            .await
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
            return Err(ApiError::Transport(format!(
                "Chromium 页面脚本执行失败：{details}"
            )));
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    async fn ensure_region(
        &mut self,
        region: &'static Region,
        seed_part: &str,
    ) -> Result<(), ApiError> {
        if self.locale == Some(region.locale) {
            return Ok(());
        }

        // 总览页也会设置同名的 shld Cookie，但该会话查询库存仍会收到 541。
        // `/shop/product/{part}` 是 Apple 自己的稳定入口，会跳到对应品类的具体
        // 商品页；以真实监控零件号建立会话后，库存请求才与官网行为一致。
        let page_url = format!("{}/shop/product/{seed_part}", region.base_url);
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
        self.ensure_region(region, &parts[0]).await?;

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

fn chromium_major_version(path: &Path) -> Option<u32> {
    let output = Command::new(path).arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    parse_chromium_major(&text)
}

fn parse_chromium_major(text: &str) -> Option<u32> {
    text.split_whitespace().find_map(|word| {
        let first = word.split('.').next()?;
        (word.contains('.'))
            .then(|| first.parse::<u32>().ok())
            .flatten()
    })
}

fn chromium_user_agent(path: &Path) -> String {
    let major = chromium_major_version(path).unwrap_or(FALLBACK_CHROMIUM_MAJOR);
    format!(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
AppleWebKit/537.36 (KHTML, like Gecko) Chrome/{major}.0.0.0 Safari/537.36"
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
        let payload = guard
            .as_mut()
            .expect("刚初始化的 Chromium 会话应当存在")
            .fetch(region, store_number, parts)
            .await?;

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
    fn 能从chromium版本输出提取主版本号() {
        assert_eq!(
            parse_chromium_major("Google Chrome 152.0.7777.0\n"),
            Some(152)
        );
        assert_eq!(
            parse_chromium_major("Microsoft Edge 151.0.0.0\n"),
            Some(151)
        );
        assert_eq!(parse_chromium_major("not a version"), None);
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
        let part = vec!["MG084CH/A".to_string()];
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
}
