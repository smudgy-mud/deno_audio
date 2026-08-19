//! Phase 0D acceptance evidence for a shared host-supplied process mixer.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use deno_audio::{AudioExtensionOptions, AudioHost};
use deno_core::futures::channel::oneshot;
use deno_core::{JsRuntime, RuntimeOptions};
use web_audio_api::context::{
    AudioContext, AudioContextOptions, AudioContextShutdownOutcome, AudioContextState,
    AudioContextStateChangeOutcome, BaseAudioContext,
};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode};
use web_audio_api::output::{
    AudioOutputConfig, AudioOutputDeathReason, AudioOutputEndpointShutdown, AudioOutputError,
    AudioOutputErrorKind, AudioOutputEventSink, AudioOutputFactory, AudioOutputRequest,
    AudioOutputStartFailure, AudioRenderCallback, AudioRenderFormat, AudioRenderStatus,
    PreparedAudioOutput, RunningAudioOutput,
};

const CHANNELS: usize = 2;
const FRAMES: usize = 128;
const MAX_SAMPLES: usize = CHANNELS * FRAMES;
const MAX_LOGICAL_INPUTS: usize = 2;
const NATIVE_SAMPLE: f32 = 0.125;

struct MixCapture {
    samples: usize,
    input_count: usize,
    ids: [u64; MAX_LOGICAL_INPUTS],
    mixed: [f32; MAX_SAMPLES],
    inputs: [[f32; MAX_SAMPLES]; MAX_LOGICAL_INPUTS],
}

impl MixCapture {
    fn new() -> Self {
        Self {
            samples: 0,
            input_count: 0,
            ids: [0; MAX_LOGICAL_INPUTS],
            mixed: [0.0; MAX_SAMPLES],
            inputs: [[0.0; MAX_SAMPLES]; MAX_LOGICAL_INPUTS],
        }
    }
}

struct MixerInput {
    callback: AudioRenderCallback,
    events: AudioOutputEventSink,
    retired: Option<oneshot::Sender<Result<(), AudioOutputError>>>,
    suspended: bool,
    scratch: Box<[f32; MAX_SAMPLES]>,
}

enum MixerCommand {
    Add {
        id: u64,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
        retired: oneshot::Sender<Result<(), AudioOutputError>>,
        accepted: mpsc::SyncSender<MixerStartResponse>,
    },
    SetSuspended {
        id: u64,
        suspended: bool,
        applied: mpsc::SyncSender<Result<(), AudioOutputError>>,
    },
    Remove {
        id: u64,
    },
    Render {
        frames: usize,
        capture: Box<MixCapture>,
        completed: mpsc::SyncSender<Box<MixCapture>>,
    },
    ShutdownFactory {
        completed: mpsc::SyncSender<()>,
    },
}

enum MixerStartResponse {
    Accepted,
    Rejected {
        error: AudioOutputError,
        callback: AudioRenderCallback,
    },
}

#[derive(Default)]
struct MixerMetrics {
    physical_threads: AtomicUsize,
    active_logical_inputs: AtomicUsize,
    peak_logical_inputs: AtomicUsize,
}

struct PhysicalThread<'a>(&'a MixerMetrics);

impl Drop for PhysicalThread<'_> {
    fn drop(&mut self) {
        self.0.physical_threads.store(0, Ordering::Release);
    }
}

struct MixerInner {
    sender: mpsc::SyncSender<MixerCommand>,
    join: Mutex<Option<JoinHandle<()>>>,
    metrics: Arc<MixerMetrics>,
}

impl Drop for MixerInner {
    fn drop(&mut self) {
        let Some(join) = self
            .join
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let (completed, receiver) = mpsc::sync_channel(0);
        let _ = self
            .sender
            .send(MixerCommand::ShutdownFactory { completed });
        let _ = receiver.recv();
        let _ = join.join();
    }
}

#[derive(Clone)]
struct FakeProcessMixer {
    inner: Arc<MixerInner>,
}

impl FakeProcessMixer {
    fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel(8);
        let metrics = Arc::new(MixerMetrics::default());
        let thread_metrics = Arc::clone(&metrics);
        let join = thread::Builder::new()
            .name("deno-audio-fake-process-mixer".to_owned())
            .spawn(move || run_mixer(&receiver, &thread_metrics))
            .expect("fake process mixer should spawn once");
        metrics.physical_threads.store(1, Ordering::Release);
        Self {
            inner: Arc::new(MixerInner {
                sender,
                join: Mutex::new(Some(join)),
                metrics,
            }),
        }
    }

    fn render(&self, frames: usize) -> Box<MixCapture> {
        assert!((1..=FRAMES).contains(&frames));
        let (completed, receiver) = mpsc::sync_channel(0);
        self.inner
            .sender
            .send(MixerCommand::Render {
                frames,
                capture: Box::new(MixCapture::new()),
                completed,
            })
            .expect("mixer should accept bounded render work");
        receiver.recv().expect("mixer render should complete")
    }

    fn shutdown_factory(&self) {
        let Some(join) = self
            .inner
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        let (completed, receiver) = mpsc::sync_channel(0);
        self.inner
            .sender
            .send(MixerCommand::ShutdownFactory { completed })
            .expect("mixer owner should accept its one shutdown");
        receiver.recv().expect("mixer shutdown should commit");
        join.join().expect("mixer owner should retire cleanly");
    }

    fn active_logical_inputs(&self) -> usize {
        self.inner
            .metrics
            .active_logical_inputs
            .load(Ordering::Acquire)
    }
}

fn retire_input(mut input: MixerInput, result: Result<(), AudioOutputError>) {
    drop(input.callback);
    if let Some(retired) = input.retired.take() {
        let _ = retired.send(result);
    }
}

fn run_mixer(receiver: &mpsc::Receiver<MixerCommand>, metrics: &MixerMetrics) {
    let _physical_thread = PhysicalThread(metrics);
    let mut inputs = BTreeMap::<u64, MixerInput>::new();
    while let Ok(command) = receiver.recv() {
        match command {
            MixerCommand::Add {
                id,
                callback,
                events,
                retired,
                accepted,
            } => {
                if inputs.contains_key(&id) || inputs.len() == MAX_LOGICAL_INPUTS {
                    let error = AudioOutputError::new(
                        AudioOutputErrorKind::NotSupported,
                        "fake mixer logical-input capacity exhausted",
                    );
                    let _ = events.report_endpoint_death(AudioOutputDeathReason::BackendFailure);
                    drop(retired);
                    let _ = accepted.send(MixerStartResponse::Rejected { error, callback });
                    continue;
                }
                inputs.insert(
                    id,
                    MixerInput {
                        callback,
                        events,
                        retired: Some(retired),
                        suspended: false,
                        scratch: Box::new([0.0; MAX_SAMPLES]),
                    },
                );
                let active = inputs.len();
                metrics
                    .active_logical_inputs
                    .store(active, Ordering::Release);
                metrics
                    .peak_logical_inputs
                    .fetch_max(active, Ordering::AcqRel);
                let _ = accepted.send(MixerStartResponse::Accepted);
            }
            MixerCommand::SetSuspended {
                id,
                suspended,
                applied,
            } => {
                let result = inputs.get_mut(&id).map_or_else(
                    || {
                        Err(AudioOutputError::new(
                            AudioOutputErrorKind::DeviceUnavailable,
                            "fake mixer input has retired",
                        ))
                    },
                    |input| {
                        input.suspended = suspended;
                        Ok(())
                    },
                );
                let _ = applied.send(result);
            }
            MixerCommand::Remove { id } => {
                if let Some(input) = inputs.remove(&id) {
                    retire_input(input, Ok(()));
                    metrics
                        .active_logical_inputs
                        .store(inputs.len(), Ordering::Release);
                }
            }
            MixerCommand::Render {
                frames,
                capture,
                completed,
            } => render_inputs(&mut inputs, frames, capture, &completed),
            MixerCommand::ShutdownFactory { completed } => {
                for input in std::mem::take(&mut inputs).into_values() {
                    let _ = input
                        .events
                        .report_endpoint_death(AudioOutputDeathReason::FactoryShutdown);
                    retire_input(input, Ok(()));
                }
                metrics.active_logical_inputs.store(0, Ordering::Release);
                let _ = completed.send(());
                return;
            }
        }
    }

    for input in inputs.into_values() {
        let error = AudioOutputError::new(
            AudioOutputErrorKind::Shutdown,
            "fake mixer owner disconnected",
        );
        let _ = input
            .events
            .report_endpoint_death(AudioOutputDeathReason::BackendFailure);
        retire_input(input, Err(error));
    }
}

fn render_inputs(
    inputs: &mut BTreeMap<u64, MixerInput>,
    frames: usize,
    mut capture: Box<MixCapture>,
    completed: &mpsc::SyncSender<Box<MixCapture>>,
) {
    let samples = frames * CHANNELS;
    capture.samples = samples;
    capture.mixed[..samples].fill(NATIVE_SAMPLE);
    for (id, input) in inputs {
        if input.suspended || capture.input_count == MAX_LOGICAL_INPUTS {
            continue;
        }
        input.scratch[..samples].fill(0.0);
        if input
            .callback
            .render_interleaved_f32(&mut input.scratch[..samples])
            == AudioRenderStatus::Stop
        {
            continue;
        }
        let index = capture.input_count;
        capture.ids[index] = *id;
        capture.inputs[index][..samples].copy_from_slice(&input.scratch[..samples]);
        for (mixed, sample) in capture.mixed[..samples]
            .iter_mut()
            .zip(&input.scratch[..samples])
        {
            *mixed += *sample;
        }
        capture.input_count += 1;
    }
    let _ = completed.send(capture);
}

struct FakePrepared {
    mixer: FakeProcessMixer,
    id: u64,
    config: AudioOutputConfig,
}

impl PreparedAudioOutput for FakePrepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let (retired, retirement) = oneshot::channel();
        let (accepted, response) = mpsc::sync_channel(0);
        let command = MixerCommand::Add {
            id: self.id,
            callback,
            events: events.clone(),
            retired,
            accepted,
        };
        if let Err(mpsc::SendError(command)) = self.mixer.inner.sender.send(command) {
            let MixerCommand::Add { callback, .. } = command else {
                unreachable!("the failed command is the exact Add payload")
            };
            let error = AudioOutputError::new(
                AudioOutputErrorKind::DeviceUnavailable,
                "fake mixer owner is unavailable",
            );
            let _ = events.report_endpoint_death(AudioOutputDeathReason::FactoryShutdown);
            return Err(AudioOutputStartFailure::new(
                error,
                AudioOutputEndpointShutdown::from_future(async move {
                    drop(callback);
                    Ok(())
                }),
            ));
        }
        match response.recv() {
            Ok(MixerStartResponse::Accepted) => Ok(Box::new(FakeRunning {
                mixer: self.mixer,
                id: self.id,
                retirement,
            })),
            Ok(MixerStartResponse::Rejected { error, callback }) => {
                drop(retirement);
                Err(AudioOutputStartFailure::new(
                    error,
                    AudioOutputEndpointShutdown::from_future(async move {
                        drop(callback);
                        Ok(())
                    }),
                ))
            }
            Err(_) => Err(AudioOutputStartFailure::new(
                AudioOutputError::new(
                    AudioOutputErrorKind::BackendSpecific,
                    "fake mixer failed before acknowledging start",
                ),
                retirement_future(retirement),
            )),
        }
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

fn retirement_future(
    retirement: oneshot::Receiver<Result<(), AudioOutputError>>,
) -> AudioOutputEndpointShutdown {
    AudioOutputEndpointShutdown::from_future(async move {
        retirement.await.unwrap_or_else(|_| {
            Err(AudioOutputError::new(
                AudioOutputErrorKind::Shutdown,
                "fake mixer lost callback-retirement proof",
            ))
        })
    })
}

struct FakeRunning {
    mixer: FakeProcessMixer,
    id: u64,
    retirement: oneshot::Receiver<Result<(), AudioOutputError>>,
}

impl FakeRunning {
    fn set_suspended(&self, suspended: bool) -> Result<(), AudioOutputError> {
        let (applied, response) = mpsc::sync_channel(0);
        self.mixer
            .inner
            .sender
            .send(MixerCommand::SetSuspended {
                id: self.id,
                suspended,
                applied,
            })
            .map_err(|_| {
                AudioOutputError::new(
                    AudioOutputErrorKind::DeviceUnavailable,
                    "fake mixer owner is unavailable",
                )
            })?;
        response.recv().unwrap_or_else(|_| {
            Err(AudioOutputError::new(
                AudioOutputErrorKind::BackendSpecific,
                "fake mixer lost transition acknowledgement",
            ))
        })
    }
}

impl RunningAudioOutput for FakeRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        self.set_suspended(false)
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        self.set_suspended(true)
    }

    fn shutdown(self: Box<Self>) -> AudioOutputEndpointShutdown {
        let _ = self
            .mixer
            .inner
            .sender
            .send(MixerCommand::Remove { id: self.id });
        retirement_future(self.retirement)
    }
}

impl AudioOutputFactory for FakeProcessMixer {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        if request.number_of_channels() != CHANNELS
            || request.requested_sample_rate() != Some(48_000.0)
        {
            return Err(AudioOutputError::new(
                AudioOutputErrorKind::NotSupported,
                "fake mixer requires exact stereo 48 kHz input",
            ));
        }
        let format = AudioRenderFormat::new(
            request.requested_sample_rate().unwrap_or(48_000.0),
            request.number_of_channels(),
            FRAMES,
        )?;
        Ok(Box::new(FakePrepared {
            mixer: self.clone(),
            id: request.context_id().get(),
            config: AudioOutputConfig::new(format, request.sink_id(), 0.0)?,
        }))
    }
}

fn context(mixer: &FakeProcessMixer) -> AudioContext {
    let factory: Arc<dyn AudioOutputFactory> = Arc::new(mixer.clone());
    AudioContext::builder(factory)
        .options(AudioContextOptions {
            sample_rate: Some(48_000.0),
            sink_id: "none".to_owned(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(CHANNELS)
        .build()
        .expect("fake mixer context should build")
}

fn attach_tone(context: &AudioContext, frequency: f32, gain_value: f32) {
    let mut oscillator = context.create_oscillator();
    oscillator.frequency().set_value(frequency);
    let gain = context.create_gain();
    gain.gain().set_value(gain_value);
    oscillator.connect(&gain);
    gain.connect(&context.destination());
    oscillator.start();
    // The exact hosted graph roots an accepted started source through its
    // connected tail even after these control handles leave scope.
}

fn render_until_two_inputs(mixer: &FakeProcessMixer) -> Box<MixCapture> {
    for _ in 0..32 {
        let capture = mixer.render(FRAMES);
        if capture.input_count == 2
            && capture.inputs[..2]
                .iter()
                .all(|input| input[..capture.samples].iter().any(|sample| *sample != 0.0))
        {
            return capture;
        }
    }
    panic!("both logical graphs should become audible within 32 quanta");
}

fn drive_state_change(
    mixer: &FakeProcessMixer,
    context: &AudioContext,
    target: AudioContextState,
    suspend: bool,
) {
    let receipt = if suspend {
        context.request_suspend()
    } else {
        context.request_resume()
    }
    .expect("state transition should be admitted");
    for _ in 0..64 {
        if context.state() == target {
            break;
        }
        let _ = mixer.render(FRAMES);
        thread::yield_now();
    }
    assert_eq!(context.state(), target);
    assert!(matches!(
        receipt.wait(),
        AudioContextStateChangeOutcome::Applied | AudioContextStateChangeOutcome::Unchanged
    ));
}

fn drive_close(mixer: &FakeProcessMixer, context: &AudioContext) {
    let receipt = context.request_close().expect("close should be admitted");
    for _ in 0..64 {
        if context.state() == AudioContextState::Closed {
            break;
        }
        let _ = mixer.render(FRAMES);
        thread::yield_now();
    }
    assert!(matches!(
        receipt.wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    assert_eq!(context.state(), AudioContextState::Closed);
}

#[test]
fn one_physical_surrogate_mixes_three_isolated_inputs_and_retires_independently() {
    let mixer = FakeProcessMixer::new();
    let first = context(&mixer);
    let second = context(&mixer);
    attach_tone(&first, 330.0, 0.25);
    attach_tone(&second, 550.0, 0.5);

    let capture = render_until_two_inputs(&mixer);
    assert_eq!(
        mixer.inner.metrics.physical_threads.load(Ordering::Acquire),
        1
    );
    assert_eq!(mixer.active_logical_inputs(), 2);
    assert_eq!(
        mixer
            .inner
            .metrics
            .peak_logical_inputs
            .load(Ordering::Acquire),
        2
    );
    for sample in 0..capture.samples {
        let expected = NATIVE_SAMPLE + capture.inputs[0][sample] + capture.inputs[1][sample];
        assert!((capture.mixed[sample] - expected).abs() <= f32::EPSILON * 2.0);
    }

    let second_time = second.current_time();
    drive_state_change(&mixer, &first, AudioContextState::Suspended, true);
    assert_eq!(first.state(), AudioContextState::Suspended);
    let suspended_time = first.current_time();
    let suspended = mixer.render(FRAMES);
    assert_eq!(suspended.input_count, 1);
    assert!((first.current_time() - suspended_time).abs() <= f64::EPSILON);
    assert!(second.current_time() > second_time);
    for sample in 0..suspended.samples {
        let expected = NATIVE_SAMPLE + suspended.inputs[0][sample];
        assert!((suspended.mixed[sample] - expected).abs() <= f32::EPSILON);
    }

    drive_state_change(&mixer, &first, AudioContextState::Running, false);
    assert_eq!(render_until_two_inputs(&mixer).input_count, 2);
    drive_close(&mixer, &first);
    assert_eq!(mixer.active_logical_inputs(), 1);
    let after_first_close = mixer.render(FRAMES);
    assert_eq!(after_first_close.input_count, 1);
    drive_close(&mixer, &second);
    assert_eq!(mixer.active_logical_inputs(), 0);
    let native_only = mixer.render(FRAMES);
    assert_eq!(native_only.input_count, 0);
    assert!(native_only.mixed[..native_only.samples]
        .iter()
        .all(|sample| *sample == NATIVE_SAMPLE));
    mixer.shutdown_factory();
    assert_eq!(
        mixer.inner.metrics.physical_threads.load(Ordering::Acquire),
        0
    );
}

#[test]
fn factory_shutdown_is_authoritative_death_for_every_logical_context() {
    let mixer = FakeProcessMixer::new();
    let first = context(&mixer);
    let second = context(&mixer);
    let first_receipt = first.shutdown_receipt().unwrap();
    let second_receipt = second.shutdown_receipt().unwrap();
    assert_eq!(mixer.active_logical_inputs(), 2);

    mixer.shutdown_factory();
    assert!(matches!(
        first_receipt.wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    assert!(matches!(
        second_receipt.wait(),
        AudioContextShutdownOutcome::Confirmed(_)
    ));
    assert_eq!(first.state(), AudioContextState::Closed);
    assert_eq!(second.state(), AudioContextState::Closed);
    assert_eq!(mixer.active_logical_inputs(), 0);
    assert_eq!(
        mixer.inner.metrics.physical_threads.load(Ordering::Acquire),
        0
    );
}

#[test]
fn rejected_third_input_retires_through_the_failed_build_receipt() {
    let mixer = FakeProcessMixer::new();
    let first = context(&mixer);
    let second = context(&mixer);

    let Err(error) = AudioContext::builder(Arc::new(mixer.clone()))
        .options(AudioContextOptions {
            sample_rate: Some(48_000.0),
            sink_id: "none".to_owned(),
            ..AudioContextOptions::default()
        })
        .number_of_channels(CHANNELS)
        .build()
    else {
        panic!("a third logical mixer input exceeded the fixed capacity");
    };
    assert_eq!(
        error.output_error().map(AudioOutputError::kind),
        Some(AudioOutputErrorKind::NotSupported)
    );
    let cleanup = error
        .cleanup_receipt()
        .expect("post-prepare rejection must retain a cleanup observer");
    assert!(matches!(
        cleanup.wait(),
        AudioContextShutdownOutcome::Confirmed(report)
            if report.reclaim_issue().is_none() && report.event_issue().is_none()
    ));
    assert_eq!(mixer.active_logical_inputs(), 2);

    drive_close(&mixer, &first);
    drive_close(&mixer, &second);
    mixer.shutdown_factory();
}

#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "the injected-mixer acceptance scenario keeps its driver and JavaScript FIFO timeline together"
)]
async fn public_audio_context_uses_the_same_injected_mixer_and_flushes_staged_graph_fifo() {
    let mixer = FakeProcessMixer::new();
    let host = Arc::new(AudioHost::default());
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(
                AudioExtensionOptions::new(Arc::clone(&host))
                    .output_factory(Arc::new(mixer.clone())),
            ),
        ],
        ..RuntimeOptions::default()
    });

    let driving = Arc::new(AtomicBool::new(true));
    let audible = Arc::new(AtomicBool::new(false));
    let driver_mixer = mixer.clone();
    let driver_running = Arc::clone(&driving);
    let driver_audible = Arc::clone(&audible);
    let driver = thread::Builder::new()
        .name("deno-audio-public-mixer-driver".to_owned())
        .spawn(move || {
            while driver_running.load(Ordering::Acquire) {
                let capture = driver_mixer.render(FRAMES);
                if capture.inputs[..capture.input_count]
                    .iter()
                    .any(|input| input[..capture.samples].iter().any(|sample| *sample != 0.0))
                {
                    driver_audible.store(true, Ordering::Release);
                }
                thread::yield_now();
            }
        })
        .expect("public mixer driver should spawn");

    let begin = runtime.execute_script(
        "phase2_public_injected_mixer.js",
        r#"
              globalThis.__publicMixerScenario = (async () => {
                const context = new AudioContext({
                  sampleRate: 48000,
                  sinkId: "none",
                });
                await context.suspend();
                if (context.state !== "suspended") {
                  throw new Error("public context did not suspend");
                }
                const oscillator = context.createOscillator();
                const gain = context.createGain();
                oscillator.connect(gain);
                oscillator.disconnect(gain);
                oscillator.connect(gain);
                gain.connect(context.destination);
                gain.gain.value = 0.25;
                oscillator.type = "square";
                oscillator.type = "triangle";
                let ended = 0;
                oscillator.onended = () => ended++;
                const start = context.currentTime;
                const stop = start + 0.03;
                oscillator.start(start);
                oscillator.stop(stop);
                await context.resume();
                while (context.currentTime < stop + 0.03) {}
                await context.close();
                if (ended !== 1 || context.state !== "closed") {
                  throw new Error(`public staged retirement mismatch: ${ended}/${context.state}`);
                }
                globalThis.__publicMixerDone = true;
              })();
            "#,
    );
    let settle = if begin.is_ok() {
        Some(
            runtime
                .run_event_loop(deno_core::PollEventLoopOptions::default())
                .await,
        )
    } else {
        None
    };
    driving.store(false, Ordering::Release);
    driver.join().expect("public mixer driver should join");
    begin.expect("the public injected-mixer scenario should begin");
    settle
        .expect("a started scenario has an event-loop result")
        .expect("the public injected-mixer scenario should settle");
    runtime
        .execute_script(
            "phase2_public_injected_mixer_result.js",
            "if (globalThis.__publicMixerDone !== true) throw new Error('public mixer scenario did not finish');",
        )
        .expect("the public injected-mixer scenario should finish successfully");

    assert!(
        audible.load(Ordering::Acquire),
        "the resumed public staged graph never reached the injected mixer"
    );
    assert_eq!(mixer.active_logical_inputs(), 0);
    let usage = host.usage();
    assert_eq!(usage.online_contexts(), 0);
    assert_eq!(usage.graph_nodes(), 0);
    assert_eq!(usage.graph_connections(), 0);
    assert_eq!(usage.scheduled_sources(), 0);
    assert_eq!(usage.queued_control_commands(), 0);
    mixer.shutdown_factory();
}
