//! 到货提醒的发送渠道。
//!
//! 这里只做「把一条通知送出去」这一件事：[`Bark`] 远程推送、[`Sound`] 本地
//! 提示音，以及把若干渠道并发组合起来的 [`Multi`]。
//!
//! 刻意**不**包含系统桌面通知：那需要界面框架的句柄，放进来会让整个 crate
//! 无法脱离 GUI 单独测试，也会把 UI 依赖传染给调度层。
//!
//! # 与状态判定的边界
//!
//! 通知发不出去**不影响**任何 [`crate::model::Availability`]。这条边界必须守住：
//! 推送失败是渠道自己的问题，绝不能反过来改写库存判定 —— 尤其不能因为
//! 「提醒没发成功」就把已经确认的「有货」退回成别的状态。所以本模块的所有
//! 失败都收敛在 [`NotifyError`] 里，与库存状态机没有任何转换通道。
//!
//! # 未配置不是错误
//!
//! 用户没填 Bark 地址、这份构建里没有内嵌音频，都属于「这个渠道没开」，
//! `notify` 直接返回 `Ok(())`。只有「开了但没成功」才是错误 —— 否则界面上
//! 会一直挂着一条用户根本没打算开的功能的报错。

use std::future::Future;
use std::io::Cursor;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::task::Poll;
use std::time::{Duration, Instant};

use reqwest::Url;
use rodio::Source;
use rodio::buffer::SamplesBuffer;
use rodio::source::UniformSourceIterator;

/// 内嵌的提示音。
///
/// 内嵌而不是运行时读文件：上游把 mp3 放在可执行文件旁边，用户换个目录启动就
/// 没声了，而「没声」恰恰是这个程序唯一不能出的岔子。
pub const ALERT_MP3: &[u8] = include_bytes!("../assets/alert.mp3");

/// 一条待发送的通知。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// 标题，如「到货提醒」。
    pub title: String,
    /// 正文，如「上海-环球港 iPhone 17 512GB 黑色 有货」。
    pub body: String,
    /// 点开通知后要打开的地址，通常是该地区的购物袋页面；可以为空。
    pub url: Option<String>,
}

impl Notification {
    pub fn new(title: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            url: None,
        }
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }
}

/// 一个通知渠道。
///
/// 实现必须遵守两条约定：渠道未被用户配置时返回 `Ok(())`（「没开这个功能」
/// 不是错误）；一切失败以 [`NotifyError`] 返回，并在错误里带上自己的渠道名，
/// 否则 [`Multi`] 汇总出来的错误就说不清是哪一路没通。
///
/// 用泛型约束而不是 trait object，与 [`crate::apple::Fetcher`] 保持一致：
/// async fn in trait 在泛型位置可以直接写，做成 `dyn` 还得装箱 future。
/// [`Multi`] 内部确实需要装箱，那由本模块自己的 [`DynNotifier`] 承担，
/// 不向外暴露，也不引第三方宏。
pub trait Notifier: Send + Sync {
    /// 渠道名，只用于日志与错误信息。
    fn name(&self) -> &str;

    /// 发送一条通知。
    fn notify(
        &self,
        n: &Notification,
    ) -> impl std::future::Future<Output = Result<(), NotifyError>> + Send;
}

/// 通知发送失败的分类。
///
/// 每个变体都带着渠道名，独立拿到一个错误也能说清是谁失败了 —— [`Multi`] 把
/// 多个错误装进 [`NotifyError::Multiple`] 时不必再额外贴标签。
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    /// 渠道配置本身有问题（地址写错、内容为空等），重试多少次结果都一样。
    #[error("{channel}：{detail}")]
    Config { channel: String, detail: String },

    /// 网络层面的失败：连不上、超时、TLS 出错等。
    #[error("{channel}：请求失败：{detail}")]
    Transport { channel: String, detail: String },

    /// 服务器用状态码明确拒绝了这次推送。
    #[error("{channel}：服务器返回 HTTP {status}：{body}")]
    Rejected {
        channel: String,
        status: u16,
        body: String,
    },

    /// 服务器回了 200，但在响应体里报了业务错误。
    #[error("{channel}：服务器返回错误 {code}：{message}")]
    Remote {
        channel: String,
        code: i64,
        message: String,
    },

    /// 本地设备（目前只有音频）不可用。
    #[error("{channel}：{detail}")]
    Device { channel: String, detail: String },

    /// 多个渠道同时失败时的汇总。
    #[error("{} 个通知渠道失败：{}", .0.len(), join_errors(.0))]
    Multiple(Vec<NotifyError>),
}

impl NotifyError {
    /// 出问题的渠道名。汇总错误没有单一渠道，返回 `None`。
    pub fn channel(&self) -> Option<&str> {
        match self {
            Self::Config { channel, .. }
            | Self::Transport { channel, .. }
            | Self::Rejected { channel, .. }
            | Self::Remote { channel, .. }
            | Self::Device { channel, .. } => Some(channel),
            Self::Multiple(_) => None,
        }
    }

    /// 展开成所有失败渠道的名字，嵌套的 [`Multi`] 也会被摊平。
    ///
    /// 界面据此告诉用户「哪一路没通」。只给一句拼起来的长错误串是不够的：
    /// 用户需要知道的是「Bark 没发出去，但提示音响了」还是反过来。
    pub fn failed_channels(&self) -> Vec<&str> {
        match self {
            Self::Multiple(errors) => errors.iter().flat_map(Self::failed_channels).collect(),
            other => other.channel().into_iter().collect(),
        }
    }
}

fn join_errors(errors: &[NotifyError]) -> String {
    errors
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("；")
}

// ---------------------------------------------------------------------------
// Bark
// ---------------------------------------------------------------------------

const BARK: &str = "Bark";

/// Bark 服务器的默认单次请求超时。
///
/// Go 版用 `http.Get` 走 `http.DefaultClient`，而它没有任何超时：Bark 服务器
/// 只连上不回包时，那次调用会一直挂着。这里无论调用方传进来的客户端怎么配，
/// 都在请求上再压一个上限。
const BARK_TIMEOUT: Duration = Duration::from_secs(10);

/// 响应体读取上限。错误信息通常只有几十字节，出错的服务器却可能返回整页 HTML。
const BARK_MAX_BODY: usize = 8 << 10;

/// 通过 Bark 服务器向 iOS 设备推送通知。
#[derive(Debug, Clone)]
pub struct Bark {
    base_url: String,
    http: reqwest::Client,
    timeout: Duration,
}

impl Bark {
    /// `base_url` 是 Bark App 里给出的推送地址，形如
    /// `https://api.day.app/<设备key>`，也可以是自建服务器地址。
    ///
    /// 为空或全是空白表示用户没有配置，此时 [`Notifier::notify`] 直接返回
    /// `Ok(())`，不发任何请求。
    ///
    /// `http` 应当传入与其他出站请求共用的客户端，以复用连接池 —— 上游为每次
    /// 请求新建客户端，空闲连接持续堆积。
    pub fn new(base_url: String, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.trim().to_string(),
            http,
            timeout: BARK_TIMEOUT,
        }
    }

    /// 覆盖默认的单次请求超时。
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// 用户是否配置了 Bark。
    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty()
    }
}

impl Notifier for Bark {
    fn name(&self) -> &str {
        BARK
    }

    async fn notify(&self, n: &Notification) -> Result<(), NotifyError> {
        if !self.is_configured() {
            return Ok(());
        }

        let url = build_bark_url(&self.base_url, n)?;

        let mut resp = self
            .http
            .get(url)
            .timeout(self.timeout)
            .header(reqwest::header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| NotifyError::Transport {
                channel: BARK.to_string(),
                detail: safe_transport_detail(&e),
            })?;

        let status = resp.status();
        let body = read_capped(&mut resp, BARK_MAX_BODY).await?;

        // 上游拿到响应后连状态码都不看：设备 key 写错、服务器返回 400 时用户
        // 毫无察觉，一直以为推送生效了，真到货那天才发现什么也没收到。
        if !status.is_success() {
            return Err(NotifyError::Rejected {
                channel: BARK.to_string(),
                status: status.as_u16(),
                body: redact_bark_secret(&summarize(&body), &self.base_url),
            });
        }

        // 部分 Bark 部署会在 HTTP 200 的响应体里用 code 字段报错，一并检查。
        if let Some((code, message)) = bark_business_error(&body) {
            return Err(NotifyError::Remote {
                channel: BARK.to_string(),
                code,
                message: redact_bark_secret(&message, &self.base_url),
            });
        }
        Ok(())
    }
}

/// 分块读取 Bark 的响应体，读满上限就停手。
async fn read_capped(resp: &mut reqwest::Response, max: usize) -> Result<Vec<u8>, NotifyError> {
    let mut body = Vec::new();
    while body.len() < max {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                let room = max - body.len();
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
            }
            Ok(None) => break,
            Err(e) => {
                return Err(NotifyError::Transport {
                    channel: BARK.to_string(),
                    detail: format!("读取响应失败：{e}"),
                });
            }
        }
    }
    Ok(body)
}

/// 从 Bark 的 JSON 响应里挑出业务错误码。
fn bark_business_error(body: &[u8]) -> Option<(i64, String)> {
    #[derive(serde::Deserialize)]
    struct Payload {
        #[serde(default)]
        code: Option<i64>,
        #[serde(default)]
        message: Option<String>,
    }

    let payload: Payload = serde_json::from_slice(body).ok()?;
    let code = payload.code?;
    // code 缺失或为 0 说明这个部署压根不用这个字段，不能当成失败。
    (code != 0 && !(200..300).contains(&code)).then(|| (code, payload.message.unwrap_or_default()))
}

/// 把响应体压成一行短文本，用于错误信息。
fn summarize(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return "(空响应)".to_string();
    }
    const LIMIT: usize = 120;
    match text.char_indices().nth(LIMIT) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// reqwest 的错误文本通常会附带完整请求 URL，而 Bark 的路径里就是设备密钥。
/// 对用户只保留可操作的网络分类，不把 URL 搬进日志和界面。
fn safe_transport_detail(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "请求超时".to_string()
    } else if error.is_connect() {
        "无法连接推送服务器".to_string()
    } else if error.is_redirect() {
        "推送服务器重定向异常".to_string()
    } else if let Some(status) = error.status() {
        format!("HTTP {}", status.as_u16())
    } else {
        "网络请求异常".to_string()
    }
}

/// 防止 Bark 服务端把请求地址或设备 key 原样回显到错误响应中。
fn redact_bark_secret(detail: &str, base_url: &str) -> String {
    let mut redacted = detail.replace(base_url, "[Bark 地址已隐藏]");
    if let Ok(url) = Url::parse(base_url) {
        for segment in url.path_segments().into_iter().flatten() {
            if !segment.is_empty() {
                redacted = redacted.replace(segment, "***");
            }
        }
    }
    redacted
}

/// 组装 Bark 推送地址。
///
/// Bark 的路径形式是 `/<设备key>/<标题>/<正文>`，另把要打开的链接放进 `url`
/// 查询参数。Go 版这里用 `fmt.Sprintf("%s/%s/%s?url=%s", ...)` 直接拼，中文、
/// 空格、斜杠一概不转义 —— 拼出来的 URL 本身就是非法的，标题里只要有一个「/」
/// 整条路径的语义就变了（正文被顶到标题位置），末尾的购物袋地址也没转义，
/// 其中的 `&` 会把后面的参数整段截断。
fn build_bark_url(base_url: &str, n: &Notification) -> Result<Url, NotifyError> {
    let mut u = Url::parse(base_url).map_err(|e| config_err(format!("地址无法解析：{e}")))?;

    if !matches!(u.scheme(), "http" | "https") || u.host_str().unwrap_or_default().is_empty() {
        return Err(config_err(
            "地址必须是以 http:// 或 https:// 开头的完整地址".to_string(),
        ));
    }

    // 已经是转义形态的路径，原样拿来当前缀，不重新编码。
    let base_path = u.path().trim_end_matches('/').to_string();
    if base_path.trim_start_matches('/').is_empty() {
        return Err(config_err(
            "地址里缺少设备 key，应形如 https://api.day.app/<设备key>".to_string(),
        ));
    }

    // 只给一段时 Bark 把它当作正文，因此空标题要整段跳过，
    // 而不是留下一个空路径段拼出 `//`。
    let mut segments = Vec::with_capacity(2);
    for text in [n.title.trim(), n.body.trim()] {
        if !text.is_empty() {
            segments.push(encode_path_segment(text));
        }
    }
    if segments.is_empty() {
        return Err(config_err("通知的标题和正文都为空".to_string()));
    }
    u.set_path(&format!("{base_path}/{}", segments.join("/")));

    let link = n
        .url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(percent_encode);
    let query = merge_query(u.query(), link.as_deref());
    u.set_query(query.as_deref());

    Ok(u)
}

fn config_err(detail: String) -> NotifyError {
    NotifyError::Config {
        channel: BARK.to_string(),
        detail,
    }
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// 把 RFC 3986 的 unreserved 集合之外的字节一律转义成 `%XX`。
///
/// 比 Go 的 `url.PathEscape` 更严格：`$&+,;=:@` 这些子分隔符在路径里合法，但
/// Bark 把路径段当纯文本用，留着它们只会多出被下游某一层重新解释的机会。
/// 中文按 UTF-8 逐字节转义，斜杠变成 `%2F` 而不再是路径分隔符。
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*b as char);
            }
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

/// 路径段转义。
fn encode_path_segment(s: &str) -> String {
    let encoded = percent_encode(s);
    // 「.」「..」是 URL 里的相对路径段，`Url::set_path` 会按规范把它们解释掉：
    // 标题写成「..」时整段会消失，正文被顶到标题的位置 —— 正是斜杠那个坑的
    // 另一种形态。点号本身是 unreserved，只有整段全是点时才需要额外转义。
    if !encoded.is_empty() && encoded.bytes().all(|b| b == b'.') {
        return encoded.replace('.', "%2E");
    }
    encoded
}

/// 把用户地址里自带的查询参数与我们要加的 `url` 参数合并。
///
/// 用户经常在 Bark 地址里带上 `?group=库存`、`?sound=alarm` 之类的设置，必须
/// 原样留住。这里刻意**不**用 `Url::query_pairs_mut()`：它会先按
/// `application/x-www-form-urlencoded` 解码再重新编码，对 `?group=a;sound=b`
/// （分号分隔）或 `?%zz=1`（非法百分号转义）这类地址是有损的 —— 推送照发，
/// 但用户的分组、铃声、图标设置无声失效，完全无从察觉。Go 版栽在同一件事的
/// 另一个形态上：`u.Query()` 把 `ParseQuery` 的错误直接吞掉并返回空集合，
/// 于是同样的地址会让用户写的全部参数被静默丢弃。
///
/// 这里只按 `&` 切段、逐段原样保留，唯一的改动是在要设置链接时去掉已有的
/// `url` 段 —— 不解码就不可能解错。
fn merge_query(existing: Option<&str>, encoded_link: Option<&str>) -> Option<String> {
    let existing = existing.unwrap_or_default();
    let Some(link) = encoded_link else {
        return (!existing.is_empty()).then(|| existing.to_string());
    };

    let mut parts: Vec<&str> = Vec::new();
    for segment in existing.split('&') {
        if segment.is_empty() {
            continue;
        }
        let key = segment.split_once('=').map_or(segment, |(k, _)| k);
        if key == "url" {
            continue;
        }
        parts.push(segment);
    }

    let appended = format!("url={link}");
    if parts.is_empty() {
        return Some(appended);
    }
    Some(format!("{}&{appended}", parts.join("&")))
}

// ---------------------------------------------------------------------------
// 本地提示音
// ---------------------------------------------------------------------------

const SOUND: &str = "提示音";

/// 播放的等待上限在音频自然时长之外额外给的余量。
const SOUND_EXTRA_WAIT: Duration = Duration::from_secs(5);

/// 打开音频设备的等待上限。
const SOUND_DEVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// 轮询播放是否结束的间隔。
const SOUND_POLL_INTERVAL: Duration = Duration::from_millis(20);

/// 提示音最长允许多少秒。超过这个长度只可能是传错了音频。
const SOUND_MAX_SECONDS: usize = 120;

/// 播放内嵌的提示音。
///
/// 音频只解码一次并缓存成 PCM，音频设备也只打开一次；任何一步失败都会被记下来，
/// 之后每次 `notify` 返回同一个错误，监控本身照常运行。
#[derive(Clone)]
pub struct Sound {
    inner: Arc<SoundInner>,
}

struct SoundInner {
    mp3: &'static [u8],
    /// 解码后的 PCM。上游每次响铃都从头解一遍 mp3，用完即弃。
    decoded: OnceLock<Result<SamplesBuffer, String>>,
    /// 音频设备的专属线程。与解码分开缓存：解码失败不该被误报成「没有声卡」，
    /// 而且解码不碰硬件，测试里可以单独走这条路径。
    backend: OnceLock<Result<Backend, String>>,
    /// 单槽标志：同一时刻只允许一次播放。
    playing: AtomicBool,
}

impl Sound {
    /// `mp3` 是内嵌的音频数据；传空切片表示这个构建里没有音频，等同于未配置，
    /// [`Notifier::notify`] 会直接返回 `Ok(())`。
    pub fn new(mp3: &'static [u8]) -> Self {
        Self {
            inner: Arc::new(SoundInner {
                mp3,
                decoded: OnceLock::new(),
                backend: OnceLock::new(),
                playing: AtomicBool::new(false),
            }),
        }
    }

    /// 用内嵌的 [`ALERT_MP3`] 构造。
    pub fn embedded() -> Self {
        Self::new(ALERT_MP3)
    }
}

impl std::fmt::Debug for Sound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 不打印 PCM 缓冲，那是几百万个浮点数。
        f.debug_struct("Sound")
            .field("mp3_bytes", &self.inner.mp3.len())
            .field("playing", &self.inner.playing.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Notifier for Sound {
    fn name(&self) -> &str {
        SOUND
    }

    async fn notify(&self, _n: &Notification) -> Result<(), NotifyError> {
        // 提示音只负责把人叫过来，通知内容用不上。
        if self.inner.mp3.is_empty() {
            return Ok(());
        }

        let inner = Arc::clone(&self.inner);
        // rodio 的设备句柄裹着 `cpal::Stream`，不是 Send，搬不进异步任务；而且
        // 播放期间必须阻塞等待播完，留在异步任务里会把整个 tokio 工作线程钉住，
        // 连带拖慢正在飞的库存查询。所以这里扔进 spawn_blocking，由它再转交给
        // 那条常驻的音频线程 —— 句柄的生命周期整个钉在那条线程上。
        match tokio::task::spawn_blocking(move || inner.play()).await {
            Ok(result) => result,
            // 阻塞任务 panic 了。这是最后一道防线：提示音是在后台发的，绝不能
            // 让它把调用方一起带走。
            Err(join) => Err(NotifyError::Device {
                channel: SOUND.to_string(),
                detail: format!("播放时内部错误已被拦截：{join}"),
            }),
        }
    }
}

impl SoundInner {
    fn play(&self) -> Result<(), NotifyError> {
        // 先解码。解码完全不碰音频硬件，把它排在设备初始化之前，音频数据本身
        // 有问题时报出来的就是「解码失败」，而不是含糊的「没有声卡」。
        let buffer = self
            .decoded
            .get_or_init(|| decode_mp3(self.mp3))
            .as_ref()
            .map_err(|detail| device_err(detail.clone()))?;

        // 已经在响了就直接返回。多个目标同一轮到货时叠加播放只会变成噪音，
        // 而且下面的超时清理是针对整条队列的，两次播放会互相打断。
        let Some(_guard) = PlayGuard::acquire(&self.playing) else {
            return Ok(());
        };

        let backend = self
            .backend
            .get_or_init(Backend::start)
            .as_ref()
            .map_err(|detail| device_err(detail.clone()))?;

        backend.play(buffer.clone()).map_err(device_err)
    }
}

fn device_err(detail: String) -> NotifyError {
    NotifyError::Device {
        channel: SOUND.to_string(),
        detail,
    }
}

/// 单槽播放许可，离开作用域自动归还。
struct PlayGuard<'a>(&'a AtomicBool);

impl<'a> PlayGuard<'a> {
    fn acquire(flag: &'a AtomicBool) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self(flag))
    }
}

impl Drop for PlayGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 解码内嵌音频并缓存成 PCM。
///
/// 上游解码失败直接 panic，而响铃是在后台 goroutine 里调的，panic 会终止整个
/// 进程。这里一切失败都是 `Err`。
fn decode_mp3(mp3: &'static [u8]) -> Result<SamplesBuffer, String> {
    let decoder =
        rodio::Decoder::new_mp3(Cursor::new(mp3)).map_err(|e| format!("解码提示音失败：{e}"))?;

    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    // 解码器的声道数与采样率是按「片段」给的，中途可能变。先统一重采样到首段的
    // 规格再整段收进内存，否则 SamplesBuffer 会拿一套参数去播另一套数据，
    // 听上去就是变调。
    let max_samples = SOUND_MAX_SECONDS * sample_rate.get() as usize * channels.get() as usize;
    let samples: Vec<rodio::Sample> = UniformSourceIterator::new(decoder, channels, sample_rate)
        .take(max_samples + 1)
        .collect();

    if samples.is_empty() {
        return Err("提示音音频为空".to_string());
    }
    if samples.len() > max_samples {
        return Err(format!(
            "提示音音频超过 {SOUND_MAX_SECONDS} 秒，多半是传错了文件"
        ));
    }
    Ok(SamplesBuffer::new(channels, sample_rate, samples))
}

/// 一次播放请求。
struct Job {
    source: SamplesBuffer,
    done: mpsc::Sender<Result<(), String>>,
}

/// 音频后端：一条独占音频设备句柄的常驻线程，外界只能通过 channel 递素材。
struct Backend {
    /// `std::sync::mpsc::Sender` 是 Send 但不是 Sync，而 [`Sound`] 要跨线程共享，
    /// 所以套一层 Mutex。竞争几乎不存在：调用方拿到单槽许可之后才走到这里。
    jobs: Mutex<mpsc::Sender<Job>>,
}

impl Backend {
    fn start() -> Result<Self, String> {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();

        std::thread::Builder::new()
            .name("apw-alert-sound".to_string())
            .spawn(move || audio_thread(&job_rx, &ready_tx))
            .map_err(|e| format!("无法启动提示音线程：{e}"))?;

        // 上游在 main 里无条件调用 speaker.Init 且不看返回值，机器上没有可用音频
        // 设备（无声卡的虚拟机、设备被独占）时初始化其实早已失败，后续播放行为
        // 未定义。这里把失败记下来，用户能在界面上看到「提示音不可用」，监控照跑。
        match ready_rx.recv_timeout(SOUND_DEVICE_TIMEOUT) {
            Ok(Ok(())) => Ok(Self {
                jobs: Mutex::new(job_tx),
            }),
            Ok(Err(detail)) => Err(detail),
            Err(mpsc::RecvTimeoutError::Timeout) => Err("打开音频设备超时".to_string()),
            // 线程若在打开设备时 panic，ready_tx 随栈展开被丢弃，这里立刻拿到
            // Disconnected。这条路径必须也变成一个能展示的错误，而不是干等下去。
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("提示音线程在初始化音频设备时意外退出".to_string())
            }
        }
    }

    fn play(&self, source: SamplesBuffer) -> Result<(), String> {
        // 等待上限在送出去之前算好：source 马上就要被移走了。
        let wait = playback_limit(&source).saturating_add(SOUND_EXTRA_WAIT);

        let (done_tx, done_rx) = mpsc::channel();
        {
            let jobs = self
                .jobs
                .lock()
                .map_err(|_| "提示音线程状态已损坏".to_string())?;
            jobs.send(Job {
                source,
                done: done_tx,
            })
            .map_err(|_| "提示音线程已退出".to_string())?;
        }

        // 音频线程内部已经有自己的播放上限，这里再压一道：线程万一整个卡死或
        // panic 掉，调用方也必须能拿到错误返回，而不是永远停在这一行。
        match done_rx.recv_timeout(wait) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err("等待提示音播放结束超时".to_string()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err("提示音线程在播放过程中意外退出".to_string())
            }
        }
    }
}

fn playback_limit(source: &SamplesBuffer) -> Duration {
    source
        .total_duration()
        .unwrap_or(Duration::from_secs(30))
        .saturating_add(SOUND_EXTRA_WAIT)
}

fn audio_thread(jobs: &mpsc::Receiver<Job>, ready: &mpsc::Sender<Result<(), String>>) {
    let mut handle = match rodio::DeviceSinkBuilder::open_default_sink() {
        Ok(handle) => handle,
        Err(e) => {
            let _ = ready.send(Err(format!("打开音频设备失败：{e}")));
            return;
        }
    };
    // rodio 默认在句柄析构时往 stderr 打一行提醒。这个句柄要活到进程结束，
    // 那行提醒只会出现在退出时，纯属噪音。
    handle.log_on_drop(false);

    if ready.send(Ok(())).is_err() {
        return;
    }

    // 发送端全被丢弃时 recv 返回 Err，线程自然收工。
    while let Ok(job) = jobs.recv() {
        let result = play_once(handle.mixer(), job.source);
        let _ = job.done.send(result);
    }
}

fn play_once(mixer: &rodio::mixer::Mixer, source: SamplesBuffer) -> Result<(), String> {
    let limit = playback_limit(&source);
    let player = rodio::Player::connect_new(mixer);
    player.append(source);

    // 上游直接死等播放结束的信号：设备被拔掉、驱动卡死时那个回调永远不会来，
    // 那条 goroutine 从此不再退出，而且每次都会再堆一条。这里按音频真实时长
    // 加余量设上限，超时就把队列停掉。
    let start = Instant::now();
    while !player.empty() {
        if start.elapsed() >= limit {
            player.stop();
            return Err(format!(
                "播放提示音超时（超过 {} 秒）",
                limit.as_secs_f64().round()
            ));
        }
        std::thread::sleep(SOUND_POLL_INTERVAL);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 组合器
// ---------------------------------------------------------------------------

type BoxedNotify<'a> = Pin<Box<dyn Future<Output = Result<(), NotifyError>> + Send + 'a>>;

/// [`Notifier`] 的装箱版，只在本模块内部用。
///
/// [`Notifier`] 用 RPITIT，不是对象安全的；而 [`Multi`] 必须装下一组类型各异的
/// 渠道。把 future 装箱是唯一的代价，且只在组合这一层付一次。
trait DynNotifier: Send + Sync {
    fn name(&self) -> &str;
    fn notify_boxed<'a>(&'a self, n: &'a Notification) -> BoxedNotify<'a>;
}

impl<T: Notifier> DynNotifier for T {
    fn name(&self) -> &str {
        Notifier::name(self)
    }

    fn notify_boxed<'a>(&'a self, n: &'a Notification) -> BoxedNotify<'a> {
        Box::pin(Notifier::notify(self, n))
    }
}

/// 把多个渠道组合成一个渠道。本身也满足 [`Notifier`]，可以再被嵌套。
pub struct Multi {
    channels: Vec<Box<dyn DynNotifier>>,
    name: String,
}

impl Default for Multi {
    fn default() -> Self {
        Self::new()
    }
}

impl Multi {
    pub fn new() -> Self {
        Self {
            channels: Vec::new(),
            name: "multi()".to_string(),
        }
    }

    /// 追加一个渠道，链式写法。
    pub fn with(mut self, notifier: impl Notifier + 'static) -> Self {
        self.push(notifier);
        self
    }

    /// 追加一个渠道。
    pub fn push(&mut self, notifier: impl Notifier + 'static) {
        self.channels.push(Box::new(notifier));
        let name = {
            let names: Vec<&str> = self.channels.iter().map(|c| c.name()).collect();
            format!("multi({})", names.join(", "))
        };
        self.name = name;
    }

    pub fn len(&self) -> usize {
        self.channels.len()
    }

    pub fn is_empty(&self) -> bool {
        self.channels.is_empty()
    }
}

impl std::fmt::Debug for Multi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

impl Notifier for Multi {
    fn name(&self) -> &str {
        &self.name
    }

    /// 并发调用全部渠道，等所有渠道结束后汇总错误。
    ///
    /// 必须并发而不是串行：渠道之间没有依赖，Bark 服务器无响应要等到超时，
    /// 串行的话本地提示音就得跟着干等十几秒 —— 而提示音恰恰是最需要立刻响的
    /// 那个。单个渠道失败也绝不能影响其他渠道：到货提醒多一路是一路。
    async fn notify(&self, n: &Notification) -> Result<(), NotifyError> {
        if self.channels.is_empty() {
            return Ok(());
        }

        // 手写并发轮询，而不是 tokio::spawn 或第三方的 join_all：这些 future
        // 借着 `&self` 与 `&n`，不是 'static，塞不进 spawn；本 crate 也不打算
        // 为这十来行去引 futures。每个槽位轮询到 Ready 就置空，不会被重复轮询。
        let mut pending: Vec<Option<BoxedNotify<'_>>> = self
            .channels
            .iter()
            .map(|c| Some(c.notify_boxed(n)))
            .collect();
        let mut failures: Vec<Option<NotifyError>> = (0..pending.len()).map(|_| None).collect();

        std::future::poll_fn(|cx| {
            let mut all_done = true;
            for (slot, failure) in pending.iter_mut().zip(failures.iter_mut()) {
                let Some(fut) = slot.as_mut() else { continue };
                match fut.as_mut().poll(cx) {
                    Poll::Ready(result) => {
                        if let Err(e) = result {
                            *failure = Some(e);
                        }
                        *slot = None;
                    }
                    Poll::Pending => all_done = false,
                }
            }
            if all_done {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;

        let mut failures: Vec<NotifyError> = failures.into_iter().flatten().collect();
        if failures.is_empty() {
            return Ok(());
        }
        // 只有一路失败时不必套一层汇总，错误本身已经说清是谁。
        if failures.len() == 1
            && let Some(only) = failures.pop()
        {
            return Err(only);
        }
        Err(NotifyError::Multiple(failures))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(title: &str, body: &str) -> Notification {
        Notification::new(title, body)
    }

    #[test]
    fn 路径段里的斜杠中文空格都必须转义() {
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment("有 货"), "%E6%9C%89%20%E8%B4%A7");
        // unreserved 集合原样保留，免得推送出来的标题全是百分号。
        assert_eq!(
            encode_path_segment("iPhone-17_Pro.5~x"),
            "iPhone-17_Pro.5~x"
        );
        // 查询参数里的 & 与 = 也必须转义，否则会把后面的参数整段截断。
        assert_eq!(percent_encode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn 全是点号的路径段不会被当成相对路径吃掉() {
        assert_eq!(encode_path_segment(".."), "%2E%2E");
        assert_eq!(encode_path_segment("."), "%2E");
        assert_eq!(encode_path_segment("1.0"), "1.0");
    }

    #[test]
    fn 自带的查询参数原样保留() {
        assert_eq!(
            merge_query(Some("group=stock&sound=alarm"), Some("x%3Ay")),
            Some("group=stock&sound=alarm&url=x%3Ay".to_string())
        );
        // 没有链接要加时，一个字节都不该动。
        assert_eq!(
            merge_query(Some("group=stock"), None),
            Some("group=stock".to_string())
        );
        assert_eq!(merge_query(None, Some("x")), Some("url=x".to_string()));
        assert_eq!(merge_query(None, None), None);
    }

    #[test]
    fn 解析不了的查询参数也不会被静默丢弃() {
        // 分号分隔与非法百分号转义都是 Go 的 ParseQuery 会直接报错的形态，
        // 而它的错误被吞掉后用户的全部设置就没了。这里必须原样带过去。
        let merged =
            merge_query(Some("group=a;sound=b&%zz=1"), Some("link")).expect("有内容就必须有查询串");
        assert!(merged.contains("group=a;sound=b"), "实际为 {merged}");
        assert!(merged.contains("%zz=1"), "实际为 {merged}");
        assert!(merged.ends_with("&url=link"));
    }

    #[test]
    fn 已有的url参数会被替换而不是叠加() {
        let merged = merge_query(Some("url=old&group=stock"), Some("new")).expect("必须有查询串");
        assert_eq!(merged, "group=stock&url=new");
        assert_eq!(merged.matches("url=").count(), 1);
    }

    #[test]
    fn 组装出来的地址里标题斜杠不是路径分隔符() {
        let url = build_bark_url(
            "https://api.day.app/devkey",
            &n("到货/提醒", "上海-环球港 有货").with_url("https://x.cn/shop/bag?a=1&b=2"),
        )
        .expect("正常地址必须能组装");

        let path = url.path();
        // 前缀一段 + 标题一段 + 正文一段，多一条斜杠就说明标题被拆开了。
        assert_eq!(path.matches('/').count(), 3, "实际路径 {path}");
        assert!(path.contains("%2F"), "标题里的斜杠必须转义：{path}");
        assert!(path.starts_with("/devkey/"));

        let query = url.query().unwrap_or_default();
        // 链接整段转义，其中的 & 不能把参数截断。
        assert!(query.contains("url=https%3A%2F%2F"), "实际查询串 {query}");
        assert!(!query.contains("b=2"), "链接里的 & 没转义：{query}");
    }

    #[test]
    fn 空标题不会拼出双斜杠() {
        let url = build_bark_url("https://api.day.app/devkey", &n("  ", "只有正文"))
            .expect("只有正文也应当能推送");
        assert!(!url.path().contains("//"), "实际路径 {}", url.path());
        assert_eq!(url.path().matches('/').count(), 2);
    }

    #[test]
    fn 地址不合法时报配置错误() {
        for bad in ["", "不是地址", "ftp://a.b/key", "https://api.day.app"] {
            let err =
                build_bark_url(bad, &n("标题", "正文")).expect_err(&format!("{bad:?} 不该被接受"));
            assert_eq!(err.channel(), Some("Bark"), "{bad:?}");
            assert!(matches!(err, NotifyError::Config { .. }), "{bad:?}");
        }
    }

    #[test]
    fn 标题正文都为空时报错而不是发一条空推送() {
        let err = build_bark_url("https://api.day.app/devkey", &n(" ", ""))
            .expect_err("空通知不该发出去");
        assert!(matches!(err, NotifyError::Config { .. }));
    }

    #[test]
    fn 汇总错误里能数出每一路渠道() {
        let inner = NotifyError::Multiple(vec![
            NotifyError::Transport {
                channel: "Bark".to_string(),
                detail: "超时".to_string(),
            },
            NotifyError::Device {
                channel: "提示音".to_string(),
                detail: "没有声卡".to_string(),
            },
        ]);
        assert_eq!(inner.channel(), None);
        assert_eq!(inner.failed_channels(), vec!["Bark", "提示音"]);
        // 嵌套的 Multi 也要能摊平。
        let outer = NotifyError::Multiple(vec![
            inner,
            NotifyError::Config {
                channel: "邮件".to_string(),
                detail: "地址为空".to_string(),
            },
        ]);
        assert_eq!(outer.failed_channels(), vec!["Bark", "提示音", "邮件"]);
        let text = outer.to_string();
        assert!(text.contains("Bark") && text.contains("提示音") && text.contains("邮件"));
    }

    #[test]
    fn 响应体摘要不会把整页html塞进错误里() {
        let long = "x".repeat(500);
        let summary = summarize(long.as_bytes());
        assert!(summary.ends_with('…'));
        assert!(summary.chars().count() <= 121);
        assert_eq!(summarize(b""), "(空响应)");
        assert_eq!(summarize(b"  a \n b "), "a b");
    }

    #[test]
    fn 内嵌的提示音本身是一段能解码的音频() {
        // 解码不碰任何音频硬件，所以这条能在没有声卡的机器上跑。它守的是
        // 「打包时把资源换错了」这类事故：编译能过，直到真到货那天才发现没声。
        let buffer = decode_mp3(ALERT_MP3).expect("内嵌提示音必须能解码");
        let duration = buffer.total_duration().expect("PCM 缓冲一定知道自己多长");
        assert!(
            duration >= Duration::from_millis(300) && duration.as_secs() <= 30,
            "提示音时长不合理：{duration:?}"
        );
    }

    #[test]
    fn 业务错误码只在明确失败时才算错() {
        assert_eq!(bark_business_error(br#"{"code":200}"#), None);
        assert_eq!(bark_business_error(br#"{"message":"ok"}"#), None);
        assert_eq!(bark_business_error(b"not json"), None);
        assert_eq!(
            bark_business_error(r#"{"code":400,"message":"key 无效"}"#.as_bytes()),
            Some((400, "key 无效".to_string()))
        );
    }
}
