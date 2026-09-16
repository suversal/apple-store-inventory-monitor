//! 商品与门店目录的测试。
//!
//! 两条主线：
//!
//! 1. **内嵌数据必须真的能用**。数据是编译期塞进二进制的，一旦哪个地区的文件
//!    结构变了、或者忘了往表里加一行，只有测试能发现 —— 运行时的表现是那个
//!    地区的下拉框空空如也，用户还以为是自己网不好。
//! 2. **查不到必须是 `None` / `Err`，绝不是 panic**。上游在
//!    `services/product.go:19` 与 `services/store.go:22` 用硬类型断言取选中项，
//!    目录一刷新、旧选项不复存在，整个程序当场崩掉。
//!
//! 在线抓取全部用本地 HTML 字符串测，不碰真实网络。

use apw_core::apple_catalog;
use apw_core::catalog::{Catalog, CatalogError};
use apw_core::model::{Category, Family, REGIONS, Region, region_by_locale};

/// 造一段带 `productSelectionData` 的购买页片段。
fn page(body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><script>
        window.PRODUCT_SELECTION_BOOTSTRAP = {{
        {body}
        }};
        </script></head><body></body></html>"#
    )
}

/// 一份最小但完整的商品数据。
const SELECTION: &str = r#"{
    "products": [
        {"partNumber":"MG724CH/A","familyType":"iphone17",
         "dimensionCapacity":"512gb","dimensionColor":"black"},
        {"partNumber":"MG714CH/A","familyType":"iphone17",
         "dimensionCapacity":"256gb","dimensionColor":"sage"}
    ],
    "displayValues": {
        "dimensionColor": {
            "title": {"singleVariantDisplayTitle": "颜色:"},
            "variantOrder": ["black","sage"],
            "black": {"value":"黑色"},
            "sage": {"value":"鼠尾草绿色"}
        }
    }
}"#;

// ---- 内嵌数据 ----

#[test]
fn 七个地区都能从内嵌数据读出商品与门店() {
    let catalog = Catalog::new();
    assert_eq!(REGIONS.len(), 7);

    for region in REGIONS {
        let products = catalog
            .products(region.locale)
            .unwrap_or_else(|e| panic!("{} 的内嵌商品数据不可用：{e}", region.locale));
        assert!(!products.is_empty(), "{} 没有任何商品", region.locale);

        let stores = catalog
            .stores(region.locale)
            .unwrap_or_else(|e| panic!("{} 的内嵌门店数据不可用：{e}", region.locale));
        assert!(!stores.is_empty(), "{} 没有任何门店", region.locale);

        for p in &products {
            assert!(
                !p.part_number.is_empty(),
                "{} 有零件号为空的商品",
                region.locale
            );
            assert!(
                p.title.starts_with(expected_prefix(p.category)),
                "{} 的 {:?} 展示名不像机型名：{}",
                region.locale,
                p.category,
                p.title
            );
            // 容量必须已经规范化过，界面直接拿去显示。
            assert!(
                !p.capacity.contains("gb") && !p.capacity.contains("tb"),
                "{} 的容量没规范化：{}",
                region.locale,
                p.capacity
            );
        }
        for s in &stores {
            assert!(!s.number.is_empty(), "{} 有编号为空的门店", region.locale);
            assert!(s.title.ends_with(&s.name), "门店展示名应当以门店名结尾");
        }
    }
}

/// 每个品类的展示名都该以什么开头。
///
/// Mac 与 Apple Watch 的商品数据里没有机型字段，展示名只能从购买页 slug 来 ——
/// 而 `/shop/buy-mac` 下面除了 Mac 还挂着显示器，所以那一类只能要求非空。
fn expected_prefix(category: Category) -> &'static str {
    match category {
        Category::Iphone => "iPhone",
        Category::Ipad => "iPad",
        Category::Mac => "",
        Category::Watch => "Apple Watch",
    }
}

#[test]
fn 四个品类在每个地区都有商品() {
    // 少了哪个品类，运行时的表现只是那个品类的下拉框空空如也 —— 用户会以为
    // 这个程序压根不支持 Mac，而不是「数据没抓下来」。
    let catalog = Catalog::new();
    for region in REGIONS {
        let products = catalog.products(region.locale).expect("内嵌数据应当可用");
        for category in Category::ALL {
            assert!(
                products.iter().any(|p| p.category == *category),
                "{} 的内嵌数据里没有 {} 商品",
                region.locale,
                category.title()
            );
        }
    }
}

#[test]
fn 中国大陆的商品展示名带本地化颜色() {
    let catalog = Catalog::new();
    let product = catalog
        .product_by_part("zh_CN", "MG724CH/A")
        .expect("内嵌数据里应当有这个零件号");

    assert_eq!(product.title, "iPhone 17 512GB 黑色");
    assert_eq!(product.family, "iphone17");
    assert_eq!(product.capacity, "512GB");
    // 颜色存的是本地化名称，不是 black 这种色值。
    assert_eq!(product.color, "黑色");
}

#[test]
fn 中国大陆门店包含北京荟聚() {
    let catalog = Catalog::new();
    let store = catalog
        .store_by_number("zh_CN", "R792")
        .expect("中国大陆门店目录应当包含北京荟聚 R792");

    assert_eq!(store.name, "北京荟聚");
    assert_eq!(store.title, "北京-北京荟聚");
}

#[test]
fn 日本站按零件号去重() {
    let catalog = Catalog::new();
    let products = catalog.products("ja_JP").expect("日本站数据应当可用");

    // 日本站把同一台机器按运营商和无锁版各列了一遍，零件号完全相同。
    // 不去重的话界面上会出现一模一样的两行，用户不知道该点哪个。
    let mut seen = std::collections::HashSet::new();
    for p in &products {
        assert!(
            seen.insert(p.part_number.clone()),
            "零件号 {} 出现了不止一次",
            p.part_number
        );
    }
    // 条数随 Apple 上下架而变，钉死它只会让快照一更新测试就红。真正要守住的
    // 是「同一零件号只出现一次」，那条上面已经断言过了。
    assert!(
        products.len() > 100,
        "日本站只有 {} 个型号，太少了",
        products.len()
    );
}

#[test]
fn 门店展示名的两种构造方式() {
    let catalog = Catalog::new();

    // hasStates 的地区用 stateName：上海-环球港。
    let cn = catalog
        .store_by_number("zh_CN", "R683")
        .expect("中国大陆应当有 R683");
    assert_eq!(cn.name, "环球港");
    assert_eq!(cn.title, "上海-环球港");

    // 没有 state 层级的地区（香港、新加坡）用 address.city。
    let hk = catalog
        .store_by_number("zh_HK", "R428")
        .expect("香港应当有 R428");
    assert_eq!(hk.title, "香港-ifc mall");

    // 日本站 stateName=Tokyo 而 city=Chiyoda-ku，必须挑前者，
    // 否则界面上会冒出一堆没人认得的区名。
    let jp = catalog
        .store_by_number("ja_JP", "R718")
        .expect("日本应当有 R718");
    assert_eq!(jp.title, "Tokyo-Marunouchi");
}

#[test]
fn 门店按编号去重且顺序稳定() {
    let catalog = Catalog::new();
    for region in REGIONS {
        let stores = catalog.stores(region.locale).expect("门店数据应当可用");
        let mut seen = std::collections::HashSet::new();
        for s in &stores {
            assert!(seen.insert(s.number.clone()), "门店 {} 重复了", s.number);
        }
        // 同一份内嵌数据反复读必须得到同一个顺序，否则下拉框每次打开都在跳。
        assert_eq!(stores, catalog.stores(region.locale).expect("再读一次"));
    }
}

// ---- 找不到时的行为 ----

#[test]
fn 找不到时返回空值而不是崩掉() {
    let catalog = Catalog::new();

    // 上游在这两个位置用 funk.Find(...).(model.Product) 硬断言，找不到直接 panic。
    assert!(catalog.product_by_part("zh_CN", "根本不存在/A").is_none());
    assert!(catalog.store_by_number("zh_CN", "R000").is_none());
    // 查不到只说明目录里没有它，绝不能被上层理解成「这个型号没货」。
    assert!(catalog.product_by_part("zh_CN", "").is_none());
}

#[test]
fn 未知地区一律返回错误而不是崩掉() {
    let catalog = Catalog::new();

    for locale in ["de_DE", "", "zh_cn", "../data/products_zh_CN.json"] {
        match catalog.products(locale) {
            Err(CatalogError::UnknownLocale { .. }) => {}
            other => panic!("地区 {locale:?} 的商品目录应当报「没有这个地区」，实际是 {other:?}"),
        }
        match catalog.stores(locale) {
            Err(CatalogError::UnknownLocale { .. }) => {}
            other => panic!("地区 {locale:?} 的门店目录应当报「没有这个地区」，实际是 {other:?}"),
        }
        assert!(catalog.product_by_part(locale, "MG724CH/A").is_none());
        assert!(catalog.store_by_number(locale, "R683").is_none());
    }
}

/// 一个没有配置任何机型的地区。
///
/// base_url 指向一个连不上的地址：万一将来有人拆掉了那道前置检查，这个测试会
/// 明确失败，而不是偷偷去打真实网络。
static NO_FAMILY_REGION: Region = Region {
    title: "测试",
    locale: "zh_CN",
    base_url: "https://127.0.0.1:1",
    families: &[],
};

/// 一个只配了 iPhone 购买页的地区，用来验证「按品类筛完是空的」也走同一条路。
static IPHONE_ONLY_REGION: Region = Region {
    title: "测试",
    locale: "zh_CN",
    base_url: "https://127.0.0.1:1",
    families: &[Family {
        category: Category::Iphone,
        slug: "iphone-17",
    }],
};

#[tokio::test]
async fn 没有可抓的购买页时不去发请求() {
    let catalog = Catalog::new();
    let http = reqwest::Client::builder().build().expect("构造客户端失败");

    match catalog
        .refresh_products(&NO_FAMILY_REGION, None, &http)
        .await
    {
        Err(CatalogError::NoFamilies { locale, .. }) => assert_eq!(locale, "zh_CN"),
        other => panic!("应当报「没有可抓取的购买页」，实际是 {other:?}"),
    }

    // 按品类筛完是空的，同样不该去打网络。base_url 指向一个连不上的地址：
    // 万一将来有人拆掉那道前置检查，这里会明确失败，而不是偷偷发真实请求。
    match catalog
        .refresh_products(&IPHONE_ONLY_REGION, Some(Category::Mac), &http)
        .await
    {
        Err(CatalogError::NoFamilies { locale, category }) => {
            assert_eq!(locale, "zh_CN");
            // 错误里必须说清是哪个品类没有，否则用户只会看到「这个地区没配」，
            // 而他明明能在 iPhone 那栏正常刷新。
            assert_eq!(category, "Mac");
        }
        other => panic!("应当报「没有可抓取的购买页」，实际是 {other:?}"),
    }

    // 刷新失败不该动到既有目录。
    assert!(!catalog.products("zh_CN").expect("仍然可读").is_empty());
}

// ---- 购买页提取 ----

#[test]
fn 正常页面能抓出商品() {
    let html = page(&format!("productSelectionData: {SELECTION}"));
    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当解析成功");

    assert_eq!(products.len(), 2);
    assert_eq!(products[0].part_number, "MG724CH/A");
    assert_eq!(products[0].title, "iPhone 17 512GB 黑色");
    assert_eq!(products[1].title, "iPhone 17 256GB 鼠尾草绿色");
}

#[test]
fn 先出现的同名字符串不会让整页失败() {
    // 页面里先有一段含同名文本的字符串字面量（埋点参数、提示文案里再正常不过）。
    // 只认第一个命中的话，定位会停在这里，发现后面不是冒号就判整页失败 ——
    // 表现是购买页明明带着完整数据，程序却一口咬定「结构不符」。
    let html = page(&format!(
        r#"note: "productSelectionData", tag: 'productSelectionData',
           productSelectionData: {SELECTION}"#
    ));
    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当跳过假命中");
    assert_eq!(products.len(), 2);
}

#[test]
fn 更长的标识符不会被误认() {
    // legacy_productSelectionData 里含有目标名，但它是另一个变量。认错的话，
    // 界面上会摆出一批早已下架的型号，用户守着永远不会有货的零件号 —— 比报错还糟。
    let stale = r#"{"products":[{"partNumber":"OLD000/A","familyType":"iphone12",
        "dimensionCapacity":"64gb","dimensionColor":"black"}],"displayValues":{}}"#;
    let html = page(&format!(
        "legacy_productSelectionData: {stale},\n productSelectionDataV2: {stale},\n \
         productSelectionData: {SELECTION}"
    ));

    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当命中真正的属性");
    assert_eq!(products.len(), 2);
    assert!(
        products.iter().all(|p| p.part_number != "OLD000/A"),
        "把 legacy_productSelectionData 当成了商品目录"
    );
}

#[test]
fn 只有更长标识符时明确报错() {
    let html = page(&format!("legacy_productSelectionData: {SELECTION}"));
    match apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17") {
        Err(CatalogError::PageSchema { detail }) => {
            assert!(detail.contains("更长标识符"), "错误说明太含糊：{detail}");
        }
        other => panic!("应当报结构不符，实际是 {other:?}"),
    }
}

#[test]
fn 带引号的键也认() {
    // 不带引号只是当前打包器的选择，不是契约。只认不带引号那种的话，
    // Apple 哪天换个打包器，整页数据就会栽在「之后不是冒号」上。
    for key in [
        r#""productSelectionData":"#,
        r#"'productSelectionData' :"#,
        "productSelectionData\n\t:",
    ] {
        let html = page(&format!("{key} {SELECTION}"));
        let products =
            apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
                .unwrap_or_else(|e| panic!("键写法 {key:?} 应当被接受：{e}"));
        assert_eq!(products.len(), 2);
    }
}

#[test]
fn 花括号配平但内容不是json时继续找下一个候选() {
    // 这段文案里的花括号是配平的，但内容根本不是 JSON。不做合法性校验的话，
    // 会抱着这段垃圾往下走，最后以「一个商品都解析不出来」收场，而真正的属性
    // 就在后面几个字节。
    let html = page(&format!(
        r#"tip: "productSelectionData 用法：productSelectionData: {{ 看这里 }}",
           productSelectionData: {SELECTION}"#
    ));
    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当跳过假命中");
    assert_eq!(products.len(), 2);
}

#[test]
fn 只有不是json的候选时报错而不是返回空目录() {
    let html = page("productSelectionData: { 这不是 JSON }");
    match apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17") {
        Err(CatalogError::PageSchema { detail }) => {
            assert!(detail.contains("合法 JSON"), "错误说明太含糊：{detail}");
        }
        other => panic!("应当报结构不符，实际是 {other:?}"),
    }
}

#[test]
fn 颜色字段里内嵌含花括号的html不会算错配对() {
    // 真实数据里颜色的 image 就是一整段 HTML，里面带着 { } 和转义引号。
    // 花括号配对不跳过字符串字面量的话，截出来的 JSON 会在半路断掉。
    let selection = r#"{
        "products":[{"partNumber":"MG724CH/A","familyType":"iphone17",
                     "dimensionCapacity":"512gb","dimensionColor":"black"}],
        "displayValues":{"dimensionColor":{
            "black":{"value":"黑色",
                     "image":"<div style=\"width:{204}px\" data-x='{\"a\":1}'>{ } {{</div>"}
        }}
    }"#;
    let html = page(&format!("productSelectionData: {selection}"));

    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当解析成功");
    assert_eq!(products.len(), 1);
    assert_eq!(products[0].title, "iPhone 17 512GB 黑色");
}

#[test]
fn 找不到全局变量时退化成全页搜索() {
    // 万一 Apple 改了变量名，还能靠键名多撑一阵子。
    let html = format!(
        "<script>window.SOMETHING_ELSE = {{ productSelectionData: {SELECTION} }};</script>"
    );
    let products = apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17")
        .expect("应当仍能找到");
    assert_eq!(products.len(), 2);
}

#[test]
fn 页面里根本没有商品数据时报错() {
    for html in [
        String::from("<html><body>Page Not Found</body></html>"),
        String::new(),
        // 花括号没闭合。
        page("productSelectionData: {\"products\":["),
        // 值不是对象。
        page(r#"productSelectionData: "已下线""#),
    ] {
        match apple_catalog::parse_buy_page(html.as_bytes(), Category::Iphone, "iphone-17") {
            Err(CatalogError::PageSchema { .. }) => {}
            other => panic!("页面 {html:?} 应当报结构不符，实际是 {other:?}"),
        }
    }
}

#[test]
fn 缺字段的商品条目不会拖垮整段数据() {
    // 线上数据随时可能变形。少一个字段只该让那一条退化，不该让整页目录报废。
    let selection = r#"{
        "products":[
            {"familyType":"iphone17","dimensionCapacity":"512gb"},
            {"partNumber":"","familyType":"iphone17"},
            {"partNumber":"MG724CH/A","familyType":"iphone17","dimensionColor":"black"},
            {"partNumber":"MG999CH/A"}
        ],
        "displayValues":{"dimensionColor":{"black":{"value":"黑色"}}}
    }"#;
    let products =
        apple_catalog::parse_product_selection(selection.as_bytes(), Category::Iphone, "iphone-17")
            .expect("应当解析成功");

    // 没有零件号的条目被丢掉，剩下两条都留着。
    assert_eq!(products.len(), 2);
    assert_eq!(products[0].title, "iPhone 17 黑色");
    // 连 familyType 都没有的那条，退回购买页 slug ——「iPhone 17」是实打实
    // 知道的（它就是从那一页抓来的），比留一个空白强。
    assert_eq!(products[1].part_number, "MG999CH/A");
    assert_eq!(products[1].title, "iPhone 17");
}

#[test]
fn 取不到本地化颜色名时退回色值() {
    // 留空会让同机型同容量的几个颜色在界面上长得一模一样，用户根本分不清。
    let selection = r#"{
        "products":[{"partNumber":"MG724CH/A","familyType":"iphone17",
                     "dimensionCapacity":"512gb","dimensionColor":"cosmicorange"}],
        "displayValues":{"dimensionColor":{"title":{"singleVariantDisplayTitle":"颜色:"}}}
    }"#;
    let products =
        apple_catalog::parse_product_selection(selection.as_bytes(), Category::Iphone, "iphone-17")
            .expect("应当解析成功");
    assert_eq!(products[0].color, "cosmicorange");
    assert_eq!(products[0].title, "iPhone 17 512GB cosmicorange");
}

#[test]
fn 地区表与内嵌数据一一对应() {
    let catalog = Catalog::new();
    // include_str! 的路径必须是字面量，所以那张表是手写的：漏了哪个地区，
    // 运行时的表现只是那个地区的下拉框空空如也，只能靠这里发现。
    for region in REGIONS {
        assert!(
            catalog.products(region.locale).is_ok(),
            "地区表里有 {} 但内嵌商品数据里没有",
            region.locale
        );
        assert!(
            region_by_locale(region.locale).is_some(),
            "{} 不在地区表里",
            region.locale
        );
    }
}
