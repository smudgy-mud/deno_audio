//! Deterministic acceptance evidence for embedder-driven runtime-generation drain.

use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use deno_audio::{
    AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLifecycleOwner,
    AudioLifecycleRegistrar, AudioLifecycleRetirement, AudioLifecycleRetirementError,
};
use deno_core::{JsRuntime, RuntimeOptions};
use tokio::sync::oneshot;
use web_audio_api::output::{
    AudioOutputConfig, AudioOutputEndpointShutdown, AudioOutputError, AudioOutputErrorKind,
    AudioOutputEventSink, AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure,
    AudioRenderCallback, AudioRenderFormat, PreparedAudioOutput, RunningAudioOutput,
};

type ShutdownResult = Result<(), AudioOutputError>;
const TEST_TIMEOUT: Duration = Duration::from_secs(5);

struct GatedShutdownFactory {
    calls: AtomicUsize,
    gate: Mutex<Option<ShutdownGate>>,
}

struct ShutdownGate {
    entered: oneshot::Sender<()>,
    release: oneshot::Receiver<ShutdownResult>,
}

struct GatedShutdownPrepared {
    config: AudioOutputConfig,
    gate: ShutdownGate,
}

struct GatedShutdownRunning {
    callback: Option<AudioRenderCallback>,
    gate: ShutdownGate,
}

impl GatedShutdownFactory {
    fn new() -> (
        Arc<Self>,
        oneshot::Receiver<()>,
        oneshot::Sender<ShutdownResult>,
    ) {
        let (entered_send, entered_recv) = oneshot::channel();
        let (release_send, release_recv) = oneshot::channel();
        (
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                gate: Mutex::new(Some(ShutdownGate {
                    entered: entered_send,
                    release: release_recv,
                })),
            }),
            entered_recv,
            release_send,
        )
    }
}

impl AudioOutputFactory for GatedShutdownFactory {
    fn prepare(
        &self,
        request: &AudioOutputRequest,
    ) -> Result<Box<dyn PreparedAudioOutput>, AudioOutputError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        let gate = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                AudioOutputError::new(
                    AudioOutputErrorKind::BackendSpecific,
                    "gated test output is single-use",
                )
            })?;
        let format = AudioRenderFormat::new(
            request.requested_sample_rate().unwrap_or(48_000.0),
            request.number_of_channels(),
            128,
        )?;
        let config = AudioOutputConfig::new(format, request.sink_id(), 0.0)?;
        Ok(Box::new(GatedShutdownPrepared { config, gate }))
    }
}

impl PreparedAudioOutput for GatedShutdownPrepared {
    fn config(&self) -> &AudioOutputConfig {
        &self.config
    }

    fn start(
        self: Box<Self>,
        callback: AudioRenderCallback,
        _events: AudioOutputEventSink,
    ) -> Result<Box<dyn RunningAudioOutput>, AudioOutputStartFailure> {
        Ok(Box::new(GatedShutdownRunning {
            callback: Some(callback),
            gate: self.gate,
        }))
    }

    fn abort(self: Box<Self>) -> AudioOutputEndpointShutdown {
        AudioOutputEndpointShutdown::ready(Ok(()))
    }
}

impl RunningAudioOutput for GatedShutdownRunning {
    fn resume(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn suspend(&mut self) -> Result<(), AudioOutputError> {
        Ok(())
    }

    fn shutdown(mut self: Box<Self>) -> AudioOutputEndpointShutdown {
        let callback = self
            .callback
            .take()
            .expect("the render callback has one retirement owner");
        let ShutdownGate { entered, release } = self.gate;
        let _ = entered.send(());
        AudioOutputEndpointShutdown::from_future(async move {
            match release.await {
                Ok(Ok(())) => {
                    drop(callback);
                    Ok(())
                }
                Ok(Err(error)) => {
                    std::mem::forget(callback);
                    Err(error)
                }
                Err(_) => {
                    std::mem::forget(callback);
                    Err(AudioOutputError::new(
                        AudioOutputErrorKind::Shutdown,
                        "gated test output lost its shutdown decision",
                    ))
                }
            }
        })
    }
}

fn runtime(
    host: Arc<AudioHost>,
    output: Arc<dyn AudioOutputFactory>,
    registrar: AudioLifecycleRegistrar,
) -> JsRuntime {
    runtime_with_registrar(host, output, Some(registrar))
}

fn runtime_with_registrar(
    host: Arc<AudioHost>,
    output: Arc<dyn AudioOutputFactory>,
    registrar: Option<AudioLifecycleRegistrar>,
) -> JsRuntime {
    let options = AudioExtensionOptions::new(host).output_factory(output);
    let options = match registrar {
        Some(registrar) => options.lifecycle_registrar(registrar),
        None => options,
    };
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

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + TEST_TIMEOUT;
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for lifecycle observer completion"
        );
        std::thread::yield_now();
    }
}

fn construct(runtime: &mut JsRuntime) {
    runtime
        .execute_script(
            "lifecycle_drain_construct.js",
            "globalThis.context = new AudioContext({ sampleRate: 48000 });",
        )
        .expect("the tracked online context should construct");
}

fn construct_native(runtime: &mut JsRuntime, global_name: &str) {
    runtime
        .execute_script(
            "lifecycle_drain_native_construct.js",
            format!(
                "globalThis.{global_name} = new Deno.core.ops.NativePhase0OnlineContext(48000, false);"
            ),
        )
        .expect("the tracked native online context should construct");
}

async fn poll_once(
    mut retirement: Pin<&mut AudioLifecycleRetirement>,
) -> Poll<Result<(), AudioLifecycleRetirementError>> {
    poll_fn(|context| Poll::Ready(retirement.as_mut().poll(context))).await
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
async fn seal_waits_for_exact_observer_and_rejects_later_construction() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let (factory, shutdown_entered, release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime(Arc::clone(&host), factory.clone(), registrar);
    construct(&mut runtime);
    assert_eq!(factory.calls.load(Ordering::Acquire), 1);

    let mut retirement = Box::pin(owner.retire());
    assert!(poll_once(retirement.as_mut()).await.is_pending());
    runtime
        .execute_script(
            "lifecycle_drain_reject_after_seal.js",
            r#"
              let rejected = false;
              try { new AudioContext({ sampleRate: 48000 }); }
              catch (error) { rejected = String(error).includes("retired"); }
              if (!rejected) throw new Error("sealed generation admitted AudioContext");
            "#,
        )
        .expect("retired construction should reject at the JavaScript boundary");
    assert_eq!(
        factory.calls.load(Ordering::Acquire),
        1,
        "seal must reject before output preparation"
    );

    drop(runtime);
    shutdown_entered
        .await
        .expect("runtime drop should initiate endpoint shutdown");
    assert!(poll_once(retirement.as_mut()).await.is_pending());
    release_shutdown
        .send(Ok(()))
        .expect("the lifecycle observer still owns the shutdown receiver");
    assert_eq!(retirement.await, Ok(()));
    assert_eq!(host.usage().online_contexts(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn unconfirmed_observer_fails_drain_and_keeps_host_accounting() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let (factory, shutdown_entered, release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime(Arc::clone(&host), factory, registrar);
    construct(&mut runtime);
    let retirement = owner.retire();
    drop(runtime);
    shutdown_entered
        .await
        .expect("runtime drop should initiate endpoint shutdown");
    release_shutdown
        .send(Err(AudioOutputError::new(
            AudioOutputErrorKind::Shutdown,
            "deliberately unconfirmed test shutdown",
        )))
        .expect("the lifecycle observer still owns the shutdown receiver");

    assert_eq!(
        retirement.await,
        Err(AudioLifecycleRetirementError::UnconfirmedShutdown)
    );
    assert_eq!(
        host.usage().online_contexts(),
        1,
        "failed proof must leave committed host accounting scarce"
    );
}

#[test]
fn dropping_owner_seals_registration_before_output_preparation() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    drop(owner);
    let (factory, _shutdown_entered, _release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime(host, factory.clone(), registrar);

    runtime
        .execute_script(
            "lifecycle_owner_drop_rejects.js",
            r#"
              let rejected = false;
              try { new AudioContext({ sampleRate: 48000 }); }
              catch (error) { rejected = String(error).includes("retired"); }
              if (!rejected) throw new Error("dropped owner did not seal generation");
            "#,
        )
        .expect("owner drop should reject at the JavaScript boundary");
    assert_eq!(factory.calls.load(Ordering::Acquire), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn dropping_retirement_does_not_cancel_observer_or_permit_release() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(1)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let (factory, shutdown_entered, release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime(Arc::clone(&host), factory, registrar);
    construct(&mut runtime);
    let mut retirement = Box::pin(owner.retire());
    drop(runtime);
    shutdown_entered
        .await
        .expect("runtime drop should initiate endpoint shutdown");
    assert!(poll_once(retirement.as_mut()).await.is_pending());
    drop(retirement);

    release_shutdown
        .send(Ok(()))
        .expect("dropping the receipt must not cancel its observer");
    wait_until(|| host.usage().online_contexts() == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn omitted_registrar_preserves_detached_online_lifecycle() {
    let host = Arc::new(AudioHost::default());
    let (sealed_owner, unused_registrar) = AudioLifecycleOwner::new();
    let sealed = sealed_owner.retire();
    assert_eq!(sealed.await, Ok(()));
    drop(unused_registrar);

    let (factory, shutdown_entered, release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime_with_registrar(Arc::clone(&host), factory, None);
    construct(&mut runtime);
    drop(runtime);
    shutdown_entered
        .await
        .expect("detached context should retain its native observer");
    release_shutdown
        .send(Ok(()))
        .expect("detached observer should own shutdown completion");
    wait_until(|| host.usage().online_contexts() == 0);
}

#[tokio::test(flavor = "current_thread")]
async fn offline_context_remains_available_after_generation_seal() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let retirement = owner.retire();
    assert_eq!(retirement.await, Ok(()));
    let (factory, _shutdown_entered, _release_shutdown) = GatedShutdownFactory::new();
    let mut runtime = runtime(host, factory.clone(), registrar);

    runtime
        .execute_script(
            "lifecycle_sealed_offline.js",
            r#"
              const context = new OfflineAudioContext(1, 1, 48000);
              if (context.length !== 1 || context.sampleRate !== 48000) {
                throw new Error("sealed generation changed offline construction");
              }
            "#,
        )
        .expect("offline contexts are outside online lifecycle drain");
    assert_eq!(
        factory.calls.load(Ordering::Acquire),
        0,
        "offline construction must not acquire online output"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn one_registrar_drains_every_sharing_extension_instance() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_online_contexts(Some(2)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let (first_factory, first_entered, release_first) = GatedShutdownFactory::new();
    let (second_factory, second_entered, release_second) = GatedShutdownFactory::new();
    let mut first = runtime(Arc::clone(&host), first_factory, registrar.clone());
    construct_native(&mut first, "firstContext");
    let mut second = runtime(Arc::clone(&host), second_factory, registrar);
    construct_native(&mut second, "secondContext");
    assert_eq!(host.usage().online_contexts(), 2);

    let mut retirement = Box::pin(owner.retire());
    drop(second);
    drop(first);
    first_entered
        .await
        .expect("first extension should initiate shutdown");
    second_entered
        .await
        .expect("second extension should initiate shutdown");
    assert!(poll_once(retirement.as_mut()).await.is_pending());

    release_first
        .send(Ok(()))
        .expect("first observer should retain its shutdown receiver");
    assert!(poll_once(retirement.as_mut()).await.is_pending());
    release_second
        .send(Ok(()))
        .expect("second observer should retain its shutdown receiver");
    assert_eq!(retirement.await, Ok(()));
    assert_eq!(host.usage().online_contexts(), 0);
}
