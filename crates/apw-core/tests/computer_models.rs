use apw_core::apple_catalog::{parse_buy_page, parse_product_selection};
use apw_core::catalog::Catalog;
use apw_core::model::{Category, REGIONS};

#[test]
fn 当前平板保留芯片且已撤下页面不再出现() {
    for region in REGIONS {
        let products = Catalog::new().products(region.locale).unwrap();
        assert!(!products.iter().any(|p| p.family.starts_with("iphone17pro")));
        for p in products.iter().filter(|p| p.category == Category::Ipad) {
            if p.family.starts_with("ipadpro") {
                assert!(p.title.contains("(M5)"), "{}", p.title);
            }
            if p.family.starts_with("ipadair") {
                assert!(p.title.contains("(M4)"), "{}", p.title);
            }
            if p.family.starts_with("ipadmini") {
                assert!(p.title.contains("(A17 Pro)"), "{}", p.title);
            }
        }
    }
}

#[test]
fn 当前mac配置来自本次官网而非旧型号表() {
    let products = Catalog::new().products("zh_CN").unwrap();
    for p in products.iter().filter(|p| p.category == Category::Mac) {
        if p.family == "macbook-air" {
            assert!(p.title.contains("M5"), "{}", p.title);
        }
        if p.family == "imac" {
            assert!(p.title.contains("M4"), "{}", p.title);
        }
        println!("{} {}", p.part_number, p.title);
    }
    let air = products
        .iter()
        .find(|p| p.part_number == "MDHE4CH/A")
        .unwrap();
    assert!(
        air.title
            .contains("13 英寸 M5 10 核 CPU / 8 核 GPU 16GB 内存 512GB 存储"),
        "{}",
        air.title
    );
}

fn raw() -> serde_json::Value {
    serde_json::json!({"products":[{"btrOrFdPartNumber":"EXACT/A", "dimensions":{
        "chassis-dimensionScreensize":"13inch","chassis-dimensionColor":"midnight","processor-cpuCoreCount-gpuCoreCount":"10-8"
    }}]})
}

#[test]
fn 多种配置无法唯一对应时不猜内存() {
    let mut data = raw();
    data["macProductLinks"] = serde_json::json!([
        "/shop/buy-mac/macbook-air/13-inch-midnight-m5-chip-10-core-cpu-8-core-gpu-16gb-memory-512gb-storage",
        "/shop/buy-mac/macbook-air/13-inch-midnight-m5-chip-10-core-cpu-8-core-gpu-24gb-memory-512gb-storage",
        "/shop/buy-mac/macbook-air/15-inch-silver-m6-chip-12-core-cpu-12-core-gpu-48gb-memory-2tb-storage"
    ]);
    let p = parse_product_selection(
        &serde_json::to_vec(&data).unwrap(),
        Category::Mac,
        "macbook-air",
    )
    .unwrap();
    assert!(p[0].title.contains("M5"));
    assert!(p[0].title.contains("512GB 存储"));
    assert!(!p[0].title.contains("内存"));
    assert!(!p[0].title.contains("M6"));
}

#[test]
fn 在线html与离线数据使用相同的配置匹配规则() {
    let link = "/shop/buy-mac/macbook-air/13-inch-midnight-m5-chip-10-core-cpu-8-core-gpu-16gb-memory-512gb-storage";
    let mut data = raw();
    let page = format!(
        "<script>window.PRODUCT_SELECTION_BOOTSTRAP = {{productSelectionData:{}}};</script><a href='{}'>Current configuration</a>",
        data, link
    );
    data["macProductLinks"] = serde_json::json!([link]);
    assert_eq!(
        parse_buy_page(page.as_bytes(), Category::Mac, "macbook-air").unwrap(),
        parse_product_selection(
            &serde_json::to_vec(&data).unwrap(),
            Category::Mac,
            "macbook-air"
        )
        .unwrap()
    );
}
