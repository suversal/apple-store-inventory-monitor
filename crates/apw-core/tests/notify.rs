//! 通知渠道的测试。
//!
//! 不碰真实网络，也不碰音频设备：Bark 打到本进程里起的一个极简 HTTP 服务端，
//! 断言它收到的**原始**请求行；提示音只测不需要声卡的那几条路径，真的要响一声
//! 的用例挂了 `#[ignore]`，手动跑。

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apw_core::notify::{Bark, Multi, Notification, Notifier, NotifyError, Sound};

// ---------------------------------------------------------------------------
// 极简 HTTP 测试服务端
// ---------------------------------------------------------------------------

/// 一个只会应答固定内容、并把收到的请求行原样记下来的 HTTP 服务端。
///
/// 必须看**原始**请求行而不是解析后的结果：这一组测试要证明的正是「转义对不对」，
/// 一旦在断言之前先解码一次，斜杠有没有变成 %2F 就看不出来了。
struct TestServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<String>>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl TestServer {
    fn start(status_line: &'static str, body: &'static str) -> Self {
        Self::spawn(Some((status_line, body)))
    }

    /// 只接受连接、收下请求，然后一直挂着不应答。
    ///
    /// 用来复现 Go 版那个坑：`http.Get` 走的 `http.DefaultClient` 没有任何超时，
    /// Bark 服务器只连上不回包时，那次调用会一直挂着不释放。
    fn stalling() -> Self {
        Self::spawn(None)
    }

    fn spawn(respond: Option<(&'static str, &'static str)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let requests = Arc::new(Mutex::new(Vec::new()));

        let join = {
            let requests = Arc::clone(&requests);
            std::thread::spawn(move || {
                // 不应答时也要把连接握住：一旦提前析构，客户端拿到的就是连接被
                // 重置，测出来的是另一回事。
                let mut held = Vec::new();
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { break };
                    let Ok(peek) = stream.try_clone() else {
                        continue;
                    };
                    let mut reader = BufReader::new(peek);

                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    // 请求行为空说明这是关停用的空连接。
                    if request_line.trim().is_empty() {
                        break;
                    }
                    // 把请求头读完，避免客户端还没写完我们就把连接关了。
                    loop {
                        let mut header = String::new();
                        match reader.read_line(&mut header) {
                            Ok(0) => break,
                            Ok(_) if header.trim().is_empty() => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    requests
                        .lock()
                        .expect("记录请求")
                        .push(request_line.trim_end().to_string());

                    let Some((status_line, body)) = respond else {
                        held.push(stream);
                        continue;
                    };
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
                drop(held);
            })
        };

        Self {
            addr,
            requests,
            join: Some(join),
        }
    }

    fn ok() -> Self {
        Self::start("200 OK", r#"{"code":200,"message":"success"}"#)
    }

    /// 带设备 key 的推送地址。
    fn base_url(&self) -> String {
        format!("http://{}/devkey", self.addr)
    }

    fn count(&self) -> usize {
        self.requests.lock().expect("读请求").len()
    }

    /// 第一条请求的原始请求目标，即 `GET` 与 `HTTP/1.1` 之间那一段。
    fn target(&self) -> String {
        let requests = self.requests.lock().expect("读请求");
        let line = requests.first().cloned().unwrap_or_default();
        line.split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        // 连一下把 accept 叫醒，线程读到空请求行就收工。
        let _ = TcpStream::connect(self.addr);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 极简百分号解码，用来把服务端收到的路径还原回去做断言。
///
/// 刻意只写解码、不复用实现里的编码：测试要是跟实现共用同一段代码，
/// 编码错了两边会一起错，测了等于没测。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("构造 HTTP 客户端")
}

fn 到货通知() -> Notification {
    Notification::new("到货/提醒", "上海-环球港 iPhone 17 有货")
        .with_url("https://www.apple.com.cn/shop/bag?step=1&next=2")
}

// ---------------------------------------------------------------------------
// Bark
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bark地址为空时直接成功且一个请求都不发() {
    let server = TestServer::ok();
    for blank in ["", "   ", "\t\n"] {
        let bark = Bark::new(blank.to_string(), client());
        assert!(!bark.is_configured());
        bark.notify(&到货通知())
            .await
            .expect("未配置不是错误，必须返回 Ok");
    }
    assert_eq!(server.count(), 0, "未配置时不该发出任何请求");
}

#[tokio::test]
async fn bark标题里的斜杠不能变成路径分隔符() {
    let server = TestServer::ok();
    Bark::new(server.base_url(), client())
        .notify(&到货通知())
        .await
        .expect("正常推送应当成功");

    let target = server.target();
    let path = target.split('?').next().unwrap_or_default();

    // /devkey/<标题>/<正文>：斜杠正好三条。多一条就说明标题被拆成了两段，
    // 正文会被顶到 Bark 的标题位置上 —— 这正是 Go 版直接 Sprintf 拼接的后果。
    assert_eq!(path.matches('/').count(), 3, "实际路径 {path}");

    let segments: Vec<&str> = path.split('/').collect();
    assert_eq!(segments.len(), 4, "实际路径 {path}");
    assert_eq!(segments[1], "devkey");
    assert!(
        segments[2].contains("%2F"),
        "标题里的斜杠必须是 %2F：{path}"
    );
    assert_eq!(percent_decode(segments[2]), "到货/提醒");
    // 空格必须转义，中文必须按 UTF-8 转义，原文一个字都不该出现在请求行里。
    assert!(segments[3].contains("%20"), "空格必须转义：{path}");
    assert!(!path.contains("到货"), "中文必须转义：{path}");
    assert_eq!(percent_decode(segments[3]), "上海-环球港 iPhone 17 有货");
}

#[tokio::test]
async fn bark通知链接必须整段转义() {
    let server = TestServer::ok();
    Bark::new(server.base_url(), client())
        .notify(&到货通知())
        .await
        .expect("正常推送应当成功");

    let target = server.target();
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();

    // Go 版把购物袋地址原样塞进 url= 后面，链接里的 & 会把参数整段截断，
    // 于是 Bark 收到的是半截地址加一个莫名其妙的 next 参数。
    assert!(!query.contains("&next=2"), "链接里的 & 没转义：{query}");
    assert!(
        query.contains("url=https%3A%2F%2F"),
        "链接必须整段转义：{query}"
    );
    let link = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("url="))
        .expect("必须带上 url 参数");
    assert_eq!(
        percent_decode(link),
        "https://www.apple.com.cn/shop/bag?step=1&next=2"
    );
}

#[tokio::test]
async fn bark保留用户自己写在地址里的分组与铃声() {
    let server = TestServer::ok();
    let base = format!("{}?group=stock&sound=alarm", server.base_url());
    Bark::new(base, client())
        .notify(&到货通知())
        .await
        .expect("带参数的地址也应当能推送");

    let query = server
        .target()
        .split_once('?')
        .map_or(String::new(), |(_, q)| q.to_string());
    assert!(query.contains("group=stock"), "分组丢了：{query}");
    assert!(query.contains("sound=alarm"), "铃声丢了：{query}");
    assert!(query.contains("url="), "链接没带上：{query}");
}

#[tokio::test]
async fn bark解析不了的查询参数也不会被静默丢弃() {
    let server = TestServer::ok();
    // 分号分隔与非法百分号转义，都是 Go 的 url.ParseQuery 会直接报错的形态。
    // Go 版把那个错误吞掉后返回空集合，用户写的全部设置就此无声失效：
    // 推送照发，分组、铃声、图标全没了，而用户完全无从察觉。
    let base = format!("{}?group=a;sound=b&%zz=1", server.base_url());
    Bark::new(base, client())
        .notify(&到货通知())
        .await
        .expect("参数再古怪也不该拦住推送");

    let query = server
        .target()
        .split_once('?')
        .map_or(String::new(), |(_, q)| q.to_string());
    assert!(
        query.contains("group=a;sound=b"),
        "分号形态的参数丢了：{query}"
    );
    assert!(query.contains("%zz=1"), "非法转义的参数丢了：{query}");
    assert!(query.contains("url="), "链接没带上：{query}");
}

#[tokio::test]
async fn bark四百和五百都必须报错() {
    for (status, code) in [("404 Not Found", 404u16), ("500 Internal Error", 500)] {
        let server = TestServer::start(status, "boom");
        let err = Bark::new(server.base_url(), client())
            .notify(&到货通知())
            .await
            .expect_err("非 2xx 必须报错");

        // 上游连状态码都不看：设备 key 写错时用户一直以为推送生效了。
        assert_eq!(err.channel(), Some("Bark"));
        assert!(matches!(err, NotifyError::Rejected { .. }), "{err}");
        let text = err.to_string();
        assert!(
            text.contains(&code.to_string()),
            "错误里要能看到状态码：{text}"
        );
        assert_eq!(server.count(), 1);
    }
}

#[tokio::test]
async fn bark错误不会泄露设备密钥() {
    let server = TestServer::start("500 Internal Error", "devkey 暂时不可用");
    let err = Bark::new(server.base_url(), client())
        .notify(&到货通知())
        .await
        .expect_err("服务端错误必须报错");
    let text = err.to_string();
    assert!(!text.contains("devkey"), "错误泄露了设备密钥：{text}");
    assert!(
        text.contains("***"),
        "脱敏后仍应保留可识别的错误摘要：{text}"
    );
}

#[tokio::test]
async fn bark网络错误不会把完整请求地址写进日志文本() {
    let secret = "super-secret-device-key";
    let err = Bark::new(format!("http://127.0.0.1:9/{secret}"), client())
        .with_timeout(Duration::from_millis(300))
        .notify(&到货通知())
        .await
        .expect_err("不可达端口必须报错");
    let text = err.to_string();
    assert!(matches!(err, NotifyError::Transport { .. }));
    assert!(!text.contains(secret), "网络错误泄露了设备密钥：{text}");
    assert!(
        !text.contains("127.0.0.1"),
        "网络错误泄露了完整地址：{text}"
    );
}

#[tokio::test]
async fn bark两百响应体里的业务错误码也要报错() {
    let server = TestServer::start("200 OK", r#"{"code":400,"message":"device key 无效"}"#);
    let err = Bark::new(server.base_url(), client())
        .notify(&到货通知())
        .await
        .expect_err("响应体里报错也是失败");
    assert_eq!(err.channel(), Some("Bark"));
    assert!(err.to_string().contains("device key 无效"), "{err}");
}

#[tokio::test]
async fn bark服务器不应答时会超时而不是永远挂着() {
    let server = TestServer::stalling();
    let started = std::time::Instant::now();

    let err = Bark::new(server.base_url(), reqwest::Client::new())
        .with_timeout(Duration::from_millis(300))
        .notify(&到货通知())
        .await
        .expect_err("超时必须报错");

    let elapsed = started.elapsed();
    // 传进来的客户端刻意没配超时：上限必须由渠道自己兜住，而不是指望调用方。
    assert!(elapsed < Duration::from_secs(3), "实际耗时 {elapsed:?}");
    assert_eq!(err.channel(), Some("Bark"));
    assert!(matches!(err, NotifyError::Transport { .. }), "{err}");
    assert_eq!(server.count(), 1, "请求确实发出去了，只是没人应答");
}

#[tokio::test]
async fn bark地址不合法时报错且不发请求() {
    let server = TestServer::ok();
    for bad in ["不是地址", "ftp://a.b/key", "https://api.day.app"] {
        let err = Bark::new(bad.to_string(), client())
            .notify(&到货通知())
            .await
            .expect_err(&format!("{bad:?} 不该被接受"));
        assert!(matches!(err, NotifyError::Config { .. }), "{bad:?}：{err}");
    }
    assert_eq!(server.count(), 0);
}

// ---------------------------------------------------------------------------
// Multi
// ---------------------------------------------------------------------------

/// 假渠道：可编程成功或失败，并记录调用次数与并发峰值。
#[derive(Clone)]
struct Fake {
    name: String,
    fail: bool,
    delay: Duration,
    calls: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Fake {
    fn new(name: &str, fail: bool) -> Self {
        Self {
            name: name.to_string(),
            fail,
            delay: Duration::from_millis(30),
            calls: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }

    /// 让多个假渠道共用同一组并发计数，才能量出整体的并发峰值。
    fn sharing(mut self, other: &Fake) -> Self {
        self.in_flight = Arc::clone(&other.in_flight);
        self.peak = Arc::clone(&other.peak);
        self
    }
}

impl Notifier for Fake {
    fn name(&self) -> &str {
        &self.name
    }

    async fn notify(&self, _n: &Notification) -> Result<(), NotifyError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);

        if self.fail {
            Err(NotifyError::Transport {
                channel: self.name.clone(),
                detail: "假的失败".to_string(),
            })
        } else {
            Ok(())
        }
    }
}

#[tokio::test]
async fn multi全部成功时返回成功() {
    let a = Fake::new("Bark", false);
    let b = Fake::new("提示音", false);
    let multi = Multi::new().with(a.clone()).with(b.clone());

    assert_eq!(multi.len(), 2);
    assert_eq!(multi.name(), "multi(Bark, 提示音)");
    multi.notify(&到货通知()).await.expect("全成功应当返回 Ok");
    assert_eq!((a.calls(), b.calls()), (1, 1));
}

#[tokio::test]
async fn multi空组合返回成功() {
    let multi = Multi::new();
    assert!(multi.is_empty());
    multi.notify(&到货通知()).await.expect("没有渠道不是错误");
}

#[tokio::test]
async fn multi单个渠道失败不影响其他渠道() {
    let bad = Fake::new("Bark", true);
    let good = Fake::new("提示音", false);
    let multi = Multi::new().with(bad.clone()).with(good.clone());

    let err = multi
        .notify(&到货通知())
        .await
        .expect_err("有渠道失败必须报错");

    // 关键：坏渠道不能把好渠道带走。到货提醒多一路是一路。
    assert_eq!(good.calls(), 1, "另一路渠道必须照常执行");
    assert_eq!(err.failed_channels(), vec!["Bark"]);
    assert_eq!(err.channel(), Some("Bark"));
}

#[tokio::test]
async fn multi错误里能看出是哪几路渠道失败() {
    let bark = Fake::new("Bark", true);
    let sound = Fake::new("提示音", false);
    let mail = Fake::new("邮件", true);
    let mut multi = Multi::new();
    multi.push(bark.clone());
    multi.push(sound.clone());
    multi.push(mail.clone());

    let err = multi
        .notify(&到货通知())
        .await
        .expect_err("两路失败必须报错");

    assert_eq!(err.failed_channels(), vec!["Bark", "邮件"]);
    let text = err.to_string();
    assert!(text.contains("Bark") && text.contains("邮件"), "{text}");
    assert!(
        !text.contains("提示音"),
        "成功的渠道不该出现在错误里：{text}"
    );
    assert_eq!(sound.calls(), 1);
}

#[tokio::test]
async fn multi嵌套之后仍然能摊平出所有失败渠道() {
    let inner = Multi::new()
        .with(Fake::new("Bark", true))
        .with(Fake::new("提示音", true));
    let outer = Multi::new().with(inner).with(Fake::new("邮件", true));

    let err = outer.notify(&到货通知()).await.expect_err("全失败必须报错");
    assert_eq!(err.failed_channels(), vec!["Bark", "提示音", "邮件"]);
}

#[tokio::test]
async fn multi是并发调用各渠道的() {
    let a = Fake::new("Bark", false);
    let b = Fake::new("提示音", false).sharing(&a);
    let c = Fake::new("邮件", false).sharing(&a);
    let multi = Multi::new().with(a.clone()).with(b.clone()).with(c.clone());

    let started = std::time::Instant::now();
    multi.notify(&到货通知()).await.expect("全成功");
    let elapsed = started.elapsed();

    // 串行的话三个 30ms 要跑满 90ms。并发峰值 3 是更直接的证据：
    // Bark 服务器无响应要等到超时，串行会让最该立刻响的提示音干等十几秒。
    assert_eq!(a.peak(), 3, "三路渠道必须同时在飞");
    assert!(elapsed < Duration::from_millis(90), "实际耗时 {elapsed:?}");
}

// ---------------------------------------------------------------------------
// 提示音
// ---------------------------------------------------------------------------

#[tokio::test]
async fn 提示音数据非法时返回错误而不是崩() {
    static 垃圾: &[u8] = b"this is definitely not an mp3 file, just some bytes";
    let sound = Sound::new(垃圾);
    assert_eq!(sound.name(), "提示音");

    // 解码排在打开音频设备之前，所以这条用例不需要任何声卡。
    // 上游在这里直接 panic，而响铃是在后台 goroutine 里调的，会终止整个进程。
    let err = sound
        .notify(&到货通知())
        .await
        .expect_err("非法音频必须返回错误");
    assert_eq!(err.channel(), Some("提示音"));
    assert!(err.to_string().contains("解码"), "{err}");

    // 失败要能被记住并重复返回，而不是第二次换个花样崩掉。
    let again = sound
        .notify(&到货通知())
        .await
        .expect_err("再来一次还是错误");
    assert_eq!(again.to_string(), err.to_string());
}

#[tokio::test]
async fn 提示音数据为空视为未配置() {
    Sound::new(b"")
        .notify(&到货通知())
        .await
        .expect("没有内嵌音频等同于未配置，不是错误");
}

#[tokio::test]
#[ignore = "需要可用的音频设备，手动跑：cargo test -p apw-core --test notify -- --ignored"]
async fn 内嵌提示音能真的响一声且并发调用不叠加() {
    let sound = Sound::embedded();
    let a = sound.clone();
    let b = sound.clone();
    let n = 到货通知();

    let started = std::time::Instant::now();
    let (ra, rb) = tokio::join!(a.notify(&n), b.notify(&n));
    let elapsed = started.elapsed();

    ra.expect("第一次播放应当成功");
    // 第二次撞上正在播放，直接返回 Ok 而不是排队再响一遍。
    rb.expect("并发调用不该报错");
    assert!(
        elapsed < Duration::from_secs(30),
        "两次并发调用不该叠加播放：{elapsed:?}"
    );
}
