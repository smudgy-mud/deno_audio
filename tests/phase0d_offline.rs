//! Phase 0D acceptance evidence for bounded offline-render ownership.

use std::sync::Arc;
use std::time::{Duration, Instant};

use deno_audio::{AudioExtensionOptions, AudioHost, AudioHostLimits, AudioLimits};
use deno_core::{JsRuntime, RuntimeOptions};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn runtime(host: Arc<AudioHost>, limits: AudioLimits) -> JsRuntime {
    JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(AudioExtensionOptions::new(host).limits(limits)),
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
            "timed out waiting for offline-render cleanup"
        );
        std::thread::yield_now();
    }
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

#[test]
fn dimensions_jobs_and_output_bytes_reject_before_render_construction() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::default());
    let mut isolate = runtime(
        Arc::clone(&host),
        AudioLimits::unlimited()
            .max_channels(Some(2))
            .max_sample_rate(Some(44_100))
            .max_offline_frames(Some(128))
            .max_offline_samples(Some(128))
            .max_offline_render_jobs(Some(1))
            .max_live_audio_bytes(Some(512)),
    );
    isolate
        .execute_script(
            "phase0d_offline_limits.js",
            r#"
              const rejected = (channels, frames, sampleRate = 44100) => {
                try {
                  new Deno.core.ops.NativePhase0OfflineRenderRequest(
                    channels,
                    frames,
                    sampleRate,
                  );
                  return false;
                } catch { return true; }
              };
              if (!rejected(3, 42)) throw new Error("channel limit did not reject");
              if (!rejected(1, 129)) throw new Error("frame limit did not reject");
              if (!rejected(2, 65)) throw new Error("sample limit did not reject");
              if (!rejected(1, 1, 48000)) {
                throw new Error("sample-rate limit did not reject");
              }

              globalThis.__heldOffline = new Deno.core.ops.NativePhase0OfflineRenderRequest(
                1,
                128,
                44100,
              );
              if (!rejected(1, 1)) throw new Error("offline job limit did not reject");
            "#,
        )
        .expect("offline limits should reject synchronously");
    assert_eq!(host.usage().offline_render_jobs(), 1);
    assert_eq!(host.usage().live_audio_bytes(), 512);

    isolate
        .execute_script(
            "phase0d_release_offline_token.js",
            "globalThis.__heldOffline = undefined;",
        )
        .unwrap();
    isolate
        .v8_isolate()
        .request_garbage_collection_for_testing(deno_core::v8::GarbageCollectionType::Full);
    wait_until(|| host.usage().offline_render_jobs() == 0 && host.usage().live_audio_bytes() == 0);

    let byte_host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_live_audio_bytes(Some(511)),
    ));
    let mut byte_runtime = runtime(byte_host.clone(), AudioLimits::unlimited());
    byte_runtime
        .execute_script(
            "phase0d_offline_byte_rejection.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0OfflineRenderRequest(1, 128, 48000);
              } catch { rejected = true; }
              if (!rejected) throw new Error("offline output bytes did not reject");
            "#,
        )
        .unwrap();
    assert_eq!(byte_host.usage().offline_render_jobs(), 0);
    assert_eq!(byte_host.usage().live_audio_bytes(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn accepted_offline_work_is_single_use_and_outlives_its_waiter() {
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default()
            .max_offline_render_jobs(Some(1))
            .max_live_audio_bytes(Some(1_024)),
    ));
    let mut runtime = runtime(Arc::clone(&host), AudioLimits::unlimited());
    runtime
        .execute_script(
            "phase0d_single_use_offline.js",
            r#"
              globalThis.__singleUseOffline = (async () => {
                const request = new Deno.core.ops.NativePhase0OfflineRenderRequest(
                  1,
                  128,
                  48000,
                );
                const first = Deno.core.ops.op_phase0_render_off_thread(request);
                let replayRejected = false;
                try {
                  await Deno.core.ops.op_phase0_render_off_thread(request);
                } catch { replayRejected = true; }
                if (!replayRejected) throw new Error("offline token was replayable");
                const result = await first;
                if (result.length !== 128 || result.peak <= 0.01 || !result.offIsolateThread) {
                  throw new Error("offline render result was invalid");
                }
              })();
            "#,
        )
        .expect("offline work should transfer exactly once");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("single-use offline work should settle");
    wait_until(|| host.usage().offline_render_jobs() == 0 && host.usage().live_audio_bytes() == 0);

    runtime
        .execute_script(
            "phase0d_discard_offline_waiter.js",
            r"
              const request = new Deno.core.ops.NativePhase0OfflineRenderRequest(
                1,
                128,
                48000,
              );
              Deno.core.ops.op_phase0_render_off_thread(request);
            ",
        )
        .expect("accepted offline work should not depend on its JavaScript promise");
    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await
        .expect("discarded offline waiter must not cancel native cleanup");
    wait_until(|| host.usage().offline_render_jobs() == 0 && host.usage().live_audio_bytes() == 0);
}

#[test]
fn shared_offline_jobs_reject_a_second_isolate_and_recover() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let host = Arc::new(AudioHost::new(
        AudioHostLimits::default().max_offline_render_jobs(Some(1)),
    ));
    let mut first = runtime(Arc::clone(&host), AudioLimits::unlimited());
    first
        .execute_script(
            "phase0d_hold_shared_offline.js",
            r"
              globalThis.request = new Deno.core.ops.NativePhase0OfflineRenderRequest(
                1,
                128,
                48000,
              );
            ",
        )
        .unwrap();
    let mut second = runtime(Arc::clone(&host), AudioLimits::unlimited());
    second
        .execute_script(
            "phase0d_reject_shared_offline.js",
            r#"
              let rejected = false;
              try {
                new Deno.core.ops.NativePhase0OfflineRenderRequest(1, 1, 48000);
              } catch { rejected = true; }
              if (!rejected) throw new Error("shared offline job did not reject");
            "#,
        )
        .unwrap();
    assert_eq!(host.usage().offline_render_jobs(), 1);
    assert_eq!(host.usage().live_audio_bytes(), 512);
    drop(second);
    drop(first);
    wait_until(|| host.usage().offline_render_jobs() == 0 && host.usage().live_audio_bytes() == 0);
}
