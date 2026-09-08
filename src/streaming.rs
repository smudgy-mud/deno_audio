//! Native file-worker ownership. No JavaScript API or source authorization is installed here.

use std::fs::File;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use deno_core::OpState;
use deno_error::JsErrorBox;
use tokio::sync::oneshot;
use web_audio_api::context::{
    AudioContext, AudioControlBatchReservation, AudioNodeLifetimeReservation, BaseAudioContext,
};
use web_audio_api::node::{PcmSourceNode, PcmSourceWriter, PCM_SOURCE_CAPACITY};
use web_audio_api::MediaFileDecoder;

use crate::buffer::{NativeAudioByteBudget, NativeAudioByteReservation};
use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::lifecycle::{AudioLifecycleLease, AudioLifecycleRegistrar};
use crate::online::{AudioRuntimeState, IsolateReservation};

/// PCM queue charge, held until graph reclamation and release of every queue handle.
pub const STREAM_PCM_QUEUE_BYTES: usize = PCM_SOURCE_CAPACITY * size_of::<[f32; 2]>();
/// Per-worker working-storage allowance, reserved before opening or spawning.
///
/// This conservatively covers the pinned decoder's bounded PCM packet, resampler,
/// and transfer buffers. It is an accounting allowance, not an allocator-enforced
/// RSS ceiling: container indexes, aggregate metadata, and codec-private storage
/// still need separate limits before untrusted input can be admitted.
pub const STREAM_DECODER_WORKING_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
struct Cancellation {
    cancelled: AtomicBool,
    joined: AtomicBool,
    queued: AtomicUsize,
    source: Weak<PcmSourceNode>,
    result: Mutex<Option<Result<AudioFileStreamOutcome, String>>>,
}

impl Cancellation {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(source) = self.source.upgrade() {
            source.stop();
        }
    }
}

/// Context shutdown retains completion without retaining the PCM node or a JS handle.
#[derive(Clone)]
pub(crate) struct FileStreamActivity(Arc<Cancellation>);

impl FileStreamActivity {
    pub(crate) fn complete(&self) -> bool {
        self.0
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }

    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }

    pub(crate) async fn joined(&self) {
        while !self.complete() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// `OpState` owns cancellation independently of runtime state retained by job leases.
#[derive(Default)]
pub(crate) struct DecoderWorkerOwner {
    workers: Vec<Weak<Cancellation>>,
}

impl DecoderWorkerOwner {
    fn register(&mut self, worker: &Arc<Cancellation>) {
        self.workers.retain(|worker| {
            worker
                .upgrade()
                .is_some_and(|worker| !worker.joined.load(Ordering::Acquire))
        });
        self.workers.push(Arc::downgrade(worker));
    }
}

impl Drop for DecoderWorkerOwner {
    fn drop(&mut self) {
        for worker in self.workers.iter().filter_map(Weak::upgrade) {
            worker.cancel();
        }
    }
}

/// Decoder termination after the native observer has joined it.
/// Queued samples can still be playing after `Eof`; use the source's `ended()` for playback EOF.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFileStreamOutcome {
    /// The complete file has been decoded and its queued tail can drain.
    Eof,
    /// The source, isolate, context, or runtime generation cancelled the worker.
    Cancelled,
}

/// Single-use join receipt. Dropping it does not cancel the worker or release its quotas.
#[must_use = "await the receipt to observe decoder errors and confirmed worker exit"]
pub struct AudioFileStreamCompletion {
    receiver: oneshot::Receiver<Result<AudioFileStreamOutcome, String>>,
}

impl Future for AudioFileStreamCompletion {
    type Output = Result<AudioFileStreamOutcome, String>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.receiver).poll(cx).map(|result| {
            result
                .unwrap_or_else(|_| Err("decoder observer exited without confirmation".to_owned()))
        })
    }
}

/// Experimental native host handle for an incrementally decoded file.
///
/// Dropping this handle requests cancellation without blocking or joining on the
/// isolate thread. An independent native observer always joins the decoder and
/// retains its job and working-byte reservations until then.
pub struct AudioFileStream {
    node: Arc<PcmSourceNode>,
    cancellation: Arc<Cancellation>,
    completion: Option<AudioFileStreamCompletion>,
}

impl AudioFileStream {
    pub(crate) fn activity(&self) -> FileStreamActivity {
        FileStreamActivity(Arc::clone(&self.cancellation))
    }
    /// Admits a paused source using the extension's per-isolate and shared budgets.
    ///
    /// The embedder must authorize the source before calling. `open` runs on the
    /// worker only after admission; it should open exactly that authorized source.
    /// The context must be a hosted context. Connections remain the native caller's
    /// responsibility. This method adds no script globals or file permissions.
    ///
    /// # Errors
    /// Returns playback permission, retired generation, quota, graph-admission, or
    /// observer-spawn errors. Open/decode/decoder-spawn failures arrive via completion.
    ///
    /// # Panics
    /// Panics if `state` does not contain an initialized `deno_audio` extension.
    pub fn open(
        state: &mut OpState,
        context: &AudioContext,
        open: impl FnOnce() -> std::io::Result<File> + Send + 'static,
    ) -> Result<Self, JsErrorBox> {
        let runtime = Arc::clone(state.borrow::<Arc<AudioRuntimeState>>());
        let bytes = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
        runtime.check_stream_playback()?;
        let rate = context.sample_rate();
        if !(8000.0..=192_000.0).contains(&rate)
            || rate.fract() != 0.0
            || runtime
                .limits()
                .sample_rate()
                .is_some_and(|limit| f64::from(rate) > f64::from(limit))
        {
            return Err(JsErrorBox::range_error("unsupported streaming sample rate"));
        }
        let lifecycle = runtime.decoder_lifecycle();
        let source_limits = SourceLimits {
            channels: runtime.limits().channels(),
            sample_rate: runtime.limits().sample_rate(),
            file_bytes: runtime.limits().streaming_file_bytes(),
        };
        let lease = lifecycle
            .as_ref()
            .map(|registrar| {
                registrar.try_admit().ok_or_else(|| {
                    JsErrorBox::generic("InvalidStateError: audio runtime generation is retired")
                })
            })
            .transpose()?;
        // All charges precede queue allocation, source opening, and thread creation.
        let isolate_job = runtime.try_reserve_streaming_job()?;
        let host_job = runtime
            .host()
            .try_reserve(AudioHostResourceKind::StreamingJobs, 1)
            .map_err(limit_error)?;
        let working_bytes = bytes
            .reserve(STREAM_DECODER_WORKING_BYTES)
            .map_err(limit_error)?;
        let queue_bytes = bytes.reserve(STREAM_PCM_QUEUE_BYTES).map_err(limit_error)?;
        let isolate_node = runtime.try_reserve_graph_nodes(1)?;
        let host_node = runtime
            .host()
            .try_reserve(AudioHostResourceKind::GraphNodes, 1)
            .map_err(limit_error)?;
        let control = runtime.try_reserve_control_commands(1)?;
        let (node, writer) = PcmSourceNode::with_reservations(
            context,
            Some(AudioNodeLifetimeReservation::new((
                isolate_node,
                host_node,
                queue_bytes,
            ))),
            Some(AudioControlBatchReservation::new(control)),
        )
        .map_err(|error| JsErrorBox::generic(error.to_string()))?;
        let node = Arc::new(node);
        let cancellation = Arc::new(Cancellation {
            source: Arc::downgrade(&node),
            ..Cancellation::default()
        });
        let worker_cancel = Arc::clone(&cancellation);
        let (sender, receiver) = oneshot::channel();
        // Registration cannot fail after spawning. OpState Drop only sets cancellation;
        // the observer retains the join obligation even if every public handle disappears.
        state
            .borrow_mut::<DecoderWorkerOwner>()
            .register(&cancellation);
        let observer = spawn_observer(
            move || {
                decode(
                    open,
                    rate,
                    writer,
                    &worker_cancel,
                    lifecycle.as_ref(),
                    source_limits,
                )
            },
            WorkerReservations {
                working_bytes,
                host_job,
                isolate_job,
                lifecycle: lease,
            },
            Arc::clone(&cancellation),
            sender,
            SpawnPolicy::from_state(state),
        );
        if let Err(error) = observer {
            node.stop();
            return Err(JsErrorBox::generic(format!(
                "failed to spawn decoder observer: {error}"
            )));
        }
        Ok(Self {
            node,
            cancellation,
            completion: Some(AudioFileStreamCompletion { receiver }),
        })
    }

    /// Native PCM node to connect and start after prefill.
    #[must_use]
    pub fn node(&self) -> &PcmSourceNode {
        &self.node
    }

    /// Number of frames published by the decoder, including frames already rendered.
    #[must_use]
    pub fn queued_frames(&self) -> usize {
        self.cancellation.queued.load(Ordering::Acquire)
    }

    /// Takes the single decoder-join receipt. Playback EOF is reported by the node separately.
    pub fn take_completion(&mut self) -> Option<AudioFileStreamCompletion> {
        self.completion.take()
    }

    /// Requests permanent cancellation and silences the node without waiting on the isolate.
    pub fn stop(&self) {
        self.cancellation.cancel();
    }

    pub(crate) fn result(&self) -> Option<Result<AudioFileStreamOutcome, String>> {
        self.cancellation
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Drop for AudioFileStream {
    fn drop(&mut self) {
        self.stop();
    }
}

fn limit_error(error: impl std::fmt::Display) -> JsErrorBox {
    JsErrorBox::range_error(format!("streaming audio admission refused: {error}"))
}

#[derive(Clone, Copy)]
struct SourceLimits {
    channels: Option<usize>,
    sample_rate: Option<u32>,
    file_bytes: Option<u64>,
}

struct WorkerReservations {
    working_bytes: NativeAudioByteReservation,
    host_job: AudioHostReservation,
    isolate_job: IsolateReservation,
    lifecycle: Option<AudioLifecycleLease>,
}

#[derive(Clone, Copy, Default)]
struct SpawnPolicy {
    #[cfg(test)]
    fail: Option<&'static str>,
}

impl SpawnPolicy {
    fn from_state(state: &OpState) -> Self {
        #[cfg(test)]
        return state.try_borrow::<Self>().copied().unwrap_or_default();
        #[cfg(not(test))]
        {
            let _ = state;
            Self {}
        }
    }

    #[allow(
        clippy::unused_self,
        reason = "test builds carry per-request spawn failure injection"
    )]
    fn spawn<T: Send + 'static>(
        self,
        name: &'static str,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> std::io::Result<thread::JoinHandle<T>> {
        #[cfg(test)]
        if self.fail == Some(name) {
            return Err(std::io::Error::other("injected thread-spawn failure"));
        }
        thread::Builder::new().name(name.into()).spawn(work)
    }
}

fn spawn_observer(
    work: impl FnOnce() -> Result<AudioFileStreamOutcome, String> + Send + 'static,
    mut reservations: WorkerReservations,
    cancellation: Arc<Cancellation>,
    sender: oneshot::Sender<Result<AudioFileStreamOutcome, String>>,
    spawn: SpawnPolicy,
) -> std::io::Result<thread::JoinHandle<()>> {
    spawn.spawn("deno-audio-file-observer", move || {
        if let Some(lease) = &mut reservations.lifecycle {
            lease.commit();
        }
        let result = match spawn.spawn("deno-audio-file-decoder", work) {
            Ok(job) => job
                .join()
                .unwrap_or_else(|_| Err("file decoder panicked".to_owned())),
            Err(error) => Err(format!("failed to spawn file decoder: {error}")),
        };
        if result != Ok(AudioFileStreamOutcome::Eof) {
            cancellation.cancel();
        }
        // join() proves the decoder, its local buffers, and TLS destructors have exited.
        drop(reservations.working_bytes);
        drop(reservations.host_job);
        drop(reservations.isolate_job);
        cancellation.joined.store(true, Ordering::Release);
        if let Some(lease) = reservations.lifecycle {
            lease.confirm();
        }
        *cancellation
            .result
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(result.clone());
        let _ = sender.send(result);
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the caller validates an integral rate in 8000..=192000"
)]
fn decode(
    open: impl FnOnce() -> std::io::Result<File>,
    rate: f32,
    mut writer: PcmSourceWriter,
    cancel: &Cancellation,
    lifecycle: Option<&AudioLifecycleRegistrar>,
    limits: SourceLimits,
) -> Result<AudioFileStreamOutcome, String> {
    let cancelled = |writer: &PcmSourceWriter| {
        cancel.cancelled.load(Ordering::Acquire)
            || writer.is_stopped()
            || lifecycle.is_some_and(AudioLifecycleRegistrar::is_retired)
    };
    if cancelled(&writer) {
        return Ok(AudioFileStreamOutcome::Cancelled);
    }
    let file = open().map_err(|error| format!("file open failed: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("audio file metadata failed: {error}"))?;
    if limits
        .file_bytes
        .is_some_and(|limit| metadata.len() > limit)
    {
        return Err("audio file exceeds the configured streaming file-size limit".to_owned());
    }
    if cancelled(&writer) {
        return Ok(AudioFileStreamOutcome::Cancelled);
    }
    let mut decoder =
        MediaFileDecoder::new(file, rate as u32).map_err(|error| error.to_string())?;
    if limits
        .channels
        .is_some_and(|limit| decoder.source_channels() > limit)
        || limits
            .sample_rate
            .is_some_and(|limit| decoder.source_sample_rate() > limit)
    {
        return Err("configured streaming channel/sample-rate limit exceeded".to_owned());
    }
    let mut frames = [[0.0; 2]; 1024];
    loop {
        if cancelled(&writer) {
            return Ok(AudioFileStreamOutcome::Cancelled);
        }
        let count = decoder
            .read(&mut frames)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Ok(AudioFileStreamOutcome::Eof);
        }
        let mut offset = 0;
        while offset < count {
            if cancelled(&writer) {
                return Ok(AudioFileStreamOutcome::Cancelled);
            }
            let written = writer.write(&frames[offset..count]);
            offset += written;
            cancel.queued.fetch_add(written, Ordering::Release);
            if written == 0 {
                thread::park_timeout(Duration::from_millis(1));
            }
        }
    }
}

#[cfg(test)]
mod tests;
