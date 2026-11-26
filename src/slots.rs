//! Download slots: how many jobs download at once. The cap changes live;
//! lowering it never interrupts a running job.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use crate::progress::JobSnapshot;

/// Downloads at once when the setting is unset. YouTube throttles many
/// parallel streams from one address, so keep this low.
pub const DEFAULT_MAX_DOWNLOADS: u8 = 2;
pub const MAX_DOWNLOADS_CAP: u8 = 4;

pub fn clamp_max_downloads(value: u8) -> u8 {
    value.clamp(1, MAX_DOWNLOADS_CAP)
}

#[derive(Debug)]
struct State {
    cap: usize,
    active: usize,
}

#[derive(Debug)]
pub struct Slots {
    state: Mutex<State>,
    notify: tokio::sync::Notify,
}

impl Slots {
    pub fn new(cap: u8) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State {
                cap: usize::from(clamp_max_downloads(cap)),
                active: 0,
            }),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn cap(&self) -> u8 {
        self.lock().cap as u8
    }

    pub fn set_cap(&self, cap: u8) {
        self.lock().cap = usize::from(clamp_max_downloads(cap));
        self.notify.notify_waiters();
    }

    pub fn active(&self) -> usize {
        self.lock().active
    }

    fn try_take(&self) -> bool {
        let mut state = self.lock();
        if state.active < state.cap {
            state.active += 1;
            true
        } else {
            false
        }
    }

    fn give_back(&self) {
        {
            let mut state = self.lock();
            state.active = state.active.saturating_sub(1);
        }
        self.notify.notify_waiters();
    }

    /// Wait for a free slot. Returns early with the job's pause/stop error,
    /// so a queued job can be paused or stopped before it starts.
    pub async fn acquire(self: &Arc<Self>, job: &JobSnapshot) -> anyhow::Result<SlotGuard> {
        loop {
            job.check_control()?;
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.try_take() {
                return Ok(SlotGuard {
                    slots: Arc::clone(self),
                });
            }
            tokio::select! {
                () = &mut notified => {}
                () = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
    }
}

/// Holding one means downloading. Dropping it frees the slot.
#[derive(Debug)]
pub struct SlotGuard {
    slots: Arc<Slots>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.slots.give_back();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cap_is_honoured() {
        let slots = Slots::new(1);
        let job = JobSnapshot::default();
        let first = slots.acquire(&job).await.unwrap();
        let waiting = tokio::time::timeout(Duration::from_millis(300), slots.acquire(&job)).await;
        assert!(waiting.is_err(), "second job must wait");
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(2), slots.acquire(&job)).await;
        assert!(second.is_ok());
    }

    #[tokio::test]
    async fn raising_the_cap_starts_a_waiter() {
        let slots = Slots::new(1);
        let job = Arc::new(JobSnapshot::default());
        let _first = slots.acquire(&job).await.unwrap();
        let waiter = {
            let slots = Arc::clone(&slots);
            let job = Arc::clone(&job);
            tokio::spawn(async move { slots.acquire(&job).await.map(|_| ()) })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        slots.set_cap(2);
        let result = tokio::time::timeout(Duration::from_secs(2), waiter).await;
        assert!(result.unwrap().unwrap().is_ok());
    }

    #[tokio::test]
    async fn stopping_a_queued_job_returns_early() {
        let slots = Slots::new(1);
        let holder = JobSnapshot::default();
        let _held = slots.acquire(&holder).await.unwrap();
        let queued = JobSnapshot::default();
        queued.request_stop();
        let error = slots.acquire(&queued).await.unwrap_err();
        assert!(crate::progress::is_stopped(&error));
    }

    #[test]
    fn clamps() {
        assert_eq!(clamp_max_downloads(0), 1);
        assert_eq!(clamp_max_downloads(9), MAX_DOWNLOADS_CAP);
    }
}
