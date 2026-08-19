//! Web Audio API extension for `deno_core`.
//!
//! This crate is in its Phase 0 binding spike. It declares its ordering
//! dependencies on `deno_webidl` and `deno_web` and contains private native
//! binding primitives; no Web Audio globals are installed yet.
//!
//! This project is not affiliated with, endorsed by, or maintained by
//! Deno Land Inc.

#![warn(missing_docs)]

mod binding;
mod buffer;
mod decode;
mod host;
mod node;
mod offline;
mod online;
mod options;
mod scheduled;

pub use host::{
    AudioHost, AudioHostLimitError, AudioHostLimits, AudioHostResourceKind, AudioHostUsage,
};
pub use options::{AllowAllAudioPermissions, AudioExtensionOptions, AudioLimits, AudioPermissions};

deno_core::extension!(
    deno_audio,
    deps = [deno_webidl, deno_web],
    ops = [
        decode::op_phase0_decode_off_thread,
        offline::op_phase0_render_off_thread,
        online::op_phase0_online_suspend,
        online::op_phase0_online_resume,
        online::op_phase0_online_close,
        scheduled::op_phase0_wait_ended,
    ],
    objects = [
        binding::NativePhase0Probe,
        buffer::NativeAudioBuffer,
        decode::NativePhase0DecodeRequest,
        offline::NativePhase0OfflineRenderRequest,
        online::NativePhase0OnlineContext,
        scheduled::NativePhase0Oscillator,
    ],
    options = {
        options: AudioExtensionOptions,
    },
    state = |state, extension_options| {
        let (host, permissions, limits, output) = extension_options.options.into_parts();
        let native_audio_byte_limit = limits
            .live_audio_bytes()
            .unwrap_or(usize::MAX);
        state.put(std::sync::Arc::new(binding::NativeBindingMetrics::default()));
        state.put(std::sync::Arc::new(buffer::NativeAudioByteBudget::new(
            native_audio_byte_limit,
            std::sync::Arc::clone(&host),
        )));
        state.put(std::sync::Arc::new(online::AudioRuntimeState::new(
            host,
            permissions,
            limits,
            output,
        )));
    },
);
