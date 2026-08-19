use std::ffi::CStr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use deno_core::cppgc::GcCell;
use deno_core::{op2, v8, GarbageCollected, OpState};
use deno_error::JsErrorBox;
use web_audio_api::{AudioBuffer, AudioBufferOptions, MAX_CHANNELS};

pub(crate) const DEFAULT_NATIVE_AUDIO_BYTE_LIMIT: usize = 64 * 1024 * 1024;
const BYTES_PER_SAMPLE: usize = size_of::<f32>();

pub(crate) struct NativeAudioByteBudget {
    limit: usize,
    used: AtomicUsize,
}

impl NativeAudioByteBudget {
    pub(crate) const fn new(limit: usize) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
        }
    }

    fn reserve(self: &Arc<Self>, bytes: usize) -> Option<NativeAudioByteReservation> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let next = used.checked_add(bytes)?;
            if next > self.limit {
                return None;
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    return Some(NativeAudioByteReservation {
                        budget: Arc::clone(self),
                        bytes,
                    });
                }
                Err(observed) => used = observed,
            }
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

struct NativeAudioByteReservation {
    budget: Arc<NativeAudioByteBudget>,
    bytes: usize,
}

impl Drop for NativeAudioByteReservation {
    fn drop(&mut self) {
        let previous = self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
        debug_assert!(previous >= self.bytes);
    }
}

/// Storage handed to V8 for one exposed channel copy.
///
/// The safe `rusty_v8` backing-store constructor owns this box and its destructor.
/// Consequently the reservation follows the bytes through `ArrayBuffer` transfer
/// and is released only when the final backing-store owner disappears.
struct ExposedChannelStorage {
    bytes: Box<[u8]>,
    _reservation: NativeAudioByteReservation,
}

impl AsMut<[u8]> for ExposedChannelStorage {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

struct NativeAudioBufferState {
    canonical: AudioBuffer,
    exposed: Vec<Option<v8::TracedReference<v8::Float32Array>>>,
}

impl v8::cppgc::Traced for NativeAudioBufferState {
    fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
        for exposed in &self.exposed {
            visitor.trace(exposed);
        }
    }
}

/// Private native `AudioBuffer` storage used by the JavaScript facade in later
/// phases. Canonical PCM is Rust-owned; only explicitly exposed channels gain
/// traced V8 copies.
pub(crate) struct NativeAudioBuffer {
    state: GcCell<NativeAudioBufferState>,
    budget: Arc<NativeAudioByteBudget>,
    _canonical_reservation: NativeAudioByteReservation,
}

// SAFETY: every V8 handle is a TracedReference inside `state`, and `trace`
// visits the entire exposed-channel set. Canonical PCM and reservations contain
// no V8 or CppGC pointers.
unsafe impl GarbageCollected for NativeAudioBuffer {
    fn trace(&self, visitor: &mut v8::cppgc::Visitor) {
        visitor.trace(&self.state);
    }

    fn get_name(&self) -> &'static CStr {
        c"NativeAudioBuffer"
    }
}

fn checked_pcm_bytes(channels: usize, length: usize) -> Option<usize> {
    channels.checked_mul(length)?.checked_mul(BYTES_PER_SAMPLE)
}

#[allow(
    clippy::cast_possible_truncation,
    reason = "Web Audio exposes a double sample rate while the pinned engine stores validated rates as f32"
)]
fn validate_options(
    number_of_channels: u32,
    length: u32,
    sample_rate: f64,
) -> Result<AudioBufferOptions, JsErrorBox> {
    let channels = number_of_channels as usize;
    let length = length as usize;
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return Err(JsErrorBox::range_error(
            "numberOfChannels must be in the inclusive range 1..=32",
        ));
    }
    if length == 0 {
        return Err(JsErrorBox::range_error("length must be greater than zero"));
    }
    if !(3_000.0..=768_000.0).contains(&sample_rate) {
        return Err(JsErrorBox::range_error(
            "sampleRate must be in the inclusive range 3000..=768000",
        ));
    }
    Ok(AudioBufferOptions {
        number_of_channels: channels,
        length,
        sample_rate: sample_rate as f32,
    })
}

fn encode_samples(samples: &[f32]) -> Box<[u8]> {
    let mut bytes = vec![0_u8; samples.len() * BYTES_PER_SAMPLE].into_boxed_slice();
    for (sample, destination) in samples.iter().zip(bytes.chunks_exact_mut(BYTES_PER_SAMPLE)) {
        destination.copy_from_slice(&sample.to_ne_bytes());
    }
    bytes
}

fn view_sample(view: &v8::Float32Array, index: usize) -> Option<f32> {
    let store = view.get_backing_store()?;
    let offset = view
        .byte_offset()
        .checked_add(index.checked_mul(BYTES_PER_SAMPLE)?)?;
    let end = offset.checked_add(BYTES_PER_SAMPLE)?;
    let source = store.get(offset..end)?;
    Some(f32::from_ne_bytes([
        source[0].get(),
        source[1].get(),
        source[2].get(),
        source[3].get(),
    ]))
}

fn set_view_sample(view: &v8::Float32Array, index: usize, value: f32) -> bool {
    let Some(store) = view.get_backing_store() else {
        return false;
    };
    let Some(offset) = index
        .checked_mul(BYTES_PER_SAMPLE)
        .and_then(|offset| view.byte_offset().checked_add(offset))
    else {
        return false;
    };
    let Some(end) = offset.checked_add(BYTES_PER_SAMPLE) else {
        return false;
    };
    let Some(destination) = store.get(offset..end) else {
        return false;
    };
    for (destination, source) in destination.iter().zip(value.to_ne_bytes()) {
        destination.set(source);
    }
    true
}

fn view_is_detached<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    view: v8::Local<'s, v8::Float32Array>,
) -> bool {
    view.buffer(scope)
        .is_none_or(|buffer| buffer.was_detached())
}

fn check_channel(state: &NativeAudioBufferState, channel: u32) -> Result<usize, JsErrorBox> {
    let channel = channel as usize;
    if channel >= state.canonical.number_of_channels() {
        return Err(JsErrorBox::range_error("channel index is out of range"));
    }
    Ok(channel)
}

fn check_sample(
    state: &NativeAudioBufferState,
    channel: u32,
    sample: u32,
) -> Result<(usize, usize), JsErrorBox> {
    let channel = check_channel(state, channel)?;
    let sample = sample as usize;
    if sample >= state.canonical.length() {
        return Err(JsErrorBox::range_error("sample index is out of range"));
    }
    Ok((channel, sample))
}

#[op2]
impl NativeAudioBuffer {
    #[constructor]
    #[cppgc]
    fn new(
        state: &mut OpState,
        number_of_channels: u32,
        length: u32,
        sample_rate: f64,
    ) -> Result<NativeAudioBuffer, JsErrorBox> {
        let options = validate_options(number_of_channels, length, sample_rate)?;
        let canonical_bytes = checked_pcm_bytes(options.number_of_channels, options.length)
            .ok_or_else(|| JsErrorBox::range_error("AudioBuffer dimensions overflow"))?;
        let budget = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
        let canonical_reservation = budget.reserve(canonical_bytes).ok_or_else(|| {
            JsErrorBox::range_error("native audio byte budget exceeded before allocation")
        })?;
        let channel_count = options.number_of_channels;
        let canonical = AudioBuffer::new(options);

        Ok(NativeAudioBuffer {
            state: GcCell::new(NativeAudioBufferState {
                canonical,
                exposed: (0..channel_count).map(|_| None).collect(),
            }),
            budget,
            _canonical_reservation: canonical_reservation,
        })
    }

    #[getter]
    fn number_of_channels(&self, isolate: &mut v8::Isolate) -> u32 {
        u32::try_from(self.state.get(isolate).canonical.number_of_channels())
            .expect("validated AudioBuffer channel count must fit u32")
    }

    #[getter]
    fn length(&self, isolate: &mut v8::Isolate) -> u32 {
        u32::try_from(self.state.get(isolate).canonical.length())
            .expect("validated AudioBuffer length must fit u32")
    }

    #[getter]
    fn budget_used(&self) -> u32 {
        u32::try_from(self.budget.used()).unwrap_or(u32::MAX)
    }

    fn get_channel_data<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        channel: u32,
    ) -> Result<v8::Local<'s, v8::Float32Array>, JsErrorBox> {
        let channel = check_channel(self.state.get(scope), channel)?;
        if let Some(exposed) = self.state.get_mut(scope).exposed[channel].take() {
            let view = exposed.get(scope);
            self.state.get_mut(scope).exposed[channel] = Some(exposed);
            return view
                .ok_or_else(|| JsErrorBox::generic("traced AudioBuffer channel view disappeared"));
        }

        let samples = self.state.get(scope).canonical.get_channel_data(channel);
        let byte_length = samples
            .len()
            .checked_mul(BYTES_PER_SAMPLE)
            .ok_or_else(|| JsErrorBox::range_error("channel byte length overflow"))?;
        let sample_count = samples.len();
        let reservation = self.budget.reserve(byte_length).ok_or_else(|| {
            JsErrorBox::range_error("native audio byte budget exceeded before V8 allocation")
        })?;
        let storage = Box::new(ExposedChannelStorage {
            bytes: encode_samples(samples),
            _reservation: reservation,
        });
        let backing_store = v8::ArrayBuffer::new_backing_store_from_bytes(storage).make_shared();
        let array_buffer = v8::ArrayBuffer::with_backing_store(scope, &backing_store);
        let view = v8::Float32Array::new(scope, array_buffer, 0, sample_count)
            .ok_or_else(|| JsErrorBox::range_error("failed to create Float32Array"))?;
        self.state.get_mut(scope).exposed[channel] = Some(v8::TracedReference::new(scope, view));
        Ok(view)
    }

    #[fast]
    fn write_sample(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        channel: u32,
        sample: u32,
        value: f32,
    ) -> Result<(), JsErrorBox> {
        let (channel, sample) = check_sample(self.state.get(scope), channel, sample)?;
        let exposed = self.state.get_mut(scope).exposed[channel].take();
        let Some(exposed) = exposed else {
            self.state
                .get_mut(scope)
                .canonical
                .get_channel_data_mut(channel)[sample] = value;
            return Ok(());
        };
        let result = exposed
            .get(scope)
            .ok_or_else(|| JsErrorBox::generic("traced AudioBuffer channel view disappeared"))
            .and_then(|view| {
                if view_is_detached(scope, view) || !set_view_sample(&view, sample, value) {
                    Err(JsErrorBox::type_error(
                        "cannot write through a detached AudioBuffer channel view",
                    ))
                } else {
                    Ok(())
                }
            });
        self.state.get_mut(scope).exposed[channel] = Some(exposed);
        result
    }

    #[fast]
    fn read_sample(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        channel: u32,
        sample: u32,
    ) -> Result<f32, JsErrorBox> {
        let (channel, sample) = check_sample(self.state.get(scope), channel, sample)?;
        let exposed = self.state.get_mut(scope).exposed[channel].take();
        let Some(exposed) = exposed else {
            return Ok(self.state.get(scope).canonical.get_channel_data(channel)[sample]);
        };
        let result = exposed
            .get(scope)
            .ok_or_else(|| JsErrorBox::generic("traced AudioBuffer channel view disappeared"))
            .and_then(|view| {
                if view_is_detached(scope, view) {
                    Err(JsErrorBox::type_error(
                        "cannot read through a detached AudioBuffer channel view",
                    ))
                } else {
                    view_sample(&view, sample).ok_or_else(|| {
                        JsErrorBox::generic("AudioBuffer channel backing store is inconsistent")
                    })
                }
            });
        self.state.get_mut(scope).exposed[channel] = Some(exposed);
        result
    }

    #[fast]
    fn canonical_sample(
        &self,
        isolate: &mut v8::Isolate,
        channel: u32,
        sample: u32,
    ) -> Result<f32, JsErrorBox> {
        let state = self.state.get(isolate);
        let (channel, sample) = check_sample(state, channel, sample)?;
        Ok(state.canonical.get_channel_data(channel)[sample])
    }

    /// Acquire all exposed channels into canonical PCM and detach their old
    /// views. Returns false when an exposed view had already been externally
    /// detached; canonical PCM is not overwritten for that channel.
    #[fast]
    fn acquire(&self, scope: &mut v8::PinScope<'_, '_>) -> Result<bool, JsErrorBox> {
        let channel_count = self.state.get(scope).exposed.len();
        let expected_length = self.state.get(scope).canonical.length();
        let mut complete = true;
        for channel in 0..channel_count {
            let Some(exposed) = self.state.get_mut(scope).exposed[channel].take() else {
                continue;
            };
            let Some(view) = exposed.get(scope) else {
                self.state.get_mut(scope).exposed[channel] = Some(exposed);
                return Err(JsErrorBox::generic(
                    "traced AudioBuffer channel view disappeared",
                ));
            };
            let Some(array_buffer) = view.buffer(scope) else {
                complete = false;
                continue;
            };
            if array_buffer.was_detached() {
                complete = false;
                continue;
            }
            if view.length() != expected_length {
                self.state.get_mut(scope).exposed[channel] = Some(exposed);
                return Err(JsErrorBox::generic(
                    "AudioBuffer channel view length changed unexpectedly",
                ));
            }
            let Some(store) = view.get_backing_store() else {
                self.state.get_mut(scope).exposed[channel] = Some(exposed);
                return Err(JsErrorBox::generic(
                    "AudioBuffer channel backing store is inconsistent",
                ));
            };
            let byte_offset = view.byte_offset();
            if array_buffer.detach(None) != Some(true) {
                self.state.get_mut(scope).exposed[channel] = Some(exposed);
                return Err(JsErrorBox::generic(
                    "failed to detach acquired AudioBuffer channel view",
                ));
            }
            let destination = self
                .state
                .get_mut(scope)
                .canonical
                .get_channel_data_mut(channel);
            for (sample, destination) in destination.iter_mut().enumerate() {
                let offset = byte_offset + sample * BYTES_PER_SAMPLE;
                let source = &store[offset..offset + BYTES_PER_SAMPLE];
                *destination = f32::from_ne_bytes([
                    source[0].get(),
                    source[1].get(),
                    source[2].get(),
                    source[3].get(),
                ]);
            }
        }
        Ok(complete)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deno_core::{JsRuntime, RuntimeOptions};

    fn runtime(limit: usize) -> JsRuntime {
        let runtime = JsRuntime::new(RuntimeOptions {
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
            shared_array_buffer_store: Some(deno_core::SharedArrayBufferStore::default()),
            ..RuntimeOptions::default()
        });
        runtime
            .op_state()
            .borrow_mut()
            .put(Arc::new(NativeAudioByteBudget::new(limit)));
        runtime
    }

    fn budget(runtime: &JsRuntime) -> Arc<NativeAudioByteBudget> {
        Arc::clone(
            runtime
                .op_state()
                .borrow()
                .borrow::<Arc<NativeAudioByteBudget>>(),
        )
    }

    fn collect(runtime: &mut JsRuntime) {
        runtime
            .v8_isolate()
            .request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
    }

    #[test]
    fn exposure_acquisition_transfer_reservations_and_gc_are_coherent() {
        let mut runtime = runtime(256);
        let budget = budget(&runtime);

        runtime
            .execute_script(
                "native_audio_buffer.js",
                r#"
                  (() => {
                    const { NativeAudioBuffer } = Deno.core.ops;
                    const buffer = new NativeAudioBuffer(2, 4, 48000);
                    if (buffer.numberOfChannels !== 2 || buffer.length !== 4) {
                      throw new Error("native dimensions mismatch");
                    }
                    if (buffer.budgetUsed !== 32) throw new Error("canonical reservation mismatch");

                    buffer.writeSample(1, 0, 0.5);
                    if (buffer.readSample(1, 0) !== 0.5 || buffer.budgetUsed !== 32) {
                      throw new Error("unexposed access materialized a V8 channel");
                    }

                    const first = buffer.getChannelData(0);
                    if (first !== buffer.getChannelData(0)) throw new Error("same-object cache failed");
                    if (buffer.budgetUsed !== 48) throw new Error("exposed reservation mismatch");
                    first[1] = 0.25;
                    if (buffer.canonicalSample(0, 1) !== 0) {
                      throw new Error("exposed mutation reached canonical PCM before acquisition");
                    }
                    if (!buffer.acquire()) throw new Error("ordinary acquisition failed");
                    if (first.length !== 0 || buffer.canonicalSample(0, 1) !== 0.25) {
                      throw new Error("acquisition did not copy then detach");
                    }

                    const transferred = buffer.getChannelData(0);
                    transferred[1] = 0.75;
                    // Raw JsRuntime does not install deno_web's global facade.
                    // Exercise the same pinned V8 transfer primitives used by
                    // deno_web's structuredClone implementation directly.
                    const transferredArrayBuffers = [transferred.buffer];
                    const serialized = Deno.core.serialize(transferred.buffer, {
                      transferredArrayBuffers,
                    });
                    const moved = Deno.core.deserialize(serialized, {
                      transferredArrayBuffers,
                    });
                    if (transferred.length !== 0 || moved.byteLength !== 16) {
                      throw new Error(
                        `external ArrayBuffer transfer failed: source=${transferred.length}, moved=${moved.byteLength}`,
                      );
                    }
                    if (buffer.acquire()) {
                      throw new Error("already-detached acquisition reported success");
                    }
                    if (buffer.canonicalSample(0, 1) !== 0.25) {
                      throw new Error("detached view overwrote canonical PCM");
                    }
                    const fresh = buffer.getChannelData(0);
                    if (fresh === transferred || fresh[1] !== 0.25) {
                      throw new Error("fresh post-acquisition view mismatch");
                    }
                    fresh[1] = 0.875;
                    if (buffer.canonicalSample(0, 1) !== 0.25) {
                      throw new Error("post-acquisition mutation synchronized too early");
                    }
                    if (!buffer.acquire() || fresh.length !== 0 ||
                        buffer.canonicalSample(0, 1) !== 0.875) {
                      throw new Error("repeated acquisition timing mismatch");
                    }
                    const retained = buffer.getChannelData(0);
                    if (retained[1] !== 0.875) {
                      throw new Error("fresh view lost repeatedly acquired PCM");
                    }

                    globalThis.__nativeAudioBuffer = buffer;
                    globalThis.__transferredAudioBytes = moved;
                  })();
                "#,
            )
            .expect("AudioBuffer exposure model should execute");

        assert_eq!(budget.used(), 64, "canonical + fresh + transferred bytes");

        runtime
            .execute_script(
                "release_transferred_audio.js",
                "globalThis.__transferredAudioBytes = undefined;",
            )
            .unwrap();
        collect(&mut runtime);
        assert_eq!(
            budget.used(),
            48,
            "transferred backing-store lease releases on GC"
        );

        runtime
            .execute_script(
                "release_native_audio_buffer.js",
                "globalThis.__nativeAudioBuffer = undefined;",
            )
            .unwrap();
        collect(&mut runtime);
        assert_eq!(
            budget.used(),
            0,
            "canonical and cached-view leases release on GC"
        );
    }

    #[test]
    fn reservation_refuses_before_canonical_or_v8_allocation() {
        let mut runtime = runtime(31);
        let budget = budget(&runtime);
        runtime
            .execute_script(
                "native_audio_buffer_budget.js",
                r#"
                  const { NativeAudioBuffer } = Deno.core.ops;
                  let refused = false;
                  try { new NativeAudioBuffer(2, 8, 48000); } catch { refused = true; }
                  if (!refused) throw new Error("canonical budget refusal missing");
                  globalThis.__smallNativeAudioBuffer = new NativeAudioBuffer(1, 4, 48000);
                  refused = false;
                  try { globalThis.__smallNativeAudioBuffer.getChannelData(0); } catch { refused = true; }
                  if (!refused) throw new Error("exposed budget refusal missing");
                "#,
            )
            .expect("budget failures should be catchable JavaScript errors");
        assert_eq!(budget.used(), 16);
        runtime
            .execute_script(
                "release_small_native_audio_buffer.js",
                "globalThis.__smallNativeAudioBuffer = undefined;",
            )
            .unwrap();
        collect(&mut runtime);
        assert_eq!(budget.used(), 0);
    }
}
