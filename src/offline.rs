use std::ffi::CStr;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};
use std::thread;

use deno_core::serde::Serialize;
use deno_core::unsync::spawn_blocking;
use deno_core::{op2, v8, GarbageCollected, OpState};
use deno_error::JsErrorBox;
use web_audio_api::context::{BaseAudioContext, OfflineAudioContext};
use web_audio_api::node::{
    AudioNode, AudioScheduledSourceNode, ChannelCountMode, ChannelInterpretation, OscillatorType,
};
use web_audio_api::MAX_CHANNELS;

use crate::buffer::{
    checked_pcm_bytes, NativeAudioBuffer, NativeAudioByteBudget, NativeAudioByteReservation,
};
use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::node::{NativeAudioNode, NativePeriodicWave, OfflineGraph};
use crate::online::{AudioRuntimeState, IsolateReservation};

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

fn validate_offline_dimensions(
    runtime: &AudioRuntimeState,
    number_of_channels: u32,
    length: u32,
    sample_rate: f64,
) -> Result<(usize, usize, f32), JsErrorBox> {
    let number_of_channels = number_of_channels as usize;
    if !(1..=MAX_CHANNELS).contains(&number_of_channels)
        || runtime
            .limits()
            .channels()
            .is_some_and(|limit| number_of_channels > limit)
    {
        return Err(JsErrorBox::range_error(
            "numberOfChannels exceeds the configured offline limit",
        ));
    }
    let length = length as usize;
    if length == 0 {
        return Err(JsErrorBox::range_error("length must be greater than zero"));
    }
    if runtime
        .limits()
        .offline_frames()
        .is_some_and(|limit| length > limit)
    {
        return Err(JsErrorBox::range_error(
            "offline render frame limit exceeded",
        ));
    }
    let samples = number_of_channels
        .checked_mul(length)
        .ok_or_else(|| JsErrorBox::range_error("offline render sample count overflowed"))?;
    if runtime
        .limits()
        .offline_samples()
        .is_some_and(|limit| samples > limit)
    {
        return Err(JsErrorBox::range_error(
            "offline render logical sample limit exceeded",
        ));
    }
    let sample_rate_f32 = validate_sample_rate(sample_rate)?;
    if runtime
        .limits()
        .sample_rate()
        .is_some_and(|limit| sample_rate > f64::from(limit))
    {
        return Err(JsErrorBox::range_error(
            "configured offline sample-rate limit exceeded",
        ));
    }
    Ok((number_of_channels, length, sample_rate_f32))
}

#[derive(Serialize)]
#[serde(crate = "deno_core::serde", rename_all = "camelCase")]
struct Phase0RenderSummary {
    number_of_channels: u32,
    length: u32,
    peak: f64,
    off_isolate_thread: bool,
}

pub(crate) enum BlockingOutcome<T> {
    Complete(T),
    Failed(String),
    Panicked,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "Web Audio exposes a double sample rate while the pinned engine stores validated rates as f32"
)]
pub(crate) fn validate_sample_rate(sample_rate: f64) -> Result<f32, JsErrorBox> {
    if !(3_000.0..=768_000.0).contains(&sample_rate) {
        return Err(JsErrorBox::range_error(
            "sampleRate must be in the inclusive range 3000..=768000",
        ));
    }
    Ok(sample_rate as f32)
}

pub(crate) fn blocking_outcome<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> BlockingOutcome<T> {
    match panic::catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(value)) => BlockingOutcome::Complete(value),
        Ok(Err(error)) => BlockingOutcome::Failed(error),
        Err(payload) => {
            std::mem::forget(payload);
            BlockingOutcome::Panicked
        }
    }
}

pub(crate) async fn resolve_blocking<T>(
    operation_name: &'static str,
    task: impl FnOnce() -> BlockingOutcome<T> + Send + 'static,
) -> Result<T, JsErrorBox>
where
    T: Send + 'static,
{
    match spawn_blocking(task).await {
        Ok(BlockingOutcome::Complete(value)) => Ok(value),
        Ok(BlockingOutcome::Failed(error)) => Err(JsErrorBox::generic(error)),
        Ok(BlockingOutcome::Panicked) => Err(JsErrorBox::generic(format!(
            "{operation_name} panicked on the blocking pool"
        ))),
        Err(error) => Err(JsErrorBox::generic(format!(
            "{operation_name} blocking task failed: {error}"
        ))),
    }
}

/// Private native context behind the public JavaScript `OfflineAudioContext`.
pub(crate) struct NativeOfflineAudioContext {
    graph: Arc<OfflineGraph>,
    runtime: Arc<AudioRuntimeState>,
    budget: Arc<NativeAudioByteBudget>,
    number_of_channels: usize,
    length: usize,
    sample_rate: f32,
}

// SAFETY: this object owns only Send + Sync Rust values and no V8 or CppGC
// pointers.
unsafe impl GarbageCollected for NativeOfflineAudioContext {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativeOfflineAudioContext"
    }
}

#[op2]
impl NativeOfflineAudioContext {
    #[constructor]
    #[cppgc]
    fn new(
        state: &mut OpState,
        number_of_channels: u32,
        length: u32,
        sample_rate: f64,
    ) -> Result<NativeOfflineAudioContext, JsErrorBox> {
        let runtime = Arc::clone(state.borrow::<Arc<AudioRuntimeState>>());
        let budget = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
        let (number_of_channels, length, sample_rate) =
            validate_offline_dimensions(&runtime, number_of_channels, length, sample_rate)?;
        let graph = OfflineGraph::new(
            Arc::clone(&runtime),
            Arc::clone(&budget),
            number_of_channels,
            length,
            sample_rate,
        )?;
        Ok(NativeOfflineAudioContext {
            graph,
            runtime,
            budget,
            number_of_channels,
            length,
            sample_rate,
        })
    }

    #[getter]
    fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    #[getter]
    fn length(&self) -> u32 {
        u32::try_from(self.length).expect("Web IDL validated offline length must fit u32")
    }

    #[getter]
    fn number_of_channels(&self) -> u32 {
        u32::try_from(self.number_of_channels)
            .expect("Web IDL validated offline channel count must fit u32")
    }

    #[getter]
    fn current_time(&self) -> f64 {
        self.graph.context().current_time()
    }

    #[getter]
    fn state(&self) -> u32 {
        self.graph.state_code()
    }

    #[cppgc]
    fn destination(&self) -> NativeAudioNode {
        self.graph.destination()
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
    #[allow(clippy::too_many_arguments)]
    fn create_oscillator(
        &self,
        oscillator_type_code: u32,
        #[cppgc] periodic_wave: Option<&NativePeriodicWave>,
        frequency: f32,
        detune: f32,
        channel_count: u32,
        count_mode: u32,
        interpretation: u32,
    ) -> Result<NativeAudioNode, JsErrorBox> {
        self.graph.create_oscillator(
            oscillator_type(oscillator_type_code)?,
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
        let max_channels = u32::try_from(web_audio_api::MAX_CHANNELS)
            .expect("the native maximum channel count fits u32");
        if !(1..=max_channels).contains(&channel_count) {
            return Err(JsErrorBox::generic(
                "NotSupportedError: channelCount is outside the supported range",
            ));
        }
        let _ = channel_count_mode(count_mode)?;
        let _ = channel_interpretation(interpretation)?;
        self.graph
            .create_buffer_source(detune, loop_, loop_start, loop_end, playback_rate)
    }
}

struct PublicOfflineRenderPayload {
    graph: Arc<OfflineGraph>,
    budget: Arc<NativeAudioByteBudget>,
    output_reservation: NativeAudioByteReservation,
    _isolate_job: IsolateReservation,
    _host_job: AudioHostReservation,
}

struct PublicOfflineRenderResult {
    canonical: web_audio_api::AudioBuffer,
    budget: Arc<NativeAudioByteBudget>,
    output_reservation: NativeAudioByteReservation,
}

impl NativeOfflineAudioContext {
    fn prepare_render(&self) -> Result<PublicOfflineRenderPayload, JsErrorBox> {
        let isolate_job = self.runtime.try_reserve_offline_render_job()?;
        let host_job = self
            .runtime
            .host()
            .try_reserve(AudioHostResourceKind::OfflineRenderJobs, 1)
            .map_err(|error| {
                JsErrorBox::range_error(format!(
                    "AudioHost offline-render-job limit exceeded: {error}"
                ))
            })?;
        self.budget.validate_audio_buffer(
            self.number_of_channels,
            self.length,
            f64::from(self.sample_rate),
        )?;
        let output_bytes = checked_pcm_bytes(self.number_of_channels, self.length)
            .ok_or_else(|| JsErrorBox::range_error("offline render byte count overflowed"))?;
        let output_reservation = self.budget.reserve(output_bytes).map_err(|error| {
            JsErrorBox::range_error(format!(
                "offline render output byte reservation failed: {error}"
            ))
        })?;
        self.graph.begin_render()?;
        Ok(PublicOfflineRenderPayload {
            graph: Arc::clone(&self.graph),
            budget: Arc::clone(&self.budget),
            output_reservation,
            _isolate_job: isolate_job,
            _host_job: host_job,
        })
    }
}

/// Renders one public offline graph away from the isolate and returns its
/// canonical PCM as a newly managed native `AudioBuffer`.
#[op2]
#[cppgc]
pub(crate) async fn op_offline_start_rendering(
    #[cppgc] context: &NativeOfflineAudioContext,
) -> Result<NativeAudioBuffer, JsErrorBox> {
    let payload = context.prepare_render()?;
    let graph_on_join_failure = Arc::clone(&payload.graph);
    let rendered = resolve_blocking("offline audio render", move || {
        let graph = Arc::clone(&payload.graph);
        let outcome = blocking_outcome(|| {
            let canonical =
                deno_core::futures::executor::block_on(payload.graph.context().start_rendering());
            Ok(PublicOfflineRenderResult {
                canonical,
                budget: payload.budget,
                output_reservation: payload.output_reservation,
            })
        });
        graph.finish_render(matches!(outcome, BlockingOutcome::Complete(_)));
        outcome
    })
    .await;
    let rendered = match rendered {
        Ok(rendered) => rendered,
        Err(error) => {
            graph_on_join_failure.finish_render(false);
            return Err(error);
        }
    };
    Ok(NativeAudioBuffer::from_canonical(
        rendered.canonical,
        rendered.budget,
        rendered.output_reservation,
    ))
}

struct OfflineRenderPayload {
    number_of_channels: usize,
    length: usize,
    sample_rate: f32,
    isolate_thread: thread::ThreadId,
    _output_bytes: NativeAudioByteReservation,
    _isolate_job: IsolateReservation,
    _host_job: AudioHostReservation,
}

/// Private single-use offline-render proof. Dimensions, output bytes, and job
/// ownership are reserved before an upstream context can be constructed.
pub(crate) struct NativePhase0OfflineRenderRequest {
    payload: Mutex<Option<OfflineRenderPayload>>,
}

// SAFETY: this object contains no V8 or CppGC pointers. It owns only fixed
// render dimensions and Send reservation guards behind a mutex.
unsafe impl GarbageCollected for NativePhase0OfflineRenderRequest {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativePhase0OfflineRenderRequest"
    }
}

#[op2]
impl NativePhase0OfflineRenderRequest {
    #[constructor]
    #[cppgc]
    fn new(
        state: &mut OpState,
        number_of_channels: u32,
        length: u32,
        sample_rate: f64,
    ) -> Result<NativePhase0OfflineRenderRequest, JsErrorBox> {
        let runtime = Arc::clone(state.borrow::<Arc<AudioRuntimeState>>());
        let number_of_channels = number_of_channels as usize;
        if !(1..=MAX_CHANNELS).contains(&number_of_channels)
            || runtime
                .limits()
                .channels()
                .is_some_and(|limit| number_of_channels > limit)
        {
            return Err(JsErrorBox::range_error(
                "numberOfChannels exceeds the configured offline limit",
            ));
        }
        let length = length as usize;
        if length == 0 {
            return Err(JsErrorBox::range_error("length must be greater than zero"));
        }
        if runtime
            .limits()
            .offline_frames()
            .is_some_and(|limit| length > limit)
        {
            return Err(JsErrorBox::range_error(
                "offline render frame limit exceeded",
            ));
        }
        let samples = number_of_channels
            .checked_mul(length)
            .ok_or_else(|| JsErrorBox::range_error("offline render sample count overflowed"))?;
        if runtime
            .limits()
            .offline_samples()
            .is_some_and(|limit| samples > limit)
        {
            return Err(JsErrorBox::range_error(
                "offline render logical sample limit exceeded",
            ));
        }
        let output_bytes = samples
            .checked_mul(size_of::<f32>())
            .ok_or_else(|| JsErrorBox::range_error("offline render byte count overflowed"))?;
        let validated_sample_rate = validate_sample_rate(sample_rate)?;
        if runtime
            .limits()
            .sample_rate()
            .is_some_and(|limit| sample_rate > f64::from(limit))
        {
            return Err(JsErrorBox::range_error(
                "configured offline sample-rate limit exceeded",
            ));
        }
        let sample_rate = validated_sample_rate;
        let isolate_job = runtime.try_reserve_offline_render_job()?;
        let host_job = runtime
            .host()
            .try_reserve(AudioHostResourceKind::OfflineRenderJobs, 1)
            .map_err(|error| {
                JsErrorBox::range_error(format!(
                    "AudioHost offline-render-job limit exceeded: {error}"
                ))
            })?;
        let byte_budget = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
        let output_bytes = byte_budget.reserve(output_bytes).map_err(|error| {
            JsErrorBox::range_error(format!(
                "offline render output byte reservation failed: {error}"
            ))
        })?;

        Ok(NativePhase0OfflineRenderRequest {
            payload: Mutex::new(Some(OfflineRenderPayload {
                number_of_channels,
                length,
                sample_rate,
                isolate_thread: thread::current().id(),
                _output_bytes: output_bytes,
                _isolate_job: isolate_job,
                _host_job: host_job,
            })),
        })
    }
}

impl NativePhase0OfflineRenderRequest {
    fn take(&self) -> Result<OfflineRenderPayload, JsErrorBox> {
        self.payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| JsErrorBox::generic("offline render request has already been consumed"))
    }
}

/// Phase 0 proof op. Context construction, graph mutation, rendering, and
/// destruction all occur on the blocking worker after reservations transfer.
#[op2]
#[serde]
pub(crate) async fn op_phase0_render_off_thread(
    #[cppgc] request: &NativePhase0OfflineRenderRequest,
) -> Result<Phase0RenderSummary, JsErrorBox> {
    let payload = request.take()?;
    resolve_blocking("offline audio render", move || {
        blocking_outcome(|| {
            let OfflineRenderPayload {
                number_of_channels,
                length,
                sample_rate,
                isolate_thread,
                _output_bytes,
                _isolate_job,
                _host_job,
            } = payload;
            let mut context = OfflineAudioContext::new(number_of_channels, length, sample_rate);
            let mut oscillator = context.create_oscillator();
            oscillator.frequency().set_value(440.0);
            oscillator.connect(&context.destination());
            oscillator.start();

            let rendered = context.start_rendering_sync();
            let peak = rendered
                .get_channel_data(0)
                .iter()
                .copied()
                .map(f32::abs)
                .fold(0.0_f32, f32::max);
            Ok(Phase0RenderSummary {
                number_of_channels: u32::try_from(rendered.number_of_channels())
                    .map_err(|_| "rendered channel count exceeds the binding range".to_owned())?,
                length: u32::try_from(rendered.length())
                    .map_err(|_| "rendered frame count exceeds the binding range".to_owned())?,
                peak: f64::from(peak),
                off_isolate_thread: thread::current().id() != isolate_thread,
            })
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use deno_core::futures::{executor, join, FutureExt as _};
    use deno_core::{JsRuntime, RuntimeOptions};

    use super::*;

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("hostile panic payload destructor");
        }
    }

    #[test]
    fn blocking_panic_boundary_contains_a_hostile_payload() {
        let outcome = blocking_outcome(|| -> Result<(), String> {
            std::panic::panic_any(PanicOnDrop);
        });
        assert!(matches!(outcome, BlockingOutcome::Panicked));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn decode_and_render_ops_leave_the_isolate_thread() {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![
                deno_webidl::deno_webidl::init(),
                deno_web::deno_web::init(
                    deno_web::BlobStore::default_arc(),
                    None,
                    Default::default(),
                    deno_web::InMemoryBroadcastChannel::default(),
                ),
                crate::deno_audio::init(crate::AudioExtensionOptions::default()),
            ],
            // V8 flags are process-global and this unit-test binary also runs
            // CppGC reclamation tests. Every test-created isolate must agree
            // on exposing the testing GC regardless of scheduling order.
            unsafe_expose_natives_and_gc: true,
            ..RuntimeOptions::default()
        });

        runtime
            .execute_script(
                "phase0_off_thread.js",
                r#"
                  globalThis.__phase0OffThreadDone = false;
                  globalThis.__phase0OffThread = (async () => {
                    const bytes = new Uint8Array(52);
                    const view = new DataView(bytes.buffer);
                    const text = (offset, value) => {
                      for (let i = 0; i < value.length; i++) {
                        bytes[offset + i] = value.charCodeAt(i);
                      }
                    };
                    text(0, "RIFF"); view.setUint32(4, 44, true);
                    text(8, "WAVEfmt "); view.setUint32(16, 16, true);
                    view.setUint16(20, 1, true); view.setUint16(22, 1, true);
                    view.setUint32(24, 48000, true); view.setUint32(28, 96000, true);
                    view.setUint16(32, 2, true); view.setUint16(34, 16, true);
                    text(36, "data"); view.setUint32(40, 8, true);
                    view.setInt16(44, 0, true); view.setInt16(46, 8192, true);
                    view.setInt16(48, -8192, true); view.setInt16(50, 0, true);

                    const request = new Deno.core.ops.NativePhase0DecodeRequest(
                      bytes,
                      48000,
                    );
                    const decoded = await Deno.core.ops.op_phase0_decode_off_thread(request);
                    if (decoded.numberOfChannels !== 1 || decoded.length !== 4 ||
                        decoded.sampleRate !== 48000 || !decoded.offIsolateThread) {
                      throw new Error(`unexpected decode summary: ${JSON.stringify(decoded)}`);
                    }

                    const renderRequest = new Deno.core.ops.NativePhase0OfflineRenderRequest(
                      1,
                      128,
                      48000,
                    );
                    const rendered = await Deno.core.ops.op_phase0_render_off_thread(renderRequest);
                    if (rendered.numberOfChannels !== 1 || rendered.length !== 128 ||
                        rendered.peak <= 0.01 || !rendered.offIsolateThread) {
                      throw new Error(`unexpected render summary: ${JSON.stringify(rendered)}`);
                    }
                    globalThis.__phase0OffThreadDone = true;
                  })();
                "#,
            )
            .expect("the owned Phase 0 requests should enter the async op boundary");
        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .expect("decode and render promises should settle");
        runtime
            .execute_script(
                "phase0_off_thread_result.js",
                r#"
                  if (globalThis.__phase0OffThreadDone !== true) {
                    throw new Error("off-thread proof did not complete");
                  }
                "#,
            )
            .expect("the off-thread proof promise should be fulfilled");
    }

    #[test]
    fn offline_suspend_and_resume_observe_the_render_quantum() {
        let context = Arc::new(OfflineAudioContext::new(1, 512, 44_100.0));
        let resumed_context = Arc::clone(&context);
        let suspend = context.suspend(128.0 / 44_100.0).then(|()| async move {
            let mut source = resumed_context.create_constant_source();
            source.connect(&resumed_context.destination());
            source.start();
            resumed_context.resume().await;
        });
        let render = context.start_rendering();
        let rendered = executor::block_on(async move { join!(suspend, render).1 });

        assert!(rendered.get_channel_data(0)[..128]
            .iter()
            .all(|sample| *sample == 0.0));
        assert!(rendered.get_channel_data(0)[128..]
            .iter()
            .all(|sample| (*sample - 1.0).abs() <= f32::EPSILON));
    }
}
