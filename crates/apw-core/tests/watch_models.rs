use apw_core::apple_catalog::parse_product_selection;
use apw_core::catalog::Catalog;
use apw_core::model::{Category, REGIONS};

#[test]
fn 七个地区的当前手表目录保留官方代数() {
    let catalog = Catalog::new();
    for region in REGIONS {
        let products = catalog.products(region.locale).unwrap();
        for product in products.iter().filter(|p| p.category == Category::Watch) {
            assert!(
                product.title.contains("Series 12")
                    || product.title.contains("SE 3")
                    || product.title.contains("Ultra 4"),
                "{} 缺少官方代数: {}",
                region.locale,
                product.title
            );
        }
    }
}

#[test]
fn 中文手表名称保留尺寸和固定规格() {
    let products = Catalog::new().products("zh_CN").unwrap();
    let ultra = products
        .iter()
        .find(|p| p.part_number == "MJCW4CH/B")
        .unwrap();
    assert_eq!(
        ultra.title,
        "Apple Watch Ultra 4 49 毫米 钛金属 GPS + 蜂窝网络 原色"
    );
    let se = products
        .iter()
        .find(|p| p.part_number == "MEHY4CH/B")
        .unwrap();
    assert_eq!(se.title, "Apple Watch SE 3 40 毫米 铝金属 GPS 午夜色");
}

fn selection(examples: serde_json::Value) -> Vec<apw_core::model::Product> {
    let raw = serde_json::json!({
        "products":[{"part":"OLD/A","dimensions":{"watch_cases-dimensionCaseSize":"49mm","watch_cases-dimensionColor":"natural"}}],
        "watchProductSelectionDataNoJS": examples,
        "displayValues":{"watch_cases-dimensionColor":{"natural":{"text":"原色"}}}
    });
    parse_product_selection(
        &serde_json::to_vec(&raw).unwrap(),
        Category::Watch,
        "apple-watch-ultra",
    )
    .unwrap()
}

#[test]
fn 相同购买页地址不把旧代商品改名成新代() {
    let examples = serde_json::json!([{"text":"Apple Watch Ultra 3 (GPS + 蜂窝网络)；49 毫米", "url":"https://www.apple.com.cn/shop/buy-watch/apple-watch-ultra/49mm-cellular-natural-titanium-black-trail-loop"}]);
    let got = selection(examples);
    assert_eq!(got[0].part_number, "OLD/A");
    assert_eq!(
        got[0].title,
        "Apple Watch Ultra 3 49 毫米 钛金属 GPS + 蜂窝网络 原色"
    );
}

#[test]
fn 示例缺失或代数冲突时不猜代数或固定规格() {
    for examples in [
        serde_json::Value::Null,
        serde_json::json!([{"text":"Apple Watch Ultra 3 (GPS)","url":"/49mm-gps-aluminium-band"},{"text":"Apple Watch Ultra 4 (GPS + Cellular)","url":"/49mm-cellular-titanium-band"}]),
    ] {
        let got = selection(examples);
        assert_eq!(got[0].title, "Apple Watch Ultra 49 毫米 原色");
    }
}
