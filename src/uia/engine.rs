//! `UiaRuntime`: owns the single OS thread that talks to UI Automation.
//!
//! COM is initialized as MTA exactly once, on the worker thread, and every UIA
//! call is serialized through a job queue so all accessibility traffic stays on
//! that one thread. Clients submit jobs via [`UiaRuntime::call`] and await the
//! result as JSON.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::time::Duration;

use tokio::sync::oneshot;
use tracing::{error, info, warn};
use uiautomation::UIAutomation;

use crate::error::AppError;

/// A unit of work executed on the UIA worker thread.
pub type Job = Box<dyn FnOnce(&UIAutomation) -> Result<serde_json::Value, AppError> + Send>;

type JobMsg = (Job, oneshot::Sender<Result<serde_json::Value, AppError>>);

/// Default upper bound for a single UIA job. UI Automation calls normally
/// return in milliseconds; a provider that hangs (broken/blocked target app)
/// would otherwise wedge the only worker thread forever.
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(30);

/// Handle to the dedicated UIA worker thread.
pub struct UiaRuntime {
    tx: SyncSender<JobMsg>,
}

impl UiaRuntime {
    /// Spawn the worker thread and wait until COM + `IUIAutomation` are ready.
    pub fn start() -> Result<Self, AppError> {
        let (tx, rx) = sync_channel::<JobMsg>(64);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), AppError>>();

        std::thread::Builder::new()
            .name("uia-worker".to_string())
            .spawn(move || {
                info!("uia worker thread started");
                // Initializes COM (MTA) on this thread; all UIA calls stay here.
                let automation = match UIAutomation::new() {
                    Ok(a) => a,
                    Err(e) => {
                        error!(error = %e, "failed to initialize UI Automation");
                        let _ = ready_tx.send(Err(AppError::Uia(e.to_string())));
                        return;
                    }
                };
                info!("UI Automation initialized (COM MTA)");
                let _ = ready_tx.send(Ok(()));

                while let Ok((job, reply)) = rx.recv() {
                    // A panicking job must not take down the only worker
                    // thread (it would wedge every subsequent call): convert
                    // the panic into a normal error response and keep going.
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        job(&automation)
                    }))
                    .unwrap_or_else(|_| {
                        Err(AppError::Internal("uia job panicked".into()))
                    });
                    // The client may have gone away; ignore send failures.
                    let _ = reply.send(result);
                }
                info!("uia worker thread exiting");
            })
            .map_err(|e| AppError::Internal(format!("failed to spawn uia worker thread: {e}")))?;

        ready_rx
            .recv()
            .map_err(|_| AppError::Internal("uia worker thread died during startup".into()))??;

        Ok(UiaRuntime { tx })
    }

    /// Submit a job and await its result, with a deadline.
    ///
    /// The deadline only guards the await: if the UIA call itself hangs inside
    /// a broken provider the worker thread stays blocked (a known Windows
    /// limitation — UIA offers no cancellation), but the caller is released
    /// with a `Timeout` error instead of hanging forever.
    pub async fn call<F>(&self, timeout: Duration, job: F) -> Result<serde_json::Value, AppError>
    where
        F: FnOnce(&UIAutomation) -> Result<serde_json::Value, AppError> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send((Box::new(job), reply_tx))
            .map_err(|_| AppError::Internal("uia worker thread is not running".into()))?;

        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(AppError::Internal(
                "uia worker dropped the job result".into(),
            )),
            Err(_) => {
                warn!(?timeout, "uia job timed out");
                Err(AppError::Timeout(format!(
                    "UI Automation call did not complete within {} ms",
                    timeout.as_millis()
                )))
            }
        }
    }
}

impl Drop for UiaRuntime {
    fn drop(&mut self) {
        // Dropping the sender terminates the recv loop; the worker thread then
        // exits and releases COM. Do not join here (the thread may be blocked
        // in a hung provider); detached exit is harmless.
        info!("shutting down uia runtime");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_runs_jobs_on_one_thread() {
        let rt = UiaRuntime::start().expect("runtime starts");
        let rt2 = UiaRuntime::start().expect("second runtime starts (separate COM is fine on its own thread)");

        let out = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                rt.call(DEFAULT_JOB_TIMEOUT, |_auto| {
                    Ok(serde_json::json!({"thread": format!("{:?}", std::thread::current().id())}))
                })
                .await
                .unwrap()
            });
        assert!(out["thread"].is_string());
        drop(rt2);
    }
}
