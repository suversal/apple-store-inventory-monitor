use apw_core::model::{Category, Product, Target};

fn target(locale: &str, part: &str) -> Target {
    Target {
        locale: locale.into(),
        store_number: "R390".into(),
        store_title: "Test store".into(),
        part_number: part.into(),
        product_name: "Test product".into(),
    }
}

#[test]
fn 商品链接保留地区前缀和完整料号() {
    for (locale, base) in [
        ("zh_CN", "https://www.apple.com.cn"),
        ("zh_HK", "https://www.apple.com/hk-zh"),
        ("zh_TW", "https://www.apple.com/tw"),
        ("ja_JP", "https://www.apple.com/jp"),
        ("en_SG", "https://www.apple.com/sg"),
        ("en_AU", "https://www.apple.com/au"),
        ("en_MY", "https://www.apple.com/my"),
    ] {
        assert_eq!(
            target(locale, "MG6W4CH/A").purchase_url(None),
            Some(format!("{base}/shop/product/MG6W4CH/A")),
        );
    }
    assert_eq!(target("unknown", "MG6W4CH/A").purchase_url(None), None);
}

#[test]
fn 料号中的特殊字符不会变成查询参数或片段() {
    let raw = target("zh_CN", "TEST?#/A").purchase_url(None).unwrap();
    let url = reqwest::Url::parse(&raw).unwrap();
    assert_eq!(url.host_str(), Some("www.apple.com.cn"));
    assert_eq!(url.path(), "/shop/product/TEST%3F%23/A");
    assert_eq!(url.query(), None);
    assert_eq!(url.fragment(), None);
}

#[test]
fn 表壳料号打开对应系列配置页以便继续选择表带() {
    let product = Product {
        part_number: "MFA04CH/B".into(),
        category: Category::Watch,
        family: "apple-watch".into(),
        capacity: "46 mm".into(),
        color: "Test".into(),
        title: "Apple Watch".into(),
    };
    assert_eq!(
        target("zh_HK", "MFA04CH/B").purchase_url(Some(&product)),
        Some("https://www.apple.com/hk-zh/shop/buy-watch/apple-watch".into()),
    );
}
