//! Phase 1 acceptance evidence for the public bounded offline graph.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioLimits};
use deno_core::{JsRuntime, RuntimeOptions};

fn runtime(host: Arc<AudioHost>, limits: AudioLimits) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(AudioExtensionOptions::new(host).limits(limits)),
        ],
        unsafe_expose_natives_and_gc: true,
        ..RuntimeOptions::default()
    })
}

fn wait_for_zero_graph_usage(host: &AudioHost) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let usage = host.usage();
        if usage.graph_nodes() == 0
            && usage.graph_connections() == 0
            && usage.scheduled_sources() == 0
            && usage.automation_events() == 0
            && usage.queued_control_commands() == 0
        {
            return;
        }
        assert!(Instant::now() < deadline, "graph resources did not retire");
        std::thread::yield_now();
    }
}

fn collect_until_live_bytes(runtime: &mut JsRuntime, host: &AudioHost, expected: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
        if host.usage().live_audio_bytes() == expected {
            return;
        }
        assert!(Instant::now() < deadline, "audio bytes did not retire");
        std::thread::yield_now();
    }
}

fn collect_until_all_public_graph_resources_retire(runtime: &mut JsRuntime, host: &AudioHost) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
        let usage = host.usage();
        if usage.live_audio_bytes() == 0
            && usage.graph_nodes() == 0
            && usage.graph_connections() == 0
            && usage.scheduled_sources() == 0
            && usage.automation_events() == 0
            && usage.queued_control_commands() == 0
            && usage.offline_render_jobs() == 0
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "discarded public graph resources did not retire"
        );
        std::thread::yield_now();
    }
}

/// V8 posts delayed garbage-collector tasks that `deno_core` schedules on the
/// tokio handle captured at `JsRuntime` construction and aborts the process
/// when none exists; synchronous tests enter this runtime's context before
/// constructing a `JsRuntime`.
fn tokio_context() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("the test tokio runtime must build")
}

#[tokio::test(flavor = "current_thread")]
async fn public_offline_buffer_source_acquires_pcm_and_renders_once() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase3_offline_buffer_source.js",
            r#"
              globalThis.__phase3OfflineBuffer = (async () => {
                const assert = (condition, message) => {
                  if (!condition) throw new Error(message);
                };
                const context = new OfflineAudioContext(1, 8, 3000);
                const buffer = context.createBuffer(1, 8, 3000);
                const exposed = buffer.getChannelData(0);
                exposed[0] = 0.25;
                exposed[1] = -0.5;
                const source = context.createBufferSource();
                assert(source instanceof AudioBufferSourceNode,
                  "createBufferSource result");
                assert(source instanceof AudioScheduledSourceNode,
                  "BufferSource inheritance");
                source.buffer = buffer;
                assert(exposed.length === 0, "buffer assignment acquired exposed PCM");
                assert(source.buffer === buffer, "buffer identity");
                assert(source.playbackRate.value === 1 && source.detune.value === 0,
                  "BufferSource AudioParams");
                source.loopStart = 0.001;
                source.loopEnd = 0.002;
                source.loop = true;
                assert(source.loop && source.loopStart === 0.001 &&
                    source.loopEnd === 0.002, "BufferSource loop mirrors");
                source.loop = false;
                let duplicateBuffer;
                try { source.buffer = buffer; } catch (error) { duplicateBuffer = error; }
                assert(duplicateBuffer?.name === "InvalidStateError",
                  "second bounded buffer assignment must reject");
                source.connect(context.destination);
                let ended = 0;
                source.onended = () => ended++;
                source.start();
                let duplicateStart;
                try { source.start(); } catch (error) { duplicateStart = error; }
                assert(duplicateStart?.name === "InvalidStateError",
                  "duplicate BufferSource start must reject");
                const rendered = await context.startRendering();
                const output = rendered.getChannelData(0);
                assert(Math.abs(output[0] - 0.25) < 0.00001,
                  `first sample mismatch: ${output[0]}`);
                assert(Math.abs(output[1] + 0.5) < 0.00001,
                  `second sample mismatch: ${output[1]}`);
                assert(ended === 1, `expected one ended event, got ${ended}`);
                globalThis.__phase3OfflineBufferDone = true;
              })();
            "#,
        )
        .expect("offline BufferSource scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("offline BufferSource scenario should settle");
    runtime
        .execute_script(
            "phase3_offline_buffer_source_result.js",
            "if (globalThis.__phase3OfflineBufferDone !== true) throw new Error('offline BufferSource did not finish');",
        )
        .expect("offline BufferSource scenario should finish");
    collect_until_all_public_graph_resources_retire(&mut runtime, &host);
}

#[tokio::test(flavor = "current_thread")]
async fn public_offline_buffer_source_routes_loop_boundaries_into_rendering() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase3_offline_buffer_source_loop.js",
            r"
              globalThis.__phase3OfflineLoop = (async () => {
                const context = new OfflineAudioContext(1, 6, 3000);
                const buffer = context.createBuffer(1, 4, 3000);
                buffer.copyToChannel(new Float32Array([0.1, 0.2, 0.3, 0.4]), 0);
                const source = new AudioBufferSourceNode(context, { buffer });
                source.loop = true;
                source.loopStart = 1 / 3000;
                source.loopEnd = 3 / 3000;
                source.connect(context.destination);
                source.start(0, 0, 6 / 3000);
                const rendered = await context.startRendering();
                const actual = rendered.getChannelData(0);
                // The selected engine interpolates at loop points using the
                // following sample. This sequence distinguishes the requested
                // inner loop from a whole-buffer repeat while pinning that
                // native convention at the public boundary.
                const expected = [0.1, 0.2, 0.3, 0.4, 0.3, 0.4];
                for (let i = 0; i < expected.length; ++i) {
                  if (Math.abs(actual[i] - expected[i]) > 0.00001) {
                    throw new Error(
                      `loop sample ${i} mismatch: expected ${expected[i]}, got ${actual[i]}; all=${Array.from(actual)}`,
                    );
                  }
                }
                globalThis.__phase3OfflineLoopDone = true;
              })();
            ",
        )
        .expect("offline BufferSource loop scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("offline BufferSource loop scenario should settle");
    runtime
        .execute_script(
            "phase3_offline_buffer_source_loop_result.js",
            "if (globalThis.__phase3OfflineLoopDone !== true) throw new Error('offline BufferSource loop did not finish');",
        )
        .expect("offline BufferSource loop scenario should finish");
    collect_until_all_public_graph_resources_retire(&mut runtime, &host);
}

#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one embedded JavaScript scenario keeps the related Web IDL hierarchy and overload assertions in one realm"
)]
async fn public_offline_graph_has_brands_overloads_and_closed_connection_semantics() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase1_offline_surface.js",
            r#"
              (() => {
                const { DOMException } = Deno.core.loadExtScript(
                  "ext:deno_web/01_dom_exception.js",
                );
                const assert = (condition, message) => {
                  if (!condition) throw new Error(message);
                };
                const throws = (callback, constructor, name) => {
                  let error;
                  try { callback(); } catch (caught) { error = caught; }
                  assert(error instanceof constructor,
                    `expected ${constructor.name}, got ${error}`);
                  if (name !== undefined) {
                    assert(error.name === name,
                      `expected ${name}, got ${error.name}`);
                  }
                };

                for (const name of [
                  "BaseAudioContext",
                  "OfflineAudioContext",
                  "AudioNode",
                  "AudioDestinationNode",
                  "GainNode",
                  "AudioScheduledSourceNode",
                  "AudioBufferSourceNode",
                  "ConstantSourceNode",
                  "OscillatorNode",
                  "PeriodicWave",
                  "AudioParam",
                  "OfflineAudioCompletionEvent",
                ]) {
                  assert(typeof globalThis[name] === "function", `${name} is missing`);
                  const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
                  assert(descriptor.writable && descriptor.configurable &&
                      !descriptor.enumerable, `${name} global descriptor mismatch`);
                  assert(globalThis[name].prototype[Symbol.toStringTag] === name,
                    `${name} toStringTag mismatch`);
                }
                assert(OfflineAudioContext.length === 1, "offline constructor length");
                assert(GainNode.length === 1 && OscillatorNode.length === 1 &&
                    AudioBufferSourceNode.length === 1 &&
                    ConstantSourceNode.length === 1 && PeriodicWave.length === 1,
                  "node constructor length");
                assert(BaseAudioContext.prototype.createBufferSource.length === 0 &&
                    AudioBufferSourceNode.prototype.start.length === 0,
                  "BufferSource method lengths");
                assert(AudioNode.prototype.connect.length === 1,
                  "connect overload length");
                assert(AudioNode.prototype.disconnect.length === 0,
                  "disconnect overload length");
                throws(() => new BaseAudioContext(), TypeError);
                throws(() => new AudioNode(), TypeError);
                throws(() => new AudioParam(), TypeError);
                throws(() => new OfflineAudioContext(), TypeError);
                throws(
                  () => new OfflineAudioContext({ length: 0, sampleRate: 48000 }),
                  DOMException,
                  "NotSupportedError",
                );

                const context = new OfflineAudioContext(2, 256, 48000);
                assert(context instanceof BaseAudioContext, "context inheritance");
                assert(context.state === "suspended" && context.currentTime === 0,
                  "initial context state");
                assert(context.length === 256 && context.sampleRate === 48000,
                  "offline dimensions");
                assert(context.destination instanceof AudioDestinationNode,
                  "destination type");
                assert(context.destination === context.destination,
                  "destination same-object");
                assert(context.destination.numberOfInputs === 1 &&
                    context.destination.numberOfOutputs === 0 &&
                    context.destination.maxChannelCount === 2,
                  "destination ports");
                assert(context.destination.channelCount === 2 &&
                    context.destination.channelCountMode === "explicit",
                  "destination channel config");
                throws(() => context.destination.channelCount = 1,
                  DOMException, "InvalidStateError");
                throws(() => context.destination.channelCountMode = "max",
                  DOMException, "InvalidStateError");

                const gain = context.createGain();
                const constructedGain = new GainNode(context, {
                  gain: 0.25,
                  channelCount: 1,
                  channelCountMode: "explicit",
                  channelInterpretation: "discrete",
                });
                assert(gain instanceof GainNode && gain instanceof AudioNode,
                  "gain inheritance");
                assert(gain.gain === gain.gain && gain.gain instanceof AudioParam,
                  "gain parameter same-object");
                assert(constructedGain.gain.value === 0.25 &&
                    constructedGain.channelCount === 1 &&
                    constructedGain.channelCountMode === "explicit" &&
                    constructedGain.channelInterpretation === "discrete",
                  "GainOptions were not applied");
                gain.channelCount = 1;
                gain.channelCountMode = "clamped-max";
                gain.channelInterpretation = "discrete";
                assert(gain.channelCount === 1 &&
                    gain.channelCountMode === "clamped-max" &&
                    gain.channelInterpretation === "discrete",
                  "channel mutations were not reflected");

                const oscillator = new OscillatorNode(context, {
                  type: "triangle",
                  frequency: 220,
                  detune: 5,
                });
                assert(oscillator instanceof AudioScheduledSourceNode &&
                    oscillator instanceof AudioNode,
                  "oscillator inheritance");
                assert(oscillator.numberOfInputs === 0 && oscillator.numberOfOutputs === 1,
                  "oscillator ports");
                assert(oscillator.type === "triangle" &&
                    oscillator.frequency.value === 220 &&
                    oscillator.detune.value === 5,
                  "OscillatorOptions were not applied");
                oscillator.type = "square";
                assert(oscillator.type === "square", "oscillator type setter");
                throws(() => oscillator.type = "custom",
                  DOMException, "InvalidStateError");
                throws(() => new OscillatorNode(context, { type: "custom" }),
                  DOMException, "InvalidStateError");
                throws(() => oscillator.setPeriodicWave({}),
                  TypeError);
                const wave = new PeriodicWave(context, { real: [0, 1] });
                oscillator.setPeriodicWave(wave);
                assert(oscillator.type === "custom", "PeriodicWave type mirror");
                const custom = new OscillatorNode(context, {
                  type: "sine",
                  periodicWave: wave,
                });
                assert(custom.type === "custom", "PeriodicWave constructor precedence");
                const constant = new ConstantSourceNode(context, { offset: -0.25 });
                assert(constant instanceof AudioScheduledSourceNode &&
                    constant.offset.value === -0.25 &&
                    constant.numberOfInputs === 0 && constant.numberOfOutputs === 1,
                  "ConstantSource surface");

                assert(oscillator.connect(gain) === gain, "connect return value");
                assert(oscillator.connect(gain) === gain, "duplicate connect");
                oscillator.disconnect(gain);
                throws(() => oscillator.disconnect(gain),
                  DOMException, "InvalidAccessError");
                oscillator.connect(gain);
                oscillator.disconnect(undefined);
                throws(() => oscillator.disconnect(gain),
                  DOMException, "InvalidAccessError");
                oscillator.connect(gain);
                gain.connect(gain.gain);
                gain.disconnect(gain.gain);
                gain.connect(context.destination);
                throws(() => context.destination.connect(gain),
                  DOMException, "IndexSizeError");

                const other = new OfflineAudioContext({
                  numberOfChannels: 2,
                  length: 128,
                  sampleRate: 48000,
                });
                throws(() => oscillator.connect(other.destination),
                  DOMException, "InvalidAccessError");
                throws(() => oscillator.connect(gain, 1),
                  DOMException, "IndexSizeError");
                throws(() => oscillator.start(-1), RangeError);
                throws(() => oscillator.stop(), DOMException, "InvalidStateError");
                oscillator.start();
                throws(() => oscillator.start(), DOMException, "InvalidStateError");
                oscillator.stop(0.002);

                const param = gain.gain;
                param.value = 0.5;
                assert(param.value === 0.5 && param.defaultValue === 1,
                  "AudioParam value/default");
                assert(param.setValueAtTime(0.5, 0) === param &&
                    param.linearRampToValueAtTime(0.75, 0.001) === param &&
                    param.exponentialRampToValueAtTime(0.5, 0.002) === param &&
                    param.setTargetAtTime(0.25, 0.003, 0.001) === param &&
                    param.setValueCurveAtTime([0.25, 0.5], 0.004, 0.001) === param &&
                    param.cancelScheduledValues(0.005) === param &&
                    param.cancelAndHoldAtTime(0.005) === param,
                  "AudioParam fluent automation");
                throws(() => param.exponentialRampToValueAtTime(0, 1), RangeError);
                throws(() => param.setValueCurveAtTime([1], 0, 1),
                  DOMException, "InvalidStateError");
                throws(() => param.cancelScheduledValues(-1), RangeError);
                let iteratorClosed = false;
                const unboundedCurve = {
                  [Symbol.iterator]() {
                    return {
                      next() { return { done: false, value: 0.5 }; },
                      return() {
                        iteratorClosed = true;
                        return { done: true };
                      },
                    };
                  },
                };
                throws(() => param.setValueCurveAtTime(unboundedCurve, 0, 1),
                  DOMException, "QuotaExceededError");
                assert(iteratorClosed, "bounded sequence did not close its iterator");

                const buffer = context.createBuffer(2, 4, 48000);
                assert(buffer instanceof AudioBuffer && buffer.numberOfChannels === 2,
                  "createBuffer result");
                globalThis.__phase1OfflineContexts = [context, other];
              })();
            "#,
        )
        .expect("the public offline graph surface should be coherent");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the four reservation families share one compact construction/rejection/retirement matrix"
)]
fn graph_resource_limits_reject_before_mutation_and_recover_on_drop() {
    let node_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&node_host),
            AudioLimits::unlimited().max_graph_nodes(Some(13)),
        );
        runtime
            .execute_script(
                "phase1_offline_node_limit.js",
                r#"
                  const context = new OfflineAudioContext(1, 128, 48000);
                  globalThis.__limitedGain = context.createGain();
                  let error;
                  try { context.createOscillator(); } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("graph-node limit did not reject");
                  if (globalThis.__limitedGain.gain.value !== 1) {
                    throw new Error("accepted graph was mutated by rejected construction");
                  }
                "#,
            )
            .expect("graph-node limit should reject cleanly");
        assert_eq!(node_host.usage().graph_nodes(), 13);
    }
    wait_for_zero_graph_usage(&node_host);

    let connection_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&connection_host),
            AudioLimits::unlimited().max_graph_connections(Some(1)),
        );
        runtime
            .execute_script(
                "phase1_offline_connection_limit.js",
                r#"
                  const context = new OfflineAudioContext(1, 128, 48000);
                  const first = context.createGain();
                  const second = context.createGain();
                  first.connect(context.destination);
                  let error;
                  try { second.connect(context.destination); } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("connection limit did not reject");
                  first.disconnect(context.destination);
                  second.connect(context.destination);
                  globalThis.__limitedConnections = [context, first, second];
                "#,
            )
            .expect("connection reservation should recover after disconnect");
        assert_eq!(connection_host.usage().graph_connections(), 1);
    }
    wait_for_zero_graph_usage(&connection_host);

    let automation_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&automation_host),
            AudioLimits::unlimited().max_automation_events(Some(1)),
        );
        runtime
            .execute_script(
                "phase1_offline_automation_limit.js",
                r#"
                  const context = new OfflineAudioContext(1, 128, 48000);
                  const gain = context.createGain();
                  gain.gain.value = 0.5;
                  let error;
                  try { gain.gain.setValueAtTime(0.25, 0); } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("automation limit did not reject");
                  if (gain.gain.value !== 0.5) {
                    throw new Error("rejected automation mutated the AudioParam");
                  }
                  globalThis.__limitedAutomation = [context, gain];
                "#,
            )
            .expect("automation reservation should reject before mutation");
        assert_eq!(automation_host.usage().automation_events(), 1);
        assert_eq!(automation_host.usage().queued_control_commands(), 28);
    }
    wait_for_zero_graph_usage(&automation_host);

    let curve_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&curve_host),
            AudioLimits::unlimited().max_live_audio_bytes(Some(7)),
        );
        runtime
            .execute_script(
                "phase1_offline_curve_byte_limit.js",
                r#"
                  const context = new OfflineAudioContext(1, 128, 48000);
                  const gain = context.createGain();
                  let error;
                  try {
                    gain.gain.setValueCurveAtTime([0, 1], 0, 1);
                  } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("automation byte limit did not reject");
                  globalThis.__curveLimitedGraph = [context, gain];
                "#,
            )
            .expect("value-curve bytes should reject before the native copy");
        assert_eq!(curve_host.usage().live_audio_bytes(), 0);
        assert_eq!(curve_host.usage().automation_events(), 0);
        assert_eq!(curve_host.usage().queued_control_commands(), 27);
    }
    wait_for_zero_graph_usage(&curve_host);

    let command_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&command_host),
            AudioLimits::unlimited().max_queued_control_commands(Some(21)),
        );
        runtime
            .execute_script(
                "phase1_offline_command_limit.js",
                r#"
                  globalThis.__commandLimitedContext =
                    new OfflineAudioContext(1, 128, 48000);
                  let error;
                  try {
                    globalThis.__commandLimitedContext.createGain();
                  } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("command limit did not reject");
                "#,
            )
            .expect("control-command limit should reject before construction");
        assert_eq!(command_host.usage().graph_nodes(), 11);
        assert_eq!(command_host.usage().queued_control_commands(), 21);
    }
    wait_for_zero_graph_usage(&command_host);
}

#[tokio::test(flavor = "current_thread")]
async fn offline_render_produces_pcm_and_dispatches_completion_and_ended_once() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase1_offline_render.js",
            r#"
              globalThis.__offlineResult = { settled: false };
              (async () => {
                const context = new OfflineAudioContext({
                  numberOfChannels: 1,
                  length: 512,
                  sampleRate: 48000,
                });
                const oscillator = context.createOscillator();
                const gain = context.createGain();
                oscillator.frequency.value = 440;
                gain.gain.value = 0.5;
                oscillator.connect(gain).connect(context.destination);
                let ended = 0;
                let completed = 0;
                let completionBuffer;
                let stateChanges = 0;
                oscillator.onended = () => ended++;
                context.onstatechange = () => stateChanges++;
                context.oncomplete = (event) => {
                  completed++;
                  if (!(event instanceof OfflineAudioCompletionEvent)) {
                    throw new Error("complete event type mismatch");
                  }
                  completionBuffer = event.renderedBuffer;
                };
                oscillator.start();
                oscillator.stop(0.005);
                const rendering = context.startRendering();
                if (context.state !== "running") {
                  throw new Error(`expected running, got ${context.state}`);
                }
                let replayRejected = false;
                try { await context.startRendering(); } catch (error) {
                  replayRejected = error.name === "InvalidStateError";
                }
                if (!replayRejected) throw new Error("render was replayable");
                const buffer = await rendering;
                if (context.state !== "closed") throw new Error("context did not close");
                if (buffer !== completionBuffer || completed !== 1 || ended !== 1) {
                  throw new Error("completion or ended delivery mismatch");
                }
                if (stateChanges < 2) throw new Error("missing statechange events");
                const data = buffer.getChannelData(0);
                let peak = 0;
                for (let i = 0; i < data.length; ++i) {
                  peak = Math.max(peak, Math.abs(data[i]));
                }
                if (peak < 0.1 || peak > 0.55) {
                  throw new Error(`unexpected rendered peak ${peak}`);
                }
                globalThis.__offlineResult = {
                  settled: true,
                  length: buffer.length,
                  peak,
                };
              })().catch((error) => {
                globalThis.__offlineResult = { settled: true, error };
              });
            "#,
        )
        .expect("offline render should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("offline render should settle");
    runtime
        .execute_script(
            "phase1_offline_render_result.js",
            r#"
              if (!globalThis.__offlineResult.settled) {
                throw new Error("offline render did not settle");
              }
              if (globalThis.__offlineResult.error !== undefined) {
                throw globalThis.__offlineResult.error;
              }
              if (globalThis.__offlineResult.length !== 512) {
                throw new Error("offline render length mismatch");
              }
            "#,
        )
        .expect("offline render should produce valid public PCM");
}

#[tokio::test(flavor = "current_thread")]
async fn rejected_output_reservation_leaves_the_public_graph_retryable() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited().max_live_audio_bytes(Some(512)),
    );
    runtime
        .execute_script(
            "phase1_offline_retry_setup.js",
            r#"
              globalThis.__heldPcm = new AudioBuffer({
                numberOfChannels: 1,
                length: 128,
                sampleRate: 48000,
              });
              globalThis.__retryContext = new OfflineAudioContext(1, 128, 48000);
              globalThis.__retryFirst = globalThis.__retryContext.startRendering()
                .then(() => { throw new Error("quota-constrained render succeeded"); })
                .catch((error) => {
                  if (error.name !== "QuotaExceededError") throw error;
                  if (globalThis.__retryContext.state !== "suspended") {
                    throw new Error("rejected render did not restore suspended state");
                  }
                });
            "#,
        )
        .expect("the first public render should be submitted");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the rejected public render should settle");
    assert_eq!(host.usage().live_audio_bytes(), 512);

    runtime
        .execute_script(
            "phase1_offline_retry_release.js",
            "globalThis.__heldPcm = undefined; globalThis.__retryFirst = undefined;",
        )
        .unwrap();
    collect_until_live_bytes(&mut runtime, &host, 0);

    runtime
        .execute_script(
            "phase1_offline_retry_render.js",
            r#"
              const oscillator = globalThis.__retryContext.createOscillator();
              oscillator.connect(globalThis.__retryContext.destination);
              oscillator.start();
              oscillator.stop(0.001);
              globalThis.__retrySecond = globalThis.__retryContext.startRendering()
                .then((buffer) => {
                  if (buffer.length !== 128 ||
                      globalThis.__retryContext.state !== "closed") {
                    throw new Error("retried render result was invalid");
                  }
                  globalThis.__retryBuffer = buffer;
                });
            "#,
        )
        .expect("the public graph should remain mutable and retryable");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the retried public render should settle");
    runtime
        .execute_script(
            "phase1_offline_retry_check.js",
            "if (globalThis.__retryBuffer?.length !== 128) throw new Error('retry failed');",
        )
        .unwrap();
    assert_eq!(host.usage().live_audio_bytes(), 512);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_public_render_outlives_a_discarded_javascript_waiter() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::default());
    runtime
        .execute_script(
            "phase1_offline_discard_waiter.js",
            r"
              (() => {
                const context = new OfflineAudioContext(1, 256, 48000);
                const oscillator = context.createOscillator();
                oscillator.connect(context.destination);
                oscillator.start();
                oscillator.stop(0.002);
                context.startRendering();
              })();
            ",
        )
        .expect("accepted render should not require a retained public promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("discarding the public waiter must not cancel native rendering");
    assert_eq!(host.usage().offline_render_jobs(), 0);
    collect_until_all_public_graph_resources_retire(&mut runtime, &host);
}

#[tokio::test(flavor = "current_thread")]
async fn constant_source_and_periodic_wave_render_through_the_public_offline_graph() {
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase2_offline_constant_periodic.js",
            r#"
              globalThis.__phase2OfflineSources = (async () => {
                const assert = (condition, message) => {
                  if (!condition) throw new Error(message);
                };
                const customContext = new OfflineAudioContext(1, 32, 48000);
                const wave = new PeriodicWave(customContext, {
                  real: [0, 0],
                  imag: [0, 1],
                  disableNormalization: true,
                });
                const custom = new OscillatorNode(customContext, {
                  frequency: 6000,
                  periodicWave: wave,
                });
                assert(custom.type === "custom", "custom constructor type");
                custom.connect(customContext.destination);
                custom.start();
                custom.stop(32 / 48000);

                const sineContext = new OfflineAudioContext(1, 32, 48000);
                const sine = new OscillatorNode(sineContext, {
                  type: "sine",
                  frequency: 6000,
                });
                sine.connect(sineContext.destination);
                sine.start();
                sine.stop(32 / 48000);

                let foreign;
                try { sine.setPeriodicWave(wave); } catch (error) { foreign = error; }
                assert(foreign?.name === "InvalidAccessError",
                  "foreign PeriodicWave must reject");

                const constantContext = new OfflineAudioContext(1, 32, 48000);
                const constant = constantContext.createConstantSource();
                constant.offset.value = 0.25;
                let ended = 0;
                constant.onended = () => ended++;
                constant.connect(constantContext.destination);
                constant.start();
                constant.stop(16 / 48000);

                const [customBuffer, sineBuffer, constantBuffer] = await Promise.all([
                  customContext.startRendering(),
                  sineContext.startRendering(),
                  constantContext.startRendering(),
                ]);
                const actual = customBuffer.getChannelData(0);
                const expected = sineBuffer.getChannelData(0);
                for (let i = 0; i < actual.length; ++i) {
                  assert(Math.abs(actual[i] - expected[i]) < 0.00001,
                    `custom sine mismatch at ${i}: ${actual[i]} vs ${expected[i]}`);
                }
                const constantOutput = constantBuffer.getChannelData(0);
                for (let i = 0; i < constantOutput.length; ++i) {
                  const expectedValue = i < 16 ? 0.25 : 0;
                  assert(Math.abs(constantOutput[i] - expectedValue) < 0.00001,
                    `constant sample ${i}: ${constantOutput[i]}`);
                }
                assert(ended === 1, `ConstantSource ended count ${ended}`);
                // PeriodicWave is a non-graph factory and remains available on a
                // context after its one-shot offline render closes.
                globalThis.__phase2ClosedWave = customContext.createPeriodicWave(
                  new Float32Array([0, 0]),
                  new Float32Array([0, 1]),
                );
                globalThis.__phase2OfflineSourcesDone = true;
              })();
            "#,
        )
        .expect("offline ConstantSource/PeriodicWave scenario should start");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("offline ConstantSource/PeriodicWave scenario should settle");
    runtime
        .execute_script(
            "phase2_offline_constant_periodic_result.js",
            "if (globalThis.__phase2OfflineSourcesDone !== true) throw new Error('offline source scenario did not finish'); globalThis.__phase2ClosedWave = undefined; globalThis.__phase2OfflineSources = undefined;",
        )
        .expect("offline ConstantSource/PeriodicWave scenario should finish");
    collect_until_all_public_graph_resources_retire(&mut runtime, &host);
}

#[test]
fn periodic_wave_reserves_coefficients_before_copy_and_storage_until_gc() {
    const TABLE_BYTES: usize = 32 * 1024;
    const TWO_COMPONENT_INPUT_BYTES: usize = 4 * std::mem::size_of::<f32>();

    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let rejected_host = Arc::new(AudioHost::default());
    {
        let mut runtime = runtime(
            Arc::clone(&rejected_host),
            AudioLimits::unlimited().max_live_audio_bytes(Some(TABLE_BYTES)),
        );
        runtime
            .execute_script(
                "phase2_periodic_wave_coefficient_limit.js",
                r#"
                  const context = new OfflineAudioContext(1, 8, 3000);
                  let error;
                  try {
                    new PeriodicWave(context, { real: [0, 0], imag: [0, 1] });
                  } catch (caught) { error = caught; }
                  if (error?.name !== "QuotaExceededError") throw error ??
                    new Error("coefficient reservation did not reject");
                "#,
            )
            .expect("temporary coefficient capacity should reject before copying");
        assert_eq!(rejected_host.usage().live_audio_bytes(), 0);
    }

    let accepted_host = Arc::new(AudioHost::default());
    let mut runtime = runtime(
        Arc::clone(&accepted_host),
        AudioLimits::unlimited()
            .max_live_audio_bytes(Some(TABLE_BYTES + TWO_COMPONENT_INPUT_BYTES)),
    );
    runtime
        .execute_script(
            "phase2_periodic_wave_storage_lease.js",
            r"
              const context = new OfflineAudioContext(1, 8, 3000);
              globalThis.__phase2HeldWave = new PeriodicWave(context, {
                real: [0, 0], imag: [0, 1],
              });
            ",
        )
        .expect("bounded PeriodicWave should construct");
    assert_eq!(accepted_host.usage().live_audio_bytes(), TABLE_BYTES);
    runtime
        .execute_script(
            "phase2_periodic_wave_storage_release.js",
            "globalThis.__phase2HeldWave = undefined;",
        )
        .unwrap();
    collect_until_live_bytes(&mut runtime, &accepted_host, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn periodic_wave_webidl_bounds_close_iterators_before_native_allocation() {
    const TABLE_BYTES: usize = 32 * 1024;
    let host = Arc::new(AudioHost::default());
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase2_periodic_wave_validation.js",
            r#"
              const context = new OfflineAudioContext(1, 8, 3000);
              const expect = (callback, name) => {
                let error;
                try { callback(); } catch (caught) { error = caught; }
                if (error?.name !== name) {
                  throw new Error(`expected ${name}, got ${error}`);
                }
              };
              expect(() => new PeriodicWave(context, { real: [0] }),
                "IndexSizeError");
              expect(() => new PeriodicWave(context, {
                real: [0, 1], imag: [0, 1, 2],
              }), "IndexSizeError");
              expect(() => new PeriodicWave(context, { real: [0, Infinity] }),
                "TypeError");
              expect(() => new PeriodicWave(null, {}), "TypeError");

              let iteratorClosed = false;
              const oversized = {
                [Symbol.iterator]() {
                  let index = 0;
                  return {
                    next() {
                      return index++ <= 8192
                        ? { value: 0, done: false }
                        : { value: undefined, done: true };
                    },
                    return() {
                      iteratorClosed = true;
                      return { value: undefined, done: true };
                    },
                  };
                },
              };
              expect(() => new PeriodicWave(context, { real: oversized }),
                "QuotaExceededError");
              if (!iteratorClosed) {
                throw new Error("bounded PeriodicWave conversion did not close its iterator");
              }
            "#,
        )
        .expect("PeriodicWave validation failures should be catchable");
    assert_eq!(host.usage().live_audio_bytes(), 0);

    runtime
        .execute_script(
            "phase2_periodic_wave_missing_counterparts.js",
            r"
              globalThis.__phase2WaveVariants = [
                new PeriodicWave(context),
                new PeriodicWave(context, { real: [0, 1] }),
                new PeriodicWave(context, { imag: [0, 1] }),
              ];
            ",
        )
        .expect("missing PeriodicWave coefficient arrays should be synthesized");
    assert_eq!(host.usage().live_audio_bytes(), 3 * TABLE_BYTES);
    runtime
        .execute_script(
            "phase2_periodic_wave_variant_release.js",
            "globalThis.__phase2WaveVariants = undefined;",
        )
        .unwrap();
    collect_until_live_bytes(&mut runtime, &host, 0);
}
