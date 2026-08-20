use std::collections::HashMap;
use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use deno_core::{op2, v8, GarbageCollected};
use deno_error::JsErrorBox;
use web_audio_api::context::{
    AudioContext, AudioControlBatchReservation, AudioControlBatchReservationProvider,
    AudioExplicitConnectionReservation, AudioExplicitConnectionReservationProvider,
    AudioGraphConnectionReservation, AudioNodeLifetimeReservation, BaseAudioContext,
    OfflineAudioContext,
};
use web_audio_api::node::{
    AudioBufferSourceNode, AudioBufferSourceOptions, AudioDestinationNode, AudioNode,
    AudioNodeDisconnectSelector, AudioNodeOptions, AudioScheduledSourceNode,
    AudioScheduledSourceNodeExt, ChannelCountMode, ChannelInterpretation, ConstantSourceNode,
    ConstantSourceOptions, GainNode, GainOptions, OscillatorNode, OscillatorOptions,
    OscillatorType,
};
use web_audio_api::{
    AudioParam, AutomationRate, PeriodicWave, PeriodicWaveOptions, PeriodicWaveStorageLease,
    MAX_PERIODIC_WAVE_COMPONENTS, PERIODIC_WAVE_TABLE_BYTES,
};

use crate::buffer::{NativeAudioBuffer, NativeAudioByteBudget, NativeAudioByteReservation};
use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::online::{
    AudioRuntimeState, ControlCommandReservations, IsolateReservation, NativeOnlineContextState,
};
use crate::scheduled::ScheduledSourceRoot;

const MAGIC_NODE_COUNT: usize = 11;
const MAGIC_COMMAND_COUNT: usize = 21;
const GAIN_NODE_COUNT: usize = 2;
const GAIN_COMMAND_COUNT_WITH_TEARDOWN: usize = 6;
const OSCILLATOR_NODE_COUNT: usize = 3;
const OSCILLATOR_COMMAND_COUNT_WITH_TEARDOWN: usize = 10;
const CONSTANT_SOURCE_NODE_COUNT: usize = 2;
const CONSTANT_SOURCE_COMMAND_COUNT_WITH_TEARDOWN: usize = 6;
const EXACT_GAIN_NODE_COUNT: usize = 2;
const EXACT_GAIN_CONSTRUCTION_COMMAND_COUNT: usize = 4;
const BUFFER_SOURCE_NODE_COUNT: usize = 3;
const BUFFER_SOURCE_CONSTRUCTION_COMMAND_COUNT: usize = 7;
const EXACT_BUFFER_SOURCE_OPTION_COMMAND_LIMIT: usize = 5;
const MAX_AUTOMATION_CURVE_SAMPLES: usize = 1_048_576;

fn host_reservation(
    runtime: &AudioRuntimeState,
    resource: AudioHostResourceKind,
    amount: usize,
    name: &'static str,
) -> Result<AudioHostReservation, JsErrorBox> {
    runtime
        .host()
        .try_reserve(resource, amount)
        .map_err(|error| {
            JsErrorBox::range_error(format!("AudioHost {name} limit exceeded: {error}"))
        })
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        std::mem::forget(payload);
        "native Web Audio operation panicked".to_owned()
    }
}

fn mutex<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn validate_periodic_wave_coefficients(
    real: Option<&[f32]>,
    imag: Option<&[f32]>,
) -> Result<usize, JsErrorBox> {
    if let (Some(real), Some(imag)) = (real, imag) {
        if real.len() != imag.len() {
            return Err(JsErrorBox::generic(
                "IndexSizeError: PeriodicWave real and imag lengths must match",
            ));
        }
    }
    for coefficients in [real, imag].into_iter().flatten() {
        if coefficients.len() < 2 {
            return Err(JsErrorBox::generic(
                "IndexSizeError: PeriodicWave needs at least two coefficients",
            ));
        }
        if coefficients.len() > MAX_PERIODIC_WAVE_COMPONENTS {
            return Err(JsErrorBox::generic(format!(
                "NotSupportedError: PeriodicWave supports at most {MAX_PERIODIC_WAVE_COMPONENTS} coefficients"
            )));
        }
        if coefficients.iter().any(|value| !value.is_finite()) {
            return Err(JsErrorBox::type_error(
                "PeriodicWave coefficients must be finite",
            ));
        }
    }
    Ok(real.or(imag).map_or(2, <[f32]>::len))
}

fn create_periodic_wave<C: BaseAudioContext>(
    context: &C,
    byte_budget: &Arc<NativeAudioByteBudget>,
    real: Option<&[f32]>,
    imag: Option<&[f32]>,
    disable_normalization: bool,
) -> Result<PeriodicWave, JsErrorBox> {
    let coefficient_count = validate_periodic_wave_coefficients(real, imag)?;
    let temporary_bytes = coefficient_count
        .checked_mul(2)
        .and_then(|samples| samples.checked_mul(size_of::<f32>()))
        .ok_or_else(|| JsErrorBox::range_error("PeriodicWave coefficient byte count overflow"))?;
    let persistent = byte_budget
        .reserve(PERIODIC_WAVE_TABLE_BYTES)
        .map_err(|error| {
            JsErrorBox::range_error(format!("PeriodicWave table reservation failed: {error}"))
        })?;
    let temporary = byte_budget.reserve(temporary_bytes).map_err(|error| {
        JsErrorBox::range_error(format!(
            "PeriodicWave coefficient reservation failed: {error}"
        ))
    })?;
    let options = PeriodicWaveOptions {
        real: real.map(<[f32]>::to_vec),
        imag: imag.map(<[f32]>::to_vec),
        disable_normalization,
    };
    let wave = panic::catch_unwind(AssertUnwindSafe(|| {
        PeriodicWave::new_with_storage_lease(
            context,
            options,
            PeriodicWaveStorageLease::new(persistent),
        )
    }))
    .map_err(|payload| JsErrorBox::generic(panic_message(payload)))?;
    drop(temporary);
    Ok(wave)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Edge {
    source: u64,
    destination: u64,
    output: usize,
    input: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OfflineGraphPhase {
    Open,
    Rendering,
    Closed,
    Poisoned,
}

struct NodeReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
    _scheduled_isolate: Option<IsolateReservation>,
    _scheduled_host: Option<AudioHostReservation>,
}

struct ConnectionReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
}

struct OnlineGraphNodeReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
}

struct AutomationReservations {
    _isolate: IsolateReservation,
    _host: AudioHostReservation,
    _bytes: Option<NativeAudioByteReservation>,
}

struct OfflineGraphInner {
    phase: OfflineGraphPhase,
    next_node_id: u64,
    node_reservations: Vec<NodeReservations>,
    control_reservations: Vec<ControlCommandReservations>,
    connection_reservations: HashMap<Edge, ConnectionReservations>,
    automation_reservations: Vec<AutomationReservations>,
}

/// Resource-accounted ownership for one public offline graph.
///
/// The upstream offline renderer uses its legacy, unbounded control channel.
/// This wrapper reserves every command before the corresponding mutation and
/// retains the reservation until rendering consumes the queue or the entire
/// graph is destroyed.
pub(crate) struct OfflineGraph {
    // Drop the upstream context and renderer before releasing the ledger.
    context: Arc<OfflineAudioContext>,
    runtime: Arc<AudioRuntimeState>,
    byte_budget: Arc<NativeAudioByteBudget>,
    inner: Mutex<OfflineGraphInner>,
}

impl OfflineGraph {
    pub(crate) fn new(
        runtime: Arc<AudioRuntimeState>,
        byte_budget: Arc<NativeAudioByteBudget>,
        number_of_channels: usize,
        length: usize,
        sample_rate: f32,
    ) -> Result<Arc<Self>, JsErrorBox> {
        let isolate_nodes = runtime.try_reserve_graph_nodes(MAGIC_NODE_COUNT)?;
        let host_nodes = host_reservation(
            &runtime,
            AudioHostResourceKind::GraphNodes,
            MAGIC_NODE_COUNT,
            "graph-node",
        )?;
        let commands = runtime.try_reserve_control_commands(MAGIC_COMMAND_COUNT)?;
        let context = panic::catch_unwind(AssertUnwindSafe(|| {
            OfflineAudioContext::new(number_of_channels, length, sample_rate)
        }))
        .map_err(|payload| {
            JsErrorBox::generic(format!(
                "offline audio context construction failed: {}",
                panic_message(payload)
            ))
        })?;

        Ok(Arc::new(Self {
            context: Arc::new(context),
            runtime,
            byte_budget,
            inner: Mutex::new(OfflineGraphInner {
                phase: OfflineGraphPhase::Open,
                next_node_id: 1,
                node_reservations: vec![NodeReservations {
                    _isolate: isolate_nodes,
                    _host: host_nodes,
                    _scheduled_isolate: None,
                    _scheduled_host: None,
                }],
                control_reservations: vec![commands],
                connection_reservations: HashMap::new(),
                automation_reservations: Vec::new(),
            }),
        }))
    }

    pub(crate) fn context(&self) -> &Arc<OfflineAudioContext> {
        &self.context
    }

    pub(crate) fn state_code(&self) -> u32 {
        match mutex(&self.inner).phase {
            OfflineGraphPhase::Open => 0,
            OfflineGraphPhase::Rendering => 1,
            OfflineGraphPhase::Closed | OfflineGraphPhase::Poisoned => 2,
        }
    }

    fn ensure_open(inner: &OfflineGraphInner) -> Result<(), JsErrorBox> {
        match inner.phase {
            OfflineGraphPhase::Open => Ok(()),
            OfflineGraphPhase::Rendering => Err(JsErrorBox::generic(
                "InvalidStateError: offline rendering has already started",
            )),
            OfflineGraphPhase::Closed => Err(JsErrorBox::generic(
                "InvalidStateError: offline audio context is closed",
            )),
            OfflineGraphPhase::Poisoned => Err(JsErrorBox::generic(
                "InvalidStateError: offline audio graph is terminally degraded",
            )),
        }
    }

    fn reserve_nodes(
        &self,
        inner: &mut OfflineGraphInner,
        nodes: usize,
        commands: usize,
        scheduled: bool,
    ) -> Result<u64, JsErrorBox> {
        Self::ensure_open(inner)?;
        let next = inner
            .next_node_id
            .checked_add(u64::try_from(nodes).map_err(|_| {
                JsErrorBox::range_error("offline graph node identity count overflowed")
            })?)
            .ok_or_else(|| {
                JsErrorBox::range_error("offline graph node identity space exhausted")
            })?;
        let isolate = self.runtime.try_reserve_graph_nodes(nodes)?;
        let host = host_reservation(
            &self.runtime,
            AudioHostResourceKind::GraphNodes,
            nodes,
            "graph-node",
        )?;
        let control = self.runtime.try_reserve_control_commands(commands)?;
        let scheduled_isolate = scheduled
            .then(|| self.runtime.try_reserve_scheduled_source())
            .transpose()?;
        let scheduled_host = scheduled
            .then(|| {
                host_reservation(
                    &self.runtime,
                    AudioHostResourceKind::ScheduledSources,
                    1,
                    "scheduled-source",
                )
            })
            .transpose()?;

        let first = inner.next_node_id;
        inner.next_node_id = next;
        inner.node_reservations.push(NodeReservations {
            _isolate: isolate,
            _host: host,
            _scheduled_isolate: scheduled_isolate,
            _scheduled_host: scheduled_host,
        });
        inner.control_reservations.push(control);
        Ok(first)
    }

    fn reserve_command(
        &self,
        inner: &mut OfflineGraphInner,
        amount: usize,
    ) -> Result<(), JsErrorBox> {
        Self::ensure_open(inner)?;
        inner
            .control_reservations
            .push(self.runtime.try_reserve_control_commands(amount)?);
        Ok(())
    }

    fn reserve_automation(
        &self,
        native_bytes: usize,
    ) -> Result<AutomationReservations, JsErrorBox> {
        let isolate = self.runtime.try_reserve_automation_event()?;
        let host = host_reservation(
            &self.runtime,
            AudioHostResourceKind::AutomationEvents,
            1,
            "automation-event",
        )?;
        let bytes = (native_bytes != 0)
            .then(|| self.byte_budget.reserve(native_bytes))
            .transpose()
            .map_err(|error| {
                JsErrorBox::range_error(format!(
                    "AudioParam automation byte reservation failed: {error}"
                ))
            })?;
        Ok(AutomationReservations {
            _isolate: isolate,
            _host: host,
            _bytes: bytes,
        })
    }

    fn run_command<T>(
        &self,
        automation: bool,
        native_bytes: usize,
        operation: impl FnOnce() -> T,
    ) -> Result<T, JsErrorBox> {
        let mut inner = mutex(&self.inner);
        Self::ensure_open(&inner)?;
        let control = self.runtime.try_reserve_control_commands(1)?;
        let automation = automation
            .then(|| self.reserve_automation(native_bytes))
            .transpose()?;
        inner.control_reservations.push(control);
        inner.automation_reservations.extend(automation);
        match panic::catch_unwind(AssertUnwindSafe(operation)) {
            Ok(value) => Ok(value),
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                Err(JsErrorBox::generic(panic_message(payload)))
            }
        }
    }

    /// Reserve one legacy control command before acquiring an owned payload.
    /// Acquisition may detach V8 storage, so it must not run until bounded
    /// command capacity has been secured.
    fn run_prepared_command<T>(
        &self,
        prepare: impl FnOnce() -> Result<T, JsErrorBox>,
        operation: impl FnOnce(T),
    ) -> Result<(), JsErrorBox> {
        let mut inner = mutex(&self.inner);
        Self::ensure_open(&inner)?;
        let control = self.runtime.try_reserve_control_commands(1)?;
        let prepared = match panic::catch_unwind(AssertUnwindSafe(prepare)) {
            Ok(result) => result?,
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                return Err(JsErrorBox::generic(panic_message(payload)));
            }
        };
        inner.control_reservations.push(control);
        match panic::catch_unwind(AssertUnwindSafe(|| operation(prepared))) {
            Ok(()) => Ok(()),
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                Err(JsErrorBox::generic(panic_message(payload)))
            }
        }
    }

    pub(crate) fn destination(self: &Arc<Self>) -> NativeAudioNode {
        NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Destination(self.context.destination()),
        }))
    }

    pub(crate) fn create_periodic_wave(
        self: &Arc<Self>,
        real: Option<&[f32]>,
        imag: Option<&[f32]>,
        disable_normalization: bool,
    ) -> Result<NativePeriodicWave, JsErrorBox> {
        let wave = create_periodic_wave(
            self.context.as_ref(),
            &self.byte_budget,
            real,
            imag,
            disable_normalization,
        )?;
        Ok(NativePeriodicWave {
            graph: NativeGraphIdentity::Offline(Arc::downgrade(self)),
            wave,
        })
    }

    pub(crate) fn create_gain(
        self: &Arc<Self>,
        gain: f32,
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !gain.is_finite() {
            return Err(JsErrorBox::type_error("gain must be finite"));
        }
        let mut inner = mutex(&self.inner);
        let first = self.reserve_nodes(
            &mut inner,
            GAIN_NODE_COUNT,
            GAIN_COMMAND_COUNT_WITH_TEARDOWN,
            false,
        )?;
        let options = GainOptions {
            gain,
            audio_node_options: AudioNodeOptions {
                channel_count,
                channel_count_mode: count_mode,
                channel_interpretation: interpretation,
            },
        };
        let node = match panic::catch_unwind(AssertUnwindSafe(|| {
            GainNode::new(self.context.as_ref(), options)
        })) {
            Ok(node) => node,
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                return Err(JsErrorBox::generic(panic_message(payload)));
            }
        };
        let parameter = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 1,
            kind: NativeNodeKind::Param(node.gain().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first,
            kind: NativeNodeKind::Gain { node, parameter },
        })))
    }

    pub(crate) fn create_constant_source(
        self: &Arc<Self>,
        offset: f32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !offset.is_finite() {
            return Err(JsErrorBox::type_error(
                "ConstantSource offset must be finite",
            ));
        }
        let mut inner = mutex(&self.inner);
        let first = self.reserve_nodes(
            &mut inner,
            CONSTANT_SOURCE_NODE_COUNT,
            CONSTANT_SOURCE_COMMAND_COUNT_WITH_TEARDOWN,
            true,
        )?;
        let node = match panic::catch_unwind(AssertUnwindSafe(|| {
            ConstantSourceNode::new(self.context.as_ref(), ConstantSourceOptions { offset })
        })) {
            Ok(node) => node,
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                return Err(JsErrorBox::generic(panic_message(payload)));
            }
        };
        let parameter = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 1,
            kind: NativeNodeKind::Param(node.offset().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first,
            kind: NativeNodeKind::ConstantSource {
                node: Box::new(Mutex::new(node)),
                parameter,
            },
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_oscillator(
        self: &Arc<Self>,
        oscillator_type: OscillatorType,
        periodic_wave: Option<&NativePeriodicWave>,
        frequency: f32,
        detune: f32,
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !frequency.is_finite() || !detune.is_finite() {
            return Err(JsErrorBox::type_error(
                "oscillator frequency and detune must be finite",
            ));
        }
        let periodic_wave = periodic_wave
            .map(|wave| wave.for_graph(&NativeGraph::Offline(Arc::clone(self))))
            .transpose()?;
        if oscillator_type == OscillatorType::Custom && periodic_wave.is_none() {
            return Err(JsErrorBox::generic(
                "InvalidStateError: custom oscillators require a PeriodicWave",
            ));
        }
        let oscillator_type = if periodic_wave.is_some() {
            OscillatorType::Custom
        } else {
            oscillator_type
        };
        let mut inner = mutex(&self.inner);
        let first = self.reserve_nodes(
            &mut inner,
            OSCILLATOR_NODE_COUNT,
            OSCILLATOR_COMMAND_COUNT_WITH_TEARDOWN,
            true,
        )?;
        let options = OscillatorOptions {
            type_: oscillator_type,
            frequency,
            detune,
            periodic_wave,
            audio_node_options: AudioNodeOptions {
                channel_count,
                channel_count_mode: count_mode,
                channel_interpretation: interpretation,
            },
        };
        let node = match panic::catch_unwind(AssertUnwindSafe(|| {
            OscillatorNode::new(self.context.as_ref(), options)
        })) {
            Ok(node) => node,
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                return Err(JsErrorBox::generic(panic_message(payload)));
            }
        };
        let frequency = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 1,
            kind: NativeNodeKind::Param(node.frequency().clone()),
        });
        let detune = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 2,
            kind: NativeNodeKind::Param(node.detune().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first,
            kind: NativeNodeKind::Oscillator {
                node: Box::new(Mutex::new(node)),
                frequency,
                detune,
            },
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_buffer_source(
        self: &Arc<Self>,
        detune: f32,
        loop_: bool,
        loop_start: f64,
        loop_end: f64,
        playback_rate: f32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !detune.is_finite()
            || !playback_rate.is_finite()
            || !loop_start.is_finite()
            || !loop_end.is_finite()
        {
            return Err(JsErrorBox::type_error(
                "AudioBufferSource options must be finite",
            ));
        }
        let mut inner = mutex(&self.inner);
        let first = self.reserve_nodes(
            &mut inner,
            BUFFER_SOURCE_NODE_COUNT,
            BUFFER_SOURCE_CONSTRUCTION_COMMAND_COUNT,
            true,
        )?;
        let options = AudioBufferSourceOptions {
            buffer: None,
            detune,
            loop_,
            loop_start,
            loop_end,
            playback_rate,
        };
        let node = match panic::catch_unwind(AssertUnwindSafe(|| {
            AudioBufferSourceNode::new(self.context.as_ref(), options)
        })) {
            Ok(node) => node,
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                return Err(JsErrorBox::generic(panic_message(payload)));
            }
        };
        let playback_rate = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 1,
            kind: NativeNodeKind::Param(node.playback_rate().clone()),
        });
        let detune = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first + 2,
            kind: NativeNodeKind::Param(node.detune().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Offline(Arc::clone(self)),
            id: first,
            kind: NativeNodeKind::BufferSource {
                node: Box::new(Mutex::new(node)),
                playback_rate,
                detune,
            },
        })))
    }

    fn validate_pair(
        &self,
        source: &NativeNodeHandle,
        destination: &NativeNodeHandle,
        output: usize,
        input: usize,
    ) -> Result<(), JsErrorBox> {
        if !source.graph.is_offline(self) || !destination.graph.is_offline(self) {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: nodes belong to different audio contexts",
            ));
        }
        if output >= source.number_of_outputs() || input >= destination.number_of_inputs() {
            return Err(JsErrorBox::generic(
                "IndexSizeError: audio node port is out of range",
            ));
        }
        Ok(())
    }

    fn connect(
        &self,
        source: &NativeNodeHandle,
        destination: &NativeNodeHandle,
        output: usize,
        input: usize,
    ) -> Result<(), JsErrorBox> {
        self.validate_pair(source, destination, output, input)?;
        let edge = Edge {
            source: source.id,
            destination: destination.id,
            output,
            input,
        };
        let mut inner = mutex(&self.inner);
        Self::ensure_open(&inner)?;
        if inner.connection_reservations.contains_key(&edge) {
            return Ok(());
        }
        let isolate = self.runtime.try_reserve_graph_connection()?;
        let host = host_reservation(
            &self.runtime,
            AudioHostResourceKind::GraphConnections,
            1,
            "graph-connection",
        )?;
        self.reserve_command(&mut inner, 1)?;
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            source.with_audio_node(|source_node| {
                destination.with_audio_node(|destination_node| {
                    source_node.connect_from_output_to_input(destination_node, output, input);
                });
            });
        }));
        let reservations = ConnectionReservations {
            _isolate: isolate,
            _host: host,
        };
        match result {
            Ok(()) => {
                inner.connection_reservations.insert(edge, reservations);
                Ok(())
            }
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                inner.connection_reservations.insert(edge, reservations);
                Err(JsErrorBox::generic(panic_message(payload)))
            }
        }
    }

    fn disconnect(
        &self,
        source: &NativeNodeHandle,
        destination: Option<&NativeNodeHandle>,
        output: Option<usize>,
        input: Option<usize>,
    ) -> Result<(), JsErrorBox> {
        if !source.graph.is_offline(self) {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: source belongs to a different audio context",
            ));
        }
        if let Some(destination) = destination {
            if !destination.graph.is_offline(self) {
                return Err(JsErrorBox::generic(
                    "InvalidAccessError: nodes belong to different audio contexts",
                ));
            }
        }
        if output.is_some_and(|output| output >= source.number_of_outputs()) {
            return Err(JsErrorBox::generic(
                "IndexSizeError: audio node output port is out of range",
            ));
        }
        if let (Some(destination), Some(input)) = (destination, input) {
            if input >= destination.number_of_inputs() {
                return Err(JsErrorBox::generic(
                    "IndexSizeError: audio node input port is out of range",
                ));
            }
        }

        let mut inner = mutex(&self.inner);
        Self::ensure_open(&inner)?;
        let matching: Vec<Edge> = inner
            .connection_reservations
            .keys()
            .copied()
            .filter(|edge| {
                edge.source == source.id
                    && output.is_none_or(|output| edge.output == output)
                    && destination.is_none_or(|destination| edge.destination == destination.id)
                    && input.is_none_or(|input| edge.input == input)
            })
            .collect();
        if matching.is_empty() {
            if destination.is_some() {
                return Err(JsErrorBox::generic(
                    "InvalidAccessError: the requested connection does not exist",
                ));
            }
            return Ok(());
        }
        self.reserve_command(&mut inner, matching.len())?;
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            source.with_audio_node(|source_node| match (destination, output, input) {
                (None, None, None) => source_node.disconnect(),
                (None, Some(output), None) => source_node.disconnect_output(output),
                (Some(destination), None, None) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_dest(destination_node);
                    });
                }
                (Some(destination), Some(output), None) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_dest_from_output(destination_node, output);
                    });
                }
                (Some(destination), Some(output), Some(input)) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_dest_from_output_to_input(
                            destination_node,
                            output,
                            input,
                        );
                    });
                }
                _ => unreachable!("JavaScript exposes only legal disconnect selectors"),
            });
        }));
        match result {
            Ok(()) => {
                for edge in matching {
                    inner.connection_reservations.remove(&edge);
                }
                Ok(())
            }
            Err(payload) => {
                inner.phase = OfflineGraphPhase::Poisoned;
                Err(JsErrorBox::generic(panic_message(payload)))
            }
        }
    }

    pub(crate) fn begin_render(&self) -> Result<(), JsErrorBox> {
        let mut inner = mutex(&self.inner);
        Self::ensure_open(&inner)?;
        inner.phase = OfflineGraphPhase::Rendering;
        Ok(())
    }

    pub(crate) fn finish_render(&self, successful: bool) {
        let mut inner = mutex(&self.inner);
        inner.phase = if successful {
            OfflineGraphPhase::Closed
        } else {
            OfflineGraphPhase::Poisoned
        };
        inner.connection_reservations.clear();
        inner.automation_reservations.clear();
        // JavaScript can retain the upstream host node handles after the
        // renderer has completed. Their eventual Drops enqueue teardown
        // records into the now-undrained legacy channel, so retain the node
        // and conservatively grouped construction/teardown command budgets
        // until the final graph handle itself is destroyed.
    }
}

/// Resource-accounted public graph over one exact hosted `AudioContext`.
///
/// The engine registry remains authoritative for explicit-edge identity. Host reservations are
/// acquired lazily from that serialized transaction and attached to the engine's node, edge, and
/// batch lifetimes instead of being mirrored in this binding.
pub(crate) struct OnlineGraph {
    state: Arc<NativeOnlineContextState>,
    runtime: Arc<AudioRuntimeState>,
    byte_budget: Arc<NativeAudioByteBudget>,
}

impl OnlineGraph {
    pub(crate) fn new(
        state: Arc<NativeOnlineContextState>,
        runtime: Arc<AudioRuntimeState>,
        byte_budget: Arc<NativeAudioByteBudget>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state,
            runtime,
            byte_budget,
        })
    }

    fn context(&self) -> Result<Arc<AudioContext>, JsErrorBox> {
        self.state.context()
    }

    pub(crate) fn event_pump_refed(&self) -> bool {
        self.runtime.event_pump_refed()
    }

    fn ensure_default_channel_options(
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<(), JsErrorBox> {
        if channel_count != 2
            || count_mode != ChannelCountMode::Max
            || interpretation != ChannelInterpretation::Speakers
        {
            return Err(JsErrorBox::generic(
                "NotSupportedError: exact online channel configuration is not yet mutable",
            ));
        }
        Ok(())
    }

    fn reserve_node_lifetime(
        &self,
        count: usize,
    ) -> Result<AudioNodeLifetimeReservation, JsErrorBox> {
        let isolate = self.runtime.try_reserve_graph_nodes(count)?;
        let host = host_reservation(
            &self.runtime,
            AudioHostResourceKind::GraphNodes,
            count,
            "graph-node",
        )?;
        Ok(AudioNodeLifetimeReservation::new(
            OnlineGraphNodeReservations {
                _isolate: isolate,
                _host: host,
            },
        ))
    }

    fn reserve_control(&self, count: usize) -> Result<AudioControlBatchReservation, JsErrorBox> {
        Ok(AudioControlBatchReservation::new(
            self.runtime.try_reserve_control_commands(count)?,
        ))
    }

    pub(crate) fn destination(self: &Arc<Self>) -> Result<NativeAudioNode, JsErrorBox> {
        let context = self.context()?;
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Destination(context.destination()),
        })))
    }

    pub(crate) fn create_periodic_wave(
        self: &Arc<Self>,
        real: Option<&[f32]>,
        imag: Option<&[f32]>,
        disable_normalization: bool,
    ) -> Result<NativePeriodicWave, JsErrorBox> {
        let context = self.context()?;
        let wave = create_periodic_wave(
            context.as_ref(),
            &self.byte_budget,
            real,
            imag,
            disable_normalization,
        )?;
        Ok(NativePeriodicWave {
            graph: NativeGraphIdentity::Online(Arc::downgrade(self)),
            wave,
        })
    }

    pub(crate) fn create_gain(
        self: &Arc<Self>,
        gain: f32,
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !gain.is_finite() {
            return Err(JsErrorBox::type_error("gain must be finite"));
        }
        Self::ensure_default_channel_options(channel_count, count_mode, interpretation)?;
        let context = self.context()?;
        let lifetime = self.reserve_node_lifetime(EXACT_GAIN_NODE_COUNT)?;
        let construction = self.reserve_control(EXACT_GAIN_CONSTRUCTION_COMMAND_COUNT)?;
        let value = (gain.to_bits() != 1.0_f32.to_bits())
            .then(|| self.reserve_control(1))
            .transpose()?;
        let node = panic::catch_unwind(AssertUnwindSafe(|| {
            context.create_gain_with_reservations(lifetime, construction)
        }))
        .map_err(|payload| JsErrorBox::generic(panic_message(payload)))?;
        if let Some(value) = value {
            panic::catch_unwind(AssertUnwindSafe(|| {
                node.gain().set_value_with_control_reservation(gain, value);
            }))
            .map_err(|payload| JsErrorBox::generic(panic_message(payload)))?;
        }
        let parameter = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(node.gain().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Gain { node, parameter },
        })))
    }

    pub(crate) fn create_constant_source(
        self: &Arc<Self>,
        offset: f32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !offset.is_finite() {
            return Err(JsErrorBox::type_error(
                "ConstantSource offset must be finite",
            ));
        }
        let root = self.state.event_hub().register_constant_source(
            &self.state,
            false,
            ConstantSourceOptions { offset },
        )?;
        let parameter = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(root.offset()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: u64::from(root.id()),
            kind: NativeNodeKind::OnlineConstantSource { root, parameter },
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_oscillator(
        self: &Arc<Self>,
        oscillator_type: OscillatorType,
        periodic_wave: Option<&NativePeriodicWave>,
        frequency: f32,
        detune: f32,
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !frequency.is_finite() || !detune.is_finite() {
            return Err(JsErrorBox::type_error(
                "oscillator frequency and detune must be finite",
            ));
        }
        let periodic_wave = periodic_wave
            .map(|wave| wave.for_graph(&NativeGraph::Online(Arc::clone(self))))
            .transpose()?;
        if oscillator_type == OscillatorType::Custom && periodic_wave.is_none() {
            return Err(JsErrorBox::generic(
                "InvalidStateError: custom oscillators require a PeriodicWave",
            ));
        }
        Self::ensure_default_channel_options(channel_count, count_mode, interpretation)?;
        let oscillator_type = if periodic_wave.is_some() {
            OscillatorType::Custom
        } else {
            oscillator_type
        };
        let options = OscillatorOptions {
            type_: oscillator_type,
            frequency,
            detune,
            periodic_wave,
            audio_node_options: AudioNodeOptions::default(),
        };
        let root = self
            .state
            .event_hub()
            .register_oscillator(&self.state, false, options)?;
        let frequency = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(root.frequency()),
        });
        let detune = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(root.detune()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: u64::from(root.id()),
            kind: NativeNodeKind::OnlineOscillator {
                root,
                frequency,
                detune,
            },
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_buffer_source(
        self: &Arc<Self>,
        detune: f32,
        loop_: bool,
        loop_start: f64,
        loop_end: f64,
        playback_rate: f32,
        channel_count: usize,
        count_mode: ChannelCountMode,
        interpretation: ChannelInterpretation,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        if !detune.is_finite()
            || !playback_rate.is_finite()
            || !loop_start.is_finite()
            || !loop_end.is_finite()
        {
            return Err(JsErrorBox::type_error(
                "AudioBufferSource options must be finite",
            ));
        }
        Self::ensure_default_channel_options(channel_count, count_mode, interpretation)?;
        let playback_rate_reservation = (playback_rate.to_bits() != 1.0_f32.to_bits())
            .then(|| self.reserve_control(1))
            .transpose()?;
        let detune_reservation = (detune.to_bits() != 0.0_f32.to_bits())
            .then(|| self.reserve_control(1))
            .transpose()?;
        let loop_reservation = loop_.then(|| self.reserve_control(1)).transpose()?;
        let loop_start_reservation = (loop_start.to_bits() != 0.0_f64.to_bits())
            .then(|| self.reserve_control(1))
            .transpose()?;
        let loop_end_reservation = (loop_end.to_bits() != 0.0_f64.to_bits())
            .then(|| self.reserve_control(1))
            .transpose()?;
        debug_assert!(
            [
                playback_rate_reservation.is_some(),
                detune_reservation.is_some(),
                loop_reservation.is_some(),
                loop_start_reservation.is_some(),
                loop_end_reservation.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count()
                <= EXACT_BUFFER_SOURCE_OPTION_COMMAND_LIMIT
        );
        let root = self
            .state
            .event_hub()
            .register_buffer_source(&self.state, false)?;
        if let Some(reservation) = playback_rate_reservation {
            root.set_playback_rate(playback_rate, reservation, &self.state)?;
        }
        if let Some(reservation) = detune_reservation {
            root.set_buffer_detune(detune, reservation, &self.state)?;
        }
        if let Some(reservation) = loop_reservation {
            root.set_loop(loop_, reservation, &self.state)?;
        }
        if let Some(reservation) = loop_start_reservation {
            root.set_loop_start(loop_start, reservation, &self.state)?;
        }
        if let Some(reservation) = loop_end_reservation {
            root.set_loop_end(loop_end, reservation, &self.state)?;
        }
        let playback_rate = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(root.playback_rate()),
        });
        let detune = Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: 0,
            kind: NativeNodeKind::Param(root.buffer_detune()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: NativeGraph::Online(Arc::clone(self)),
            id: u64::from(root.id()),
            kind: NativeNodeKind::OnlineBufferSource {
                root,
                playback_rate,
                detune,
            },
        })))
    }

    fn validate_pair(
        self: &Arc<Self>,
        source: &NativeNodeHandle,
        destination: &NativeNodeHandle,
        output: usize,
        input: usize,
    ) -> Result<(), JsErrorBox> {
        if !source.graph.is_online(self) || !destination.graph.is_online(self) {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: nodes belong to different audio contexts",
            ));
        }
        if output >= source.number_of_outputs() || input >= destination.number_of_inputs() {
            return Err(JsErrorBox::generic(
                "IndexSizeError: audio node port is out of range",
            ));
        }
        Ok(())
    }

    fn connect(
        self: &Arc<Self>,
        source: &NativeNodeHandle,
        destination: &NativeNodeHandle,
        output: usize,
        input: usize,
    ) -> Result<(), JsErrorBox> {
        self.validate_pair(source, destination, output, input)?;
        let runtime = Arc::clone(&self.runtime);
        let refused = Arc::new(AtomicBool::new(false));
        let provider_refused = Arc::clone(&refused);
        let provider = AudioExplicitConnectionReservationProvider::new(move || {
            let reserve = || {
                let isolate = runtime.try_reserve_graph_connection().ok()?;
                let host = runtime
                    .host()
                    .try_reserve(AudioHostResourceKind::GraphConnections, 1)
                    .ok()?;
                let control = runtime.try_reserve_control_commands(1).ok()?;
                Some((isolate, host, control))
            };
            let Some((isolate, host, control)) = reserve() else {
                provider_refused.store(true, Ordering::Release);
                return None;
            };
            Some(AudioExplicitConnectionReservation::new(
                AudioGraphConnectionReservation::new(ConnectionReservations {
                    _isolate: isolate,
                    _host: host,
                }),
                AudioControlBatchReservation::new(control),
            ))
        });
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            source.with_audio_node(|source_node| {
                destination.with_audio_node(|destination_node| {
                    source_node.connect_from_output_to_input_with_reservations(
                        destination_node,
                        output,
                        input,
                        provider,
                    );
                });
            });
        }));
        match result {
            Ok(()) => Ok(()),
            Err(payload) if refused.load(Ordering::Acquire) => {
                std::mem::forget(payload);
                Err(JsErrorBox::range_error(
                    "explicit connection exceeds a configured audio resource limit",
                ))
            }
            Err(payload) => Err(JsErrorBox::generic(panic_message(payload))),
        }
    }

    fn disconnect(
        self: &Arc<Self>,
        source: &NativeNodeHandle,
        destination: Option<&NativeNodeHandle>,
        output: Option<usize>,
        input: Option<usize>,
    ) -> Result<(), JsErrorBox> {
        if !source.graph.is_online(self)
            || destination.is_some_and(|destination| !destination.graph.is_online(self))
        {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: nodes belong to different audio contexts",
            ));
        }
        if output.is_some_and(|output| output >= source.number_of_outputs()) {
            return Err(JsErrorBox::generic(
                "IndexSizeError: audio node output port is out of range",
            ));
        }
        if let (Some(destination), Some(input)) = (destination, input) {
            if input >= destination.number_of_inputs() {
                return Err(JsErrorBox::generic(
                    "IndexSizeError: audio node input port is out of range",
                ));
            }
        }
        let runtime = Arc::clone(&self.runtime);
        let refused = Arc::new(AtomicBool::new(false));
        let provider_refused = Arc::clone(&refused);
        let provider = AudioControlBatchReservationProvider::new(move |count| {
            let reservation = runtime.try_reserve_control_commands(count).ok();
            if reservation.is_none() {
                provider_refused.store(true, Ordering::Release);
            }
            reservation.map(AudioControlBatchReservation::new)
        });
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            source.with_audio_node(|source_node| match (destination, output, input) {
                (None, None, None) => source_node.disconnect_with_control_reservation(
                    AudioNodeDisconnectSelector::All,
                    provider,
                ),
                (None, Some(output), None) => source_node.disconnect_with_control_reservation(
                    AudioNodeDisconnectSelector::Output(output),
                    provider,
                ),
                (Some(destination), None, None) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_with_control_reservation(
                            AudioNodeDisconnectSelector::Destination(destination_node),
                            provider,
                        );
                    });
                }
                (Some(destination), Some(output), None) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_with_control_reservation(
                            AudioNodeDisconnectSelector::DestinationOutput {
                                destination: destination_node,
                                output,
                            },
                            provider,
                        );
                    });
                }
                (Some(destination), Some(output), Some(input)) => {
                    destination.with_audio_node(|destination_node| {
                        source_node.disconnect_with_control_reservation(
                            AudioNodeDisconnectSelector::Exact {
                                destination: destination_node,
                                output,
                                input,
                            },
                            provider,
                        );
                    });
                }
                _ => unreachable!("JavaScript exposes only legal disconnect selectors"),
            });
        }));
        match result {
            Ok(()) => Ok(()),
            Err(payload) if refused.load(Ordering::Acquire) => {
                std::mem::forget(payload);
                Err(JsErrorBox::range_error(
                    "explicit disconnection exceeds a configured audio resource limit",
                ))
            }
            Err(payload) => Err(JsErrorBox::generic(panic_message(payload))),
        }
    }

    fn set_param_value(&self, param: &AudioParam, value: f32) -> Result<(), JsErrorBox> {
        let reservation = self.reserve_control(1)?;
        panic::catch_unwind(AssertUnwindSafe(|| {
            param.set_value_with_control_reservation(value, reservation);
        }))
        .map_err(|payload| JsErrorBox::generic(panic_message(payload)))
    }

    fn unsupported_mutation(&self, name: &'static str) -> Result<(), JsErrorBox> {
        self.context()?;
        Err(JsErrorBox::generic(format!(
            "NotSupportedError: {name} is not available on exact online contexts"
        )))
    }
}

#[derive(Clone)]
enum NativeGraph {
    Offline(Arc<OfflineGraph>),
    Online(Arc<OnlineGraph>),
}

impl NativeGraph {
    fn is_offline(&self, graph: &OfflineGraph) -> bool {
        matches!(self, Self::Offline(candidate) if std::ptr::eq(candidate.as_ref(), graph))
    }

    fn is_online(&self, graph: &Arc<OnlineGraph>) -> bool {
        matches!(self, Self::Online(candidate) if Arc::ptr_eq(candidate, graph))
    }

    fn connect(
        &self,
        source: &NativeNodeHandle,
        destination: &NativeNodeHandle,
        output: usize,
        input: usize,
    ) -> Result<(), JsErrorBox> {
        match self {
            Self::Offline(graph) => graph.connect(source, destination, output, input),
            Self::Online(graph) => graph.connect(source, destination, output, input),
        }
    }

    fn disconnect(
        &self,
        source: &NativeNodeHandle,
        destination: Option<&NativeNodeHandle>,
        output: Option<usize>,
        input: Option<usize>,
    ) -> Result<(), JsErrorBox> {
        match self {
            Self::Offline(graph) => graph.disconnect(source, destination, output, input),
            Self::Online(graph) => graph.disconnect(source, destination, output, input),
        }
    }

    fn run_offline_command(
        &self,
        name: &'static str,
        automation: bool,
        native_bytes: usize,
        operation: impl FnOnce(),
    ) -> Result<(), JsErrorBox> {
        match self {
            Self::Offline(graph) => graph.run_command(automation, native_bytes, operation),
            Self::Online(graph) => graph.unsupported_mutation(name),
        }
    }
}

enum NativeGraphIdentity {
    Offline(Weak<OfflineGraph>),
    Online(Weak<OnlineGraph>),
}

impl NativeGraphIdentity {
    fn matches(&self, graph: &NativeGraph) -> bool {
        match (self, graph) {
            (Self::Offline(identity), NativeGraph::Offline(candidate)) => {
                Weak::ptr_eq(identity, &Arc::downgrade(candidate))
            }
            (Self::Online(identity), NativeGraph::Online(candidate)) => {
                Weak::ptr_eq(identity, &Arc::downgrade(candidate))
            }
            _ => false,
        }
    }
}

/// Private native storage behind the public JavaScript `PeriodicWave` facade.
pub(crate) struct NativePeriodicWave {
    graph: NativeGraphIdentity,
    wave: PeriodicWave,
}

impl NativePeriodicWave {
    fn for_graph(&self, graph: &NativeGraph) -> Result<PeriodicWave, JsErrorBox> {
        if !self.graph.matches(graph) {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: PeriodicWave belongs to a different audio context",
            ));
        }
        Ok(self.wave.clone())
    }
}

// SAFETY: the object owns only thread-safe native graph and wavetable storage and contains no V8
// or CppGC pointers.
unsafe impl GarbageCollected for NativePeriodicWave {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativePeriodicWave"
    }
}

#[op2]
impl NativePeriodicWave {}

enum NativeNodeKind {
    Destination(AudioDestinationNode),
    Gain {
        node: GainNode,
        parameter: Arc<NativeNodeHandle>,
    },
    Oscillator {
        node: Box<Mutex<OscillatorNode>>,
        frequency: Arc<NativeNodeHandle>,
        detune: Arc<NativeNodeHandle>,
    },
    OnlineOscillator {
        root: Arc<ScheduledSourceRoot>,
        frequency: Arc<NativeNodeHandle>,
        detune: Arc<NativeNodeHandle>,
    },
    BufferSource {
        node: Box<Mutex<AudioBufferSourceNode>>,
        playback_rate: Arc<NativeNodeHandle>,
        detune: Arc<NativeNodeHandle>,
    },
    OnlineBufferSource {
        root: Arc<ScheduledSourceRoot>,
        playback_rate: Arc<NativeNodeHandle>,
        detune: Arc<NativeNodeHandle>,
    },
    ConstantSource {
        node: Box<Mutex<ConstantSourceNode>>,
        parameter: Arc<NativeNodeHandle>,
    },
    OnlineConstantSource {
        root: Arc<ScheduledSourceRoot>,
        parameter: Arc<NativeNodeHandle>,
    },
    Param(AudioParam),
}

struct NativeNodeHandle {
    graph: NativeGraph,
    id: u64,
    kind: NativeNodeKind,
}

impl NativeNodeHandle {
    fn with_audio_node<T>(&self, operation: impl FnOnce(&dyn AudioNode) -> T) -> T {
        match &self.kind {
            NativeNodeKind::Destination(node) => operation(node),
            NativeNodeKind::Gain { node, .. } => operation(node),
            NativeNodeKind::Oscillator { node, .. } => operation(&*mutex(node)),
            NativeNodeKind::OnlineOscillator { root, .. }
            | NativeNodeKind::OnlineBufferSource { root, .. }
            | NativeNodeKind::OnlineConstantSource { root, .. } => root.with_audio_node(operation),
            NativeNodeKind::BufferSource { node, .. } => operation(&*mutex(node)),
            NativeNodeKind::ConstantSource { node, .. } => operation(&*mutex(node)),
            NativeNodeKind::Param(param) => operation(param),
        }
    }

    fn with_param<T>(&self, operation: impl FnOnce(&AudioParam) -> T) -> Result<T, JsErrorBox> {
        match &self.kind {
            NativeNodeKind::Param(param) => Ok(operation(param)),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioParam",
            )),
        }
    }

    fn number_of_inputs(&self) -> usize {
        self.with_audio_node(|node| node.number_of_inputs())
    }

    fn number_of_outputs(&self) -> usize {
        self.with_audio_node(|node| node.number_of_outputs())
    }
}

/// One erased native node/parameter handle behind all public graph wrappers.
pub(crate) struct NativeAudioNode {
    handle: Arc<NativeNodeHandle>,
}

impl NativeAudioNode {
    fn from_handle(handle: Arc<NativeNodeHandle>) -> Self {
        Self { handle }
    }

    fn oscillator_type(value: u32) -> Result<OscillatorType, JsErrorBox> {
        match value {
            0 => Ok(OscillatorType::Sine),
            1 => Ok(OscillatorType::Square),
            2 => Ok(OscillatorType::Sawtooth),
            3 => Ok(OscillatorType::Triangle),
            _ => Err(JsErrorBox::generic(
                "InvalidStateError: custom oscillator type is unavailable",
            )),
        }
    }
}

// SAFETY: the object contains only thread-safe Rust ownership. It has no V8 or
// CppGC pointers.
unsafe impl GarbageCollected for NativeAudioNode {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativeAudioNode"
    }
}

#[op2]
impl NativeAudioNode {
    #[getter]
    fn kind(&self) -> u32 {
        match &self.handle.kind {
            NativeNodeKind::Destination(_) => 0,
            NativeNodeKind::Gain { .. } => 1,
            NativeNodeKind::Oscillator { .. } | NativeNodeKind::OnlineOscillator { .. } => 2,
            NativeNodeKind::Param(_) => 3,
            NativeNodeKind::BufferSource { .. } | NativeNodeKind::OnlineBufferSource { .. } => 4,
            NativeNodeKind::ConstantSource { .. } | NativeNodeKind::OnlineConstantSource { .. } => {
                5
            }
        }
    }

    #[getter]
    fn number_of_inputs(&self) -> u32 {
        u32::try_from(self.handle.number_of_inputs()).expect("Web Audio port count must fit u32")
    }

    #[getter]
    fn number_of_outputs(&self) -> u32 {
        u32::try_from(self.handle.number_of_outputs()).expect("Web Audio port count must fit u32")
    }

    #[getter]
    fn channel_count(&self) -> u32 {
        u32::try_from(self.handle.with_audio_node(|node| node.channel_count()))
            .expect("validated channel count must fit u32")
    }

    #[getter]
    fn channel_count_mode(&self) -> u32 {
        match self
            .handle
            .with_audio_node(|node| node.channel_count_mode())
        {
            ChannelCountMode::Max => 0,
            ChannelCountMode::ClampedMax => 1,
            ChannelCountMode::Explicit => 2,
        }
    }

    #[getter]
    fn channel_interpretation(&self) -> u32 {
        match self
            .handle
            .with_audio_node(|node| node.channel_interpretation())
        {
            ChannelInterpretation::Speakers => 0,
            ChannelInterpretation::Discrete => 1,
        }
    }

    #[fast]
    fn set_channel_count(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = value as usize;
        if !(1..=web_audio_api::MAX_CHANNELS).contains(&value) {
            return Err(JsErrorBox::generic(
                "NotSupportedError: channelCount is outside the supported range",
            ));
        }
        self.handle
            .graph
            .run_offline_command("channelCount mutation", false, 0, || {
                self.handle
                    .with_audio_node(|node| node.set_channel_count(value));
            })
    }

    #[fast]
    fn set_channel_count_mode(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = match value {
            0 => ChannelCountMode::Max,
            1 => ChannelCountMode::ClampedMax,
            2 => ChannelCountMode::Explicit,
            _ => return Err(JsErrorBox::type_error("unknown channelCountMode")),
        };
        self.handle
            .graph
            .run_offline_command("channelCountMode mutation", false, 0, || {
                self.handle
                    .with_audio_node(|node| node.set_channel_count_mode(value));
            })
    }

    #[fast]
    fn set_channel_interpretation(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = match value {
            0 => ChannelInterpretation::Speakers,
            1 => ChannelInterpretation::Discrete,
            _ => return Err(JsErrorBox::type_error("unknown channelInterpretation")),
        };
        self.handle
            .graph
            .run_offline_command("channelInterpretation mutation", false, 0, || {
                self.handle
                    .with_audio_node(|node| node.set_channel_interpretation(value));
            })
    }

    #[fast]
    fn connect(
        &self,
        #[cppgc] destination: &NativeAudioNode,
        output: u32,
        input: u32,
    ) -> Result<(), JsErrorBox> {
        self.handle.graph.connect(
            &self.handle,
            &destination.handle,
            output as usize,
            input as usize,
        )
    }

    #[fast]
    fn disconnect(
        &self,
        #[cppgc] destination: Option<&NativeAudioNode>,
        output: i32,
        input: i32,
    ) -> Result<(), JsErrorBox> {
        self.handle.graph.disconnect(
            &self.handle,
            destination.map(|destination| destination.handle.as_ref()),
            usize::try_from(output).ok(),
            usize::try_from(input).ok(),
        )
    }

    #[cppgc]
    fn gain(&self) -> Result<NativeAudioNode, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Gain { parameter, .. } => Ok(Self::from_handle(Arc::clone(parameter))),
            _ => Err(JsErrorBox::type_error("native binding expected a GainNode")),
        }
    }

    #[cppgc]
    fn frequency(&self) -> Result<NativeAudioNode, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Oscillator { frequency, .. }
            | NativeNodeKind::OnlineOscillator { frequency, .. } => {
                Ok(Self::from_handle(Arc::clone(frequency)))
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[cppgc]
    fn detune(&self) -> Result<NativeAudioNode, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Oscillator { detune, .. }
            | NativeNodeKind::OnlineOscillator { detune, .. }
            | NativeNodeKind::BufferSource { detune, .. }
            | NativeNodeKind::OnlineBufferSource { detune, .. } => {
                Ok(Self::from_handle(Arc::clone(detune)))
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected a scheduled source with detune",
            )),
        }
    }

    #[cppgc]
    fn playback_rate(&self) -> Result<NativeAudioNode, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::BufferSource { playback_rate, .. }
            | NativeNodeKind::OnlineBufferSource { playback_rate, .. } => {
                Ok(Self::from_handle(Arc::clone(playback_rate)))
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[cppgc]
    fn offset(&self) -> Result<NativeAudioNode, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::ConstantSource { parameter, .. }
            | NativeNodeKind::OnlineConstantSource { parameter, .. } => {
                Ok(Self::from_handle(Arc::clone(parameter)))
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected a ConstantSourceNode",
            )),
        }
    }

    #[getter]
    fn value(&self) -> Result<f32, JsErrorBox> {
        self.handle.with_param(AudioParam::value)
    }

    #[fast]
    fn set_value(&self, value: f32) -> Result<(), JsErrorBox> {
        if !value.is_finite() {
            return Err(JsErrorBox::type_error("AudioParam value must be finite"));
        }
        match &self.handle.graph {
            NativeGraph::Offline(graph) => graph.run_command(true, 0, || {
                self.handle
                    .with_param(|param| {
                        param.set_value(value);
                    })
                    .expect("binding prevalidated AudioParam kind");
            }),
            NativeGraph::Online(graph) => self
                .handle
                .with_param(|param| graph.set_param_value(param, value))?,
        }
    }

    #[getter]
    fn default_value(&self) -> Result<f32, JsErrorBox> {
        self.handle.with_param(AudioParam::default_value)
    }

    #[getter]
    fn min_value(&self) -> Result<f32, JsErrorBox> {
        self.handle.with_param(AudioParam::min_value)
    }

    #[getter]
    fn max_value(&self) -> Result<f32, JsErrorBox> {
        self.handle.with_param(AudioParam::max_value)
    }

    #[getter]
    fn automation_rate(&self) -> Result<u32, JsErrorBox> {
        self.handle
            .with_param(|param| match param.automation_rate() {
                AutomationRate::A => 0,
                AutomationRate::K => 1,
            })
    }

    #[fast]
    fn set_automation_rate(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = match value {
            0 => AutomationRate::A,
            1 => AutomationRate::K,
            _ => return Err(JsErrorBox::type_error("unknown automationRate")),
        };
        self.handle
            .graph
            .run_offline_command("automationRate mutation", false, 0, || {
                self.handle
                    .with_param(|param| param.set_automation_rate(value))
                    .expect("binding prevalidated AudioParam kind");
            })
    }

    #[fast]
    fn set_value_at_time(&self, value: f32, time: f64) -> Result<(), JsErrorBox> {
        self.run_param_automation(value, time, |param, value, time| {
            param.set_value_at_time(value, time);
        })
    }

    #[fast]
    fn linear_ramp_to_value_at_time(&self, value: f32, time: f64) -> Result<(), JsErrorBox> {
        self.run_param_automation(value, time, |param, value, time| {
            param.linear_ramp_to_value_at_time(value, time);
        })
    }

    #[fast]
    fn exponential_ramp_to_value_at_time(&self, value: f32, time: f64) -> Result<(), JsErrorBox> {
        if value == 0.0 {
            return Err(JsErrorBox::range_error(
                "AudioParam exponential ramp target cannot be zero",
            ));
        }
        self.run_param_automation(value, time, |param, value, time| {
            param.exponential_ramp_to_value_at_time(value, time);
        })
    }

    #[fast]
    fn set_target_at_time(
        &self,
        value: f32,
        start_time: f64,
        time_constant: f64,
    ) -> Result<(), JsErrorBox> {
        if !value.is_finite()
            || !start_time.is_finite()
            || start_time < 0.0
            || !time_constant.is_finite()
            || time_constant < 0.0
        {
            return Err(JsErrorBox::type_error(
                "AudioParam automation arguments are invalid",
            ));
        }
        self.handle
            .graph
            .run_offline_command("AudioParam automation", true, 0, || {
                self.handle
                    .with_param(|param| {
                        param.set_target_at_time(value, start_time, time_constant);
                    })
                    .expect("binding prevalidated AudioParam kind");
            })
    }

    #[fast]
    fn cancel_scheduled_values(&self, time: f64) -> Result<(), JsErrorBox> {
        self.run_param_time(time, |param, time| {
            param.cancel_scheduled_values(time);
        })
    }

    #[fast]
    fn cancel_and_hold_at_time(&self, time: f64) -> Result<(), JsErrorBox> {
        self.run_param_time(time, |param, time| {
            param.cancel_and_hold_at_time(time);
        })
    }

    #[fast]
    fn set_value_curve_at_time(
        &self,
        #[buffer] values: &[f32],
        start_time: f64,
        duration: f64,
    ) -> Result<(), JsErrorBox> {
        if values.len() < 2
            || values.iter().any(|value| !value.is_finite())
            || !start_time.is_finite()
            || start_time < 0.0
            || !duration.is_finite()
            || duration <= 0.0
        {
            return Err(JsErrorBox::type_error(
                "AudioParam value curve arguments are invalid",
            ));
        }
        if values.len() > MAX_AUTOMATION_CURVE_SAMPLES {
            return Err(JsErrorBox::range_error(
                "AudioParam value curve exceeds the fixed sample limit",
            ));
        }
        let native_bytes = values.len().checked_mul(size_of::<f32>()).ok_or_else(|| {
            JsErrorBox::range_error("AudioParam value curve byte count overflowed")
        })?;
        self.handle
            .graph
            .run_offline_command("AudioParam automation", true, native_bytes, || {
                self.handle
                    .with_param(|param| {
                        param.set_value_curve_at_time(values, start_time, duration);
                    })
                    .expect("binding prevalidated AudioParam kind");
            })
    }

    #[fast]
    fn set_buffer(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        #[cppgc] buffer: &NativeAudioBuffer,
    ) -> Result<(), JsErrorBox> {
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => graph
                .run_prepared_command(
                    || buffer.acquire_for_playback(scope),
                    |buffer| mutex(node).set_buffer(buffer),
                ),
            (NativeGraph::Online(graph), NativeNodeKind::OnlineBufferSource { root, .. }) => {
                let reservation = graph.reserve_control(1)?;
                let buffer = buffer.acquire_for_playback(scope)?;
                root.set_buffer(buffer, reservation, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[getter]
    fn loop_enabled(&self) -> Result<bool, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::BufferSource { node, .. } => Ok(mutex(node).loop_()),
            NativeNodeKind::OnlineBufferSource { root, .. } => Ok(root.loop_()),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[fast]
    fn set_loop_enabled(&self, value: bool) -> Result<(), JsErrorBox> {
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).set_loop(value))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineBufferSource { root, .. }) => {
                root.set_loop(value, graph.reserve_control(1)?, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[getter]
    fn loop_start(&self) -> Result<f64, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::BufferSource { node, .. } => Ok(mutex(node).loop_start()),
            NativeNodeKind::OnlineBufferSource { root, .. } => Ok(root.loop_start()),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[fast]
    fn set_loop_start(&self, value: f64) -> Result<(), JsErrorBox> {
        if !value.is_finite() {
            return Err(JsErrorBox::type_error("loopStart must be finite"));
        }
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).set_loop_start(value))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineBufferSource { root, .. }) => {
                root.set_loop_start(value, graph.reserve_control(1)?, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[getter]
    fn loop_end(&self) -> Result<f64, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::BufferSource { node, .. } => Ok(mutex(node).loop_end()),
            NativeNodeKind::OnlineBufferSource { root, .. } => Ok(root.loop_end()),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[fast]
    fn set_loop_end(&self, value: f64) -> Result<(), JsErrorBox> {
        if !value.is_finite() {
            return Err(JsErrorBox::type_error("loopEnd must be finite"));
        }
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).set_loop_end(value))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineBufferSource { root, .. }) => {
                root.set_loop_end(value, graph.reserve_control(1)?, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[getter]
    fn oscillator_type(&self) -> Result<u32, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Oscillator { node, .. } => Ok(match mutex(node).type_() {
                OscillatorType::Sine => 0,
                OscillatorType::Square => 1,
                OscillatorType::Sawtooth => 2,
                OscillatorType::Triangle => 3,
                OscillatorType::Custom => 4,
            }),
            NativeNodeKind::OnlineOscillator { root, .. } => Ok(match root.type_() {
                OscillatorType::Sine => 0,
                OscillatorType::Square => 1,
                OscillatorType::Sawtooth => 2,
                OscillatorType::Triangle => 3,
                OscillatorType::Custom => 4,
            }),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[fast]
    fn set_oscillator_type(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = Self::oscillator_type(value)?;
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::Oscillator { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).set_type(value))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineOscillator { root, .. }) => {
                root.set_type(value, graph.reserve_control(1)?, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[fast]
    fn set_periodic_wave(
        &self,
        #[cppgc] periodic_wave: &NativePeriodicWave,
    ) -> Result<(), JsErrorBox> {
        let periodic_wave = periodic_wave.for_graph(&self.handle.graph)?;
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::Oscillator { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).set_periodic_wave(periodic_wave))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineOscillator { root, .. }) => {
                let reservation = graph.reserve_control(1)?;
                root.set_periodic_wave(periodic_wave, reservation, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[fast]
    fn start(&self, when: f64) -> Result<(), JsErrorBox> {
        if !when.is_finite() || when < 0.0 {
            return Err(JsErrorBox::type_error(
                "start time must be finite and non-negative",
            ));
        }
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::Oscillator { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).start_at(when))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineOscillator { root, .. }) => {
                root.start(when, graph.reserve_control(1)?, &graph.state)
            }
            (NativeGraph::Offline(graph), NativeNodeKind::ConstantSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).start_at(when))
            }
            (NativeGraph::Online(graph), NativeNodeKind::OnlineConstantSource { root, .. }) => {
                root.start_constant_source(when, graph.reserve_control(1)?, &graph.state)
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[fast]
    fn start_buffer(&self, when: f64, offset: f64, duration: f64) -> Result<(), JsErrorBox> {
        if !when.is_finite()
            || when < 0.0
            || !offset.is_finite()
            || offset < 0.0
            || !duration.is_finite()
            || duration < 0.0
        {
            return Err(JsErrorBox::type_error(
                "BufferSource start values must be finite and non-negative",
            ));
        }
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => graph
                .run_command(false, 0, || {
                    mutex(node).start_at_with_offset_and_duration(when, offset, duration);
                }),
            (NativeGraph::Online(graph), NativeNodeKind::OnlineBufferSource { root, .. }) => root
                .start_buffer_source(
                    when,
                    offset,
                    duration,
                    graph.reserve_control(1)?,
                    &graph.state,
                ),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioBufferSourceNode",
            )),
        }
    }

    #[fast]
    fn stop(&self, when: f64) -> Result<(), JsErrorBox> {
        if !when.is_finite() || when < 0.0 {
            return Err(JsErrorBox::type_error(
                "stop time must be finite and non-negative",
            ));
        }
        match (&self.handle.graph, &self.handle.kind) {
            (NativeGraph::Offline(graph), NativeNodeKind::Oscillator { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).stop_at(when))
            }
            (NativeGraph::Offline(graph), NativeNodeKind::BufferSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).stop_at(when))
            }
            (NativeGraph::Offline(graph), NativeNodeKind::ConstantSource { node, .. }) => {
                graph.run_command(false, 0, || mutex(node).stop_at(when))
            }
            (
                NativeGraph::Online(graph),
                NativeNodeKind::OnlineOscillator { root, .. }
                | NativeNodeKind::OnlineBufferSource { root, .. }
                | NativeNodeKind::OnlineConstantSource { root, .. },
            ) => root.stop(when, graph.reserve_control(1)?, &graph.state),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioScheduledSourceNode",
            )),
        }
    }

    #[getter]
    fn complete(&self) -> Result<bool, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Oscillator { node, .. } => {
                Ok(mutex(node).completion_token().is_complete())
            }
            NativeNodeKind::BufferSource { node, .. } => {
                Ok(mutex(node).completion_token().is_complete())
            }
            NativeNodeKind::ConstantSource { node, .. } => {
                Ok(mutex(node).completion_token().is_complete())
            }
            NativeNodeKind::OnlineOscillator { root, .. }
            | NativeNodeKind::OnlineBufferSource { root, .. }
            | NativeNodeKind::OnlineConstantSource { root, .. } => Ok(root.is_complete()),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an AudioScheduledSourceNode",
            )),
        }
    }

    #[getter]
    fn source_id(&self) -> f64 {
        match &self.handle.kind {
            NativeNodeKind::OnlineOscillator { root, .. }
            | NativeNodeKind::OnlineBufferSource { root, .. }
            | NativeNodeKind::OnlineConstantSource { root, .. } => f64::from(root.id()),
            _ => 0.0,
        }
    }
}

impl NativeAudioNode {
    fn run_param_automation(
        &self,
        value: f32,
        time: f64,
        operation: impl FnOnce(&AudioParam, f32, f64),
    ) -> Result<(), JsErrorBox> {
        if !value.is_finite() || !time.is_finite() || time < 0.0 {
            return Err(JsErrorBox::type_error(
                "AudioParam automation arguments are invalid",
            ));
        }
        self.handle
            .graph
            .run_offline_command("AudioParam automation", true, 0, || {
                self.handle
                    .with_param(|param| operation(param, value, time))
                    .expect("binding prevalidated AudioParam kind");
            })
    }

    fn run_param_time(
        &self,
        time: f64,
        operation: impl FnOnce(&AudioParam, f64),
    ) -> Result<(), JsErrorBox> {
        if !time.is_finite() || time < 0.0 {
            return Err(JsErrorBox::type_error(
                "AudioParam automation time must be finite and non-negative",
            ));
        }
        self.handle
            .graph
            .run_offline_command("AudioParam automation", true, 0, || {
                self.handle
                    .with_param(|param| operation(param, time))
                    .expect("binding prevalidated AudioParam kind");
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use web_audio_api::AudioBuffer;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn upstream_offline_values_remain_send_sync() {
        assert_send_sync::<AudioBuffer>();
        assert_send_sync::<OfflineAudioContext>();
        assert_send_sync::<GainNode>();
        assert_send_sync::<OscillatorNode>();
        assert_send_sync::<AudioBufferSourceNode>();
        assert_send_sync::<AudioParam>();
        assert_send_sync::<NativeAudioNode>();
    }
}
