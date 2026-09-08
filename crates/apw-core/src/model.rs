//! 跨模块共享的核心类型。
//!
//! 这个模块不发起网络请求，也不认识任何界面框架。

use serde::{Deserialize, Serialize};

/// 某个型号在某个门店的可取货状态。
///
/// 这是整个项目唯一真正重要的类型，它的形状本身就是一条不变量：
/// **任何「查不到 / 判不了」的情况都必须是 `Unknown`，绝不能变成 `OutOfStock`。**
///
/// 上游 hteen/apple-store-helper 正是死在这条线上：它把请求失败折叠成「无货」，
/// Apple 换掉接口之后，所有用户看到的都是一屏永远不变的「无货」，程序看上去
/// 在正常工作，实际上早已失去意义 —— 而且这样静默地失效了大半年没人察觉。
///
/// 用 Rust 重写的首要理由就是让编译器来守这条线，而不是靠测试和自觉：
///
/// * `Unknown` 必须携带 [`UnknownReason`]，构造不出一个「没有原因的未知」。
///   Go 版把状态和原因拆成 `Availability` 与 `LastError` 两个字段，两者可能
///   不同步 —— 独立审查找到的两条最严重的缺陷，根子都在这种不同步上。
/// * 刻意不实现 `Default`。有了默认值，就会有人在解析失败时顺手
///   `unwrap_or_default()`，而那个默认值迟早会被写成「无货」。
/// * 所有分支处理都得 `match` 到底，将来新增状态时，漏掉的地方直接编译不过。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Availability {
    /// Apple 明确回答了「此门店该型号可取货」。
    InStock,
    /// Apple 明确回答了「此门店该型号不可取货」。
    OutOfStock,
    /// 没有得到明确答复。这**不代表**没有货。
    Unknown(UnknownReason),
}

impl Availability {
    /// 是否明确有货。只有这一种情况才该提醒用户。
    pub fn is_in_stock(&self) -> bool {
        matches!(self, Self::InStock)
    }

    /// 是否处于「说不清」的状态。界面必须把它和「无货」区分开展示。
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    /// 当前状态是否代表一次故障，用于统计失败数与决定是否退避。
    pub fn is_failure(&self) -> bool {
        match self {
            Self::InStock | Self::OutOfStock => false,
            Self::Unknown(reason) => reason.is_failure(),
        }
    }

    /// 供界面展示的中文描述。
    pub fn label(&self) -> &'static str {
        match self {
            Self::InStock => "有货",
            Self::OutOfStock => "无货",
            Self::Unknown(UnknownReason::NotYetChecked) => "待查询",
            Self::Unknown(_) => "未知",
        }
    }

    /// 处于未知状态时返回一句可读的原因说明，否则返回 `None`。
    pub fn describe_reason(&self) -> Option<String> {
        match self {
            Self::InStock | Self::OutOfStock => None,
            Self::Unknown(reason) => Some(reason.describe()),
        }
    }
}

/// 状态为 [`Availability::Unknown`] 时的具体原因。
///
/// 每一个变体都对应一种真实发生过的失效方式。界面要据此告诉用户「现在的状态
/// 为什么不可信」，而不是只丢一个「未知」让人干瞪眼。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum UnknownReason {
    /// 刚加进监控列表，还没轮到它。这是唯一一种「正常」的未知。
    NotYetChecked,
    /// 请求被 Apple 边缘节点拦截。
    ///
    /// Apple 的商品页会先完成浏览器环境校验；缺少有效页面会话的请求可能返回
    /// HTTP 541。它只表示查询被拒绝，不能当作无货。
    Blocked { detail: String },
    /// 触发了频率限制，正在退避。
    RateLimited,
    /// 响应能解析成 JSON，但结构与预期不符 —— 通常意味着 Apple 又改了接口。
    ///
    /// 必须让用户看见。带上出问题的字段与原始取值，否则排查时无从下手。
    SchemaDrift { field: String, raw: String },
    /// Apple 明确返回了一条业务错误信息。
    AppleError { message: String },
    /// 网络层面的失败：连不上、超时、TLS 出错等。
    Transport { detail: String },
}

impl UnknownReason {
    /// 供界面展示的一句话说明。
    pub fn describe(&self) -> String {
        match self {
            Self::NotYetChecked => "尚未查询".to_string(),
            Self::Blocked { detail } => format!("请求被 Apple 拦截：{detail}"),
            Self::RateLimited => "请求过于频繁被限流，正在退避".to_string(),
            Self::SchemaDrift { field, raw } => {
                format!("接口返回结构与预期不符：字段 {field} 的取值为 {raw:?}")
            }
            Self::AppleError { message } => format!("Apple 返回错误：{message}"),
            Self::Transport { detail } => format!("网络请求失败：{detail}"),
        }
    }

    /// 这个原因是否代表一次真正的故障。
    ///
    /// `NotYetChecked` 只是还没轮到，不该被算进失败数，也不该触发告警或退避。
    pub fn is_failure(&self) -> bool {
        !matches!(self, Self::NotYetChecked)
    }
}

/// 一个可监控的商品品类。
///
/// 品类不只是界面上的一个筛选器：它决定购买页挂在哪条路径下
/// （`/shop/buy-iphone` 与 `/shop/buy-mac` 是两套页面），也决定那页数据该按
/// 哪种形状解析（见 [`crate::apple_catalog`]）。因此它必须是一个类型，而不是
/// 散落在各处的字符串。
///
/// 变体顺序就是界面上的排列顺序，也是商品排序时的第一关键字。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    Iphone,
    Ipad,
    Mac,
    Watch,
}

impl Category {
    /// 全部品类，顺序与变体声明一致。
    ///
    /// 写成常量而不是让调用方自己列举：将来新增品类时，凡是遍历这里的地方
    /// 都会自动跟上，不会有谁悄悄漏掉一个。
    pub const ALL: &'static [Category] = &[Self::Iphone, Self::Ipad, Self::Mac, Self::Watch];

    /// 购买页所在的路径段，如 `buy-iphone`。
    pub fn buy_path(self) -> &'static str {
        match self {
            Self::Iphone => "buy-iphone",
            Self::Ipad => "buy-ipad",
            Self::Mac => "buy-mac",
            Self::Watch => "buy-watch",
        }
    }

    /// 界面上展示的品类名。四个地区站点都用这一组英文原名，无需本地化。
    pub fn title(self) -> &'static str {
        match self {
            Self::Iphone => "iPhone",
            Self::Ipad => "iPad",
            Self::Mac => "Mac",
            Self::Watch => "Apple Watch",
        }
    }
}

/// 一个可抓取的购买页：品类 + slug。
///
/// slug 单独存在没有意义 —— `apple-watch` 和 `macbook-air` 只有配上各自的品类
/// 才能拼出地址。把两者绑在一个类型里，就不存在「拿 Mac 的 slug 去拼 iPhone
/// 的路径」这种拼错的可能。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Family {
    pub category: Category,
    /// 购买页 slug，如 `iphone-17`、`macbook-air`。
    pub slug: &'static str,
}

/// 一个 Apple 在线商店的地区站点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    /// 界面上展示的地区名。
    pub title: &'static str,
    /// Apple 的语言地区标识，如 `zh_CN`，同时用作内置目录数据的文件名后缀。
    pub locale: &'static str,
    /// 该地区商店站点前缀，不含结尾斜杠。
    ///
    /// 注意中国大陆用的是独立域名 `www.apple.com.cn`，而不是 `www.apple.com/cn`。
    /// 上游按 `www.apple.com/{shortCode}` 统一拼接，对中国大陆是错的。
    pub base_url: &'static str,
    /// 需要抓取的购买页，用于在线刷新商品目录。
    pub families: &'static [Family],
}

impl Region {
    /// 取货状态查询接口地址。
    pub fn pickup_message_url(&self) -> String {
        // Apple 当前商品页的 fulfillmentBootstrap.pickupURL 明确指向这里。
        // 这个端点必须在真实网页会话完成 shld 握手后访问；具体传输由宿主负责。
        format!("{}/shop/fulfillment-messages", self.base_url)
    }

    /// 购物袋页面地址。
    pub fn bag_url(&self) -> String {
        format!("{}/shop/bag", self.base_url)
    }

    /// 某个购买页的地址，用于在线刷新商品目录。
    pub fn buy_page_url(&self, family: &Family) -> String {
        format!(
            "{}/shop/{}/{}",
            self.base_url,
            family.category.buy_path(),
            family.slug
        )
    }

    /// 该地区某个品类下的全部购买页。
    pub fn families_in(&self, category: Category) -> impl Iterator<Item = &'static Family> {
        self.families.iter().filter(move |f| f.category == category)
    }

    /// 该地区请求应当带的 `Accept-Language`。
    pub fn accept_language(&self) -> &'static str {
        match self.locale {
            "zh_CN" => "zh-CN,zh;q=0.9",
            "zh_HK" => "zh-HK,zh;q=0.9",
            "zh_TW" => "zh-TW,zh;q=0.9",
            "ja_JP" => "ja-JP,ja;q=0.9",
            _ => "en-US,en;q=0.9",
        }
    }
}

/// 当前在售的购买页。
///
/// 新机型发布后只需在这里追加一行，商品目录会自动从 Apple 官网抓取，
/// 不必像上游那样每代都手工从开发者工具里复制 `productSelectionData`。
///
/// 七个地区共用同一张表：实测这些 slug 在每个站点都存在（见
/// `tests/live.rs` 里的契约测试）。某个地区少了其中一页也不至于出事 ——
/// [`crate::catalog::Catalog::refresh_products`] 会保住其余页的结果，
/// 只把失败的那几页报出来。
const DEFAULT_FAMILIES: &[Family] = &[
    Family {
        category: Category::Iphone,
        slug: "iphone-17",
    },
    Family {
        category: Category::Iphone,
        slug: "iphone-17-pro",
    },
    Family {
        category: Category::Iphone,
        slug: "iphone-air",
    },
    Family {
        category: Category::Ipad,
        slug: "ipad-pro",
    },
    Family {
        category: Category::Ipad,
        slug: "ipad-air",
    },
    Family {
        category: Category::Ipad,
        slug: "ipad",
    },
    Family {
        category: Category::Ipad,
        slug: "ipad-mini",
    },
    Family {
        category: Category::Mac,
        slug: "macbook-air",
    },
    Family {
        category: Category::Mac,
        slug: "macbook-pro",
    },
    Family {
        category: Category::Mac,
        slug: "macbook-neo",
    },
    Family {
        category: Category::Mac,
        slug: "imac",
    },
    Family {
        category: Category::Mac,
        slug: "mac-mini",
    },
    Family {
        category: Category::Mac,
        slug: "mac-studio",
    },
    // 显示器不是 Mac，但 Apple 自己把它们摆在 /shop/buy-mac 下面，取货查询
    // 也走同一个接口。盯一台 Studio Display 到货和盯一台 Mac 是同一件事，
    // 没有理由单开一个品类。
    Family {
        category: Category::Mac,
        slug: "studio-display",
    },
    Family {
        category: Category::Mac,
        slug: "studio-display-xdr",
    },
    Family {
        category: Category::Watch,
        slug: "apple-watch",
    },
    Family {
        category: Category::Watch,
        slug: "apple-watch-se",
    },
    Family {
        category: Category::Watch,
        slug: "apple-watch-ultra",
    },
    Family {
        category: Category::Watch,
        slug: "apple-watch-hermes",
    },
    Family {
        category: Category::Watch,
        slug: "apple-watch-hermes-ultra",
    },
];

/// 内置地区表。
///
/// 每个 `base_url` 都对应 Apple 当前在线商店的正式地区入口。
pub const REGIONS: &[Region] = &[
    Region {
        title: "中国大陆",
        locale: "zh_CN",
        base_url: "https://www.apple.com.cn",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "中国香港",
        locale: "zh_HK",
        base_url: "https://www.apple.com/hk-zh",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "中国台湾",
        locale: "zh_TW",
        base_url: "https://www.apple.com/tw",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "日本",
        locale: "ja_JP",
        base_url: "https://www.apple.com/jp",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "Singapore",
        locale: "en_SG",
        base_url: "https://www.apple.com/sg",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "Australia",
        locale: "en_AU",
        base_url: "https://www.apple.com/au",
        families: DEFAULT_FAMILIES,
    },
    Region {
        title: "Malaysia",
        locale: "en_MY",
        base_url: "https://www.apple.com/my",
        families: DEFAULT_FAMILIES,
    },
];

/// 按 locale 查找地区。
pub fn region_by_locale(locale: &str) -> Option<&'static Region> {
    REGIONS.iter().find(|r| r.locale == locale)
}

/// 一个具体可购买的配置（机型 + 各项规格）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    /// Apple 零件号，如 `MG724CH/A`，查询库存时的唯一标识。
    pub part_number: String,
    /// 所属品类。界面靠它分组，[`crate::catalog`] 靠它决定刷新哪一批购买页。
    pub category: Category,
    /// 机型系列，如 `iphone17`、`macbook-air`。
    ///
    /// iPhone 与 iPad 用购买页数据里的 `familyType`（同一页可能有好几个，
    /// 比如 Pro 和 Pro Max 就同页）；Mac 与 Apple Watch 的数据里没有这个字段，
    /// 退回用购买页 slug。两者都只用来分组和排序，不参与任何库存判定。
    pub family: String,
    /// 存储容量，如 `512GB`。Apple Watch 这类没有容量维度的品类为空。
    pub capacity: String,
    /// 颜色的本地化名称，如「黑色」。取不到时为空。
    pub color: String,
    /// 界面展示名，如「iPhone 17 512GB 黑色」。
    pub title: String,
}

/// 一家 Apple 直营店。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Store {
    /// 门店编号，如 `R683`，查询库存时的唯一标识。
    pub number: String,
    /// 门店名，如「环球港」。
    pub name: String,
    /// 界面展示名，如「上海-环球港」。
    pub title: String,
}

/// 一条监控目标：在某地区的某门店盯某个型号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Target {
    pub locale: String,
    pub store_number: String,
    pub store_title: String,
    pub part_number: String,
    pub product_name: String,
}

impl Target {
    /// 唯一键，用于去重与状态索引。
    pub fn key(&self) -> TargetKey {
        TargetKey(format!(
            "{}|{}|{}",
            self.locale, self.store_number, self.part_number
        ))
    }
}

/// [`Target`] 的唯一键。
///
/// 单独包一层而不是直接用 `String`，是为了防止它和门店号、零件号这类同样是
/// 字符串的标识混用 —— 那种串味的 bug 编译器不会拦，但类型可以。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetKey(pub String);

impl std::fmt::Display for TargetKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 未知状态必然带着原因() {
        // 与其说这个测试在测行为，不如说它在钉住类型的形状：
        // Availability::Unknown 是元组变体，写不出一个不带原因的未知。
        let a = Availability::Unknown(UnknownReason::Blocked {
            detail: "HTTP 541".into(),
        });
        assert!(a.is_unknown());
        assert!(!a.is_in_stock());
        assert!(
            a.describe_reason()
                .expect("未知状态必须能说出原因")
                .contains("541")
        );
        assert!(Availability::OutOfStock.describe_reason().is_none());
    }

    #[test]
    fn 尚未查询不算故障() {
        assert!(!UnknownReason::NotYetChecked.is_failure());
        assert!(UnknownReason::RateLimited.is_failure());
        assert!(
            UnknownReason::Blocked {
                detail: String::new()
            }
            .is_failure()
        );

        assert!(!Availability::Unknown(UnknownReason::NotYetChecked).is_failure());
        assert!(!Availability::OutOfStock.is_failure());
        assert!(Availability::Unknown(UnknownReason::RateLimited).is_failure());
    }

    #[test]
    fn 待查询与未知与无货在界面上是三种文案() {
        let pending = Availability::Unknown(UnknownReason::NotYetChecked);
        let broken = Availability::Unknown(UnknownReason::RateLimited);
        assert_eq!(pending.label(), "待查询");
        assert_eq!(broken.label(), "未知");
        assert_eq!(Availability::OutOfStock.label(), "无货");
        // 三者必须互不相同 —— 上游把它们全糊成一种显示，用户无从分辨程序是在
        // 正常工作还是已经废了。
        assert_ne!(pending.label(), broken.label());
        assert_ne!(broken.label(), Availability::OutOfStock.label());
    }

    #[test]
    fn 地区表里每个站点都能拼出接口地址() {
        assert_eq!(REGIONS.len(), 7);
        let cn = region_by_locale("zh_CN").expect("地区表里应当有中国大陆");
        assert_eq!(
            cn.pickup_message_url(),
            "https://www.apple.com.cn/shop/fulfillment-messages"
        );
        // 中国大陆用独立域名，不能是 apple.com/cn —— 那正是上游拼错的地方。
        assert!(!cn.base_url.contains("apple.com/cn"));
        assert!(region_by_locale("de_DE").is_none());

        for r in REGIONS {
            assert!(
                r.base_url.starts_with("https://"),
                "{} 的站点前缀不对",
                r.locale
            );
            assert!(
                !r.base_url.ends_with('/'),
                "{} 的站点前缀不该带结尾斜杠",
                r.locale
            );
            assert!(!r.families.is_empty(), "{} 没有可抓取的购买页", r.locale);
        }
    }

    #[test]
    fn 目标键能区分同店不同型号() {
        let mk = |part: &str| Target {
            locale: "zh_CN".into(),
            store_number: "R683".into(),
            store_title: "上海-环球港".into(),
            part_number: part.into(),
            product_name: "x".into(),
        };
        assert_ne!(mk("MG724CH/A").key(), mk("MG0A4CH/A").key());
        assert_eq!(mk("MG724CH/A").key(), mk("MG724CH/A").key());
    }
}
