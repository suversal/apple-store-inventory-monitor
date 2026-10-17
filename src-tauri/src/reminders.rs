//! 到货后的重复提醒。
//!
//! 「在 X 秒内每隔 Y 秒再提醒一次」由这里实现：每次检测到有货（`InStock`
//! 事件）都会为对应目标重启一个重复窗口，窗口内周期性地重新发送同一份
//! 提醒（渠道与首次提醒相同：Bark + 提示音，取决于用户开启的项）。
//!
//! # 与查询主进程的边界
//!
//! 重复提醒跑在独立的 tokio 任务里，只做三件事：等待、读一份设置快照、
//! 发通知。它不持有查询器（Chromium 会话）的任何锁，也不参与调度决策；
//! 发送失败只写日志，不会向库存状态或轮次逻辑传播。手动停止监控、删除目标、
//! 关闭重复提醒，或在窗口内再次命中，都会让旧任务在下一个检查点自行退出。

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use apw_core::config::BarkOptions;
use apw_core::model::{Target, TargetKey};
use apw_core::notify::Notification;
use tauri::{AppHandle, Emitter, Manager};

use crate::{
    AppState, NOTICE_CHANNEL, dispatch_notification, in_stock_notification_body, target_open_url,
};

/// 重复提醒的排程。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RepeatSchedule {
    /// 两次提醒之间的间隔。
    pub interval: Duration,
    /// 最多重复发送多少次（不含首次提醒）。
    pub total: u32,
}

/// 从设置里解出重复排程；未开启或凑不出一次重复时返回 `None`。
///
/// 参数已在 `BarkOptions::normalize` 收敛过，这里再做一次防御性取整，
/// 保证运行期不会出现「间隔大于窗口」或除零这类事情。
pub(crate) fn repeat_schedule(options: &BarkOptions) -> Option<RepeatSchedule> {
    if options.repeat_seconds == 0 || options.repeat_interval_seconds == 0 {
        return None;
    }
    let interval = options.repeat_interval_seconds.max(1);
    let total = (options.repeat_seconds / interval).min(u64::from(u32::MAX)) as u32;
    if total == 0 {
        return None;
    }
    Some(RepeatSchedule {
        interval: Duration::from_secs(interval),
        total,
    })
}

/// 每个目标的重复世代 + 手动停止的总开关。
///
/// 任务是「自更新」的：每次新命中把该目标的世代 +1，旧任务在下一个检查点
/// 看到世代不符就退出；手动停止监控则推进总开关，所有在飞任务一起失效。
#[derive(Debug, Default)]
pub(crate) struct RepeatScheduler {
    stop_generation: AtomicU64,
    targets: Mutex<HashMap<TargetKey, u64>>,
}

impl RepeatScheduler {
    fn guard(&self) -> std::sync::MutexGuard<'_, HashMap<TargetKey, u64>> {
        // 锁内只有哈希表读写在持有，不会 panic；万一发生中毒也继续用，
        // 让提醒功能自行恢复，而不是把应用拖进二次崩溃。
        self.targets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 新命中：换代并返回新世代，旧任务将不再继续发送。
    pub(crate) fn renew(&self, key: &TargetKey) -> u64 {
        let mut targets = self.guard();
        let entry = targets.entry(key.clone()).or_insert(0);
        *entry += 1;
        *entry
    }

    /// 该目标当前的世代；从未命中过时为 0。
    pub(crate) fn generation(&self, key: &TargetKey) -> u64 {
        self.guard().get(key).copied().unwrap_or(0)
    }

    /// 手动停止的总世代。
    pub(crate) fn stop_generation(&self) -> u64 {
        self.stop_generation.load(Ordering::Acquire)
    }

    /// 手动停止监控：让所有在飞的重复提醒立即作废。
    ///
    /// 只挂在手动「暂停监控」上；自动的「到货后暂停监控」不经过这里 ——
    /// 那种场景下用户往往正需要重复提醒把人叫回来。
    pub(crate) fn cancel_all(&self) {
        self.stop_generation.fetch_add(1, Ordering::AcqRel);
    }
}

/// 为一次命中排程重复提醒；未开启或没有任何提醒渠道时什么都不做。
pub(crate) fn schedule(app: &AppHandle, target: Target) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    // 无论是否开启都先换代：这样旧的重复任务一定会停下。
    let generation = state.reminders.renew(&target.key());
    let settings = state.settings_snapshot();
    let Some(schedule) = repeat_schedule(&settings.bark_options) else {
        return;
    };
    if !settings.sound_enabled && settings.bark_url_for(&target).trim().is_empty() {
        return;
    }
    let stop_generation = state.reminders.stop_generation();
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let alive_key = target.key();
        let alive_app = app.clone();
        let send_app = app.clone();
        run_repeats(
            schedule,
            move || repeat_alive(&alive_app, &alive_key, generation, stop_generation),
            move |round| {
                let send_app = send_app.clone();
                let target = target.clone();
                async move {
                    send_repeat(&send_app, &target, round).await;
                }
            },
        )
        .await;
    });
}

/// 重复提醒循环的通用内核。
///
/// `alive` 在每个发送点之前调用一次，返回 `false` 立即结束（被新命中取代、
/// 手动停止、设置关闭或目标被删）；`send` 每轮调用一次。返回实际发送次数。
/// 抽成泛型是为了让测试用计数闭包驱动它。
pub(crate) async fn run_repeats<A, F, Fut>(
    schedule: RepeatSchedule,
    mut alive: A,
    mut send: F,
) -> u32
where
    A: FnMut() -> bool,
    F: FnMut(u32) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut sent = 0;
    for round in 1..=schedule.total {
        tokio::time::sleep(schedule.interval).await;
        if !alive() {
            break;
        }
        send(round).await;
        sent += 1;
    }
    sent
}

/// 任务是否还应继续：世代没变、没被手动停止、功能仍开启、目标还在列表里。
fn repeat_alive(app: &AppHandle, key: &TargetKey, generation: u64, stop_generation: u64) -> bool {
    let Some(state) = app.try_state::<AppState>() else {
        return false;
    };
    if state.reminders.stop_generation() != stop_generation {
        return false;
    }
    if state.reminders.generation(key) != generation {
        return false;
    }
    let settings = state.settings_snapshot();
    settings.bark_options.repeat_seconds > 0
        && settings.targets.iter().any(|target| target.key() == *key)
}

/// 发送一轮重复提醒。内容与首次提醒一致，但不打开页面、不触发结账。
async fn send_repeat(app: &AppHandle, target: &Target, round: u32) {
    let Some(state) = app.try_state::<AppState>() else {
        return;
    };
    let settings = state.settings_snapshot();
    let bark_url = settings.bark_url_for(target).to_owned();
    let mut notification =
        Notification::new("有货了（重复提醒）", in_stock_notification_body(target));
    if let Some(url) = target_open_url(app, target, settings.open_on_hit) {
        notification = notification.with_url(url);
    }
    let summary = format!("{} {}", target.store_title, target.product_name);
    match dispatch_notification(app, notification, &bark_url).await {
        Ok(()) => {
            let _ = app.emit(
                NOTICE_CHANNEL,
                format!("重复提醒（第 {round} 次）已发送：{summary}"),
            );
        }
        Err(error) => {
            let _ = app.emit(NOTICE_CHANNEL, format!("重复提醒发送失败：{error}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU32;

    fn 参数(window: u64, interval: u64) -> BarkOptions {
        BarkOptions {
            repeat_seconds: window,
            repeat_interval_seconds: interval,
            ..BarkOptions::default()
        }
    }

    #[test]
    fn 未开启或参数不足时没有排程() {
        assert_eq!(repeat_schedule(&参数(0, 30)), None, "关闭时不应排程");
        assert_eq!(repeat_schedule(&参数(300, 0)), None, "没有间隔不应排程");
        assert_eq!(
            repeat_schedule(&参数(1, 1)),
            Some(RepeatSchedule {
                interval: Duration::from_secs(1),
                total: 1,
            })
        );
    }

    #[test]
    fn 排程按窗口和间隔算出次数() {
        let schedule = repeat_schedule(&参数(300, 30)).expect("应当有排程");
        assert_eq!(schedule.interval, Duration::from_secs(30));
        assert_eq!(schedule.total, 10, "300 秒内每 30 秒一次应当是 10 次");
    }

    #[test]
    fn 新命中换代让旧任务失效且手动停止会推进总世代() {
        let scheduler = RepeatScheduler::default();
        let key = TargetKey("zh_CN|R390|MG6X4CH/A".into());
        assert_eq!(scheduler.generation(&key), 0);
        let first = scheduler.renew(&key);
        assert_eq!(scheduler.generation(&key), first);
        let second = scheduler.renew(&key);
        assert_ne!(first, second, "新命中必须换新世代");
        assert_eq!(scheduler.stop_generation(), 0);
        scheduler.cancel_all();
        assert_eq!(scheduler.stop_generation(), 1, "手动停止应当推进总世代");
    }

    #[tokio::test]
    async fn 重复循环按次数发送且可在中途停止() {
        let schedule = RepeatSchedule {
            interval: Duration::from_millis(15),
            total: 4,
        };
        let sent = run_repeats(schedule, || true, |_| async {}).await;
        assert_eq!(sent, 4, "未被打断时应完整发送");

        let counter = Arc::new(AtomicU32::new(0));
        let gate = counter.clone();
        let tick = counter.clone();
        let sent = run_repeats(
            RepeatSchedule {
                interval: Duration::from_millis(10),
                total: 10,
            },
            move || gate.load(Ordering::Relaxed) < 2,
            move |_| {
                let tick = tick.clone();
                async move {
                    tick.fetch_add(1, Ordering::Relaxed);
                }
            },
        )
        .await;
        assert_eq!(sent, 2, "达到停止条件后不应继续");
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }
}
