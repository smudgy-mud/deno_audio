//! Phase 2 acceptance evidence for the public exact hosted graph.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use deno_audio::{
    AudioEventPumpLiveness, AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLimits,
    AudioPermissions,
};
use deno_core::{JsRuntime, RuntimeOptions};
use deno_error::JsErrorBox;
use web_audio_api::output::{
    AudioOutputError, AudioOutputFactory, AudioOutputRequest, PreparedAudioOutput,
    SystemAudioOutput,
};

struct CountingSystemOutput {
    calls: Arc<AtomicUsize>,
    system: SystemAudioOutput,
}

impl AudioOutputFactory for CountingSystemOutput {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.system.prepare(request)
    }
}

struct DenyPlayback;

impl AudioPermissions for DenyPlayback {
    fn check_playback(&self, _api_name: &'static str) -> Result<(), JsErrorBox> {
        Err(JsErrorBox::generic(
            "NotAllowedError: audio playback is denied by the embedder",
        ))
    }

    fn check_capture(&self, _api_name: &'static str) -> Result<(), JsErrorBox> {
        Ok(())
    }
}

fn runtime_with_options(options: AudioExtensionOptions) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(options),
        ],
        unsafe_expose_natives_and_gc: true,
        ..RuntimeOptions::default()
    })
}

fn runtime(host: Arc<AudioHost>, limits: AudioLimits) -> JsRuntime {
    runtime_with_options(
        AudioExtensionOptions::new(host)
            .limits(limits)
            .output_factory(Arc::new(SystemAudioOutput::new())),
    )
}

fn wait_for_online_retirement(host: &AudioHost) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let usage = host.usage();
        if usage.online_contexts() == 0
            && usage.graph_nodes() == 0
            && usage.graph_connections() == 0
            && usage.scheduled_sources() == 0
            && usage.queued_control_commands() == 0
            && usage.queued_events() == 0
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "public online graph resources did not retire: {usage:?}"
        );
        std::thread::yield_now();
    }
}

#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one public JavaScript scenario keeps lifecycle, graph, event, and post-close assertions in a single ownership timeline"
)]
async fn public_online_graph_runs_lifecycle_and_exact_ended() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_online_happy_path.js",
            r#"
              globalThis.__phase2Online = (async () => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const assert = (condition, message) => {
                  if (!condition) throw new Error(message);
                };
                const throws = (callback, name) => {
                  let error;
                  try { callback(); } catch (caught) { error = caught; }
                  assert(error instanceof DOMException,
                    `expected DOMException, got ${error}`);
                  assert(error.name === name, `expected ${name}, got ${error.name}`);
                };

                assert(AudioContext.prototype instanceof BaseAudioContext,
                  "AudioContext inheritance");
                assert(AudioContext.length === 0 && AudioContext.name === "AudioContext",
                  "AudioContext constructor shape");
                let illegalConstructor;
                try { AudioContext(); } catch (error) { illegalConstructor = error; }
                assert(illegalConstructor instanceof TypeError,
                  "AudioContext requires construction");
                const context = new AudioContext({
                  latencyHint: "interactive",
                  sampleRate: 48000,
                  sinkId: { type: "none" },
                });
                globalThis.__phase2Context = context;
                assert(context.state === "running", "initial running state");
                assert(Object.prototype.toString.call(context) === "[object AudioContext]",
                  "AudioContext toStringTag");
                assert(context.sampleRate === 48000, "accepted sample rate");
                assert(context.sinkId === "none", "accepted silent sink");
                assert(context.baseLatency === 0 && context.outputLatency === 0,
                  "silent latency facts");
                assert(context.destination instanceof AudioDestinationNode,
                  "online destination");

                let stateChanges = 0;
                context.onstatechange = () => stateChanges++;
                const gain = new GainNode(context, { gain: 0.5 });
                const oscillator = new OscillatorNode(context, {
                  type: "triangle",
                  frequency: 220,
                  detune: 5,
                });
                globalThis.__phase2Oscillator = oscillator;
                gain.gain.value = 0.25;
                assert(gain.gain.value === 0.25, "exact scalar AudioParam value");
                assert(oscillator.type === "triangle" &&
                    oscillator.frequency.value === 220 &&
                    oscillator.detune.value === 5,
                  "exact oscillator options");

                throws(() => gain.channelCount = 1, "NotSupportedError");
                assert(gain.channelCount === 2, "rejected channel mutation was inert");
                throws(() => gain.gain.setValueAtTime(0.75, 0), "NotSupportedError");
                assert(gain.gain.value === 0.25, "rejected automation was inert");
                throws(
                  () => new OscillatorNode(context, { type: "custom" }),
                  "NotSupportedError",
                );

                assert(oscillator.connect(gain) === gain, "connect result");
                assert(oscillator.connect(gain) === gain, "duplicate connect result");
                gain.connect(context.destination);

                await context.suspend();
                assert(context.state === "suspended", "suspend acknowledgement");
                await context.resume();
                assert(context.state === "running", "resume acknowledgement");

                let ended = 0;
                oscillator.onended = () => ended++;
                const start = context.currentTime;
                const stop = start + 0.02;
                oscillator.start(start);
                oscillator.stop(stop);
                while (context.currentTime < stop + 0.03) {}

                const firstClose = context.close();
                assert(firstClose === context.close(), "close receipt is shared");
                await firstClose;
                assert(context.state === "closed", "confirmed closed state");
                assert(ended === 1, `expected one ended event, got ${ended}`);
                assert(stateChanges === 3,
                  `expected suspend/resume/close events, got ${stateChanges}`);
                assert(oscillator.type === "triangle" &&
                    oscillator.frequency.value === 220,
                  "ended wrapper properties remain readable");
                throws(() => gain.gain.value = 0.5, "InvalidStateError");
                throws(() => oscillator.connect(gain), "InvalidStateError");
                throws(() => context.createGain(), "InvalidStateError");
                globalThis.__phase2OnlineDone = true;
              })();
            "#,
        )
        .expect("the public online scenario should begin");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the public online scenario should settle");
    runtime
        .execute_script(
            "phase2_online_result.js",
            "if (globalThis.__phase2OnlineDone !== true) throw new Error('online scenario did not finish');",
        )
        .expect("the public online scenario should complete successfully");
    wait_for_online_retirement(&host);
}

#[test]
fn public_options_and_playback_permission_reject_before_output_prepare() {
    let host = Arc::new(AudioHost::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let output = Arc::new(CountingSystemOutput {
        calls: Arc::clone(&calls),
        system: SystemAudioOutput::new(),
    });
    let mut runtime = runtime_with_options(
        AudioExtensionOptions::new(Arc::clone(&host))
            .permissions(Arc::new(DenyPlayback))
            .output_factory(output),
    );
    runtime
        .execute_script(
            "phase2_online_permission.js",
            r#"
              const { DOMException } = Deno.core.loadExtScript(
                "ext:deno_web/01_dom_exception.js",
              );
              const assert = (condition, message) => {
                if (!condition) throw new Error(message);
              };
              let sampleError;
              try { new AudioContext({ sampleRate: 1, sinkId: "none" }); }
              catch (error) { sampleError = error; }
              assert(sampleError instanceof DOMException &&
                  sampleError.name === "NotSupportedError",
                `invalid sample rate mapped incorrectly: ${sampleError}`);
              let latencyError;
              try { new AudioContext({ latencyHint: 0, sinkId: "none" }); }
              catch (error) { latencyError = error; }
              assert(latencyError instanceof RangeError,
                `invalid latency mapped incorrectly: ${latencyError}`);
              let permissionError;
              try { new AudioContext({ sampleRate: 48000, sinkId: "none" }); }
              catch (error) { permissionError = error; }
              assert(permissionError instanceof DOMException &&
                  permissionError.name === "NotAllowedError",
                `permission denial mapped incorrectly: ${permissionError}`);
            "#,
        )
        .expect("public validation and permission denial should be catchable");
    assert_eq!(calls.load(Ordering::Acquire), 0);
    assert_eq!(host.usage().online_contexts(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn public_context_brands_reject_cross_graph_and_unsupported_options_inertly() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_online_brands.js",
            r#"
              const { DOMException } = Deno.core.loadExtScript(
                "ext:deno_web/01_dom_exception.js",
              );
              const first = new AudioContext({ sampleRate: 48000, sinkId: "none" });
              const second = new AudioContext({ sampleRate: 48000, sinkId: "none" });
              const oscillator = first.createOscillator();
              const gain = second.createGain();
              let foreign;
              try { oscillator.connect(gain); } catch (error) { foreign = error; }
              if (!(foreign instanceof DOMException) ||
                  foreign.name !== "InvalidAccessError") {
                throw new Error(`cross-context error mismatch: ${foreign}`);
              }
              let unsupported;
              try { new GainNode(first, { channelCount: 1 }); }
              catch (error) { unsupported = error; }
              if (!(unsupported instanceof DOMException) ||
                  unsupported.name !== "NotSupportedError") {
                throw new Error(`unsupported option mismatch: ${unsupported}`);
              }
              globalThis.__phase2BrandClose = Promise.all([
                first.close(),
                second.close(),
              ]);
            "#,
        )
        .expect("public brand and unsupported-option checks should remain catchable");
    assert_eq!(host.usage().online_contexts(), 2);
    assert_eq!(host.usage().graph_nodes(), 5);
    assert_eq!(host.usage().graph_connections(), 0);
    assert_eq!(host.usage().scheduled_sources(), 0);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("both public contexts should close");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn public_online_disconnect_overloads_preserve_exact_no_match_semantics() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_online_disconnect_overloads.js",
            r#"
              const { DOMException } = Deno.core.loadExtScript(
                "ext:deno_web/01_dom_exception.js",
              );
              const context = new AudioContext({ sampleRate: 48000, sinkId: "none" });
              const oscillator = context.createOscillator();
              const gain = context.createGain();
              const expectInvalidAccess = (callback) => {
                let error;
                try { callback(); } catch (caught) { error = caught; }
                if (!(error instanceof DOMException) ||
                    error.name !== "InvalidAccessError") {
                  throw new Error(`expected InvalidAccessError, got ${error}`);
                }
              };

              oscillator.connect(gain);
              oscillator.disconnect(gain);
              expectInvalidAccess(() => oscillator.disconnect(gain));

              oscillator.connect(gain);
              oscillator.disconnect(0);
              oscillator.disconnect(0);

              oscillator.connect(gain);
              oscillator.disconnect(gain, 0);
              expectInvalidAccess(() => oscillator.disconnect(gain, 0));

              oscillator.connect(gain);
              oscillator.disconnect(gain, 0, 0);
              expectInvalidAccess(() => oscillator.disconnect(gain, 0, 0));

              oscillator.connect(gain.gain);
              oscillator.disconnect(gain.gain);
              expectInvalidAccess(() => oscillator.disconnect(gain.gain));

              oscillator.disconnect();
              globalThis.__phase2DisconnectClose = context.close();
            "#,
        )
        .expect("every public online disconnect selector should execute");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the disconnect-overload context should close");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn exact_connection_limits_are_lazy_and_close_reclaims_every_guard() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_online_contexts(Some(1))
            .max_graph_nodes(Some(5))
            .max_graph_connections(Some(2)),
    ));
    let limits = AudioLimits::default()
        .max_online_contexts(Some(1))
        .max_graph_nodes(Some(5))
        .max_graph_connections(Some(2));
    let mut runtime = runtime(Arc::clone(&host), limits);
    runtime
        .execute_script(
            "phase2_online_limits.js",
            r#"
              const context = new AudioContext({ sampleRate: 48000, sinkId: "none" });
              const oscillator = context.createOscillator();
              const gain = context.createGain();
              oscillator.connect(gain);
              oscillator.connect(gain);
              gain.connect(context.destination);
              oscillator.start();
              let quota;
              try { oscillator.connect(context.destination); } catch (error) { quota = error; }
              if (quota?.name !== "QuotaExceededError") {
                throw new Error(`expected QuotaExceededError, got ${quota}`);
              }
              oscillator.connect(gain);
              globalThis.__phase2LimitedContext = context;
            "#,
        )
        .expect("lazy exact connection admission should preserve duplicate success");
    let usage = host.usage();
    assert_eq!(usage.online_contexts(), 1);
    assert_eq!(usage.graph_nodes(), 5);
    assert_eq!(usage.graph_connections(), 2);
    assert_eq!(usage.scheduled_sources(), 1);

    runtime
        .execute_script(
            "phase2_online_limits_close.js",
            r"
              globalThis.__phase2LimitedClose = (async () => {
                await globalThis.__phase2LimitedContext.close();
                globalThis.__phase2LimitedClosed = true;
              })();
            ",
        )
        .expect("limited context close should begin");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("limited context close should settle");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn buffer_assignment_reserves_before_pcm_acquisition_and_recovers_after_resume() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_graph_nodes(Some(23))
            .max_queued_control_commands(Some(28)),
    ));
    let limits = AudioLimits::default()
        .max_graph_nodes(Some(23))
        .max_queued_control_commands(Some(28));
    let mut runtime = runtime(Arc::clone(&host), limits);
    runtime
        .execute_script(
            "phase3_buffer_assignment_reservation.js",
            r#"
              globalThis.__phase3BufferReservation = (async () => {
                const context = new AudioContext({ sampleRate: 48000, sinkId: "none" });
                await context.suspend();
                const buffer = context.createBuffer(1, 8, 48000);
                const exposed = buffer.getChannelData(0);
                exposed[1] = 0.75;
                const held = [
                  context.createBufferSource(),
                  context.createBufferSource(),
                  context.createBufferSource(),
                ];
                const source = context.createBufferSource();
                let quota;
                try { source.buffer = buffer; } catch (error) { quota = error; }
                if (quota?.name !== "QuotaExceededError") {
                  throw new Error(`expected bounded buffer admission failure, got ${quota}`);
                }
                if (exposed.length !== 8 || source.buffer !== null) {
                  throw new Error("rejected buffer assignment acquired or retained PCM");
                }
                await context.resume();
                if (held.length !== 3) throw new Error("construction holders disappeared");
                const retryDeadline = context.currentTime + 0.5;
                while (source.buffer === null) {
                  try {
                    source.buffer = buffer;
                  } catch (error) {
                    if (error?.name !== "QuotaExceededError") throw error;
                    if (context.currentTime >= retryDeadline) {
                      throw new Error("buffer command capacity did not recover");
                    }
                  }
                }
                if (exposed.length !== 0 || source.buffer !== buffer) {
                  throw new Error("retried buffer assignment did not acquire PCM");
                }
                await context.close();
                globalThis.__phase3BufferReservationDone = true;
              })();
            "#,
        )
        .expect("bounded buffer assignment scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("bounded buffer assignment scenario should settle");
    runtime
        .execute_script(
            "phase3_buffer_assignment_reservation_result.js",
            "if (globalThis.__phase3BufferReservationDone !== true) throw new Error('buffer reservation scenario did not finish');",
        )
        .expect("bounded buffer assignment scenario should finish");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unbounded_started_source_does_not_pin_the_deno_event_loop() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_unref_ended_pump.js",
            r#"
              const context = new AudioContext({ sampleRate: 48000, sinkId: "none" });
              const oscillator = context.createOscillator();
              oscillator.connect(context.destination);
              let ended = 0;
              oscillator.onended = () => ended++;
              oscillator.start();
              globalThis.__phase2UnrefContext = context;
              globalThis.__phase2UnrefOscillator = oscillator;
              globalThis.__phase2UnrefEnded = () => ended;
            "#,
        )
        .expect("an unbounded public source should start");
    let started = Instant::now();
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the ended reconciler should be unreferenced");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an open source pinned the Deno event loop"
    );

    runtime
        .execute_script(
            "phase2_unref_close.js",
            r#"
              globalThis.__phase2UnrefClose = (async () => {
                await globalThis.__phase2UnrefContext.close();
                if (globalThis.__phase2UnrefEnded() !== 1) {
                  throw new Error("close did not retire the started source exactly once");
                }
                globalThis.__phase2UnrefClosed = true;
              })();
            "#,
        )
        .expect("unbounded source close should begin");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("unbounded source close should settle");
    runtime
        .execute_script(
            "phase2_unref_result.js",
            "if (globalThis.__phase2UnrefClosed !== true) throw new Error('close did not settle');",
        )
        .expect("unbounded source close should complete");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn a_started_buffer_source_is_rooted_until_native_completion_then_reconciled() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase3_buffer_source_root.js",
            r#"
              globalThis.__phase3BufferContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__phase3BufferEnded = 0;
              (() => {
                const buffer = __phase3BufferContext.createBuffer(1, 480, 48000);
                buffer.getChannelData(0).fill(0.25);
                const source = __phase3BufferContext.createBufferSource();
                source.buffer = buffer;
                source.connect(__phase3BufferContext.destination);
                source.onended = () => __phase3BufferEnded++;
                source.start();
              })();
            "#,
        )
        .expect("a finite BufferSource should start without an external wrapper root");
    assert_eq!(host.usage().scheduled_sources(), 1);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the unreferenced event pump should let the runtime idle");

    let deadline = Instant::now() + Duration::from_secs(5);
    while host.usage().scheduled_sources() != 0 {
        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .expect("the unreferenced completion pump should remain healthy");
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            Instant::now() < deadline,
            "native completion did not release the scheduled-source root"
        );
        std::thread::yield_now();
    }

    runtime
        .execute_script(
            "phase3_buffer_source_close.js",
            r"
              globalThis.__phase3BufferClose = (async () => {
                await globalThis.__phase3BufferContext.close();
                if (globalThis.__phase3BufferEnded !== 1) {
                  throw new Error(
                    `BufferSource ended reconciliation mismatch: ${globalThis.__phase3BufferEnded}`,
                  );
                }
                globalThis.__phase3BufferClosed = true;
              })();
            ",
        )
        .expect("the BufferSource context should begin closing");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the BufferSource context should close");
    runtime
        .execute_script(
            "phase3_buffer_source_close_result.js",
            "if (globalThis.__phase3BufferClosed !== true) throw new Error('BufferSource close did not settle');",
        )
        .expect("the BufferSource close should finish");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn null_buffer_sources_reconcile_every_ended_without_wake_capacity() {
    const SOURCE_COUNT: usize = 3;

    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_graph_nodes(Some(32))
            .max_scheduled_sources(Some(SOURCE_COUNT))
            .max_queued_events(Some(0)),
    ));
    let limits = AudioLimits::unlimited()
        .max_graph_nodes(Some(32))
        .max_scheduled_sources(Some(SOURCE_COUNT))
        .max_queued_events(Some(0));
    let mut runtime = runtime_with_options(
        AudioExtensionOptions::new(Arc::clone(&host))
            .limits(limits)
            .event_pump_liveness(AudioEventPumpLiveness::Ref)
            .output_factory(Arc::new(SystemAudioOutput::new())),
    );
    runtime
        .execute_script(
            "phase3_null_buffer_source_saturation.js",
            r#"
              globalThis.__phase3NullBuffer = (async () => {
                const context = new AudioContext({ sampleRate: 48000, sinkId: "none" });
                const counts = [0, 0, 0];
                const sources = [];
                let resolveAll;
                const allEnded = new Promise((resolve) => resolveAll = resolve);
                for (let i = 0; i < counts.length; ++i) {
                  const source = context.createBufferSource();
                  source.onended = () => {
                    counts[i]++;
                    if (counts.every((count) => count === 1)) resolveAll();
                  };
                  source.start();
                  sources.push(source);
                }
                const blockedUntil = context.currentTime + 0.05;
                while (context.currentTime < blockedUntil) {}
                globalThis.__phase3NullBufferContext = context;
                globalThis.__phase3NullBufferSources = sources;
                globalThis.__phase3NullBufferCounts = counts;
                await allEnded;
                if (!counts.every((count) => count === 1)) {
                  throw new Error(`null-buffer ended counts mismatch: ${counts}`);
                }
                await context.close();
                globalThis.__phase3NullBufferDone = true;
              })();
            "#,
        )
        .expect("null-buffer wake-saturation scenario should start");
    assert_eq!(host.usage().queued_events(), 0);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("null-buffer completion reconciliation should settle");
    runtime
        .execute_script(
            "phase3_null_buffer_source_saturation_result.js",
            "if (globalThis.__phase3NullBufferDone !== true) throw new Error('null-buffer reconciliation did not finish');",
        )
        .expect("all null-buffer sources should dispatch ended exactly once");
    drop(runtime);
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn an_embedder_can_keep_the_event_pump_referenced_until_close() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime_with_options(
        AudioExtensionOptions::new(Arc::clone(&host))
            .event_pump_liveness(AudioEventPumpLiveness::Ref)
            .output_factory(Arc::new(SystemAudioOutput::new())),
    );
    runtime
        .execute_script(
            "phase2_refed_event_pump.js",
            r#"
              globalThis.__phase2RefedContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
            "#,
        )
        .expect("a referenced event pump should construct");

    let idle = tokio::time::timeout(
        Duration::from_millis(100),
        runtime.run_event_loop(deno_core::PollEventLoopOptions::default()),
    )
    .await;
    assert!(
        idle.is_err(),
        "a referenced event pump let the runtime go idle"
    );

    runtime
        .execute_script(
            "phase2_refed_event_pump_close.js",
            r"
              globalThis.__phase2RefedClose = (async () => {
                await globalThis.__phase2RefedContext.close();
                globalThis.__phase2RefedClosed = true;
              })();
            ",
        )
        .expect("the referenced context should begin closing");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("close should retire the referenced event pump");
    runtime
        .execute_script(
            "phase2_refed_event_pump_result.js",
            "if (globalThis.__phase2RefedClosed !== true) throw new Error('referenced close did not settle');",
        )
        .expect("the referenced context should close");
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn the_online_event_pump_does_not_root_an_unclosed_public_context() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_collect_unclosed_context.js",
            r#"
              globalThis.__phase2CollectContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__phase2CollectContext = undefined;
            "#,
        )
        .expect("an unclosed public context should become unreachable");
    assert_eq!(host.usage().online_contexts(), 1);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
        tokio::time::sleep(Duration::from_millis(30)).await;
        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .expect("the unreferenced online event pump should remain healthy");
        if host.usage().online_contexts() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the online event pump kept an unreachable AudioContext alive"
        );
    }
    wait_for_online_retirement(&host);
}

#[tokio::test(flavor = "current_thread")]
async fn an_unstarted_public_source_is_not_rooted_by_the_context() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase2_unstarted_source.js",
            r#"
              globalThis.__phase2UnstartedContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__phase2UnstartedSource =
                globalThis.__phase2UnstartedContext.createOscillator();
            "#,
        )
        .expect("an unstarted public source should construct");
    assert_eq!(host.usage().graph_nodes(), 3);
    assert_eq!(host.usage().scheduled_sources(), 0);

    runtime
        .execute_script(
            "phase2_drop_unstarted_source.js",
            "globalThis.__phase2UnstartedSource = undefined;",
        )
        .expect("the unstarted source wrapper should become unreachable");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
        if host.usage().graph_nodes() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "an unstarted source remained rooted by its context"
        );
        std::thread::yield_now();
    }
    assert_eq!(host.usage().scheduled_sources(), 0);

    runtime
        .execute_script(
            "phase2_close_after_unstarted_gc.js",
            r"
              globalThis.__phase2UnstartedClose =
                globalThis.__phase2UnstartedContext.close();
            ",
        )
        .expect("context close should begin after source collection");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("context close should settle after source collection");
    wait_for_online_retirement(&host);
}
