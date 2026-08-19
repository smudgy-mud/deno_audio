use std::collections::HashMap;
use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use deno_core::{op2, v8, GarbageCollected};
use deno_error::JsErrorBox;
use web_audio_api::context::{BaseAudioContext, OfflineAudioContext};
use web_audio_api::node::{
    AudioDestinationNode, AudioNode, AudioNodeOptions, AudioScheduledSourceNode,
    AudioScheduledSourceNodeExt, ChannelCountMode, ChannelInterpretation, GainNode, GainOptions,
    OscillatorNode, OscillatorOptions, OscillatorType,
};
use web_audio_api::{AudioParam, AutomationRate};

use crate::buffer::{NativeAudioByteBudget, NativeAudioByteReservation};
use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::online::{AudioRuntimeState, ControlCommandReservations, IsolateReservation};

const MAGIC_NODE_COUNT: usize = 11;
const MAGIC_COMMAND_COUNT: usize = 21;
const GAIN_NODE_COUNT: usize = 2;
const GAIN_COMMAND_COUNT_WITH_TEARDOWN: usize = 6;
const OSCILLATOR_NODE_COUNT: usize = 3;
const OSCILLATOR_COMMAND_COUNT_WITH_TEARDOWN: usize = 10;
const MAX_AUTOMATION_CURVE_SAMPLES: usize = 1_048_576;

static NEXT_OFFLINE_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_context_id() -> Result<u64, JsErrorBox> {
    NEXT_OFFLINE_CONTEXT_ID
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map_err(|_| JsErrorBox::generic("offline audio context identity space is exhausted"))
}

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
    context_id: u64,
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
        let context_id = next_context_id()?;
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
            context_id,
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

    pub(crate) fn destination(self: &Arc<Self>) -> NativeAudioNode {
        NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: Arc::clone(self),
            id: 0,
            kind: NativeNodeKind::Destination(self.context.destination()),
        }))
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
            graph: Arc::clone(self),
            id: first + 1,
            kind: NativeNodeKind::Param(node.gain().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: Arc::clone(self),
            id: first,
            kind: NativeNodeKind::Gain { node, parameter },
        })))
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn create_oscillator(
        self: &Arc<Self>,
        oscillator_type: OscillatorType,
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
        if oscillator_type == OscillatorType::Custom {
            return Err(JsErrorBox::generic(
                "InvalidStateError: custom oscillators require a PeriodicWave",
            ));
        }
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
            periodic_wave: None,
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
            graph: Arc::clone(self),
            id: first + 1,
            kind: NativeNodeKind::Param(node.frequency().clone()),
        });
        let detune = Arc::new(NativeNodeHandle {
            graph: Arc::clone(self),
            id: first + 2,
            kind: NativeNodeKind::Param(node.detune().clone()),
        });
        Ok(NativeAudioNode::from_handle(Arc::new(NativeNodeHandle {
            graph: Arc::clone(self),
            id: first,
            kind: NativeNodeKind::Oscillator {
                node: Box::new(Mutex::new(node)),
                frequency,
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
        if !Arc::ptr_eq(&source.graph, &destination.graph)
            || source.graph.context_id != self.context_id
        {
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
        if source.graph.context_id != self.context_id {
            return Err(JsErrorBox::generic(
                "InvalidAccessError: source belongs to a different audio context",
            ));
        }
        if let Some(destination) = destination {
            if !Arc::ptr_eq(&source.graph, &destination.graph) {
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
    Param(AudioParam),
}

struct NativeNodeHandle {
    graph: Arc<OfflineGraph>,
    id: u64,
    kind: NativeNodeKind,
}

impl NativeNodeHandle {
    fn with_audio_node<T>(&self, operation: impl FnOnce(&dyn AudioNode) -> T) -> T {
        match &self.kind {
            NativeNodeKind::Destination(node) => operation(node),
            NativeNodeKind::Gain { node, .. } => operation(node),
            NativeNodeKind::Oscillator { node, .. } => operation(&*mutex(node)),
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
            NativeNodeKind::Oscillator { .. } => 2,
            NativeNodeKind::Param(_) => 3,
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
        self.handle.graph.run_command(false, 0, || {
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
        self.handle.graph.run_command(false, 0, || {
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
        self.handle.graph.run_command(false, 0, || {
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
            NativeNodeKind::Oscillator { frequency, .. } => {
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
            NativeNodeKind::Oscillator { detune, .. } => Ok(Self::from_handle(Arc::clone(detune))),
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
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
        self.handle.graph.run_command(true, 0, || {
            self.handle
                .with_param(|param| {
                    param.set_value(value);
                })
                .expect("binding prevalidated AudioParam kind");
        })
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
        self.handle.graph.run_command(false, 0, || {
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
        self.handle.graph.run_command(true, 0, || {
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
        self.handle.graph.run_command(true, native_bytes, || {
            self.handle
                .with_param(|param| {
                    param.set_value_curve_at_time(values, start_time, duration);
                })
                .expect("binding prevalidated AudioParam kind");
        })
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
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
        }
    }

    #[fast]
    fn set_oscillator_type(&self, value: u32) -> Result<(), JsErrorBox> {
        let value = Self::oscillator_type(value)?;
        self.handle
            .graph
            .run_command(false, 0, || match &self.handle.kind {
                NativeNodeKind::Oscillator { node, .. } => mutex(node).set_type(value),
                _ => unreachable!("binding prevalidated OscillatorNode kind"),
            })
    }

    #[fast]
    fn start(&self, when: f64) -> Result<(), JsErrorBox> {
        if !when.is_finite() || when < 0.0 {
            return Err(JsErrorBox::type_error(
                "start time must be finite and non-negative",
            ));
        }
        self.handle
            .graph
            .run_command(false, 0, || match &self.handle.kind {
                NativeNodeKind::Oscillator { node, .. } => mutex(node).start_at(when),
                _ => unreachable!("binding prevalidated OscillatorNode kind"),
            })
    }

    #[fast]
    fn stop(&self, when: f64) -> Result<(), JsErrorBox> {
        if !when.is_finite() || when < 0.0 {
            return Err(JsErrorBox::type_error(
                "stop time must be finite and non-negative",
            ));
        }
        self.handle
            .graph
            .run_command(false, 0, || match &self.handle.kind {
                NativeNodeKind::Oscillator { node, .. } => mutex(node).stop_at(when),
                _ => unreachable!("binding prevalidated OscillatorNode kind"),
            })
    }

    #[getter]
    fn complete(&self) -> Result<bool, JsErrorBox> {
        match &self.handle.kind {
            NativeNodeKind::Oscillator { node, .. } => {
                Ok(mutex(node).completion_token().is_complete())
            }
            _ => Err(JsErrorBox::type_error(
                "native binding expected an OscillatorNode",
            )),
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
        self.handle.graph.run_command(true, 0, || {
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
        self.handle.graph.run_command(true, 0, || {
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
        assert_send_sync::<AudioParam>();
        assert_send_sync::<NativeAudioNode>();
    }
}
