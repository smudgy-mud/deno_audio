//! Web Audio API extension for `deno_core`.
//!
//! The crate contains the proven native ownership foundation and installs the
//! Web Audio globals implemented by the current public vertical slice.
//!
//! This project is not affiliated with, endorsed by, or maintained by
//! Deno Land Inc.

#![warn(missing_docs)]

mod binding;
mod buffer;
mod decode;
mod host;
mod lifecycle;
mod node;
mod offline;
mod online;
mod options;
mod scheduled;

pub use host::{
    AudioHost, AudioHostLimitError, AudioHostLimits, AudioHostResourceKind, AudioHostUsage,
};
pub use lifecycle::{
    AudioLifecycleOwner, AudioLifecycleRegistrar, AudioLifecycleRetirement,
    AudioLifecycleRetirementError,
};
pub use options::{
    AllowAllAudioPermissions, AudioEventPumpLiveness, AudioExtensionOptions, AudioLimits,
    AudioPermissions,
};
pub use web_audio_api::context::{AudioContextLatencyCategory, AudioContextRenderSizeCategory};
pub use web_audio_api::output::{
    AudioOutputConfig, AudioOutputContextId, AudioOutputDeathReason, AudioOutputEndpointShutdown,
    AudioOutputError, AudioOutputErrorKind, AudioOutputEvent, AudioOutputEventSink,
    AudioOutputFactory, AudioOutputRequest, AudioOutputStartFailure, AudioRenderCallback,
    AudioRenderFormat, AudioRenderStatus, PreparedAudioOutput, RunningAudioOutput,
    SystemAudioOutput, MAX_AUDIO_OUTPUT_CALLBACK_FRAMES,
};

deno_core::extension!(
    deno_audio,
    deps = [deno_webidl, deno_web],
    ops = [
        decode::op_phase0_decode_off_thread,
        decode::op_decode_audio_data,
        offline::op_phase0_render_off_thread,
        offline::op_offline_start_rendering,
        online::op_phase0_online_suspend,
        online::op_phase0_online_resume,
        online::op_phase0_online_close,
        scheduled::op_phase0_wait_ended,
        scheduled::op_online_wait_events,
    ],
    objects = [
        binding::NativePhase0Probe,
        buffer::NativeAudioBuffer,
        decode::NativePhase0DecodeRequest,
        offline::NativePhase0OfflineRenderRequest,
        offline::NativeOfflineAudioContext,
        node::NativeAudioNode,
        node::NativePeriodicWave,
        online::NativePhase0OnlineContext,
        scheduled::NativePhase0Oscillator,
    ],
    esm_entry_point = "ext:deno_audio/99_main.js",
    esm = [
        dir "js",
        "00_webidl.js",
        "01_audio_buffer.js",
        "02_offline_audio.js",
        "99_main.js",
    ],
    options = {
        options: AudioExtensionOptions,
    },
    state = |state, extension_options| {
        let options::AudioExtensionParts {
            host,
            permissions,
            limits,
            output,
            event_pump_liveness,
            lifecycle_registrar,
        } = extension_options.options.into_parts();
        let native_audio_byte_limit = limits
            .live_audio_bytes()
            .unwrap_or(usize::MAX);
        state.put(std::sync::Arc::new(binding::NativeBindingMetrics::default()));
        state.put(std::sync::Arc::new(buffer::NativeAudioByteBudget::with_limits(
            native_audio_byte_limit,
            std::sync::Arc::clone(&host),
            limits.channels(),
            limits.audio_buffer_samples(),
            limits.sample_rate(),
        )));
        state.put(std::sync::Arc::new(online::AudioRuntimeState::new(
            host,
            permissions,
            limits,
            output,
            event_pump_liveness,
            lifecycle_registrar,
        )));
    },
);
