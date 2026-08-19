//! Phase 0D acceptance evidence for shared context permits and shutdown ownership.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioHostLimits};
use deno_core::{JsRuntime, RuntimeOptions};
use web_audio_api::output::{
    AudioOutputConfig, AudioOutputEndpointShutdown, AudioOutputError, AudioOutputErrorKind,
    AudioOutputEventSink, AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure,
    AudioRenderCallback, AudioRenderFormat, PreparedAudioOutput, RunningAudioOutput,
    SystemAudioOutput,
};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct CountingSystemFactory {
    calls: Arc<AtomicUsize>,
    system: SystemAudioOutput,
}

impl CountingSystemFactory {
    fn new(calls: Arc<AtomicUsize>) -> Self {
        Self {
            calls,
            system: SystemAudioOutput::new(),
        }
    }
}

impl AudioOutputFactory for CountingSystemFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.system.prepare(request)
    }
}

fn runtime(host: Arc<AudioHost>, output: Arc<dyn AudioOutputFactory>) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(AudioExtensionOptions::new(host).output_factory(output)),
        ],
        unsafe_expose_natives_and_gc: true,
        ..RuntimeOptions::default()
    })
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for lifecycle proof"
        );
        std::thread::yield_now();
    }
}

fn construct(runtime: &mut JsRuntime, global_name: &str) {
    construct_with_state(runtime, global_name, false);
}

fn construct_with_state(runtime: &mut JsRuntime, global_name: &str, initially_suspended: bool) {
    runtime
        .execute_script(
            "phase0d_construct.js",
            format!(
                r"
                  globalThis.{global_name} = new Deno.core.ops.NativePhase0OnlineContext(
                    48000,
                    {initially_suspended},
                  );
                "
            ),
        )
        .expect("the private hosted context should construct");
}

async fn close(runtime: &mut JsRuntime, global_name: &str) {
    runtime
        .execute_script(
            "phase0d_close.js",
            format!(
                r#"
                  globalThis.__phase0dClose = Deno.core.ops.op_phase0_online_close(
                    globalThis.{global_name},
                  ).then((confirmed) => {{
                    if (confirmed !== true) throw new Error("shutdown was not confirmed");
                    globalThis.__phase0dClosed = true;
                  }});
                "#
            ),
        )
        .expect("close should commit before returning its promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("confirmed shutdown should settle the close promise");
    runtime
        .execute_script(
            "phase0d_close_result.js",
            "if (globalThis.__phase0dClosed !== true) throw new Error('close did not settle');",
        )
        .expect("the close result should be observable");
}

#[tokio::test(flavor = "current_thread")]
async fn shared_host_rejects_before_construction_and_releases_only_after_shutdown() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let mut first = runtime(
        Arc::clone(&host),
        Arc::new(CountingSystemFactory::new(Arc::clone(&first_calls))),
    );
    construct(&mut first, "firstContext");
    assert_eq!(host.usage().online_contexts(), 1);
    assert_eq!(first_calls.load(Ordering::Acquire), 1);

    let mut second = runtime(
        Arc::clone(&host),
        Arc::new(CountingSystemFactory::new(Arc::clone(&second_calls))),
    );
    second
        .execute_script(
            "phase0d_host_rejection.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0OnlineContext(48000, false);
              } catch (error) {
                rejected = String(error).includes("AudioHost limit");
              }
              if (!rejected) throw new Error("shared host did not reject context two");
            "#,
        )
        .expect("the second isolate should observe a typed native rejection");
    assert_eq!(
        second_calls.load(Ordering::Acquire),
        0,
        "host admission must precede output-factory construction"
    );
    drop(second);

    close(&mut first, "firstContext").await;
    wait_until(|| host.usage().online_contexts() == 0);

    let mut second = runtime(
        Arc::clone(&host),
        Arc::new(CountingSystemFactory::new(Arc::clone(&second_calls))),
    );
    construct(&mut second, "secondContext");
    assert_eq!(second_calls.load(Ordering::Acquire), 1);
    assert_eq!(host.usage().online_contexts(), 1);
    drop(second);
    wait_until(|| host.usage().online_contexts() == 0);
}

struct UnconfirmedPrepared {
    config: AudioOutputConfig,
}

impl PreparedAudioOutput for UnconfirmedPrepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        _events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        Ok(Box::new(UnconfirmedRunning {
            callback: Some(callback),
        }))
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

struct UnconfirmedRunning {
    callback: Option<AudioRenderCallback>,
}

impl RunningAudioOutput for UnconfirmedRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let callback = self.callback.take().expect("callback is single-owner");
        AudioOutputEndpointShutdown::from_future(async move {
            // An unconfirmed endpoint must retain rather than destroy the
            // callback authority it can no longer prove quiescent.
            std::mem::forget(callback);
            Err(AudioOutputError::new(
                AudioOutputErrorKind::Shutdown,
                "deliberately unconfirmed endpoint shutdown",
            ))
        })
    }
}

struct UnconfirmedFactory;

impl AudioOutputFactory for UnconfirmedFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let format = AudioRenderFormat::new(
            request.requested_sample_rate().unwrap_or(48_000.0),
            request.number_of_channels(),
            128,
        )?;
        let config = AudioOutputConfig::new(format, request.sink_id(), 0.0)?;
        Ok(Box::new(UnconfirmedPrepared { config }))
    }
}

#[test]
fn unconfirmed_isolate_drop_retains_the_shared_permit() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let mut runtime = runtime(Arc::clone(&host), Arc::new(UnconfirmedFactory));
    construct(&mut runtime, "unconfirmedContext");
    assert_eq!(host.usage().online_contexts(), 1);

    drop(runtime);
    wait_until(|| host.usage().online_contexts() == 1);
    assert_eq!(
        host.usage().online_contexts(),
        1,
        "unconfirmed output ownership must remain scarce"
    );
}

#[test]
fn shared_audio_bytes_exhaust_and_recover_across_isolates() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_live_audio_bytes(Some(12)),
    ));
    let output: Arc<dyn AudioOutputFactory> = Arc::new(SystemAudioOutput::new());
    let mut first = runtime(Arc::clone(&host), Arc::clone(&output));
    first
        .execute_script(
            "phase0d_first_buffer.js",
            r"
              globalThis.firstBuffer = new Deno.core.ops.NativeAudioBuffer(1, 1, 48000);
              globalThis.firstBuffer.getChannelData(0);
            ",
        )
        .expect("the first isolate should reserve canonical and exposed bytes");
    assert_eq!(host.usage().live_audio_bytes(), 8);

    let mut second = runtime(Arc::clone(&host), output);
    second
        .execute_script(
            "phase0d_second_buffer.js",
            r#"
              globalThis.secondBuffer = new Deno.core.ops.NativeAudioBuffer(1, 1, 48000);
              let rejected = false;
              try { globalThis.secondBuffer.getChannelData(0); } catch { rejected = true; }
              if (!rejected) throw new Error("shared byte budget did not reject exposure");
            "#,
        )
        .expect("the second isolate should remain responsive after shared exhaustion");
    assert_eq!(host.usage().live_audio_bytes(), 12);

    drop(second);
    wait_until(|| host.usage().live_audio_bytes() == 8);

    first
        .execute_script(
            "phase0d_release_first_buffer.js",
            "globalThis.firstBuffer = undefined;",
        )
        .expect("the first isolate should release its buffer root");
    first
        .v8_isolate()
        .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
    wait_until(|| host.usage().live_audio_bytes() == 0);
}

enum DeathDriverCommand {
    Trigger,
    Shutdown,
}

struct DeathPrepared {
    config: AudioOutputConfig,
    sender: mpsc::Sender<DeathDriverCommand>,
    commands: Arc<Mutex<Option<mpsc::Receiver<DeathDriverCommand>>>>,
}

impl PreparedAudioOutput for DeathPrepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        mut callback: AudioRenderCallback,
        _events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let receiver = self
            .commands
            .lock()
            .expect("death-driver command lock poisoned")
            .take()
            .expect("death driver is single-use");
        let join = std::thread::Builder::new()
            .name("deno-audio-render-death-test".to_owned())
            .spawn(move || {
                while let Ok(command) = receiver.recv() {
                    match command {
                        DeathDriverCommand::Trigger => {
                            assert_eq!(
                                callback.render_interleaved_f32(&mut []),
                                web_audio_api::output::AudioRenderStatus::Stop
                            );
                        }
                        DeathDriverCommand::Shutdown => break,
                    }
                }
                drop(callback);
            })
            .expect("death-driver worker should spawn");
        Ok(Box::new(DeathRunning {
            sender: self.sender.clone(),
            join: Some(join),
        }))
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

struct DeathRunning {
    sender: mpsc::Sender<DeathDriverCommand>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl RunningAudioOutput for DeathRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let _ = self.sender.send(DeathDriverCommand::Shutdown);
        let join = self.join.take().expect("death driver joins once");
        AudioOutputEndpointShutdown::ready(join.join().map_err(|_| {
            AudioOutputError::new(
                AudioOutputErrorKind::Shutdown,
                "death-driver worker panicked",
            )
        }))
    }
}

struct DeathFactory {
    sender: mpsc::Sender<DeathDriverCommand>,
    receiver: Arc<Mutex<Option<mpsc::Receiver<DeathDriverCommand>>>>,
}

impl DeathFactory {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            sender,
            receiver: Arc::new(Mutex::new(Some(receiver))),
        }
    }

    fn trigger(&self) {
        self.sender
            .send(DeathDriverCommand::Trigger)
            .expect("running death driver should receive its trigger");
    }
}

impl AudioOutputFactory for DeathFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let format = AudioRenderFormat::new(
            request.requested_sample_rate().unwrap_or(48_000.0),
            request.number_of_channels(),
            128,
        )?;
        Ok(Box::new(DeathPrepared {
            config: AudioOutputConfig::new(format, request.sink_id(), 0.0)?,
            sender: self.sender.clone(),
            commands: Arc::clone(&self.receiver),
        }))
    }
}

impl Drop for DeathFactory {
    fn drop(&mut self) {
        let _ = self.sender.send(DeathDriverCommand::Shutdown);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn render_callback_death_poison_closes_and_releases_after_acknowledgement() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let factory = Arc::new(DeathFactory::new());
    let mut runtime = runtime(Arc::clone(&host), factory.clone());
    construct(&mut runtime, "deathContext");
    assert_eq!(host.usage().online_contexts(), 1);

    factory.trigger();
    wait_until(|| host.usage().online_contexts() == 0);
    runtime
        .execute_script(
            "phase0d_render_death.js",
            r#"
              if (globalThis.deathContext.contextState !== 2) {
                throw new Error("render death did not terminally close the context");
              }
              globalThis.__deathRejected = Deno.core.ops
                .op_phase0_online_resume(globalThis.deathContext)
                .then(
                  () => { throw new Error("closed context resumed"); },
                  () => true,
                );
            "#,
        )
        .expect("closed context should remain inspectable without graph reentry");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("post-death rejection should settle without V8 from the callback");
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_state_and_close_commands_outlive_javascript_waiters() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let mut runtime = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));

    runtime
        .execute_script(
            "phase0d_invalid_before_send.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0OnlineContext(1, true);
              } catch {
                rejected = true;
              }
              if (!rejected) throw new Error("invalid construction unexpectedly committed");
            "#,
        )
        .expect("validation before send should leave the isolate operational");
    assert_eq!(host.usage().online_contexts(), 0);

    construct_with_state(&mut runtime, "cancelContext", true);
    runtime
        .execute_script(
            "phase0d_discard_resume.js",
            r"
              Deno.core.ops.op_phase0_online_resume(globalThis.cancelContext);
            ",
        )
        .expect("resume should commit even though its promise is discarded");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the controller-owned resume should finish");
    runtime
        .execute_script(
            "phase0d_resume_result.js",
            r#"
              if (globalThis.cancelContext.contextState !== 1) {
                throw new Error("discarded resume waiter canceled native work");
              }
              Deno.core.ops.op_phase0_online_suspend(globalThis.cancelContext);
            "#,
        )
        .expect("suspend should commit without retaining its promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the controller-owned suspend should finish");
    runtime
        .execute_script(
            "phase0d_suspend_result.js",
            r#"
              if (globalThis.cancelContext.contextState !== 0) {
                throw new Error("discarded suspend waiter canceled native work");
              }
              Deno.core.ops.op_phase0_online_close(globalThis.cancelContext);
            "#,
        )
        .expect("close should commit without retaining its promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the controller-owned close should finish");
    wait_until(|| host.usage().online_contexts() == 0);
    runtime
        .execute_script(
            "phase0d_close_result.js",
            r#"
              if (globalThis.cancelContext.contextState !== 2) {
                throw new Error("discarded close waiter canceled native shutdown");
              }
            "#,
        )
        .expect("confirmed close should remain observable through the native object");
}

#[allow(
    clippy::too_many_lines,
    reason = "the end-to-end saturation proof keeps construction, bounded dispatch, and close accounting in one ownership timeline"
)]
#[tokio::test(flavor = "current_thread")]
async fn saturated_scheduled_sources_dispatch_once_or_release_on_close() {
    const SOURCE_COUNT: usize = 300;
    const DISPATCHED_BEFORE_CLOSE: usize = 256;

    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_online_contexts(Some(1))
            .max_scheduled_sources(Some(SOURCE_COUNT))
            .max_queued_events(Some(256)),
    ));
    let mut runtime = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct(&mut runtime, "eventContext");

    runtime
        .execute_script(
            "phase0d_event_saturation.js",
            format!(
                r"
                  globalThis.__eventSaturation = (async () => {{
                    const sources = [];
                    while (sources.length < {SOURCE_COUNT}) {{
                      try {{
                        sources.push(new Deno.core.ops.NativePhase0Oscillator(
                          globalThis.eventContext,
                        ));
                        if ((sources.length & 3) === 0) {{
                          await Deno.core.ops.op_phase0_wait_ended(globalThis.eventContext);
                        }}
                      }} catch {{
                        // Running construction is bounded. Yield to the native
                        // renderer without consuming a terminal event.
                        await Deno.core.ops.op_phase0_wait_ended(globalThis.eventContext);
                      }}
                    }}

                    await Deno.core.ops.op_phase0_wait_ended(globalThis.eventContext);
                    const start = globalThis.eventContext.currentTime + 3.0;
                    const stop = start + 128 / 48000;
                    for (let i = 0; i < sources.length; i++) {{
                      sources[i].schedule(start, stop);
                      if ((i & 3) === 3) {{
                        await Deno.core.ops.op_phase0_wait_ended(globalThis.eventContext);
                      }}
                    }}

                    // Deliberately block the isolate beyond the common stop
                    // quantum. Native callbacks may enqueue bounded wakes but
                    // cannot enter V8 or reconcile JavaScript roots here.
                    while (globalThis.eventContext.currentTime < stop + 0.05) {{}}
                    globalThis.__eventSources = sources;
                    globalThis.__eventSaturationReady = true;
                  }})();
                "
            ),
        )
        .expect("the saturation program should begin without exposing Web Audio globals");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("source construction and the slow-consumer interval should finish");
    assert_eq!(host.usage().scheduled_sources(), SOURCE_COUNT);
    assert!(host.usage().queued_events() <= 256);
    runtime
        .execute_script(
            "phase0d_reconcile_one_batch.js",
            r#"
              globalThis.__reconcileBatch = (async () => {
                globalThis.__endedIds = await Deno.core.ops.op_phase0_wait_ended(
                  globalThis.eventContext,
                );
                if (globalThis.__endedIds.length !== 128) {
                  throw new Error(`expected one bounded dispatch batch, got ${globalThis.__endedIds.length}`);
                }
                if (new Set(globalThis.__endedIds).size !== globalThis.__endedIds.length) {
                  throw new Error("a scheduled source dispatched ended more than once");
                }
              })();
            "#,
        )
        .expect("one authoritative reconciliation batch should begin");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("one reconciliation batch should settle");
    runtime
        .execute_script(
            "phase0d_reconcile_second_batch.js",
            r#"
              globalThis.__reconcileSecondBatch = (async () => {
                const second = await Deno.core.ops.op_phase0_wait_ended(
                  globalThis.eventContext,
                );
                if (second.length !== 128) {
                  throw new Error(`expected the second bounded batch, got ${second.length}`);
                }
                const first = new Set(globalThis.__endedIds);
                if (second.some((id) => first.has(id))) {
                  throw new Error("a scheduled source dispatched ended twice");
                }
                globalThis.__endedIds.push(...second);
              })();
            "#,
        )
        .expect("the second authoritative reconciliation should begin");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the second reconciliation batch should settle");
    assert_eq!(
        host.usage().scheduled_sources(),
        SOURCE_COUNT - DISPATCHED_BEFORE_CLOSE
    );

    close(&mut runtime, "eventContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.online_contexts() == 0 && usage.scheduled_sources() == 0 && usage.queued_events() == 0
    });
    runtime
        .execute_script(
            "phase0d_event_release_result.js",
            r#"
              if (globalThis.eventContext.rootedSourceCount !== 0) {
                throw new Error("context shutdown retained scheduled-source roots");
              }
            "#,
        )
        .expect("confirmed close should release every undispatched source root");
}
