use std::panic::{self, AssertUnwindSafe};
use std::thread;

use deno_core::op2;
use deno_core::serde::Serialize;
use deno_core::unsync::spawn_blocking;
use deno_error::JsErrorBox;
use web_audio_api::context::{BaseAudioContext, OfflineAudioContext};
use web_audio_api::node::{AudioNode, AudioScheduledSourceNode};
use web_audio_api::MAX_CHANNELS;

#[derive(Serialize)]
#[serde(crate = "deno_core::serde", rename_all = "camelCase")]
struct Phase0RenderSummary {
    number_of_channels: u32,
    length: u32,
    peak: f64,
    off_isolate_thread: bool,
}

pub(crate) enum BlockingOutcome<T> {
    Complete(T),
    Failed(String),
    Panicked,
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "Web Audio exposes a double sample rate while the pinned engine stores validated rates as f32"
)]
pub(crate) fn validate_sample_rate(sample_rate: f64) -> Result<f32, JsErrorBox> {
    if !(3_000.0..=768_000.0).contains(&sample_rate) {
        return Err(JsErrorBox::range_error(
            "sampleRate must be in the inclusive range 3000..=768000",
        ));
    }
    Ok(sample_rate as f32)
}

pub(crate) fn blocking_outcome<T>(
    operation: impl FnOnce() -> Result<T, String>,
) -> BlockingOutcome<T> {
    match panic::catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(value)) => BlockingOutcome::Complete(value),
        Ok(Err(error)) => BlockingOutcome::Failed(error),
        Err(payload) => {
            std::mem::forget(payload);
            BlockingOutcome::Panicked
        }
    }
}

pub(crate) async fn resolve_blocking<T>(
    operation_name: &'static str,
    task: impl FnOnce() -> BlockingOutcome<T> + Send + 'static,
) -> Result<T, JsErrorBox>
where
    T: Send + 'static,
{
    match spawn_blocking(task).await {
        Ok(BlockingOutcome::Complete(value)) => Ok(value),
        Ok(BlockingOutcome::Failed(error)) => Err(JsErrorBox::generic(error)),
        Ok(BlockingOutcome::Panicked) => Err(JsErrorBox::generic(format!(
            "{operation_name} panicked on the blocking pool"
        ))),
        Err(error) => Err(JsErrorBox::generic(format!(
            "{operation_name} blocking task failed: {error}"
        ))),
    }
}

/// Phase 0 proof op. Every value captured across the await is an owned Rust
/// scalar; context construction, graph mutation, rendering, and destruction
/// all occur on the blocking worker.
#[op2]
#[serde]
pub(crate) async fn op_phase0_render_off_thread(
    number_of_channels: u32,
    length: u32,
    sample_rate: f64,
) -> Result<Phase0RenderSummary, JsErrorBox> {
    let number_of_channels = number_of_channels as usize;
    if !(1..=MAX_CHANNELS).contains(&number_of_channels) {
        return Err(JsErrorBox::range_error(
            "numberOfChannels must be in the inclusive range 1..=32",
        ));
    }
    let length = length as usize;
    if length == 0 {
        return Err(JsErrorBox::range_error("length must be greater than zero"));
    }
    let sample_rate = validate_sample_rate(sample_rate)?;
    let isolate_thread = thread::current().id();

    resolve_blocking("offline audio render", move || {
        blocking_outcome(|| {
            let mut context = OfflineAudioContext::new(number_of_channels, length, sample_rate);
            let mut oscillator = context.create_oscillator();
            oscillator.frequency().set_value(440.0);
            oscillator.connect(&context.destination());
            oscillator.start();

            let rendered = context.start_rendering_sync();
            let peak = rendered
                .get_channel_data(0)
                .iter()
                .copied()
                .map(f32::abs)
                .fold(0.0_f32, f32::max);
            Ok(Phase0RenderSummary {
                number_of_channels: u32::try_from(rendered.number_of_channels())
                    .map_err(|_| "rendered channel count exceeds the binding range".to_owned())?,
                length: u32::try_from(rendered.length())
                    .map_err(|_| "rendered frame count exceeds the binding range".to_owned())?,
                peak: f64::from(peak),
                off_isolate_thread: thread::current().id() != isolate_thread,
            })
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use deno_core::futures::{executor, join, FutureExt as _};
    use deno_core::{JsRuntime, RuntimeOptions};

    use super::*;

    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("hostile panic payload destructor");
        }
    }

    #[test]
    fn blocking_panic_boundary_contains_a_hostile_payload() {
        let outcome = blocking_outcome(|| -> Result<(), String> {
            std::panic::panic_any(PanicOnDrop);
        });
        assert!(matches!(outcome, BlockingOutcome::Panicked));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn decode_and_render_ops_leave_the_isolate_thread() {
        let mut runtime = JsRuntime::new(RuntimeOptions {
            extensions: vec![
                deno_webidl::deno_webidl::init(),
                deno_web::deno_web::init(
                    deno_web::BlobStore::default_arc(),
                    None,
                    Default::default(),
                    deno_web::InMemoryBroadcastChannel::default(),
                ),
                crate::deno_audio::init(crate::AudioExtensionOptions::default()),
            ],
            ..RuntimeOptions::default()
        });

        runtime
            .execute_script(
                "phase0_off_thread.js",
                r#"
                  globalThis.__phase0OffThreadDone = false;
                  globalThis.__phase0OffThread = (async () => {
                    const bytes = new Uint8Array(52);
                    const view = new DataView(bytes.buffer);
                    const text = (offset, value) => {
                      for (let i = 0; i < value.length; i++) {
                        bytes[offset + i] = value.charCodeAt(i);
                      }
                    };
                    text(0, "RIFF"); view.setUint32(4, 44, true);
                    text(8, "WAVEfmt "); view.setUint32(16, 16, true);
                    view.setUint16(20, 1, true); view.setUint16(22, 1, true);
                    view.setUint32(24, 48000, true); view.setUint32(28, 96000, true);
                    view.setUint16(32, 2, true); view.setUint16(34, 16, true);
                    text(36, "data"); view.setUint32(40, 8, true);
                    view.setInt16(44, 0, true); view.setInt16(46, 8192, true);
                    view.setInt16(48, -8192, true); view.setInt16(50, 0, true);

                    const request = new Deno.core.ops.NativePhase0DecodeRequest(
                      bytes,
                      48000,
                    );
                    const decoded = await Deno.core.ops.op_phase0_decode_off_thread(request);
                    if (decoded.numberOfChannels !== 1 || decoded.length !== 4 ||
                        decoded.sampleRate !== 48000 || !decoded.offIsolateThread) {
                      throw new Error(`unexpected decode summary: ${JSON.stringify(decoded)}`);
                    }

                    const rendered = await Deno.core.ops.op_phase0_render_off_thread(
                      1,
                      128,
                      48000,
                    );
                    if (rendered.numberOfChannels !== 1 || rendered.length !== 128 ||
                        rendered.peak <= 0.01 || !rendered.offIsolateThread) {
                      throw new Error(`unexpected render summary: ${JSON.stringify(rendered)}`);
                    }
                    globalThis.__phase0OffThreadDone = true;
                  })();
                "#,
            )
            .expect("the owned Phase 0 requests should enter the async op boundary");
        runtime
            .run_event_loop(deno_core::PollEventLoopOptions::default())
            .await
            .expect("decode and render promises should settle");
        runtime
            .execute_script(
                "phase0_off_thread_result.js",
                r#"
                  if (globalThis.__phase0OffThreadDone !== true) {
                    throw new Error("off-thread proof did not complete");
                  }
                "#,
            )
            .expect("the off-thread proof promise should be fulfilled");
    }

    #[test]
    fn offline_suspend_and_resume_observe_the_render_quantum() {
        let context = Arc::new(OfflineAudioContext::new(1, 512, 44_100.0));
        let resumed_context = Arc::clone(&context);
        let suspend = context.suspend(128.0 / 44_100.0).then(|()| async move {
            let mut source = resumed_context.create_constant_source();
            source.connect(&resumed_context.destination());
            source.start();
            resumed_context.resume().await;
        });
        let render = context.start_rendering();
        let rendered = executor::block_on(async move { join!(suspend, render).1 });

        assert!(rendered.get_channel_data(0)[..128]
            .iter()
            .all(|sample| *sample == 0.0));
        assert!(rendered.get_channel_data(0)[128..]
            .iter()
            .all(|sample| (*sample - 1.0).abs() <= f32::EPSILON));
    }
}
