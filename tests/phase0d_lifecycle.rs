//! Phase 0D acceptance evidence for shared context permits and shutdown ownership.

use std::future::poll_fn;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLimits};
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

struct CommandObservingFactory {
    calls: Arc<AtomicUsize>,
    observed_commands: Arc<AtomicUsize>,
    host: Arc<AudioHost>,
    system: SystemAudioOutput,
}

struct BlockingLifecycleFactory {
    entered: mpsc::SyncSender<()>,
    release: Mutex<Option<mpsc::Receiver<()>>>,
    system: SystemAudioOutput,
}

struct BlockingLifecyclePrepared {
    inner: Box<dyn PreparedAudioOutput>,
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
}

struct BlockingLifecycleRunning {
    inner: Box<dyn RunningAudioOutput>,
    entered: mpsc::SyncSender<()>,
    release: mpsc::Receiver<()>,
    suspend_blocked: bool,
}

impl BlockingLifecycleFactory {
    fn new() -> (Self, mpsc::Receiver<()>, mpsc::SyncSender<()>) {
        let (entered_send, entered_recv) = mpsc::sync_channel(1);
        let (release_send, release_recv) = mpsc::sync_channel(1);
        (
            Self {
                entered: entered_send,
                release: Mutex::new(Some(release_recv)),
                system: SystemAudioOutput::new(),
            },
            entered_recv,
            release_send,
        )
    }
}

impl AudioOutputFactory for BlockingLifecycleFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        let inner = self.system.prepare(request)?;
        let release = self
            .release
            .lock()
            .expect("blocking lifecycle factory lock poisoned")
            .take()
            .ok_or_else(|| {
                AudioOutputError::new(
                    AudioOutputErrorKind::BackendSpecific,
                    "blocking lifecycle factory is single-use",
                )
            })?;
        Ok(Box::new(BlockingLifecyclePrepared {
            inner,
            entered: self.entered.clone(),
            release,
        }))
    }
}

impl PreparedAudioOutput for BlockingLifecyclePrepared {
    fn config(&self) -> &AudioOutputConfig {
        self.inner.config()
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        let inner = self.inner.start(callback, events)?;
        Ok(Box::new(BlockingLifecycleRunning {
            inner,
            entered: self.entered,
            release: self.release,
            suspend_blocked: false,
        }))
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        self.inner.abort()
    }
}

impl RunningAudioOutput for BlockingLifecycleRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        self.inner.resume()
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        if !self.suspend_blocked {
            self.suspend_blocked = true;
            self.entered.send(()).map_err(|_| {
                AudioOutputError::new(
                    AudioOutputErrorKind::BackendSpecific,
                    "lifecycle retry test observer disconnected",
                )
            })?;
            self.release.recv().map_err(|_| {
                AudioOutputError::new(
                    AudioOutputErrorKind::BackendSpecific,
                    "lifecycle retry test release disconnected",
                )
            })?;
        }
        self.inner.suspend()
    }

    fn shutdown(self: Box<Self>) -> AudioOutputEndpointShutdown {
        self.inner.shutdown()
    }
}

impl AudioOutputFactory for CommandObservingFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        self.observed_commands.store(
            self.host.usage().queued_control_commands(),
            Ordering::Release,
        );
        self.system.prepare(request)
    }
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
    runtime_with_limits(host, output, AudioLimits::default())
}

fn runtime_with_limits(
    host: Arc<AudioHost>,
    output: Arc<dyn AudioOutputFactory>,
    limits: AudioLimits,
) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(
                AudioExtensionOptions::new(host)
                    .limits(limits)
                    .output_factory(output),
            ),
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

#[tokio::test(flavor = "current_thread")]
async fn configured_online_dimensions_reject_before_output_factory() {
    for limits in [
        AudioLimits::unlimited().max_channels(Some(1)),
        AudioLimits::unlimited().max_sample_rate(Some(44_100)),
    ] {
        let host = Arc::new(AudioHost::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut runtime = runtime_with_limits(
            Arc::clone(&host),
            Arc::new(CountingSystemFactory::new(Arc::clone(&calls))),
            limits,
        );
        runtime
            .execute_script(
                "phase1_online_dimension_limit.js",
                r#"
                  let rejected = false;
                  try {
                    new Deno.core.ops.NativePhase0OnlineContext(48000, false);
                  } catch {
                    rejected = true;
                  }
                  if (!rejected) {
                    throw new Error("configured online dimension was accepted");
                  }
                "#,
            )
            .expect("configured dimensions should fail before output preparation");
        assert_eq!(calls.load(Ordering::Acquire), 0);
        assert_eq!(host.usage().online_contexts(), 0);
    }
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

fn evaluate_boolean(runtime: &mut JsRuntime, name: &'static str, expression: &str) -> bool {
    match runtime.execute_script(
        name,
        format!(r#"if (!({expression})) throw new Error("__phase0d_waiting__");"#),
    ) {
        Ok(_) => true,
        Err(error) if error.to_string().contains("__phase0d_waiting__") => false,
        Err(error) => panic!("lifecycle readiness expression failed: {error}"),
    }
}

async fn poll_pending_event_loop_once(runtime: &mut JsRuntime) {
    let poll = poll_fn(|cx| {
        Poll::Ready(runtime.poll_event_loop(cx, deno_core::PollEventLoopOptions::default()))
    })
    .await;
    match poll {
        Poll::Pending => {}
        Poll::Ready(Ok(())) => panic!("event loop settled while lifecycle work was blocked"),
        Poll::Ready(Err(error)) => {
            panic!("event loop failed while lifecycle work was blocked: {error}")
        }
    }
}

async fn drive_until_suspend_blocks(runtime: &mut JsRuntime, entered: &mpsc::Receiver<()>) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        poll_pending_event_loop_once(runtime).await;
        match entered.try_recv() {
            Ok(()) => return,
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                panic!("blocking lifecycle endpoint retired before suspend")
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for the native suspend boundary"
        );
        tokio::task::yield_now().await;
    }
}

async fn drive_until_javascript_flag(
    runtime: &mut JsRuntime,
    name: &'static str,
    expression: &str,
) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !evaluate_boolean(runtime, name, expression) {
        poll_pending_event_loop_once(runtime).await;
        assert!(
            Instant::now() < deadline,
            "timed out waiting for JavaScript lifecycle observation"
        );
        tokio::task::yield_now().await;
    }
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

#[tokio::test(flavor = "current_thread")]
async fn magic_control_batch_reserves_before_factory_and_refuses_without_leaking() {
    const MAGIC_COMMANDS: usize =
        web_audio_api::context::AudioContextBuilder::INITIAL_GRAPH_CONTROL_COMMAND_COUNT;

    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_queued_control_commands(Some(MAGIC_COMMANDS)),
    ));
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicUsize::new(0));
    let mut accepted = runtime_with_limits(
        Arc::clone(&host),
        Arc::new(CommandObservingFactory {
            calls: Arc::clone(&calls),
            observed_commands: Arc::clone(&observed),
            host: Arc::clone(&host),
            system: SystemAudioOutput::new(),
        }),
        AudioLimits::default().max_queued_control_commands(Some(MAGIC_COMMANDS)),
    );
    construct(&mut accepted, "magicControlContext");
    assert_eq!(calls.load(Ordering::Acquire), 1);
    assert_eq!(observed.load(Ordering::Acquire), MAGIC_COMMANDS);
    assert_eq!(host.usage().queued_control_commands(), 0);
    close(&mut accepted, "magicControlContext").await;
    wait_until(|| host.usage().online_contexts() == 0);

    let isolate_calls = Arc::new(AtomicUsize::new(0));
    let isolate_host = Arc::new(AudioHost::default());
    let mut isolate_refused = runtime_with_limits(
        Arc::clone(&isolate_host),
        Arc::new(CountingSystemFactory::new(Arc::clone(&isolate_calls))),
        AudioLimits::default().max_queued_control_commands(Some(MAGIC_COMMANDS - 1)),
    );
    isolate_refused
        .execute_script(
            "phase0d_magic_isolate_refusal.js",
            r#"
              for (let attempt = 0; attempt < 2; attempt++) {
                let refused = false;
                try {
                  new Deno.core.ops.NativePhase0OnlineContext(48000, false);
                } catch (error) {
                  refused = String(error).includes(
                    "per-isolate queued graph-control command limit",
                  );
                }
                if (!refused) throw new Error("magic isolate refusal was not typed");
              }
            "#,
        )
        .expect("magic admission must lose the isolate budget before factory invocation");
    assert_eq!(isolate_calls.load(Ordering::Acquire), 0);
    assert_eq!(isolate_host.usage().online_contexts(), 0);
    assert_eq!(isolate_host.usage().queued_control_commands(), 0);

    let host_calls = Arc::new(AtomicUsize::new(0));
    let limited_host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_queued_control_commands(Some(MAGIC_COMMANDS - 1)),
    ));
    let mut host_refused = runtime_with_limits(
        Arc::clone(&limited_host),
        Arc::new(CountingSystemFactory::new(Arc::clone(&host_calls))),
        AudioLimits::default().max_queued_control_commands(Some(MAGIC_COMMANDS)),
    );
    host_refused
        .execute_script(
            "phase0d_magic_host_refusal.js",
            r#"
              for (let attempt = 0; attempt < 2; attempt++) {
                let refused = false;
                try {
                  new Deno.core.ops.NativePhase0OnlineContext(48000, false);
                } catch (error) {
                  refused = String(error).includes(
                    "AudioHost queued graph-control command limit",
                  );
                }
                if (!refused) throw new Error("magic host refusal was not typed");
              }
            "#,
        )
        .expect("host refusal must release each partially acquired isolate reservation");
    assert_eq!(host_calls.load(Ordering::Acquire), 0);
    assert_eq!(limited_host.usage().online_contexts(), 0);
    assert_eq!(limited_host.usage().queued_control_commands(), 0);
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

#[tokio::test(flavor = "current_thread")]
async fn unconfirmed_isolate_drop_retains_the_shared_permit() {
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

#[tokio::test(flavor = "current_thread")]
async fn shared_audio_bytes_exhaust_and_recover_across_isolates() {
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
    // The event observer is deliberately unreferenced so an otherwise idle
    // AudioContext cannot pin Deno's event loop. Give its already-signaled
    // blocking reconciliation task time to publish before polling the realm.
    std::thread::sleep(Duration::from_millis(100));
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
async fn public_context_observes_endpoint_death_without_an_active_source() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let factory = Arc::new(DeathFactory::new());
    let mut runtime = runtime(Arc::clone(&host), factory.clone());
    runtime
        .execute_script(
            "phase2_public_render_death_setup.js",
            r"
              const context = new AudioContext({ sampleRate: 48000 });
              let stateChanges = 0;
              context.onstatechange = () => stateChanges++;
              globalThis.__publicDeathContext = context;
              globalThis.__publicDeathStateChanges = () => stateChanges;
            ",
        )
        .expect("the public hosted context should construct");

    factory.trigger();
    wait_until(|| host.usage().online_contexts() == 0);
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the unreferenced state observer should dispatch ready terminal work");
    runtime
        .execute_script(
            "phase2_public_render_death_result.js",
            r#"
              if (globalThis.__publicDeathContext.state !== "closed") {
                throw new Error("endpoint death did not close the public context");
              }
              if (globalThis.__publicDeathStateChanges() !== 1) {
                throw new Error("endpoint death did not dispatch statechange exactly once");
              }
            "#,
        )
        .expect("public terminal state should be observable without a source pump");
}

#[tokio::test(flavor = "current_thread")]
async fn endpoint_death_terminal_batch_releases_started_javascript_source_roots() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let factory = Arc::new(DeathFactory::new());
    let mut runtime = runtime(Arc::clone(&host), factory.clone());
    runtime
        .execute_script(
            "phase2_public_started_render_death_setup.js",
            r"
              globalThis.__publicStartedDeathContext = new AudioContext({
                sampleRate: 48000,
              });
              const source = __publicStartedDeathContext.createOscillator();
              source.start();
            ",
        )
        .expect("a public started source should construct");
    assert_eq!(host.usage().graph_nodes(), 3);
    assert_eq!(host.usage().scheduled_sources(), 1);

    factory.trigger();
    let deadline = Instant::now() + TEST_TIMEOUT;
    while host.usage().online_contexts() != 0 {
        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .expect("terminal event reconciliation should remain healthy");
        assert!(
            Instant::now() < deadline,
            "endpoint death did not retire its public context"
        );
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the terminal event batch should settle");
    runtime
        .execute_script(
            "phase2_public_started_render_death_result.js",
            r#"
              if (__publicStartedDeathContext.state !== "closed") {
                throw new Error("endpoint death did not close the public context");
              }
            "#,
        )
        .expect("the public context should expose terminal state");

    let deadline = Instant::now() + TEST_TIMEOUT;
    loop {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
        let usage = host.usage();
        if usage.graph_nodes() == 0 && usage.scheduled_sources() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "terminal event retirement retained JavaScript source roots: {usage:?}"
        );
        std::thread::yield_now();
    }
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

#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end test keeps refusal, unchanged state, capacity recovery, and retirement in one ownership timeline"
)]
async fn lifecycle_waiter_limit_rejects_before_native_mutation_and_recovers() {
    let host = Arc::new(AudioHost::default());
    let (factory, entered, release) = BlockingLifecycleFactory::new();
    let limits = AudioLimits::default().max_lifecycle_waiters(Some(1));
    let mut runtime = runtime_with_limits(Arc::clone(&host), Arc::new(factory), limits);
    runtime
        .execute_script(
            "phase0d_lifecycle_limit_start.js",
            r#"
              globalThis.__lifecycleLimitContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__lifecycleLimitFirstDone = false;
              globalThis.__lifecycleLimitFirstError = undefined;
              __lifecycleLimitContext.suspend().then(
                () => { __lifecycleLimitFirstDone = true; },
                (error) => { __lifecycleLimitFirstError = error; },
              );
            "#,
        )
        .expect("the first lifecycle request should start");
    drive_until_suspend_blocks(&mut runtime, &entered).await;

    runtime
        .execute_script(
            "phase0d_lifecycle_limit_reject.js",
            r#"
              globalThis.__lifecycleLimitRejected = false;
              globalThis.__lifecycleLimitError = undefined;
              __lifecycleLimitContext.resume().then(
                () => { __lifecycleLimitError = "fulfilled"; },
                (error) => {
                  __lifecycleLimitError = error?.name;
                  __lifecycleLimitRejected = true;
                },
              );
            "#,
        )
        .expect("the over-limit lifecycle request should be scheduled");
    drive_until_javascript_flag(
        &mut runtime,
        "phase0d_lifecycle_limit_pending.js",
        "globalThis.__lifecycleLimitRejected === true",
    )
    .await;
    runtime
        .execute_script(
            "phase0d_lifecycle_limit_pending_result.js",
            r#"
              if (__lifecycleLimitError !== "QuotaExceededError") {
                throw new Error(`unexpected lifecycle limit error ${__lifecycleLimitError}`);
              }
              if (__lifecycleLimitFirstDone || __lifecycleLimitFirstError !== undefined) {
                throw new Error("the blocked accepted request settled prematurely");
              }
            "#,
        )
        .expect("waiter refusal should settle without disturbing accepted work");

    release
        .send(())
        .expect("the blocked endpoint should still own its release receiver");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("the accepted suspend should finish after release");
    runtime
        .execute_script(
            "phase0d_lifecycle_limit_recovery.js",
            r#"
              if (!__lifecycleLimitFirstDone || __lifecycleLimitFirstError !== undefined) {
                throw new Error("the accepted suspend did not finish cleanly");
              }
              if (__lifecycleLimitContext.state !== "suspended") {
                throw new Error("the refused resume mutated native lifecycle state");
              }
              globalThis.__lifecycleLimitRecovered = false;
              __lifecycleLimitContext.resume()
                .then(() => {
                  if (__lifecycleLimitContext.state !== "running") {
                    throw new Error("waiter capacity did not recover after acknowledgement");
                  }
                  return __lifecycleLimitContext.close();
                })
                .then(() => { __lifecycleLimitRecovered = true; });
            "#,
        )
        .expect("released waiter capacity should admit another lifecycle request");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("recovered resume and close should settle");
    runtime
        .execute_script(
            "phase0d_lifecycle_limit_recovery_result.js",
            r#"
              if (!__lifecycleLimitRecovered || __lifecycleLimitContext.state !== "closed") {
                throw new Error("lifecycle waiter capacity did not recover cleanly");
              }
            "#,
        )
        .expect("waiter recovery should remain observable");
    wait_until(|| host.usage().online_contexts() == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_requests_are_fifo_after_discarded_waiter() {
    let host = Arc::new(AudioHost::default());
    let (factory, entered, release) = BlockingLifecycleFactory::new();
    let limits = AudioLimits::default().max_lifecycle_waiters(Some(4));
    let mut runtime = runtime_with_limits(Arc::clone(&host), Arc::new(factory), limits);
    runtime
        .execute_script(
            "phase0d_lifecycle_fifo_start.js",
            r#"
              globalThis.__lifecycleFifoContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__lifecycleFifoStates = [];
              __lifecycleFifoContext.onstatechange = () => {
                __lifecycleFifoStates.push(__lifecycleFifoContext.state);
              };
              __lifecycleFifoContext.suspend();
              globalThis.__lifecycleFifoDone = false;
              globalThis.__lifecycleFifoBeforeClose = undefined;
              __lifecycleFifoContext.resume()
                .then(() => {
                  __lifecycleFifoBeforeClose = __lifecycleFifoContext.state;
                  return __lifecycleFifoContext.close();
                })
                .then(() => { __lifecycleFifoDone = true; });
            "#,
        )
        .expect("discarded suspend and queued resume should start");
    drive_until_suspend_blocks(&mut runtime, &entered).await;
    release
        .send(())
        .expect("the blocked endpoint should still own its release receiver");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("FIFO lifecycle requests should settle");
    runtime
        .execute_script(
            "phase0d_lifecycle_fifo_result.js",
            r#"
              if (!__lifecycleFifoDone || __lifecycleFifoBeforeClose !== "running") {
                throw new Error("queued resume did not follow the discarded suspend waiter");
              }
              if (__lifecycleFifoStates[0] !== "suspended" ||
                  __lifecycleFifoStates[1] !== "running" ||
                  __lifecycleFifoContext.state !== "closed") {
                throw new Error(`unexpected lifecycle order ${__lifecycleFifoStates}`);
              }
            "#,
        )
        .expect("accepted state work should remain FIFO after JavaScript cancellation");
    wait_until(|| host.usage().online_contexts() == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn close_dominates_a_blocked_and_queued_lifecycle_request() {
    let host = Arc::new(AudioHost::default());
    let (factory, entered, release) = BlockingLifecycleFactory::new();
    let limits = AudioLimits::default().max_lifecycle_waiters(Some(4));
    let mut runtime = runtime_with_limits(Arc::clone(&host), Arc::new(factory), limits);
    runtime
        .execute_script(
            "phase0d_lifecycle_close_start.js",
            r#"
              globalThis.__lifecycleCloseContext = new AudioContext({
                sampleRate: 48000,
                sinkId: "none",
              });
              globalThis.__lifecycleCloseFirst = __lifecycleCloseContext.suspend();
            "#,
        )
        .expect("the blocking suspend should start");
    drive_until_suspend_blocks(&mut runtime, &entered).await;

    runtime
        .execute_script(
            "phase0d_lifecycle_close_dominance.js",
            r#"
              globalThis.__lifecycleCloseDone = false;
              globalThis.__lifecycleCloseResumeSettled = false;
              globalThis.__lifecycleCloseResumeError = undefined;
              const closing = __lifecycleCloseContext.close().then(() => {
                __lifecycleCloseDone = true;
              });
              const queued = __lifecycleCloseContext.resume().then(
                () => {
                  __lifecycleCloseResumeError = "fulfilled";
                  __lifecycleCloseResumeSettled = true;
                },
                (error) => {
                  __lifecycleCloseResumeError = error?.name;
                  __lifecycleCloseResumeSettled = true;
                },
              );
              globalThis.__lifecycleCloseAll = Promise.allSettled([
                __lifecycleCloseFirst,
                queued,
                closing,
              ]);
            "#,
        )
        .expect("Close and the later state request should be scheduled");
    drive_until_javascript_flag(
        &mut runtime,
        "phase0d_lifecycle_close_pending.js",
        "globalThis.__lifecycleCloseResumeSettled === true",
    )
    .await;
    runtime
        .execute_script(
            "phase0d_lifecycle_close_pending_result.js",
            r#"
              if (__lifecycleCloseResumeError !== "InvalidStateError") {
                throw new Error(`Close did not reject queued Resume: ${__lifecycleCloseResumeError}`);
              }
              if (__lifecycleCloseDone) {
                throw new Error("Close completed before the active endpoint transition released");
              }
            "#,
        )
        .expect("Close should dominate without falsely completing retirement");

    release
        .send(())
        .expect("the blocked endpoint should still own its release receiver");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("Close should finish after the active transition releases");
    runtime
        .execute_script(
            "phase0d_lifecycle_close_result.js",
            r#"
              if (!__lifecycleCloseDone || __lifecycleCloseContext.state !== "closed") {
                throw new Error("Close did not complete authoritative retirement");
              }
            "#,
        )
        .expect("Close dominance should remain observable");
    wait_until(|| host.usage().online_contexts() == 0);
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
            .max_graph_nodes(Some(SOURCE_COUNT * 3))
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
    assert_eq!(host.usage().graph_nodes(), SOURCE_COUNT * 3);
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
    // Reconciliation releases the scheduled-source roots, but the test deliberately retains all
    // JavaScript wrappers. Public source objects remain readable after `ended`, so their exact
    // graph lifetimes stay charged until wrapper GC or whole-context retirement.
    assert_eq!(host.usage().graph_nodes(), SOURCE_COUNT * 3);

    close(&mut runtime, "eventContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.online_contexts() == 0
            && usage.graph_nodes() == 0
            && usage.scheduled_sources() == 0
            && usage.queued_events() == 0
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

#[tokio::test(flavor = "current_thread")]
async fn shared_graph_node_budget_rejects_before_oscillator_construction_and_recovers() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_graph_nodes(Some(3)),
    ));
    let mut first = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct(&mut first, "firstGraphContext");
    first
        .execute_script(
            "phase0d_first_graph_nodes.js",
            r"
              globalThis.__firstGraphSource = new Deno.core.ops.NativePhase0Oscillator(
                globalThis.firstGraphContext,
              );
            ",
        )
        .expect("the first exact oscillator should reserve three graph nodes");
    assert_eq!(host.usage().graph_nodes(), 3);

    let mut second = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct(&mut second, "secondGraphContext");
    second
        .execute_script(
            "phase0d_graph_node_refusal.js",
            r#"
              let refused = false;
              try {
                new Deno.core.ops.NativePhase0Oscillator(globalThis.secondGraphContext);
              } catch (error) {
                refused = error instanceof RangeError;
              }
              if (!refused) throw new Error("shared graph-node exhaustion was not a RangeError");
            "#,
        )
        .expect("shared graph-node exhaustion must reject at the native boundary");
    assert_eq!(host.usage().graph_nodes(), 3);
    assert_eq!(host.usage().scheduled_sources(), 1);
    drop(second);

    close(&mut first, "firstGraphContext").await;
    wait_until(|| host.usage().graph_nodes() == 0);
    let mut recovered = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct(&mut recovered, "recoveredGraphContext");
    recovered
        .execute_script(
            "phase0d_graph_node_recovery.js",
            r#"
              globalThis.__secondGraphSource = new Deno.core.ops.NativePhase0Oscillator(
                globalThis.recoveredGraphContext,
              );
              if (globalThis.__secondGraphSource.sourceId !== 1) {
                throw new Error("a rejected construction consumed wrapper source identity");
              }
            "#,
        )
        .expect("confirmed retirement should restore the shared graph-node budget");
    assert_eq!(host.usage().graph_nodes(), 3);

    close(&mut recovered, "recoveredGraphContext").await;
    wait_until(|| host.usage().graph_nodes() == 0 && host.usage().scheduled_sources() == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn per_isolate_graph_node_budget_rejects_before_a_second_oscillator() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_graph_nodes(Some(6)),
    ));
    let mut runtime = runtime_with_limits(
        Arc::clone(&host),
        Arc::new(SystemAudioOutput::new()),
        AudioLimits::default().max_graph_nodes(Some(3)),
    );
    construct(&mut runtime, "isolateGraphContext");
    runtime
        .execute_script(
            "phase0d_isolate_graph_nodes.js",
            r#"
              globalThis.__isolateGraphSource = new Deno.core.ops.NativePhase0Oscillator(
                globalThis.isolateGraphContext,
              );
              let refused = false;
              try {
                new Deno.core.ops.NativePhase0Oscillator(globalThis.isolateGraphContext);
              } catch (error) {
                refused = error instanceof RangeError
                  && String(error).includes("per-isolate live graph node limit");
              }
              if (!refused) throw new Error("per-isolate graph-node exhaustion was not typed");
            "#,
        )
        .expect("the second exact oscillator must lose the isolate budget before mutation");
    assert_eq!(host.usage().graph_nodes(), 3);
    assert_eq!(host.usage().scheduled_sources(), 1);

    close(&mut runtime, "isolateGraphContext").await;
    wait_until(|| host.usage().graph_nodes() == 0 && host.usage().scheduled_sources() == 0);
}

#[tokio::test(flavor = "current_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end test must retain both isolates through saturation, rollback, and recovery"
)]
async fn suspended_control_commands_are_shared_retained_and_recovered_across_isolates() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_graph_nodes(Some(21))
            .max_scheduled_sources(Some(7))
            .max_queued_control_commands(Some(44)),
    ));
    let mut first = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct_with_state(&mut first, "firstControlContext", true);
    first
        .execute_script(
            "phase0d_first_suspended_commands.js",
            r"
              globalThis.__firstControlSources = Array.from(
                { length: 3 },
                () => new Deno.core.ops.NativePhase0Oscillator(
                  globalThis.firstControlContext,
                ),
              );
              globalThis.__firstControlSources[0].schedule(0, 0.01);
            ",
        )
        .expect("three constructors plus one start/stop pair should reserve 23 commands");
    assert_eq!(host.usage().queued_control_commands(), 23);
    assert_eq!(host.usage().graph_nodes(), 9);
    assert_eq!(host.usage().scheduled_sources(), 3);

    let second_limits = AudioLimits::default()
        .max_graph_nodes(Some(12))
        .max_scheduled_sources(Some(4))
        .max_queued_control_commands(Some(28));
    let mut second = runtime_with_limits(
        Arc::clone(&host),
        Arc::new(SystemAudioOutput::new()),
        second_limits,
    );
    construct_with_state(&mut second, "secondControlContext", true);
    second
        .execute_script(
            "phase0d_shared_control_rejection.js",
            r#"
              globalThis.__secondControlSources = Array.from(
                { length: 3 },
                () => new Deno.core.ops.NativePhase0Oscillator(
                  globalThis.secondControlContext,
                ),
              );
              for (let attempt = 0; attempt < 2; attempt++) {
                let rejected = false;
                try {
                  new Deno.core.ops.NativePhase0Oscillator(globalThis.secondControlContext);
                } catch (error) {
                  rejected = String(error).includes(
                    "AudioHost queued graph-control command limit",
                  );
                }
                if (!rejected) {
                  throw new Error("shared suspended command budget did not reject");
                }
              }
            "#,
        )
        .expect("both attempts should release partial isolate admission before returning");
    assert_eq!(host.usage().queued_control_commands(), 44);
    assert_eq!(host.usage().graph_nodes(), 18);
    assert_eq!(host.usage().scheduled_sources(), 6);
    drop(second);
    wait_until(|| {
        let usage = host.usage();
        usage.queued_control_commands() == 23
            && usage.graph_nodes() == 9
            && usage.scheduled_sources() == 3
    });

    first
        .execute_script(
            "phase0d_resume_control_context.js",
            r"
              globalThis.__firstControlResume = Deno.core.ops.op_phase0_online_resume(
                globalThis.firstControlContext,
              ).then((state) => {
                if (state !== 0) throw new Error(`unexpected resume outcome ${state}`);
                globalThis.__firstControlResumed = true;
              });
            ",
        )
        .expect("Resume should own the staged command flush");
    first
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("Resume should apply every staged batch");
    wait_until(|| host.usage().queued_control_commands() == 0);

    close(&mut first, "firstControlContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.queued_control_commands() == 0
            && usage.graph_nodes() == 0
            && usage.scheduled_sources() == 0
            && usage.online_contexts() == 0
    });

    let mut recovered = runtime(Arc::clone(&host), Arc::new(SystemAudioOutput::new()));
    construct_with_state(&mut recovered, "recoveredControlContext", true);
    recovered
        .execute_script(
            "phase0d_recovered_control_constructor.js",
            r#"
              globalThis.__recoveredControlSource = new Deno.core.ops.NativePhase0Oscillator(
                globalThis.recoveredControlContext,
              );
              if (globalThis.__recoveredControlSource.sourceId !== 1) {
                throw new Error("rejected construction consumed a wrapper source identity");
              }
            "#,
        )
        .expect("off-render-thread reclamation should restore the shared command budget");
    assert_eq!(host.usage().queued_control_commands(), 7);
    assert_eq!(host.usage().graph_nodes(), 3);
    assert_eq!(host.usage().scheduled_sources(), 1);

    close(&mut recovered, "recoveredControlContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.queued_control_commands() == 0
            && usage.graph_nodes() == 0
            && usage.scheduled_sources() == 0
            && usage.online_contexts() == 0
    });
}

#[tokio::test(flavor = "current_thread")]
async fn isolate_control_refusal_precedes_schedule_state_and_close_extracts_staging() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_graph_nodes(Some(9))
            .max_scheduled_sources(Some(3))
            .max_queued_control_commands(Some(23)),
    ));
    let limits = AudioLimits::default().max_queued_control_commands(Some(22));
    let mut runtime = runtime_with_limits(
        Arc::clone(&host),
        Arc::new(SystemAudioOutput::new()),
        limits,
    );
    construct_with_state(&mut runtime, "isolateControlContext", true);
    runtime
        .execute_script(
            "phase0d_isolate_control_refusal.js",
            r#"
              globalThis.__isolateControlSources = Array.from(
                { length: 3 },
                () => new Deno.core.ops.NativePhase0Oscillator(
                  globalThis.isolateControlContext,
                ),
              );
              for (let attempt = 0; attempt < 2; attempt++) {
                let rejected = false;
                try {
                  globalThis.__isolateControlSources[0].schedule(0, 0.01);
                } catch (error) {
                  rejected = String(error).includes(
                    "per-isolate queued graph-control command limit",
                  );
                }
                if (!rejected) {
                  throw new Error("isolate command limit did not reject before schedule state");
                }
              }
            "#,
        )
        .expect("both attempts should see budget refusal rather than duplicate-start state");
    assert_eq!(host.usage().queued_control_commands(), 21);
    assert_eq!(host.usage().graph_nodes(), 9);
    assert_eq!(host.usage().scheduled_sources(), 3);

    close(&mut runtime, "isolateControlContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.queued_control_commands() == 0
            && usage.graph_nodes() == 0
            && usage.scheduled_sources() == 0
            && usage.online_contexts() == 0
    });
}

#[tokio::test(flavor = "current_thread")]
async fn host_control_refusal_precedes_schedule_state_and_releases_partial_isolate_reservation() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_graph_nodes(Some(9))
            .max_scheduled_sources(Some(3))
            .max_queued_control_commands(Some(22)),
    ));
    let mut runtime = runtime_with_limits(
        Arc::clone(&host),
        Arc::new(SystemAudioOutput::new()),
        AudioLimits::default().max_queued_control_commands(Some(23)),
    );
    construct_with_state(&mut runtime, "hostControlContext", true);
    runtime
        .execute_script(
            "phase0d_host_control_refusal.js",
            r#"
              globalThis.__hostControlSources = Array.from(
                { length: 3 },
                () => new Deno.core.ops.NativePhase0Oscillator(
                  globalThis.hostControlContext,
                ),
              );
              for (let attempt = 0; attempt < 2; attempt++) {
                let rejected = false;
                try {
                  globalThis.__hostControlSources[0].schedule(0, 0.01);
                } catch (error) {
                  rejected = String(error).includes(
                    "AudioHost queued graph-control command limit",
                  );
                }
                if (!rejected) {
                  throw new Error("host command limit did not reject before schedule state");
                }
              }
            "#,
        )
        .expect("host refusal should roll back each already-acquired isolate reservation");
    assert_eq!(host.usage().queued_control_commands(), 21);
    assert_eq!(host.usage().graph_nodes(), 9);
    assert_eq!(host.usage().scheduled_sources(), 3);

    close(&mut runtime, "hostControlContext").await;
    wait_until(|| {
        let usage = host.usage();
        usage.queued_control_commands() == 0
            && usage.graph_nodes() == 0
            && usage.scheduled_sources() == 0
            && usage.online_contexts() == 0
    });
}
