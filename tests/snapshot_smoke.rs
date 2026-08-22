//! Startup-snapshot bootstrap evidence.
//!
//! This lives in its own integration-test binary because `JsRuntimeForSnapshot`
//! initializes process-global V8 with snapshot flags (`--predictable`, a fixed
//! random seed) while a plain `JsRuntime` initializes it without them; whichever
//! construction runs first configures V8 for the whole process. Sharing a
//! process with non-snapshot runtime tests races that one-time initialization.

use deno_core::{Extension, JsRuntime, JsRuntimeForSnapshot, RuntimeOptions};

fn extensions() -> Vec<Extension> {
    vec![
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::init(
            deno_web::BlobStore::default_arc(),
            None,
            Default::default(),
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_audio::deno_audio::init(deno_audio::AudioExtensionOptions::default()),
    ]
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
fn extension_bootstrap_survives_a_startup_snapshot() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let snapshot = {
        let runtime = JsRuntimeForSnapshot::new(RuntimeOptions {
            extensions: extensions(),
            ..RuntimeOptions::default()
        });
        runtime.snapshot()
    };
    let snapshot = Box::leak(snapshot);
    let mut runtime = JsRuntime::new(RuntimeOptions {
        startup_snapshot: Some(snapshot),
        extensions: extensions(),
        ..RuntimeOptions::default()
    });

    runtime
        .execute_script(
            "phase1_snapshot_smoke.js",
            r#"
              const buffer = new AudioBuffer({ length: 2, sampleRate: 48000 });
              if (buffer.length !== 2 || buffer.numberOfChannels !== 1) {
                throw new Error("snapshotted AudioBuffer bootstrap is unusable");
              }
              const context = new OfflineAudioContext(1, 128, 48000);
              if (!(context.createGain() instanceof GainNode)) {
                throw new Error("snapshotted offline graph bootstrap is unusable");
              }
            "#,
        )
        .expect("the public bootstrap should be snapshot-friendly");
}
