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
fn extension_loads_after_web_platform_dependencies() {
    let tokio_runtime = tokio_context();
    let _tokio_context = tokio_runtime.enter();
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: extensions(),
        ..Default::default()
    });

    runtime
        .execute_script(
            "phase0_runtime_smoke.js",
            r#"
        const { Event, EventTarget } = Deno.core.loadExtScript(
          "ext:deno_web/02_event.js",
        );
        const { DOMException } = Deno.core.loadExtScript(
          "ext:deno_web/01_dom_exception.js",
        );

        const target = new EventTarget();
        let calls = 0;
        target.addEventListener("phase0", () => calls++);
        target.dispatchEvent(new Event("phase0"));

        if (calls !== 1) throw new Error(`expected one listener call, got ${calls}`);
        if (new DOMException("smoke", "AbortError").name !== "AbortError") {
          throw new Error("DOMException bootstrap failed");
        }
        if (typeof AudioBuffer !== "function") {
          throw new Error("Phase 1 AudioBuffer global was not installed");
        }
        if (typeof OfflineAudioContext !== "function" ||
            typeof GainNode !== "function" ||
            typeof AudioBufferSourceNode !== "function" ||
            typeof OscillatorNode !== "function") {
          throw new Error("Phase 1 offline graph globals were not installed");
        }
        if (typeof AudioContext !== "function" ||
            !(AudioContext.prototype instanceof BaseAudioContext)) {
          throw new Error("Phase 2 AudioContext global was not installed");
        }
      "#,
        )
        .expect("the pinned Deno tuple and public extension should initialize");
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
