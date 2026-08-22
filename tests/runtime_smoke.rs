//! Extension bootstrap evidence for a plain (non-snapshot) runtime. The
//! startup-snapshot variant lives in `snapshot_smoke.rs`: `JsRuntimeForSnapshot`
//! configures process-global V8 differently, and the two constructions must not
//! race one another in a single process.

use deno_core::{Extension, JsRuntime, RuntimeOptions};

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
