use apw_core::config::{BrowserRefreshSettings, Settings, is_apple_bag_url};
use serde_json::json;

#[test]
fn old_settings_keep_refresh_disabled_and_wire_fields_roundtrip() {
    let old: Settings = serde_json::from_value(json!({"checkout": {"enabled": true}})).unwrap();
    assert_eq!(
        old.checkout.browser_refresh,
        BrowserRefreshSettings::default()
    );
    assert!(!old.checkout.browser_refresh.enabled);
    assert_eq!(old.checkout.browser_refresh.interval_seconds, 300);
    let value = json!({"enabled": true, "intervalSeconds": 600, "url": "https://www.apple.com.cn/shop/bag"});
    let refresh: BrowserRefreshSettings = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(refresh).unwrap(), value);
}

#[test]
fn refresh_interval_is_bounded_and_url_is_trimmed() {
    for (input, expected) in [(0, 60), (5, 60), (300, 300), (u64::MAX, 86_400)] {
        let mut settings = Settings::default();
        settings.checkout.browser_refresh.interval_seconds = input;
        settings.checkout.browser_refresh.url = " https://www.apple.com.cn/shop/bag ".into();
        settings.normalize();
        assert_eq!(settings.checkout.browser_refresh.interval_seconds, expected);
        assert!(settings.checkout.browser_refresh.validate().is_ok());
    }
}

#[test]
fn fixed_destinations_are_only_https_apple_bags() {
    for url in [
        "https://www.apple.com.cn/shop/bag",
        "https://www.apple.com/hk/shop/bag",
        "https://www.apple.com.cn/cn-k12/shop/bag/",
    ] {
        assert!(is_apple_bag_url(url), "{url}");
    }
    for url in [
        "http://www.apple.com.cn/shop/bag",
        "javascript:alert(1)",
        "https://www.apple.com.cn.evil.test/shop/bag",
        "https://www.apple.com.cn/shop/checkout",
        "https://www.apple.com.cn/shop/bag?checkout=1",
        "https://www.apple.com.cn/shop/bag#checkout",
        "https://user:pass@www.apple.com.cn/shop/bag",
        "https://www.apple.com.cn:8443/shop/bag",
        "https://secure.store.apple.com.cn/shop/signIn",
    ] {
        assert!(!is_apple_bag_url(url), "{url}");
        assert!(
            BrowserRefreshSettings {
                url: url.into(),
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
}
