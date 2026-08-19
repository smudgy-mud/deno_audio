use std::ffi::CStr;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use deno_core::cppgc::GcCell;
use deno_core::{op2, v8, GarbageCollected, OpState};
use deno_error::JsErrorBox;
use web_audio_api::{AudioBuffer, AudioBufferOptions, AudioBufferStorageLease, MAX_CHANNELS};

use crate::host::{
    release_atomic, AudioHost, AudioHostLimitError, AudioHostReservation, AudioHostResourceKind,
};

const BYTES_PER_SAMPLE: usize = size_of::<f32>();

pub(crate) struct NativeAudioByteBudget {
    limit: usize,
    used: AtomicUsize,
    host: Arc<AudioHost>,
    max_channels: Option<usize>,
    max_audio_buffer_samples: Option<usize>,
    max_sample_rate: Option<u32>,
}

impl NativeAudioByteBudget {
    #[cfg(test)]
    pub(crate) fn new(limit: usize, host: Arc<AudioHost>) -> Self {
        Self::with_limits(limit, host, None, None, None)
    }

    pub(crate) fn with_limits(
        limit: usize,
        host: Arc<AudioHost>,
        max_channels: Option<usize>,
        max_audio_buffer_samples: Option<usize>,
        max_sample_rate: Option<u32>,
    ) -> Self {
        Self {
            limit,
            used: AtomicUsize::new(0),
            host,
            max_channels,
            max_audio_buffer_samples,
            max_sample_rate,
        }
    }

    pub(crate) fn validate_audio_buffer(
        &self,
        channels: usize,
        length: usize,
        sample_rate: f64,
    ) -> Result<(), JsErrorBox> {
        if self.max_channels.is_some_and(|limit| channels > limit) {
            return Err(JsErrorBox::range_error(
                "configured AudioBuffer channel limit exceeded",
            ));
        }
        let samples = channels
            .checked_mul(length)
            .ok_or_else(|| JsErrorBox::range_error("AudioBuffer sample count overflow"))?;
        if self
            .max_audio_buffer_samples
            .is_some_and(|limit| samples > limit)
        {
            return Err(JsErrorBox::range_error(
                "configured AudioBuffer logical sample limit exceeded",
            ));
        }
        if self
            .max_sample_rate
            .is_some_and(|limit| sample_rate > f64::from(limit))
        {
            return Err(JsErrorBox::range_error(
                "configured AudioBuffer sample-rate limit exceeded",
            ));
        }
        Ok(())
    }

    pub(crate) fn reserve(
        self: &Arc<Self>,
        bytes: usize,
    ) -> Result<NativeAudioByteReservation, NativeAudioByteLimitError> {
        let mut used = self.used.load(Ordering::Acquire);
        loop {
            let Some(next) = used.checked_add(bytes) else {
                return Err(NativeAudioByteLimitError::Local {
                    current: used,
                    requested: bytes,
                    limit: self.limit,
                });
            };
            if next > self.limit {
                return Err(NativeAudioByteLimitError::Local {
                    current: used,
                    requested: bytes,
                    limit: self.limit,
                });
            }
            match self
                .used
                .compare_exchange_weak(used, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    let local = LocalNativeAudioByteReservation {
                        budget: Arc::clone(self),
                        bytes,
                    };
                    let host = match self
                        .host
                        .try_reserve(AudioHostResourceKind::LiveAudioBytes, bytes)
                    {
                        Ok(host) => host,
                        Err(error) => {
                            drop(local);
                            return Err(NativeAudioByteLimitError::Host(error));
                        }
                    };
                    return Ok(NativeAudioByteReservation { local, host });
                }
                Err(observed) => used = observed,
            }
        }
    }

    pub(crate) fn used(&self) -> usize {
        self.used.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) enum NativeAudioByteLimitError {
    Local {
        current: usize,
        requested: usize,
        limit: usize,
    },
    Host(AudioHostLimitError),
}

impl fmt::Display for NativeAudioByteLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local {
                current,
                requested,
                limit,
            } => write!(
                formatter,
                "per-isolate live audio byte limit {limit} exceeded by reservation of {requested} at {current}"
            ),
            Self::Host(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for NativeAudioByteLimitError {}

struct LocalNativeAudioByteReservation {
    budget: Arc<NativeAudioByteBudget>,
    bytes: usize,
}

impl Drop for LocalNativeAudioByteReservation {
    fn drop(&mut self) {
        release_atomic(&self.budget.used, self.bytes);
    }
}

pub(crate) struct NativeAudioByteReservation {
    local: LocalNativeAudioByteReservation,
    host: AudioHostReservation,
}

impl NativeAudioByteReservation {
    pub(crate) fn shrink_to(&mut self, bytes: usize) {
        if bytes > self.local.bytes {
            return;
        }
        let released = self.local.bytes - bytes;
        self.local.bytes = bytes;
        release_atomic(&self.local.budget.used, released);
        self.host.shrink_to(bytes);
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

/// Private native storage behind the public JavaScript `AudioBuffer` facade.
/// Canonical PCM is Rust-owned; only explicitly exposed channels gain traced
/// V8 copies.
pub(crate) struct NativeAudioBuffer {
    state: GcCell<NativeAudioBufferState>,
    budget: Arc<NativeAudioByteBudget>,
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

pub(crate) fn checked_pcm_bytes(channels: usize, length: usize) -> Option<usize> {
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

fn copy_view_samples(
    source: &v8::Float32Array,
    source_offset: usize,
    destination: &v8::Float32Array,
    destination_offset: usize,
    count: usize,
) -> Result<(), JsErrorBox> {
    if count == 0 {
        return Ok(());
    }

    let source_store = source
        .get_backing_store()
        .ok_or_else(|| JsErrorBox::type_error("source Float32Array is detached"))?;
    let destination_store = destination
        .get_backing_store()
        .ok_or_else(|| JsErrorBox::type_error("destination Float32Array is detached"))?;
    let source_sample_offset = source_offset
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or_else(|| JsErrorBox::range_error("source Float32Array offset overflow"))?;
    let source_start = source
        .byte_offset()
        .checked_add(source_sample_offset)
        .ok_or_else(|| JsErrorBox::range_error("source Float32Array offset overflow"))?;
    let destination_sample_offset = destination_offset
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or_else(|| JsErrorBox::range_error("destination Float32Array offset overflow"))?;
    let destination_start = destination
        .byte_offset()
        .checked_add(destination_sample_offset)
        .ok_or_else(|| JsErrorBox::range_error("destination Float32Array offset overflow"))?;
    let byte_count = count
        .checked_mul(BYTES_PER_SAMPLE)
        .ok_or_else(|| JsErrorBox::range_error("Float32Array copy length overflow"))?;
    let source_end = source_start
        .checked_add(byte_count)
        .ok_or_else(|| JsErrorBox::range_error("source Float32Array range overflow"))?;
    let destination_end = destination_start
        .checked_add(byte_count)
        .ok_or_else(|| JsErrorBox::range_error("destination Float32Array range overflow"))?;
    let same_store = source_store.data() == destination_store.data();
    let copy_backwards = same_store
        && destination_start > source_start
        && destination_start < source_end
        && source_start < destination_end;

    let copy_one = |index: usize| {
        let value = view_sample(source, source_offset + index)
            .ok_or_else(|| JsErrorBox::type_error("source Float32Array is detached"))?;
        if !set_view_sample(destination, destination_offset + index, value) {
            return Err(JsErrorBox::type_error(
                "destination Float32Array is detached",
            ));
        }
        Ok(())
    };

    if copy_backwards {
        for index in (0..count).rev() {
            copy_one(index)?;
        }
    } else {
        for index in 0..count {
            copy_one(index)?;
        }
    }
    Ok(())
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

impl NativeAudioBuffer {
    pub(crate) fn from_canonical(
        mut canonical: AudioBuffer,
        budget: Arc<NativeAudioByteBudget>,
        canonical_reservation: NativeAudioByteReservation,
    ) -> Self {
        canonical.attach_storage_lease(AudioBufferStorageLease::new(canonical_reservation));
        let channel_count = canonical.number_of_channels();
        Self {
            state: GcCell::new(NativeAudioBufferState {
                canonical,
                exposed: (0..channel_count).map(|_| None).collect(),
            }),
            budget,
        }
    }

    pub(crate) fn from_budgeted_decode(
        canonical: AudioBuffer,
        budget: Arc<NativeAudioByteBudget>,
    ) -> Self {
        let channel_count = canonical.number_of_channels();
        Self {
            state: GcCell::new(NativeAudioBufferState {
                canonical,
                exposed: (0..channel_count).map(|_| None).collect(),
            }),
            budget,
        }
    }
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
        budget.validate_audio_buffer(options.number_of_channels, options.length, sample_rate)?;
        let canonical_reservation = budget.reserve(canonical_bytes).map_err(|error| {
            JsErrorBox::range_error(format!(
                "native audio byte budget exceeded before allocation: {error}"
            ))
        })?;
        let channel_count = options.number_of_channels;
        let mut canonical = AudioBuffer::new(options);
        canonical.attach_storage_lease(AudioBufferStorageLease::new(canonical_reservation));

        Ok(NativeAudioBuffer {
            state: GcCell::new(NativeAudioBufferState {
                canonical,
                exposed: (0..channel_count).map(|_| None).collect(),
            }),
            budget,
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
    fn sample_rate(&self, isolate: &mut v8::Isolate) -> f32 {
        self.state.get(isolate).canonical.sample_rate()
    }

    #[getter]
    fn duration(&self, isolate: &mut v8::Isolate) -> f64 {
        self.state.get(isolate).canonical.duration()
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
        let reservation = self.budget.reserve(byte_length).map_err(|error| {
            JsErrorBox::range_error(format!(
                "native audio byte budget exceeded before V8 allocation: {error}"
            ))
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
    fn copy_from_channel(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        destination: v8::Local<'_, v8::Float32Array>,
        channel: u32,
        buffer_offset: u32,
    ) -> Result<(), JsErrorBox> {
        let channel = check_channel(self.state.get(scope), channel)?;
        let buffer_offset = buffer_offset as usize;
        let available = self
            .state
            .get(scope)
            .canonical
            .length()
            .saturating_sub(buffer_offset);
        let count = available.min(destination.length());
        if count == 0 {
            return Ok(());
        }

        let exposed = self.state.get_mut(scope).exposed[channel].take();
        let Some(exposed) = exposed else {
            let source = self.state.get(scope).canonical.get_channel_data(channel);
            for (index, value) in source[buffer_offset..buffer_offset + count]
                .iter()
                .copied()
                .enumerate()
            {
                if !set_view_sample(&destination, index, value) {
                    return Err(JsErrorBox::type_error(
                        "destination Float32Array is detached",
                    ));
                }
            }
            return Ok(());
        };

        let result = exposed
            .get(scope)
            .ok_or_else(|| JsErrorBox::generic("traced AudioBuffer channel view disappeared"))
            .and_then(|source| copy_view_samples(&source, buffer_offset, &destination, 0, count));
        self.state.get_mut(scope).exposed[channel] = Some(exposed);
        result
    }

    #[fast]
    fn copy_to_channel(
        &self,
        scope: &mut v8::PinScope<'_, '_>,
        source: v8::Local<'_, v8::Float32Array>,
        channel: u32,
        buffer_offset: u32,
    ) -> Result<(), JsErrorBox> {
        let channel = check_channel(self.state.get(scope), channel)?;
        let buffer_offset = buffer_offset as usize;
        let available = self
            .state
            .get(scope)
            .canonical
            .length()
            .saturating_sub(buffer_offset);
        let count = available.min(source.length());
        if count == 0 {
            return Ok(());
        }

        let exposed = self.state.get_mut(scope).exposed[channel].take();
        let Some(exposed) = exposed else {
            let destination = self
                .state
                .get_mut(scope)
                .canonical
                .get_channel_data_mut(channel);
            for (index, destination) in destination[buffer_offset..buffer_offset + count]
                .iter_mut()
                .enumerate()
            {
                *destination = view_sample(&source, index)
                    .ok_or_else(|| JsErrorBox::type_error("source Float32Array is detached"))?;
            }
            return Ok(());
        };

        let result = exposed
            .get(scope)
            .ok_or_else(|| JsErrorBox::generic("traced AudioBuffer channel view disappeared"))
            .and_then(|destination| {
                copy_view_samples(&source, 0, &destination, buffer_offset, count)
            });
        self.state.get_mut(scope).exposed[channel] = Some(exposed);
        result
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
                crate::deno_audio::init(crate::AudioExtensionOptions::default()),
            ],
            unsafe_expose_natives_and_gc: true,
            shared_array_buffer_store: Some(deno_core::SharedArrayBufferStore::default()),
            ..RuntimeOptions::default()
        });
        runtime
            .op_state()
            .borrow_mut()
            .put(Arc::new(NativeAudioByteBudget::new(
                limit,
                Arc::new(crate::AudioHost::default()),
            )));
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

    fn collect_until(runtime: &mut JsRuntime, budget: &NativeAudioByteBudget, expected: usize) {
        for _ in 0..16 {
            runtime
                .v8_isolate()
                .request_garbage_collection_for_testing(v8::GarbageCollectionType::Full);
            if budget.used() == expected {
                return;
            }
            std::thread::yield_now();
        }
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
        collect_until(&mut runtime, &budget, 48);
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
        collect_until(&mut runtime, &budget, 0);
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
        collect_until(&mut runtime, &budget, 0);
        assert_eq!(budget.used(), 0);
    }
}
