use std::collections::BTreeMap;
use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use deno_core::serde::Serialize;
use deno_core::unsync::spawn_blocking;
use deno_core::{op2, v8, GarbageCollected};
use deno_error::JsErrorBox;
use web_audio_api::context::{AudioControlBatchReservation, AudioNodeLifetimeReservation};
use web_audio_api::node::{
    AudioNode, AudioScheduledSourceNode as _, AudioScheduledSourceNodeExt as _, OscillatorNode,
    OscillatorType,
};
use web_audio_api::AudioParam;

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
// Register frequency, initial frequency, register detune, initial detune, register oscillator,
// and the two hidden parameter-owner edges form one exact construction batch.
const OSCILLATOR_CONSTRUCTION_COMMAND_COST: usize = 7;
const OSCILLATOR_SCHEDULE_COMMAND_COST: usize = 2;

struct NativeEventWake {
    _source_id: u32,
    _isolate_reservation: IsolateReservation,
    _host_reservation: AudioHostReservation,
}

struct GraphNodeReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
}

struct ScheduledSourcePayload {
    oscillator: OscillatorNode,
}

struct ScheduledSourceReservations {
    _isolate_reservation: IsolateReservation,
    _host_reservation: AudioHostReservation,
}

pub(crate) struct ScheduledSourceRoot {
    id: u32,
    completion: web_audio_api::node::ScheduledSourceCompletionToken,
    hub: Weak<ContextEventHub>,
    payload: Mutex<Option<ScheduledSourcePayload>>,
    reservations: Mutex<Option<ScheduledSourceReservations>>,
    rooted: AtomicBool,
    scheduled: AtomicBool,
}

impl ScheduledSourceRoot {
    pub(crate) fn id(&self) -> u32 {
        self.id
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.completion.is_complete()
    }

    pub(crate) fn with_audio_node<T>(&self, operation: impl FnOnce(&dyn AudioNode) -> T) -> T {
        let payload = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let payload = payload
            .as_ref()
            .expect("a live scheduled-source root retains its oscillator");
        operation(&payload.oscillator)
    }

    pub(crate) fn frequency(&self) -> AudioParam {
        self.with_oscillator(|oscillator| oscillator.frequency().clone())
    }

    pub(crate) fn detune(&self) -> AudioParam {
        self.with_oscillator(|oscillator| oscillator.detune().clone())
    }

    pub(crate) fn type_(&self) -> OscillatorType {
        self.with_oscillator(OscillatorNode::type_)
    }

    pub(crate) fn set_frequency(
        &self,
        value: f32,
        reservation: AudioControlBatchReservation,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        self.mutate(context, move |oscillator| {
            oscillator
                .frequency()
                .set_value_with_control_reservation(value, reservation);
        })
    }

    pub(crate) fn set_detune(
        &self,
        value: f32,
        reservation: AudioControlBatchReservation,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        self.mutate(context, move |oscillator| {
            oscillator
                .detune()
                .set_value_with_control_reservation(value, reservation);
        })
    }

    pub(crate) fn set_type(
        &self,
        value: OscillatorType,
        reservation: AudioControlBatchReservation,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        self.mutate(context, move |oscillator| {
            oscillator.set_type_with_control_reservation(value, reservation);
        })
    }

    pub(crate) fn start(
        self: &Arc<Self>,
        when: f64,
        reservation: AudioControlBatchReservation,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        if self.scheduled.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic(
                "InvalidStateError: OscillatorNode has already been started",
            ));
        }
        self.ensure_rooted(context)?;
        if self
            .scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(JsErrorBox::generic(
                "InvalidStateError: OscillatorNode has already been started",
            ));
        }
        self.mutate(context, move |oscillator| {
            oscillator.start_at_with_control_reservation(when, reservation);
        })
    }

    fn ensure_rooted(
        self: &Arc<Self>,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        if self.rooted.load(Ordering::Acquire) {
            return Ok(());
        }
        let hub = self
            .hub
            .upgrade()
            .ok_or_else(|| JsErrorBox::generic("AudioContext event hub has retired"))?;
        let reservations = hub.reserve_scheduled_source()?;
        let mut roots = hub
            .roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if hub.closed.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic("AudioContext event hub is closed"));
        }
        if self.rooted.load(Ordering::Acquire) {
            return Ok(());
        }
        if roots.insert(self.id, Arc::clone(self)).is_some() {
            drop(roots);
            hub.shutdown();
            context.poison();
            return Err(JsErrorBox::generic(
                "duplicate scheduled-source identity poisoned the event hub",
            ));
        }
        *self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(reservations);
        self.rooted.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn stop(
        &self,
        when: f64,
        reservation: AudioControlBatchReservation,
        context: &NativeOnlineContextState,
    ) -> Result<(), JsErrorBox> {
        if !self.scheduled.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic(
                "InvalidStateError: OscillatorNode cannot stop before start",
            ));
        }
        self.mutate(context, move |oscillator| {
            oscillator.stop_at_with_control_reservation(when, reservation);
        })
    }

    fn with_oscillator<T>(&self, operation: impl FnOnce(&OscillatorNode) -> T) -> T {
        let payload = self
            .payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let payload = payload
            .as_ref()
            .expect("a live scheduled-source root retains its oscillator");
        operation(&payload.oscillator)
    }

    fn mutate(
        &self,
        context: &NativeOnlineContextState,
        operation: impl FnOnce(&mut OscillatorNode),
    ) -> Result<(), JsErrorBox> {
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            let mut payload = self
                .payload
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let payload = payload
                .as_mut()
                .expect("a live scheduled-source root retains its oscillator");
            operation(&mut payload.oscillator);
        }));
        match result {
            Ok(()) => Ok(()),
            Err(payload) => {
                std::mem::forget(payload);
                context.poison();
                Err(JsErrorBox::generic(
                    "scheduled-source mutation panicked and poisoned its AudioContext",
                ))
            }
        }
    }

    fn release_reservations(&self) {
        let reservations = self
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop_contained(reservations);
    }

    fn retire(&self) {
        self.release_reservations();
    }
}

impl Drop for ScheduledSourceRoot {
    fn drop(&mut self) {
        let payload = self
            .payload
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let reservations = self
            .reservations
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop_contained(reservations);
        drop_contained(payload);
    }
}

fn drop_contained<T>(value: T) {
    if let Err(panic_payload) = panic::catch_unwind(AssertUnwindSafe(|| drop(value))) {
        // A hostile native destructor cannot unwind through CppGC finalization
        // or context retirement. The payload may itself have a hostile Drop.
        std::mem::forget(panic_payload);
    }
}

/// Native, V8-free reconciliation state shared by one online context.
pub(crate) struct ContextEventHub {
    runtime: Arc<AudioRuntimeState>,
    sender: mpsc::SyncSender<NativeEventWake>,
    receiver: Mutex<mpsc::Receiver<NativeEventWake>>,
    roots: Mutex<BTreeMap<u32, Arc<ScheduledSourceRoot>>>,
    terminal_ids: Mutex<Vec<u32>>,
    next_source_id: AtomicU32,
    state_changed: AtomicBool,
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
            terminal_ids: Mutex::new(Vec::new()),
            next_source_id: AtomicU32::new(1),
            state_changed: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn rooted_source_count(&self) -> usize {
        self.roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    fn next_source_id(&self) -> Result<u32, JsErrorBox> {
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

    fn reserve_scheduled_source(&self) -> Result<ScheduledSourceReservations, JsErrorBox> {
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
        Ok(ScheduledSourceReservations {
            _isolate_reservation: isolate_reservation,
            _host_reservation: host_reservation,
        })
    }

    pub(crate) fn register_oscillator(
        self: &Arc<Self>,
        context: &Arc<NativeOnlineContextState>,
        root_immediately: bool,
    ) -> Result<Arc<ScheduledSourceRoot>, JsErrorBox> {
        if self.closed.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic("AudioContext event hub is closed"));
        }
        let reservations = root_immediately
            .then(|| self.reserve_scheduled_source())
            .transpose()?;
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
        let construction_commands = self
            .runtime
            .try_reserve_control_commands(OSCILLATOR_CONSTRUCTION_COMMAND_COST)?;
        let native_context = context.context()?;
        let graph_lifetime = AudioNodeLifetimeReservation::new(GraphNodeReservations {
            _isolate: isolate_graph_nodes,
            _host: host_graph_nodes,
        });
        let construction_commands = AudioControlBatchReservation::new(construction_commands);
        let oscillator = match panic::catch_unwind(AssertUnwindSafe(move || {
            native_context
                .create_oscillator_with_reservations(graph_lifetime, construction_commands)
        })) {
            Ok(oscillator) => oscillator,
            Err(payload) => {
                std::mem::forget(payload);
                context.poison();
                return Err(JsErrorBox::generic(
                    "Oscillator construction panicked and poisoned its AudioContext",
                ));
            }
        };
        let completion = oscillator.completion_token();
        let id = match self.next_source_id() {
            Ok(id) => id,
            Err(error) => {
                context.poison();
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
            context.poison();
            return Err(JsErrorBox::generic(
                "ended-handler installation panicked and poisoned its AudioContext",
            ));
        }
        let root = Arc::new(ScheduledSourceRoot {
            id,
            completion,
            hub: Arc::downgrade(self),
            payload: Mutex::new(Some(ScheduledSourcePayload { oscillator })),
            reservations: Mutex::new(reservations),
            rooted: AtomicBool::new(root_immediately),
            scheduled: AtomicBool::new(false),
        });

        if !root_immediately {
            return Ok(root);
        }

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
            context.poison();
            return Err(JsErrorBox::generic(
                "duplicate scheduled-source identity poisoned the event hub",
            ));
        }
        Ok(root)
    }

    fn enqueue_wake(&self, source_id: u32) {
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

    pub(crate) fn enqueue_state_wake(&self) {
        self.state_changed.store(true, Ordering::Release);
        self.enqueue_wake(0);
    }

    fn drain_wakes(&self) {
        let receiver = self
            .receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while receiver.try_recv().is_ok() {}
    }

    fn reconcile(&self) -> Vec<u32> {
        self.drain_wakes();
        if self.closed.load(Ordering::Acquire) {
            return self.take_terminal_ids();
        }
        let completed = {
            let mut roots = self
                .roots
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let ids: Vec<u32> = roots
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

    fn take_terminal_ids(&self) -> Vec<u32> {
        let mut terminal_ids = self
            .terminal_ids
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *terminal_ids)
    }

    fn wait_and_reconcile(&self) -> Vec<u32> {
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

    fn wait_and_reconcile_events(&self) -> OnlineEventBatch {
        let mut ended_ids = self.reconcile();
        let mut state_changed = self.state_changed.swap(false, Ordering::AcqRel);
        let mut closed = self.closed.load(Ordering::Acquire);
        if ended_ids.is_empty() && !state_changed && !closed {
            {
                let receiver = self
                    .receiver
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let _ = receiver.recv_timeout(RECONCILE_INTERVAL);
            }
            ended_ids = self.reconcile();
            state_changed = self.state_changed.swap(false, Ordering::AcqRel);
            closed = self.closed.load(Ordering::Acquire);
        }
        OnlineEventBatch {
            ended_ids,
            state_changed,
            closed,
        }
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
        let mut completed = Vec::new();
        for root in roots.into_values() {
            if root.is_complete() {
                completed.push(root.id());
            }
            root.retire();
        }
        if !completed.is_empty() {
            self.terminal_ids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(completed);
        }
    }
}

#[derive(Serialize)]
#[serde(crate = "deno_core::serde", rename_all = "camelCase")]
pub(crate) struct OnlineEventBatch {
    ended_ids: Vec<u32>,
    state_changed: bool,
    closed: bool,
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
    runtime: Arc<AudioRuntimeState>,
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
        let lifecycle = context.lifecycle_state();
        Ok(NativePhase0Oscillator {
            root: hub.register_oscillator(&lifecycle, true)?,
            context: lifecycle,
            runtime: Arc::clone(&hub.runtime),
        })
    }

    #[getter]
    fn source_id(&self) -> f64 {
        f64::from(self.root.id())
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
        if self.root.scheduled.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic(
                "scheduled source has already been started",
            ));
        }
        let control_commands = self
            .runtime
            .try_reserve_control_commands(OSCILLATOR_SCHEDULE_COMMAND_COST)?;
        // Preallocate both opaque attachments before publishing scheduled=true. A panic cannot
        // leave wrapper state changed without complete reservation owners ready for both exact
        // runtime batches.
        let shared_reservation = Arc::new(control_commands);
        let start_reservation = AudioControlBatchReservation::new(Arc::clone(&shared_reservation));
        let stop_reservation = AudioControlBatchReservation::new(shared_reservation);
        if self
            .root
            .scheduled
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
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
            payload
                .oscillator
                .start_at_with_control_reservation(start_when, start_reservation);
            payload
                .oscillator
                .stop_at_with_control_reservation(stop_when, stop_reservation);
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
) -> Result<Vec<u32>, JsErrorBox> {
    let hub = context.event_hub();
    spawn_blocking(move || hub.wait_and_reconcile())
        .await
        .map_err(|error| JsErrorBox::generic(format!("event reconciliation failed: {error}")))
}

#[op2]
#[serde]
pub(crate) async fn op_online_wait_events(
    #[cppgc] context: &NativePhase0OnlineContext,
) -> Result<OnlineEventBatch, JsErrorBox> {
    let hub = context.event_hub();
    spawn_blocking(move || hub.wait_and_reconcile_events())
        .await
        .map_err(|error| JsErrorBox::generic(format!("event reconciliation failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AllowAllAudioPermissions, AudioEventPumpLiveness, AudioHost, AudioLimits};
    use web_audio_api::output::SystemAudioOutput;

    #[test]
    fn repeated_shutdown_reclaims_a_late_admitted_wake() {
        let host = Arc::new(AudioHost::default());
        let runtime = Arc::new(AudioRuntimeState::new(
            Arc::clone(&host),
            Arc::new(AllowAllAudioPermissions),
            AudioLimits::unlimited(),
            Arc::new(SystemAudioOutput::new()),
            AudioEventPumpLiveness::Unref,
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

    #[test]
    fn javascript_safe_source_identity_exhaustion_never_wraps() {
        let runtime = Arc::new(AudioRuntimeState::new(
            Arc::new(AudioHost::default()),
            Arc::new(AllowAllAudioPermissions),
            AudioLimits::unlimited(),
            Arc::new(SystemAudioOutput::new()),
            AudioEventPumpLiveness::Unref,
        ));
        let hub = ContextEventHub::new(runtime);
        hub.next_source_id.store(u32::MAX, Ordering::Release);

        assert!(hub.next_source_id().is_err());
        assert!(hub.closed.load(Ordering::Acquire));
        assert_eq!(hub.next_source_id.load(Ordering::Acquire), u32::MAX);
    }
}
