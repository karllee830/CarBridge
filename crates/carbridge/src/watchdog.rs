// SPDX-FileCopyrightText: 2026 CarBridge contributors
// SPDX-License-Identifier: AGPL-3.0-only

//! A watchdog of the main loop: a task on the runtime beats every second, and a thread of its own ends the program
//! with an error once no beat came for [`LIMIT`], for procd to start it again.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// How long the runtime may go without a beat.
pub const LIMIT: Duration = Duration::from_secs(60);
/// How often the runtime beats, and how often the thread looks.
const BEAT: Duration = Duration::from_secs(1);
const LOOK: Duration = Duration::from_secs(5);

/// When the program started, which beats count from.
static START: OnceLock<Instant> = OnceLock::new();
/// The last beat, in milliseconds since [`START`].
static LAST_BEAT: AtomicU64 = AtomicU64::new(0);

fn since_start(now: Instant) -> u64 {
    let start = *START.get_or_init(|| now);
    now.saturating_duration_since(start).as_millis() as u64
}

/// Starts the thread that watches the beats; the time it starts counts as a beat.
pub fn start() {
    LAST_BEAT.store(since_start(Instant::now()), Ordering::Release);
    let started = std::thread::Builder::new()
        .name("watchdog".into())
        .spawn(|| loop {
            std::thread::sleep(LOOK);
            let silent = silence(LAST_BEAT.load(Ordering::Acquire), since_start(Instant::now()));
            if silent >= LIMIT {
                log!(
                    "carbridge: its main loop has not run for {} s; exiting for procd to start it again",
                    silent.as_secs()
                );
                crate::logfile::flush();
                std::process::exit(1);
            }
        });
    if let Err(err) = started {
        log!("carbridge: no watchdog: {err}");
    }
}

/// Beats every second, for as long as the runtime runs its tasks.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub async fn beat() {
    let mut tick = tokio::time::interval(BEAT);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        LAST_BEAT.store(since_start(Instant::now()), Ordering::Release);
    }
}

/// How long it has been since the beat at `last`, at `now`, both in milliseconds since the start.
fn silence(last: u64, now: u64) -> Duration {
    Duration::from_millis(now.saturating_sub(last))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_after_the_limit() {
        assert!(silence(1_000, 2_000) < LIMIT);
        assert!(silence(1_000, 60_999) < LIMIT);
        assert!(silence(1_000, 61_000) >= LIMIT);
        // A beat newer than the look, as when the two race, is no silence.
        assert_eq!(silence(5_000, 4_000), Duration::ZERO);
    }
}
