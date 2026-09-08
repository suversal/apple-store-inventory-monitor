//! 取货状态响应的解析测试。
//!
//! 这些用例守的是同一条线：**任何「查不到 / 判不了」都必须是 Unknown，
//! 绝不能变成 OutOfStock**。上游正是在这里失守，静默失效了大半年。

use std::time::Duration;

use apw_core::apple::{ApiError, ClientConfig, availability_from, parse_pickup_message};
use apw_core::model::{Availability, UnknownReason};

/// 造一份结构正常的响应。
fn response(parts: &str) -> String {
    format!(
        r#"{{"head":{{"status":"200"}},
            "body":{{"stores":[{{"storeNumber":"R683","storeName":"环球港",
                     "partsAvailability":{{{parts}}}}}]}}}}"#
    )
}

fn part(display: &str) -> String {
    format!(
        r#""MG724CH/A":{{"partNumber":"MG724CH/A","pickupDisplay":"{display}",
            "messageTypes":{{"regular":{{"storePickupProductTitle":"iPhone 17 512GB 黑色"}}}}}}"#
    )
}

#[test]
fn 明确取值被正确翻译() {
    for (display, want) in [
        ("available", Availability::InStock),
        ("unavailable", Availability::OutOfStock),
        ("ineligible", Availability::OutOfStock),
    ] {
        let raw = response(&part(display));
        let got = parse_pickup_message(raw.as_bytes(), "R683").expect("应当解析成功");
        assert_eq!(
            got.parts["MG724CH/A"].availability, want,
            "pickupDisplay={display} 翻译错了"
        );
        assert_eq!(got.store_name, "环球港");
    }
}

#[test]
fn 无法识别的取值绝不能变成无货() {
    // 这是本项目的核心不变量。Apple 若新增取值或改掉字段名，解析器必须落到
    // 「未知 + 带上原始值」，而不是随便猜一个已知状态。
    for display in ["limitedAvailability", "AVAILABLE_SOON", "", "  ", "无货"] {
        let raw = response(&part(display));
        let got = parse_pickup_message(raw.as_bytes(), "R683").expect("应当解析成功");
        let availability = &got.parts["MG724CH/A"].availability;

        assert_ne!(
            *availability,
            Availability::OutOfStock,
            "pickupDisplay={display:?} 被判成了「无货」，这正是上游那个致命缺陷"
        );
        assert_ne!(*availability, Availability::InStock);

        match availability {
            Availability::Unknown(UnknownReason::SchemaDrift { raw, .. }) => {
                // 原始取值必须带出来，否则排查时无从下手。
                assert_eq!(raw, &display.trim().to_ascii_lowercase());
            }
            other => panic!("pickupDisplay={display:?} 应当是接口漂移，实际为 {other:?}"),
        }
    }
}

#[test]
fn 信封里的错误优先于门店数据() {
    // 这条是独立审查找出来的：errorMessage 非空但 stores 也非空时，Go 版会
    // 无视错误照常解析，最终得出「无货」。
    let raw = format!(
        r#"{{"head":{{"status":"500"}},
            "body":{{"errorMessage":"request failed",
                     "stores":[{{"storeNumber":"R683","storeName":"环球港",
                      "partsAvailability":{{{}}}}}]}}}}"#,
        part("unavailable")
    );
    match parse_pickup_message(raw.as_bytes(), "R683") {
        Err(ApiError::Apple(msg)) => assert!(msg.contains("request failed")),
        other => panic!("带着 errorMessage 的响应必须报错，实际为 {other:?}"),
    }
}

#[test]
fn head状态非成功值时报错() {
    let raw = format!(
        r#"{{"head":{{"status":"500"}},
            "body":{{"stores":[{{"storeNumber":"R683","storeName":"环球港",
                     "partsAvailability":{{{}}}}}]}}}}"#,
        part("unavailable")
    );
    match parse_pickup_message(raw.as_bytes(), "R683") {
        Err(ApiError::SchemaDrift { field, .. }) => assert_eq!(field, "head.status"),
        other => panic!("head.status=500 必须报错，实际为 {other:?}"),
    }
}

#[test]
fn head状态缺失不算失败() {
    // 保留的旧接口兜底路径本就没有 head 这一层，把「没给」当失败会让它直接报废。
    for head in [r#""head":{},"#, r#""head":{"status":null},"#, ""] {
        let raw = format!(
            r#"{{{head}"body":{{"stores":[{{"storeNumber":"R683","storeName":"环球港",
                 "partsAvailability":{{{}}}}}]}}}}"#,
            part("available")
        );
        let got = parse_pickup_message(raw.as_bytes(), "R683")
            .unwrap_or_else(|e| panic!("head={head:?} 时不该报错：{e}"));
        assert!(got.parts["MG724CH/A"].availability.is_in_stock());
    }
}

#[test]
fn head状态是数字200也接受() {
    let raw = format!(
        r#"{{"head":{{"status":200}},
            "body":{{"stores":[{{"storeNumber":"R683","storeName":"环球港",
                     "partsAvailability":{{{}}}}}]}}}}"#,
        part("available")
    );
    assert!(parse_pickup_message(raw.as_bytes(), "R683").is_ok());
}

#[test]
fn 零件号自相矛盾时报错而不是随便采信一个() {
    let raw = response(
        r#""MG724CH/A":{"partNumber":"MG0A4CH/A","pickupDisplay":"available",
            "messageTypes":{"regular":{}}}"#,
    );
    match parse_pickup_message(raw.as_bytes(), "R683") {
        Err(ApiError::SchemaDrift { raw, .. }) => {
            assert!(raw.contains("MG0A4CH/A") && raw.contains("MG724CH/A"));
        }
        other => panic!("零件号不一致必须报错，实际为 {other:?}"),
    }
}

#[test]
fn 条目省略零件号是允许的() {
    let raw =
        response(r#""MG724CH/A":{"pickupDisplay":"available","messageTypes":{"regular":{}}}"#);
    let got = parse_pickup_message(raw.as_bytes(), "R683").expect("省略冗余字段应当被接受");
    assert!(got.parts["MG724CH/A"].availability.is_in_stock());
}

#[test]
fn 门店号对不上时不冒充() {
    let raw = response(&part("available"));
    match parse_pickup_message(raw.as_bytes(), "R390") {
        Err(ApiError::SchemaDrift { raw, .. }) => assert!(raw.contains("R390")),
        other => panic!("门店号不符必须报错，实际为 {other:?}"),
    }
}

#[test]
fn 空的型号表报错而不是当成无货() {
    let raw = r#"{"head":{"status":"200"},
        "body":{"stores":[{"storeNumber":"R683","storeName":"环球港","partsAvailability":{}}]}}"#;
    assert!(matches!(
        parse_pickup_message(raw.as_bytes(), "R683"),
        Err(ApiError::SchemaDrift { .. })
    ));
}

#[test]
fn 不是json时报接口漂移() {
    let raw = b"<!doctype html><html><title>Page Not Found</title></html>";
    assert!(matches!(
        parse_pickup_message(raw, "R683"),
        Err(ApiError::SchemaDrift { .. })
    ));
}

#[test]
fn 旧接口的嵌套结构仍能兜底解析() {
    let raw = format!(
        r#"{{"body":{{"content":{{"pickupMessage":{{"stores":[
            {{"storeNumber":"R683","storeName":"环球港","partsAvailability":{{{}}}}}]}}}}}}}}"#,
        part("available")
    );
    let got = parse_pickup_message(raw.as_bytes(), "R683").expect("兜底路径应当仍然可用");
    assert!(got.parts["MG724CH/A"].availability.is_in_stock());
}

#[test]
fn 任何请求错误都只能变成未知() {
    // 这是类型层面的不变量：ApiError 到状态的转换是全覆盖且单向的，
    // 没有任何一条错误能变成有货或无货。
    let errors = [
        ApiError::Blocked("HTTP 541".into()),
        ApiError::RateLimited("HTTP 429".into()),
        ApiError::SchemaDrift {
            field: "x".into(),
            raw: "y".into(),
        },
        ApiError::Apple("Product(s) Invalid or not buyable".into()),
        ApiError::Transport("connection refused".into()),
    ];
    for err in errors {
        let text = err.to_string();
        let state = Availability::Unknown(err.into_unknown_reason());
        assert!(state.is_unknown(), "{text} 竟然没落到未知");
        assert!(state.is_failure(), "{text} 应当被算作故障");
        assert!(state.describe_reason().is_some(), "{text} 没能说出原因");
    }
}

#[test]
fn 只有临时网络和限流错误适合快速重试() {
    assert!(!ApiError::Blocked(String::new()).is_retryable());
    assert!(ApiError::RateLimited(String::new()).is_retryable());
    assert!(ApiError::Transport(String::new()).is_retryable());
    // 结构不符和业务错误重试多少次结果都一样。
    assert!(!ApiError::Apple(String::new()).is_retryable());
    assert!(
        !ApiError::SchemaDrift {
            field: String::new(),
            raw: String::new()
        }
        .is_retryable()
    );
}

#[test]
fn 正式客户端为会话暖场留出足够间隔() {
    assert!(ClientConfig::default().min_interval >= Duration::from_secs(2));
}

#[test]
fn 大小写与空白不影响判定() {
    assert_eq!(availability_from("  AVAILABLE  "), Availability::InStock);
    assert_eq!(availability_from("Unavailable"), Availability::OutOfStock);
}
