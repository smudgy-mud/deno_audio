use std::fmt;
use std::sync::Arc;

use deno_error::JsErrorBox;
use web_audio_api::output::{AudioOutputFactory, SystemAudioOutput};

use crate::host::AudioHost;

/// Sandbox-oriented per-isolate resource ceilings.
///
/// Shared cross-isolate ceilings belong to [`crate::AudioHostLimits`]. This
/// type controls only resources owned through one extension instance.
#[derive(Clone, Debug)]
#[non_exhaustive]
#[allow(
    clippy::struct_field_names,
    reason = "the private max_ prefix keeps every non-exhaustive limit unambiguous"
)]
pub struct AudioLimits {
    max_online_contexts: Option<usize>,
    max_live_audio_bytes: Option<usize>,
    max_channels: Option<usize>,
    max_decoded_frames: Option<usize>,
    max_decoded_samples: Option<usize>,
    max_encoded_bytes: Option<usize>,
    max_decode_jobs: Option<usize>,
    max_offline_frames: Option<usize>,
    max_offline_samples: Option<usize>,
    max_offline_render_jobs: Option<usize>,
    max_graph_nodes: Option<usize>,
    max_scheduled_sources: Option<usize>,
    max_queued_events: Option<usize>,
}

impl Default for AudioLimits {
    fn default() -> Self {
        Self {
            // Six mirrors the long-standing browser-scale online-context cap
            // while still allowing trusted embedders to opt out explicitly.
            max_online_contexts: Some(6),
            max_live_audio_bytes: Some(64 * 1024 * 1024),
            max_channels: Some(web_audio_api::MAX_CHANNELS),
            max_decoded_frames: Some(16 * 1024 * 1024),
            max_decoded_samples: Some(16 * 1024 * 1024),
            max_encoded_bytes: Some(32 * 1024 * 1024),
            max_decode_jobs: Some(4),
            max_offline_frames: Some(16 * 1024 * 1024),
            max_offline_samples: Some(16 * 1024 * 1024),
            max_offline_render_jobs: Some(2),
            max_graph_nodes: Some(1024),
            max_scheduled_sources: Some(512),
            max_queued_events: Some(256),
        }
    }
}

impl AudioLimits {
    /// Creates explicitly unlimited per-isolate ceilings.
    #[must_use]
    pub const fn unlimited() -> Self {
        Self {
            max_online_contexts: None,
            max_live_audio_bytes: None,
            max_channels: None,
            max_decoded_frames: None,
            max_decoded_samples: None,
            max_encoded_bytes: None,
            max_decode_jobs: None,
            max_offline_frames: None,
            max_offline_samples: None,
            max_offline_render_jobs: None,
            max_graph_nodes: None,
            max_scheduled_sources: None,
            max_queued_events: None,
        }
    }

    /// Sets the maximum number of online contexts owned by this isolate.
    #[must_use]
    pub const fn max_online_contexts(mut self, limit: Option<usize>) -> Self {
        self.max_online_contexts = limit;
        self
    }

    /// Sets the wrapper-owned live audio-byte ceiling for this isolate.
    #[must_use]
    pub const fn max_live_audio_bytes(mut self, limit: Option<usize>) -> Self {
        self.max_live_audio_bytes = limit;
        self
    }

    /// Sets the maximum channel count accepted by native buffer and render work.
    #[must_use]
    pub const fn max_channels(mut self, limit: Option<usize>) -> Self {
        self.max_channels = limit;
        self
    }

    /// Sets the maximum frame count accepted from one decoded result.
    #[must_use]
    pub const fn max_decoded_frames(mut self, limit: Option<usize>) -> Self {
        self.max_decoded_frames = limit;
        self
    }

    /// Sets the maximum logical sample count accepted from one decoded result.
    #[must_use]
    pub const fn max_decoded_samples(mut self, limit: Option<usize>) -> Self {
        self.max_decoded_samples = limit;
        self
    }

    /// Sets the maximum encoded payload accepted by one decode request.
    #[must_use]
    pub const fn max_encoded_bytes(mut self, limit: Option<usize>) -> Self {
        self.max_encoded_bytes = limit;
        self
    }

    /// Sets the concurrent decode-job ceiling for this isolate.
    #[must_use]
    pub const fn max_decode_jobs(mut self, limit: Option<usize>) -> Self {
        self.max_decode_jobs = limit;
        self
    }

    /// Sets the maximum frame count accepted by one offline render.
    #[must_use]
    pub const fn max_offline_frames(mut self, limit: Option<usize>) -> Self {
        self.max_offline_frames = limit;
        self
    }

    /// Sets the maximum logical sample count accepted by one offline render.
    #[must_use]
    pub const fn max_offline_samples(mut self, limit: Option<usize>) -> Self {
        self.max_offline_samples = limit;
        self
    }

    /// Sets the concurrent offline-render-job ceiling for this isolate.
    #[must_use]
    pub const fn max_offline_render_jobs(mut self, limit: Option<usize>) -> Self {
        self.max_offline_render_jobs = limit;
        self
    }

    /// Sets the maximum number of live wrapper-created graph nodes for this isolate.
    #[must_use]
    pub const fn max_graph_nodes(mut self, limit: Option<usize>) -> Self {
        self.max_graph_nodes = limit;
        self
    }

    /// Sets the maximum number of scheduled sources rooted by this isolate.
    #[must_use]
    pub const fn max_scheduled_sources(mut self, limit: Option<usize>) -> Self {
        self.max_scheduled_sources = limit;
        self
    }

    /// Sets the maximum number of queued native event records for this isolate.
    #[must_use]
    pub const fn max_queued_events(mut self, limit: Option<usize>) -> Self {
        self.max_queued_events = limit;
        self
    }

    pub(crate) const fn online_contexts(&self) -> Option<usize> {
        self.max_online_contexts
    }

    pub(crate) const fn live_audio_bytes(&self) -> Option<usize> {
        self.max_live_audio_bytes
    }

    pub(crate) const fn channels(&self) -> Option<usize> {
        self.max_channels
    }

    pub(crate) const fn decoded_frames(&self) -> Option<usize> {
        self.max_decoded_frames
    }

    pub(crate) const fn decoded_samples(&self) -> Option<usize> {
        self.max_decoded_samples
    }

    pub(crate) const fn encoded_bytes(&self) -> Option<usize> {
        self.max_encoded_bytes
    }

    pub(crate) const fn decode_jobs(&self) -> Option<usize> {
        self.max_decode_jobs
    }

    pub(crate) const fn offline_frames(&self) -> Option<usize> {
        self.max_offline_frames
    }

    pub(crate) const fn offline_samples(&self) -> Option<usize> {
        self.max_offline_samples
    }

    pub(crate) const fn offline_render_jobs(&self) -> Option<usize> {
        self.max_offline_render_jobs
    }

    pub(crate) const fn graph_nodes(&self) -> Option<usize> {
        self.max_graph_nodes
    }

    pub(crate) const fn scheduled_sources(&self) -> Option<usize> {
        self.max_scheduled_sources
    }

    pub(crate) const fn queued_events(&self) -> Option<usize> {
        self.max_queued_events
    }
}

/// Host policy checks used before native audio authority is acquired.
pub trait AudioPermissions: Send + Sync + 'static {
    /// Checks permission to construct or retarget an audible online context.
    ///
    /// # Errors
    ///
    /// Returns the host-selected JavaScript error when playback is denied.
    fn check_playback(&self, api_name: &'static str) -> Result<(), JsErrorBox>;

    /// Checks permission to acquire an audio capture source.
    ///
    /// # Errors
    ///
    /// Returns the host-selected JavaScript error when capture is denied.
    fn check_capture(&self, api_name: &'static str) -> Result<(), JsErrorBox>;
}

/// Permission helper for trusted embedders and conformance tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllAudioPermissions;

impl AudioPermissions for AllowAllAudioPermissions {
    fn check_playback(&self, _api_name: &'static str) -> Result<(), JsErrorBox> {
        Ok(())
    }

    fn check_capture(&self, _api_name: &'static str) -> Result<(), JsErrorBox> {
        Ok(())
    }
}

/// Embedder-owned configuration installed into one `deno_audio` extension.
///
/// Sharing the same [`AudioHost`] across values makes aggregate accounting
/// span isolates. The output factory remains a separate authority and may be a
/// built-in [`SystemAudioOutput`] or an embedder-supplied logical mixer.
#[derive(Clone)]
#[non_exhaustive]
pub struct AudioExtensionOptions {
    host: Arc<AudioHost>,
    permissions: Arc<dyn AudioPermissions>,
    limits: AudioLimits,
    output: Arc<dyn AudioOutputFactory>,
}

impl fmt::Debug for AudioExtensionOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioExtensionOptions")
            .field("host", &self.host)
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl Default for AudioExtensionOptions {
    fn default() -> Self {
        Self::new(Arc::new(AudioHost::default()))
    }
}

impl AudioExtensionOptions {
    /// Creates options using built-in system output and allow-all permissions.
    #[must_use]
    pub fn new(host: Arc<AudioHost>) -> Self {
        Self {
            host,
            permissions: Arc::new(AllowAllAudioPermissions),
            limits: AudioLimits::default(),
            output: Arc::new(SystemAudioOutput::new()),
        }
    }

    /// Replaces the host permission service.
    #[must_use]
    pub fn permissions(mut self, permissions: Arc<dyn AudioPermissions>) -> Self {
        self.permissions = permissions;
        self
    }

    /// Replaces the per-isolate ceilings.
    #[must_use]
    pub fn limits(mut self, limits: AudioLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Replaces the logical output factory.
    #[must_use]
    pub fn output_factory(mut self, output: Arc<dyn AudioOutputFactory>) -> Self {
        self.output = output;
        self
    }

    /// Shared accounting domain used by this extension instance.
    #[must_use]
    pub fn host(&self) -> &Arc<AudioHost> {
        &self.host
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        Arc<AudioHost>,
        Arc<dyn AudioPermissions>,
        AudioLimits,
        Arc<dyn AudioOutputFactory>,
    ) {
        (self.host, self.permissions, self.limits, self.output)
    }
}
