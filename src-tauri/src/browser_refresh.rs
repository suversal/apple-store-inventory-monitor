//! One backend timer for the managed checkout tab, independent of stock polling.
use std::time::Duration;

use apw_core::config::BrowserRefreshSettings;
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;

use crate::checkout::CheckoutRunner;

pub async fn run(
    mut settings: watch::Receiver<BrowserRefreshSettings>,
    checkout: CheckoutRunner,
    app: AppHandle,
) {
    while let Some(current) = next_refresh(&mut settings).await {
        let message = match checkout.refresh_browser(&current).await {
            Ok(message) => message,
            Err(error) => format!("失败：{error}"),
        };
        let _ = app.emit(
            super::NOTICE_CHANNEL,
            format!("下单浏览器定时刷新：{message}"),
        );
    }
}

async fn next_refresh(
    settings: &mut watch::Receiver<BrowserRefreshSettings>,
) -> Option<BrowserRefreshSettings> {
    loop {
        let current = settings.borrow_and_update().clone();
        if !current.enabled {
            if settings.changed().await.is_err() {
                return None;
            }
            continue;
        }
        // Sleep first: enabling or changing the interval never refreshes at once.
        // Repeated/overdue ticks never accumulate into a burst after wake-up.
        tokio::select! {
            biased;
            result = settings.changed() => {
                if result.is_err() { return None; }
            }
            _ = tokio::time::sleep(Duration::from_secs(current.interval_seconds.clamp(60, 86_400))) => {
                return Some(current);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn interval_changes_reset_the_countdown_and_disabling_cancels_it() {
        let initial = BrowserRefreshSettings {
            enabled: true,
            interval_seconds: 300,
            ..Default::default()
        };
        let (tx, mut rx) = watch::channel(initial);
        let task = tokio::spawn(async move { next_refresh(&mut rx).await });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(299)).await;
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "never refresh immediately or before the deadline"
        );
        tx.send_modify(|s| s.interval_seconds = 60);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "a change starts a full new interval");
        tx.send_modify(|s| s.enabled = false);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(86_400)).await;
        tokio::task::yield_now().await;
        assert!(!task.is_finished(), "disabled timer remains idle");
        tx.send_modify(|s| s.enabled = true);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(task.await.unwrap().unwrap().interval_seconds, 60);
    }

    #[tokio::test]
    async fn closed_settings_channel_stops_the_timer() {
        let (tx, mut rx) = watch::channel(BrowserRefreshSettings::default());
        drop(tx);
        assert!(next_refresh(&mut rx).await.is_none());
    }
}
