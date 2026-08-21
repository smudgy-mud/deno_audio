use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Unique authority that seals one embedder-defined audio runtime generation.
///
/// Create an owner/registrar pair with [`AudioLifecycleOwner::new`], clone the
/// registrar into every extension instance belonging to the generation, then
/// consume the owner with [`AudioLifecycleOwner::retire`] before dropping those
/// runtimes. The returned receipt resolves only after every online context
/// admitted before the seal has confirmed native shutdown.
pub struct AudioLifecycleOwner {
    inner: Arc<AudioLifecycleState>,
}

/// Cloneable admission authority for online contexts in one runtime generation.
///
/// Install clones with
/// [`AudioExtensionOptions::lifecycle_registrar`](crate::AudioExtensionOptions::lifecycle_registrar).
/// Registration is linearized with generation retirement: once the owner is
/// retired (or dropped), later online-context construction is rejected before
/// output preparation or native context construction.
#[derive(Clone)]
pub struct AudioLifecycleRegistrar {
    inner: Arc<AudioLifecycleState>,
}

/// Receipt for exact retirement of one audio runtime generation.
///
/// Waiting is cancellation-independent. Dropping this future never cancels
/// native context shutdown or the lifecycle observers proving it.
#[must_use = "the generation is not proven retired unless this receipt is awaited"]
pub struct AudioLifecycleRetirement {
    inner: Arc<AudioLifecycleState>,
}

/// Failure to prove exact retirement of an admitted online context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioLifecycleRetirementError {
    /// At least one admitted context lost or returned no confirmed native
    /// shutdown acknowledgement. Its committed host accounting remains held.
    UnconfirmedShutdown,
}

impl fmt::Display for AudioLifecycleRetirementError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnconfirmedShutdown => formatter
                .write_str("an admitted AudioContext did not provide confirmed native shutdown"),
        }
    }
}

impl Error for AudioLifecycleRetirementError {}

impl AudioLifecycleOwner {
    /// Creates the unique owner and cloneable registrar for one generation.
    #[must_use]
    #[allow(
        clippy::new_ret_no_self,
        reason = "the unique owner and cloneable registrar must originate from one inseparable state"
    )]
    pub fn new() -> (Self, AudioLifecycleRegistrar) {
        let inner = Arc::new(AudioLifecycleState::default());
        (
            Self {
                inner: Arc::clone(&inner),
            },
            AudioLifecycleRegistrar { inner },
        )
    }

    /// Atomically seals this generation and returns its exact drain receipt.
    ///
    /// Context registrations linearized before this call are included in the
    /// receipt. Registrations linearized afterward are rejected. The receipt
    /// succeeds only if every included context confirms shutdown.
    pub fn retire(self) -> AudioLifecycleRetirement {
        self.inner.seal();
        AudioLifecycleRetirement {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Drop for AudioLifecycleOwner {
    fn drop(&mut self) {
        self.inner.seal();
    }
}

impl fmt::Debug for AudioLifecycleOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioLifecycleOwner")
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for AudioLifecycleRegistrar {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioLifecycleRegistrar")
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for AudioLifecycleRetirement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioLifecycleRetirement")
            .finish_non_exhaustive()
    }
}

impl Future for AudioLifecycleRetirement {
    type Output = Result<(), AudioLifecycleRetirementError>;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.poll_retirement(context)
    }
}

#[derive(Default)]
struct AudioLifecycleState {
    status: Mutex<AudioLifecycleStatus>,
}

#[derive(Default)]
struct AudioLifecycleStatus {
    sealed: bool,
    admitted: usize,
    failed: bool,
    waker: Option<Waker>,
}

impl AudioLifecycleState {
    fn try_admit(self: &Arc<Self>) -> Option<AudioLifecycleLease> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if status.sealed {
            return None;
        }
        let Some(admitted) = status.admitted.checked_add(1) else {
            // A process cannot actually own usize::MAX observer threads. Treat
            // impossible counter exhaustion as closed rather than wrapping.
            status.sealed = true;
            status.failed = true;
            return None;
        };
        status.admitted = admitted;
        Some(AudioLifecycleLease {
            inner: Arc::clone(self),
            committed: false,
            confirmed: false,
        })
    }

    fn seal(&self) {
        let wake = {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            status.sealed = true;
            (status.admitted == 0)
                .then(|| status.waker.take())
                .flatten()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }

    fn complete(&self, failed: bool) {
        let wake = {
            let mut status = self
                .status
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if status.admitted == 0 {
                status.failed = true;
            } else {
                status.admitted -= 1;
                status.failed |= failed;
            }
            (status.sealed && status.admitted == 0)
                .then(|| status.waker.take())
                .flatten()
        };
        if let Some(waker) = wake {
            waker.wake();
        }
    }

    fn poll_retirement(
        &self,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), AudioLifecycleRetirementError>> {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        debug_assert!(status.sealed, "a retirement receipt is always sealed");
        if status.admitted == 0 {
            return Poll::Ready(if status.failed {
                Err(AudioLifecycleRetirementError::UnconfirmedShutdown)
            } else {
                Ok(())
            });
        }
        if status
            .waker
            .as_ref()
            .is_none_or(|waker| !waker.will_wake(context.waker()))
        {
            status.waker = Some(context.waker().clone());
        }
        Poll::Pending
    }
}

pub(crate) struct AudioLifecycleLease {
    inner: Arc<AudioLifecycleState>,
    committed: bool,
    confirmed: bool,
}

impl AudioLifecycleRegistrar {
    pub(crate) fn try_admit(&self) -> Option<AudioLifecycleLease> {
        self.inner.try_admit()
    }
}

impl AudioLifecycleLease {
    pub(crate) fn commit(&mut self) {
        self.committed = true;
    }

    pub(crate) fn confirm(mut self) {
        debug_assert!(self.committed, "only committed observers need confirmation");
        self.confirmed = true;
    }
}

impl Drop for AudioLifecycleLease {
    fn drop(&mut self) {
        self.inner.complete(self.committed && !self.confirmed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Wake, Waker};

    #[derive(Default)]
    struct CountingWake(std::sync::atomic::AtomicUsize);

    impl Wake for CountingWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
    }

    fn poll(
        receipt: &mut AudioLifecycleRetirement,
        wake: &Arc<CountingWake>,
    ) -> Poll<Result<(), AudioLifecycleRetirementError>> {
        let waker = Waker::from(Arc::clone(wake));
        let mut context = Context::from_waker(&waker);
        Pin::new(receipt).poll(&mut context)
    }

    #[test]
    fn zero_and_racing_admissions_have_no_lost_wake() {
        let (owner, registrar) = AudioLifecycleOwner::new();
        let lease = registrar.try_admit().unwrap();
        let mut retirement = owner.retire();
        assert!(registrar.try_admit().is_none());

        let wake = Arc::new(CountingWake::default());
        assert!(poll(&mut retirement, &wake).is_pending());
        drop(lease);
        assert_eq!(wake.0.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(poll(&mut retirement, &wake), Poll::Ready(Ok(())));

        let (empty_owner, empty_registrar) = AudioLifecycleOwner::new();
        let mut empty_retirement = empty_owner.retire();
        assert!(empty_registrar.try_admit().is_none());
        assert_eq!(poll(&mut empty_retirement, &wake), Poll::Ready(Ok(())));
    }

    #[test]
    fn committed_lease_requires_confirmation() {
        let (owner, registrar) = AudioLifecycleOwner::new();
        let mut lease = registrar.try_admit().unwrap();
        lease.commit();
        let mut retirement = owner.retire();
        drop(lease);

        let wake = Arc::new(CountingWake::default());
        assert_eq!(
            poll(&mut retirement, &wake),
            Poll::Ready(Err(AudioLifecycleRetirementError::UnconfirmedShutdown))
        );
    }

    #[test]
    fn concurrent_admission_and_seal_linearize_without_lost_completion() {
        for _ in 0..32 {
            let (owner, registrar) = AudioLifecycleOwner::new();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let admission_barrier = Arc::clone(&barrier);
            let admission = std::thread::spawn(move || {
                admission_barrier.wait();
                registrar.try_admit()
            });
            barrier.wait();
            let mut retirement = owner.retire();
            let lease = admission.join().expect("admission thread should finish");
            let wake = Arc::new(CountingWake::default());

            if let Some(lease) = lease {
                assert!(poll(&mut retirement, &wake).is_pending());
                drop(lease);
            }
            assert_eq!(poll(&mut retirement, &wake), Poll::Ready(Ok(())));
        }
    }

    #[test]
    fn public_handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        fn assert_send_future<T: Future + Send>() {}

        assert_send_sync::<AudioLifecycleOwner>();
        assert_send_sync::<AudioLifecycleRegistrar>();
        assert_send_sync::<AudioLifecycleRetirement>();
        assert_send_future::<AudioLifecycleRetirement>();
    }
}
