//! 本地模拟会话初始化，覆盖多门店并发；不访问 Apple 或发送提醒。
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use apw_core::apple::{AppleClient, ClientConfig};
use apw_core::model::{Availability, Region};

struct SessionServer {
    addr: SocketAddr,
    warms: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl SessionServer {
    fn start(fail_first_warm: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let warms = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let count = warms.clone();
        let stopped = stop.clone();
        let join = std::thread::spawn(move || {
            let mut handlers = Vec::new();
            for stream in listener.incoming() {
                let stream = stream.unwrap();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                let count = count.clone();
                handlers.push(std::thread::spawn(move || {
                    serve(stream, count, fail_first_warm)
                }));
            }
            for handler in handlers {
                handler.join().unwrap();
            }
        });
        Self {
            addr,
            warms,
            stop,
            join: Some(join),
        }
    }

    fn region(&self) -> Region {
        Region {
            title: "test",
            locale: "test",
            base_url: Box::leak(format!("http://{}", self.addr).into_boxed_str()),
            families: &[],
        }
    }
}

impl Drop for SessionServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        self.join.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, warms: Arc<AtomicUsize>, fail_first: bool) {
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request = String::new();
    reader.read_line(&mut request).unwrap();
    let mut cookie = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
            break;
        }
        if line.to_ascii_lowercase().starts_with("cookie:") {
            cookie = line;
        }
    }
    let (status, headers, body) = if request.starts_with("GET /shop/bag ") {
        let generation = warms.fetch_add(1, Ordering::SeqCst) + 1;
        // 让第二个并发调用有时间进入，确定性覆盖检查与初始化之间的竞争窗口。
        std::thread::sleep(Duration::from_millis(100));
        if fail_first && generation == 1 {
            (
                "503 Service Unavailable",
                String::new(),
                "unavailable".into(),
            )
        } else {
            (
                "200 OK",
                format!("Set-Cookie: session={generation}; Path=/\r\n"),
                "<html>bag</html>".into(),
            )
        }
    } else if cookie.contains(&format!("session={}", warms.load(Ordering::SeqCst))) {
        let store = if request.contains("store=R390") {
            "R390"
        } else {
            "R683"
        };
        (
            "200 OK",
            String::new(),
            serde_json::json!({
                "head": {"status": "200"},
                "body": {"stores": [{"storeNumber": store,
                    "partsAvailability": {"TEST/A": {"pickupDisplay": "unavailable"}}}]}
            })
            .to_string(),
        )
    } else {
        ("541 Blocked", String::new(), "invalid session".into())
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
}

fn client() -> AppleClient {
    AppleClient::new(ClientConfig {
        min_interval: Duration::ZERO,
        max_retries: 0,
        timeout: Duration::from_secs(3),
        ..Default::default()
    })
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_stores_share_one_warmup_and_reuse_session() {
    let server = SessionServer::start(false);
    let region = server.region();
    let client = client();
    let parts = vec!["TEST/A".into()];
    let (a, b) = tokio::join!(
        client.pickup_message(&region, "R683", &parts),
        client.pickup_message(&region, "R390", &parts),
    );
    assert_eq!(
        server.warms.load(Ordering::SeqCst),
        1,
        "并发初始化不应覆盖会话"
    );
    for result in [a, b] {
        assert_eq!(
            result.unwrap().parts["TEST/A"].availability,
            Availability::OutOfStock
        );
    }
    client
        .pickup_message(&region, "R683", &parts)
        .await
        .unwrap();
    assert_eq!(
        server.warms.load(Ordering::SeqCst),
        1,
        "下一轮应复用成功会话"
    );
}

#[tokio::test]
async fn failed_warmup_is_not_cached_as_success() {
    let server = SessionServer::start(true);
    let region = server.region();
    let client = client();
    let parts = vec!["TEST/A".into()];
    assert!(
        client
            .pickup_message(&region, "R683", &parts)
            .await
            .is_err()
    );
    let result = client
        .pickup_message(&region, "R683", &parts)
        .await
        .unwrap();
    assert_eq!(
        result.parts["TEST/A"].availability,
        Availability::OutOfStock
    );
    assert_eq!(server.warms.load(Ordering::SeqCst), 2);
}
