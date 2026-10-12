use super::*;
use crate::{
    AllowAllAudioPermissions, AudioEventPumpLiveness, AudioHost, AudioHostLimits,
    AudioLifecycleOwner, AudioLimits,
};
use std::cell::RefCell;
use std::sync::mpsc;
use std::time::Instant;
use web_audio_api::node::AudioNode;
use web_audio_api::output::SilentAudioOutput;

mod playback;

fn state(host: Arc<AudioHost>, limits: AudioLimits, registrar: AudioLifecycleRegistrar) -> OpState {
    let mut state = OpState::new(None);
    state.put(DecoderWorkerOwner::default());
    state.put(Arc::new(NativeAudioByteBudget::new(
        limits.live_audio_bytes().unwrap_or(usize::MAX),
        Arc::clone(&host),
    )));
    state.put(Arc::new(AudioRuntimeState::new(
        host,
        Arc::new(AllowAllAudioPermissions),
        limits,
        Arc::new(SilentAudioOutput::new()),
        AudioEventPumpLiveness::Unref,
        Some(registrar),
    )));
    state
}

fn context() -> AudioContext {
    AudioContext::builder(Arc::new(SilentAudioOutput::new()))
        .build()
        .unwrap()
}

async fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready() {
        assert!(Instant::now() < deadline, "stream did not make progress");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

async fn completed(stream: &mut AudioFileStream) -> Result<AudioFileStreamOutcome, String> {
    tokio::time::timeout(Duration::from_secs(5), stream.take_completion().unwrap())
        .await
        .unwrap()
}

static NEXT_FILE: AtomicUsize = AtomicUsize::new(0);
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn wav(frames: u32) -> Self {
        let path = std::env::temp_dir().join(format!(
            "deno-audio-stream-{}-{}.wav",
            std::process::id(),
            NEXT_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let data_bytes = frames * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&48_000_u32.to_le_bytes());
        bytes.extend_from_slice(&96_000_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_bytes.to_le_bytes());
        for _ in 0..frames {
            bytes.extend_from_slice(&8192_i16.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }

    fn opener(&self) -> impl FnOnce() -> std::io::Result<File> + Send + 'static {
        let path = self.0.clone();
        move || File::open(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_file(&self.0).unwrap();
    }
}

#[tokio::test]
async fn eof_releases_worker_charges_but_keeps_queued_pcm_until_graph_reclamation() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
    let context = context();
    let fixture = Fixture::wav(129);
    let mut stream = AudioFileStream::open(&mut state, &context, fixture.opener()).unwrap();
    assert_eq!(
        completed(&mut stream).await.unwrap(),
        AudioFileStreamOutcome::Eof
    );
    assert_eq!(stream.queued_frames(), 129);
    assert_eq!(host.usage().streaming_jobs(), 0);
    assert_eq!(host.usage().live_audio_bytes(), STREAM_PCM_QUEUE_BYTES);
    assert_eq!(host.usage().graph_nodes(), 1);
    stream.node().connect(&context.destination());
    stream.node().start();
    wait_for(|| stream.node().ended()).await;
    assert_eq!(stream.node().rendered_frames(), 129);
    drop(stream);
    wait_for(|| host.usage().live_audio_bytes() == 0 && host.usage().graph_nodes() == 0).await;
    owner.retire().await.unwrap();
    context.close_sync();
}

#[tokio::test]
async fn refusal_precedes_open_and_leaves_local_and_shared_budgets_reusable() {
    for local in [false, true] {
        for bytes in [false, true] {
            let host_limits = if local {
                AudioHostLimits::unlimited()
            } else if bytes {
                AudioHostLimits::unlimited()
                    .max_live_audio_bytes(Some(STREAM_DECODER_WORKING_BYTES))
            } else {
                AudioHostLimits::unlimited().max_streaming_jobs(Some(0))
            };
            let limits = if !local {
                AudioLimits::unlimited()
            } else if bytes {
                AudioLimits::unlimited().max_live_audio_bytes(Some(STREAM_DECODER_WORKING_BYTES))
            } else {
                AudioLimits::unlimited().max_streaming_jobs(Some(0))
            };
            let host = Arc::new(AudioHost::new(host_limits));
            let (owner, registrar) = AudioLifecycleOwner::new();
            let mut state = state(Arc::clone(&host), limits, registrar);
            let context = context();
            let called = Arc::new(AtomicBool::new(false));
            for _ in 0..3 {
                let called = Arc::clone(&called);
                assert!(AudioFileStream::open(&mut state, &context, move || {
                    called.store(true, Ordering::Release);
                    Err(std::io::Error::other("must not open"))
                })
                .is_err());
                assert_eq!(host.usage().streaming_jobs(), 0);
                assert_eq!(host.usage().live_audio_bytes(), 0);
                assert_eq!(state.borrow::<Arc<NativeAudioByteBudget>>().used(), 0);
                assert_eq!(host.usage().graph_nodes(), 0);
            }
            assert!(!called.load(Ordering::Acquire));
            owner.retire().await.unwrap();
            context.close_sync();
        }
    }
}

#[tokio::test]
async fn shared_job_limit_applies_across_isolates_and_recovers_after_join() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::unlimited().max_streaming_jobs(Some(1)),
    ));
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut first = state(Arc::clone(&host), AudioLimits::default(), registrar.clone());
    let mut second = state(Arc::clone(&host), AudioLimits::default(), registrar);
    let context = context();
    let fixture = Fixture::wav(48_000);
    let mut stream = AudioFileStream::open(&mut first, &context, fixture.opener()).unwrap();
    wait_for(|| stream.queued_frames() == PCM_SOURCE_CAPACITY).await;
    assert!(
        AudioFileStream::open(&mut second, &context, || panic!("rejected opener ran")).is_err()
    );
    stream.stop();
    assert_eq!(
        completed(&mut stream).await.unwrap(),
        AudioFileStreamOutcome::Cancelled
    );
    let mut replacement = AudioFileStream::open(&mut second, &context, fixture.opener()).unwrap();
    replacement.stop();
    assert_eq!(
        completed(&mut replacement).await.unwrap(),
        AudioFileStreamOutcome::Cancelled
    );
    drop(stream);
    drop(replacement);
    owner.retire().await.unwrap();
    context.close_sync();
}

#[tokio::test]
async fn source_drop_isolate_drop_context_close_and_generation_seal_cancel_full_queue() {
    for mode in 0..4 {
        let host = Arc::new(AudioHost::default());
        let (owner, registrar) = AudioLifecycleOwner::new();
        let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
        let context = context();
        let fixture = Fixture::wav(48_000);
        let mut stream = AudioFileStream::open(&mut state, &context, fixture.opener()).unwrap();
        wait_for(|| stream.queued_frames() == PCM_SOURCE_CAPACITY).await;
        let completion = stream.take_completion().unwrap();
        let mut retirement = None;
        let mut owner = Some(owner);
        match mode {
            0 => drop(stream),
            1 => drop(state),
            2 => context.close_sync(),
            _ => retirement = Some(owner.take().unwrap().retire()),
        }
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), completion)
                .await
                .unwrap()
                .unwrap(),
            AudioFileStreamOutcome::Cancelled
        );
        assert_eq!(host.usage().streaming_jobs(), 0);
        if let Some(retirement) = retirement {
            retirement.await.unwrap();
        }
        if let Some(owner) = owner {
            owner.retire().await.unwrap();
        }
        context.close_sync();
    }
}

#[tokio::test]
async fn existing_audio_charges_and_graph_refusal_block_streams_before_open() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(
        Arc::clone(&host),
        AudioLimits::unlimited()
            .max_streaming_jobs(Some(1))
            .max_live_audio_bytes(Some(STREAM_DECODER_WORKING_BYTES + STREAM_PCM_QUEUE_BYTES)),
        registrar,
    );
    let context = context();
    let runtime = Arc::clone(state.borrow::<Arc<AudioRuntimeState>>());
    let budget = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
    let other_job = runtime.try_reserve_streaming_job().unwrap();
    assert!(AudioFileStream::open(&mut state, &context, || panic!(
        "job refusal must precede open"
    ))
    .is_err());
    drop(other_job);
    let other_pcm = budget.reserve(1).unwrap();
    assert!(AudioFileStream::open(&mut state, &context, || panic!(
        "byte refusal must precede open"
    ))
    .is_err());
    assert_eq!(budget.used(), 1);
    drop(other_pcm);
    // Force native source refusal after wrapper reservations succeeded.
    context.close_sync();
    assert!(AudioFileStream::open(&mut state, &context, || panic!(
        "closed graph must precede open"
    ))
    .is_err());
    assert_eq!(host.usage().streaming_jobs(), 0);
    assert_eq!(host.usage().live_audio_bytes(), 0);
    assert_eq!(host.usage().graph_nodes(), 0);
    assert_eq!(host.usage().queued_control_commands(), 0);
    owner.retire().await.unwrap();
}

#[tokio::test]
async fn source_dimensions_respect_isolate_policy_before_publishing_pcm() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(
        Arc::clone(&host),
        AudioLimits::default().max_channels(Some(0)),
        registrar,
    );
    let context = context();
    let fixture = Fixture::wav(128);
    let mut stream = AudioFileStream::open(&mut state, &context, fixture.opener()).unwrap();
    assert!(completed(&mut stream)
        .await
        .unwrap_err()
        .contains("channel/sample-rate limit"));
    assert_eq!(stream.queued_frames(), 0);
    assert_eq!(host.usage().streaming_jobs(), 0);
    drop(stream);
    owner.retire().await.unwrap();
    context.close_sync();
}

#[tokio::test]
async fn observer_and_decoder_spawn_failure_roll_back_without_opening() {
    for fail in ["deno-audio-file-observer", "deno-audio-file-decoder"] {
        let host = Arc::new(AudioHost::default());
        let (owner, registrar) = AudioLifecycleOwner::new();
        let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
        state.put(SpawnPolicy { fail: Some(fail) });
        let context = context();
        let called = Arc::new(AtomicBool::new(false));
        let worker_called = Arc::clone(&called);
        let result = AudioFileStream::open(&mut state, &context, move || {
            worker_called.store(true, Ordering::Release);
            Err(std::io::Error::other("opener must not run"))
        });
        if fail == "deno-audio-file-observer" {
            assert!(result.is_err());
        } else {
            let mut stream = result.unwrap();
            assert!(completed(&mut stream)
                .await
                .unwrap_err()
                .contains("failed to spawn file decoder"));
            drop(stream);
        }
        assert!(!called.load(Ordering::Acquire));
        assert_eq!(host.usage().streaming_jobs(), 0);
        wait_for(|| host.usage().live_audio_bytes() == 0 && host.usage().graph_nodes() == 0).await;
        owner.retire().await.unwrap();
        context.close_sync();
    }
}

struct ExitGate {
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}
impl Drop for ExitGate {
    fn drop(&mut self) {
        self.entered.send(()).unwrap();
        let _ = self.release.recv_timeout(Duration::from_secs(5));
    }
}
thread_local! { static EXIT_GATE: RefCell<Option<ExitGate>> = const { RefCell::new(None) }; }

#[tokio::test]
async fn dropped_observer_and_isolate_do_not_release_a_worker_blocked_in_thread_exit() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
    let context = context();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let mut stream = AudioFileStream::open(&mut state, &context, move || {
        EXIT_GATE.with(|slot| {
            *slot.borrow_mut() = Some(ExitGate {
                entered: entered_tx,
                release: release_rx,
            });
        });
        Err(std::io::Error::other("test open failure"))
    })
    .unwrap();
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(stream.take_completion());
    drop(stream);
    drop(state);
    let mut retirement = Box::pin(owner.retire());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut retirement)
            .await
            .is_err()
    );
    assert_eq!(host.usage().streaming_jobs(), 1);
    assert!(host.usage().live_audio_bytes() >= STREAM_DECODER_WORKING_BYTES);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), retirement)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(host.usage().streaming_jobs(), 0);
    wait_for(|| host.usage().live_audio_bytes() == 0).await;
    context.close_sync();
}

#[tokio::test]
async fn open_decode_and_panic_failures_join_and_return_admission() {
    let host = Arc::new(AudioHost::default());
    let (owner, registrar) = AudioLifecycleOwner::new();
    let mut state = state(Arc::clone(&host), AudioLimits::default(), registrar);
    let context = context();
    for mode in 0..3 {
        let fixture = Fixture::wav(128);
        std::fs::write(&fixture.0, b"not audio").unwrap();
        let path = fixture.0.clone();
        let mut stream = AudioFileStream::open(&mut state, &context, move || match mode {
            0 => Err(std::io::Error::other("test open failure")),
            1 => File::open(path),
            _ => panic!("test decoder panic"),
        })
        .unwrap();
        assert!(completed(&mut stream).await.is_err());
        assert_eq!(host.usage().streaming_jobs(), 0);
        assert_eq!(host.usage().live_audio_bytes(), STREAM_PCM_QUEUE_BYTES);
        drop(stream);
        wait_for(|| host.usage().live_audio_bytes() == 0).await;
    }
    owner.retire().await.unwrap();
    assert!(AudioFileStream::open(&mut state, &context, || panic!("retired opener ran")).is_err());
    context.close_sync();
}
