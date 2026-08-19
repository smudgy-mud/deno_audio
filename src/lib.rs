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
mod node;
mod offline;

deno_core::extension!(
    deno_audio,
    deps = [deno_webidl, deno_web],
    ops = [
        offline::op_phase0_decode_off_thread,
        offline::op_phase0_render_off_thread,
    ],
    objects = [binding::NativePhase0Probe, buffer::NativeAudioBuffer],
    state = |state| {
        state.put(std::sync::Arc::new(binding::NativeBindingMetrics::default()));
        state.put(std::sync::Arc::new(buffer::NativeAudioByteBudget::new(
            buffer::DEFAULT_NATIVE_AUDIO_BYTE_LIMIT,
        )));
    },
);
