use std::collections::BTreeMap;
use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use deno_core::unsync::spawn_blocking;
use deno_core::{op2, v8, GarbageCollected};
use deno_error::JsErrorBox;
use web_audio_api::context::AudioNodeLifetimeReservation;
use web_audio_api::node::{
    AudioScheduledSourceNode as _, AudioScheduledSourceNodeExt as _, OscillatorNode,
};

use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::online::{
    AudioRuntimeState, IsolateReservation, NativeOnlineContextState, NativePhase0OnlineContext,
};

const EVENT_WAKE_CAPACITY: usize = 256;
const MAX_DISPATCHES_PER_RECONCILE: usize = 128;
const RECONCILE_INTERVAL: Duration = Duration::from_millis(25);
// The reviewed exact oscillator transaction creates the oscillator plus its
// frequency and detune AudioParams as three independently reclaimed graph nodes.
const OSCILLATOR_GRAPH_NODE_COST: usize = 3;

struct NativeEventWake {
    _source_id: u64,
    _isolate_reservation: IsolateReservation,
    _host_reservation: AudioHostReservation,
}

struct GraphNodeReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
}

struct ScheduledSourcePayload {
    oscillator: OscillatorNode,
    _isolate_reservation: IsolateReservation,
    _host_reservation: AudioHostReservation,
}

struct ScheduledSourceRoot {
    id: u64,
    completion: web_audio_api::node::ScheduledSourceCompletionToken,
    payload: Mutex<Option<ScheduledSourcePayload>>,
    scheduled: AtomicBool,
}

impl ScheduledSourceRoot {
    fn retire(&self) {
        let payload = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Err(panic_payload) = panic::catch_unwind(AssertUnwindSafe(|| drop(payload))) {
            // A hostile native destructor cannot be allowed to unwind through
            // CppGC finalization or context retirement.
            std::mem::forget(panic_payload);
        }
    }
}

/// Native, V8-free reconciliation state shared by one online context.
pub(crate) struct ContextEventHub {
    runtime: Arc<AudioRuntimeState>,
    sender: mpsc::SyncSender<NativeEventWake>,
    receiver: Mutex<mpsc::Receiver<NativeEventWake>>,
    roots: Mutex<BTreeMap<u64, Arc<ScheduledSourceRoot>>>,
    next_source_id: AtomicU64,
    closed: AtomicBool,
}

impl ContextEventHub {
    pub(crate) fn new(runtime: Arc<AudioRuntimeState>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(EVENT_WAKE_CAPACITY);
        Self {
            runtime,
            sender,
            receiver: Mutex::new(receiver),
            roots: Mutex::new(BTreeMap::new()),
            next_source_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn rooted_source_count(&self) -> usize {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn next_source_id(&self) -> Result<u64, JsErrorBox> {
        let id = self
            .next_source_id
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            });
        if let Ok(id) = id {
            Ok(id)
        } else {
            self.shutdown();
            Err(JsErrorBox::generic(
                "scheduled-source identity space exhausted",
            ))
        }
    }

    fn register_oscillator(
        self: &Arc<Self>,
        context: &NativePhase0OnlineContext,
    ) -> Result<Arc<ScheduledSourceRoot>, JsErrorBox> {
        if self.closed.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic("AudioContext event hub is closed"));
        }
        let isolate_reservation = self.runtime.try_reserve_scheduled_source()?;
        let host_reservation = self
            .runtime
            .host()
            .try_reserve(AudioHostResourceKind::ScheduledSources, 1)
            .map_err(|error| {
                JsErrorBox::range_error(format!(
                    "AudioHost scheduled-source limit exceeded: {error}"
                ))
            })?;
        let isolate_graph_nodes = self
            .runtime
            .try_reserve_graph_nodes(OSCILLATOR_GRAPH_NODE_COST)?;
        let host_graph_nodes = self
            .runtime
            .host()
            .try_reserve(
                AudioHostResourceKind::GraphNodes,
                OSCILLATOR_GRAPH_NODE_COST,
            )
            .map_err(|error| {
                JsErrorBox::range_error(format!("AudioHost graph-node limit exceeded: {error}"))
            })?;
        let native_context = context.native_context()?;
        let graph_lifetime = AudioNodeLifetimeReservation::new(GraphNodeReservations {
            _isolate: isolate_graph_nodes,
            _host: host_graph_nodes,
        });
        let oscillator = match panic::catch_unwind(AssertUnwindSafe(move || {
            native_context.create_oscillator_with_lifetime_reservation(graph_lifetime)
        })) {
            Ok(oscillator) => oscillator,
            Err(payload) => {
                std::mem::forget(payload);
                context.lifecycle_state().poison();
                return Err(JsErrorBox::generic(
                    "Oscillator construction panicked and poisoned its AudioContext",
                ));
            }
        };
        let completion = oscillator.completion_token();
        let id = match self.next_source_id() {
            Ok(id) => id,
            Err(error) => {
                context.lifecycle_state().poison();
                return Err(error);
            }
        };
        let weak_hub = Arc::downgrade(self);
        if let Err(payload) = panic::catch_unwind(AssertUnwindSafe(|| {
            oscillator.set_onended(move |_| {
                if let Some(hub) = Weak::upgrade(&weak_hub) {
                    hub.enqueue_wake(id);
                }
            });
        })) {
            std::mem::forget(payload);
            context.lifecycle_state().poison();
            return Err(JsErrorBox::generic(
                "ended-handler installation panicked and poisoned its AudioContext",
            ));
        }
        let root = Arc::new(ScheduledSourceRoot {
            id,
            completion,
            payload: Mutex::new(Some(ScheduledSourcePayload {
                oscillator,
                _isolate_reservation: isolate_reservation,
                _host_reservation: host_reservation,
            })),
            scheduled: AtomicBool::new(false),
        });

        let mut roots = self
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closed.load(Ordering::Acquire) {
            drop(roots);
            root.retire();
            return Err(JsErrorBox::generic("AudioContext event hub is closed"));
        }
        if roots.insert(id, Arc::clone(&root)).is_some() {
            drop(roots);
            root.retire();
            self.shutdown();
            context.lifecycle_state().poison();
            return Err(JsErrorBox::generic(
                "duplicate scheduled-source identity poisoned the event hub",
            ));
        }
        Ok(root)
    }

    fn enqueue_wake(&self, source_id: u64) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        let Ok(isolate_reservation) = self.runtime.try_reserve_queued_event() else {
            return;
        };
        let Ok(host_reservation) = self
            .runtime
            .host()
            .try_reserve(AudioHostResourceKind::QueuedEvents, 1)
        else {
            return;
        };
        let _ = self.sender.try_send(NativeEventWake {
            _source_id: source_id,
            _isolate_reservation: isolate_reservation,
            _host_reservation: host_reservation,
        });
    }

    fn drain_wakes(&self) {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while receiver.try_recv().is_ok() {}
    }

    fn reconcile(&self) -> Vec<u64> {
        self.drain_wakes();
        if self.closed.load(Ordering::Acquire) {
            return Vec::new();
        }
        let completed = {
            let mut roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids: Vec<u64> = roots
                .iter()
                .filter_map(|(id, root)| root.completion.is_complete().then_some(*id))
                .take(MAX_DISPATCHES_PER_RECONCILE)
                .collect();
            ids.into_iter()
                .filter_map(|id| roots.remove(&id).map(|root| (id, root)))
                .collect::<Vec<_>>()
        };
        let mut ids = Vec::with_capacity(completed.len());
        for (id, root) in completed {
            root.retire();
            ids.push(id);
        }
        ids
    }

    fn wait_and_reconcile(&self) -> Vec<u64> {
        let completed = self.reconcile();
        if !completed.is_empty() || self.closed.load(Ordering::Acquire) {
            return completed;
        }
        {
            let receiver = self
                .receiver
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let _ = receiver.recv_timeout(RECONCILE_INTERVAL);
        }
        self.reconcile()
    }

    pub(crate) fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        // Always perform the drains. A producer that observed the hub open
        // immediately before the first shutdown may finish enqueueing after
        // that call's initial drain. Confirmed context retirement and Drop
        // call this method again after producer quiescence, so repeated calls
        // must reclaim those late records rather than returning early.
        self.drain_wakes();
        let roots = {
            let mut roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut *roots)
        };
        for root in roots.into_values() {
            root.retire();
        }
    }
}

impl Drop for ContextEventHub {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Private scheduled-source proof object used before the `WebIDL` facade lands.
pub(crate) struct NativePhase0Oscillator {
    root: Arc<ScheduledSourceRoot>,
    context: Arc<NativeOnlineContextState>,
}

// SAFETY: the object contains no V8 or CppGC pointers. Native source ownership
// is synchronized and the context event hub owns terminal reconciliation.
unsafe impl GarbageCollected for NativePhase0Oscillator {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativePhase0Oscillator"
    }
}

#[op2]
impl NativePhase0Oscillator {
    #[constructor]
    #[cppgc]
    fn new(
        #[cppgc] context: &NativePhase0OnlineContext,
    ) -> Result<NativePhase0Oscillator, JsErrorBox> {
        let hub = context.event_hub();
        Ok(NativePhase0Oscillator {
            root: hub.register_oscillator(context)?,
            context: context.lifecycle_state(),
        })
    }

    #[getter]
    #[number]
    fn source_id(&self) -> u64 {
        self.root.id
    }

    #[fast]
    fn schedule(&self, start_when: f64, stop_when: f64) -> Result<(), JsErrorBox> {
        if !start_when.is_finite()
            || !stop_when.is_finite()
            || start_when < 0.0
            || stop_when < start_when
        {
            return Err(JsErrorBox::range_error(
                "scheduled source times must be finite, nonnegative, and ordered",
            ));
        }
        if self.root.scheduled.swap(true, Ordering::AcqRel) {
            return Err(JsErrorBox::generic(
                "scheduled source has already been started",
            ));
        }
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut payload = self
                .root
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let payload = payload
                .as_mut()
                .ok_or_else(|| JsErrorBox::generic("scheduled source has retired"))?;
            payload.oscillator.start_at(start_when);
            payload.oscillator.stop_at(stop_when);
            Ok::<(), JsErrorBox>(())
        }));
        match result {
            Ok(result) => result,
            Err(payload) => {
                std::mem::forget(payload);
                self.context.poison();
                Err(JsErrorBox::generic(
                    "scheduled-source mutation panicked and poisoned its AudioContext",
                ))
            }
        }
    }
}

#[op2]
#[serde]
pub(crate) async fn op_phase0_wait_ended(
    #[cppgc] context: &NativePhase0OnlineContext,
) -> Result<Vec<u64>, JsErrorBox> {
    let hub = context.event_hub();
    spawn_blocking(move || hub.wait_and_reconcile())
        .await
        .map_err(|error| JsErrorBox::generic(format!("event reconciliation failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllowAllAudioPermissions, AudioHost, AudioLimits};
    use web_audio_api::output::SystemAudioOutput;

    #[test]
    fn repeated_shutdown_reclaims_a_late_admitted_wake() {
        let host = Arc::new(AudioHost::default());
        let runtime = Arc::new(AudioRuntimeState::new(
            Arc::clone(&host),
            Arc::new(AllowAllAudioPermissions),
            AudioLimits::unlimited(),
            Arc::new(SystemAudioOutput::new()),
        ));
        let hub = ContextEventHub::new(Arc::clone(&runtime));

        hub.shutdown();
        let isolate_reservation = runtime.try_reserve_queued_event().unwrap();
        let host_reservation = host
            .try_reserve(AudioHostResourceKind::QueuedEvents, 1)
            .unwrap();
        hub.sender
            .try_send(NativeEventWake {
                _source_id: 1,
                _isolate_reservation: isolate_reservation,
                _host_reservation: host_reservation,
            })
            .unwrap();
        assert_eq!(host.usage().queued_events(), 1);

        hub.shutdown();
        assert_eq!(host.usage().queued_events(), 0);
    }
}
