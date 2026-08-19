use deno_core::{JsRuntime, RuntimeOptions};

#[test]
fn private_extension_loads_after_web_platform_dependencies() {
    let mut runtime = JsRuntime::new(RuntimeOptions {
        extensions: vec![
            deno_webidl::deno_webidl::init(),
            deno_web::deno_web::init(
                deno_web::BlobStore::default_arc(),
                None,
                Default::default(),
                deno_web::InMemoryBroadcastChannel::default(),
            ),
            deno_audio::deno_audio::init(),
        ],
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
        if (typeof AudioContext !== "undefined") {
          throw new Error("Phase 0 extension unexpectedly installed AudioContext");
        }
      "#,
        )
        .expect("the pinned Deno tuple and empty extension should initialize");
}
