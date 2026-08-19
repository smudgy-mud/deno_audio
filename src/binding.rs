use std::cell::Cell;
use std::ffi::CStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use deno_core::cppgc::{self, GcCell, Member};
use deno_core::{op2, v8, GarbageCollected, OpState};

#[derive(Default)]
pub(crate) struct NativeBindingMetrics {
    created: AtomicUsize,
    finalized: AtomicUsize,
    traces: AtomicUsize,
}

/// Private `CppGC` proof object used to establish the native-object conventions
/// before spec-facing JavaScript wrappers are installed.
pub(crate) struct NativePhase0Probe {
    value: Cell<u32>,
    child: GcCell<Option<Member<Self>>>,
    metrics: Arc<NativeBindingMetrics>,
}

// SAFETY: every managed pointer is stored in `child` and traced below. The
// remaining fields contain no V8 or CppGC pointers.
unsafe impl GarbageCollected for NativePhase0Probe {
    fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
        self.metrics.traces.fetch_add(1, Ordering::Relaxed);
        visitor.trace(&self.child);
    }

    fn get_name(&self) -> &'static CStr {
        c"NativePhase0Probe"
    }
}

impl Drop for NativePhase0Probe {
    fn drop(&mut self) {
        self.metrics.finalized.fetch_add(1, Ordering::Release);
    }
}

#[op2]
impl NativePhase0Probe {
    #[constructor]
    #[cppgc]
    fn new(state: &mut OpState, value: u32) -> NativePhase0Probe {
        let metrics = Arc::clone(state.borrow::<Arc<NativeBindingMetrics>>());
        metrics.created.fetch_add(1, Ordering::Relaxed);
        NativePhase0Probe {
            value: Cell::new(value),
            child: GcCell::new(None),
            metrics,
        }
    }

    #[getter]
    fn value(&self) -> u32 {
        self.value.get()
    }

    #[fast]
    fn set_value(&self, value: u32) {
        self.value.set(value);
    }

    #[fast]
    fn retain<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        value: v8::Local<'s, v8::Value>,
    ) -> bool {
        let Some(child) = cppgc::try_unwrap_cppgc_persistent_object::<Self>(scope, value) else {
            return false;
        };
        *self.child.get_mut(scope) = Some(child.into());
        true
    }

    #[fast]
    fn clear(&self, scope: &mut v8::PinScope<'_, '_>) {
        self.child.get_mut(scope).take();
    }

    fn child_value(&self, scope: &mut v8::PinScope<'_, '_>) -> Option<u32> {
        self.child
            .get(scope)
            .as_ref()
            .map(|child| child.value.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_core::{JsRuntime, RuntimeOptions};

    fn runtime() -> JsRuntime {
        JsRuntime::new(RuntimeOptions {
            extensions: vec![
                deno_webidl::deno_webidl::init(),
                deno_web::deno_web::init(
                    deno_web::BlobStore::default_arc(),
                    None,
                    Default::default(),
                    deno_web::InMemoryBroadcastChannel::default(),
                ),
                crate::deno_audio::init(),
            ],
            unsafe_expose_natives_and_gc: true,
            ..RuntimeOptions::default()
        })
    }

    fn metrics(runtime: &JsRuntime) -> Arc<NativeBindingMetrics> {
        Arc::clone(
            runtime
                .op_state()
                .borrow()
                .borrow::<Arc<NativeBindingMetrics>>(),
        )
    }

    fn collect(runtime: &mut JsRuntime) {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
    }

    #[test]
    fn cppgc_dispatch_tracing_and_finalization_are_real() {
        let mut runtime = runtime();
        let metrics = metrics(&runtime);

        runtime
            .execute_script(
                "phase0_cppgc.js",
                r#"
                  (() => {
                    const { NativePhase0Probe } = Deno.core.ops;
                    const parent = new NativePhase0Probe(7);
                    let child = new NativePhase0Probe(22);
                    if (parent.value !== 7) throw new Error("getter dispatch failed");
                    parent.setValue(9);
                    if (parent.value !== 9) throw new Error("method dispatch failed");
                    if (!parent.retain(child)) throw new Error("CppGC link failed");
                    if (parent.childValue() !== 22) throw new Error("linked dispatch failed");
                    child = undefined;
                    globalThis.__phase0Parent = parent;

                    let brandChecked = false;
                    try {
                      NativePhase0Probe.prototype.setValue.call({}, 1);
                    } catch {
                      brandChecked = true;
                    }
                    if (!brandChecked) throw new Error("CppGC method accepted a foreign receiver");
                  })();
                "#,
            )
            .expect("private CppGC construction and dispatch should succeed");
        assert_eq!(metrics.created.load(Ordering::Acquire), 2);

        collect(&mut runtime);
        assert!(metrics.traces.load(Ordering::Acquire) > 0);
        assert_eq!(
            metrics.finalized.load(Ordering::Acquire),
            0,
            "the traced member must keep its child alive"
        );

        runtime
            .execute_script(
                "phase0_cppgc_clear.js",
                "globalThis.__phase0Parent.clear();",
            )
            .expect("clearing the traced member should succeed");
        collect(&mut runtime);
        assert_eq!(metrics.finalized.load(Ordering::Acquire), 1);

        runtime
            .execute_script(
                "phase0_cppgc_release.js",
                "globalThis.__phase0Parent = undefined;",
            )
            .expect("releasing the final JS root should succeed");
        collect(&mut runtime);
        assert_eq!(metrics.finalized.load(Ordering::Acquire), 2);
    }
}
