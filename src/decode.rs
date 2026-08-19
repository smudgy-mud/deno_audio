use std::error::Error;
use std::ffi::CStr;
use std::fmt;
use std::io::Cursor;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use deno_core::serde::Serialize;
use deno_core::{op2, v8, GarbageCollected, JsBuffer, OpState};
use deno_error::JsErrorBox;
use web_audio_api::context::OfflineAudioContext;
use web_audio_api::{
    AudioBuffer, BaseAudioContextDecodeBudgetExt, DecodeBudget, DecodeBudgetError,
    DecodeBudgetKind, DecodeBudgetRequest, DecodeBudgetReservation,
};

use crate::buffer::{NativeAudioBuffer, NativeAudioByteBudget, NativeAudioByteReservation};
use crate::host::{release_atomic, AudioHostReservation, AudioHostResourceKind};
use crate::offline::{blocking_outcome, resolve_blocking, validate_sample_rate};
use crate::online::{AudioRuntimeState, IsolateReservation};

#[derive(Serialize)]
#[serde(crate = "deno_core::serde", rename_all = "camelCase")]
pub(crate) struct Phase0DecodeSummary {
    number_of_channels: u32,
    length: u32,
    sample_rate: f64,
    off_isolate_thread: bool,
}

#[derive(Debug)]
struct DecodeLimitError {
    kind: DecodeBudgetKind,
    current: usize,
    requested: usize,
    limit: usize,
}

impl fmt::Display for DecodeLimitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "decode {:?} limit {} exceeded by reservation of {} at {}",
            self.kind, self.limit, self.requested, self.current
        )
    }
}

impl Error for DecodeLimitError {}

struct DecodeDimensionReservation {
    counter: Arc<AtomicUsize>,
    amount: usize,
}

impl DecodeBudgetReservation for DecodeDimensionReservation {
    fn shrink_to(&mut self, amount: u64) {
        let Ok(amount) = usize::try_from(amount) else {
            return;
        };
        if amount > self.amount {
            return;
        }
        let released = self.amount - amount;
        self.amount = amount;
        release_atomic(&self.counter, released);
    }
}

impl Drop for DecodeDimensionReservation {
    fn drop(&mut self) {
        release_atomic(&self.counter, self.amount);
    }
}

struct DecodeByteReservation {
    reservation: NativeAudioByteReservation,
    amount: usize,
}

impl DecodeBudgetReservation for DecodeByteReservation {
    fn shrink_to(&mut self, amount: u64) {
        let Ok(amount) = usize::try_from(amount) else {
            return;
        };
        if amount > self.amount {
            return;
        }
        self.amount = amount;
        self.reservation.shrink_to(amount);
    }
}

struct NativeDecodeBudget {
    byte_budget: Arc<NativeAudioByteBudget>,
    channel_limit: Option<usize>,
    frame_limit: Option<usize>,
    sample_limit: Option<usize>,
    channels: Arc<AtomicUsize>,
    frames: Arc<AtomicUsize>,
    samples: Arc<AtomicUsize>,
}

impl NativeDecodeBudget {
    fn new(runtime: &AudioRuntimeState, byte_budget: Arc<NativeAudioByteBudget>) -> Self {
        Self {
            byte_budget,
            channel_limit: runtime.limits().channels(),
            frame_limit: runtime.limits().decoded_frames(),
            sample_limit: runtime.limits().decoded_samples(),
            channels: Arc::new(AtomicUsize::new(0)),
            frames: Arc::new(AtomicUsize::new(0)),
            samples: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn dimension(&self, kind: DecodeBudgetKind) -> Option<(&Arc<AtomicUsize>, Option<usize>)> {
        match kind {
            DecodeBudgetKind::DecodedChannels => Some((&self.channels, self.channel_limit)),
            DecodeBudgetKind::DecodedFrames => Some((&self.frames, self.frame_limit)),
            DecodeBudgetKind::DecodedSamples => Some((&self.samples, self.sample_limit)),
            _ => None,
        }
    }

    fn limit_error(
        kind: DecodeBudgetKind,
        current: usize,
        requested: usize,
        limit: Option<usize>,
    ) -> DecodeBudgetError {
        Box::new(DecodeLimitError {
            kind,
            current,
            requested,
            limit: limit.unwrap_or(usize::MAX),
        })
    }
}

impl DecodeBudget for NativeDecodeBudget {
    fn try_reserve(
        &self,
        request: DecodeBudgetRequest,
    ) -> Result<Box<dyn DecodeBudgetReservation>, DecodeBudgetError> {
        let amount = usize::try_from(request.amount())
            .map_err(|_| Self::limit_error(request.kind(), 0, usize::MAX, Some(usize::MAX)))?;

        if let Some((counter, limit)) = self.dimension(request.kind()) {
            let mut current = counter.load(Ordering::Acquire);
            loop {
                let Some(next) = current.checked_add(amount) else {
                    return Err(Self::limit_error(request.kind(), current, amount, limit));
                };
                if limit.is_some_and(|limit| next > limit) {
                    return Err(Self::limit_error(request.kind(), current, amount, limit));
                }
                match counter.compare_exchange_weak(
                    current,
                    next,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => {
                        return Ok(Box::new(DecodeDimensionReservation {
                            counter: Arc::clone(counter),
                            amount,
                        }));
                    }
                    Err(observed) => current = observed,
                }
            }
        }

        match request.kind() {
            DecodeBudgetKind::CanonicalPcmBytes | DecodeBudgetKind::ResamplePcmBytes => self
                .byte_budget
                .reserve(amount)
                .map(|reservation| {
                    Box::new(DecodeByteReservation {
                        reservation,
                        amount,
                    }) as Box<dyn DecodeBudgetReservation>
                })
                .map_err(|error| Box::new(error) as DecodeBudgetError),
            _ => Err(Self::limit_error(request.kind(), 0, amount, Some(0))),
        }
    }
}

struct DecodeRequestPayload {
    encoded: Vec<u8>,
    sample_rate: f32,
    isolate_thread: thread::ThreadId,
    budget: Arc<dyn DecodeBudget>,
    byte_budget: Arc<NativeAudioByteBudget>,
    _encoded_bytes: NativeAudioByteReservation,
    _isolate_job: IsolateReservation,
    _host_job: AudioHostReservation,
}

/// Private single-use decode proof. Input bytes and all outer reservations are
/// acquired synchronously before the blocking codec task can be spawned.
pub(crate) struct NativePhase0DecodeRequest {
    payload: Mutex<Option<DecodeRequestPayload>>,
}

// SAFETY: this object contains no V8 or CppGC pointers. Its byte copy and
// reservations are ordinary Send native ownership behind a mutex.
unsafe impl GarbageCollected for NativePhase0DecodeRequest {
    fn trace(&self, _visitor: &mut v8::cppgc::Visitor) {}

    fn get_name(&self) -> &'static CStr {
        c"NativePhase0DecodeRequest"
    }
}

#[op2]
impl NativePhase0DecodeRequest {
    #[constructor]
    #[cppgc]
    fn new(
        state: &mut OpState,
        #[buffer] encoded: JsBuffer,
        sample_rate: f64,
    ) -> Result<NativePhase0DecodeRequest, JsErrorBox> {
        let runtime = Arc::clone(state.borrow::<Arc<AudioRuntimeState>>());
        let validated_sample_rate = validate_sample_rate(sample_rate)?;
        if runtime
            .limits()
            .sample_rate()
            .is_some_and(|limit| sample_rate > f64::from(limit))
        {
            return Err(JsErrorBox::range_error(
                "configured decode sample-rate limit exceeded",
            ));
        }
        let sample_rate = validated_sample_rate;
        let encoded = encoded.into_parts();
        let byte_budget = Arc::clone(state.borrow::<Arc<NativeAudioByteBudget>>());
        let encoded_len = encoded.len();
        if runtime
            .limits()
            .encoded_bytes()
            .is_some_and(|limit| encoded_len > limit)
        {
            return Err(JsErrorBox::range_error("encoded audio byte limit exceeded"));
        }

        let isolate_job = runtime.try_reserve_decode_job()?;
        let host_job = runtime
            .host()
            .try_reserve(AudioHostResourceKind::DecodeJobs, 1)
            .map_err(|error| {
                JsErrorBox::range_error(format!("AudioHost decode-job limit exceeded: {error}"))
            })?;
        let encoded_bytes = byte_budget.reserve(encoded_len).map_err(|error| {
            JsErrorBox::range_error(format!(
                "live audio byte limit rejected encoded decode input: {error}"
            ))
        })?;
        let mut owned = Vec::new();
        owned.try_reserve_exact(encoded_len).map_err(|error| {
            JsErrorBox::range_error(format!("encoded audio allocation failed: {error}"))
        })?;
        owned.extend_from_slice(encoded.as_ref());

        let decode_budget = Arc::new(NativeDecodeBudget::new(&runtime, Arc::clone(&byte_budget)));
        Ok(NativePhase0DecodeRequest {
            payload: Mutex::new(Some(DecodeRequestPayload {
                encoded: owned,
                sample_rate,
                isolate_thread: thread::current().id(),
                budget: decode_budget,
                byte_budget,
                _encoded_bytes: encoded_bytes,
                _isolate_job: isolate_job,
                _host_job: host_job,
            })),
        })
    }
}

impl NativePhase0DecodeRequest {
    fn take(&self) -> Result<DecodeRequestPayload, JsErrorBox> {
        self.payload
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| JsErrorBox::generic("decode request has already been consumed"))
    }
}

struct CompletedDecode {
    decoded: AudioBuffer,
    byte_budget: Arc<NativeAudioByteBudget>,
    off_isolate_thread: bool,
}

async fn decode_request(
    request: &NativePhase0DecodeRequest,
) -> Result<CompletedDecode, JsErrorBox> {
    let payload = request.take()?;
    resolve_blocking("audio decode", move || {
        blocking_outcome(|| {
            let DecodeRequestPayload {
                encoded,
                sample_rate,
                isolate_thread,
                budget,
                byte_budget,
                _encoded_bytes,
                _isolate_job,
                _host_job,
            } = payload;
            let context = OfflineAudioContext::new(1, 1, sample_rate);
            let decoded = context
                .decode_audio_data_sync_with_budget(Cursor::new(encoded), budget)
                .map_err(|error| format!("audio decode failed: {error}"))?;
            Ok(CompletedDecode {
                decoded,
                byte_budget,
                off_isolate_thread: thread::current().id() != isolate_thread,
            })
        })
    })
    .await
}

#[op2]
#[serde]
pub(crate) async fn op_phase0_decode_off_thread(
    #[cppgc] request: &NativePhase0DecodeRequest,
) -> Result<Phase0DecodeSummary, JsErrorBox> {
    let completed = decode_request(request).await?;
    Ok(Phase0DecodeSummary {
        number_of_channels: u32::try_from(completed.decoded.number_of_channels()).map_err(
            |_| JsErrorBox::range_error("decoded channel count exceeds the binding range"),
        )?,
        length: u32::try_from(completed.decoded.length()).map_err(|_| {
            JsErrorBox::range_error("decoded frame count exceeds the binding range")
        })?,
        sample_rate: f64::from(completed.decoded.sample_rate()),
        off_isolate_thread: completed.off_isolate_thread,
    })
}

/// Public `BaseAudioContext.decodeAudioData` completion. The decoded engine buffer carries the
/// decoder's exact dimension and PCM reservations; the wrapper budget is retained separately for
/// any later V8 channel exposure.
#[op2]
#[cppgc]
pub(crate) async fn op_decode_audio_data(
    #[cppgc] request: &NativePhase0DecodeRequest,
) -> Result<NativeAudioBuffer, JsErrorBox> {
    let completed = decode_request(request).await?;
    Ok(NativeAudioBuffer::from_budgeted_decode(
        completed.decoded,
        completed.byte_budget,
    ))
}
