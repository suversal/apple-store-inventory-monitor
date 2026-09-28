//! 通过独立的 Chromium 会话查询 Apple 库存。
//!
//! Apple 当前会在商品页执行 `shop/shld/v2_1/verify.js`，完成浏览器环境校验后才
//! 接受库存请求。普通 HTTP 客户端或 WKWebView 即便拿到了部分 Cookie，仍会收到
//! HTTP 541；真正的 Chromium 会话则能得到正常 JSON。这里启动一个使用应用专属
//! 持久资料目录的浏览器，通过 DevTools 协议复用同一会话查询所有门店。
//!
//! 这个实现不会读取用户现有 Chrome 的个人资料、Cookie 或浏览记录。专属会话
//! 始终在后台启动，不会因查询失败自动弹出页面；浏览器进程由应用持有并在退出时终止。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use apw_core::apple::{
    ApiError, CycleStats, DeliveryInfo as ProductDelivery, Fetcher, ScheduleHint,
    StoreAvailability, parse_pickup_message,
};
use apw_core::model::{DeliveryRegion, Region, Target};
use apw_core::watcher::MAX_PARTS_PER_REQUEST;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

const CHROME_START_TIMEOUT: Duration = Duration::from_secs(12);
const DEVTOOLS_SOCKET_TIMEOUT: Duration = Duration::from_secs(10);
const SESSION_READY_TIMEOUT: Duration = Duration::from_secs(50);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(25);
const MIN_REQUEST_INTERVAL: Duration = Duration::from_secs(2);
const DELIVERY_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const EMPTY_DELIVERY_CACHE_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_RESPONSE_BYTES: usize = 4 << 20;
const MAX_EXCEPTION_SUMMARY_CHARS: usize = 160;
const CHROMIUM_PROFILE_DIR: &str = "chromium-profile-v1";
const SESSION_ESTABLISHED_MARKER: &str = ".apple-session-established";
const PAGE_FETCH_FAILURE_MARKER: &str = "__APW_PAGE_FETCH_FAILED__:";
const PAGE_FETCH_FAILURE_PREFIX: &str = "Apple 页面请求失败：";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DebugTarget {
    #[serde(rename = "type")]
    kind: String,
    web_socket_debugger_url: Option<String>,
}

#[derive(Debug)]
struct ChromiumProcess {
    child: Child,
    /// Unix 下为 Chromium 独立进程组的 ID；退出时必须清理整个多进程树。
    process_group_id: Option<u32>,
}

impl Drop for ChromiumProcess {
    fn drop(&mut self) {
        terminate_chromium(&mut self.child, self.process_group_id);
    }
}

#[derive(Debug)]
struct ChromiumSession {
    /// 必须放在会话建立的最早阶段。DevTools 连接前任一错误返回，都要回收进程树。
    _process: ChromiumProcess,
    profile_dir: PathBuf,
    socket: Socket,
    next_command_id: u64,
    locale: Option<&'static str>,
    established: bool,
}

fn terminate_chromium(child: &mut Child, _process_group_id: Option<u32>) {
    #[cfg(unix)]
    if let Some(process_group_id) = _process_group_id {
        // Chromium 会再派生 renderer、GPU、utility 等子进程。只 kill 主进程会让
        // 它们被 launchd/systemd 接管，最终表现为 Dock 认为 Chrome 仍在运行。
        // 独立进程组让我们可以先温和终止整棵树，再用 SIGKILL 做有界兜底。
        let group = -(process_group_id as i32);
        unsafe {
            libc::kill(group, libc::SIGTERM);
        }
        for _ in 0..10 {
            if child.try_wait().ok().flatten().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        unsafe {
            libc::kill(group, libc::SIGKILL);
        }
        let _ = child.wait();
        return;
    }

    let _ = child.kill();
    let _ = child.wait();
}

fn reserve_loopback_port() -> Result<u16, ApiError> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| ApiError::Transport(format!("无法预留 Chromium 本机调试端口：{e}")))?;
    listener
        .local_addr()
        .map(|address| address.port())
        .map_err(|e| ApiError::Transport(format!("无法读取 Chromium 本机调试端口：{e}")))
}

fn chromium_profile_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("apple-store-inventory-monitor")
        .join(CHROMIUM_PROFILE_DIR)
}

#[cfg(unix)]
fn profile_lock_pid(profile_dir: &Path) -> Option<u32> {
    let target = std::fs::read_link(profile_dir.join("SingletonLock")).ok()?;
    target
        .to_string_lossy()
        .rsplit('-')
        .next()
        .and_then(|pid| pid.parse().ok())
}

#[cfg(unix)]
fn reclaim_orphaned_profile_process(profile_dir: &Path) -> Result<(), ApiError> {
    let Some(pid) = profile_lock_pid(profile_dir) else {
        return Ok(());
    };
    let pid_text = pid.to_string();
    let output = Command::new("ps")
        .args(["-p", pid_text.as_str(), "-o", "ppid=", "-o", "command="])
        .output()
        .map_err(|e| ApiError::Transport(format!("无法检查遗留 Chromium 进程：{e}")))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(());
    }

    let process = String::from_utf8_lossy(&output.stdout);
    let mut fields = process.split_whitespace();
    let parent_pid = fields.next().and_then(|value| value.parse::<u32>().ok());
    let command = fields.collect::<Vec<_>>().join(" ");
    let profile_arg = format!("--user-data-dir={}", profile_dir.display());
    let lower = command.to_ascii_lowercase();
    let is_scoped_chromium = command.contains(&profile_arg)
        && (lower.contains("google chrome")
            || lower.contains("microsoft edge")
            || lower.contains("chromium"));
    if !is_scoped_chromium {
        return Err(ApiError::Transport(
            "Chromium 资料目录被其他进程占用，为避免影响你的普通浏览器，本轮未启动新会话".into(),
        ));
    }
    if parent_pid != Some(1) {
        return Err(ApiError::Transport(
            "另一个果到雷达实例正在使用专属 Chromium 会话，本轮未重复打开浏览器".into(),
        ));
    }

    let process_group = -(pid as i32);
    unsafe {
        libc::kill(process_group, libc::SIGTERM);
    }
    for _ in 0..20 {
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if !alive {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    unsafe {
        libc::kill(process_group, libc::SIGKILL);
    }
    for _ in 0..20 {
        let alive = unsafe { libc::kill(pid as i32, 0) == 0 };
        if !alive {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Err(ApiError::Transport(
        "应用专属 Chromium 残留进程未能退出，本轮不会继续重复启动".into(),
    ))
}

#[cfg(not(unix))]
fn reclaim_orphaned_profile_process(_profile_dir: &Path) -> Result<(), ApiError> {
    Ok(())
}

impl ChromiumSession {
    async fn start(profile_dir: PathBuf) -> Result<Self, ApiError> {
        let chrome = find_chromium().ok_or_else(|| {
            ApiError::Transport(
                "未找到 Google Chrome 或 Microsoft Edge；Apple 当前库存接口要求完整 Chromium 浏览器会话"
                    .into(),
            )
        })?;
        std::fs::create_dir_all(&profile_dir)
            .map_err(|e| ApiError::Transport(format!("无法创建 Chromium 专属资料目录：{e}")))?;
        reclaim_orphaned_profile_process(&profile_dir)?;
        let established = profile_dir.join(SESSION_ESTABLISHED_MARKER).is_file();
        let profile_arg = format!("--user-data-dir={}", profile_dir.display());

        // 不用 `--remote-debugging-port=0`：Chrome 会把这个特殊值视为自动化信号。
        // 先在本机回环上预留一个端口，立即释放后交给 Chromium 监听。
        let debug_port = reserve_loopback_port()?;
        let debug_port_arg = format!("--remote-debugging-port={debug_port}");
        eprintln!(
            "正在启动 Apple 专属 Chromium 会话：profile={}，mode=background-headed",
            profile_dir.display()
        );
        let mut command = Command::new(chrome);
        command
            .args([
                "--remote-debugging-address=127.0.0.1",
                debug_port_arg.as_str(),
                profile_arg.as_str(),
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
                "--disable-default-apps",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // 始终在后台建立会话。HTTP 541 或初始化失败也不能抢焦点、弹出页面；
        // 错误留在应用日志中，并按用户设置的查询间隔重试。
        command.arg("--start-minimized");
        command.arg("about:blank");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|e| ApiError::Transport(format!("无法启动 Chromium：{e}")))?;
        #[cfg(unix)]
        let process_group_id = Some(child.id());
        #[cfg(not(unix))]
        let process_group_id = None;
        // 从 spawn 成功这一刻起就装进守卫；下面任何 `?` 提前返回都不会泄漏。
        let mut process = ChromiumProcess {
            child,
            process_group_id,
        };

        let deadline = Instant::now() + CHROME_START_TIMEOUT;
        let targets_url = format!("http://127.0.0.1:{debug_port}/json/list");
        let targets = loop {
            if let Some(status) = process
                .child
                .try_wait()
                .map_err(|e| ApiError::Transport(format!("无法检查 Chromium 状态：{e}")))?
            {
                return Err(ApiError::Transport(format!(
                    "Chromium 启动后立即退出：{status}"
                )));
            }
            if Instant::now() >= deadline {
                return Err(ApiError::Transport("等待 Chromium 启动超时".into()));
            }
            let target_attempt = tokio::time::timeout(Duration::from_millis(500), async {
                let response = reqwest::get(&targets_url).await.ok()?;
                response.json::<Vec<DebugTarget>>().await.ok()
            })
            .await;
            if let Ok(Some(targets)) = target_attempt {
                break targets;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let socket_url = targets
            .into_iter()
            .find(|target| target.kind == "page")
            .and_then(|target| target.web_socket_debugger_url)
            .ok_or_else(|| ApiError::Transport("Chromium 没有可用页面目标".into()))?;
        let (socket, _) = tokio::time::timeout(DEVTOOLS_SOCKET_TIMEOUT, connect_async(&socket_url))
            .await
            .map_err(|_| ApiError::Transport("连接 Chromium 调试 WebSocket 超时".into()))?
            .map_err(|e| ApiError::Transport(format!("无法连接 Chromium 页面：{e}")))?;

        let mut session = Self {
            _process: process,
            profile_dir,
            socket,
            next_command_id: 1,
            locale: None,
            established,
        };
        if let Ok(version) = session.command("Browser.getVersion", json!({})).await {
            let product = version
                .get("product")
                .and_then(Value::as_str)
                .unwrap_or("未知版本");
            let user_agent = version
                .get("userAgent")
                .and_then(Value::as_str)
                .unwrap_or("未知 UA");
            eprintln!("Apple 专属 Chromium 会话已连接：{product}，{user_agent}");
        }
        Ok(session)
    }

    async fn mark_established(&mut self) {
        if !self.established {
            let marker = self.profile_dir.join(SESSION_ESTABLISHED_MARKER);
            if let Err(error) = std::fs::write(&marker, b"ok\n") {
                eprintln!("Apple 会话成功，但无法写入已建立标记：{error}");
            } else {
                self.established = true;
            }
        }
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

        // 具体购买页不是可靠的会话入口：澳大利亚站会对自动化 Chromium 关闭
        // `/shop/buy-*` 连接，但地区首页加载后，同源取货接口仍能正常返回 JSON。
        // 首页不依赖某个仍在售的 SKU，也适用于 iPhone、Watch、iPad 和 Mac。
        let page_url = region.session_page_url();
        let navigation = self
            .command("Page.navigate", json!({ "url": page_url }))
            .await?;
        if let Some(error) = navigation
            .get("errorText")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|error| !error.is_empty())
        {
            return Err(ApiError::Transport(format!(
                "Apple 地区页面导航失败：{error}"
            )));
        }
        let deadline = Instant::now() + SESSION_READY_TIMEOUT;
        loop {
            let state = self
                .evaluate(r#"JSON.stringify({readyState:document.readyState})"#, false)
                .await?;
            if let Some(raw) = state.as_str()
                && let Ok(state) = serde_json::from_str::<ReadyState>(raw)
                && state.ready_state == "complete"
            {
                self.locale = Some(region.locale);
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(ApiError::Transport(
                    "Apple 地区页面在等待时间内未完成加载".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn fetch(
        &mut self,
        region: &'static Region,
        scope: &PickupScope,
        parts: &[String],
        gate: &mut RequestGate,
    ) -> Result<BrowserPayload, ApiError> {
        self.ensure_region(region).await?;
        gate.acquire().await;

        let pairs = pickup_query_pairs(scope, parts);
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
                try {{
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
                }} catch(error) {{
                    const detail=error instanceof Error ? `${{error.name}}: ${{error.message}}` : String(error);
                    return {{status:0,body:'__APW_PAGE_FETCH_FAILED__:'+detail}};
                }}
            }})()"#
        );
        let value = self.evaluate(&expression, true).await?;
        serde_json::from_value(value)
            .map_err(|e| ApiError::Transport(format!("Chromium 库存结果无法解析：{e}")))
    }

    /// 单独读取普通商品的送货说明。该请求无论怎样失败，都不能改变取货库存。
    async fn fetch_product_delivery(
        &mut self,
        region: &'static Region,
        parts: &[String],
        location: &DeliveryRegion,
        gate: &mut RequestGate,
    ) -> Result<BTreeMap<String, ProductDelivery>, ApiError> {
        gate.acquire().await;
        let pairs = delivery_query_pairs(parts, location);

        #[derive(Serialize)]
        struct BrowserRequest<'a> {
            url: String,
            pairs: &'a [(String, String)],
            max_bytes: usize,
        }
        let request = serde_json::to_string(&BrowserRequest {
            url: region.delivery_message_url(),
            pairs: &pairs,
            max_bytes: MAX_RESPONSE_BYTES,
        })
        .map_err(|e| ApiError::Transport(format!("无法编码送货请求：{e}")))?;
        let expression = format!(
            r#"(async()=>{{
                try {{
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
                }} catch(error) {{
                    const detail=error instanceof Error ? `${{error.name}}: ${{error.message}}` : String(error);
                    return {{status:0,body:'__APW_PAGE_FETCH_FAILED__:'+detail}};
                }}
            }})()"#
        );
        let value = self.evaluate(&expression, true).await?;
        let payload: BrowserPayload = serde_json::from_value(value)
            .map_err(|e| ApiError::Transport(format!("Chromium 送货结果无法解析：{e}")))?;
        match payload.status {
            200 => parse_product_delivery(payload.body.as_bytes()),
            403 | 541 => Err(ApiError::Blocked(format!("HTTP {}", payload.status))),
            429 => Err(ApiError::RateLimited("HTTP 429".into())),
            status if status >= 500 => Err(ApiError::Transport(format!("HTTP {status}"))),
            0 => Err(browser_payload_transport_error(&payload.body)),
            status => Err(ApiError::Transport(format!("HTTP {status}"))),
        }
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
            url: region.delivery_message_url(),
            pairs: &pairs,
            max_bytes: MAX_RESPONSE_BYTES,
        })
        .map_err(|e| ApiError::Transport(format!("无法编码 Watch 送货请求：{e}")))?;
        let expression = format!(
            r#"(async()=>{{
                try {{
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
                }} catch(error) {{
                    const detail=error instanceof Error ? `${{error.name}}: ${{error.message}}` : String(error);
                    return {{status:0,body:'__APW_PAGE_FETCH_FAILED__:'+detail}};
                }}
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
            0 => return Err(browser_payload_transport_error(&payload.body)),
            status => return Err(ApiError::Transport(format!("HTTP {status}"))),
        };
        Ok(message)
    }
}

fn parse_product_delivery(raw: &[u8]) -> Result<BTreeMap<String, ProductDelivery>, ApiError> {
    let value: Value = serde_json::from_slice(raw).map_err(|e| ApiError::SchemaDrift {
        field: "body.content.deliveryMessage".into(),
        raw: format!("送货响应不是 JSON：{e}"),
    })?;
    if let Some(message) = value
        .pointer("/body/errorMessage")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty())
    {
        return Err(ApiError::Apple(message.to_string()));
    }
    let Some(messages) = value
        .pointer("/body/content/deliveryMessage")
        .and_then(Value::as_object)
    else {
        return Ok(BTreeMap::new());
    };
    Ok(messages
        .iter()
        .filter_map(|(part, message)| {
            let regular = message.get("regular")?;
            let sale_reason = regular
                .pointer("/buyability/reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let sale_message = regular
                .pointer("/deliveryOptionMessages/0/displayName")
                .and_then(Value::as_str)
                .map(str::to_owned);
            (sale_reason.is_some() || sale_message.is_some()).then(|| {
                (
                    part.clone(),
                    ProductDelivery {
                        sale_reason,
                        sale_message,
                    },
                )
            })
        })
        .collect())
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
}

#[derive(Debug, Deserialize)]
struct BrowserPayload {
    status: u16,
    body: String,
}

fn browser_payload_transport_error(body: &str) -> ApiError {
    let detail: String = body
        .strip_prefix(PAGE_FETCH_FAILURE_MARKER)
        .map(|detail| format!("{PAGE_FETCH_FAILURE_PREFIX}{detail}"))
        .unwrap_or_else(|| body.to_string())
        .chars()
        .take(300)
        .collect();
    ApiError::Transport(detail)
}

fn is_page_fetch_failure(error: &ApiError) -> bool {
    matches!(error, ApiError::Transport(detail) if detail.starts_with(PAGE_FETCH_FAILURE_PREFIX))
}

#[derive(Debug, Clone)]
enum PickupScope {
    Store(String),
    Nearby(String),
}

impl PickupScope {
    fn query_pair(&self) -> (&'static str, &str) {
        match self {
            Self::Store(store) => ("store", store),
            Self::Nearby(location) => ("location", location),
        }
    }
}

fn pickup_query_pairs(scope: &PickupScope, parts: &[String]) -> Vec<(String, String)> {
    let mut pairs = vec![
        ("pl".to_string(), "true".to_string()),
        ("mts.0".to_string(), "regular".to_string()),
    ];
    let (scope_key, scope_value) = scope.query_pair();
    pairs.push((scope_key.to_string(), scope_value.to_string()));
    pairs.extend(
        parts
            .iter()
            .enumerate()
            .map(|(index, part)| (format!("parts.{index}"), part.clone())),
    );
    pairs
}

/// 地点查询没有给出目标门店时，单店查询仍可能正常工作。
///
/// 只认三种明确的“地点无结果”，不能把限流、网络错误或任意结构变化都变成一次
/// 额外请求，否则恰好会在 Apple 拒绝访问时加重请求突发。
fn nearby_response_needs_store_fallback(payload: &BrowserPayload, store_number: &str) -> bool {
    if payload.status != 200 {
        return false;
    }
    match parse_pickup_message(payload.body.as_bytes(), store_number) {
        Err(ApiError::StorePickupUnavailable { .. } | ApiError::NoPickupData { .. }) => true,
        Err(ApiError::SchemaDrift { field, .. }) => field == "body.stores[].storeNumber",
        _ => false,
    }
}

fn delivery_query_pairs(parts: &[String], location: &DeliveryRegion) -> Vec<(String, String)> {
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
    // 大陆送货使用独立省市区。location 是取货地点搜索参数，送货接口可能
    // 忽略它并返回 messageType=Ship 的发货时长，不能当作所选地址的送达日期。
    pairs.extend([
        ("state".to_string(), location.state.clone()),
        ("city".to_string(), location.city.clone()),
        ("district".to_string(), location.district.clone()),
    ]);
    pairs
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

/// 可交给核心监控引擎的 Chromium 查询器。
#[derive(Debug, Clone)]
pub struct AppleChromiumFetcher {
    state: Arc<Mutex<QueryState>>,
    profile_dir: PathBuf,
}

type PickupCacheKey = (&'static str, Vec<String>, String);
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeliveryCacheKey {
    locale: String,
    part: String,
    kit: Option<String>,
    companion: Option<String>,
    location: DeliveryRegion,
}

impl DeliveryCacheKey {
    fn new(target: &Target, location: &DeliveryRegion) -> Self {
        Self {
            locale: target.locale.clone(),
            part: target.part_number.clone(),
            kit: target.kit_part.clone(),
            companion: target.companion_part.clone(),
            location: location.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedDelivery {
    fetched_at: Instant,
    info: ProductDelivery,
}

/// 按地区和地点合并普通商品型号。整组装入，避免把同一门店的原始批次拆散；
/// 每个合并请求最多 20 个型号，Watch 组合商品继续走原查询路径。
fn plan_nearby_parts(groups: &[Vec<Target>]) -> HashMap<PickupCacheKey, Vec<String>> {
    type Batch = (BTreeSet<String>, Vec<PickupCacheKey>);
    let mut batches: BTreeMap<(&'static str, String), Vec<Batch>> = BTreeMap::new();
    for targets in groups {
        let Some(first) = targets.first() else {
            continue;
        };
        let Some(region) = apw_core::model::region_by_locale(&first.locale) else {
            continue;
        };
        if targets.iter().any(|target| {
            target.locale != first.locale
                || target.companion_part.is_some()
                || target.kit_part.is_some()
        }) {
            continue;
        }
        let Some(location) = targets
            .iter()
            .filter_map(|target| target.pickup_location.as_deref())
            .map(str::trim)
            .find(|location| !location.is_empty())
        else {
            continue;
        };
        let parts: BTreeSet<_> = targets
            .iter()
            .map(|target| target.part_number.clone())
            .collect();
        if parts.len() > MAX_PARTS_PER_REQUEST {
            continue;
        }
        let key = (
            region.locale,
            parts.iter().cloned().collect(),
            location.to_string(),
        );
        let city_batches = batches
            .entry((region.locale, location.to_string()))
            .or_default();
        if let Some((merged, members)) = city_batches
            .iter_mut()
            .find(|(merged, _)| merged.union(&parts).count() <= MAX_PARTS_PER_REQUEST)
        {
            merged.extend(parts);
            members.push(key);
        } else {
            city_batches.push((parts, vec![key]));
        }
    }
    batches
        .into_values()
        .flatten()
        .flat_map(|(parts, members)| {
            let parts: Vec<_> = parts.into_iter().collect();
            members.into_iter().map(move |key| (key, parts.clone()))
        })
        .collect()
}

#[derive(Debug, Default)]
struct RequestGate {
    last_sent: Option<Instant>,
    cycle_request_count: u32,
}

impl RequestGate {
    fn next_delay(&mut self, now: Instant) -> Duration {
        self.last_sent
            .map(|last| (last + MIN_REQUEST_INTERVAL).saturating_duration_since(now))
            .unwrap_or_default()
    }

    async fn acquire(&mut self) {
        loop {
            let now = Instant::now();
            let delay = self.next_delay(now);
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
                continue;
            }

            let now = Instant::now();
            self.last_sent = Some(now);
            self.cycle_request_count = self.cycle_request_count.saturating_add(1);
            return;
        }
    }
}

#[derive(Debug, Clone)]
enum CyclePickupRejection {
    Blocked(String),
    RateLimited(String),
}

impl CyclePickupRejection {
    fn into_api_error(self) -> ApiError {
        match self {
            Self::Blocked(detail) => ApiError::Blocked(detail),
            Self::RateLimited(detail) => ApiError::RateLimited(detail),
        }
    }
}

#[derive(Debug, Default)]
struct QueryState {
    session: Option<ChromiumSession>,
    /// 本轮启动浏览器会话失败后不再为每个门店重复启动。
    /// 新一轮会清空并再次尝试，重试节奏仍由用户设置决定。
    session_start_failure: Option<String>,
    pickup_cache: HashMap<PickupCacheKey, String>,
    nearby_parts: HashMap<PickupCacheKey, Vec<String>>,
    /// 本轮已经证实无法用于批量查询的地点。后续同地点门店直接按编号查询，
    /// 避免每家店都重复一次必然失败的地点请求；下一轮仍会重新探测。
    unavailable_nearby: HashSet<PickupCacheKey>,
    /// 本轮已经被 Apple 拒绝的取货地区。
    ///
    /// 541/403/429 发生后继续为同地区逐店发请求，不会提高成功率，只会把
    /// 一轮查询从十几秒拖到数分钟。这里只短路当前轮次；`begin_cycle` 会清空它，
    /// 下一轮仍按用户设置的间隔复用同一会话探测，不是保护冷却或指数退避。
    pickup_rejections: HashMap<&'static str, CyclePickupRejection>,
    /// 本轮已经被旧 fulfillment 送货接口拒绝的地区。
    ///
    /// 同轮不再重复请求，避免一轮里连续触发 541；新一轮立即清空，继续按用户设置
    /// 的频率查询，不建立跨轮次冷却。它也不能阻断门店取货查询。
    delivery_rejections: HashSet<&'static str>,
    delivery_cache: HashMap<DeliveryCacheKey, CachedDelivery>,
    /// 成功响应中的空文案、仅购买状态和缺失型号短暂复用，避免逐店重查。
    empty_delivery_cache: HashMap<DeliveryCacheKey, CachedDelivery>,
    /// 包括失败在内，每个型号和地址一轮最多尝试一次。
    delivery_attempts: HashSet<DeliveryCacheKey>,
    gate: RequestGate,
    cycle_reused_response_count: u32,
}

impl QueryState {
    fn begin_cycle(&mut self) {
        self.session_start_failure = None;
        self.pickup_cache.clear();
        self.nearby_parts.clear();
        self.unavailable_nearby.clear();
        self.pickup_rejections.clear();
        self.delivery_rejections.clear();
        self.delivery_attempts.clear();
        self.empty_delivery_cache
            .retain(|_, entry| entry.fetched_at.elapsed() < EMPTY_DELIVERY_CACHE_TTL);
        self.gate.cycle_request_count = 0;
        self.cycle_reused_response_count = 0;
    }

    fn cached_delivery(
        &self,
        target: &Target,
        location: &DeliveryRegion,
    ) -> Option<ProductDelivery> {
        let key = DeliveryCacheKey::new(target, location);
        // 到期仅触发刷新，不删除上次成功结果；空响应或刷新失败不能清空日期。
        self.delivery_cache
            .get(&key)
            .or_else(|| {
                self.empty_delivery_cache
                    .get(&key)
                    .filter(|entry| entry.fetched_at.elapsed() < EMPTY_DELIVERY_CACHE_TTL)
            })
            .map(|entry| entry.info.clone())
    }

    fn needs_delivery(&self, target: &Target, location: &DeliveryRegion) -> bool {
        let key = DeliveryCacheKey::new(target, location);
        !self.delivery_attempts.contains(&key)
            && !self
                .delivery_cache
                .get(&key)
                .is_some_and(|entry| entry.fetched_at.elapsed() < DELIVERY_CACHE_TTL)
            && !self
                .empty_delivery_cache
                .get(&key)
                .is_some_and(|entry| entry.fetched_at.elapsed() < EMPTY_DELIVERY_CACHE_TTL)
    }

    fn cache_delivery(
        &mut self,
        target: &Target,
        location: &DeliveryRegion,
        info: ProductDelivery,
    ) {
        let key = DeliveryCacheKey::new(target, location);
        // 空响应只延后五分钟重试，不覆盖已有日期，也不延长已有日期的有效期。
        // 明确的“暂无供应”有送货文案，正常替换旧日期并缓存一小时。
        if !info
            .sale_message
            .as_deref()
            .is_some_and(|message| !message.trim().is_empty())
        {
            self.empty_delivery_cache.insert(
                key,
                CachedDelivery {
                    fetched_at: Instant::now(),
                    info,
                },
            );
            return;
        }
        self.empty_delivery_cache.remove(&key);
        self.delivery_cache.insert(
            key,
            CachedDelivery {
                fetched_at: Instant::now(),
                info,
            },
        );
    }

    fn schedule_hint(&mut self, _locales: &[String]) -> ScheduleHint {
        ScheduleHint::default()
    }

    fn retry_now(&mut self) {
        self.session_start_failure = None;
        self.pickup_cache.clear();
        self.unavailable_nearby.clear();
        self.pickup_rejections.clear();
        self.delivery_rejections.clear();
        self.delivery_attempts.clear();
    }

    fn pickup_rejection(&self, region: &'static Region) -> Option<ApiError> {
        self.pickup_rejections
            .get(region.locale)
            .cloned()
            .map(CyclePickupRejection::into_api_error)
    }

    fn reject_pickup_for_cycle(
        &mut self,
        region: &'static Region,
        rejection: CyclePickupRejection,
    ) {
        self.pickup_rejections
            .entry(region.locale)
            .or_insert(rejection);
        self.pickup_cache.retain(|key, _| key.0 != region.locale);
    }

    fn delivery_rejected_this_cycle(&self, region: &'static Region) -> bool {
        self.delivery_rejections.contains(region.locale)
    }

    fn reject_delivery_for_cycle(&mut self, region: &'static Region) {
        self.delivery_rejections.insert(region.locale);
    }
}

impl AppleChromiumFetcher {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(QueryState::default())),
            profile_dir: chromium_profile_dir(),
        }
    }

    /// 显式关闭共享浏览器会话。
    ///
    /// Tauri 的 `app.exit()` 不保证异步任务按持有顺序析构，所以不能只依赖
    /// `ChromiumSession::drop` 在进程退出的最后一刻碰运气。
    pub async fn shutdown(&self) {
        self.state.lock().await.session = None;
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
        let pickup_location = targets
            .iter()
            .filter_map(|target| target.pickup_location.as_deref())
            .map(str::trim)
            .find(|location| !location.is_empty())
            .map(str::to_string);
        let can_reuse_nearby = pickup_location.is_some()
            && targets
                .iter()
                .all(|target| target.companion_part.is_none() && target.kit_part.is_none());
        let original_key = pickup_location
            .as_ref()
            .filter(|_| can_reuse_nearby)
            .map(|location| (region.locale, parts.clone(), location.clone()));

        // 一把锁覆盖整个浏览器命令往返。监控引擎可以并发调多个门店，但同一个
        // DevTools 连接与 Apple 会话必须串行使用，避免请求突发再次触发 541。
        let mut guard = self.state.lock().await;
        let nearby_parts = original_key
            .as_ref()
            .and_then(|key| guard.nearby_parts.get(key))
            .cloned()
            .unwrap_or_else(|| parts.clone());
        let cache_key =
            original_key.map(|(locale, _, location)| (locale, nearby_parts.clone(), location));
        if let Some(error) = guard.pickup_rejection(region) {
            return Err(error);
        }
        if let Some(detail) = guard.session_start_failure.clone() {
            return Err(ApiError::Transport(detail));
        }
        let nearby_unavailable = cache_key
            .as_ref()
            .is_some_and(|cache_key| guard.unavailable_nearby.contains(cache_key));
        let cached_body = cache_key
            .as_ref()
            .and_then(|key| guard.pickup_cache.get(key))
            .cloned();
        let (mut payload, mut used_nearby_response) = if let Some(body) = cached_body {
            // 缓存只保存取货接口原文。命中后仍要继续执行下方的配送合并，不能
            // 提前返回，否则同城第二家门店会把预计送货显示成“未返回”。
            if parse_pickup_message(body.as_bytes(), store_number).is_ok() {
                guard.cycle_reused_response_count =
                    guard.cycle_reused_response_count.saturating_add(1);
            }
            (BrowserPayload { status: 200, body }, true)
        } else {
            if guard.session.is_none() {
                match ChromiumSession::start(self.profile_dir.clone()).await {
                    Ok(session) => guard.session = Some(session),
                    Err(error) => {
                        let detail = match &error {
                            ApiError::Transport(detail) => detail.clone(),
                            _ => error.to_string(),
                        };
                        guard.session_start_failure = Some(detail);
                        return Err(error);
                    }
                }
            }
            let scope = if can_reuse_nearby && !nearby_unavailable {
                PickupScope::Nearby(pickup_location.expect("附近查询必须有地点"))
            } else {
                PickupScope::Store(store_number.to_string())
            };
            let fetched = {
                let QueryState { session, gate, .. } = &mut *guard;
                session
                    .as_mut()
                    .expect("刚初始化的 Chromium 会话应当存在")
                    .fetch(
                        region,
                        &scope,
                        if matches!(scope, PickupScope::Nearby(_)) {
                            &nearby_parts
                        } else {
                            &parts
                        },
                        gate,
                    )
                    .await
            };
            let payload = match fetched {
                Ok(payload) => payload,
                Err(error) => {
                    // CDP 断连、命令超时或页面执行失败后，不再缓存这个失效会话。
                    // 保留原错误交给引擎处理，后续查询才重建，不在失败请求内重试。
                    // Apple 的 HTTP 限流和库存数据仍走下面原有的分类逻辑。
                    if let ApiError::Blocked(detail) = &error {
                        guard.reject_pickup_for_cycle(
                            region,
                            CyclePickupRejection::Blocked(detail.clone()),
                        );
                    } else if let ApiError::RateLimited(detail) = &error {
                        guard.reject_pickup_for_cycle(
                            region,
                            CyclePickupRejection::RateLimited(detail.clone()),
                        );
                    }
                    if matches!(error, ApiError::Transport(_)) {
                        guard.session = None;
                    }
                    return Err(error);
                }
            };
            let used_nearby_response = matches!(scope, PickupScope::Nearby(_));
            // 先保存整份地点响应，即使首家店不在其中，也能让后续门店复用。
            // 缺门店时直接补查该店，不再重复发送同样的地点请求。
            if used_nearby_response
                && payload.status == 200
                && let Some(key) = cache_key.as_ref()
            {
                guard.pickup_cache.insert(key.clone(), payload.body.clone());
            }
            (payload, used_nearby_response)
        };
        if used_nearby_response && nearby_response_needs_store_fallback(&payload, store_number) {
            // 地点搜索可能返回空门店、业务无结果，或只返回附近的有限门店。
            // 这些结果都不能代表目标门店不可查询，立即按门店编号补查。
            // 这份单店响应不能放进地点缓存，否则后续同城门店都会误用它。
            if let Some(cache_key) = cache_key.as_ref() {
                guard.unavailable_nearby.insert(cache_key.clone());
            }
            let fallback_scope = PickupScope::Store(store_number.to_string());
            let fallback = {
                let QueryState { session, gate, .. } = &mut *guard;
                session
                    .as_mut()
                    .expect("附近查询期间 Chromium 会话应当存在")
                    .fetch(region, &fallback_scope, &parts, gate)
                    .await
            };
            payload = match fallback {
                Ok(payload) => payload,
                Err(error) => {
                    if let ApiError::Blocked(detail) = &error {
                        guard.reject_pickup_for_cycle(
                            region,
                            CyclePickupRejection::Blocked(detail.clone()),
                        );
                    } else if let ApiError::RateLimited(detail) = &error {
                        guard.reject_pickup_for_cycle(
                            region,
                            CyclePickupRejection::RateLimited(detail.clone()),
                        );
                    }
                    if matches!(error, ApiError::Transport(_)) {
                        guard.session = None;
                    }
                    return Err(error);
                }
            };
            used_nearby_response = false;
        }

        match payload.status {
            200 => {
                let bytes = payload.body.as_bytes();
                if bytes
                    .iter()
                    .find(|byte| !byte.is_ascii_whitespace())
                    .is_some_and(|byte| *byte != b'{' && *byte != b'[')
                {
                    guard.reject_pickup_for_cycle(
                        region,
                        CyclePickupRejection::Blocked("HTTP 200 但响应不是 JSON".into()),
                    );
                    return Err(ApiError::Blocked("HTTP 200 但响应不是 JSON".into()));
                }
                let mut availability = match parse_pickup_message(bytes, store_number) {
                    Ok(availability) => availability,
                    Err(error @ (ApiError::Blocked(_) | ApiError::RateLimited(_))) => {
                        match &error {
                            ApiError::Blocked(detail) => guard.reject_pickup_for_cycle(
                                region,
                                CyclePickupRejection::Blocked(detail.clone()),
                            ),
                            ApiError::RateLimited(detail) => guard.reject_pickup_for_cycle(
                                region,
                                CyclePickupRejection::RateLimited(detail.clone()),
                            ),
                            _ => unreachable!(),
                        }
                        return Err(error);
                    }
                    Err(error) => return Err(error),
                };
                if let Some(session) = guard.session.as_mut() {
                    session.mark_established().await;
                }
                if used_nearby_response && let Some(cache_key) = cache_key {
                    guard.pickup_cache.insert(cache_key, payload.body.clone());
                }
                if let Some(location) = delivery_region {
                    let mut missing_parts: Vec<_> = targets
                        .iter()
                        .filter(|target| {
                            target.kit_part.is_none() && guard.needs_delivery(target, location)
                        })
                        .map(|target| target.part_number.clone())
                        .collect();
                    missing_parts.sort_unstable();
                    missing_parts.dedup();
                    if !missing_parts.is_empty() && !guard.delivery_rejected_this_cycle(region) {
                        for target in targets.iter().filter(|target| {
                            target.kit_part.is_none() && missing_parts.contains(&target.part_number)
                        }) {
                            guard
                                .delivery_attempts
                                .insert(DeliveryCacheKey::new(target, location));
                        }
                        let fetched = {
                            let QueryState { session, gate, .. } = &mut *guard;
                            session
                                .as_mut()
                                .expect("查询期间 Chromium 会话应当存在")
                                .fetch_product_delivery(region, &missing_parts, location, gate)
                                .await
                        };
                        match fetched {
                            Ok(delivery) => {
                                for target in targets.iter().filter(|target| {
                                    target.kit_part.is_none()
                                        && missing_parts.contains(&target.part_number)
                                }) {
                                    let info = delivery
                                        .get(&target.part_number)
                                        .cloned()
                                        .unwrap_or_default();
                                    guard.cache_delivery(target, location, info);
                                }
                            }
                            Err(error) => {
                                eprintln!("普通商品送货查询失败：{error}");
                                if matches!(error, ApiError::Blocked(_) | ApiError::RateLimited(_))
                                {
                                    guard.reject_delivery_for_cycle(region);
                                }
                            }
                        }
                    }
                    for target in targets.iter().filter(|target| target.kit_part.is_some()) {
                        if guard.delivery_rejected_this_cycle(region) {
                            break;
                        }
                        if !guard.needs_delivery(target, location) {
                            continue;
                        }
                        guard
                            .delivery_attempts
                            .insert(DeliveryCacheKey::new(target, location));
                        let fetched = {
                            let QueryState { session, gate, .. } = &mut *guard;
                            session
                                .as_mut()
                                .expect("查询期间 Chromium 会话应当存在")
                                .fetch_watch_delivery(region, target, location, gate)
                                .await
                        };
                        match fetched {
                            Ok(Some(message)) => guard.cache_delivery(
                                target,
                                location,
                                ProductDelivery {
                                    sale_reason: None,
                                    sale_message: Some(message),
                                },
                            ),
                            Ok(None) => {
                                guard.cache_delivery(target, location, ProductDelivery::default())
                            }
                            Err(error) => {
                                eprintln!("Watch 送货查询失败（{}）：{error}", target.part_number);
                                if matches!(error, ApiError::Blocked(_) | ApiError::RateLimited(_))
                                {
                                    guard.reject_delivery_for_cycle(region);
                                }
                            }
                        }
                    }
                    // 即使刷新失败或其他型号配送被拦，也继续显示上次成功取得的送货信息。
                    for target in targets {
                        if let Some(info) = guard.cached_delivery(target, location)
                            && let Some(status) = availability.parts.get_mut(&target.part_number)
                        {
                            info.merge_into(&mut status.pickup_details);
                        }
                    }
                }
                Ok(availability)
            }
            403 | 541 => {
                // 保留专属 Profile、Cookie 和当前会话，显示 Apple 页面便于完成必要的
                // 浏览器校验。只短路本轮同地区；下一轮按用户设置的间隔复用它。
                let detail = format!("HTTP {}", payload.status);
                guard
                    .reject_pickup_for_cycle(region, CyclePickupRejection::Blocked(detail.clone()));
                Err(ApiError::Blocked(detail))
            }
            429 => {
                let detail = "HTTP 429".to_string();
                guard.reject_pickup_for_cycle(
                    region,
                    CyclePickupRejection::RateLimited(detail.clone()),
                );
                Err(ApiError::RateLimited(detail))
            }
            status if status >= 500 => Err(ApiError::Transport(format!(
                "Apple 服务暂时异常：HTTP {status}"
            ))),
            0 => {
                let error = browser_payload_transport_error(&payload.body);
                // 页面里的 fetch 偶发失败并不等于 DevTools 连接或 Chromium 进程已死。
                // 保留会话，让同轮其他门店继续查询；当前门店保持未知，并由下一轮
                // 按用户设置的查询间隔重试。只有无法解析等真正会话故障才重建。
                if !is_page_fetch_failure(&error) {
                    guard.session = None;
                }
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

    async fn plan_cycle(&self, groups: &[Vec<Target>]) {
        self.state.lock().await.nearby_parts = plan_nearby_parts(groups);
    }

    async fn cached_delivery(
        &self,
        target: &Target,
        location: Option<&DeliveryRegion>,
    ) -> Option<ProductDelivery> {
        let location = location?;
        self.state.lock().await.cached_delivery(target, location)
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

    async fn retry_now(&self) {
        self.state.lock().await.retry_now();
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
    use apw_core::model::{Availability, UnknownReason, region_by_locale};

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
            pickup_location: None,
        }
    }

    /// 模拟浏览器的 CDP 边界，不启动 Chromium，也不访问 Apple。
    async fn cdp_fixture(response: Value) -> (AppleChromiumFetcher, tokio::task::JoinHandle<bool>) {
        cdp_responses(vec![response]).await
    }

    async fn cdp_responses(
        responses: Vec<Value>,
    ) -> (AppleChromiumFetcher, tokio::task::JoinHandle<bool>) {
        let (fetcher, peer, _) = cdp_recorded_responses(responses).await;
        (fetcher, peer)
    }

    async fn cdp_recorded_responses(
        responses: Vec<Value>,
    ) -> (
        AppleChromiumFetcher,
        tokio::task::JoinHandle<bool>,
        Arc<Mutex<Vec<Value>>>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
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
                recorded.lock().await.push(request.clone());
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
            _process: ChromiumProcess {
                child: Command::new("rustc")
                    .arg("--version")
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap(),
                process_group_id: None,
            },
            profile_dir: std::env::temp_dir(),
            socket,
            next_command_id: 1,
            locale: Some("zh_CN"),
            established: true,
        };
        (
            AppleChromiumFetcher {
                state: Arc::new(Mutex::new(QueryState {
                    session: Some(session),
                    ..QueryState::default()
                })),
                profile_dir: std::env::temp_dir(),
            },
            peer,
            requests,
        )
    }

    fn nearby_target(store: &str, part: &str) -> Target {
        let mut target = test_target(part);
        target.store_number = store.into();
        target.pickup_location = Some("上海 上海".into());
        target
    }

    fn multi_pickup_response(entries: &[(&str, &[(&str, &str)])]) -> Value {
        let stores: Vec<_> = entries
            .iter()
            .map(|(store, parts)| {
                let parts: serde_json::Map<_, _> = parts
                    .iter()
                    .map(|(part, display)| {
                        (
                            part.to_string(),
                            json!({"partNumber":part, "pickupDisplay":display}),
                        )
                    })
                    .collect();
                json!({"storeNumber":store, "partsAvailability":parts})
            })
            .collect();
        json!({"result":{"result":{"value":{
            "status":200, "body":json!({"body":{"stores":stores}}).to_string()
        }}}})
    }

    fn sent_pairs(request: &Value) -> Vec<(String, String)> {
        let expression = request["params"]["expression"].as_str().unwrap();
        let raw = expression.split_once("const request=").unwrap().1;
        let value = serde_json::Deserializer::from_str(raw)
            .into_iter::<Value>()
            .next()
            .unwrap()
            .unwrap();
        serde_json::from_value(value["pairs"].clone()).unwrap()
    }

    #[tokio::test]
    async fn 调度同城不同型号仅发一次请求且库存不串店() {
        use apw_core::watcher::{Event, Watcher, WatcherConfig};
        let response = multi_pickup_response(&[
            ("R678", &[("A", "available"), ("B", "available")]),
            (
                "R683",
                &[
                    ("A", "unavailable"),
                    ("B", "unavailable"),
                    ("C", "available"),
                ],
            ),
            ("R390", &[("D", "unavailable")]),
        ]);
        let (fetcher, peer, requests) = cdp_recorded_responses(vec![response]).await;
        let (watcher, mut events, engine) = Watcher::new(fetcher.clone(), WatcherConfig::default());
        let task = tokio::spawn(engine);
        watcher
            .set_targets(vec![
                nearby_target("R678", "A"),
                nearby_target("R683", "B"),
                nearby_target("R683", "C"),
                nearby_target("R390", "D"),
            ])
            .await;
        watcher.start().await;
        let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Event::CycleComplete {
                    request_count,
                    reused_response_count,
                    healthy,
                    next_check_in_secs,
                    snapshot,
                    ..
                } = events.recv().await.unwrap()
                {
                    assert_eq!(request_count, 1);
                    assert_eq!(reused_response_count, 2);
                    assert_eq!(next_check_in_secs, 30);
                    assert!(healthy);
                    break snapshot;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(snapshot.len(), 4, "合并请求不能新增监控项");
        for state in snapshot {
            let expected = matches!(state.target.part_number.as_str(), "A" | "C");
            assert_eq!(
                state.availability.is_in_stock(),
                expected,
                "{} {}",
                state.target.store_number,
                state.target.part_number
            );
        }
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 1);
        let pairs = sent_pairs(&requests[0]);
        assert!(pairs.contains(&("location".into(), "上海 上海".into())));
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key.starts_with("parts."))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["A", "B", "C", "D"]
        );
        watcher.stop().await;
        task.abort();
        assert!(!peer.await.unwrap());
    }

    #[test]
    fn 合并型号限制二十个且不同地点地区与套件隔离() {
        let mut groups: Vec<Vec<Target>> = (0..21)
            .map(|i| vec![nearby_target(&format!("R{i}"), &format!("P{i:02}"))])
            .collect();
        let mut overseas = nearby_target("HK", "HK");
        overseas.locale = "zh_HK".into();
        groups.push(vec![overseas]);
        let mut another_city = nearby_target("CD", "CD");
        another_city.pickup_location = Some("四川 成都".into());
        groups.push(vec![another_city]);
        let mut kit = nearby_target("WATCH", "WATCH");
        kit.kit_part = Some("kit".into());
        groups.push(vec![kit]);
        let plan = plan_nearby_parts(&groups);
        let shanghai: Vec<_> = plan
            .iter()
            .filter(|(key, _)| key.0 == "zh_CN" && key.2 == "上海 上海")
            .collect();
        assert_eq!(shanghai.len(), 21);
        let batches: HashSet<_> = shanghai.iter().map(|(_, parts)| *parts).collect();
        assert_eq!(batches.len(), 2);
        for ((_, requested, _), merged) in shanghai {
            assert!(merged.len() <= 20);
            assert!(requested.iter().all(|part| merged.contains(part)));
            assert!(
                !merged
                    .iter()
                    .any(|part| ["CD", "HK", "WATCH"].contains(&part.as_str()))
            );
        }
        assert_eq!(plan.len(), 23);
    }

    #[tokio::test]
    async fn 合并响应缺型号不重复查询且新一轮重新规划() {
        let (fetcher, peer, requests) = cdp_recorded_responses(vec![
            multi_pickup_response(&[
                ("R678", &[("A", "available")]),
                ("R683", &[("A", "unavailable")]),
            ]),
            pickup_response(&["R683"], "C", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let first = vec![nearby_target("R678", "A")];
        let second = vec![nearby_target("R683", "B")];
        fetcher.begin_cycle().await;
        fetcher.plan_cycle(&[first.clone(), second.clone()]).await;
        fetcher.pickup(region, "R678", &first, None).await.unwrap();
        let result = fetcher.pickup(region, "R683", &second, None).await.unwrap();
        assert!(
            !result.parts.contains_key("B"),
            "未返回的型号必须留给引擎标记未知"
        );
        assert_eq!(fetcher.cycle_stats().await.request_count, 1);
        fetcher.begin_cycle().await;
        fetcher.state.lock().await.gate.last_sent = None;
        let changed = vec![nearby_target("R683", "C")];
        fetcher.plan_cycle(std::slice::from_ref(&changed)).await;
        let result = fetcher
            .pickup(region, "R683", &changed, None)
            .await
            .unwrap();
        assert!(!result.parts["C"].availability.is_in_stock());
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let pairs = sent_pairs(&requests[1]);
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key.starts_with("parts."))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["C"]
        );
        assert!(!peer.await.unwrap());
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

    fn pickup_error_response(message: &str) -> Value {
        json!({"result":{"result":{"value":{
            "status": 200,
            "body": json!({"body":{"errorMessage":message}}).to_string()
        }}}})
    }

    fn http_response(status: u16) -> Value {
        json!({"result":{"result":{"value":{
            "status": status,
            "body": ""
        }}}})
    }

    fn page_fetch_failure_response(detail: &str) -> Value {
        json!({"result":{"result":{"value":{
            "status": 0,
            "body": format!("{PAGE_FETCH_FAILURE_MARKER}{detail}")
        }}}})
    }

    fn product_delivery_response(part: &str, display_name: &str) -> Value {
        json!({"result":{"result":{"value":{
            "status": 200,
            "body": json!({
                "body": {
                    "content": {
                        "deliveryMessage": {
                            part: {
                                "regular": {
                                    "deliveryOptionMessages": [{"displayName": display_name}]
                                }
                            }
                        }
                    }
                }
            }).to_string()
        }}}})
    }

    #[test]
    fn 只有明确的地点无结果才触发单店补查() {
        let business_error = BrowserPayload {
            status: 200,
            body: json!({"body":{"errorMessage":"没有与此搜索相关的零售店。请尝试其他搜索内容。"}})
                .to_string(),
        };
        let empty_stores = BrowserPayload {
            status: 200,
            body: json!({"body":{"stores":[]}}).to_string(),
        };
        let missing_store = BrowserPayload {
            status: 200,
            body: json!({"body":{"stores":[{"storeNumber":"R761","partsAvailability":{}}]}})
                .to_string(),
        };
        let unrelated_error = BrowserPayload {
            status: 200,
            body: json!({"body":{"errorMessage":"Product invalid"}}).to_string(),
        };
        let blocked = BrowserPayload {
            status: 541,
            body: String::new(),
        };

        assert!(nearby_response_needs_store_fallback(
            &business_error,
            "R793"
        ));
        assert!(nearby_response_needs_store_fallback(&empty_stores, "R793"));
        assert!(nearby_response_needs_store_fallback(&missing_store, "R793"));
        assert!(!nearby_response_needs_store_fallback(
            &unrelated_error,
            "R793"
        ));
        assert!(!nearby_response_needs_store_fallback(&blocked, "R793"));
    }

    #[tokio::test]
    async fn 显式关闭会话会释放浏览器连接() {
        let (fetcher, peer) = cdp_responses(Vec::new()).await;

        fetcher.shutdown().await;

        assert!(fetcher.state.lock().await.session.is_none());
        assert!(peer.await.unwrap(), "关闭会话应同步关闭 DevTools 连接");
    }

    #[cfg(unix)]
    #[tokio::test]
    #[ignore = "需要本机 Chromium"]
    async fn 真实chromium会话析构后进程组消失() {
        let profile = tempfile::tempdir().unwrap();
        let session = ChromiumSession::start(profile.path().to_path_buf())
            .await
            .unwrap();
        let process_group_id = session._process.process_group_id.unwrap();

        drop(session);

        let result = unsafe { libc::kill(-(process_group_id as i32), 0) };
        assert_eq!(result, -1, "Chromium 进程组不应在会话析构后残留");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
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
                let mut target = test_target("MJXQ4ZA/A");
                target.pickup_location = Some("香港".into());
                let result = fetcher
                    .pickup_message(region, store, &[target], None)
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
    async fn 首店缺失只补查自身且其余门店复用合并响应() {
        let (fetcher, peer, requests) = cdp_recorded_responses(vec![
            multi_pickup_response(&[("R683", &[("B", "unavailable")])]),
            pickup_response(&["R678"], "A", "available"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let first = vec![nearby_target("R678", "A")];
        let second = vec![nearby_target("R683", "B")];
        fetcher.begin_cycle().await;
        fetcher.plan_cycle(&[first.clone(), second.clone()]).await;
        assert!(
            fetcher
                .pickup(region, "R678", &first, None)
                .await
                .unwrap()
                .parts["A"]
                .availability
                .is_in_stock()
        );
        assert!(
            !fetcher
                .pickup(region, "R683", &second, None)
                .await
                .unwrap()
                .parts["B"]
                .availability
                .is_in_stock()
        );
        assert_eq!(
            fetcher.cycle_stats().await,
            CycleStats {
                request_count: 2,
                reused_response_count: 1
            }
        );
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        let pairs = sent_pairs(&requests[1]);
        assert!(pairs.contains(&("store".into(), "R678".into())));
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key.starts_with("parts."))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["A"],
            "单店补查不应携带其他门店的型号"
        );
        assert!(!peer.await.unwrap());
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

        let mut first_target = test_target("one");
        first_target.pickup_location = Some("上海 上海".into());
        let first = fetcher
            .pickup_message(region, "R390", &[first_target], None)
            .await
            .unwrap();
        assert!(first.parts["one"].availability.is_in_stock());
        fetcher.state.lock().await.gate.last_sent = None;
        let mut second_target = test_target("one");
        second_target.pickup_location = Some("上海 上海".into());
        let second = fetcher
            .pickup_message(region, "R683", &[second_target], None)
            .await
            .unwrap();
        assert!(!second.parts["one"].availability.is_in_stock());
        assert_eq!(fetcher.cycle_stats().await.request_count, 2);
        assert_eq!(fetcher.cycle_stats().await.reused_response_count, 0);
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 地点查询无门店时回退单店且同轮跳过失效地点() {
        let message = "没有与此搜索相关的零售店。请尝试其他搜索内容。";
        let (fetcher, peer) = cdp_responses(vec![
            pickup_error_response(message),
            pickup_response(&["R761"], "one", "unavailable"),
            pickup_response(&["R793"], "one", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        fetcher.begin_cycle().await;

        let mut first_target = test_target("one");
        first_target.store_number = "R761".into();
        first_target.pickup_location = Some("广东 深圳".into());
        let first = fetcher
            .pickup_message(region, "R761", &[first_target], None)
            .await
            .unwrap();
        assert_eq!(first.store_number, "R761");
        assert_eq!(fetcher.cycle_stats().await.request_count, 2);

        let key = (
            region.locale,
            vec!["one".to_string()],
            "广东 深圳".to_string(),
        );
        assert!(fetcher.state.lock().await.unavailable_nearby.contains(&key));

        // 避免测试真的等待生产环境的两秒间隔；这里仍会记录一次请求。
        fetcher.state.lock().await.gate.last_sent = None;
        let mut second_target = test_target("one");
        second_target.store_number = "R793".into();
        second_target.pickup_location = Some("广东 深圳".into());
        let second = fetcher
            .pickup_message(region, "R793", &[second_target], None)
            .await
            .unwrap();
        assert_eq!(second.store_number, "R793");
        assert_eq!(fetcher.cycle_stats().await.request_count, 3);
        assert_eq!(fetcher.cycle_stats().await.reused_response_count, 0);

        fetcher.begin_cycle().await;
        assert!(fetcher.state.lock().await.unavailable_nearby.is_empty());
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 单店补查仍无关联门店时保留暂停取货语义() {
        let message = "没有与此搜索相关的零售店。请尝试其他搜索内容。";
        let (fetcher, peer) = cdp_responses(vec![
            pickup_error_response(message),
            pickup_error_response(message),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        fetcher.begin_cycle().await;

        let mut target = test_target("one");
        target.store_number = "R793".into();
        target.pickup_location = Some("广东 深圳".into());
        let error = fetcher
            .pickup_message(region, "R793", &[target], None)
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            ApiError::StorePickupUnavailable { store_number } if store_number == "R793"
        ));
        assert_eq!(fetcher.cycle_stats().await.request_count, 2);
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 送货被拒只跳过同轮且下一轮恢复查询() {
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&["R390"], "watch-one", "available"),
            http_response(541),
            pickup_response(&["R683"], "regular-two", "unavailable"),
            pickup_response(&["R683"], "regular-two", "unavailable"),
            product_delivery_response("regular-two", "明天"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let destination = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        fetcher.begin_cycle().await;

        let mut watch = test_target("watch-one");
        watch.companion_part = Some("band-one".into());
        watch.kit_part = Some("kit-one".into());
        let first = fetcher
            .pickup_message(region, "R390", &[watch], Some(&destination))
            .await
            .unwrap();
        assert!(first.parts["watch-one"].availability.is_in_stock());
        {
            let mut state = fetcher.state.lock().await;
            assert!(state.delivery_rejected_this_cycle(region));
            assert_eq!(
                state.schedule_hint(&[region.locale.to_string()]),
                ScheduleHint::default(),
                "辅助送货失败不能改变取货轮询间隔"
            );
            // 避免测试真的等待生产环境的两秒间隔。
            state.gate.last_sent = None;
        }

        let second = fetcher
            .pickup_message(
                region,
                "R683",
                &[test_target("regular-two")],
                Some(&destination),
            )
            .await
            .unwrap();
        assert!(!second.parts["regular-two"].availability.is_in_stock());
        assert_eq!(
            fetcher.cycle_stats().await.request_count,
            3,
            "送货接口被拒后只应继续取货，不应再次查询送货"
        );

        fetcher.begin_cycle().await;
        {
            let mut state = fetcher.state.lock().await;
            assert!(
                !state.delivery_rejected_this_cycle(region),
                "新一轮必须立即清除送货拒绝标记"
            );
            // 避免测试真的等待生产环境的两秒间隔。
            state.gate.last_sent = None;
        }

        let next_cycle = fetcher
            .pickup_message(
                region,
                "R683",
                &[test_target("regular-two")],
                Some(&destination),
            )
            .await
            .unwrap();
        assert_eq!(
            next_cycle.parts["regular-two"]
                .pickup_details
                .as_ref()
                .and_then(|details| details.sale_message.as_deref()),
            Some("明天"),
            "下一轮必须重新查询并合并预计送货数据"
        );
        assert_eq!(
            fetcher.cycle_stats().await.request_count,
            2,
            "新一轮应重新发出一次取货请求和一次送货请求"
        );
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 缓存取货响应仍会合并配送信息() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        let location = "四川 成都".to_string();
        let part = "one".to_string();
        let destination = DeliveryRegion {
            state: "四川".into(),
            city: "成都".into(),
            district: "锦江区".into(),
        };
        let pickup_body = json!({
            "body": {
                "stores": [{
                    "storeNumber": "R502",
                    "partsAvailability": {
                        &part: {"partNumber": &part, "pickupDisplay": "available"}
                    }
                }]
            }
        })
        .to_string();
        let mut delivery = BTreeMap::new();
        delivery.insert(
            part.clone(),
            ProductDelivery {
                sale_reason: Some("可送货".into()),
                sale_message: Some("明天".into()),
            },
        );
        {
            let mut state = fetcher.state.lock().await;
            state.pickup_cache.insert(
                (region.locale, vec![part.clone()], location.clone()),
                pickup_body,
            );
            state.cache_delivery(
                &test_target(&part),
                &destination,
                delivery.remove(&part).unwrap(),
            );
        }
        let mut target = test_target(&part);
        target.store_number = "R502".into();
        target.pickup_location = Some(location);

        let result = fetcher
            .pickup(region, "R502", &[target], Some(&destination))
            .await
            .unwrap();
        let details = result.parts[&part].pickup_details.as_ref().unwrap();
        assert_eq!(details.sale_reason.as_deref(), Some("可送货"));
        assert_eq!(details.sale_message.as_deref(), Some("明天"));
        assert_eq!(
            fetcher.cycle_stats().await,
            CycleStats {
                request_count: 0,
                reused_response_count: 1,
            }
        );
    }

    #[test]
    fn 送货缓存按时长地址和套件区分且到期保留旧结果() {
        let mut state = QueryState::default();
        let target = test_target("A");
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        state.cache_delivery(&target, &location, ProductDelivery::default());
        assert!(!state.needs_delivery(&target, &location));
        state
            .empty_delivery_cache
            .values_mut()
            .next()
            .unwrap()
            .fetched_at = Instant::now() - EMPTY_DELIVERY_CACHE_TTL;
        assert!(state.needs_delivery(&target, &location));
        assert!(state.cached_delivery(&target, &location).is_none());
        state.cache_delivery(
            &target,
            &location,
            ProductDelivery {
                sale_reason: Some("OK".into()),
                sale_message: Some("明天".into()),
            },
        );
        state.begin_cycle();
        state.retry_now();
        assert!(!state.needs_delivery(&target, &location));
        assert_eq!(
            state
                .cached_delivery(&target, &location)
                .unwrap()
                .sale_message
                .as_deref(),
            Some("明天")
        );
        let other = DeliveryRegion {
            district: "静安区".into(),
            ..location.clone()
        };
        assert!(state.cached_delivery(&target, &other).is_none());
        assert!(state.needs_delivery(&target, &other));
        let mut watch = target.clone();
        watch.kit_part = Some("kit".into());
        watch.companion_part = Some("band".into());
        assert!(state.cached_delivery(&watch, &location).is_none());
        assert!(state.needs_delivery(&watch, &location));
        state.delivery_cache.values_mut().next().unwrap().fetched_at =
            Instant::now() - DELIVERY_CACHE_TTL;
        assert!(state.needs_delivery(&target, &location));
        state.begin_cycle();
        assert_eq!(
            state
                .cached_delivery(&target, &location)
                .unwrap()
                .sale_message
                .as_deref(),
            Some("明天")
        );
        state.cache_delivery(&target, &location, ProductDelivery::default());
        assert!(!state.needs_delivery(&target, &location));
        assert_eq!(
            state
                .cached_delivery(&target, &location)
                .unwrap()
                .sale_message
                .as_deref(),
            Some("明天")
        );
        state
            .empty_delivery_cache
            .values_mut()
            .next()
            .unwrap()
            .fetched_at = Instant::now() - EMPTY_DELIVERY_CACHE_TTL;
        state.begin_cycle();
        assert!(
            state.needs_delivery(&target, &location),
            "空响应不能延长旧日期的一小时有效期"
        );
    }

    #[tokio::test]
    async fn 五家店的空送货结果跨轮短暂复用且到期只补查一次() {
        let region = region_by_locale("zh_CN").unwrap();
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        let stores = ["R390", "R683", "R401", "R402", "R403"];
        for regular in [
            json!({"buyability":{"reason":"COMING_SOON"}}),
            json!({"buyability":{"reason":"NOT_FOR_SALE"}}),
            json!({"deliveryOptionMessages":[{"displayName":"  "}]}),
            Value::Null,
        ] {
            let messages = if regular.is_null() {
                json!({})
            } else {
                json!({"A":{"regular":regular}})
            };
            let empty = json!({"result":{"result":{"value":{
                "status":200,
                "body":json!({"body":{"content":{"deliveryMessage":messages}}}).to_string()
            }}}});
            let (fetcher, peer, requests) = cdp_recorded_responses(vec![
                pickup_response(&stores, "A", "unavailable"),
                empty.clone(),
                pickup_response(&stores, "A", "unavailable"),
                pickup_response(&stores, "A", "unavailable"),
                empty,
            ])
            .await;
            for (cycle, expected) in [(0, 2), (1, 1), (2, 2)] {
                if cycle == 2 {
                    for entry in fetcher.state.lock().await.empty_delivery_cache.values_mut() {
                        entry.fetched_at = Instant::now() - EMPTY_DELIVERY_CACHE_TTL;
                    }
                }
                fetcher.begin_cycle().await;
                fetcher.state.lock().await.gate.last_sent = None;
                for store in stores {
                    let target = nearby_target(store, "A");
                    let result = fetcher
                        .pickup(region, store, &[target], Some(&location))
                        .await
                        .unwrap();
                    assert!(!result.parts["A"].availability.is_in_stock());
                    if let Some(reason) = regular
                        .pointer("/buyability/reason")
                        .and_then(Value::as_str)
                    {
                        assert_eq!(
                            result.parts["A"]
                                .pickup_details
                                .as_ref()
                                .unwrap()
                                .sale_reason
                                .as_deref(),
                            Some(reason)
                        );
                    }
                }
                assert_eq!(fetcher.cycle_stats().await.request_count, expected);
            }
            assert_eq!(requests.lock().await.len(), 5);
            assert!(!peer.await.unwrap());
        }
    }

    #[tokio::test]
    async fn 配送过期后失败或缺项保留旧日期而有效刷新替换日期() {
        let stores = ["R390", "R683"];
        let (fetcher, peer, requests) = cdp_recorded_responses(vec![
            pickup_response(&stores, "A", "available"),
            product_delivery_response("A", "今天"),
            pickup_response(&stores, "A", "available"),
            http_response(500),
            pickup_response(&stores, "A", "available"),
            product_delivery_response("other", "明天"),
            pickup_response(&stores, "A", "available"),
            product_delivery_response("A", "暂无供应"),
            pickup_response(&stores, "A", "available"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        for (cycle, expected, message) in [
            (0, 2, "今天"),
            (1, 2, "今天"),
            (2, 2, "今天"),
            (3, 2, "暂无供应"),
            (4, 1, "暂无供应"),
        ] {
            if cycle == 1 {
                for entry in fetcher.state.lock().await.delivery_cache.values_mut() {
                    entry.fetched_at = Instant::now() - DELIVERY_CACHE_TTL;
                }
            }
            if cycle == 3 {
                for entry in fetcher.state.lock().await.empty_delivery_cache.values_mut() {
                    entry.fetched_at = Instant::now() - EMPTY_DELIVERY_CACHE_TTL;
                }
            }
            fetcher.begin_cycle().await;
            fetcher.state.lock().await.gate.last_sent = None;
            for store in stores {
                let result = fetcher
                    .pickup(region, store, &[nearby_target(store, "A")], Some(&location))
                    .await
                    .unwrap();
                let status = &result.parts["A"];
                assert!(
                    status.availability.is_in_stock(),
                    "送货状态不能覆盖取货结论"
                );
                assert_eq!(
                    status
                        .pickup_details
                        .as_ref()
                        .unwrap()
                        .sale_message
                        .as_deref(),
                    Some(message)
                );
            }
            assert_eq!(fetcher.cycle_stats().await.request_count, expected);
        }
        assert_eq!(requests.lock().await.len(), 9);
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn watch空送货结果跨店跨轮复用且过期后补查() {
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&["R390"], "watch", "available"),
            product_delivery_response("kit", ""),
            pickup_response(&["R683"], "watch", "available"),
            pickup_response(&["R390"], "watch", "available"),
            pickup_response(&["R390"], "watch", "available"),
            product_delivery_response("kit", "明天"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        let mut target = test_target("watch");
        target.kit_part = Some("kit".into());
        target.companion_part = Some("band".into());
        for (index, store, count) in [
            (0, "R390", 2),
            (1, "R683", 3),
            (2, "R390", 1),
            (3, "R390", 2),
        ] {
            if index == 3 {
                for entry in fetcher.state.lock().await.empty_delivery_cache.values_mut() {
                    entry.fetched_at = Instant::now() - EMPTY_DELIVERY_CACHE_TTL;
                }
            }
            if index != 1 {
                fetcher.begin_cycle().await;
            }
            fetcher.state.lock().await.gate.last_sent = None;
            let result = fetcher
                .pickup(
                    region,
                    store,
                    std::slice::from_ref(&target),
                    Some(&location),
                )
                .await
                .unwrap();
            assert_eq!(fetcher.cycle_stats().await.request_count, count);
            if index == 3 {
                assert_eq!(
                    result.parts["watch"]
                        .pickup_details
                        .as_ref()
                        .unwrap()
                        .sale_message
                        .as_deref(),
                    Some("明天")
                );
            }
        }
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 有效期内送货跨轮跨店复用且新增型号只补查缺项() {
        let (fetcher, peer, requests) = cdp_recorded_responses(vec![
            pickup_response(&["R390"], "A", "available"),
            product_delivery_response("A", "明天"),
            pickup_response(&["R683"], "A", "unavailable"),
            multi_pickup_response(&[("R683", &[("A", "unavailable"), ("B", "available")])]),
            product_delivery_response("B", "后天"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        for (store, parts, count) in [
            ("R390", vec!["A"], 2),
            ("R683", vec!["A"], 1),
            ("R683", vec!["A", "B"], 2),
        ] {
            fetcher.begin_cycle().await;
            fetcher.state.lock().await.gate.last_sent = None;
            let targets: Vec<_> = parts.iter().map(|part| test_target(part)).collect();
            let result = fetcher
                .pickup(region, store, &targets, Some(&location))
                .await
                .unwrap();
            assert_eq!(
                result.parts["A"]
                    .pickup_details
                    .as_ref()
                    .unwrap()
                    .sale_message
                    .as_deref(),
                Some("明天")
            );
            assert_eq!(fetcher.cycle_stats().await.request_count, count);
        }
        let requests = requests.lock().await;
        let pairs = sent_pairs(&requests[4]);
        assert_eq!(
            pairs
                .iter()
                .filter(|(key, _)| key.starts_with("parts."))
                .map(|(_, value)| value.as_str())
                .collect::<Vec<_>>(),
            vec!["B"]
        );
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn 取货被拦仍展示送货缓存但不沿用取货结论() {
        use apw_core::watcher::{Event, Watcher, WatcherConfig};
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&["R390"], "A", "available"),
            product_delivery_response("A", "明天"),
            http_response(541),
        ])
        .await;
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        let config = WatcherConfig {
            interval: Duration::from_millis(10),
            delivery_region: Some(location),
            ..WatcherConfig::default()
        };
        let (watcher, mut events, engine) = Watcher::new(fetcher.clone(), config);
        let task = tokio::spawn(engine);
        watcher.set_targets(vec![test_target("A")]).await;
        watcher.start().await;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if let Event::CycleComplete {
                    cycle: 2,
                    snapshot,
                    request_count,
                    ..
                } = events.recv().await.unwrap()
                {
                    let row = &snapshot[0];
                    assert!(row.availability.is_unknown());
                    let details = row.pickup_details.as_ref().unwrap();
                    assert_eq!(details.sale_message.as_deref(), Some("明天"));
                    assert!(details.pickup_display.is_empty());
                    assert!(details.pickup_quote.is_none());
                    assert_eq!(request_count, 1);
                    break;
                }
            }
        })
        .await
        .unwrap();
        watcher.stop().await;
        task.abort();
        assert!(!peer.await.unwrap());
    }

    #[tokio::test]
    async fn watch送货有效期内也只查询一次() {
        let (fetcher, peer) = cdp_responses(vec![
            pickup_response(&["R390"], "watch", "available"),
            product_delivery_response("kit", "2026/10/21 — 免费"),
            pickup_response(&["R390"], "watch", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let location = DeliveryRegion {
            state: "上海".into(),
            city: "上海".into(),
            district: "黄浦区".into(),
        };
        let mut target = test_target("watch");
        target.kit_part = Some("kit".into());
        target.companion_part = Some("band".into());
        for count in [2, 1] {
            fetcher.begin_cycle().await;
            fetcher.state.lock().await.gate.last_sent = None;
            let result = fetcher
                .pickup(
                    region,
                    "R390",
                    std::slice::from_ref(&target),
                    Some(&location),
                )
                .await
                .unwrap();
            assert_eq!(
                result.parts["watch"]
                    .pickup_details
                    .as_ref()
                    .unwrap()
                    .sale_message
                    .as_deref(),
                Some("2026/10/21 — 免费")
            );
            assert_eq!(fetcher.cycle_stats().await.request_count, count);
        }
        assert!(!peer.await.unwrap());
    }

    #[test]
    fn 真实请求只保留两秒最小间隔() {
        let start = Instant::now();
        let mut gate = RequestGate {
            last_sent: Some(start),
            ..RequestGate::default()
        };
        assert_eq!(gate.next_delay(start), MIN_REQUEST_INTERVAL);
        assert_eq!(
            gate.next_delay(start + Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            gate.next_delay(start + MIN_REQUEST_INTERVAL),
            Duration::ZERO
        );
    }

    #[test]
    fn 正常轮次不会因历史请求量额外延后() {
        let mut state = QueryState::default();
        state.gate.cycle_request_count = 4;
        assert_eq!(
            state.schedule_hint(&["zh_CN".to_string()]),
            ScheduleHint::default()
        );

        state.begin_cycle();
        state.gate.cycle_request_count = 4;
        assert_eq!(
            state.schedule_hint(&["zh_CN".to_string()]),
            ScheduleHint::default()
        );
    }

    #[tokio::test]
    async fn 服务端五百错误不改变正常轮询间隔() {
        let (fetcher, peer) = cdp_fixture(json!({
            "result": {"result": {"value": {"status": 503, "body": "{}"}}}
        }))
        .await;
        let region = region_by_locale("zh_CN").unwrap();
        let result = fetcher
            .pickup_message(region, "R390", &[test_target("MG6X4CH/A")], None)
            .await;
        assert!(matches!(result, Err(ApiError::Transport(_))));
        assert_eq!(
            fetcher.schedule_hint(&[region.locale.to_string()]).await,
            ScheduleHint::default()
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
    async fn 拒绝响应短路同轮同地区且下一轮恢复探测() {
        for status in [429, 403, 541] {
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
            match status {
                429 => assert!(matches!(result, Err(ApiError::RateLimited(_)))),
                403 | 541 => assert!(matches!(result, Err(ApiError::Blocked(_)))),
                _ => unreachable!(),
            }

            let second = tokio::time::timeout(
                Duration::from_millis(100),
                fetcher.pickup_message(
                    region_by_locale("zh_CN").unwrap(),
                    "R683",
                    &[test_target("MG6X4CH/A")],
                    None,
                ),
            )
            .await
            .expect("同轮后续门店应直接复用拒绝结果，不能重新启动 Chromium");
            match status {
                429 => assert!(matches!(second, Err(ApiError::RateLimited(_)))),
                403 | 541 => assert!(matches!(second, Err(ApiError::Blocked(_)))),
                _ => unreachable!(),
            }
            assert_eq!(
                fetcher.cycle_stats().await.request_count,
                1,
                "HTTP {status} 后同轮同地区不得继续发请求"
            );
            assert_eq!(
                fetcher.schedule_hint(&["zh_CN".to_string()]).await,
                ScheduleHint::default(),
                "HTTP {status} 不得延长用户设定的轮询间隔"
            );

            fetcher.begin_cycle().await;
            assert!(
                fetcher
                    .state
                    .lock()
                    .await
                    .pickup_rejection(region_by_locale("zh_CN").unwrap())
                    .is_none(),
                "HTTP {status} 的短路标记只能存在于当前轮次"
            );
            assert!(
                !peer.await.unwrap(),
                "HTTP {status} 不应销毁持久会话或丢失 Cookie"
            );
        }
    }

    #[tokio::test]
    async fn http_541后下一轮复用原会话而不是换新身份() {
        let (fetcher, peer) = cdp_responses(vec![
            http_response(541),
            pickup_response(&["R390"], "MG6X4CH/A", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();

        fetcher.begin_cycle().await;
        let first = fetcher
            .pickup_message(region, "R390", &[test_target("MG6X4CH/A")], None)
            .await;
        assert!(matches!(first, Err(ApiError::Blocked(_))));
        assert!(fetcher.state.lock().await.session.is_some());

        fetcher.begin_cycle().await;
        fetcher.state.lock().await.gate.last_sent = None;
        let second = fetcher
            .pickup_message(region, "R390", &[test_target("MG6X4CH/A")], None)
            .await
            .expect("下一轮应复用同一 DevTools 连接继续探测");

        assert!(
            !second.parts["MG6X4CH/A"].availability.is_in_stock(),
            "第二轮的正常响应应能解析"
        );
        assert!(fetcher.state.lock().await.session.is_some());
        assert!(
            !peer.await.unwrap(),
            "从 541 恢复到正常响应的过程不应断开原 Chromium 会话"
        );
    }

    #[tokio::test]
    async fn 页面请求瞬时失败不会销毁浏览器会话或影响下一门店() {
        let (fetcher, peer) = cdp_responses(vec![
            page_fetch_failure_response("TypeError: Failed to fetch"),
            pickup_response(&["R683"], "MG6X4CH/A", "unavailable"),
        ])
        .await;
        let region = region_by_locale("zh_CN").unwrap();

        fetcher.begin_cycle().await;
        let first = fetcher
            .pickup_message(region, "R580", &[test_target("MJYK4CH/A")], None)
            .await;
        assert!(matches!(
            first,
            Err(ApiError::Transport(ref detail))
                if detail == "Apple 页面请求失败：TypeError: Failed to fetch"
        ));
        assert!(
            fetcher.state.lock().await.session.is_some(),
            "页面 fetch 失败时 DevTools 会话仍然可用"
        );

        fetcher.state.lock().await.gate.last_sent = None;
        let second = fetcher
            .pickup_message(region, "R683", &[test_target("MG6X4CH/A")], None)
            .await
            .expect("下一门店应继续复用原会话查询");
        assert!(!second.parts["MG6X4CH/A"].availability.is_in_stock());
        assert!(
            !peer.await.unwrap(),
            "页面 fetch 失败后不应断开原 Chromium 会话"
        );
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
    fn 取货请求只发送地点与型号而不混入送货地区() {
        let parts = vec!["MJYL4CH/A".into(), "MJYM4CH/A".into()];
        let pairs = pickup_query_pairs(&PickupScope::Nearby("四川 成都".into()), &parts);
        assert!(pairs.contains(&("location".into(), "四川 成都".into())));
        assert!(pairs.contains(&("parts.0".into(), "MJYL4CH/A".into())));
        assert!(pairs.contains(&("parts.1".into(), "MJYM4CH/A".into())));
        for forbidden in ["store", "searchNearby", "state", "city", "district", "fae"] {
            assert!(
                pairs.iter().all(|(key, _)| key != forbidden),
                "取货请求不应携带 {forbidden}"
            );
        }
    }

    #[test]
    fn 普通商品送货响应可独立解析() {
        let raw = r#"{"body":{"content":{"deliveryMessage":{"MJYL4CH/A":{"regular":{"buyability":{"reason":"OK"},"deliveryOptionMessages":[{"displayName":"15 个工作日"}]}}}}}}"#;
        let delivery = parse_product_delivery(raw.as_bytes()).unwrap();
        assert_eq!(delivery["MJYL4CH/A"].sale_reason.as_deref(), Some("OK"));
        assert_eq!(
            delivery["MJYL4CH/A"].sale_message.as_deref(),
            Some("15 个工作日")
        );
    }

    #[test]
    fn 普通商品送货请求分别传递省市区而不使用取货location() {
        let destination = DeliveryRegion {
            state: "江苏".into(),
            city: "苏州".into(),
            district: "吴江区".into(),
        };
        let pairs = delivery_query_pairs(&["MJYM4CH/A".into()], &destination);

        assert!(pairs.contains(&("state".into(), "江苏".into())));
        assert!(pairs.contains(&("city".into(), "苏州".into())));
        assert!(pairs.contains(&("district".into(), "吴江区".into())));
        assert!(pairs.contains(&("parts.0".into(), "MJYM4CH/A".into())));
        for forbidden in ["store", "searchNearby", "location"] {
            assert!(
                pairs.iter().all(|(key, _)| key != forbidden),
                "送货请求不应携带 {forbidden}"
            );
        }
    }

    #[tokio::test]
    #[ignore = "送货地址参数对照，需要本机 Chromium 和 Apple 官网网络"]
    async fn diagnose_delivery_address_parameters() {
        let region = region_by_locale("zh_CN").unwrap();
        let profile = tempfile::tempdir().unwrap();
        let mut session = ChromiumSession::start(profile.path().to_path_buf())
            .await
            .unwrap();
        session.ensure_region(region).await.unwrap();
        let location = DeliveryRegion {
            state: "北京".into(),
            city: "北京".into(),
            district: "石景山区".into(),
        };
        let mut gate = RequestGate::default();
        for separate in [false, true] {
            let mut pairs = delivery_query_pairs(&["MJYJ4CH/A".into()], &location);
            if !separate {
                pairs.retain(|(key, _)| !["state", "city", "district"].contains(&key.as_str()));
                pairs.push((
                    "location".into(),
                    format!("{} {} {}", location.state, location.city, location.district),
                ));
            }
            gate.acquire().await;
            let pairs = serde_json::to_string(&pairs).unwrap();
            let url = serde_json::to_string(&region.delivery_message_url()).unwrap();
            let expression = format!(
                r#"(async()=>{{const url=new URL({url});for(const [k,v] of {pairs})url.searchParams.append(k,v);const r=await fetch(url,{{credentials:'same-origin',headers:{{'Accept':'application/json, text/javascript, */*; q=0.01','X-Requested-With':'XMLHttpRequest'}}}});return {{status:r.status,body:await r.text()}};}})()"#
            );
            let result = session.evaluate(&expression, true).await.unwrap();
            let body: Value =
                serde_json::from_str(result["body"].as_str().unwrap_or("")).unwrap_or(Value::Null);
            let regular = &body["body"]["content"]["deliveryMessage"]["MJYJ4CH/A"]["regular"];
            eprintln!(
                "separate={separate} status={} message={} type={} address={}",
                result["status"],
                regular["deliveryOptionMessages"][0]["displayName"],
                regular["messageType"],
                regular["address"]
            );
            if separate {
                assert_eq!(result["status"], 200);
                assert_eq!(
                    regular["address"],
                    serde_json::to_value(&location).unwrap(),
                    "Apple 必须明确识别所选省市区，不能只断言有送货文字"
                );
                assert_eq!(regular["messageType"], "Delivery");
            }
        }
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实普通商品能按省市区查询送货时间() {
        let region = region_by_locale("zh_CN").unwrap();
        let profile = tempfile::tempdir().unwrap();
        let mut session = ChromiumSession::start(profile.path().to_path_buf())
            .await
            .unwrap();
        let mut gate = RequestGate::default();
        session.ensure_region(region).await.unwrap();
        let destination = DeliveryRegion {
            state: "北京".into(),
            city: "北京".into(),
            district: "石景山区".into(),
        };
        let delivery = session
            .fetch_product_delivery(region, &["MJYM4CH/A".into()], &destination, &mut gate)
            .await
            .expect("普通商品送货请求应成功");
        let message = delivery
            .get("MJYM4CH/A")
            .and_then(|details| details.sale_message.as_deref())
            .expect("普通商品应返回送货时间");

        eprintln!("普通商品北京石景山区送货：{message}");
        assert!(!message.trim().is_empty());
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
    fn chromium使用应用专属持久资料目录() {
        let path = chromium_profile_dir();
        assert!(path.ends_with(CHROMIUM_PROFILE_DIR));
        assert!(
            path.to_string_lossy()
                .contains("apple-store-inventory-monitor")
        );
    }

    #[cfg(unix)]
    #[test]
    fn 能从chromium资料目录锁中识别专属进程号() {
        use std::os::unix::fs::symlink;

        let profile = tempfile::tempdir().unwrap();
        symlink(
            "MacBook-Sue.local-29642",
            profile.path().join("SingletonLock"),
        )
        .unwrap();
        assert_eq!(profile_lock_pid(profile.path()), Some(29642));
    }

    #[test]
    fn 会话启动失败只短路当前轮次() {
        let mut state = QueryState {
            session_start_failure: Some("Chromium 启动后立即退出".into()),
            ..QueryState::default()
        };
        assert_eq!(
            state.session_start_failure.as_deref(),
            Some("Chromium 启动后立即退出")
        );

        state.begin_cycle();

        assert!(state.session_start_failure.is_none());
        assert_eq!(
            state.schedule_hint(&["zh_CN".into()]),
            ScheduleHint::default(),
            "启动失败不能改变用户设置的轮询间隔"
        );
    }

    #[test]
    fn 中国送货使用购买页暖场而海外使用地区首页() {
        for locale in [
            "zh_CN", "zh_HK", "zh_TW", "ja_JP", "en_SG", "en_AU", "en_MY",
        ] {
            let region = region_by_locale(locale).expect("地区应当受支持");
            let url = region.session_page_url();

            if locale == "zh_CN" {
                assert!(url.contains("/shop/buy-"));
            } else {
                assert_eq!(url, format!("{}/", region.base_url));
                assert!(!url.contains("/shop/"));
            }
        }
    }

    #[tokio::test]
    async fn 海外会话就绪不依赖中国区cookie名称() {
        let ready = json!({
            "result": {
                "result": {
                    "value": r#"{"readyState":"complete"}"#
                }
            }
        });
        let (fetcher, peer) = cdp_responses(vec![json!({"result": {}}), ready]).await;
        let region = region_by_locale("en_AU").unwrap();

        {
            let mut state = fetcher.state.lock().await;
            state
                .session
                .as_mut()
                .unwrap()
                .ensure_region(region)
                .await
                .unwrap();
            assert_eq!(state.session.as_ref().unwrap().locale, Some("en_AU"));
        }

        fetcher.shutdown().await;
        assert!(peer.await.unwrap());
    }

    #[tokio::test]
    async fn 地区页面导航错误立即返回而不是等待超时() {
        let (fetcher, peer) = cdp_responses(vec![json!({
            "result": {"errorText": "net::ERR_CONNECTION_CLOSED"}
        })])
        .await;
        let region = region_by_locale("en_AU").unwrap();

        let error = {
            let mut state = fetcher.state.lock().await;
            state
                .session
                .as_mut()
                .unwrap()
                .ensure_region(region)
                .await
                .unwrap_err()
        };

        assert!(matches!(
            error,
            ApiError::Transport(detail) if detail.contains("ERR_CONNECTION_CLOSED")
        ));
        fetcher.shutdown().await;
        assert!(peer.await.unwrap());
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

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 澳洲官网网络"]
    async fn 真实澳大利亚首页会话可以查询brisbane取货() {
        let region = region_by_locale("en_AU").expect("应当有澳大利亚地区配置");
        let fetcher = AppleChromiumFetcher::new();
        let mut target = test_target("MJXT4X/A");
        target.locale = "en_AU".into();
        target.store_number = "R466".into();
        target.store_title = "Queensland-Brisbane".into();
        target.pickup_location = Some("Brisbane".into());

        let result = fetcher
            .pickup(region, "R466", &[target.clone()], None)
            .await
            .expect("澳大利亚首页会话应能完成取货查询");
        let status = result
            .parts
            .get(&target.part_number)
            .expect("澳大利亚响应应包含请求型号");

        assert_eq!(result.store_number, "R466");
        assert!(!status.availability.is_unknown());
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 澳洲官网网络"]
    async fn 真实澳大利亚未接入取货的chermside明确返回暂无数据() {
        let region = region_by_locale("en_AU").expect("应当有澳大利亚地区配置");
        let fetcher = AppleChromiumFetcher::new();
        let mut target = test_target("MJXT4X/A");
        target.locale = "en_AU".into();
        target.store_number = "R384".into();
        target.store_title = "Queensland-Chermside".into();
        target.pickup_location = Some("Chermside".into());

        let error = fetcher
            .pickup(region, "R384", &[target], None)
            .await
            .expect_err("Apple 当前没有提供 Chermside 的在线取货数据");

        assert!(matches!(
            error,
            ApiError::StorePickupUnavailable { store_number } if store_number == "R384"
        ));
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实成都地点一次覆盖万象城和太古里() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        fetcher.begin_cycle().await;
        let stores = [("R502", "成都-成都万象城"), ("R580", "成都-成都太古里")];
        for (store, title) in stores {
            let mut target = test_target("MJYN4CH/A");
            target.store_number = store.into();
            target.store_title = title.into();
            target.pickup_location = Some("四川 成都".into());
            let result = fetcher
                .pickup(region, store, &[target], None)
                .await
                .unwrap_or_else(|error| panic!("成都门店 {store} 查询失败：{error}"));
            assert_eq!(result.store_number, store);
            assert!(result.parts.contains_key("MJYN4CH/A"));
        }
        let stats = fetcher.cycle_stats().await;
        assert_eq!(stats.request_count, 1, "成都两店应由一次地点查询覆盖");
        assert_eq!(stats.reused_response_count, 1);
    }

    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实深圳三店在地点无结果时仍能按门店补查() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        fetcher.begin_cycle().await;
        let stores = [
            ("R761", "深圳-深圳万象城"),
            ("R484", "深圳-深圳益田假日广场"),
            ("R793", "深圳-前海壹方城"),
        ];

        for (store, title) in stores {
            let mut target = test_target("MJYN4CH/A");
            target.store_number = store.into();
            target.store_title = title.into();
            target.pickup_location = Some("广东 深圳".into());
            let result = fetcher
                .pickup(region, store, &[target], None)
                .await
                .unwrap_or_else(|error| panic!("深圳门店 {store} 查询失败：{error}"));
            assert_eq!(result.store_number, store);
            assert!(result.parts.contains_key("MJYN4CH/A"));
        }

        let stats = fetcher.cycle_stats().await;
        assert!(
            (1..=4).contains(&stats.request_count),
            "深圳三店应由一次地点查询覆盖，或由一次地点探测加三次单店补查完成：{stats:?}"
        );
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
    async fn 真实apple_watch套件能按省市区查询明确送货时效() {
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
        assert!(!message.trim().is_empty(), "送货时效不应为空");
        assert!(!message.contains('周'), "不应退回模糊周范围：{message}");
    }

    /// 用户界面回归：Series 12 表壳必须和页面默认表带组成套件后再查送货，
    /// 否则取货接口只会留下“2-3 周”这种不精确的通用文案。
    #[tokio::test]
    #[ignore = "需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实series_12默认表带能按浦东新区查询明确送货时效() {
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
        assert!(!message.trim().is_empty(), "送货时效不应为空");
        assert!(!message.contains('周'), "不应退回模糊周范围：{message}");
    }

    #[tokio::test]
    #[ignore = "用户现场 Series 12 双型号诊断，需要本机 Chromium 与 Apple 官网网络"]
    async fn 真实series_12双型号取货响应可以解析() {
        let region = region_by_locale("zh_CN").unwrap();
        let fetcher = AppleChromiumFetcher::new();
        let targets: Vec<_> = ["MJKE4CH/B", "MJKD4CH/B"]
            .into_iter()
            .map(|part| {
                let mut target = test_target(part);
                target.companion_part = Some("MJUY4FE/A".into());
                target.kit_part = Some("Z0YQ".into());
                target
            })
            .collect();
        let destination = DeliveryRegion {
            state: "江苏".into(),
            city: "苏州".into(),
            district: "吴江区".into(),
        };
        let result = fetcher
            .pickup(region, "R678", &targets, Some(&destination))
            .await
            .expect("Series 12 双型号查询应成功");
        eprintln!("Series 12 双型号取货响应：{result:#?}");
        for target in &targets {
            let status = result
                .parts
                .get(&target.part_number)
                .unwrap_or_else(|| panic!("响应缺少 {}", target.part_number));
            assert_eq!(
                status.availability,
                Availability::Unknown(UnknownReason::PickupPending),
                "{} 应识别为正常的待开放取货状态",
                target.part_number
            );
            assert!(!status.availability.is_failure());
        }
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
        let profile = tempfile::tempdir().unwrap();
        let mut session = ChromiumSession::start(profile.path().to_path_buf())
            .await
            .unwrap();
        let mut gate = RequestGate::default();
        for (store, part) in [
            ("R581", "MFA04CH/B"),
            ("R683", "MG8X4CH/A"),
            ("R581", "MG6W4CH/A"),
            ("R683", "MJYH4CH/A"),
        ] {
            let scope = PickupScope::Store(store.to_string());
            let payload = session
                .fetch(region, &scope, &[part.to_string()], &mut gate)
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
        let profile = tempfile::tempdir().unwrap();
        let mut session = ChromiumSession::start(profile.path().to_path_buf())
            .await
            .unwrap();
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
            let scope = PickupScope::Store("R359".into());
            let payload = session
                .fetch(region, &scope, &parts, &mut gate)
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
