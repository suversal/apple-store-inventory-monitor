//! 出口线路池：记录每条线路在每个 Apple 站点上的保护冷却，并挑选下一条线路。
//!
//! Apple 按出口 IP 计数，连续约 30 次请求后会以 HTTP 541 拒绝十几分钟。切换
//! 线路只有在「被拦的线路先休息」时才有意义；否则只会把每个 IP 依次烧完。
//! 这里只做记账和选择，不发任何网络请求，也不认识 Clash —— 线路只是一个名字。
//!
//! 选择策略是「粘住当前线路」：同一出口配合同一套浏览器 Cookie 持续访问最像真实
//! 用户，每次切换还要重新建立连接和页面校验。只有当前线路进入冷却时才换下一条。

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 冷却档位：探测仍被拒绝时逐级延长，最长 30 分钟。
pub const COOLDOWN_STEPS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(20 * 60),
    Duration::from_secs(30 * 60),
];

/// 节点连不通时的短暂跳过时间。连不通是节点自身的问题，与 Apple 的封禁无关，
/// 不必像 541 那样长时间冷却；定期测速会把持续超时的节点提前排除。
pub const UNREACHABLE_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cooldown {
    level: usize,
    until: Instant,
}

/// 线路池。`site` 是 Apple 站点主机名，例如 `www.apple.com.cn`。
#[derive(Debug, Default)]
pub struct RoutePool {
    routes: Vec<String>,
    active: Option<String>,
    cooldowns: HashMap<(String, String), Cooldown>,
    /// 连不通的节点，到期前对所有站点都跳过。
    unreachable: HashMap<String, Instant>,
}

impl RoutePool {
    /// 用最新的线路列表替换旧列表。仍存在的线路保留冷却记录，不能因为重新读取
    /// 节点列表就让刚被拦的节点立即复用。
    pub fn set_routes(&mut self, routes: Vec<String>) {
        self.cooldowns
            .retain(|(route, _), _| routes.iter().any(|name| name == route));
        self.unreachable.retain(|route, _| routes.contains(route));
        if self
            .active
            .as_ref()
            .is_some_and(|active| !routes.contains(active))
        {
            self.active = None;
        }
        self.routes = routes;
    }

    pub fn routes(&self) -> &[String] {
        &self.routes
    }

    pub fn active(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn set_active(&mut self, route: Option<String>) {
        self.active = route.filter(|route| self.routes.contains(route));
    }

    fn remaining(&self, route: &str, site: &str, now: Instant) -> Option<Duration> {
        self.cooldowns
            .get(&(route.to_string(), site.to_string()))
            .map(|cooldown| cooldown.until.saturating_duration_since(now))
            .filter(|remaining| !remaining.is_zero())
    }

    pub fn is_available(&self, route: &str, site: &str, now: Instant) -> bool {
        self.remaining(route, site, now).is_none()
            && self
                .unreachable
                .get(route)
                .is_none_or(|until| *until <= now)
    }

    /// 节点连不通：短暂跳过，不计入 Apple 的冷却档位。
    pub fn mark_unreachable(&mut self, route: &str, now: Instant) {
        self.unreachable
            .insert(route.to_string(), now + UNREACHABLE_COOLDOWN);
    }

    /// 选出这次请求该用的线路：当前线路可用就继续用；否则从它之后按顺序找第一条
    /// 可用线路。全部冷却时返回 `None`。
    pub fn choose(&self, site: &str, now: Instant) -> Option<&str> {
        if let Some(active) = self.active()
            && self.is_available(active, site, now)
        {
            return Some(active);
        }
        let start = self
            .active
            .as_ref()
            .and_then(|active| self.routes.iter().position(|route| route == active))
            .map_or(0, |index| index + 1);
        (0..self.routes.len())
            .map(|offset| &self.routes[(start + offset) % self.routes.len()])
            .find(|route| self.is_available(route, site, now))
            .map(String::as_str)
    }

    /// 记录一次拒绝，返回这条线路本次的冷却时长。
    ///
    /// 首次被拒从 5 分钟开始；冷却结束后的探测再被拒才升一档。冷却期内重复登记
    /// （例如同一轮并发请求先后返回）不升档，避免一次事故被数成多次。
    pub fn block(&mut self, route: &str, site: &str, now: Instant) -> Duration {
        let key = (route.to_string(), site.to_string());
        let level = match self.cooldowns.get(&key) {
            Some(cooldown) if cooldown.until > now => {
                return cooldown.until.saturating_duration_since(now);
            }
            Some(cooldown) => (cooldown.level + 1).min(COOLDOWN_STEPS.len() - 1),
            None => 0,
        };
        let duration = COOLDOWN_STEPS[level];
        self.cooldowns.insert(
            key,
            Cooldown {
                level,
                until: now + duration,
            },
        );
        duration
    }

    /// 线路在该站点拿到了正常响应，清除冷却档位。
    pub fn succeed(&mut self, route: &str, site: &str) {
        self.cooldowns
            .remove(&(route.to_string(), site.to_string()));
    }

    /// 该站点最早恢复可用的等待时间；已有可用线路时为 `None`。
    pub fn wait_for(&self, site: &str, now: Instant) -> Option<Duration> {
        if self.routes.is_empty() || self.choose(site, now).is_some() {
            return None;
        }
        self.routes
            .iter()
            .filter_map(|route| {
                let unreachable = self
                    .unreachable
                    .get(route)
                    .map(|until| until.saturating_duration_since(now))
                    .filter(|remaining| !remaining.is_zero());
                self.remaining(route, site, now).max(unreachable)
            })
            .min()
    }

    /// 用户手动指定节点：清除它在所有站点上的冷却和短暂跳过。
    pub fn clear_route(&mut self, route: &str) {
        self.cooldowns.retain(|(name, _), _| name != route);
        self.unreachable.remove(route);
    }

    /// 线路是否在任一站点冷却或暂时连不通。
    pub fn is_resting(&self, route: &str, now: Instant) -> bool {
        self.unreachable
            .get(route)
            .is_some_and(|until| *until > now)
            || self
                .cooldowns
                .iter()
                .any(|((name, _), cooldown)| name == route && cooldown.until > now)
    }

    /// 用户要求立即重试：结束所有等待，但保留档位。探测若仍被拒，会继续升档。
    pub fn release_waits(&mut self, now: Instant) {
        for cooldown in self.cooldowns.values_mut() {
            cooldown.until = cooldown.until.min(now);
        }
        self.unreachable.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SITE: &str = "www.apple.com.cn";

    fn pool(names: &[&str]) -> RoutePool {
        let mut pool = RoutePool::default();
        pool.set_routes(names.iter().map(|name| name.to_string()).collect());
        pool
    }

    #[test]
    fn 当前线路可用时保持不变() {
        let mut pool = pool(&["香港01", "日本01", "新加坡01"]);
        pool.set_active(Some("日本01".into()));
        assert_eq!(pool.choose(SITE, Instant::now()), Some("日本01"));
    }

    #[test]
    fn 当前线路被拦后按顺序换到下一条可用线路() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01", "新加坡01"]);
        pool.set_active(Some("日本01".into()));
        pool.block("日本01", SITE, now);
        pool.block("新加坡01", SITE, now);
        assert_eq!(pool.choose(SITE, now), Some("香港01"));
    }

    #[test]
    fn 冷却按站点区分() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.set_active(Some("香港01".into()));
        pool.block("香港01", SITE, now);
        assert_eq!(pool.choose(SITE, now), None);
        assert_eq!(pool.choose("www.apple.com", now), Some("香港01"));
    }

    #[test]
    fn 只有冷却结束后的探测再被拦才升档() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[0]);
        // 冷却期内的重复登记不升档。
        let later = now + Duration::from_secs(60);
        assert_eq!(
            pool.block("香港01", SITE, later),
            COOLDOWN_STEPS[0] - Duration::from_secs(60)
        );
        let mut at = now + COOLDOWN_STEPS[0];
        for step in &COOLDOWN_STEPS[1..] {
            assert_eq!(pool.block("香港01", SITE, at), *step);
            at += *step;
        }
        // 封顶后保持最长档。
        assert_eq!(pool.block("香港01", SITE, at), COOLDOWN_STEPS[3]);
    }

    #[test]
    fn 探测成功后从第一档重新开始() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.block("香港01", SITE, now);
        pool.block("香港01", SITE, now + COOLDOWN_STEPS[0]);
        pool.succeed("香港01", SITE);
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[0]);
    }

    #[test]
    fn 全部冷却时给出最早恢复时间() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.block("香港01", SITE, now);
        pool.block("日本01", SITE, now + Duration::from_secs(120));
        assert_eq!(pool.wait_for(SITE, now), Some(COOLDOWN_STEPS[0]));
        pool.set_routes(vec!["香港01".into(), "日本01".into(), "美国01".into()]);
        assert_eq!(pool.wait_for(SITE, now), None);
    }

    #[test]
    fn 重新读取节点列表保留仍存在线路的冷却() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.set_active(Some("日本01".into()));
        pool.block("香港01", SITE, now);
        pool.set_routes(vec!["香港01".into(), "新加坡01".into()]);
        assert!(!pool.is_available("香港01", SITE, now));
        assert_eq!(pool.active(), None);
        assert_eq!(pool.choose(SITE, now), Some("新加坡01"));
    }

    #[test]
    fn 立即重试结束等待但保留档位() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.block("香港01", SITE, now);
        pool.release_waits(now);
        assert!(pool.is_available("香港01", SITE, now));
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[1]);
    }

    #[test]
    fn 连不通的节点短暂跳过且对所有站点生效() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.set_active(Some("香港01".into()));
        pool.mark_unreachable("香港01", now);
        assert_eq!(pool.choose(SITE, now), Some("日本01"));
        assert_eq!(pool.choose("www.apple.com", now), Some("日本01"));
        assert_eq!(
            pool.choose(SITE, now + UNREACHABLE_COOLDOWN),
            Some("香港01")
        );
        // 连不通不计入 Apple 冷却档位。
        assert_eq!(pool.block("香港01", SITE, now), COOLDOWN_STEPS[0]);
    }

    #[test]
    fn 全部节点都连不通时给出最早恢复时间() {
        let now = Instant::now();
        let mut pool = pool(&["香港01"]);
        pool.mark_unreachable("香港01", now);
        assert_eq!(pool.wait_for(SITE, now), Some(UNREACHABLE_COOLDOWN));
        pool.release_waits(now);
        assert_eq!(pool.choose(SITE, now), Some("香港01"));
    }

    #[test]
    fn 手动指定节点清除它的冷却() {
        let now = Instant::now();
        let mut pool = pool(&["香港01", "日本01"]);
        pool.block("香港01", SITE, now);
        pool.mark_unreachable("香港01", now);
        assert!(pool.is_resting("香港01", now));
        pool.clear_route("香港01");
        assert!(!pool.is_resting("香港01", now));
        assert!(pool.is_available("香港01", SITE, now));
    }
}
