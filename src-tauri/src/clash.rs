//! Clash / mihomo 外部控制接口的最小客户端。
//!
//! 只做三件事：读取专用策略组的节点、切换该组的节点、断开经过该组的连接。
//! 切换节点后必须断开旧连接：Chrome 会复用已有 HTTP/2 连接，不断开的话请求仍从
//! 旧节点出去。这里只断开链路里包含该策略组的连接，不影响电脑上的其他流量。

use std::collections::HashMap;
use std::time::Duration;

use apw_core::config::ClashSettings;
use serde::{Deserialize, Serialize};
use serde_json::json;

const CONTROLLER_TIMEOUT: Duration = Duration::from_secs(4);

/// 测速地址与 Clash 默认一致。不用 Apple 的地址，避免测速本身消耗 Apple 的请求额度。
pub const DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
/// 单个节点的测速超时。
pub const DELAY_TIMEOUT_MS: u32 = 3000;

/// 策略组里不能当作出口的条目类型：嵌套策略组、拒绝和兼容占位。
const NON_ROUTE_TYPES: &[&str] = &[
    "Selector",
    "URLTest",
    "Fallback",
    "LoadBalance",
    "Relay",
    "Reject",
    "RejectDrop",
    "Pass",
    "Compatible",
];

/// 机场常用来展示套餐信息的假节点关键词。
const INFO_NODE_KEYWORDS: &[&str] = &[
    "剩余", "到期", "过期", "官网", "重置", "套餐", "网址", "expire", "traffic",
];

#[derive(Debug, Clone)]
pub struct ClashController {
    http: reqwest::Client,
    base: reqwest::Url,
    secret: String,
}

#[derive(Debug, Deserialize)]
struct ProxyEntry {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    all: Vec<String>,
    #[serde(default)]
    now: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProxiesResponse {
    proxies: HashMap<String, ProxyEntry>,
}

#[derive(Debug, Deserialize)]
struct ProviderProxy {
    name: String,
}

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    #[serde(default)]
    proxies: Vec<ProviderProxy>,
}

#[derive(Debug, Deserialize)]
struct ProvidersResponse {
    #[serde(default)]
    providers: HashMap<String, ProviderEntry>,
}

#[derive(Debug, Deserialize)]
struct Connection {
    id: String,
    #[serde(default)]
    chains: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ConnectionsResponse {
    #[serde(default)]
    connections: Option<Vec<Connection>>,
}

/// 专用策略组的节点情况。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupNodes {
    /// 策略组当前选中的节点。
    pub now: Option<String>,
    /// 可作为查询出口的节点，保持策略组内的顺序。
    pub nodes: Vec<String>,
    /// 被排除的条目数：嵌套策略组、拒绝、信息节点或不符合关键词。
    pub skipped: usize,
    /// 来自订阅（proxy-providers）的节点所属订阅名。订阅节点只能通过订阅接口测速。
    #[serde(skip)]
    pub providers: HashMap<String, String>,
}

impl ClashController {
    pub fn new(settings: &ClashSettings) -> Result<Self, String> {
        let base = reqwest::Url::parse(&settings.controller)
            .map_err(|e| format!("Clash 控制接口地址无效：{e}"))?;
        if !matches!(base.scheme(), "http" | "https") {
            return Err("Clash 控制接口地址需以 http:// 或 https:// 开头".into());
        }
        // 控制接口在本机，绝不能再经过任何代理。
        let http = reqwest::Client::builder()
            .no_proxy()
            .timeout(CONTROLLER_TIMEOUT)
            .build()
            .map_err(|e| format!("无法创建 Clash 控制接口客户端：{e}"))?;
        Ok(Self {
            http,
            base,
            secret: settings.secret.clone(),
        })
    }

    fn url(&self, segments: &[&str]) -> reqwest::Url {
        let mut url = self.base.clone();
        if let Ok(mut path) = url.path_segments_mut() {
            path.pop_if_empty().extend(segments);
        }
        url
    }

    fn request(&self, method: reqwest::Method, segments: &[&str]) -> reqwest::RequestBuilder {
        let request = self.http.request(method, self.url(segments));
        if self.secret.is_empty() {
            request
        } else {
            request.bearer_auth(&self.secret)
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response, String> {
        let response = request
            .send()
            .await
            .map_err(|e| format!("无法连接 Clash 控制接口：{e}"))?;
        match response.status().as_u16() {
            200..=299 => Ok(response),
            401 | 403 => Err("Clash 控制接口拒绝访问，请检查密钥".into()),
            404 => Err("Clash 控制接口找不到该策略组".into()),
            status => Err(format!("Clash 控制接口返回 HTTP {status}")),
        }
    }

    /// 内核版本，用于测试连接。
    pub async fn version(&self) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Version {
            version: String,
        }
        let response = self
            .send(self.request(reqwest::Method::GET, &["version"]))
            .await?;
        response
            .json::<Version>()
            .await
            .map(|version| version.version)
            .map_err(|e| format!("Clash 版本信息无法解析：{e}"))
    }

    /// 读取策略组内可用作出口的节点。
    pub async fn group_nodes(&self, group: &str, filter: &str) -> Result<GroupNodes, String> {
        let response = self
            .send(self.request(reqwest::Method::GET, &["proxies"]))
            .await?;
        let proxies = response
            .json::<ProxiesResponse>()
            .await
            .map_err(|e| format!("Clash 节点列表无法解析：{e}"))?
            .proxies;
        let entry = proxies
            .get(group)
            .ok_or_else(|| format!("Clash 中没有名为「{group}」的策略组"))?;
        if entry.kind != "Selector" {
            return Err(format!(
                "「{group}」的类型是 {}，需要 select 类型的策略组才能由应用切换节点",
                entry.kind
            ));
        }
        let types: HashMap<&str, &str> = proxies
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.kind.as_str()))
            .collect();
        let (nodes, skipped) = filter_nodes(&entry.all, &types, filter);
        let providers = self.provider_index().await.unwrap_or_default();
        Ok(GroupNodes {
            now: entry.now.clone(),
            nodes,
            skipped,
            providers,
        })
    }

    /// 订阅节点名到订阅名的对应关系。
    async fn provider_index(&self) -> Result<HashMap<String, String>, String> {
        let response = self
            .send(self.request(reqwest::Method::GET, &["providers", "proxies"]))
            .await?;
        let providers = response
            .json::<ProvidersResponse>()
            .await
            .map_err(|e| format!("Clash 订阅列表无法解析：{e}"))?
            .providers;
        Ok(providers
            .into_iter()
            .flat_map(|(provider, entry)| {
                entry
                    .proxies
                    .into_iter()
                    .map(move |proxy| (proxy.name, provider.clone()))
            })
            .collect())
    }

    /// 把策略组切到指定节点。
    pub async fn select(&self, group: &str, node: &str) -> Result<(), String> {
        self.send(
            self.request(reqwest::Method::PUT, &["proxies", group])
                .json(&json!({ "name": node })),
        )
        .await
        .map(|_| ())
    }

    fn delay_request(&self, segments: &[&str]) -> reqwest::RequestBuilder {
        self.request(reqwest::Method::GET, segments)
            .query(&[
                ("url", DELAY_TEST_URL.to_string()),
                ("timeout", DELAY_TIMEOUT_MS.to_string()),
            ])
            // 控制接口要等测速结束才返回，客户端超时要比测速超时更长。
            .timeout(Duration::from_millis(u64::from(DELAY_TIMEOUT_MS) + 5000))
    }

    /// 测单个节点的延迟（毫秒）。超时或连不通时返回错误。
    ///
    /// 订阅节点不在 `/proxies` 下，必须走 `/providers/proxies/<订阅>/<节点>/healthcheck`。
    pub async fn node_delay(&self, node: &str, provider: Option<&str>) -> Result<u32, String> {
        #[derive(Deserialize)]
        struct Delay {
            delay: u32,
        }
        let request = match provider {
            Some(provider) => {
                self.delay_request(&["providers", "proxies", provider, node, "healthcheck"])
            }
            None => self.delay_request(&["proxies", node, "delay"]),
        };
        let response = request
            .send()
            .await
            .map_err(|e| format!("测速请求失败：{e}"))?;
        match response.status().as_u16() {
            200..=299 => {}
            504 => return Err("测速超时".into()),
            status => return Err(format!("测速失败（HTTP {status}）")),
        }
        match response.json::<Delay>().await {
            Ok(Delay { delay }) if delay > 0 => Ok(delay),
            Ok(_) => Err("测速超时".into()),
            Err(e) => Err(format!("测速结果无法解析：{e}")),
        }
    }

    /// 一次测完策略组内全部节点，返回测通节点的延迟。超时节点不在结果中。
    pub async fn group_delays(&self, group: &str) -> Result<HashMap<String, u32>, String> {
        let response = self
            .send(self.delay_request(&["group", group, "delay"]))
            .await?;
        let delays = response
            .json::<HashMap<String, u32>>()
            .await
            .map_err(|e| format!("策略组测速结果无法解析：{e}"))?;
        Ok(delays.into_iter().filter(|(_, delay)| *delay > 0).collect())
    }

    /// 断开链路中经过该策略组的连接，返回断开的数量。
    pub async fn close_group_connections(&self, group: &str) -> Result<usize, String> {
        let response = self
            .send(self.request(reqwest::Method::GET, &["connections"]))
            .await?;
        let connections = response
            .json::<ConnectionsResponse>()
            .await
            .map_err(|e| format!("Clash 连接列表无法解析：{e}"))?
            .connections
            .unwrap_or_default();
        let mut closed = 0;
        for connection in connections
            .iter()
            .filter(|connection| connection.chains.iter().any(|chain| chain == group))
        {
            self.send(self.request(reqwest::Method::DELETE, &["connections", &connection.id]))
                .await?;
            closed += 1;
        }
        Ok(closed)
    }
}

/// 从策略组条目中挑出可作为出口的节点。
fn filter_nodes(all: &[String], types: &HashMap<&str, &str>, filter: &str) -> (Vec<String>, usize) {
    let keywords: Vec<String> = filter
        .split('|')
        .map(|keyword| keyword.trim().to_lowercase())
        .filter(|keyword| !keyword.is_empty())
        .collect();
    let nodes: Vec<String> = all
        .iter()
        .filter(|name| {
            let kind = types.get(name.as_str()).copied().unwrap_or_default();
            let lower = name.to_lowercase();
            !NON_ROUTE_TYPES.contains(&kind)
                && !INFO_NODE_KEYWORDS
                    .iter()
                    .any(|keyword| lower.contains(keyword))
                && (keywords.is_empty() || keywords.iter().any(|keyword| lower.contains(keyword)))
        })
        .cloned()
        .collect();
    let skipped = all.len() - nodes.len();
    (nodes, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn 过滤嵌套策略组拒绝和信息节点() {
        let all = names(&[
            "剩余流量：100 GB",
            "套餐到期：2026-12-01",
            "自动选择",
            "REJECT",
            "DIRECT",
            "香港 01",
            "日本 02",
        ]);
        let types = HashMap::from([
            ("自动选择", "URLTest"),
            ("REJECT", "Reject"),
            ("DIRECT", "Direct"),
            ("香港 01", "Vmess"),
            ("日本 02", "Trojan"),
        ]);
        let (nodes, skipped) = filter_nodes(&all, &types, "");
        assert_eq!(nodes, names(&["DIRECT", "香港 01", "日本 02"]));
        assert_eq!(skipped, 4);
    }

    #[test]
    fn 关键词过滤不区分大小写() {
        let all = names(&["HK 01", "JP 01", "US 01"]);
        let types = HashMap::from([("HK 01", "Ss"), ("JP 01", "Ss"), ("US 01", "Ss")]);
        let (nodes, skipped) = filter_nodes(&all, &types, "hk| jp ");
        assert_eq!(nodes, names(&["HK 01", "JP 01"]));
        assert_eq!(skipped, 1);
    }

    #[test]
    fn 策略组名和连接编号按路径段编码() {
        let controller = ClashController::new(&ClashSettings {
            controller: "http://127.0.0.1:9097".into(),
            ..ClashSettings::default()
        })
        .expect("地址有效");
        assert_eq!(
            controller.url(&["proxies", "果到 雷达/测试"]).as_str(),
            "http://127.0.0.1:9097/proxies/%E6%9E%9C%E5%88%B0%20%E9%9B%B7%E8%BE%BE%2F%E6%B5%8B%E8%AF%95"
        );
    }

    #[test]
    fn 拒绝非http控制地址() {
        let error = ClashController::new(&ClashSettings {
            controller: "unix:///tmp/verge.sock".into(),
            ..ClashSettings::default()
        })
        .expect_err("应拒绝");
        assert!(error.contains("http"));
    }
}
