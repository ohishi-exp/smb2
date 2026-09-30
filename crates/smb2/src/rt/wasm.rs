//! The wasm32 backend: tasks on the host's event loop, timers on JS
//! `setTimeout`. No sockets; the caller brings a transport.
//!
//! Built for single-threaded hosts (Cloudflare Workers, a browser's main
//! thread). The crate's task bodies are `Send` because the other backends
//! migrate them between threads; here they never leave the one thread, and
//! the JS timer that isn't `Send` is wrapped in `SendWrapper`, which panics if
//! anything ever proves that wrong instead of misbehaving quietly.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::future::{AbortHandle, Abortable};
use gloo_timers::future::TimeoutFuture;
use send_wrapper::SendWrapper;

use super::Instant;

/// Run `future` on the host's event loop. The returned handle aborts it;
/// dropping the handle detaches, as on every backend.
pub(super) fn spawn<F>(future: F) -> AbortHandle
where
    F: Future<Output = ()> + 'static,
{
    let (handle, registration) = AbortHandle::new_pair();
    let task = Abortable::new(future, registration);
    wasm_bindgen_futures::spawn_local(async move {
        // `Err(Aborted)` is the abort we asked for; there is nothing to report.
        let _ = task.await;
    });
    handle
}

/// `setTimeout` takes a signed 32-bit millisecond delay and fires at once
/// for anything larger, so a far deadline is reached in steps of this.
const MAX_TIMEOUT_MS: u32 = i32::MAX as u32;

/// A timer that fires at a deadline on the crate's clock.
///
/// Re-arms until the clock has actually passed the deadline, instead of
/// trusting one `setTimeout`: a far deadline (`Duration::MAX` means "never")
/// doesn't fit in one, and a host may round timers or freeze its clock
/// between I/O events (Workers does), so a timer firing is a reason to look
/// at the clock, not a verdict.
#[derive(Debug)]
pub(super) struct Sleep {
    deadline: Instant,
    timer: Option<SendWrapper<TimeoutFuture>>,
}

impl Sleep {
    pub(super) fn until(deadline: Instant) -> Self {
        Self {
            deadline,
            timer: None,
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        loop {
            if let Some(timer) = this.timer.as_mut() {
                match Pin::new(&mut **timer).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(()) => this.timer = None,
                }
            }
            let remaining = this.deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Poll::Ready(());
            }
            this.timer = Some(SendWrapper::new(TimeoutFuture::new(millis_ceil(remaining))));
        }
    }
}

/// `duration` in whole milliseconds, rounded up so the timer never fires
/// before the deadline, and capped at what `setTimeout` takes.
fn millis_ceil(duration: Duration) -> u32 {
    let millis = duration.as_nanos().div_ceil(1_000_000);
    u32::try_from(millis).map_or(MAX_TIMEOUT_MS, |m| m.min(MAX_TIMEOUT_MS))
}
