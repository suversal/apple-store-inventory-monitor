//! 针对 Apple 真实接口的契约测试。默认不参与 `cargo test`，需要显式启用：
//!
//! ```shell
//! cargo test -p apw-core --features live -- --nocapture
//! ```
//!
//! 它存在的理由很直接：上游项目就是因为 Apple 悄悄换掉了接口而彻底失效，却因为
//! 没有任何测试，半年时间里没人发现程序返回的「无货」其实是被拦截。单元测试用
//! 假响应验证解析逻辑，但拦不住这种事；只有真的打一次 Apple 才行。
//!
//! 建议在 CI 里按天定时跑，而不是每次提交都跑，以免给 Apple 添麻烦。

#![cfg(feature = "live")]

use apw_core::apple::{ApiError, AppleClient, ClientConfig};
use apw_core::catalog::Catalog;
use apw_core::model::{Availability, Category, UnknownReason, region_by_locale};

/// 每个地区挑一家真实门店和一个真实零件号。
///
/// 零件号会随机型更新而失效，届时应当更新此表，而不是删掉测试。
const CASES: &[(&str, &str, &str)] = &[
    ("zh_CN", "R683", "MG724CH/A"),
    ("zh_HK", "R428", "MG6L4ZA/A"),
    ("zh_TW", "R694", "MG6L4ZP/A"),
    ("ja_JP", "R119", "MG6A4J/A"),
    ("en_SG", "R669", "MG6L4X/A"),
    ("en_AU", "R440", "MG6L4X/A"),
    ("en_MY", "R742", "MG6L4X/A"),
];

fn client() -> AppleClient {
    // 使用正式客户端配置，确保真实接口测试覆盖应用实际采用的请求节奏。
    AppleClient::new(ClientConfig::default()).expect("构造客户端失败")
}

#[tokio::test(flavor = "multi_thread")]
async fn 七个地区的接口都还能用() {
    let client = client();

    for (locale, store, part) in CASES {
        let region = region_by_locale(locale).expect("地区表里没有这个 locale");
        let parts = vec![part.to_string()];

        let result = client
            .pickup_message(region, store, &parts)
            .await
            .unwrap_or_else(|e| match e {
                ApiError::Blocked(d) => {
                    panic!("{locale} 的请求被 Apple 拦截，接口或请求特征需要调整：{d}")
                }
                ApiError::SchemaDrift { field, raw } => {
                    panic!("{locale} 的响应结构已改变，解析逻辑需要更新：{field} = {raw}")
                }
                other => panic!("{locale} 查询失败：{other}"),
            });

        assert_eq!(result.store_number, *store, "{locale} 返回了别的门店");
        let status = result.parts.get(*part).unwrap_or_else(|| {
            panic!("{locale} 的响应里没有零件号 {part}（可能已下架，需更新用例）")
        });

        assert!(
            !status.pickup_display.is_empty(),
            "{locale} 的 pickupDisplay 为空，字段名可能已变更"
        );

        // 是有货还是无货取决于当下库存，不做断言；但解析结果不能是「无法识别」——
        // 那说明 Apple 换了词表，得去补 availability_from 的分支。
        if let Availability::Unknown(UnknownReason::SchemaDrift { raw, .. }) = &status.availability
        {
            panic!("{locale} 返回了无法识别的 pickupDisplay {raw:?}，需要补充分支");
        }

        println!(
            "{locale:6} {:12} {part:12} -> {:4} (pickupDisplay={}, 商品名={})",
            result.store_name,
            status.availability.label(),
            status.pickup_display,
            status.product_title.as_deref().unwrap_or("(无)"),
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 必须以http2跟apple说话() {
    // 这条测试守的是一个「配置写漏了、功能却完全没坏」的缺陷。
    //
    // reqwest 的 HTTP/2 支持挂在 http2 feature 上，而我们用的是
    // default-features = false。那个 feature 曾经漏了整整一个版本：客户端静默
    // 退回 HTTP/1.1，查询照常成功、离线测试全绿，谁都看不出问题。但我们的 UA
    // 自称 Chrome 130 —— 真实的 Chrome 早就不用 HTTP/1.1 跟 apple.com 说话了，
    // 这个矛盾是最一眼可辨的机器人特征，在受风控审查的网络上直接换来 HTTP 541。
    //
    // 这种缺陷只有真的去连一次才发现得了，所以它必须待在契约测试里。
    let client = client();

    for locale in ["zh_CN", "zh_TW"] {
        let region = region_by_locale(locale).expect("地区表里没有这个 locale");
        let version = client
            .negotiated_http_version(region)
            .await
            .unwrap_or_else(|e| panic!("{locale} 探测 HTTP 版本失败：{e}"));

        assert!(
            version.contains('2'),
            "{locale} 协商出来的是 {version}，不是 HTTP/2 —— \
             检查 Cargo.toml 里 reqwest 的 http2 feature 是不是又漏了"
        );
        println!("{locale:6} 协商出的协议：{version}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 查询时必须带上cookie() {
    // Apple 的边缘节点会对没带 cookie 的取货查询下手。issue #3 的报告者在同一个
    // 浏览器里做了十轮成对对照，唯一的变量就是带不带 cookie：
    //
    //     带 cookie   10/10 全部 200
    //     不带 cookie  8/10  返回 541
    //
    // 要命的是这件事**只在受审查的网络上才看得出来**：在没被盯上的网络里两种
    // 都是 200，怎么对照都测不出差别 —— 这个假设正是这么被误杀过一次的。
    //
    // 所以这条测试不去验「带了 cookie 就不会被拦」（在这里验不出来），只钉住
    // 一件能验的事：**罐子里真的有东西**。一个「以为自己在带 cookie、其实罐是
    // 空的」的客户端，功能上和现在一模一样，不会有任何迹象。
    let client = client();
    let region = region_by_locale("zh_TW").expect("地区表里应当有中国台湾");

    assert!(
        client.cookies_for(region).is_none(),
        "还没发过任何请求，cookie 罐就该是空的"
    );

    let parts = vec!["MDV94TA/A".to_string()];
    let result = client.pickup_message(region, "R713", &parts).await;

    let cookies = client
        .cookies_for(region)
        .expect("查过一次之后，cookie 罐不该还是空的 —— 暖场或响应里的 Set-Cookie 没生效");

    // Cookie 是会话凭据，只打印名称，不把值写进测试日志。
    let cookie_names: Vec<_> = cookies
        .split(';')
        .filter_map(|cookie| cookie.trim().split_once('=').map(|(name, _)| name))
        .collect();
    println!("攒到的 Cookie 名称：{cookie_names:?}");
    // Apple 的 shop 会话 cookie。名字变了要来更新这里，而不是删掉断言。
    assert!(
        cookies.contains("dssid2") || cookies.contains("as_dc"),
        "攒到的 cookie 里没有 shop 的会话项，只有：{cookie_names:?}"
    );

    result.unwrap_or_else(|e| panic!("已成功建立 Cookie 会话，但库存查询失败：{e}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn 无效零件号不会被判成无货() {
    // 本项目最关键的不变量在真实接口上也必须成立。
    let client = client();
    let region = region_by_locale("zh_CN").unwrap();

    match client
        .pickup_message(region, "R683", &["NOSUCHPART/A".to_string()])
        .await
    {
        // Apple 对无效零件号会直接报业务错误，这是可接受且正确的结果。
        Err(e) => println!("无效零件号返回错误（可接受）：{e}"),
        Ok(result) => {
            if let Some(status) = result.parts.get("NOSUCHPART/A") {
                assert_ne!(
                    status.availability,
                    Availability::OutOfStock,
                    "无效零件号被判定成了「无货」，这正是上游那个致命缺陷：{status:?}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 四个品类的零件号接口都认() {
    // 「iPad / Mac / Apple Watch 也能用同一个取货接口查」是整个多品类支持的前提。
    // 它是个关于 Apple 的假设，不是我们能在本地测出来的东西：一旦哪天 Mac 的
    // 零件号不再被这个接口接受，界面上会是一屏「未知」，而用户只会以为又被拦了。
    //
    // 零件号从内嵌目录里现取，不写死：写死的表会随机型更新失效，然后有人为了
    // 让 CI 变绿而删掉测试。
    let client = client();
    let region = region_by_locale("zh_CN").expect("地区表里应当有中国大陆");
    let catalog = Catalog::new();
    let products = catalog.products("zh_CN").expect("内嵌目录应当可用");

    for category in Category::ALL {
        let product = products
            .iter()
            .find(|p| p.category == *category)
            .unwrap_or_else(|| panic!("内嵌目录里没有 {} 商品", category.title()));
        let parts = vec![product.part_number.clone()];

        let result = client
            .pickup_message(region, "R683", &parts)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "{} 的零件号 {} 查询失败：{e}",
                    category.title(),
                    product.part_number
                )
            });

        let status = result.parts.get(&product.part_number).unwrap_or_else(|| {
            panic!(
                "{} 的响应里没有零件号 {}（接口可能不接受这个品类）",
                category.title(),
                product.part_number
            )
        });

        // 有货没货取决于当下库存，不做断言。要守住的是「必须拿到明确答复」——
        // 拿不到就说明这条链路对这个品类是断的。
        assert!(
            !status.availability.is_unknown(),
            "{} 的 {} 没拿到明确答复：{:?}",
            category.title(),
            product.part_number,
            status.availability
        );

        println!(
            "{:11} {:12} -> {:4} （{}）",
            category.title(),
            product.part_number,
            status.availability.label(),
            product.title
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn 一次请求可以带多个零件号() {
    // 这决定了正确的请求策略：每门店每轮只发一个请求，覆盖该店所有关注型号。
    let client = client();
    let region = region_by_locale("zh_CN").unwrap();
    let parts: Vec<String> = ["MG724CH/A", "MG0A4CH/A", "MG364CH/A"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    let result = client
        .pickup_message(region, "R683", &parts)
        .await
        .expect("批量查询应当成功");

    assert_eq!(
        result.parts.len(),
        parts.len(),
        "批量请求的型号数与返回的不一致：{:?}",
        result.parts.keys().collect::<Vec<_>>()
    );
    for p in &parts {
        assert!(result.parts.contains_key(p), "响应里缺少 {p}");
    }
}
