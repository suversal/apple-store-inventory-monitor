//! 从 Apple 官网购买页在线抓取商品目录。
//!
//! Go 版之前（以及上游）的做法是把 `productSelectionData` 手工从浏览器开发者
//! 工具里复制成 `products_<locale>.json` 提交进仓库，于是每次 Apple 发新机都得
//! 等作者手动更新并发版；上游作者停更之后，那份目录就永远停在了旧机型上。
//! 这里改成直接从购买页现抓，内嵌 JSON 只作为离线兜底。
//!
//! 分工：本模块只负责「把商品数据从购买页里抠出来」，缓存与离线兜底在
//! [`crate::catalog`]，库存查询在 [`crate::apple`]。
//!
//! 没有直接复用 `apple.rs` 的取页函数，是因为那边的限速与重试都是私有的，
//! 而且请求头是按 JSON 接口配的（`Accept: application/json`），拿来抓 HTML
//! 页面并不合适。这里按最小需要另写一份，但**共用调用方传进来的
//! `reqwest::Client`**，连接池仍然是同一个 —— 上游 `services/listen.go:221`
//! 每次请求都 `gorequest.New()` 造一个新客户端，连接池完全无法复用，空闲连接
//! 持续堆积，配合它 500ms 一轮的轮询，几小时就能涨到十几 GB 内存。

use std::collections::{BTreeMap, HashSet};
use std::time::Duration;

use serde::Deserialize;

use crate::apple::ApiError;
use crate::catalog::CatalogError;
use crate::model::{Category, Family, Product, Region};

/// 购买页里承载商品数据的全局变量名。
///
/// 页面中的原文形如：
///
/// ```text
/// window.PRODUCT_SELECTION_BOOTSTRAP = {
///         productSelectionData: {...}
/// ```
///
/// 先定位到这个变量，是为了避开页面里别处的同名文本；万一 Apple 改了变量名，
/// 找不到就退化成全页搜索键名，还能多撑一阵子。
const BOOTSTRAP_MARKER: &[u8] = b"PRODUCT_SELECTION_BOOTSTRAP";

/// 商品数据的键名。
///
/// 注意它是**不带引号的 JS 标识符**，整段不是 JSON，不能直接反序列化，只能
/// 定位到冒号后的第一个 `{` 再做花括号配对，把那个「本身是合法 JSON」的子对象
/// 截出来。不过「不带引号」只是当前打包器的选择，不是契约，带引号的写法也要认。
const SELECTION_KEY_STR: &str = "productSelectionData";
const SELECTION_KEY: &[u8] = SELECTION_KEY_STR.as_bytes();

/// 购买页 HTML 的读取上限。购买页本身在 2 MB 量级，留一倍余量。
///
/// 设上限的目的和 `apple.rs` 一样：万一被换成了别的巨大页面（比如拦截页），
/// 不至于把它整个读进内存。
const MAX_PAGE_BYTES: usize = 8 << 20;

/// 单次取页的超时。
///
/// 挂在请求上而不是客户端上：客户端是调用方给的，这里没有资格改它的全局配置，
/// 但也不能因为对方忘了设超时就把刷新任务永远挂住。
const PAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// 单次抓取内部的最大重试次数（不含首次请求）。
const MAX_RETRIES: u32 = 2;

/// 与 `apple.rs` 保持一致的 UA。
///
/// 那边的常量是私有的，引用不到，只能抄一份。上游写死的是 Chrome/94（2021 年），
/// 这种年代久远的 UA 本身就是明显的机器人特征。
const PAGE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
     AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36";

/// 抓取 `region` 站点上某个购买页的全部可购买配置。
///
/// `family` 取值来自 [`Region::families`]，它同时决定了取哪个地址、以及解析出来
/// 的商品算哪个品类。
///
/// `http` 必须是调用方长期持有的那一个客户端，绝不能在这里现造 —— 见模块文档。
pub async fn fetch_products(
    http: &reqwest::Client,
    region: &Region,
    family: &Family<'_>,
) -> Result<Vec<Product>, CatalogError> {
    if family.slug.trim().is_empty() {
        return Err(CatalogError::Fetch(ApiError::Transport(
            "机型 slug 为空".into(),
        )));
    }

    let page = fetch_page(http, region, family).await?;
    let products = parse_buy_page(&page, family.category, family.slug)?;
    if products.is_empty() {
        // 页面拿到了、JSON 也截出来了，却一个商品都没有，说明字段名变了。
        // 这必须报错：静默返回空列表会让上层用一份空目录覆盖掉可用的兜底数据。
        return Err(CatalogError::PageSchema {
            detail: format!("{} 购买页没有解析出任何商品", family.slug),
        });
    }
    Ok(products)
}

/// 从所选地区的品类入口发现购买页，不依赖预先写死的代际名称。
pub async fn discover_families(
    http: &reqwest::Client,
    region: &Region,
    category: Category,
) -> Result<Vec<String>, CatalogError> {
    let url = format!("{}/shop/{}", region.base_url, category.buy_path());
    let page =
        crate::apple::with_retry(MAX_RETRIES, || fetch_page_once(http, &url, region)).await?;
    parse_family_links(&page, region, category)
}

/// 只接受本地区同一品类的购买链接，将具体 SKU 链接归并到机型页。
pub fn parse_family_links(
    page: &[u8],
    region: &Region,
    category: Category,
) -> Result<Vec<String>, CatalogError> {
    let index = reqwest::Url::parse(&format!("{}/shop/{}", region.base_url, category.buy_path()))
        .map_err(|e| CatalogError::PageSchema {
        detail: e.to_string(),
    })?;
    let prefix = format!("{}/", index.path());
    let html = String::from_utf8_lossy(page);
    let document = dom_query::Document::from(html.as_ref());
    let mut slugs = std::collections::BTreeSet::new();
    for link in document.select("a[href]").iter() {
        let Some(href) = link.attr("href") else {
            continue;
        };
        let Ok(url) = index.join(&href) else { continue };
        if url.origin() != index.origin() || !url.username().is_empty() || url.password().is_some()
        {
            continue;
        }
        let Some(slug) = url.path().strip_prefix(&prefix) else {
            continue;
        };
        // Watch Ultra 的入口可能直接指向某个表壳或 SKU；只取机型段。
        let slug = slug.split('/').next().unwrap_or_default();
        let is_product_family = match category {
            Category::Iphone => slug.starts_with("iphone"),
            Category::Ipad => slug.starts_with("ipad"),
            Category::Watch => slug.starts_with("apple-watch"),
            Category::Mac => {
                slug.starts_with("mac") || slug == "imac" || slug.starts_with("studio-display")
            }
        };
        if is_product_family
            && !slug.is_empty()
            && slug.len() <= 80
            && slug
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            slugs.insert(slug.to_string());
        }
    }
    if slugs.is_empty() {
        return Err(CatalogError::PageSchema {
            detail: format!(
                "{} 品类入口没有发现机型购买页，保留现有目录",
                category.title()
            ),
        });
    }
    Ok(slugs.into_iter().collect())
}

/// 从一段购买页 HTML 里解析出商品列表。
///
/// `category` 与 `slug` 说明这页 HTML 是从哪来的。数据里不带这两样东西 ——
/// Mac 与 Apple Watch 的商品数据里连机型名都没有 —— 只能由调用方交代。
///
/// 单独暴露出来是为了能用本地 HTML 字符串完整测掉解析链路，不必请求真实网络。
pub fn parse_buy_page(
    page: &[u8],
    category: Category,
    slug: &str,
) -> Result<Vec<Product>, CatalogError> {
    let raw = extract_product_selection_data(page)?;
    let mut data: ProductSelection =
        serde_json::from_slice(raw).map_err(|e| CatalogError::PageSchema {
            detail: format!("{SELECTION_KEY_STR} 结构与预期不符：{e}"),
        })?;
    if category == Category::Mac {
        let html = String::from_utf8_lossy(page);
        let document = dom_query::Document::from(html.as_ref());
        data.mac_links = document
            .select("a[href]")
            .iter()
            .filter_map(|link| link.attr("href").map(|s| s.to_string()))
            .filter(|url| url.contains(&format!("/shop/buy-mac/{slug}/")))
            .collect();
    }
    Ok(data.to_products(category, slug))
}

/// 取一个 HTML 页面，失败按 [`ApiError`] 的口径分类。
///
/// 不复用 `apple.rs` 的 `get`：那里写死了 `Accept: application/json` 和
/// `X-Requested-With: XMLHttpRequest`，那是查询接口的请求特征，拿它去取一个
/// 普通网页反而更像脚本。底层客户端和 UA 仍然共用同一份。
async fn fetch_page(
    http: &reqwest::Client,
    region: &Region,
    family: &Family<'_>,
) -> Result<Vec<u8>, ApiError> {
    let url = region.buy_page_url(family);
    crate::apple::with_retry(MAX_RETRIES, || fetch_page_once(http, &url, region)).await
}

async fn fetch_page_once(
    http: &reqwest::Client,
    url: &str,
    region: &Region,
) -> Result<Vec<u8>, ApiError> {
    let resp = http
        .get(url)
        .timeout(PAGE_TIMEOUT)
        .header(reqwest::header::USER_AGENT, PAGE_USER_AGENT)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, region.accept_language())
        .header(reqwest::header::REFERER, format!("{}/", region.base_url))
        // 刻意不设置 Accept-Encoding：交给 reqwest 的 gzip 特性自动协商并透明
        // 解压，手动指定反而会拿到一坨没人解压的压缩字节。
        .send()
        .await
        .map_err(|e| ApiError::Transport(e.to_string()))?;

    let status = resp.status().as_u16();
    let body = crate::apple::read_body_capped(resp, MAX_PAGE_BYTES).await?;

    if let Some(err) = crate::apple::classify_status(status) {
        return Err(err);
    }
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiError::Blocked("HTTP 200 但响应体为空".into()));
    }
    // 这里不校验内容是不是真的商品页：拦截页同样是 HTML，靠 Content-Type 分不出来。
    // 真假交给后面的解析步骤判定。
    Ok(body)
}

/// 从购买页 HTML 中截出 `productSelectionData` 的值。
///
/// 返回的是页面里的原始切片，本身保证是合法 JSON 对象。
pub fn extract_product_selection_data(page: &[u8]) -> Result<&[u8], CatalogError> {
    let (search, offset) = match find(page, BOOTSTRAP_MARKER) {
        Some(i) => (&page[i..], i),
        None => (page, 0),
    };

    // 必须把所有同名位置都试一遍，而不是只认第一个。Go 版为此返工过一次：
    // 页面里只要先出现一段含有 productSelectionData 文本的字符串字面量
    // （埋点参数、提示文案里出现这种字面量再正常不过），定位就停在那里，
    // 发现后面不是冒号便直接判整页失败，真正的属性再也没机会被看到。表现是
    // 购买页明明带着完整数据，程序却一口咬定「结构不符」，用户对着空目录发呆。
    let mut last_err = None;
    let mut from = 0usize;

    while from < search.len() {
        let Some(rel) = find(&search[from..], SELECTION_KEY) else {
            break;
        };
        let at = from + rel;
        from = at + SELECTION_KEY.len();

        match selection_value_at(search, at, offset) {
            Ok(raw) => return Ok(raw),
            // 单个候选不成立只说明这里不是那个属性，接着找下一个。
            Err(err) => last_err = Some(err),
        }
    }

    // 全部候选都不成立时，报最后一个候选的具体原因；一个候选都没有才是「找不到」。
    Err(last_err.unwrap_or_else(|| CatalogError::PageSchema {
        detail: format!("页面里找不到 {SELECTION_KEY_STR}"),
    }))
}

/// 判断 `search[at..]` 处的键名是不是一个真的属性名，是则把它的值原样截出来。
///
/// `offset` 只用于把出错位置换算回整页偏移，方便排查。
fn selection_value_at(search: &[u8], at: usize, offset: usize) -> Result<&[u8], CatalogError> {
    let not_an_identifier = || CatalogError::PageSchema {
        detail: format!(
            "偏移 {} 处的 {SELECTION_KEY_STR} 只是更长标识符的一部分",
            offset + at
        ),
    };

    // 前后都得是标识符边界。否则 legacy_productSelectionData、
    // productSelectionDataV2 这类把目标名包在里面的更长标识符也会算命中，
    // 进而把一份旧数据当成商品目录 —— 那比报错还糟：界面上会摆出一批早已下架
    // 的型号，用户守着永远不会有货的零件号。
    if at > 0 && search.get(at - 1).copied().is_some_and(is_ident_byte) {
        return Err(not_an_identifier());
    }
    let mut pos = at + SELECTION_KEY.len();
    if search.get(pos).copied().is_some_and(is_ident_byte) {
        return Err(not_an_identifier());
    }

    pos = skip_space(search, pos);
    // 键带不带引号都要认：JS 对象字面量里 productSelectionData: 和
    // "productSelectionData": 一样合法。只认不带引号那种的话，Apple 哪天顺手
    // 加上引号（比如换个打包器），整页数据就会栽在「之后不是冒号」上。
    if matches!(search.get(pos), Some(b'"' | b'\'')) {
        pos = skip_space(search, pos + 1);
    }
    if search.get(pos) != Some(&b':') {
        return Err(CatalogError::PageSchema {
            detail: format!("偏移 {} 处的 {SELECTION_KEY_STR} 之后不是冒号", offset + at),
        });
    }
    pos = skip_space(search, pos + 1);
    if search.get(pos) != Some(&b'{') {
        return Err(CatalogError::PageSchema {
            detail: format!("偏移 {} 处的 {SELECTION_KEY_STR} 的值不是对象", offset + at),
        });
    }

    let end = match_object(search, pos).map_err(|detail| CatalogError::PageSchema {
        detail: format!(
            "截取 {SELECTION_KEY_STR} 失败（起始偏移 {}）：{detail}",
            offset + pos
        ),
    })?;
    let raw = search.get(pos..end).unwrap_or_default();

    // 花括号配平不等于内容合法。补这一道校验，才能把「命中的其实是某段文案里的
    // 同名文本」这类假阳性挡在门外 —— 挡掉之后循环还能继续去找真正的属性，
    // 而不是抱着一段垃圾往下走，最后以「一个商品都解析不出来」收场。
    serde_json::from_slice::<serde::de::IgnoredAny>(raw).map_err(|e| CatalogError::PageSchema {
        detail: format!("{SELECTION_KEY_STR} 的值不是合法 JSON：{e}"),
    })?;

    Ok(raw)
}

/// 从 `start` 处的 `{` 开始做花括号配对，返回配对的 `}` 之后一位的下标。
///
/// 必须跳过字符串字面量，否则商品数据里那些含有 `{` 的 HTML 片段（颜色的
/// `image` 字段就是整段 HTML）会把配对算错。按字节扫描对 UTF-8 是安全的：
/// 多字节序列的每个字节最高位都是 1，永远不会等于这里关心的 ASCII 定界符 ——
/// 这也是整个提取过程都在字节上做、而不是在 `&str` 上切片的原因，后者一旦切在
/// 字符中间就是 panic。
fn match_object(b: &[u8], start: usize) -> Result<usize, &'static str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut quote = 0u8;
    let mut escaped = false;

    for (i, &ch) in b.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == b'\\' {
                escaped = true;
            } else if ch == quote {
                in_string = false;
            }
            continue;
        }
        match ch {
            b'"' | b'\'' => {
                in_string = true;
                quote = ch;
            }
            b'{' => depth += 1,
            b'}' => {
                // 调用方保证 start 处是 `{`，走到这里 depth 至少为 1；
                // 仍然用 saturating_sub，免得将来有人换了调用方式就地下溢。
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Ok(i + 1);
                }
            }
            _ => {}
        }
    }
    Err("花括号未闭合")
}

/// 一个字节能否出现在 JS 标识符中间。
fn is_ident_byte(b: u8) -> bool {
    b == b'_' || b == b'$' || b.is_ascii_alphanumeric()
}

fn skip_space(b: &[u8], mut i: usize) -> usize {
    while matches!(b.get(i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
        i += 1;
    }
    i
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// 购买页商品数据中用得到的那部分字段。
///
/// 这份数据来自线上，Apple 随时可能改动，所以每个字段都写成「缺了也认」：
/// 少一个字段只该让那一条记录退化，不该让整页目录报废。内嵌快照的元素是同一种
/// 结构，因此 [`crate::catalog`] 直接复用它解析离线数据，没有理由维护两套。
///
/// # 一份变量名，两种排布
///
/// 四个品类的购买页共用同一个 `productSelectionData` 变量，里面的商品却是两种
/// 完全不同的排布：
///
/// * **平铺**（iPhone、iPad）：规格直接摆在商品对象上（`dimensionCapacity`、
///   `dimensionColor`……），零件号在 `partNumber`，本地化文案在 `displayValues`。
/// * **维度表**（Mac、Apple Watch）：规格收在 `dimensions` 子对象里，键名带着
///   分组前缀（`chassis-dimensionColor`、`watch_cases-dimensionCaseSize`），
///   零件号在 `btrOrFdPartNumber`（Mac）或 `part`（Watch）上，本地化文案可能挂在
///   `mainDisplayValues`（Mac）而不是 `displayValues`。
///
/// 这里两种排布都认，而不是按品类分派两套解析。品类是我们自己贴的标签，
/// Apple 不知道也不保证；照形状本身判断，哪天某一页换了排布也照样能解析出来，
/// 而不是整页归零。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProductSelection {
    /// 用 `Option` 而不是 `#[serde(default)]` 的 `Vec`：字段给成 JSON `null`
    /// 时后者会直接失败，而这两种情况该有同样的处置。
    #[serde(default)]
    products: Option<Vec<RawProduct>>,
    #[serde(default)]
    display_values: Option<DisplayGroups>,
    /// Mac 页把同一份文案挂在这个键下。
    #[serde(default)]
    main_display_values: Option<DisplayGroups>,
    /// Watch 的完整机型名位于无脚本商品入口，不能用会跨代复用的页面 slug 猜。
    #[serde(rename = "watchProductSelectionDataNoJS", default)]
    watch_examples: serde_json::Value,
    /// 当前页官方具体配置链接，保留在快照中供同一套解析器读取。
    #[serde(rename = "macProductLinks", default)]
    mac_links: Vec<String>,
}

/// 维度键 → 取值 → 展示条目。
///
/// 两层都用 `Value` 承接，因为这张表里混着非维度的条目：`prices` 是价格表、
/// `title` 是 `{"singleVariantDisplayTitle": ...}`、`variantOrder` 干脆是个数组。
/// 映射成具体结构体会让整个对象反序列化失败，所有颜色名就全丢了。
type DisplayGroups = BTreeMap<String, serde_json::Value>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawProduct {
    #[serde(default)]
    aos_container_part_number: Option<String>,
    /// 平铺形状里的零件号。
    #[serde(default)]
    part_number: Option<String>,
    /// Mac 页里预配置机型的零件号。
    #[serde(default)]
    btr_or_fd_part_number: Option<String>,
    /// Apple Watch 页里表壳的零件号。**只在维度表形状下才当零件号用**，
    /// 见 [`RawProduct::part_number`]。
    #[serde(default)]
    part: Option<String>,
    #[serde(default)]
    family_type: Option<String>,
    /// 维度表形状的规格。值用 `Value` 承接：万一某个取值不是字符串，
    /// 该退化的只有那一个维度，不该让整页反序列化失败。
    #[serde(default)]
    dimensions: Option<BTreeMap<String, serde_json::Value>>,
    /// 平铺形状的规格散落在对象顶层，只能整个兜住再挑。
    #[serde(flatten)]
    rest: BTreeMap<String, serde_json::Value>,
}

/// 商品的一项规格。
#[derive(Debug, PartialEq, Eq)]
struct Dimension<'a> {
    /// 页面里的原始键名，如 `chassis-dimensionColor`。查本地化文案要用它。
    key: &'a str,
    /// 去掉分组前缀之后的名字，如 `dimensionColor`。排序与识别用它。
    name: &'a str,
    /// 原始取值，如 `midnight`。
    value: &'a str,
}

impl<'a> Dimension<'a> {
    fn new(key: &'a str, value: &'a str) -> Self {
        // 维度表形状的键名带着分组前缀（chassis-、watch_cases-、processor-）。
        // 剥掉之后两种排布的维度名就统一了，下面的识别与排序只认这一个名字；
        // 查文案仍然得用原始键名，因为 displayValues 是按原始键名分组的。
        let name = match key.find("-dimension") {
            Some(at) => &key[at + 1..],
            None => key,
        };
        Self {
            key,
            name,
            value: value.trim(),
        }
    }
}

/// 各维度在展示名里的先后。
///
/// 顺序照抄 Apple 自己在购买页上的提问顺序：先定形状（尺寸、材质），再定性能
/// （芯片、核心数、容量），最后定外观（网络、面板、颜色）。数字之间留了空档，
/// 将来插入新维度不必重排。
fn dimension_rank(name: &str) -> u8 {
    match name {
        "dimensionScreensize" => 10,
        "dimensionCaseSize" => 20,
        "dimensionCaseMaterial" => 30,
        "dimensionChip" => 40,
        "dimensionMemory" => 55,
        "dimensionCapacity" => 60,
        "dimensionConnection" => 70,
        "dimensionFinish" => 80,
        "dimensionColor" => 90,
        // 认不出的维度落在芯片与容量之间。Mac 的核心数维度就在这里 —— 它的
        // 键名形如 `processor-dimensionChip-cpuCoreCount-gpuCoreCount`，
        // 紧跟着芯片正合适。
        _ => 50,
    }
}

impl RawProduct {
    /// 这条记录的零件号，取不到时返回 `None`。
    ///
    /// 三个候选字段按可信度排队：
    ///
    /// * `partNumber` —— 平铺形状里的正牌零件号。
    /// * `btrOrFdPartNumber` —— Mac 页里预配置机型的零件号。同一页还列着
    ///   `CONFIGURABLE` 的定制机，那个字段为 null；定制机本来就不能到店取货，
    ///   没有零件号正好把它们滤掉。
    /// * `part` —— Apple Watch 页里表壳的零件号。
    ///
    /// **`part` 要过两道关才认。** iPad 的平铺记录上也有一个 `part`，装的却是
    /// 产品线代号 `IPADPRO11_WI_2025`。照单全收会让整个 iPad 目录挂满查不到的
    /// 假零件号 —— 而那些查询失败会一路老实地变成「未知」，用户只会看到一屏
    /// 说不清的状态，还以为是 Apple 又拦了请求。两道关是：
    ///
    /// 1. 这条记录得真的是维度表形状，即 `dimensions` 里**至少有一个可用维度**。
    ///    光看 `dimensions` 字段在不在不够：页面哪天多给一个空的 `dimensions: {}`，
    ///    产品线代号就又混进来了。
    /// 2. 取值得长得像零件号。Apple 的零件号一律带一条斜杠（`MFCN4CH/B`），
    ///    产品线代号没有。
    fn part_number(&self) -> Option<&str> {
        [
            self.part_number.as_deref(),
            self.btr_or_fd_part_number.as_deref(),
            self.part
                .as_deref()
                .filter(|part| self.has_usable_dimension() && part.contains('/')),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|p| !p.is_empty())
    }

    /// `dimensions` 里有没有至少一个能用的维度。
    ///
    /// 判据要和 [`RawProduct::dimensions`] 挑出来的那批完全一致，否则
    /// `crates/apw-core/data/generate.py` 裁剪出来的离线快照会和在线抓取
    /// 产生不同的结果 —— 同一台机器在线能选、离线选不了，最难查的那种不一致。
    fn has_usable_dimension(&self) -> bool {
        self.dimensions.as_ref().is_some_and(|dims| {
            dims.values()
                .any(|v| v.as_str().is_some_and(|s| !s.trim().is_empty()))
        })
    }

    /// 这条记录的全部规格，已按展示顺序排好。
    fn dimensions(&self) -> Vec<Dimension<'_>> {
        let mut dims: Vec<Dimension<'_>> = match &self.dimensions {
            Some(map) => map
                .iter()
                .filter_map(|(key, value)| Some(Dimension::new(key, value.as_str()?)))
                .collect(),
            None => self
                .rest
                .iter()
                // `dimensionSteporder` 是页面上的排列序号，不是规格。它在数据里
                // 是数字，`as_str` 本来就会把它挡掉，但那是巧合不是契约 ——
                // 换个地区站点给成字符串，展示名里就会平白多出一个「320」。
                .filter(|(key, _)| key.starts_with("dimension") && *key != "dimensionSteporder")
                .filter_map(|(key, value)| Some(Dimension::new(key, value.as_str()?)))
                .collect(),
        };
        dims.retain(|d| !d.value.is_empty());
        dims.sort_by_key(|d| (dimension_rank(d.name), d.key));
        dims
    }
}

impl ProductSelection {
    /// 把一段商品数据转成商品列表，按零件号去重。
    ///
    /// `category` 与 `slug` 是这一页的来历：数据本身不带品类，Mac 与 Apple Watch
    /// 的数据里连机型名都没有，只能由调用方告诉它这是从哪一页抓来的。
    pub(crate) fn to_products(&self, category: Category, slug: &str) -> Vec<Product> {
        let raw = self.products.as_deref().unwrap_or_default();

        let mut products = Vec::with_capacity(raw.len());
        let mut seen = HashSet::with_capacity(raw.len());
        let watch_model = (category == Category::Watch)
            .then(|| self.watch_model_name())
            .flatten();

        for item in raw {
            let Some(part_number) = item.part_number() else {
                continue;
            };
            // 同一零件号会重复出现：日本站把同一台机器按运营商
            // （SOFTBANK_IPHONE17）和无锁版（UNLOCKED_JP）各列了一遍，零件号
            // 完全相同；Mac 页上同一台机器也会既作为预配置机型、又作为定制机的
            // 起点各出现一次。查库存只认零件号，留一条就够，否则界面上会出现
            // 一模一样的两行，用户不知道该点哪个。
            if !seen.insert(part_number) {
                continue;
            }

            let family_type = trimmed(&item.family_type);
            // 展示名优先按 familyType 拼：同一页里的 Pro 与 Pro Max 只有它能分开。
            // Mac 与 Apple Watch 的数据里没有这个字段，退回购买页 slug。
            let decoded_family = family_display_name(family_type);
            let display_family = watch_model
                .clone()
                .or_else(|| decoded_family.clone())
                .unwrap_or_else(|| slug_display_name(slug));

            let mut labels: Vec<String> = Vec::new();
            let mut capacity = String::new();
            let mut color = String::new();

            let mac_hints = if category == Category::Mac {
                self.mac_dimension_hints(item, slug)
            } else {
                BTreeMap::new()
            };
            let mut dimensions = item.dimensions();
            for (key, value) in &mac_hints {
                if !dimensions.iter().any(|d| d.name == *key) {
                    dimensions.push(Dimension::new(key, value));
                }
            }
            dimensions.sort_by_key(|d| (dimension_rank(d.name), d.key));
            if category == Category::Watch {
                for name in ["dimensionCaseMaterial", "dimensionConnection"] {
                    if !dimensions.iter().any(|d| d.name == name)
                        && let Some(value) = self.watch_fixed_dimension(name)
                    {
                        dimensions.push(Dimension::new(name, value));
                    }
                }
                dimensions.sort_by_key(|d| (dimension_rank(d.name), d.key));
            }
            for dim in &dimensions {
                // familyType 拼出来的名字里已经带着屏幕尺寸（iPhone 17 Pro Max、
                // iPad Pro 11），再拼一遍就成了「iPad Pro 11 11 英寸机型」。
                // 退回 slug 的那些品类没这个问题，尺寸必须从维度里补。
                if dim.name == "dimensionScreensize" && decoded_family.is_some() {
                    continue;
                }

                let label = if dim.name == "dimensionCapacity" {
                    // 容量用原始取值规范化，不取本地化文案：后者是
                    // 「512GB 存储容量」这种整句，拼进展示名太长。
                    capacity = normalize_capacity(dim.value);
                    if category == Category::Mac {
                        format!("{capacity} 存储")
                    } else {
                        capacity.clone()
                    }
                } else {
                    let Some(text) = (category == Category::Mac)
                        .then(|| mac_dimension_label(dim.name, dim.value))
                        .flatten()
                        .or_else(|| self.display_name(dim.key, dim.value))
                        .or_else(|| {
                            (category == Category::Watch)
                                .then(|| watch_dimension_fallback(dim.name, dim.value))
                                .flatten()
                        })
                        .or_else(|| {
                            // 取不到本地化文案时，只有取值本身还认得出来才拿它顶替。
                            // 颜色是 `cosmicorange` 这种词，留着比留空强 —— 留空会让
                            // 同机型同容量的几个颜色在界面上长得一模一样。
                            //
                            // 带数字的取值就不行了：`m5-10-10`、`6-5` 是芯片与核心数
                            // 的机器标识，摆进展示名只会让人更糊涂。这条路径不是假设
                            // 出来的 —— Apple 只给「可升级」的那几档配了文案，基础
                            // 配置那一档在 displayValues 里压根没有条目。
                            //
                            // 整个略去之后万一因此重名，下面的 disambiguate_titles
                            // 会补上零件号，不会出现两个一字不差的选项。
                            (!dim.value.contains(|c: char| c.is_ascii_digit()))
                                .then(|| dim.value.to_string())
                        })
                    else {
                        continue;
                    };
                    if dim.name == "dimensionColor" {
                        color = text.clone();
                    }
                    text
                };

                if !label.is_empty() {
                    labels.push(label);
                }
            }

            let mut parts = Vec::with_capacity(labels.len() + 1);
            parts.push(display_family);
            parts.extend(labels);

            products.push(Product {
                title: join_non_empty(&parts),
                part_number: part_number.to_string(),
                category,
                // family 只用来分组和排序。iPhone / iPad 用 familyType，
                // Mac / Apple Watch 没有这个字段，退回 slug。
                family: if family_type.is_empty() {
                    slug.to_string()
                } else {
                    family_type.to_string()
                },
                capacity,
                color,
            });
        }

        disambiguate_titles(&mut products);
        products
    }

    fn mac_dimension_hints(&self, item: &RawProduct, slug: &str) -> BTreeMap<&'static str, String> {
        let mut hints = BTreeMap::new();
        let candidates: Vec<_> = self
            .mac_links
            .iter()
            .filter_map(|url| {
                let tail = url
                    .split(&format!("/shop/buy-mac/{slug}/"))
                    .nth(1)?
                    .trim()
                    .split(['?', '#', '/'])
                    .next()?;
                let fields = mac_link_fields(tail);
                let dims = item.dimensions();
                let matches = dims.iter().all(|d| match d.name {
                    "dimensionScreensize" => fields
                        .get("dimensionScreensize")
                        .is_some_and(|v| v == d.value),
                    "dimensionChip" => fields.get("dimensionChip").is_some_and(|v| v == d.value),
                    "dimensionCapacity" => fields
                        .get("dimensionCapacity")
                        .is_some_and(|v| v == d.value),
                    "dimensionColor" => {
                        let normalize = |s: &str| s.replace(['-', '_'], "").replace("grey", "gray");
                        normalize(tail).contains(&normalize(d.value))
                    }
                    "dimensionFinish" => match d.value {
                        "nano_texture" | "matte" => tail.contains("nano-texture"),
                        "standard" | "glossy" => {
                            tail.contains("standard") || !tail.contains("nano-texture")
                        }
                        _ => false,
                    },
                    name if name.contains("cpuCoreCount-gpuCoreCount") => {
                        let numbers: Vec<_> = d.value.split('-').collect();
                        numbers.len() >= 2
                            && fields
                                .get("cpu")
                                .is_some_and(|v| v == numbers[numbers.len() - 2])
                            && fields
                                .get("gpu")
                                .is_some_and(|v| v == numbers[numbers.len() - 1])
                    }
                    _ => true,
                });
                matches.then_some(fields)
            })
            .collect();
        for key in [
            "dimensionChip",
            "dimensionScreensize",
            "dimensionMemory",
            "dimensionCapacity",
        ] {
            if let Some(first) = candidates.first().and_then(|c| c.get(key))
                && candidates.iter().all(|c| c.get(key) == Some(first))
            {
                hints.insert(key, first.clone());
            }
        }
        // 型号容器明确写出的单一芯片也可用；包含多个芯片时绝不选其中一个。
        if !hints.contains_key("dimensionChip") {
            let chips: HashSet<_> = item
                .aos_container_part_number
                .as_deref()
                .unwrap_or_default()
                .split('_')
                .filter_map(|token| normalize_chip(token).map(|_| token.to_ascii_lowercase()))
                .collect();
            if chips.len() == 1 {
                hints.insert("dimensionChip", chips.into_iter().next().unwrap());
            }
        }
        hints
    }

    /// 只接受这页所有商品示例一致的机型名；混合代数或缺少代数时不猜。
    fn watch_model_name(&self) -> Option<String> {
        let examples = self.watch_examples.as_array()?;
        let mut model: Option<String> = None;
        for example in examples {
            let text = plain_text(example.get("text")?.as_str()?);
            let name = text.split(['(', '（']).next()?.split(" GPS").next()?.trim();
            if !name.starts_with("Apple Watch ") || !name.ends_with(|c: char| c.is_ascii_digit()) {
                return None;
            }
            if model.as_deref().is_some_and(|previous| previous != name) {
                return None;
            }
            model = Some(name.to_string());
        }
        model
    }

    /// Ultra 等页面省略了固定维度，只有全部官方示例 URL 一致时才补回。
    fn watch_fixed_dimension(&self, name: &str) -> Option<&'static str> {
        let examples = self.watch_examples.as_array()?;
        let candidates: &[(&str, &str)] = match name {
            "dimensionCaseMaterial" => &[
                ("-titanium-", "titanium"),
                ("-aluminium-", "aluminum"),
                ("-aluminum-", "aluminum"),
                ("-ceramic-", "ceramic"),
            ],
            "dimensionConnection" => &[("-cellular-", "gpscell"), ("-gps-", "gps")],
            _ => return None,
        };
        if examples.is_empty() {
            return None;
        }
        candidates.iter().find_map(|(segment, value)| {
            examples
                .iter()
                .all(|example| {
                    example
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|url| {
                            url.rsplit('/').next().unwrap_or_default().contains(segment)
                        })
                })
                .then_some(*value)
        })
    }

    /// 查某个维度取值的本地化展示名。
    fn display_name(&self, dimension: &str, value: &str) -> Option<String> {
        let groups = [
            self.display_values.as_ref(),
            self.main_display_values.as_ref(),
        ];

        for group in groups.into_iter().flatten() {
            let Some(entry) = group.get(dimension).and_then(|d| d.get(value)) else {
                continue;
            };
            // 同一件事，三种页面用了三个字段名：iPhone / iPad 用 `value`，
            // Mac 用 `header`，Apple Watch 的颜色用 `text`。全试一遍，谁先给出
            // 非空文案就用谁 —— 挨个写死品类对应哪个字段，Apple 改一次就废一次。
            for field in ["value", "header", "text"] {
                let Some(raw) = entry.get(field).and_then(serde_json::Value::as_str) else {
                    continue;
                };
                let text = plain_text(raw);
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
        None
    }
}

fn normalize_chip(raw: &str) -> Option<String> {
    let value = raw.to_ascii_lowercase();
    let mut chars = value.chars();
    let series = chars.next()?;
    if series != 'm' && series != 'a' {
        return None;
    }
    let digits: String = chars.clone().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    let suffix = &value[1 + digits.len()..];
    let suffix = match suffix {
        "" => "",
        "pro" => " Pro",
        "max" => " Max",
        "ultra" => " Ultra",
        _ => return None,
    };
    Some(format!("{}{digits}{suffix}", series.to_ascii_uppercase()))
}

fn mac_link_fields(tail: &str) -> BTreeMap<&'static str, String> {
    let tokens: Vec<_> = tail.split('-').collect();
    let mut fields = BTreeMap::new();
    for (i, token) in tokens.iter().enumerate() {
        let next = tokens.get(i + 1).copied().unwrap_or_default();
        if normalize_chip(token).is_some() {
            let suffix = if matches!(next, "pro" | "max" | "ultra") {
                next
            } else {
                ""
            };
            fields.insert("dimensionChip", format!("{token}{suffix}"));
        }
        if token.chars().all(|c| c.is_ascii_digit()) && !token.is_empty() && next == "inch" {
            fields.insert("dimensionScreensize", format!("{token}inch"));
        }
        if (token.ends_with("gb") || token.ends_with("tb")) && matches!(next, "memory" | "storage")
        {
            fields.insert(
                if next == "memory" {
                    "dimensionMemory"
                } else {
                    "dimensionCapacity"
                },
                token.to_string(),
            );
        }
        if next == "core"
            && let Some(kind @ ("cpu" | "gpu")) = tokens.get(i + 2).copied()
        {
            fields.insert(if kind == "cpu" { "cpu" } else { "gpu" }, token.to_string());
        }
    }
    fields
}

fn mac_dimension_label(name: &str, value: &str) -> Option<String> {
    match name {
        "dimensionChip" => normalize_chip(value),
        "dimensionMemory" => Some(format!("{} 内存", normalize_capacity(value))),
        "dimensionScreensize" => value
            .strip_suffix("inch")
            .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit() || c == '.'))
            .map(|size| format!("{size} 英寸")),
        name if name.contains("cpuCoreCount-gpuCoreCount") => {
            let values: Vec<_> = value.split('-').collect();
            if values.len() < 2 {
                return None;
            }
            let (cpu, gpu) = (values[values.len() - 2], values[values.len() - 1]);
            (cpu.chars().all(|c| c.is_ascii_digit()) && gpu.chars().all(|c| c.is_ascii_digit()))
                .then(|| format!("{cpu} 核 CPU / {gpu} 核 GPU"))
        }
        _ => None,
    }
}

/// 已知 Watch 规格的展示回退；保留 49mm 等真实尺寸，不猜缺失规格。
fn watch_dimension_fallback(name: &str, value: &str) -> Option<String> {
    if name == "dimensionCaseSize"
        && let Some(size) = value.strip_suffix("mm")
        && !size.is_empty()
        && size.chars().all(|c| c.is_ascii_digit())
    {
        return Some(format!("{size} 毫米"));
    }
    match (name, value) {
        ("dimensionCaseMaterial", "aluminum" | "aluminium") => Some("铝金属".into()),
        ("dimensionCaseMaterial", "titanium") => Some("钛金属".into()),
        ("dimensionCaseMaterial", "ceramic") => Some("陶瓷".into()),
        ("dimensionConnection", "gpscell") => Some("GPS + 蜂窝网络".into()),
        ("dimensionConnection", "gps") => Some("GPS".into()),
        _ => None,
    }
}

/// 同一页里出现重名时，给重名的那几条都补上零件号。
///
/// 重名是真会发生的：MacBook Pro 页上有两台 16 英寸深空黑色 M5 Max，差别只在
/// 图形处理器核心数一个维度上；那个维度的文案万一取不到（Apple 改了字段名就会
/// 发生），两条就长得一字不差。让用户在两个完全相同的选项里挑一个，等于让他
/// 掷骰子决定监控哪个零件号 —— 而这种错误直到抢购当天都不会有任何迹象。
pub(crate) fn disambiguate_titles(products: &mut [Product]) {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for p in products.iter() {
        *counts.entry(p.title.as_str()).or_insert(0) += 1;
    }
    let duplicated: HashSet<String> = counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(title, _)| title.to_string())
        .collect();

    for p in products.iter_mut() {
        if duplicated.contains(&p.title) {
            p.title = join_non_empty(&[p.title.clone(), p.part_number.clone()]);
        }
    }
}

/// 把一个 `productSelectionData` 对象解析成商品列表。
pub fn parse_product_selection(
    raw: &[u8],
    category: Category,
    slug: &str,
) -> Result<Vec<Product>, CatalogError> {
    let data: ProductSelection =
        serde_json::from_slice(raw).map_err(|e| CatalogError::PageSchema {
            detail: format!("{SELECTION_KEY_STR} 结构与预期不符：{e}"),
        })?;
    Ok(data.to_products(category, slug))
}

fn trimmed(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or_default().trim()
}

fn join_non_empty(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 展示文案里只是给几个字换样式的行内标签，去掉标签本身即可。
///
/// 反过来说：**没列在这里的标签都当成分段边界**。宁可把一个没见过的行内标签
/// 误判成边界（最多丢掉后半句），也不能把块级标签当成行内 —— 那会把补充说明
/// 整段粘进展示名。
///
/// `span` 必须算行内。Apple 用它给一句话里的几段加样式：MacBook Pro 的核心数
/// 写作 `<span>18 核中央处理器</span>、<span>32 核图形处理器</span>`，把 span
/// 当边界就只剩「18 核中央处理器」—— 而两台 16 英寸 M5 Max 的差别正好只在
/// 被切掉的那半句上，界面上会出现两个一模一样的选项。
const INLINE_TAGS: &[&str] = &[
    "small", "b", "strong", "em", "i", "u", "s", "sub", "abbr", "mark", "nobr", "wbr", "span", "a",
    "font",
];

/// 带这些类名的标签是补充说明，不管标签名是什么都算分段边界。
///
/// 光看标签名不够：Apple 的补充说明大多挂在 `<div class="form-label-small">` 上，
/// 但 Apple Watch 的铝金属表壳用的是同一个类名的 `<span>`。而 span 又必须算
/// 行内（见上），只好再认一次类名。
const EXPLAINER_CLASSES: &[&str] = &["form-label-small", "as-subheading"];

/// 把购买页里的一段展示文案压成一行纯文本。
///
/// Apple 在这些字段里塞的是 HTML，而且几乎每一条都是「一句正文 + 一段补充说明」：
///
/// ```text
/// 钛金属<div class="form-label-small">提供 GPS + 蜂窝网络</div>
/// ```
///
/// 下拉框里只该出现「钛金属」。所以这里不是简单地删标签 —— 那样会得到
/// 「钛金属提供 GPS + 蜂窝网络」这种糊成一句的长串 —— 而是把块级标签和换行都
/// 当成分段边界，取**第一段非空文本**。
///
/// 取第一段非空、而不是第一段，是因为有的条目正文本身就包在块级标签里
/// （Apple Watch 的铝金属表壳是 `\n<div>铝金属</div>\n<span…>`），
/// 从第一个边界切一刀只会切出一个空串。
fn plain_text(html: &str) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut rest = html;
    // 一旦确认后面再没有 `>`，就把 `<` 从要找的字符里去掉，只继续找换行。
    // 不去掉的话，每个 `<` 都会为了找 `>` 把剩余部分重扫一遍，一串连续的
    // `<` 就是 O(n²)：界面进程会卡住不动，而原因只是文案里多了几个尖括号。
    let mut delimiters: &[char] = &['<', '\n', '\r'];

    while let Some(at) = rest.find(delimiters) {
        let (head, tail) = rest.split_at(at);
        current.push_str(head);

        if !tail.starts_with('<') {
            segments.push(std::mem::take(&mut current));
            rest = &tail[1..];
            continue;
        }

        match tail.find('>') {
            Some(end) => {
                if !is_inline_tag(&tail[1..end]) {
                    segments.push(std::mem::take(&mut current));
                }
                rest = &tail[end + 1..];
            }
            // 后面再没有 `>` 了，也就不可能再有标签。这个 `<` 当普通字符
            // 处理 —— 一个没闭合的尖括号不该让这条商品失去展示名 —— 但换行
            // 仍然是分段边界，不能连着后面的补充说明一起吞进来。
            None => {
                current.push('<');
                rest = &tail[1..];
                delimiters = &['\n', '\r'];
            }
        }
    }
    current.push_str(rest);
    segments.push(current);

    segments
        .into_iter()
        .map(|s| collapse_whitespace(&decode_entities(&s)))
        .find(|s| !s.is_empty())
        .unwrap_or_default()
}

/// 判断 `<` 与 `>` 之间的这段内容是不是一个行内标签。
fn is_inline_tag(raw: &str) -> bool {
    if EXPLAINER_CLASSES.iter().any(|class| raw.contains(class)) {
        return false;
    }
    let name = raw.trim().trim_start_matches('/').trim_start();
    let name = name
        .split(|c: char| c.is_whitespace() || c == '/')
        .next()
        .unwrap_or_default();
    INLINE_TAGS.iter().any(|t| t.eq_ignore_ascii_case(name))
}

/// 还原 HTML 实体。
///
/// 只认得几个常见的具名实体加数字实体就够了：这些字段里真正常见的是 `&nbsp;`
/// （Apple 用它把「10 核中央处理器」黏成不折行的一块）。认不出的实体原样保留，
/// 而不是丢掉 —— 展示名里多几个字符总好过少几个字。
fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at..];
        // 实体名再长也就十来个字符，所以只在开头这一小段窗口里找分号。
        //
        // 限窗口有两个理由：一个孤零零的 `&` 不该把远处某个分号当成自己的
        // 结尾、把中间的正文整段吃掉；以及，扫描范围必须是常数，否则一串
        // 连续的 `&` 会让每一轮都重扫剩余全文，退化成 O(n²)。
        //
        // 按字节找而不是按字符找：分号是 ASCII，命中的位置必然落在字符边界上，
        // 而按字节切片永远不会切在多字节字符中间（那是 panic）。
        let window = (ENTITY_MAX_BYTES + 2).min(after.len());
        match after.as_bytes()[..window].iter().position(|b| *b == b';') {
            Some(end) => {
                let entity = &after[1..end];
                match decode_entity(entity) {
                    Some(ch) => out.push(ch),
                    None => out.push_str(&after[..=end]),
                }
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// 实体名的字节数上限（不含两头的 `&` 与 `;`）。`&#x1F600;` 这种也才 7 个字节。
const ENTITY_MAX_BYTES: usize = 12;

fn decode_entity(entity: &str) -> Option<char> {
    match entity {
        // 不间断空格换成普通空格，好让后面的空白折叠正常起作用。
        "nbsp" => return Some(' '),
        "amp" => return Some('&'),
        "lt" => return Some('<'),
        "gt" => return Some('>'),
        "quot" => return Some('"'),
        "apos" => return Some('\''),
        _ => {}
    }

    let digits = entity.strip_prefix('#')?;
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    char::from_u32(code)
}

fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace()
        .map(|word| {
            word.chars()
                .filter(|c| !is_zero_width(*c))
                .collect::<String>()
        })
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 零宽字符。
///
/// Apple 用它们控制中文的换行位置（Studio Display 的「可调倾斜度及高度的支架」
/// 里就夹着两个 U+200D）。界面上看不见，但会跟着展示名进搜索框和通知里，
/// 让用户搜自己刚加的那一项反而搜不到。
fn is_zero_width(c: char) -> bool {
    matches!(c, '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}')
}

/// 把 Apple 的容量标识规范成展示形式，如 `512gb` → `512GB`。
pub fn normalize_capacity(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let lower = trimmed.to_ascii_lowercase();
    for unit in ["tb", "gb", "mb"] {
        if let Some(number) = lower.strip_suffix(unit) {
            return format!("{}{}", number.trim(), unit.to_ascii_uppercase());
        }
    }
    // 认不出单位就原样返回。拼一个自以为是的名字比留着原文更容易误导人。
    trimmed.to_string()
}

/// 把容量标识换算成可比较的数值（单位 GB），用于排序。
///
/// 直接按字符串排会把 `1TB` 排在 `256GB` 前面。
pub(crate) fn capacity_rank(capacity: &str) -> u32 {
    let lower = capacity.trim().to_ascii_lowercase();
    let (number, multiplier) = if let Some(n) = lower.strip_suffix("tb") {
        (n, 1024)
    } else if let Some(n) = lower.strip_suffix("gb") {
        (n, 1)
    } else {
        return 0;
    };
    number
        .trim()
        .parse::<u32>()
        .map(|n| n.saturating_mul(multiplier))
        .unwrap_or(0)
}

/// 机型标识里可识别的词，按顺序尝试匹配。
///
/// 拆成单词而不是穷举全部机型，是为了新机发布时不必改代码：`iphone17promax`
/// 会被拆成 pro + max，未来的 `iphone18pro`、`ipadair13` 同样成立。
const FAMILY_WORDS: &[(&str, &str)] = &[
    ("pro", "Pro"),
    ("max", "Max"),
    ("plus", "Plus"),
    ("mini", "mini"),
    ("air", "Air"),
    ("duo", "Duo"),
    ("se", "SE"),
    ("e", "e"),
];

/// 能被拆成人话的机型标识前缀。
const FAMILY_PREFIXES: &[(&str, &str)] = &[("iphone", "iPhone"), ("ipad", "iPad")];

/// 把 `familyType` 转成人类可读的机型名，如 `iphone17promax` → `iPhone 17 Pro Max`。
///
/// 认不出前缀就返回 `None`，让调用方退回购买页 slug。这里刻意不再像从前那样
/// 「认不出就把原文返回」：`ipadpro11_m5_2025` 这种原文摆进下拉框，和没有名字
/// 没什么区别，还会让人以为程序解析对了。
pub fn family_display_name(family_type: &str) -> Option<String> {
    let trimmed = family_type.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();
    // 先解析机型主体；iPad 后缀中的明确芯片随后补回，年份不当作规格。
    let head = lower.split('_').next().unwrap_or_default();

    let (prefix, display) = FAMILY_PREFIXES
        .iter()
        .find(|(prefix, _)| head.starts_with(prefix))?;
    let rest = head.strip_prefix(prefix).unwrap_or_default();

    // 逐字符处理而不是按字节切片：familyType 理论上可能带非 ASCII，按字节切
    // `&str` 一旦切在字符中间就是 panic，而库代码不允许有 panic。
    let chars: Vec<char> = rest.chars().collect();
    let mut tokens: Vec<String> = vec![(*display).to_string()];
    let mut i = 0usize;

    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            tokens.push(chars[start..i].iter().collect());
            continue;
        }

        if let Some((token, display)) = FAMILY_WORDS
            .iter()
            .find(|(token, _)| starts_with_at(&chars, i, token))
        {
            // 「16e」这类后缀紧贴数字，中间不加空格。
            let glue = *token == "e"
                && tokens
                    .last()
                    .and_then(|t| t.chars().next())
                    .is_some_and(|c| c.is_ascii_digit());
            if glue {
                if let Some(last) = tokens.last_mut() {
                    last.push_str(display);
                }
            } else {
                tokens.push((*display).to_string());
            }
            i += token.chars().count();
            continue;
        }

        // 遇到没登记的词，整段保留并首字母大写，至少不丢信息。
        let start = i;
        while i < chars.len() && !chars[i].is_ascii_digit() {
            i += 1;
        }
        let word: String = chars[start..i].iter().collect();
        // 紧跟其后的数字并进同一个词，不拆开：`ipada16` 里的 a16 是芯片型号，
        // 拆成「iPad A 16」就成了另一回事。登记过的词不这么合 —— iphonese3
        // 该是「iPhone SE 3」，那个 3 是代数。
        let mut token = capitalize(&word);
        while i < chars.len() && chars[i].is_ascii_digit() {
            token.push(chars[i]);
            i += 1;
        }
        tokens.push(token);
    }

    if *prefix == "ipad" {
        if tokens
            .last()
            .is_some_and(|t| t.chars().all(|c| c.is_ascii_digit()))
            && tokens.iter().any(|t| t == "Pro" || t == "Air")
        {
            tokens.push("英寸".into());
        }
        for token in lower.split('_').skip(1) {
            if let Some(chip) = normalize_chip(token) {
                tokens.push(format!("({chip})"));
            }
        }
    }
    Some(tokens.join(" "))
}

/// slug 里那些有固定写法的词。没登记的词首字母大写了事。
const SLUG_WORDS: &[(&str, &str)] = &[
    ("iphone", "iPhone"),
    ("ipad", "iPad"),
    ("imac", "iMac"),
    ("macbook", "MacBook"),
    ("mac", "Mac"),
    ("apple", "Apple"),
    ("watch", "Watch"),
    ("air", "Air"),
    ("duo", "Duo"),
    ("pro", "Pro"),
    ("max", "Max"),
    // Apple 自己就写小写的 mini（Mac mini、iPad mini）。
    ("mini", "mini"),
    ("se", "SE"),
    ("ultra", "Ultra"),
    ("studio", "Studio"),
    ("xdr", "XDR"),
    ("hermes", "Hermès"),
];

/// 把购买页 slug 拼成机型名，如 `macbook-air` → `MacBook Air`。
///
/// Mac 与 Apple Watch 的商品数据里没有任何机型字段 —— 页面上那句「MacBook Air」
/// 是排版文案，不在 `productSelectionData` 里。slug 是这两个品类唯一稳定的
/// 机型来源。
pub fn slug_display_name(slug: &str) -> String {
    slug.split('-')
        .filter(|word| !word.is_empty())
        .map(|word| {
            let lower = word.to_lowercase();
            SLUG_WORDS
                .iter()
                .find(|(raw, _)| *raw == lower)
                .map(|(_, display)| (*display).to_string())
                .unwrap_or_else(|| capitalize(&lower))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn starts_with_at(chars: &[char], i: usize, word: &str) -> bool {
    let len = word.chars().count();
    chars
        .get(i..i + len)
        .is_some_and(|slice| slice.iter().copied().eq(word.chars()))
}

fn capitalize(word: &str) -> String {
    let mut it = word.chars();
    match it.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + it.as_str(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 容量被规范成展示形式() {
        assert_eq!(normalize_capacity("512gb"), "512GB");
        assert_eq!(normalize_capacity(" 1tb "), "1TB");
        assert_eq!(normalize_capacity("256GB"), "256GB");
        assert_eq!(normalize_capacity(""), "");
        // 认不出的单位原样保留，不瞎猜。
        assert_eq!(normalize_capacity("大杯"), "大杯");
    }

    #[test]
    fn 容量按真实大小排序而不是按字符串() {
        assert!(capacity_rank("256GB") < capacity_rank("512GB"));
        // 按字符串排会把 1TB 排在 256GB 前面。
        assert!(capacity_rank("512GB") < capacity_rank("1TB"));
        assert!(capacity_rank("1TB") < capacity_rank("2TB"));
        assert_eq!(capacity_rank("看不懂"), 0);
    }

    #[test]
    fn 机型标识被拆成可读的名字() {
        for (raw, want) in [
            ("iphone17", "iPhone 17"),
            ("iphone17pro", "iPhone 17 Pro"),
            ("iphone17promax", "iPhone 17 Pro Max"),
            ("iphoneair", "iPhone Air"),
            ("iphone16e", "iPhone 16e"),
            ("iphone16plus", "iPhone 16 Plus"),
            ("iphone13mini", "iPhone 13 mini"),
            ("iphonese3", "iPhone SE 3"),
            // iPad 芯片保留，尺寸带单位，年份不混入规格。
            ("ipadpro11_m5_2025", "iPad Pro 11 英寸 (M5)"),
            ("ipadair13_m3_2025", "iPad Air 13 英寸 (M3)"),
            ("ipadmini7", "iPad mini 7"),
            ("ipad_a16_2025", "iPad (A16)"),
        ] {
            assert_eq!(
                family_display_name(raw).as_deref(),
                Some(want),
                "机型 {raw} 拼错了"
            );
        }

        // 认不出的前缀返回 None，让调用方退回购买页 slug。返回原文才是最糟的：
        // 下拉框里摆着 `foobar_m5_2025`，看上去像解析成功了。
        for raw in ["", "  ", "macbookair", "applewatch"] {
            assert_eq!(family_display_name(raw), None, "{raw} 不该被硬拼成机型名");
        }
    }

    #[test]
    fn 非ascii机型标识不会切在字符中间() {
        // 只要求不 panic：库代码里任何输入都不允许把进程带走。
        for raw in ["iphone17专业版", "苹果17", "iphone🙂", "ipad🙂pro"] {
            let _ = family_display_name(raw);
        }
    }

    #[test]
    fn 购买页slug被拼成机型名() {
        for (slug, want) in [
            ("macbook-air", "MacBook Air"),
            ("macbook-pro", "MacBook Pro"),
            ("imac", "iMac"),
            // Apple 自己写的就是小写 mini。
            ("mac-mini", "Mac mini"),
            ("mac-studio", "Mac Studio"),
            ("apple-watch", "Apple Watch"),
            ("apple-watch-se", "Apple Watch SE"),
            ("apple-watch-ultra", "Apple Watch Ultra"),
            ("ipad-mini", "iPad mini"),
            // 没登记的词首字母大写了事，至少不丢信息。
            ("mac-quantum", "Mac Quantum"),
            ("", ""),
        ] {
            assert_eq!(slug_display_name(slug), want, "slug {slug} 拼错了");
        }
    }

    #[test]
    fn 展示文案只取第一段正文() {
        for (raw, want) in [
            // 正文后面跟着补充说明，只要正文。
            (
                r#"钛金属<div class="form-label-small">提供 GPS + 蜂窝网络</div>"#,
                "钛金属",
            ),
            // 正文本身包在块级标签里，而且前面还有个换行 —— 从第一个边界
            // 切一刀会切出空串，所以必须取第一段**非空**文本。
            (
                "\n<div>铝金属</div>\n<span class=\"form-label-small\">可选 GPS</span>",
                "铝金属",
            ),
            // 脚注是块级边界。
            (
                r#"13 英寸<as-footnote data-id="x"><sup>1</sup></as-footnote>"#,
                "13 英寸",
            ),
            // &nbsp; 还原成普通空格，好让空白折叠起作用。
            (
                r#"10&nbsp;核中央处理器、8&nbsp;核图形处理器 <div class="x">快</div>"#,
                "10 核中央处理器、8 核图形处理器",
            ),
            // 行内标签只脱标签，不断句。
            ("256<small>GB</small> 存储容量", "256GB 存储容量"),
            // 没闭合的尖括号当普通字符，不能把后面整段吞掉。
            ("容量 < 1TB", "容量 < 1TB"),
            // ……但换行仍然是分段边界。图省事在这里直接收尾的话，补充说明会被
            // 一起拼进展示名。
            ("容量 < 1TB\n仅供参考", "容量 < 1TB"),
            // 认不出的实体原样保留，不静悄悄丢字符。
            ("A&unknownentity;B", "A&unknownentity;B"),
            ("&#65;&#x42;", "AB"),
            ("", ""),
            ("<div></div>", ""),
        ] {
            assert_eq!(plain_text(raw), want, "文案 {raw:?} 处理错了");
        }
    }

    #[test]
    fn 维度按购买页的提问顺序排列() {
        let raw = r#"{
            "products": [{
                "part": "MFCN4CH/B",
                "dimensions": {
                    "watch_cases-dimensionColor": "gold",
                    "watch_cases-dimensionCaseSize": "46mm",
                    "watch_cases-dimensionCaseMaterial": "titanium",
                    "watch_cases-dimensionConnection": "gpscell"
                }
            }],
            "displayValues": {
                "watch_cases-dimensionCaseSize": {"46mm": {"header": "46 毫米"}},
                "watch_cases-dimensionCaseMaterial": {"titanium": {"header": "钛金属"}},
                "watch_cases-dimensionConnection": {"gpscell": {"header": "GPS + 蜂窝网络"}},
                "watch_cases-dimensionColor": {"gold": {"text": "金色"}}
            }
        }"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Watch, "apple-watch")
            .expect("应当解析成功");
        assert_eq!(products.len(), 1);
        // 尺寸 → 材质 → 网络 → 颜色，和 Apple 在页面上的提问顺序一致。
        assert_eq!(
            products[0].title,
            "Apple Watch 46 毫米 钛金属 GPS + 蜂窝网络 金色"
        );
        assert_eq!(products[0].part_number, "MFCN4CH/B");
        assert_eq!(products[0].color, "金色");
        assert_eq!(products[0].capacity, "");
    }

    #[test]
    fn 病态文案不会把进程卡住() {
        // 这两种输入都不会 panic、也不会死循环，但只要扫描退化成 O(n²)，界面
        // 进程就会长时间没有响应 —— 而原因只是 Apple 的文案里多了一串尖括号
        // 或者 & 符号。
        //
        // 用一个宽到离谱的绝对预算，而不是比较两次耗时的倍率：倍率对机器负载
        // 太敏感，会变成一个时不时就红一次、最后被人加 #[ignore] 的测试。
        // 线性扫 20 万字节是毫秒级，平方级则是几十秒 —— 两者之间隔着四个数量级，
        // 两秒的预算落在中间，噪声再大也翻不过去。
        let n = 200_000;
        let started = std::time::Instant::now();
        assert!(!plain_text(&"<".repeat(n)).is_empty());
        assert!(!decode_entities(&"&".repeat(n)).is_empty());
        // 顺带把「一个 & 后面很远才有分号」也扫一遍：分号搜索窗口没限住的话，
        // 这条同样是平方级。
        assert!(!decode_entities(&format!("{}{}", "&".repeat(n), ";")).is_empty());
        let elapsed = started.elapsed();

        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "扫描 {n} 字节用了 {elapsed:?}，退化成平方级了"
        );
    }

    #[test]
    fn 空的维度表不足以让part被当成零件号() {
        // 光看 `dimensions` 字段在不在是不够的：页面哪天多给一个空的
        // `dimensions: {}`，iPad 的产品线代号就又混进来了 —— 那会让目录里
        // 挂满查不到的假零件号，界面上全是说不清的「未知」。
        for raw in [
            r#"{"products":[{"part":"IPADPRO11_WI_2025","dimensions":{}}]}"#,
            // 维度全是空白，等于没有维度。裁剪脚本也会把这种记录的 dimensions
            // 丢掉，两边的口径必须一致。
            r#"{"products":[{"part":"IPADPRO11_WI_2025","dimensions":{"a":"  "}}]}"#,
            // 维度有了，但取值不像零件号 —— Apple 的零件号一律带一条斜杠。
            r#"{"products":[{"part":"IPADPRO11_WI_2025","dimensions":{"a":"b"}}]}"#,
        ] {
            let products = parse_product_selection(raw.as_bytes(), Category::Ipad, "ipad-pro")
                .expect("应当解析成功");
            assert!(products.is_empty(), "{raw} 不该产出商品");
        }

        // 反过来，真的表壳记录照收不误。
        let raw = r#"{"products":[{"part":"MFCN4CH/B","dimensions":{"watch_cases-dimensionColor":"gold"}}]}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Watch, "apple-watch")
            .expect("应当解析成功");
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].part_number, "MFCN4CH/B");
    }

    #[test]
    fn ipad的part字段不是零件号() {
        // 平铺形状里的 `part` 装的是产品线代号。把它当零件号收下，整个 iPad
        // 目录就会挂满查不到的假零件号 —— 而那些查询失败会一路老实地变成
        // 「未知」，用户只会看到一屏说不清的状态。
        let raw = r#"{"products":[
            {"part":"IPADPRO11_WI_2025","familyType":"ipadpro11_m5_2025"},
            {"partNumber":"MDWU4CH/A","part":"IPADPRO11_WI_2025",
             "familyType":"ipadpro11_m5_2025","dimensionCapacity":"2tb"}
        ]}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Ipad, "ipad-pro")
            .expect("应当解析成功");
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].part_number, "MDWU4CH/A");
    }

    #[test]
    fn mac的定制机没有零件号会被跳过() {
        // 定制机（CONFIGURABLE）本来就不能到店取货，btrOrFdPartNumber 为 null
        // 正好把它们滤掉。
        let raw = r#"{"products":[
            {"type":"CONFIGURABLE","btrOrFdPartNumber":null,
             "dimensions":{"chassis-dimensionColor":"silver"}},
            {"type":"PRECONFIGURED_BTR","btrOrFdPartNumber":"MDH74CH/A",
             "dimensions":{"chassis-dimensionColor":"silver",
                           "chassis-dimensionScreensize":"13inch"}}
        ],
        "mainDisplayValues":{
            "chassis-dimensionColor":{"silver":{"header":"银色"}},
            "chassis-dimensionScreensize":{"13inch":{"header":"13 英寸"}}
        }}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Mac, "macbook-air")
            .expect("应当解析成功");
        assert_eq!(products.len(), 1);
        assert_eq!(products[0].part_number, "MDH74CH/A");
        // Mac 的机型名只能从 slug 来，尺寸必须从维度里补。
        assert_eq!(products[0].title, "MacBook Air 13 英寸 银色");
        assert_eq!(products[0].family, "macbook-air");
    }

    #[test]
    fn 重名的商品会补上零件号() {
        // 两台机器只差一个维度，而那个维度的文案取不到时，展示名会一字不差。
        // 让用户在两个完全相同的选项里挑一个，等于让他掷骰子。
        let raw = r#"{"products":[
            {"btrOrFdPartNumber":"MGED4CH/A","dimensions":{"chassis-dimensionColor":"spaceblack"}},
            {"btrOrFdPartNumber":"MGEE4CH/A","dimensions":{"chassis-dimensionColor":"spaceblack"}}
        ]}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Mac, "macbook-pro")
            .expect("应当解析成功");
        assert_eq!(products.len(), 2);
        assert_eq!(products[0].title, "MacBook Pro spaceblack MGED4CH/A");
        assert_eq!(products[1].title, "MacBook Pro spaceblack MGEE4CH/A");
        assert_ne!(products[0].title, products[1].title);
    }

    #[test]
    fn 排序序号不会被当成规格() {
        // dimensionSteporder 是页面上的排列序号。它在数据里通常是数字，但那是
        // 巧合不是契约 —— 换个站点给成字符串，展示名里就会平白多出一个「320」。
        let raw = r#"{"products":[{"partNumber":"MG724CH/A","familyType":"iphone17",
            "dimensionSteporder":"320","dimensionCapacity":"512gb"}]}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Iphone, "iphone-17")
            .expect("应当解析成功");
        assert_eq!(products[0].title, "iPhone 17 512GB");
    }

    #[test]
    fn 机型名认得出时不再重复拼屏幕尺寸() {
        // iphone17promax 已经说明了这是 6.9 英寸那台，再拼一遍就成了
        // 「iPhone 17 Pro Max 6.9 英寸 1TB」。
        let raw = r#"{"products":[{"partNumber":"MG0A4CH/A","familyType":"iphone17promax",
            "dimensionScreensize":"6_9inch","dimensionCapacity":"1tb",
            "dimensionColor":"cosmicorange"}],
            "displayValues":{"dimensionColor":{"cosmicorange":{"value":"宇宙橙色"}},
            "dimensionScreensize":{"6_9inch":{"value":"6.9 英寸"}}}}"#;
        let products = parse_product_selection(raw.as_bytes(), Category::Iphone, "iphone-17-pro")
            .expect("应当解析成功");
        assert_eq!(products[0].title, "iPhone 17 Pro Max 1TB 宇宙橙色");
    }
}
