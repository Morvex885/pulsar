//! Task registry of the runtime thread, so `main` can wait for every actor's teardown (LEDs
//! dark, radio reset released, mDNS goodbye) before the process exits.

use std::cell::RefCell;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;

thread_local! {
    // Per thread: parallel `cargo test` threads each run their own runtime and registry.
    static REGISTRY: RefCell<Vec<(&'static str, JoinHandle<()>)>> = const { RefCell::new(Vec::new()) };
}

/// Spawns `fut` on the current `LocalSet` and registers it for `join_all`.
pub fn spawn_actor(name: &'static str, fut: impl Future<Output = ()> + 'static) {
    let handle = tokio::task::spawn_local(fut);
    REGISTRY.with_borrow_mut(|r| {
        r.retain(|(_, h)| !h.is_finished());
        r.push((name, handle));
    });
}

/// Waits until every registered actor (including ones spawned meanwhile) has finished, at most
/// `timeout` in total; true if all did.
pub async fn join_all(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let actors = REGISTRY.with_borrow_mut(std::mem::take);
        if actors.is_empty() {
            return true;
        }
        let mut all = true;
        for (name, handle) in actors {
            match tokio::time::timeout_at(deadline, handle).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => log::error!("actor {name}: {e}"),
                Err(_) => {
                    log::warn!("actor {name} did not stop");
                    all = false;
                }
            }
        }
        if !all {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn join_all_waits_for_actors_and_times_out_on_stuck_ones() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, rx) = tokio::sync::oneshot::channel::<()>();
                spawn_actor("quick", async {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                });
                spawn_actor("waits", async {
                    let _ = rx.await;
                    // Spawned during shutdown: still waited for.
                    spawn_actor("late", tokio::time::sleep(Duration::from_millis(200)));
                });
                drop(tx);
                let start = Instant::now();
                assert!(join_all(Duration::from_secs(3)).await);
                assert_eq!((Instant::now() - start).as_millis(), 200);

                spawn_actor("stuck", std::future::pending());
                assert!(!join_all(Duration::from_secs(3)).await);
                assert_eq!((Instant::now() - start).as_millis(), 3200);
            })
            .await;
    }
}
