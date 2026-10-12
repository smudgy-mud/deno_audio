use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::thread;

use deno_core::futures::FutureExt as _;
use deno_core::{op2, v8, GarbageCollected, OpState};
use deno_error::JsErrorBox;
use tokio::sync::{Mutex as AsyncMutex, Notify};
use web_audio_api::context::{
    AudioContext, AudioContextBuildError, AudioContextBuildErrorKind, AudioContextLatencyCategory,
    AudioContextLifecycleError, AudioContextOptions, AudioContextShutdownOutcome,
    AudioContextShutdownReceipt, AudioContextState, AudioContextStateChangeOutcome,
    AudioContextStateChangeReceipt, AudioControlBatchReservation,
};
use web_audio_api::node::{ChannelCountMode, ChannelInterpretation, OscillatorType};
use web_audio_api::output::AudioOutputFactory;

use crate::buffer::NativeAudioByteBudget;
use crate::host::{
    release_atomic, AudioHost, AudioHostLimitError, AudioHostOnlineContextPermit,
    AudioHostReservation, AudioHostResourceKind,
};
use crate::lifecycle::{AudioLifecycleLease, AudioLifecycleRegistrar};
use crate::node::{NativeAudioNode, NativePeriodicWave, OnlineGraph};
use crate::options::{AudioEventPumpLiveness, AudioLimits, AudioPermissions};
use crate::scheduled::ContextEventHub;

pub(crate) struct AudioRuntimeState {
    host: Arc<AudioHost>,
    permissions: Arc<dyn AudioPermissions>,
    limits: AudioLimits,
    output: Arc<dyn AudioOutputFactory>,
    event_pump_liveness: AudioEventPumpLiveness,
    lifecycle_registrar: Option<AudioLifecycleRegistrar>,
    online_contexts: AtomicUsize,
    decode_jobs: AtomicUsize,
    streaming_jobs: AtomicUsize,
    offline_render_jobs: AtomicUsize,
    graph_nodes: AtomicUsize,
    graph_connections: AtomicUsize,
    scheduled_sources: AtomicUsize,
    automation_events: AtomicUsize,
    queued_control_commands: AtomicUsize,
    queued_events: AtomicUsize,
}

impl AudioRuntimeState {
    pub(crate) fn new(
        host: Arc<AudioHost>,
        permissions: Arc<dyn AudioPermissions>,
        limits: AudioLimits,
        output: Arc<dyn AudioOutputFactory>,
        event_pump_liveness: AudioEventPumpLiveness,
        lifecycle_registrar: Option<AudioLifecycleRegistrar>,
    ) -> Self {
        Self {
            host,
            permissions,
            limits,
            output,
            event_pump_liveness,
            lifecycle_registrar,
            online_contexts: AtomicUsize::new(0),
            decode_jobs: AtomicUsize::new(0),
            streaming_jobs: AtomicUsize::new(0),
            offline_render_jobs: AtomicUsize::new(0),
            graph_nodes: AtomicUsize::new(0),
            graph_connections: AtomicUsize::new(0),
            scheduled_sources: AtomicUsize::new(0),
            automation_events: AtomicUsize::new(0),
            queued_control_commands: AtomicUsize::new(0),
            queued_events: AtomicUsize::new(0),
        }
    }

    pub(crate) fn host(&self) -> &Arc<AudioHost> {
        &self.host
    }

    pub(crate) fn decoder_lifecycle(&self) -> Option<AudioLifecycleRegistrar> {
        self.lifecycle_registrar.clone()
    }

    pub(crate) fn check_stream_playback(&self) -> Result<(), JsErrorBox> {
        self.permissions.check_playback("AudioFileStream")
    }

    pub(crate) fn limits(&self) -> &AudioLimits {
        &self.limits
    }

    pub(crate) const fn event_pump_refed(&self) -> bool {
        self.event_pump_liveness.is_refed()
    }

    fn try_admit_lifecycle_observer(&self) -> Result<Option<AudioLifecycleLease>, JsErrorBox> {
        self.lifecycle_registrar
            .as_ref()
            .map(|registrar| {
                registrar.try_admit().ok_or_else(|| {
                    JsErrorBox::generic("InvalidStateError: audio runtime generation is retired")
                })
            })
            .transpose()
    }

    pub(crate) fn try_reserve_decode_job(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::DecodeJobs, 1)
    }

    pub(crate) fn try_reserve_streaming_job(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::StreamingJobs, 1)
    }

    pub(crate) fn try_reserve_offline_render_job(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::OfflineRenderJobs, 1)
    }

    pub(crate) fn try_reserve_scheduled_source(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::ScheduledSources, 1)
    }

    pub(crate) fn try_reserve_automation_event(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::AutomationEvents, 1)
    }

    pub(crate) fn try_reserve_graph_nodes(
        self: &Arc<Self>,
        amount: usize,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::GraphNodes, amount)
    }

    pub(crate) fn try_reserve_graph_connection(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::GraphConnections, 1)
    }

    pub(crate) fn try_reserve_queued_event(
        self: &Arc<Self>,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::QueuedEvents, 1)
    }

    pub(crate) fn try_reserve_queued_control_commands(
        self: &Arc<Self>,
        amount: usize,
    ) -> Result<IsolateReservation, JsErrorBox> {
        self.try_reserve_resource(IsolateResourceKind::QueuedControlCommands, amount)
    }

    pub(crate) fn try_reserve_control_commands(
        self: &Arc<Self>,
        amount: usize,
    ) -> Result<ControlCommandReservations, JsErrorBox> {
        let isolate = self.try_reserve_queued_control_commands(amount)?;
        let host = self
            .host
            .try_reserve(AudioHostResourceKind::QueuedControlCommands, amount)
            .map_err(|error| {
                JsErrorBox::range_error(format!(
                    "AudioHost queued graph-control command limit exceeded: {error}"
                ))
            })?;
        Ok(ControlCommandReservations {
            _isolate: isolate,
            _host: host,
        })
    }

    fn try_reserve_resource(
        self: &Arc<Self>,
        resource: IsolateResourceKind,
        amount: usize,
    ) -> Result<IsolateReservation, JsErrorBox> {
        let (counter, limit, name) = match resource {
            IsolateResourceKind::DecodeJobs => {
                (&self.decode_jobs, self.limits.decode_jobs(), "decode job")
            }
            IsolateResourceKind::StreamingJobs => (
                &self.streaming_jobs,
                self.limits.streaming_jobs(),
                "streaming job",
            ),
            IsolateResourceKind::OfflineRenderJobs => (
                &self.offline_render_jobs,
                self.limits.offline_render_jobs(),
                "offline render job",
            ),
            IsolateResourceKind::GraphNodes => (
                &self.graph_nodes,
                self.limits.graph_nodes(),
                "live graph node",
            ),
            IsolateResourceKind::GraphConnections => (
                &self.graph_connections,
                self.limits.graph_connections(),
                "live graph connection",
            ),
            IsolateResourceKind::ScheduledSources => (
                &self.scheduled_sources,
                self.limits.scheduled_sources(),
                "scheduled source",
            ),
            IsolateResourceKind::AutomationEvents => (
                &self.automation_events,
                self.limits.automation_events(),
                "queued automation event",
            ),
            IsolateResourceKind::QueuedControlCommands => (
                &self.queued_control_commands,
                self.limits.queued_control_commands(),
                "queued graph-control command",
            ),
            IsolateResourceKind::QueuedEvents => (
                &self.queued_events,
                self.limits.queued_events(),
                "queued native event",
            ),
        };
        let mut current = counter.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(amount) else {
                return Err(JsErrorBox::range_error(format!(
                    "per-isolate {name} count overflowed"
                )));
            };
            if limit.is_some_and(|limit| next > limit) {
                return Err(JsErrorBox::range_error(format!(
                    "per-isolate {name} limit exceeded"
                )));
            }
            match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Ok(IsolateReservation {
                        runtime: Arc::clone(self),
                        resource,
                        amount,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    fn try_reserve_online_context(
        self: &Arc<Self>,
    ) -> Result<IsolateOnlineContextPermit, JsErrorBox> {
        let mut current = self.online_contexts.load(Ordering::Acquire);
        loop {
            let Some(next) = current.checked_add(1) else {
                return Err(JsErrorBox::range_error(
                    "per-isolate online AudioContext count overflowed",
                ));
            };
            if self
                .limits
                .online_contexts()
                .is_some_and(|limit| next > limit)
            {
                return Err(JsErrorBox::range_error(
                    "per-isolate online AudioContext limit exceeded",
                ));
            }
            match self.online_contexts.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Ok(IsolateOnlineContextPermit {
                        runtime: Arc::clone(self),
                        committed: false,
                        release: true,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }

    #[cfg(test)]
    fn online_contexts(&self) -> usize {
        self.online_contexts.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy)]
enum IsolateResourceKind {
    DecodeJobs,
    StreamingJobs,
    OfflineRenderJobs,
    GraphNodes,
    GraphConnections,
    ScheduledSources,
    AutomationEvents,
    QueuedControlCommands,
    QueuedEvents,
}

pub(crate) struct IsolateReservation {
    runtime: Arc<AudioRuntimeState>,
    resource: IsolateResourceKind,
    amount: usize,
}

pub(crate) struct ControlCommandReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
}

impl Drop for IsolateReservation {
    fn drop(&mut self) {
        let counter = match self.resource {
            IsolateResourceKind::DecodeJobs => &self.runtime.decode_jobs,
            IsolateResourceKind::StreamingJobs => &self.runtime.streaming_jobs,
            IsolateResourceKind::OfflineRenderJobs => &self.runtime.offline_render_jobs,
            IsolateResourceKind::GraphNodes => &self.runtime.graph_nodes,
            IsolateResourceKind::GraphConnections => &self.runtime.graph_connections,
            IsolateResourceKind::ScheduledSources => &self.runtime.scheduled_sources,
            IsolateResourceKind::AutomationEvents => &self.runtime.automation_events,
            IsolateResourceKind::QueuedControlCommands => &self.runtime.queued_control_commands,
            IsolateResourceKind::QueuedEvents => &self.runtime.queued_events,
        };
        release_atomic(counter, self.amount);
    }
}

struct IsolateOnlineContextPermit {
    runtime: Arc<AudioRuntimeState>,
    committed: bool,
    release: bool,
}

impl IsolateOnlineContextPermit {
    fn commit(&mut self) {
        self.committed = true;
    }

    fn release_confirmed(mut self) {
        self.committed = false;
        self.release = true;
    }
}

impl Drop for IsolateOnlineContextPermit {
    fn drop(&mut self) {
        if self.committed {
            self.release = false;
        }
        if self.release {
            release_atomic(&self.runtime.online_contexts, 1);
        }
    }
}

struct LifecycleObservation {
    receipt: AudioContextShutdownReceipt,
    host_permit: AudioHostOnlineContextPermit,
    isolate_permit: IsolateOnlineContextPermit,
    event_hub: Option<Arc<ContextEventHub>>,
    lifecycle_requests: Option<Weak<LifecycleRequestCoordinator>>,
    lifecycle_lease: Option<AudioLifecycleLease>,
}

fn lifecycle_observer(receiver: &mpsc::Receiver<LifecycleObservation>) {
    let Ok(observation) = receiver.recv() else {
        return;
    };
    let LifecycleObservation {
        receipt,
        host_permit,
        isolate_permit,
        event_hub,
        lifecycle_requests,
        lifecycle_lease,
    } = observation;
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| receipt.wait()));
    if let Some(coordinator) = lifecycle_requests.and_then(|weak| weak.upgrade()) {
        coordinator.begin_close();
    }
    if let Some(event_hub) = event_hub {
        event_hub.shutdown();
    }
    if matches!(outcome, Ok(AudioContextShutdownOutcome::Confirmed(_))) {
        host_permit.release_confirmed();
        isolate_permit.release_confirmed();
        if let Some(lease) = lifecycle_lease {
            lease.confirm();
        }
    }
    // Every other result intentionally drops committed permits without
    // decrementing their counters. Unconfirmed output ownership must remain
    // scarce until the accounting domain itself is discarded.
}

fn spawn_lifecycle_observer() -> Result<
    (
        mpsc::SyncSender<LifecycleObservation>,
        thread::JoinHandle<()>,
    ),
    JsErrorBox,
> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let join = thread::Builder::new()
        .name("deno-audio-lifecycle-observer".to_owned())
        .spawn(move || lifecycle_observer(&receiver))
        .map_err(|error| {
            JsErrorBox::generic(format!(
                "failed to establish AudioContext lifecycle ownership: {error}"
            ))
        })?;
    Ok((sender, join))
}

#[derive(Default)]
struct LifecycleRequestQueue {
    pending: Option<AudioContextStateChangeReceipt>,
}

struct LifecycleRequestCoordinator {
    serial: AsyncMutex<LifecycleRequestQueue>,
    waiters: AtomicUsize,
    waiter_limit: Option<usize>,
    closing: AtomicBool,
    retry: Notify,
}

impl LifecycleRequestCoordinator {
    fn new(waiter_limit: Option<usize>) -> Arc<Self> {
        Arc::new(Self {
            serial: AsyncMutex::new(LifecycleRequestQueue::default()),
            waiters: AtomicUsize::new(0),
            waiter_limit,
            closing: AtomicBool::new(false),
            retry: Notify::new(),
        })
    }

    fn try_waiter(self: &Arc<Self>) -> Result<LifecycleWaiterPermit, JsErrorBox> {
        self.waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                let next = count.checked_add(1)?;
                self.waiter_limit
                    .is_none_or(|limit| next <= limit)
                    .then_some(next)
            })
            .map_err(|_| {
                JsErrorBox::range_error(
                    "AudioContext lifecycle waiter limit exceeded before native admission",
                )
            })?;
        Ok(LifecycleWaiterPermit {
            coordinator: Arc::clone(self),
        })
    }

    fn begin_close(&self) {
        self.closing.store(true, Ordering::Release);
        self.wake_retry();
    }

    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    fn wake_retry(&self) {
        self.retry.notify_one();
    }

    async fn wait_to_retry(&self) {
        self.retry.notified().await;
    }
}

struct LifecycleWaiterPermit {
    coordinator: Arc<LifecycleRequestCoordinator>,
}

impl Drop for LifecycleWaiterPermit {
    fn drop(&mut self) {
        let previous = self.coordinator.waiters.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "lifecycle waiter count underflow");
    }
}

pub(crate) struct NativeOnlineContextState {
    context: Mutex<Option<Arc<AudioContext>>>,
    event_hub: Arc<ContextEventHub>,
    lifecycle_requests: Arc<LifecycleRequestCoordinator>,
}

impl NativeOnlineContextState {
    pub(crate) fn context(&self) -> Result<Arc<AudioContext>, JsErrorBox> {
        self.context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(Arc::clone)
            .ok_or_else(|| JsErrorBox::generic("AudioContext is poisoned or retired"))
    }

    pub(crate) fn event_hub(&self) -> Arc<ContextEventHub> {
        Arc::clone(&self.event_hub)
    }

    pub(crate) fn poison(&self) {
        self.lifecycle_requests.begin_close();
        let context = self
            .context
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(context);
    }
}

impl Drop for NativeOnlineContextState {
    fn drop(&mut self) {
        self.lifecycle_requests.begin_close();
        let context = self
            .context
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        drop(context);
    }
}

/// Private online-context proof object. The later `WebIDL` facade will own this
/// same lifecycle state rather than a raw upstream context.
pub(crate) struct NativePhase0OnlineContext {
    state: Arc<NativeOnlineContextState>,
    graph: Arc<OnlineGraph>,
}

impl NativePhase0OnlineContext {
    pub(crate) fn take_file_workers(&self) -> Vec<crate::streaming::FileStreamActivity> {
        self.graph.take_file_workers()
    }
    pub(crate) fn event_hub(&self) -> Arc<ContextEventHub> {
        Arc::clone(&self.state.event_hub)
    }

    pub(crate) fn lifecycle_state(&self) -> Arc<NativeOnlineContextState> {
        Arc::clone(&self.state)
    }
}

// SAFETY: this object contains no V8 or CppGC pointers. Its native context is
// owned through Arc and its lifecycle observer owns all terminal accounting.
unsafe impl GarbageCollected for NativePhase0OnlineContext {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativePhase0OnlineContext"
    }
}

fn map_host_limit(error: &AudioHostLimitError) -> JsErrorBox {
    JsErrorBox::range_error(format!("AudioHost limit rejected AudioContext: {error}"))
}

fn map_build_error(error: &AudioContextBuildError) -> JsErrorBox {
    let prefix = match error.kind() {
        AudioContextBuildErrorKind::InvalidConfiguration => "invalid audio output configuration",
        AudioContextBuildErrorKind::OutputRejected => "audio output was rejected",
        AudioContextBuildErrorKind::OutputPanicked => "audio output panicked",
        AudioContextBuildErrorKind::BootstrapFailed => "audio graph bootstrap failed",
        AudioContextBuildErrorKind::StartFailed => "audio output failed to start",
        AudioContextBuildErrorKind::LifecycleUnavailable => "audio lifecycle is unavailable",
        _ => "audio context construction failed",
    };
    JsErrorBox::generic(format!("{prefix}: {error}"))
}

fn abandon_committed(
    mut host_permit: AudioHostOnlineContextPermit,
    mut isolate_permit: IsolateOnlineContextPermit,
) {
    host_permit.commit();
    isolate_permit.commit();
    drop(host_permit);
    drop(isolate_permit);
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    reason = "both requested and configured rates are range-checked below 768001, where integral values are exactly representable as f32"
)]
fn exact_context_options(
    runtime: &AudioRuntimeState,
    sample_rate: f64,
    sink_id: Option<String>,
    latency_hint: Option<u32>,
    custom_latency: Option<f64>,
) -> Result<AudioContextOptions, JsErrorBox> {
    let sample_rate = if sample_rate == 0.0 {
        match runtime.limits().sample_rate() {
            None | Some(768_000..) => None,
            Some(limit) if limit >= 3_000 => Some(limit.min(48_000) as f32),
            Some(_) => {
                return Err(JsErrorBox::range_error(
                    "configured online sample-rate limit admits no Web Audio rate",
                ));
            }
        }
    } else {
        Some(sample_rate as f32)
    };
    let latency_hint = match latency_hint.unwrap_or(1) {
        0 => AudioContextLatencyCategory::Balanced,
        1 => AudioContextLatencyCategory::Interactive,
        2 => AudioContextLatencyCategory::Playback,
        3 => {
            let value = custom_latency.ok_or_else(|| {
                JsErrorBox::type_error("custom latencyHint is missing its numeric value")
            })?;
            if !value.is_finite() || value < 0.0 {
                return Err(JsErrorBox::range_error(
                    "custom latencyHint must be finite and non-negative",
                ));
            }
            AudioContextLatencyCategory::Custom(value)
        }
        _ => return Err(JsErrorBox::type_error("unknown AudioContext latencyHint")),
    };
    Ok(AudioContextOptions {
        latency_hint,
        sample_rate,
        // The private Phase 0 constructor historically selected the silent
        // endpoint. The public facade always supplies its WebIDL default "".
        sink_id: sink_id.unwrap_or_else(|| "none".to_owned()),
        ..AudioContextOptions::default()
    })
}

fn validate_online_dimensions(
    runtime: &AudioRuntimeState,
    sample_rate: f64,
) -> Result<(), JsErrorBox> {
    if sample_rate != 0.0 && !(3_000.0..=768_000.0).contains(&sample_rate) {
        return Err(JsErrorBox::range_error(
            "sampleRate must be in the inclusive range 3000..=768000",
        ));
    }
    if runtime
        .limits()
        .sample_rate()
        .is_some_and(|limit| sample_rate != 0.0 && sample_rate > f64::from(limit))
    {
        return Err(JsErrorBox::range_error(
            "configured online sample-rate limit exceeded",
        ));
    }
    if runtime.limits().channels().is_some_and(|limit| limit < 2) {
        return Err(JsErrorBox::range_error(
            "configured channel limit does not admit stereo online output",
        ));
    }
    Ok(())
}

fn channel_count_mode(value: u32) -> Result<ChannelCountMode, JsErrorBox> {
    match value {
        0 => Ok(ChannelCountMode::Max),
        1 => Ok(ChannelCountMode::ClampedMax),
        2 => Ok(ChannelCountMode::Explicit),
        _ => Err(JsErrorBox::type_error("unknown channelCountMode")),
    }
}

fn channel_interpretation(value: u32) -> Result<ChannelInterpretation, JsErrorBox> {
    match value {
        0 => Ok(ChannelInterpretation::Speakers),
        1 => Ok(ChannelInterpretation::Discrete),
        _ => Err(JsErrorBox::type_error("unknown channelInterpretation")),
    }
}

fn oscillator_type(value: u32) -> Result<OscillatorType, JsErrorBox> {
    match value {
        0 => Ok(OscillatorType::Sine),
        1 => Ok(OscillatorType::Square),
        2 => Ok(OscillatorType::Sawtooth),
        3 => Ok(OscillatorType::Triangle),
        4 => Ok(OscillatorType::Custom),
        _ => Err(JsErrorBox::generic(
            "InvalidStateError: unknown oscillator type",
        )),
    }
}

fn install_state_wake_handler(
    context: &AudioContext,
    event_hub: &Arc<ContextEventHub>,
    lifecycle_requests: &Arc<LifecycleRequestCoordinator>,
) -> Result<(), JsErrorBox> {
    let weak_hub = Arc::downgrade(event_hub);
    let weak_lifecycle_requests = Arc::downgrade(lifecycle_requests);
    panic::catch_unwind(AssertUnwindSafe(|| {
        use web_audio_api::context::BaseAudioContext as _;
        context.set_onstatechange(move |_| {
            if let Some(hub) = weak_hub.upgrade() {
                hub.enqueue_state_wake();
            }
            if let Some(coordinator) = weak_lifecycle_requests.upgrade() {
                coordinator.wake_retry();
            }
        });
    }))
    .map_err(|payload| {
        std::mem::forget(payload);
        JsErrorBox::generic("state-handler installation panicked and poisoned its AudioContext")
    })
}

fn runtime_resources(state: &OpState) -> (Arc<AudioRuntimeState>, Arc<NativeAudioByteBudget>) {
    (
        Arc::clone(state.borrow::<Arc<AudioRuntimeState>>()),
        Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>()),
    )
}

fn publish_online_context(
    context: AudioContext,
    runtime: &Arc<AudioRuntimeState>,
    byte_budget: Arc<NativeAudioByteBudget>,
    observer: (
        mpsc::SyncSender<LifecycleObservation>,
        thread::JoinHandle<()>,
    ),
    permits: (AudioHostOnlineContextPermit, IsolateOnlineContextPermit),
    mut lifecycle_lease: Option<AudioLifecycleLease>,
) -> Result<NativePhase0OnlineContext, JsErrorBox> {
    // Successful build means a physical or logical endpoint may already exist. From this point
    // onward, every unwind must retain accounting unless the observer sees confirmed shutdown.
    let (observer_sender, observer_join) = observer;
    let (mut host_permit, mut isolate_permit) = permits;
    host_permit.commit();
    isolate_permit.commit();
    if let Some(lease) = &mut lifecycle_lease {
        lease.commit();
    }

    let Ok(receipt) = context.shutdown_receipt() else {
        drop(observer_sender);
        let _ = observer_join.join();
        drop(context);
        drop(host_permit);
        drop(isolate_permit);
        return Err(JsErrorBox::generic(
            "hosted AudioContext did not expose a shutdown receipt",
        ));
    };
    let event_hub = Arc::new(ContextEventHub::new(Arc::clone(runtime)));
    let lifecycle_requests = LifecycleRequestCoordinator::new(runtime.limits().lifecycle_waiters());
    if observer_sender
        .send(LifecycleObservation {
            receipt,
            host_permit,
            isolate_permit,
            event_hub: Some(Arc::clone(&event_hub)),
            lifecycle_requests: Some(Arc::downgrade(&lifecycle_requests)),
            lifecycle_lease,
        })
        .is_err()
    {
        drop(context);
        drop(observer_join);
        return Err(JsErrorBox::generic(
            "AudioContext lifecycle observer terminated before ownership transfer",
        ));
    }
    drop(observer_join);

    if let Err(error) = install_state_wake_handler(&context, &event_hub, &lifecycle_requests) {
        drop(context);
        return Err(error);
    }

    let state = Arc::new(NativeOnlineContextState {
        context: Mutex::new(Some(Arc::new(context))),
        event_hub,
        lifecycle_requests,
    });
    let graph = OnlineGraph::new(Arc::clone(&state), Arc::clone(runtime), byte_budget);
    Ok(NativePhase0OnlineContext { state, graph })
}

#[op2]
impl NativePhase0OnlineContext {
    #[constructor]
    #[cppgc]
    fn new(
        state: &mut OpState,
        sample_rate: f64,
        initially_suspended: bool,
        #[string] sink_id: Option<String>,
        latency_hint: Option<u32>,
        custom_latency: Option<f64>,
    ) -> Result<NativePhase0OnlineContext, JsErrorBox> {
        let (runtime, byte_budget) = runtime_resources(state);
        validate_online_dimensions(&runtime, sample_rate)?;
        let options =
            exact_context_options(&runtime, sample_rate, sink_id, latency_hint, custom_latency)?;
        runtime.permissions.check_playback("AudioContext")?;
        let mut lifecycle_lease = runtime.try_admit_lifecycle_observer()?;
        let isolate_permit = runtime.try_reserve_online_context()?;
        let host_permit = runtime
            .host
            .try_reserve_online_context()
            .map_err(|error| map_host_limit(&error))?;
        let initial_control_reservation =
            AudioControlBatchReservation::new(runtime.try_reserve_control_commands(
                web_audio_api::context::AudioContextBuilder::INITIAL_GRAPH_CONTROL_COMMAND_COUNT,
            )?);
        let (observer_sender, observer_join) = spawn_lifecycle_observer()?;

        let build = panic::catch_unwind(AssertUnwindSafe(|| {
            AudioContext::builder(Arc::clone(&runtime.output))
                .options(options)
                .number_of_channels(2)
                .initially_suspended(initially_suspended)
                .diagnostic_label("deno_audio online context")
                .initial_control_reservation(initial_control_reservation)
                .build()
        }));

        let context = match build {
            Ok(Ok(context)) => context,
            Ok(Err(error)) => {
                if let Some(receipt) = error.cleanup_receipt() {
                    let mut host_permit = host_permit;
                    let mut isolate_permit = isolate_permit;
                    host_permit.commit();
                    isolate_permit.commit();
                    if let Some(lease) = &mut lifecycle_lease {
                        lease.commit();
                    }
                    if observer_sender
                        .send(LifecycleObservation {
                            receipt,
                            host_permit,
                            isolate_permit,
                            event_hub: None,
                            lifecycle_requests: None,
                            lifecycle_lease,
                        })
                        .is_err()
                    {
                        // SendError drops its committed payload fail closed.
                    }
                    drop(observer_join);
                } else {
                    drop(observer_sender);
                    let _ = observer_join.join();
                }
                return Err(map_build_error(&error));
            }
            Err(payload) => {
                std::mem::forget(payload);
                drop(observer_sender);
                let _ = observer_join.join();
                abandon_committed(host_permit, isolate_permit);
                if let Some(mut lease) = lifecycle_lease {
                    lease.commit();
                }
                return Err(JsErrorBox::generic(
                    "AudioContext construction panicked and was quarantined",
                ));
            }
        };

        publish_online_context(
            context,
            &runtime,
            byte_budget,
            (observer_sender, observer_join),
            (host_permit, isolate_permit),
            lifecycle_lease,
        )
    }

    #[getter]
    fn sample_rate(&self) -> Result<f64, JsErrorBox> {
        use web_audio_api::context::BaseAudioContext as _;
        Ok(f64::from(self.state.context()?.sample_rate()))
    }

    #[getter]
    fn current_time(&self) -> Result<f64, JsErrorBox> {
        use web_audio_api::context::BaseAudioContext as _;
        Ok(self.state.context()?.current_time())
    }

    #[getter]
    #[smi]
    fn context_state(&self) -> Result<u32, JsErrorBox> {
        use web_audio_api::context::BaseAudioContext as _;
        match self.state.context()?.state() {
            AudioContextState::Suspended => Ok(0),
            AudioContextState::Running => Ok(1),
            AudioContextState::Closed => Ok(2),
        }
    }

    #[getter]
    #[smi]
    fn rooted_source_count(&self) -> u32 {
        u32::try_from(self.state.event_hub.rooted_source_count())
            .expect("bounded scheduled-source count fits u32")
    }

    #[getter]
    fn base_latency(&self) -> Result<f64, JsErrorBox> {
        Ok(self.state.context()?.base_latency())
    }

    #[getter]
    fn output_latency(&self) -> Result<f64, JsErrorBox> {
        Ok(self.state.context()?.output_latency())
    }

    #[getter]
    #[string]
    fn sink_id(&self) -> Result<String, JsErrorBox> {
        Ok(self.state.context()?.sink_id())
    }

    #[getter]
    #[smi]
    fn number_of_channels(&self) -> u32 {
        2
    }

    #[getter]
    fn event_pump_refed(&self) -> bool {
        self.graph.event_pump_refed()
    }

    #[cppgc]
    fn destination(&self) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.destination()
    }

    #[cppgc]
    fn create_file_source(
        &self,
        state: &mut OpState,
        #[string] source: &str,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_file_source(state, source)
    }

    #[cppgc]
    fn create_gain(
        &self,
        gain: f32,
        channel_count: u32,
        count_mode: u32,
        interpretation: u32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_gain(
            gain,
            channel_count as usize,
            channel_count_mode(count_mode)?,
            channel_interpretation(interpretation)?,
        )
    }

    #[cppgc]
    fn create_constant_source(&self, offset: f32) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_constant_source(offset)
    }

    #[cppgc]
    fn create_periodic_wave(
        &self,
        #[buffer] real: &[f32],
        real_present: bool,
        #[buffer] imag: &[f32],
        imag_present: bool,
        disable_normalization: bool,
    ) -> Result<NativePeriodicWave, JsErrorBox> {
        self.graph.create_periodic_wave(
            real_present.then_some(real),
            imag_present.then_some(imag),
            disable_normalization,
        )
    }

    #[cppgc]
    fn create_oscillator(
        &self,
        oscillator_type_value: u32,
        #[cppgc] periodic_wave: Option<&NativePeriodicWave>,
        frequency: f32,
        detune: f32,
        channel_count: u32,
        count_mode: u32,
        interpretation: u32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_oscillator(
            oscillator_type(oscillator_type_value)?,
            periodic_wave,
            frequency,
            detune,
            channel_count as usize,
            channel_count_mode(count_mode)?,
            channel_interpretation(interpretation)?,
        )
    }

    #[cppgc]
    #[allow(clippy::too_many_arguments)]
    fn create_buffer_source(
        &self,
        detune: f32,
        loop_: bool,
        loop_start: f64,
        loop_end: f64,
        playback_rate: f32,
        channel_count: u32,
        count_mode: u32,
        interpretation: u32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_buffer_source(
            detune,
            loop_,
            loop_start,
            loop_end,
            playback_rate,
            channel_count as usize,
            channel_count_mode(count_mode)?,
            channel_interpretation(interpretation)?,
        )
    }
}

fn try_request_state_change(
    state: &Arc<NativeOnlineContextState>,
    suspend: bool,
) -> Result<Result<AudioContextStateChangeReceipt, AudioContextLifecycleError>, JsErrorBox> {
    let context = state.context()?;
    let request = panic::catch_unwind(AssertUnwindSafe(|| {
        if suspend {
            context.request_suspend()
        } else {
            context.request_resume()
        }
    }));
    match request {
        Ok(result) => Ok(result),
        Err(payload) => {
            std::mem::forget(payload);
            state.poison();
            Err(JsErrorBox::generic(
                "AudioContext state transition panicked and poisoned the context",
            ))
        }
    }
}

fn lifecycle_closed_error() -> JsErrorBox {
    JsErrorBox::generic("InvalidStateError: AudioContext shutdown has started")
}

fn map_lifecycle_error(error: AudioContextLifecycleError) -> JsErrorBox {
    JsErrorBox::generic(format!("AudioContext lifecycle request failed: {error:?}"))
}

async fn retry_lifecycle_request<T>(
    coordinator: &LifecycleRequestCoordinator,
    mut request: impl FnMut() -> Result<Result<T, AudioContextLifecycleError>, JsErrorBox>,
) -> Result<T, JsErrorBox> {
    loop {
        if coordinator.is_closing() {
            return Err(lifecycle_closed_error());
        }
        match request()? {
            Ok(value) => return Ok(value),
            Err(AudioContextLifecycleError::Contended | AudioContextLifecycleError::Capacity) => {
                if coordinator.is_closing() {
                    return Err(lifecycle_closed_error());
                }
                // These immediate native failures retain no command credits. The sole Deno
                // leader sleeps until a state transition, terminal observation, or Close emits
                // an explicit wake; queued Deno callers remain parked on the FIFO mutex.
                coordinator.wait_to_retry().await;
            }
            Err(error) => return Err(map_lifecycle_error(error)),
        }
    }
}

async fn observe_state_change(
    state: Arc<NativeOnlineContextState>,
    receipt: AudioContextStateChangeReceipt,
) -> Result<u32, JsErrorBox> {
    match AssertUnwindSafe(receipt).catch_unwind().await {
        Ok(AudioContextStateChangeOutcome::Applied) => Ok(0),
        Ok(AudioContextStateChangeOutcome::Unchanged) => Ok(1),
        Ok(AudioContextStateChangeOutcome::SupersededByShutdown) => Ok(2),
        Ok(AudioContextStateChangeOutcome::Closed) => {
            Err(JsErrorBox::generic("AudioContext is closed"))
        }
        Ok(AudioContextStateChangeOutcome::TransportFailed) => {
            state.poison();
            Err(JsErrorBox::generic("AudioContext control transport failed"))
        }
        Ok(AudioContextStateChangeOutcome::EventDeliveryFailed) => Ok(4),
        Ok(AudioContextStateChangeOutcome::EndpointUncertain) => {
            state.poison();
            Err(JsErrorBox::generic(
                "AudioContext output endpoint became uncertain",
            ))
        }
        Ok(AudioContextStateChangeOutcome::WorkerFailed) => {
            state.poison();
            Err(JsErrorBox::generic("AudioContext lifecycle worker failed"))
        }
        Ok(_) => {
            state.poison();
            Err(JsErrorBox::generic(
                "AudioContext returned an unknown state transition outcome",
            ))
        }
        Err(payload) => {
            std::mem::forget(payload);
            state.poison();
            Err(JsErrorBox::generic(
                "AudioContext state acknowledgement panicked and poisoned the context",
            ))
        }
    }
}

async fn request_and_observe_state_change(
    state: Arc<NativeOnlineContextState>,
    suspend: bool,
) -> Result<u32, JsErrorBox> {
    let coordinator = Arc::clone(&state.lifecycle_requests);
    if coordinator.is_closing() {
        return Err(lifecycle_closed_error());
    }
    let _waiter = coordinator.try_waiter()?;
    let receipt = {
        let mut queue = coordinator.serial.lock().await;
        loop {
            if coordinator.is_closing() {
                return Err(lifecycle_closed_error());
            }
            if let Some(pending) = queue.pending.clone() {
                let observation = observe_state_change(Arc::clone(&state), pending).await;
                queue.pending = None;
                observation?;
                continue;
            }
            let receipt =
                retry_lifecycle_request(&coordinator, || try_request_state_change(&state, suspend))
                    .await?;
            queue.pending = Some(receipt.clone());
            break receipt;
        }
    };
    observe_state_change(state, receipt).await
}

#[op2]
#[smi]
pub(crate) async fn op_phase0_online_suspend(
    #[cppgc] context: &NativePhase0OnlineContext,
) -> Result<u32, JsErrorBox> {
    let state = Arc::clone(&context.state);
    request_and_observe_state_change(state, true).await
}

#[op2]
#[smi]
pub(crate) async fn op_phase0_online_resume(
    #[cppgc] context: &NativePhase0OnlineContext,
) -> Result<u32, JsErrorBox> {
    let state = Arc::clone(&context.state);
    request_and_observe_state_change(state, false).await
}

#[op2]
pub(crate) async fn op_phase0_online_close(
    #[cppgc] context: &NativePhase0OnlineContext,
) -> Result<bool, JsErrorBox> {
    let state = Arc::clone(&context.state);
    state.lifecycle_requests.begin_close();
    let native = state.context()?;
    let request = panic::catch_unwind(AssertUnwindSafe(|| native.request_close()));
    let receipt = match request {
        Ok(Ok(receipt)) => receipt,
        Ok(Err(error)) => return Err(map_lifecycle_error(error)),
        Err(payload) => {
            std::mem::forget(payload);
            state.poison();
            return Err(JsErrorBox::generic(
                "AudioContext close panicked and poisoned the context",
            ));
        }
    };
    drop(native);
    match AssertUnwindSafe(receipt).catch_unwind().await {
        Ok(AudioContextShutdownOutcome::Confirmed(_)) => Ok(true),
        Ok(AudioContextShutdownOutcome::Unconfirmed { .. }) => Err(JsErrorBox::generic(
            "AudioContext shutdown could not be confirmed",
        )),
        Ok(AudioContextShutdownOutcome::ControllerTerminated) => Err(JsErrorBox::generic(
            "AudioContext lifecycle controller terminated",
        )),
        Ok(_) => Err(JsErrorBox::generic(
            "AudioContext returned an unknown shutdown outcome",
        )),
        Err(payload) => {
            std::mem::forget(payload);
            state.poison();
            Err(JsErrorBox::generic(
                "AudioContext shutdown acknowledgement panicked",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn wait_for_attempts(attempts: &AtomicUsize, expected: usize) {
        for _ in 0..64 {
            if attempts.load(Ordering::Acquire) == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
        panic!("lifecycle retry loop did not reach attempt {expected}");
    }

    #[test]
    fn isolate_permit_rolls_back_then_retains_after_commit() {
        let runtime = Arc::new(AudioRuntimeState::new(
            Arc::new(AudioHost::default()),
            Arc::new(crate::AllowAllAudioPermissions),
            AudioLimits::default().max_online_contexts(Some(1)),
            Arc::new(web_audio_api::output::SystemAudioOutput::new()),
            AudioEventPumpLiveness::Unref,
            None,
        ));
        drop(runtime.try_reserve_online_context().unwrap());
        assert_eq!(runtime.online_contexts(), 0);

        let mut permit = runtime.try_reserve_online_context().unwrap();
        permit.commit();
        drop(permit);
        assert_eq!(runtime.online_contexts(), 1);
        assert!(runtime.try_reserve_online_context().is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn transient_lifecycle_failures_require_explicit_wakes() {
        let coordinator = LifecycleRequestCoordinator::new(Some(4));
        let attempts = Arc::new(AtomicUsize::new(0));
        let request_attempts = Arc::clone(&attempts);
        let request = retry_lifecycle_request(&coordinator, move || {
            let attempt = request_attempts.fetch_add(1, Ordering::AcqRel);
            Ok(match attempt {
                0 => Err(AudioContextLifecycleError::Contended),
                1 => Err(AudioContextLifecycleError::Capacity),
                _ => Ok(7_u8),
            })
        });
        let drive = async {
            wait_for_attempts(&attempts, 1).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(attempts.load(Ordering::Acquire), 1, "retry loop spun");
            coordinator.wake_retry();

            wait_for_attempts(&attempts, 2).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(attempts.load(Ordering::Acquire), 2, "retry loop spun");
            coordinator.wake_retry();
        };
        let (result, ()) = tokio::join!(request, drive);
        assert_eq!(result.unwrap(), 7);
        assert_eq!(attempts.load(Ordering::Acquire), 3);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn close_wakes_and_dominates_transient_lifecycle_retry() {
        let coordinator = LifecycleRequestCoordinator::new(Some(4));
        let attempts = AtomicUsize::new(0);
        let request = retry_lifecycle_request(&coordinator, || {
            attempts.fetch_add(1, Ordering::AcqRel);
            Ok::<_, JsErrorBox>(Err::<(), _>(AudioContextLifecycleError::Contended))
        });
        let close = async {
            wait_for_attempts(&attempts, 1).await;
            coordinator.begin_close();
        };
        let (result, ()) = tokio::join!(request, close);
        let error = result.expect_err("Close must reject a transient retry");
        assert!(error.to_string().contains("InvalidStateError"));
        assert_eq!(attempts.load(Ordering::Acquire), 1);
    }
}
