//! 商品刷新回归测试：从入口发现新代际，部分失败时保留新旧商品。
use apw_core::apple_catalog::parse_family_links;
use apw_core::catalog::{Catalog, CatalogError};
use apw_core::model::{Category, Family, REGIONS, Region, region_by_locale};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

#[test]
fn discovery_accepts_new_generations_and_rejects_wrong_regions_and_sku_links() {
    let region = region_by_locale("zh_TW").unwrap();
    let html = br#"
      <a href='/tw/shop/buy-iphone/iphone-99-pro'>New</a>
      <a href='https://www.apple.com/tw/shop/buy-iphone/iphone-99-pro?x=1&amp;y=2'>Duplicate</a>
      <a HREF = '/tw/shop/buy-iphone/iphone-duo/'>Duo</a>
      <a href='/tw/shop/buy-iphone/iphone-99-pro/sku/a'>SKU</a>
      <a href='/tw/shop/buy-ipad/ipad-pro'>Other category</a>
      <a href='/jp/shop/buy-iphone/iphone-98'>Wrong region</a>
      <a href='https://example.com/tw/shop/buy-iphone/iphone-98'>Wrong host</a>
      <a href='/tw/shop/buy-iphone'>Index</a>
      <a href='/tw/shop/buy-iphone/carrier-offers'>Carrier offers, not a product family</a>
      <a href='/tw/shop/buy-iphone/%2e%2e'>Traversal</a>
      <script>const x = '/tw/shop/buy-iphone/iphone-stale';</script>
      <!-- <a href='/tw/shop/buy-iphone/iphone-stale'>Old</a> -->
    "#;
    assert_eq!(
        parse_family_links(html, region, Category::Iphone).unwrap(),
        vec!["iphone-99-pro", "iphone-duo"]
    );
    assert!(parse_family_links(b"<html>blocked</html>", region, Category::Iphone).is_err());
}

#[test]
fn real_new_iphone_snapshots_have_distinct_models_capacities_and_colors() {
    let catalog = Catalog::new();
    for region in REGIONS {
        let products = catalog.products(region.locale).unwrap();
        let new: Vec<_> = products
            .iter()
            .filter(|p| p.family.starts_with("iphone18pro"))
            .collect();
        assert_eq!(new.len(), 32, "{}", region.locale);
        for family in ["iphone18pro", "iphone18promax"] {
            let model: Vec<_> = new.iter().filter(|p| p.family == family).collect();
            assert_eq!(model.len(), 16, "{} {family}", region.locale);
            assert!(
                model
                    .iter()
                    .all(|p| !p.capacity.is_empty() && !p.color.is_empty())
            );
            let names: std::collections::HashSet<_> = model.iter().map(|p| &p.title).collect();
            assert_eq!(names.len(), 16);
        }
    }
    let cn = catalog.products("zh_CN").unwrap();
    assert_eq!(
        cn.iter()
            .filter(|p| p.family == "iphoneduo" && p.title.starts_with("iPhone Duo "))
            .count(),
        8
    );
}

struct Server {
    addr: std::net::SocketAddr,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new(index: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let requests = paths.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                if stopped.load(Ordering::SeqCst) {
                    break;
                }
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let path = request.split_whitespace().nth(1).unwrap().to_string();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                requests.lock().unwrap().push(path.clone());
                let body = if path == "/shop/buy-iphone" {
                    index
                } else if path.ends_with("iphone-99-pro") {
                    r#"<script>productSelectionData: {"products":[{"partNumber":"FUTURE/A","familyType":"iphone99pro","dimensionCapacity":"256gb","dimensionColor":"black"}]}</script>"#
                } else {
                    "<html>No product data</html>"
                };
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        Self {
            addr,
            paths,
            stop,
            thread: Some(thread),
        }
    }
    fn region(&self) -> &'static Region {
        Box::leak(Box::new(Region {
            title: "test",
            locale: "zh_CN",
            base_url: Box::leak(format!("http://{}", self.addr).into_boxed_str()),
            families: &[Family {
                category: Category::Iphone,
                slug: "iphone-old",
            }],
        }))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[tokio::test]
async fn refresh_discovers_unregistered_model_and_removes_withdrawn_catalog_pages() {
    let server = Server::new("<a href='/shop/buy-iphone/iphone-99-pro'>New</a>");
    let catalog = Catalog::new();
    let old = catalog.products("zh_CN").unwrap()[0].part_number.clone();
    assert_eq!(
        catalog
            .refresh_products(
                server.region(),
                Some(Category::Iphone),
                &reqwest::Client::new()
            )
            .await
            .unwrap(),
        1
    );
    assert!(catalog.product_by_part("zh_CN", "FUTURE/A").is_some());
    assert!(catalog.product_by_part("zh_CN", &old).is_none());
    assert!(
        catalog
            .products("zh_CN")
            .unwrap()
            .iter()
            .any(|p| p.category == Category::Mac)
    );
    assert_eq!(
        *server.paths.lock().unwrap(),
        vec!["/shop/buy-iphone", "/shop/buy-iphone/iphone-99-pro"]
    );
}

#[tokio::test]
async fn partial_refresh_reports_failure_but_installs_successful_new_page() {
    let server = Server::new(
        "<a href='/shop/buy-iphone/iphone-99-pro'>New</a><a href='/shop/buy-iphone/iphone-broken'>Broken</a>",
    );
    let catalog = Catalog::new();
    let before = catalog.products("zh_CN").unwrap().len();
    match catalog
        .refresh_products(
            server.region(),
            Some(Category::Iphone),
            &reqwest::Client::new(),
        )
        .await
    {
        Err(CatalogError::RefreshFailed {
            fetched, failures, ..
        }) => {
            assert_eq!(fetched, 1);
            assert_eq!(failures.len(), 1);
            assert!(failures[0].contains("iphone-broken"));
        }
        other => panic!("expected partial failure, got {other:?}"),
    }
    assert!(catalog.product_by_part("zh_CN", "FUTURE/A").is_some());
    assert_eq!(catalog.products("zh_CN").unwrap().len(), before + 1);
}

#[tokio::test]
async fn failed_discovery_is_reported_and_preserves_existing_catalog() {
    let server = Server::new("<html>Temporary placeholder</html>");
    let catalog = Catalog::new();
    let before = catalog.products("zh_CN").unwrap();
    match catalog
        .refresh_products(
            server.region(),
            Some(Category::Iphone),
            &reqwest::Client::new(),
        )
        .await
    {
        Err(CatalogError::RefreshFailed {
            fetched, failures, ..
        }) => {
            assert_eq!(fetched, 0);
            assert!(failures[0].contains("机型发现失败"));
        }
        other => panic!("expected discovery failure, got {other:?}"),
    }
    assert_eq!(catalog.products("zh_CN").unwrap(), before);
    assert!(
        server
            .paths
            .lock()
            .unwrap()
            .contains(&"/shop/buy-iphone/iphone-18-pro".to_string())
    );
}

#[tokio::test]
#[ignore = "手动检查 Apple 真实商品刷新，需要网络"]
async fn live_china_refresh_discovers_new_iphones() {
    let region = region_by_locale("zh_CN").unwrap();
    let http = reqwest::Client::new();
    let slugs = apw_core::apple_catalog::discover_families(&http, region, Category::Iphone)
        .await
        .unwrap();
    println!("官网机型: {slugs:?}");
    assert!(slugs.iter().any(|s| s == "iphone-18-pro"));
    assert!(slugs.iter().any(|s| s == "iphone-duo"));
    let count = Catalog::new()
        .refresh_products(region, Some(Category::Iphone), &http)
        .await
        .unwrap();
    println!("刷新成功: {count} 个 SKU");
    assert!(count >= 40);
}

#[test]
fn watch_configuration_links_are_normalized_to_family_pages() {
    let region = region_by_locale("zh_CN").unwrap();
    let html = br#"<a href='/shop/buy-watch/apple-watch-ultra/case_ultra_4_ti_c49'>Ultra</a>
        <a href='/shop/buy-watch/apple-watch-hermes-ultra/mje04ch/b'>Hermes</a>"#;
    assert_eq!(
        parse_family_links(html, region, Category::Watch).unwrap(),
        vec!["apple-watch-hermes-ultra", "apple-watch-ultra"]
    );
}
