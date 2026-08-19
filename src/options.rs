use std::fmt;
use std::sync::Arc;

use deno_error::JsErrorBox;
use web_audio_api::output::{AudioOutputFactory, SystemAudioOutput};

use crate::host::AudioHost;

/// Whether an online context's native-event observer keeps Deno's event loop alive.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioEventPumpLiveness {
    /// Do not keep an otherwise idle runtime alive solely for audio events.
    ///
    /// This is the default for CLI-style embedders. Playback ends when the embedder stops driving
    /// and drops its runtime, and native context teardown remains cancellation-independent.
    #[default]
    Unref,
    /// Keep the runtime alive until every online context event observer retires.
    Ref,
}

impl AudioEventPumpLiveness {
    pub(crate) const fn is_refed(self) -> bool {
        matches!(self, Self::Ref)
    }
}

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
    max_audio_buffer_samples: Option<usize>,
    max_sample_rate: Option<u32>,
    max_decoded_frames: Option<usize>,
    max_decoded_samples: Option<usize>,
    max_encoded_bytes: Option<usize>,
    max_decode_jobs: Option<usize>,
    max_offline_frames: Option<usize>,
    max_offline_samples: Option<usize>,
    max_offline_render_jobs: Option<usize>,
    max_graph_nodes: Option<usize>,
    max_graph_connections: Option<usize>,
    max_scheduled_sources: Option<usize>,
    max_automation_events: Option<usize>,
    max_queued_control_commands: Option<usize>,
    max_queued_events: Option<usize>,
}

impl Default for AudioLimits {
    fn default() -> Self {
        Self {
            // Six mirrors the long-standing browser-scale online-context cap
            // while still allowing trusted embedders to opt out explicitly.
            max_online_contexts: Some(6),
            max_live_audio_bytes: Some(64 * 1024 * 1024),
            // Preserve Web Audio's nominal 32-channel and 768 kHz ranges. A
            // 16M-sample f32 buffer is 64 MiB of canonical PCM, so one buffer
            // cannot silently exceed the default aggregate byte ceiling.
            max_channels: Some(web_audio_api::MAX_CHANNELS),
            max_audio_buffer_samples: Some(16 * 1024 * 1024),
            max_sample_rate: Some(768_000),
            max_decoded_frames: Some(16 * 1024 * 1024),
            max_decoded_samples: Some(16 * 1024 * 1024),
            max_encoded_bytes: Some(32 * 1024 * 1024),
            max_decode_jobs: Some(4),
            max_offline_frames: Some(16 * 1024 * 1024),
            max_offline_samples: Some(16 * 1024 * 1024),
            max_offline_render_jobs: Some(2),
            max_graph_nodes: Some(1024),
            max_graph_connections: Some(256),
            max_scheduled_sources: Some(512),
            max_automation_events: Some(4096),
            max_queued_control_commands: Some(256),
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
            max_audio_buffer_samples: None,
            max_sample_rate: None,
            max_decoded_frames: None,
            max_decoded_samples: None,
            max_encoded_bytes: None,
            max_decode_jobs: None,
            max_offline_frames: None,
            max_offline_samples: None,
            max_offline_render_jobs: None,
            max_graph_nodes: None,
            max_graph_connections: None,
            max_scheduled_sources: None,
            max_automation_events: None,
            max_queued_control_commands: None,
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

    /// Sets the maximum logical sample count accepted by one `AudioBuffer`.
    #[must_use]
    pub const fn max_audio_buffer_samples(mut self, limit: Option<usize>) -> Self {
        self.max_audio_buffer_samples = limit;
        self
    }

    /// Sets the maximum sample rate accepted by buffer, decode, and render work.
    #[must_use]
    pub const fn max_sample_rate(mut self, limit: Option<u32>) -> Self {
        self.max_sample_rate = limit;
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

    /// Sets the maximum number of live explicit graph connections for this isolate.
    #[must_use]
    pub const fn max_graph_connections(mut self, limit: Option<usize>) -> Self {
        self.max_graph_connections = limit;
        self
    }

    /// Sets the maximum number of scheduled sources rooted by this isolate.
    #[must_use]
    pub const fn max_scheduled_sources(mut self, limit: Option<usize>) -> Self {
        self.max_scheduled_sources = limit;
        self
    }

    /// Sets the maximum number of queued automation events for this isolate.
    #[must_use]
    pub const fn max_automation_events(mut self, limit: Option<usize>) -> Self {
        self.max_automation_events = limit;
        self
    }

    /// Sets the maximum number of queued or in-flight exact graph-control commands for this
    /// isolate. Commands staged while a context is suspended remain charged.
    #[must_use]
    pub const fn max_queued_control_commands(mut self, limit: Option<usize>) -> Self {
        self.max_queued_control_commands = limit;
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

    pub(crate) const fn audio_buffer_samples(&self) -> Option<usize> {
        self.max_audio_buffer_samples
    }

    pub(crate) const fn sample_rate(&self) -> Option<u32> {
        self.max_sample_rate
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

    pub(crate) const fn graph_connections(&self) -> Option<usize> {
        self.max_graph_connections
    }

    pub(crate) const fn scheduled_sources(&self) -> Option<usize> {
        self.max_scheduled_sources
    }

    pub(crate) const fn automation_events(&self) -> Option<usize> {
        self.max_automation_events
    }

    pub(crate) const fn queued_control_commands(&self) -> Option<usize> {
        self.max_queued_control_commands
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
    event_pump_liveness: AudioEventPumpLiveness,
}

pub(crate) struct AudioExtensionParts {
    pub(crate) host: Arc<AudioHost>,
    pub(crate) permissions: Arc<dyn AudioPermissions>,
    pub(crate) limits: AudioLimits,
    pub(crate) output: Arc<dyn AudioOutputFactory>,
    pub(crate) event_pump_liveness: AudioEventPumpLiveness,
}

impl fmt::Debug for AudioExtensionOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AudioExtensionOptions")
            .field("host", &self.host)
            .field("limits", &self.limits)
            .field("event_pump_liveness", &self.event_pump_liveness)
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
            event_pump_liveness: AudioEventPumpLiveness::default(),
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

    /// Selects whether pending online native-event observers keep Deno's event loop alive.
    #[must_use]
    pub const fn event_pump_liveness(mut self, liveness: AudioEventPumpLiveness) -> Self {
        self.event_pump_liveness = liveness;
        self
    }

    /// Shared accounting domain used by this extension instance.
    #[must_use]
    pub fn host(&self) -> &Arc<AudioHost> {
        &self.host
    }

    pub(crate) fn into_parts(self) -> AudioExtensionParts {
        AudioExtensionParts {
            host: self.host,
            permissions: self.permissions,
            limits: self.limits,
            output: self.output,
            event_pump_liveness: self.event_pump_liveness,
        }
    }
}
