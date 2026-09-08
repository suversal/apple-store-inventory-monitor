//! 调度引擎的测试。
//!
//! 每一条基本都对应 Go 版踩过的一个坑。用假的 Fetcher，不碰网络。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use apw_core::apple::{ApiError, Fetcher, PartStatus, StoreAvailability};
use apw_core::model::{Availability, Region, Target, UnknownReason};
use apw_core::watcher::{Event, Watcher, WatcherConfig};
use tokio::sync::Mutex;
use tokio::sync::mpsc::Receiver;

/// 假查询源的应答函数：入参依次是「第几次调用」「门店号」「请求的零件号」。
type Responder =
    Arc<dyn Fn(usize, &str, &[String]) -> Result<StoreAvailability, ApiError> + Send + Sync>;

/// 假的查询源，可编程返回值，并记录调用情况。
#[derive(Clone)]
struct FakeFetcher {
    calls: Arc<AtomicUsize>,
    /// 同一时刻在飞的查询数峰值。只盯一个门店时，一条循环任何时候最多一次在飞，
    /// 峰值超过 1 就只能是同时存在两条循环。
    in_flight: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    /// 每次调用记录下收到的零件号，用于验证「按门店合并请求」。
    seen_parts: Arc<Mutex<Vec<Vec<String>>>>,
    delay: Duration,
    responder: Responder,
}

impl FakeFetcher {
    fn new(
        responder: impl Fn(usize, &str, &[String]) -> Result<StoreAvailability, ApiError>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
            seen_parts: Arc::new(Mutex::new(Vec::new())),
            delay: Duration::from_millis(5),
            responder: Arc::new(responder),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn peak_in_flight(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl Fetcher for FakeFetcher {
    async fn pickup_message(
        &self,
        _region: &'static Region,
        store_number: &str,
        parts: &[String],
    ) -> Result<StoreAvailability, ApiError> {
        let nth = self.calls.fetch_add(1, Ordering::SeqCst);
        // 必须用 guard 来减计数，不能在函数末尾手动减。
        //
        // 引擎停止时会直接丢弃在飞的查询 future（Rust 里丢弃即取消），末尾那行
        // 根本没机会执行，计数会一路泄漏 —— 第一版就是这么写的，结果峰值恰好
        // 等于循环次数，看上去像引擎跑出了二十条循环。Drop 在取消路径上照样执行。
        let _guard = InFlightGuard::enter(&self.in_flight, &self.peak);
        self.seen_parts.lock().await.push(parts.to_vec());

        tokio::time::sleep(self.delay).await;
        (self.responder)(nth, store_number, parts)
    }
}

/// 在飞计数的 RAII guard：无论正常返回还是被取消，都会把计数减回去。
struct InFlightGuard {
    in_flight: Arc<AtomicUsize>,
}

impl InFlightGuard {
    fn enter(in_flight: &Arc<AtomicUsize>, peak: &Arc<AtomicUsize>) -> Self {
        let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        peak.fetch_max(now, Ordering::SeqCst);
        Self {
            in_flight: Arc::clone(in_flight),
        }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

fn ok_response(store: &str, parts: &[String], availability: Availability) -> StoreAvailability {
    let mut map = BTreeMap::new();
    for p in parts {
        map.insert(
            p.clone(),
            PartStatus {
                part_number: p.clone(),
                availability: availability.clone(),
                product_title: Some("iPhone 17".into()),
                pickup_display: "available".into(),
            },
        );
    }
    StoreAvailability {
        store_number: store.to_string(),
        store_name: "环球港".into(),
        parts: map,
    }
}

fn target(store: &str, part: &str) -> Target {
    Target {
        locale: "zh_CN".into(),
        store_number: store.into(),
        store_title: format!("上海-{store}"),
        part_number: part.into(),
        product_name: format!("型号 {part}"),
    }
}

fn fast_config() -> WatcherConfig {
    WatcherConfig {
        interval: Duration::from_millis(20),
        jitter: 0.0,
        concurrency: 4,
        event_buffer: 256,
    }
}

/// 等到至少收到一次 CycleComplete，返回这期间的全部事件。
async fn wait_cycle(rx: &mut Receiver<Event>) -> Vec<Event> {
    let mut got = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!remaining.is_zero(), "等一轮查询结束超时，已收到 {got:?}");
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(ev)) => {
                let done = matches!(ev, Event::CycleComplete { .. });
                got.push(ev);
                if done {
                    return got;
                }
            }
            Ok(None) => panic!("事件流意外关闭"),
            Err(_) => panic!("等一轮查询结束超时"),
        }
    }
}

fn count_in_stock(events: &[Event]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, Event::InStock { .. }))
        .count()
}

#[tokio::test]
async fn 查询失败必须落到未知而不是无货() {
    // 这是整个项目的核心不变量。上游在这里失守，静默失效了大半年。
    let fake = FakeFetcher::new(|_, _, _| Err(ApiError::Blocked("HTTP 541".into())));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;
    w.start().await;

    let events = wait_cycle(&mut rx).await;
    w.stop().await;

    let snap = w.snapshot().await;
    assert_eq!(snap.len(), 1);
    match &snap[0].availability {
        Availability::Unknown(UnknownReason::Blocked { detail }) => {
            assert!(detail.contains("541"));
        }
        other => panic!("被拦截时的状态应当是「被拦截」，实际为 {other:?}"),
    }
    assert_ne!(snap[0].availability, Availability::OutOfStock);

    // 整轮全败，不能报告为健康，而且要发告警。
    let healthy = events
        .iter()
        .any(|e| matches!(e, Event::CycleComplete { healthy: true, .. }));
    assert!(!healthy, "整轮被拦截却报告为健康");
    assert!(
        events.iter().any(|e| matches!(e, Event::Trouble { .. })),
        "被拦截时必须发告警，否则用户不知道界面上的状态已经不可信"
    );
}

#[tokio::test]
async fn 每轮确认有货都会提醒() {
    // 持续有货也要每轮提醒；离开有货的轮次不提醒。
    let fake = FakeFetcher::new(|nth, store, parts| {
        // 第 0、1 轮有货，第 2 轮无货，第 3 轮又有货。
        let a = match nth {
            0 | 1 => Availability::InStock,
            2 => Availability::OutOfStock,
            _ => Availability::InStock,
        };
        Ok(ok_response(store, parts, a))
    });
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;
    w.start().await;

    let mut all = Vec::new();
    for _ in 0..4 {
        all.extend(wait_cycle(&mut rx).await);
    }
    w.stop().await;

    // 第 0、1、3 轮有货，因此一共提醒三次。
    assert_eq!(
        count_in_stock(&all),
        3,
        "期望每轮确认有货都提醒，实际 {} 次",
        count_in_stock(&all)
    );
}

#[tokio::test]
async fn 每轮都会报告开始与完成即使库存没变() {
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::OutOfStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![
        target("R683", "WATCH-1/A"),
        target("R683", "WATCH-2/A"),
        target("R683", "WATCH-3/A"),
    ])
    .await;
    w.start().await;

    let first = wait_cycle(&mut rx).await;
    let second = wait_cycle(&mut rx).await;
    w.stop().await;

    assert!(first.iter().any(|event| matches!(
        event,
        Event::CycleStarted {
            cycle: 1,
            store_count: 1,
            target_count: 3,
        }
    )));
    assert!(first.iter().any(|event| matches!(
        event,
        Event::CycleComplete {
            cycle: 1,
            healthy: true,
            ..
        }
    )));
    assert!(second.iter().any(|event| matches!(
        event,
        Event::CycleStarted {
            cycle: 2,
            store_count: 1,
            target_count: 3,
        }
    )));
    assert!(second.iter().any(|event| matches!(
        event,
        Event::CycleComplete {
            cycle: 2,
            healthy: true,
            ..
        }
    )));
    assert!(fake.call_count() >= 2, "两轮都应实际调用查询源");
}

#[tokio::test]
async fn 暂停后重新开始会重新武装有货提醒() {
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::InStock)));
    let (w, mut rx) = Watcher::spawn(fake, fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;

    w.start().await;
    let first = wait_cycle(&mut rx).await;
    assert_eq!(count_in_stock(&first), 1, "首次有货应当提醒");
    w.stop().await;
    while rx.try_recv().is_ok() {}

    w.start().await;
    let resumed = wait_cycle(&mut rx).await;
    assert_eq!(
        count_in_stock(&resumed),
        1,
        "暂停后重新开始时，仍有货的项目应当重新提醒一次"
    );

    let next = wait_cycle(&mut rx).await;
    assert_eq!(
        count_in_stock(&next),
        1,
        "持续运行期间库存仍有货，下一轮也应再次提醒"
    );
    w.stop().await;
}

#[tokio::test]
async fn 一个候选有货不会停止其余候选的监控() {
    let fake = FakeFetcher::new(|nth, store, parts| {
        let mut statuses = BTreeMap::new();
        for (index, part) in parts.iter().enumerate() {
            let availability = if index <= nth {
                Availability::InStock
            } else {
                Availability::OutOfStock
            };
            statuses.insert(
                part.clone(),
                PartStatus {
                    part_number: part.clone(),
                    availability,
                    product_title: Some(format!("候选 {}", index + 1)),
                    pickup_display: "available".into(),
                },
            );
        }
        Ok(StoreAvailability {
            store_number: store.to_string(),
            store_name: "香港广场".into(),
            parts: statuses,
        })
    });
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![
        target("R390", "FIRST/A"),
        target("R390", "SECOND/A"),
        target("R390", "THIRD/A"),
    ])
    .await;
    w.start().await;

    let mut events = Vec::new();
    for _ in 0..3 {
        events.extend(wait_cycle(&mut rx).await);
    }
    w.stop().await;

    let alerted: std::collections::BTreeSet<_> = events
        .iter()
        .filter_map(|event| match event {
            Event::InStock { state } => Some(state.target.part_number.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        alerted,
        std::collections::BTreeSet::from(["FIRST/A", "SECOND/A", "THIRD/A"]),
        "三个候选应当在各自变为有货时分别提醒"
    );
    assert!(fake.call_count() >= 3, "第一个候选有货后查询循环停止了");
    assert!(
        w.snapshot()
            .await
            .iter()
            .all(|state| state.availability.is_in_stock()),
        "三轮后每个候选都应更新为有货"
    );
}

#[tokio::test]
async fn 同一门店的多个型号合并成一次请求() {
    // 既是效率问题也是风控问题：每个型号单独发一次，出站请求量会翻好几倍。
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::OutOfStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![
        target("R683", "MG724CH/A"),
        target("R683", "MG0A4CH/A"),
        target("R683", "MG364CH/A"),
    ])
    .await;
    w.start().await;
    wait_cycle(&mut rx).await;
    w.stop().await;

    let seen = fake.seen_parts.lock().await;
    assert!(!seen.is_empty(), "一次请求都没发出");
    assert_eq!(
        seen[0].len(),
        3,
        "三个型号应当合并成一次请求，实际 {:?}",
        seen[0]
    );
    assert_eq!(w.snapshot().await.len(), 3);
}

#[tokio::test]
async fn 停止之后不再发起查询() {
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::OutOfStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;
    w.start().await;
    wait_cycle(&mut rx).await;

    w.stop().await;
    let after_stop = fake.call_count();
    assert!(!w.is_running().await);

    // stop() 返回时本轮必然已经收尾，此后不该再有任何查询。
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        fake.call_count(),
        after_stop,
        "停止之后又发起了查询，说明还有循环在跑"
    );
}

#[tokio::test]
async fn 暂停后重新开始会立即查询并继续下一轮() {
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::OutOfStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;

    w.start().await;
    wait_cycle(&mut rx).await;
    w.stop().await;
    let before_resume = fake.call_count();

    while rx.try_recv().is_ok() {}
    w.start().await;
    wait_cycle(&mut rx).await;
    let after_first_resumed_cycle = fake.call_count();
    assert!(
        after_first_resumed_cycle > before_resume,
        "重新开始后没有立即发起查询"
    );

    wait_cycle(&mut rx).await;
    assert!(
        fake.call_count() > after_first_resumed_cycle,
        "重新开始只执行了第一轮，之后没有继续监控"
    );
    assert!(w.is_running().await, "查询循环已停止但运行状态仍应为真");
    w.stop().await;
}

#[tokio::test]
async fn 并发启停不会跑出两套循环() {
    // Go 版正是在这里翻车：Stop 释放锁去等旧循环退出，中间闯进来的 Start
    // 会再拉起一条，两条循环同时查询、交替写同一份状态。
    // actor 模型下命令是串行处理的，这种情况从结构上就不可能发生。
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::OutOfStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;

    for _ in 0..20 {
        w.start().await;
        wait_cycle(&mut rx).await;
        let (a, b) = (w.clone(), w.clone());
        let (s, t) = tokio::join!(
            tokio::spawn(async move { a.stop().await }),
            tokio::spawn(async move { b.start().await })
        );
        s.unwrap();
        t.unwrap();
        w.stop().await;
        while rx.try_recv().is_ok() {}
    }

    assert_eq!(
        fake.peak_in_flight(),
        1,
        "同时出现了 {} 个在飞的查询，单门店单循环最多只该有 1 个",
        fake.peak_in_flight()
    );
}

#[tokio::test]
async fn 全部型号都缺失时判定为门店级失败() {
    // 请求成功但一个型号都对不上，说明这轮实质是废的。若还算成功，就不退避、
    // 不告警，程序会继续按原频率请求一个已经失效的结构。
    let fake = FakeFetcher::new(|_, store, _| {
        // 返回一个完全不相干的型号。
        Ok(ok_response(
            store,
            &["OTHER/A".to_string()],
            Availability::InStock,
        ))
    });
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;
    w.start().await;
    let events = wait_cycle(&mut rx).await;
    w.stop().await;

    let snap = w.snapshot().await;
    assert!(
        snap[0].availability.is_unknown(),
        "型号在响应里找不到时必须是未知，实际为 {:?}",
        snap[0].availability
    );
    assert!(
        events.iter().any(|e| matches!(e, Event::Trouble { .. })),
        "整店型号全对不上必须告警"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::CycleComplete { healthy: true, .. })),
        "整店型号全对不上却报告为健康"
    );
}

#[tokio::test]
async fn 个别型号缺失不会拖垮整个门店() {
    // 与上一条相反的边界：只有一个型号对不上时，不该把整个门店判成故障。
    let fake = FakeFetcher::new(|_, store, parts| {
        // 只回第一个型号。
        Ok(ok_response(store, &parts[..1], Availability::OutOfStock))
    });
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![
        target("R683", "MG724CH/A"),
        target("R683", "MG0A4CH/A"),
    ])
    .await;
    w.start().await;
    let events = wait_cycle(&mut rx).await;
    w.stop().await;

    assert!(
        !events.iter().any(|e| matches!(e, Event::Trouble { .. })),
        "只有一个型号对不上就发告警，对单个零件号的问题反应过度"
    );
    let snap = w.snapshot().await;
    let unknown = snap.iter().filter(|s| s.availability.is_unknown()).count();
    assert_eq!(unknown, 1, "应当恰好一个型号处于未知");
}

#[tokio::test]
async fn 地区认不出来时登记为故障而不是静默跳过() {
    // 静默跳过会让这些行永远停在「待查询」，用户看不出程序从没查过它们。
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::InStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    let mut bad = target("R683", "MG724CH/A");
    bad.locale = "de_DE".into();
    w.set_targets(vec![bad]).await;
    w.start().await;
    let events = wait_cycle(&mut rx).await;
    w.stop().await;

    assert_eq!(fake.call_count(), 0, "地区都认不出来，不该发出任何请求");
    let snap = w.snapshot().await;
    match &snap[0].availability {
        Availability::Unknown(UnknownReason::SchemaDrift { raw, .. }) => {
            assert!(raw.contains("de_DE"));
        }
        other => panic!("无效地区应当登记成故障，实际为 {other:?}"),
    }
    assert!(events.iter().any(|e| matches!(e, Event::Trouble { .. })));
}

#[tokio::test]
async fn 替换目标列表会保留仍存在目标的状态() {
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::InStock)));
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![
        target("R683", "MG724CH/A"),
        target("R683", "MG0A4CH/A"),
    ])
    .await;
    w.start().await;
    wait_cycle(&mut rx).await;
    w.stop().await;

    // 去掉一个、加一个新的。
    w.set_targets(vec![
        target("R683", "MG724CH/A"),
        target("R390", "MG364CH/A"),
    ])
    .await;

    let snap = w.snapshot().await;
    assert_eq!(snap.len(), 2);
    let kept = snap
        .iter()
        .find(|s| s.target.part_number == "MG724CH/A")
        .expect("保留下来的目标应当还在");
    assert!(
        kept.availability.is_in_stock(),
        "保留下来的目标丢了既有状态"
    );

    let fresh = snap
        .iter()
        .find(|s| s.target.part_number == "MG364CH/A")
        .expect("新加的目标应当在");
    assert_eq!(
        fresh.availability,
        Availability::Unknown(UnknownReason::NotYetChecked),
        "新加的目标应当是「待查询」"
    );
}

#[tokio::test]
async fn 事件通道再小也不会丢掉到货提醒() {
    // 到货提醒是这个程序存在的全部理由，绝不能因为消费方慢就被丢掉。
    // 这里把通道压到极小，并且在一轮结束前完全不消费。
    let fake =
        FakeFetcher::new(|_, store, parts| Ok(ok_response(store, parts, Availability::InStock)));
    let config = WatcherConfig {
        event_buffer: 1,
        ..fast_config()
    };
    let (w, mut rx) = Watcher::spawn(fake.clone(), config);

    let targets: Vec<Target> = (0..12)
        .map(|i| target("R683", &format!("PART{i}/A")))
        .collect();
    w.set_targets(targets).await;
    w.start().await;

    // 慢慢地把事件读出来，模拟一个跟不上的界面。
    let mut in_stock = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while in_stock < 12 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Event::InStock { .. })) => in_stock += 1,
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    w.stop().await;

    assert_eq!(
        in_stock, 12,
        "12 个目标同时有货，只收到 {in_stock} 条提醒 —— 有提醒被丢掉了"
    );
}

/// 快照事件不能被丢弃，哪怕通道很小、每轮都有大量状态翻转。
///
/// 界面的列表**只**认 CycleComplete 带来的快照（StateChanged 是可丢的，前端刻意
/// 不拿它做增量）。这条事件一旦丢失，界面就会停在上一轮的取值上 —— 而那很可能
/// 正是「无货」。这正是本项目立项要消灭的失效形态。
///
/// 触发是确定性的而非概率性的：apply() 里那段发事件的循环一个 await 点都没有，
/// 一轮的事件在同一次 poll 里连着灌进通道，消费方根本没机会被调度；目标数超过通道
/// 容量时，排在最后的 CycleComplete 必然被丢。
///
/// 注意这里让 fetcher **每轮在成功与失败之间翻转**：否则第一轮之后状态不再变化，
/// 就不再有 StateChanged 突发，后续轮次的 CycleComplete 会畅通无阻，测试也就
/// 钉不住任何东西 —— 第一版正是这么写的，把修复还原后它照样通过。
#[tokio::test]
async fn 快照事件在每轮翻转且通道极小时也不会丢() {
    let round = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&round);
    let fake = FakeFetcher::new(move |nth, store, parts| {
        // 只盯一个门店，所以「第几次调用」就是「第几轮」。相邻两轮结果必须不同，
        // 否则第一轮之后状态不再翻转，就制造不出事件突发。
        seen.store(nth, Ordering::SeqCst);
        if nth % 2 == 0 {
            Ok(ok_response(store, parts, Availability::OutOfStock))
        } else {
            Err(ApiError::Blocked("HTTP 541".into()))
        }
    });
    let config = WatcherConfig {
        event_buffer: 4,
        ..fast_config()
    };
    let (w, mut rx) = Watcher::spawn(fake.clone(), config);

    // 目标数远超通道容量，制造单轮内的事件突发。
    let targets: Vec<Target> = (0..40)
        .map(|i| target("R683", &format!("PART{i}/A")))
        .collect();
    w.set_targets(targets).await;
    w.start().await;

    // 连续等若干轮，每一轮都必须收到快照。
    let mut cycles = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while cycles < 4 && tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
            Ok(Some(Event::CycleComplete { snapshot, .. })) => {
                assert_eq!(snapshot.len(), 40, "快照条数不对");
                cycles += 1;
            }
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(_) => break,
        }
    }
    w.stop().await;

    assert_eq!(
        cycles, 4,
        "通道容量 4、目标 40 个且每轮翻转，只收到 {cycles} 轮快照 —— 丢掉的那些轮里，界面会一直停在旧状态"
    );
}

/// 查询任务异常结束时，受影响的目标必须落回未知，不能停在上一轮的取值上。
///
/// 引擎兜住了子任务的 panic（发 Trouble、本轮不算健康），但如果那些目标的状态没被
/// 更新，它们会保持上一轮的值 —— 包括「无货」。一次故障因此被伪装成一个看起来正常
/// 的答案，而这正是上游静默失效大半年的形态。
#[tokio::test]
async fn 查询任务异常后目标不会停在陈旧的无货上() {
    let boom = Arc::new(AtomicUsize::new(0));
    let flag = Arc::clone(&boom);
    let fake = FakeFetcher::new(move |_, store, parts| {
        if flag.load(Ordering::SeqCst) > 0 {
            panic!("模拟查询任务内部错误");
        }
        Ok(ok_response(store, parts, Availability::OutOfStock))
    });
    let (w, mut rx) = Watcher::spawn(fake.clone(), fast_config());
    w.set_targets(vec![target("R683", "MG724CH/A")]).await;
    w.start().await;

    // 第一轮正常，拿到真实的「无货」。
    wait_one_cycle(&mut rx).await;
    assert_eq!(
        w.snapshot().await[0].availability,
        Availability::OutOfStock,
        "前置条件不成立"
    );

    // 之后每轮都 panic。
    boom.store(1, Ordering::SeqCst);
    wait_one_cycle(&mut rx).await;
    w.stop().await;

    let state = &w.snapshot().await[0];
    assert_ne!(
        state.availability,
        Availability::OutOfStock,
        "查询任务异常后仍停在陈旧的「无货」，这是本项目要根除的失效形态"
    );
    assert!(
        state.availability.is_unknown(),
        "应当落回未知，实际为 {:?}",
        state.availability
    );
}

/// 等一轮 CycleComplete。
async fn wait_one_cycle(rx: &mut Receiver<Event>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), rx.recv()).await {
            Ok(Some(Event::CycleComplete { .. })) => return,
            Ok(Some(_)) => {}
            _ => break,
        }
    }
    panic!("等一轮查询结束超时");
}
