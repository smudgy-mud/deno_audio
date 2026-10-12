//! Host-authorized file sources on the existing online graph.

use crate::host::{AudioHostReservation, AudioHostResourceKind};
use crate::node::NativeAudioNode;
use crate::online::NativePhase0OnlineContext;
use crate::online::{AudioRuntimeState, IsolateReservation, NativeOnlineContextState};
use crate::{AudioFileStream, AudioFileStreamOutcome};
use deno_core::{op2, OpState};
use deno_error::JsErrorBox;
use std::fs::File;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use web_audio_api::node::PCM_SOURCE_CAPACITY;

/// A single-use, authorized file opener executed on an admitted decoder worker.
pub type AudioFileOpener = Box<dyn FnOnce() -> io::Result<File> + Send + 'static>;

/// Embedder service that checks the calling isolate's authority for a file source.
/// No resolver is installed by default; file-source construction then fails closed.
pub trait AudioFileResolver: Send + Sync + 'static {
    /// Checks source authority and prepares an opener without reading encoded contents.
    ///
    /// # Errors
    /// Returns an error for unsupported sources or denied access.
    fn resolve(&self, state: &OpState, source: &str) -> Result<AudioFileOpener, JsErrorBox>;
}

struct Resolver(Arc<dyn AudioFileResolver>);

/// Installs the host's file authority in one initialized isolate. This grants no
/// read permissions itself; the resolver must check the calling isolate's permissions.
pub fn install_audio_file_resolver(state: &mut OpState, resolver: Arc<dyn AudioFileResolver>) {
    state.put(Resolver(resolver));
}

pub(crate) fn resolve(state: &OpState, source: &str) -> Result<AudioFileOpener, JsErrorBox> {
    state
        .borrow::<Arc<AudioRuntimeState>>()
        .check_stream_playback()?;
    let resolver = state.try_borrow::<Resolver>().ok_or_else(|| {
        JsErrorBox::generic("NotSupportedError: the host has not enabled audio file access")
    })?;
    resolver.0.resolve(state, source)
}

pub(crate) struct FileSource {
    pub(crate) stream: AudioFileStream,
    context: Weak<NativeOnlineContextState>,
    runtime: Arc<AudioRuntimeState>,
    started: AtomicBool,
    stopped: AtomicBool,
    scheduled: Mutex<Option<(IsolateReservation, AudioHostReservation)>>,
}

impl FileSource {
    pub(crate) fn new(
        stream: AudioFileStream,
        context: &Arc<NativeOnlineContextState>,
        runtime: Arc<AudioRuntimeState>,
    ) -> Self {
        Self {
            stream,
            context: Arc::downgrade(context),
            runtime,
            started: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            scheduled: Mutex::new(None),
        }
    }

    fn live(&self) -> bool {
        self.context
            .upgrade()
            .is_some_and(|context| context.context().is_ok())
    }

    pub(crate) fn start(&self) -> Result<(), JsErrorBox> {
        if !self.live() || self.stopped.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic(
                "InvalidStateError: audio file source is stopped or closed",
            ));
        }
        if self.started.load(Ordering::Acquire) {
            return Err(JsErrorBox::generic(
                "InvalidStateError: audio file source has already started",
            ));
        }
        let isolate = self.runtime.try_reserve_scheduled_source()?;
        let host = self
            .runtime
            .host()
            .try_reserve(AudioHostResourceKind::ScheduledSources, 1)
            .map_err(|error| JsErrorBox::range_error(error.to_string()))?;
        self.started.store(true, Ordering::Release);
        *self
            .scheduled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((isolate, host));
        self.stream.node().start();
        Ok(())
    }

    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.stream.stop();
    }

    async fn ready(&self) -> Result<(), JsErrorBox> {
        loop {
            if !self.live() {
                self.stop();
            }
            if let Some(result) = self.stream.result() {
                return match result {
                    Ok(AudioFileStreamOutcome::Eof) if !self.stopped.load(Ordering::Acquire) => {
                        Ok(())
                    }
                    Ok(_) => Err(JsErrorBox::generic("AbortError: audio file load cancelled")),
                    Err(error) => Err(JsErrorBox::generic(format!("EncodingError: {error}"))),
                };
            }
            if self.live() && self.stream.queued_frames() >= PCM_SOURCE_CAPACITY {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn ended(&self) -> Result<(), JsErrorBox> {
        loop {
            if !self.live() {
                self.stop();
            }
            if let Some(result) = self.stream.result() {
                if self.stopped.load(Ordering::Acquire)
                    || self.stream.node().ended()
                    || result != Ok(AudioFileStreamOutcome::Eof)
                {
                    self.scheduled
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .take();
                    return result
                        .map(|_| ())
                        .map_err(|error| JsErrorBox::generic(format!("EncodingError: {error}")));
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

#[op2]
pub(crate) async fn op_file_source_ready(
    #[cppgc] node: &NativeAudioNode,
) -> Result<(), JsErrorBox> {
    node.file_source()?.ready().await
}

#[op2]
pub(crate) async fn op_online_close_files(#[cppgc] context: &NativePhase0OnlineContext) {
    let workers = context.take_file_workers();
    for worker in &workers {
        worker.cancel();
    }
    for worker in workers {
        worker.joined().await;
    }
}

#[op2]
pub(crate) async fn op_file_source_ended(
    #[cppgc] node: &NativeAudioNode,
) -> Result<(), JsErrorBox> {
    node.file_source()?.ended().await
}
