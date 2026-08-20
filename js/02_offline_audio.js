// Copyright 2026 the deno_audio authors. MIT license.

import {
  core,
  domException,
  primordials,
  webidl,
} from "ext:deno_audio/00_webidl.js";
import {
  AudioBuffer,
  createAudioBuffer,
  getNativeAudioBuffer,
} from "ext:deno_audio/01_audio_buffer.js";

const { NativeOfflineAudioContext, NativePhase0OnlineContext } = core.ops;
const { markNotSerializable } = core.loadExtScript(
  "ext:deno_web/13_message_port.js",
);
const {
  defineEventHandler,
  Event,
  EventTarget,
  EventTargetPrototype,
  reportException,
} = core.loadExtScript("ext:deno_web/02_event.js");
const {
  ArrayBufferPrototypeTransferToFixedLength,
  ArrayPrototypePush,
  Float32Array,
  MapPrototypeDelete,
  MapPrototypeGet,
  MapPrototypeSet,
  NumberIsFinite,
  ObjectPrototypeIsPrototypeOf,
  PromisePrototypeThen,
  PromiseReject,
  RangeError,
  RangeErrorPrototype,
  ReflectApply,
  SafeMap,
  SafeSet,
  SafeSetIterator,
  SafeWeakMap,
  SafeWeakRef,
  SetPrototypeAdd,
  SetPrototypeClear,
  SetPrototypeDelete,
  String,
  StringPrototypeIncludes,
  Symbol,
  SymbolIterator,
  TypeError,
  TypeErrorPrototype,
  Uint8Array,
  WeakMapPrototypeGet,
  WeakMapPrototypeSet,
  WeakRefPrototypeDeref,
} = primordials;

const MAX_AUTOMATION_CURVE_SAMPLES = 1_048_576;
const constructNode = Symbol("[[constructAudioNode]]");
const constructContext = Symbol("[[constructBaseAudioContext]]");
const nativeContexts = new SafeWeakMap();
const nativeNodes = new SafeWeakMap();
const nativeParams = new SafeWeakMap();
const completionBuffers = new SafeWeakMap();

webidl.converters.AudioContextState = webidl.createEnumConverter(
  "AudioContextState",
  ["suspended", "running", "closed"],
);
webidl.converters.ChannelCountMode = webidl.createEnumConverter(
  "ChannelCountMode",
  ["max", "clamped-max", "explicit"],
);
webidl.converters.ChannelInterpretation = webidl.createEnumConverter(
  "ChannelInterpretation",
  ["speakers", "discrete"],
);
webidl.converters.AutomationRate = webidl.createEnumConverter(
  "AutomationRate",
  ["a-rate", "k-rate"],
);
webidl.converters.OscillatorType = webidl.createEnumConverter(
  "OscillatorType",
  ["sine", "square", "sawtooth", "triangle", "custom"],
);
webidl.converters.AudioContextLatencyCategory = webidl.createEnumConverter(
  "AudioContextLatencyCategory",
  ["balanced", "interactive", "playback"],
);
webidl.converters.AudioSinkType = webidl.createEnumConverter(
  "AudioSinkType",
  ["none"],
);

webidl.converters.AudioSinkOptions = webidl.createDictionaryConverter(
  "AudioSinkOptions",
  [{
    key: "type",
    converter: webidl.converters.AudioSinkType,
    required: true,
  }],
);

function convertLatencyHint(value, prefix, context) {
  if (typeof value === "number") {
    value = webidl.converters.double(value, prefix, context);
    if (!NumberIsFinite(value) || value <= 0) {
      throw new RangeError("A numeric latencyHint must be finite and positive.");
    }
    return value;
  }
  return webidl.converters.AudioContextLatencyCategory(
    value,
    prefix,
    context,
  );
}

function convertSinkId(value, prefix, context) {
  if (value !== null && typeof value === "object") {
    webidl.converters.AudioSinkOptions(value, prefix, context);
    return "none";
  }
  return webidl.converters.DOMString(value, prefix, context);
}

webidl.converters.AudioContextOptions = webidl.createDictionaryConverter(
  "AudioContextOptions",
  [
    {
      key: "latencyHint",
      converter: convertLatencyHint,
      defaultValue: "interactive",
    },
    {
      key: "sampleRate",
      converter: webidl.converters.float,
    },
    {
      key: "sinkId",
      converter: convertSinkId,
      defaultValue: "",
    },
  ],
);

const audioNodeOptionsMembers = [
  {
    key: "channelCount",
    converter: webidl.converters["unsigned long"],
    defaultValue: 2,
  },
  {
    key: "channelCountMode",
    converter: webidl.converters.ChannelCountMode,
    defaultValue: "max",
  },
  {
    key: "channelInterpretation",
    converter: webidl.converters.ChannelInterpretation,
    defaultValue: "speakers",
  },
];

webidl.converters.AudioNodeOptions = webidl.createDictionaryConverter(
  "AudioNodeOptions",
  audioNodeOptionsMembers,
);
webidl.converters.GainOptions = webidl.createDictionaryConverter(
  "GainOptions",
  audioNodeOptionsMembers,
  [{
    key: "gain",
    converter: webidl.converters.float,
    defaultValue: 1,
  }],
);
webidl.converters.OscillatorOptions = webidl.createDictionaryConverter(
  "OscillatorOptions",
  audioNodeOptionsMembers,
  [
    {
      key: "detune",
      converter: webidl.converters.float,
      defaultValue: 0,
    },
    {
      key: "frequency",
      converter: webidl.converters.float,
      defaultValue: 440,
    },
    {
      key: "periodicWave",
      converter: webidl.converters.object,
    },
    {
      key: "type",
      converter: webidl.converters.OscillatorType,
      defaultValue: "sine",
    },
  ],
);
function convertNullableAudioBuffer(value, prefix, context) {
  if (value === null || value === undefined) return null;
  return webidl.converters.AudioBuffer(value, prefix, context);
}
webidl.converters.AudioBufferSourceOptions = webidl.createDictionaryConverter(
  "AudioBufferSourceOptions",
  audioNodeOptionsMembers,
  [
    {
      key: "buffer",
      converter: convertNullableAudioBuffer,
      defaultValue: null,
    },
    {
      key: "detune",
      converter: webidl.converters.float,
      defaultValue: 0,
    },
    {
      key: "loop",
      converter: webidl.converters.boolean,
      defaultValue: false,
    },
    {
      key: "loopEnd",
      converter: webidl.converters.double,
      defaultValue: 0,
    },
    {
      key: "loopStart",
      converter: webidl.converters.double,
      defaultValue: 0,
    },
    {
      key: "playbackRate",
      converter: webidl.converters.float,
      defaultValue: 1,
    },
  ],
);
webidl.converters.OfflineAudioContextOptions =
  webidl.createDictionaryConverter("OfflineAudioContextOptions", [
    {
      key: "length",
      converter: webidl.converters["unsigned long"],
      required: true,
    },
    {
      key: "numberOfChannels",
      converter: webidl.converters["unsigned long"],
      defaultValue: 1,
    },
    {
      key: "sampleRate",
      converter: webidl.converters.float,
      required: true,
    },
  ]);
function closeIterator(iterator) {
  const returnMethod = iterator.return;
  if (typeof returnMethod === "function") {
    ReflectApply(returnMethod, iterator, []);
  }
}

function boundedFloatSequence(value, prefix, context) {
  if (value === null || value === undefined) {
    throw new TypeError(`${prefix}: ${context} is not iterable.`);
  }
  const iteratorMethod = value[SymbolIterator];
  if (typeof iteratorMethod !== "function") {
    throw new TypeError(`${prefix}: ${context} is not iterable.`);
  }
  const iterator = ReflectApply(iteratorMethod, value, []);
  if (
    iterator === null ||
    (typeof iterator !== "object" && typeof iterator !== "function")
  ) {
    throw new TypeError(`${prefix}: ${context} returned an invalid iterator.`);
  }
  const next = iterator.next;
  if (typeof next !== "function") {
    throw new TypeError(`${prefix}: ${context} returned an invalid iterator.`);
  }
  const converted = [];
  while (true) {
    let item;
    try {
      item = ReflectApply(next, iterator, []);
      if (
        item === null ||
        (typeof item !== "object" && typeof item !== "function")
      ) {
        throw new TypeError("Iterator result is not an object.");
      }
      if (item.done) return converted;
      if (converted.length === MAX_AUTOMATION_CURVE_SAMPLES) {
        throw domException(
          "The AudioParam value curve exceeds the fixed sample limit.",
          "QuotaExceededError",
        );
      }
      ArrayPrototypePush(
        converted,
        webidl.converters.float(
          item.value,
          prefix,
          `${context}, index ${converted.length}`,
        ),
      );
    } catch (error) {
      closeIterator(iterator);
      throw error;
    }
  }
}

function contextRecord(value) {
  webidl.assertBranded(value, BaseAudioContextPrototype);
  return WeakMapPrototypeGet(nativeContexts, value);
}

function nodeRecord(value) {
  webidl.assertBranded(value, AudioNodePrototype);
  return WeakMapPrototypeGet(nativeNodes, value);
}

function paramRecord(value) {
  webidl.assertBranded(value, AudioParamPrototype);
  return WeakMapPrototypeGet(nativeParams, value);
}

function messageOf(error) {
  try {
    return String(error?.message ?? error);
  } catch {
    return "native Web Audio operation failed";
  }
}

function rethrowNativeError(error, operation) {
  if (ObjectPrototypeIsPrototypeOf(RangeErrorPrototype, error)) {
    throw domException(
      `${operation} exceeded a configured audio resource limit.`,
      "QuotaExceededError",
    );
  }
  if (ObjectPrototypeIsPrototypeOf(TypeErrorPrototype, error)) throw error;
  const message = messageOf(error);
  const names = [
    "IndexSizeError",
    "InvalidAccessError",
    "InvalidStateError",
    "NotAllowedError",
    "NotFoundError",
    "NotSupportedError",
    "QuotaExceededError",
  ];
  for (let i = 0; i < names.length; ++i) {
    if (StringPrototypeIncludes(message, names[i])) {
      throw domException(message, names[i]);
    }
  }
  throw domException(`${operation} failed: ${message}`, "InvalidStateError");
}

function callNative(operation, callback) {
  try {
    return callback();
  } catch (error) {
    rethrowNativeError(error, operation);
  }
}

function channelCountModeCode(value) {
  switch (value) {
    case "max":
      return 0;
    case "clamped-max":
      return 1;
    default:
      return 2;
  }
}

function channelInterpretationCode(value) {
  return value === "speakers" ? 0 : 1;
}

function oscillatorTypeCode(value) {
  switch (value) {
    case "sine":
      return 0;
    case "square":
      return 1;
    case "sawtooth":
      return 2;
    case "triangle":
      return 3;
    default:
      return 4;
  }
}

function oscillatorTypeName(value) {
  switch (value) {
    case 0:
      return "sine";
    case 1:
      return "square";
    case 2:
      return "sawtooth";
    case 3:
      return "triangle";
    default:
      return "custom";
  }
}

function validateNodeOptions(options, prefix) {
  if (options.channelCount === 0 || options.channelCount > 32) {
    throw domException(
      `${prefix}: channelCount must be in the range 1 through 32.`,
      "NotSupportedError",
    );
  }
}

function dispatch(target, event) {
  return ReflectApply(EventTargetPrototype.dispatchEvent, target, [event]);
}

function initializeNode(value, context, native, inputCount, outputCount) {
  WeakMapPrototypeSet(nativeNodes, value, {
    __proto__: null,
    context,
    inputCount,
    native,
    outputCount,
  });
}

function createParam(context, native) {
  const param = webidl.createBranded(AudioParam);
  WeakMapPrototypeSet(nativeParams, param, {
    __proto__: null,
    context,
    native,
  });
  return param;
}

function createDestination(context, native) {
  const destination = new AudioDestinationNode(constructNode);
  initializeNode(destination, context, native, 1, 0);
  return destination;
}

function contextStateName(value) {
  switch (value) {
    case 0:
      return "suspended";
    case 1:
      return "running";
    default:
      return "closed";
  }
}

function updateOnlineState(context, record) {
  let state;
  try {
    state = contextStateName(record.native.contextState);
  } catch {
    state = "closed";
  }
  if (state !== record.state) {
    record.state = state;
    dispatch(context, new Event("statechange"));
  }
}

function dispatchEndedIds(record, ids) {
  for (let i = 0; i < ids.length; ++i) {
    const source = MapPrototypeGet(record.sourceById, ids[i]);
    if (source === undefined) continue;
    MapPrototypeDelete(record.sourceById, ids[i]);
    SetPrototypeDelete(record.sources, source);
    const sourceRecord = nodeRecord(source);
    if (!sourceRecord.ended) {
      sourceRecord.ended = true;
      dispatch(source, new Event("ended"));
    }
  }
}

function releaseOnlineSources(record) {
  record.sourceById = new SafeMap();
  SetPrototypeClear(record.sources);
}

function publishOnlineEventWait(contextRef, pending) {
  const context = WeakRefPrototypeDeref(contextRef);
  if (context === undefined) return;
  const record = contextRecord(context);
  record.eventPumpPending = pending;
  if (record.eventPumpClosing) core.refOpPromise(pending);
}

function clearOnlineEventWait(contextRef, pending) {
  const context = WeakRefPrototypeDeref(contextRef);
  if (context === undefined) return;
  const record = contextRecord(context);
  if (record.eventPumpPending === pending) {
    record.eventPumpPending = undefined;
  }
}

function dispatchOnlineEventBatch(contextRef, batch) {
  const context = WeakRefPrototypeDeref(contextRef);
  if (context === undefined) return false;
  const record = contextRecord(context);
  dispatchEndedIds(record, batch.endedIds);
  if (batch.stateChanged || batch.closed) {
    updateOnlineState(context, record);
  }
  if (batch.closed) {
    record.eventPumpRetired = true;
    releaseOnlineSources(record);
  }
  return !batch.closed;
}

function finishOnlineEventPump(contextRef, error = undefined) {
  const context = WeakRefPrototypeDeref(contextRef);
  if (context === undefined) return;
  const record = contextRecord(context);
  record.eventPumpError = error;
}

async function pumpOnlineEvents(contextRef, native, refed) {
  let failure;
  try {
    while (true) {
      let batch;
      let pending;
      try {
        pending = core.ops.op_online_wait_events(native);
        if (!refed) core.unrefOpPromise(pending);
        publishOnlineEventWait(contextRef, pending);
        batch = await pending;
      } catch (error) {
        failure = error;
        return;
      } finally {
        if (pending !== undefined) clearOnlineEventWait(contextRef, pending);
      }
      if (!dispatchOnlineEventBatch(contextRef, batch)) return;
    }
  } finally {
    finishOnlineEventPump(contextRef, failure);
  }
}

function ensureSameContext(source, destination) {
  if (source.context !== destination.context) {
    throw domException(
      "The source and destination belong to different audio contexts.",
      "InvalidAccessError",
    );
  }
}

function convertOutput(value, prefix, position) {
  return webidl.converters["unsigned long"](value, prefix, position);
}

function validatePort(index, count, kind) {
  if (index >= count) {
    throw domException(`The ${kind} port is out of range.`, "IndexSizeError");
  }
}

function convertNullableCallback(value, prefix, position) {
  if (value === undefined || value === null) return undefined;
  return webidl.converters.Function(value, prefix, position);
}

function attachDecodeCallbacks(promise, successCallback, errorCallback) {
  if (successCallback === undefined && errorCallback === undefined) return;
  PromisePrototypeThen(
    promise,
    successCallback === undefined
      ? undefined
      : (buffer) => {
        try {
          ReflectApply(successCallback, undefined, [buffer]);
        } catch (error) {
          reportException(error);
        }
      },
    errorCallback === undefined
      ? undefined
      : (error) => {
        try {
          ReflectApply(errorCallback, undefined, [error]);
        } catch (callbackError) {
          reportException(callbackError);
        }
      },
  );
}

async function decodeAudioBuffer(record, encoded) {
  try {
    const request = new core.ops.NativePhase0DecodeRequest(
      new Uint8Array(encoded),
      record.sampleRate,
    );
    return createAudioBuffer(await core.ops.op_decode_audio_data(request));
  } catch (error) {
    if (StringPrototypeIncludes(String(error), "audio decode failed")) {
      throw domException("Unable to decode audio data.", "EncodingError");
    }
    rethrowNativeError(error, "BaseAudioContext decodeAudioData");
  }
}

class BaseAudioContext extends EventTarget {
  constructor(key = undefined) {
    super();
    if (key !== constructContext) webidl.illegalConstructor();
    this[webidl.brand] = webidl.brand;
  }

  get destination() {
    return contextRecord(this).destination;
  }

  get sampleRate() {
    return contextRecord(this).sampleRate;
  }

  get currentTime() {
    return contextRecord(this).native.currentTime;
  }

  get state() {
    const record = contextRecord(this);
    if (record.online) updateOnlineState(this, record);
    return record.state;
  }

  createBuffer(numberOfChannels, length, sampleRate) {
    const prefix = "Failed to execute 'createBuffer' on 'BaseAudioContext'";
    contextRecord(this);
    webidl.requiredArguments(arguments.length, 3, prefix);
    return new AudioBuffer({
      numberOfChannels: webidl.converters["unsigned long"](
        numberOfChannels,
        prefix,
        "Argument 1",
      ),
      length: webidl.converters["unsigned long"](
        length,
        prefix,
        "Argument 2",
      ),
      sampleRate: webidl.converters.float(
        sampleRate,
        prefix,
        "Argument 3",
      ),
    });
  }

  decodeAudioData(
    audioData,
    successCallback = undefined,
    errorCallback = undefined,
  ) {
    const prefix = "Failed to execute 'decodeAudioData' on 'BaseAudioContext'";
    const record = contextRecord(this);
    webidl.requiredArguments(arguments.length, 1, prefix);
    audioData = webidl.converters.ArrayBuffer(audioData, prefix, "Argument 1");
    successCallback = convertNullableCallback(
      successCallback,
      prefix,
      "Argument 2",
    );
    errorCallback = convertNullableCallback(errorCallback, prefix, "Argument 3");

    let encoded;
    try {
      encoded = ArrayBufferPrototypeTransferToFixedLength(audioData);
    } catch {
      const error = domException(
        "The encoded audio ArrayBuffer is already detached.",
        "DataCloneError",
      );
      const promise = PromiseReject(error);
      attachDecodeCallbacks(promise, successCallback, errorCallback);
      return promise;
    }
    const promise = decodeAudioBuffer(record, encoded);
    attachDecodeCallbacks(promise, successCallback, errorCallback);
    return promise;
  }

  createGain() {
    contextRecord(this);
    return new GainNode(this);
  }

  createOscillator() {
    contextRecord(this);
    return new OscillatorNode(this);
  }

  createBufferSource() {
    contextRecord(this);
    return new AudioBufferSourceNode(this);
  }
}

class OfflineAudioContext extends BaseAudioContext {
  constructor(numberOfChannelsOrOptions, length = undefined, sampleRate = undefined) {
    super(constructContext);
    const prefix = "Failed to construct 'OfflineAudioContext'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    let options;
    if (
      numberOfChannelsOrOptions !== null &&
      typeof numberOfChannelsOrOptions !== "object" &&
      typeof numberOfChannelsOrOptions !== "function"
    ) {
      webidl.requiredArguments(arguments.length, 3, prefix);
      options = {
        __proto__: null,
        numberOfChannels: webidl.converters["unsigned long"](
          numberOfChannelsOrOptions,
          prefix,
          "Argument 1",
        ),
        length: webidl.converters["unsigned long"](
          length,
          prefix,
          "Argument 2",
        ),
        sampleRate: webidl.converters.float(
          sampleRate,
          prefix,
          "Argument 3",
        ),
      };
    } else {
      options = webidl.converters.OfflineAudioContextOptions(
        numberOfChannelsOrOptions,
        prefix,
        "Argument 1",
      );
    }
    if (
      options.numberOfChannels === 0 || options.numberOfChannels > 32 ||
      options.length === 0 || options.sampleRate < 3000 ||
      options.sampleRate > 768000
    ) {
      throw domException(
        "The OfflineAudioContext options are outside the supported range.",
        "NotSupportedError",
      );
    }
    const native = callNative("OfflineAudioContext construction", () =>
      new NativeOfflineAudioContext(
        options.numberOfChannels,
        options.length,
        options.sampleRate,
      ));
    const record = {
      __proto__: null,
      destination: undefined,
      native,
      online: false,
      rendering: false,
      sampleRate: options.sampleRate,
      sources: new SafeSet(),
      state: "suspended",
    };
    WeakMapPrototypeSet(nativeContexts, this, record);
    record.destination = createDestination(this, native.destination());
  }

  get length() {
    return contextRecord(this).native.length;
  }

  async startRendering() {
    const record = contextRecord(this);
    if (record.rendering || record.native.state !== 0) {
      throw domException(
        "Offline rendering has already started.",
        "InvalidStateError",
      );
    }
    record.rendering = true;
    let rendering;
    try {
      rendering = core.ops.op_offline_start_rendering(record.native);
    } catch (error) {
      record.rendering = false;
      rethrowNativeError(error, "OfflineAudioContext rendering");
    }
    record.state = "running";
    dispatch(this, new Event("statechange"));
    let nativeBuffer;
    try {
      nativeBuffer = await rendering;
    } catch (error) {
      if (record.native.state === 0) {
        record.rendering = false;
        record.state = "suspended";
      } else {
        record.state = "closed";
      }
      dispatch(this, new Event("statechange"));
      rethrowNativeError(error, "OfflineAudioContext rendering");
    }
    const renderedBuffer = createAudioBuffer(nativeBuffer);
    for (const source of new SafeSetIterator(record.sources)) {
      const sourceRecord = nodeRecord(source);
      if (!sourceRecord.ended && sourceRecord.native.complete) {
        sourceRecord.ended = true;
        SetPrototypeDelete(record.sources, source);
        dispatch(source, new Event("ended"));
      }
    }
    SetPrototypeClear(record.sources);
    record.state = "closed";
    dispatch(this, new Event("statechange"));
    dispatch(
      this,
      new OfflineAudioCompletionEvent("complete", { renderedBuffer }),
    );
    return renderedBuffer;
  }
}

class AudioContext extends BaseAudioContext {
  constructor(options = undefined) {
    super(constructContext);
    const prefix = "Failed to construct 'AudioContext'";
    options = webidl.converters.AudioContextOptions(
      options,
      prefix,
      "Argument 1",
    );
    if (
      options.sampleRate !== undefined &&
      (options.sampleRate < 3000 || options.sampleRate > 768000)
    ) {
      throw domException(
        "The AudioContext sampleRate is outside the supported range.",
        "NotSupportedError",
      );
    }
    const latencyHint = options.latencyHint;
    const latencyCode = typeof latencyHint === "number"
      ? 3
      : latencyHint === "balanced"
      ? 0
      : latencyHint === "playback"
      ? 2
      : 1;
    const native = callNative("AudioContext construction", () =>
      new NativePhase0OnlineContext(
        options.sampleRate ?? 0,
        false,
        options.sinkId,
        latencyCode,
        typeof latencyHint === "number" ? latencyHint : undefined,
      ));
    const record = {
      __proto__: null,
      closePromise: undefined,
      destination: undefined,
      eventPumpClosing: false,
      eventPumpError: undefined,
      eventPumpPending: undefined,
      eventPumpPromise: undefined,
      eventPumpRefed: native.eventPumpRefed,
      eventPumpRetired: false,
      native,
      online: true,
      sampleRate: native.sampleRate,
      sourceById: new SafeMap(),
      sources: new SafeSet(),
      state: contextStateName(native.contextState),
    };
    WeakMapPrototypeSet(nativeContexts, this, record);
    record.destination = createDestination(this, native.destination());
    record.eventPumpPromise = pumpOnlineEvents(
      new SafeWeakRef(this),
      native,
      record.eventPumpRefed,
    );
  }

  get baseLatency() {
    return contextRecord(this).native.baseLatency;
  }

  get outputLatency() {
    return contextRecord(this).native.outputLatency;
  }

  get sinkId() {
    return contextRecord(this).native.sinkId;
  }

  async suspend() {
    const record = contextRecord(this);
    if (record.state === "closed") {
      throw domException("The AudioContext is closed.", "InvalidStateError");
    }
    try {
      await core.ops.op_phase0_online_suspend(record.native);
    } catch (error) {
      updateOnlineState(this, record);
      rethrowNativeError(error, "AudioContext suspend");
    }
    updateOnlineState(this, record);
  }

  async resume() {
    const record = contextRecord(this);
    if (record.state === "closed") {
      throw domException("The AudioContext is closed.", "InvalidStateError");
    }
    try {
      await core.ops.op_phase0_online_resume(record.native);
    } catch (error) {
      updateOnlineState(this, record);
      rethrowNativeError(error, "AudioContext resume");
    }
    updateOnlineState(this, record);
  }

  close() {
    const record = contextRecord(this);
    if (record.closePromise !== undefined) return record.closePromise;
    record.closePromise = closeOnlineContext(this, record);
    return record.closePromise;
  }
}

async function closeOnlineContext(context, record) {
  if (record.eventPumpRetired) return;
  record.eventPumpClosing = true;
  if (record.eventPumpPending !== undefined) {
    core.refOpPromise(record.eventPumpPending);
  }
  try {
    await core.ops.op_phase0_online_close(record.native);
  } catch (error) {
    record.eventPumpClosing = false;
    if (!record.eventPumpRefed && record.eventPumpPending !== undefined) {
      core.unrefOpPromise(record.eventPumpPending);
    }
    updateOnlineState(context, record);
    rethrowNativeError(error, "AudioContext close");
  }
  try {
    await record.eventPumpPromise;
    if (record.eventPumpError !== undefined) {
      rethrowNativeError(record.eventPumpError, "AudioContext event retirement");
    }
  } finally {
    releaseOnlineSources(record);
  }
  updateOnlineState(context, record);
}

class AudioNode extends EventTarget {
  constructor(key = undefined) {
    super();
    if (key !== constructNode) webidl.illegalConstructor();
    this[webidl.brand] = webidl.brand;
  }

  get context() {
    return nodeRecord(this).context;
  }

  get numberOfInputs() {
    return nodeRecord(this).inputCount;
  }

  get numberOfOutputs() {
    return nodeRecord(this).outputCount;
  }

  get channelCount() {
    return nodeRecord(this).native.channelCount;
  }

  set channelCount(value) {
    const prefix = "Failed to set 'channelCount' on 'AudioNode'";
    const record = nodeRecord(this);
    value = webidl.converters["unsigned long"](value, prefix, "Value");
    if (value === 0 || value > 32) {
      throw domException("channelCount is outside the supported range.", "NotSupportedError");
    }
    if (
      ObjectPrototypeIsPrototypeOf(AudioDestinationNodePrototype, this) &&
      value !== contextRecord(record.context).native.numberOfChannels
    ) {
      throw domException(
        "The destination channelCount cannot be changed.",
        "InvalidStateError",
      );
    }
    if (value === record.native.channelCount) return;
    callNative("channelCount mutation", () => record.native.setChannelCount(value));
  }

  get channelCountMode() {
    switch (nodeRecord(this).native.channelCountMode) {
      case 0:
        return "max";
      case 1:
        return "clamped-max";
      default:
        return "explicit";
    }
  }

  set channelCountMode(value) {
    const prefix = "Failed to set 'channelCountMode' on 'AudioNode'";
    const record = nodeRecord(this);
    value = webidl.converters.ChannelCountMode(value, prefix, "Value");
    if (
      ObjectPrototypeIsPrototypeOf(AudioDestinationNodePrototype, this) &&
      value !== "explicit"
    ) {
      throw domException(
        "The destination channelCountMode cannot be changed.",
        "InvalidStateError",
      );
    }
    const code = channelCountModeCode(value);
    if (code === record.native.channelCountMode) return;
    callNative("channelCountMode mutation", () =>
      record.native.setChannelCountMode(code));
  }

  get channelInterpretation() {
    return nodeRecord(this).native.channelInterpretation === 0
      ? "speakers"
      : "discrete";
  }

  set channelInterpretation(value) {
    const prefix = "Failed to set 'channelInterpretation' on 'AudioNode'";
    const record = nodeRecord(this);
    value = webidl.converters.ChannelInterpretation(value, prefix, "Value");
    const code = channelInterpretationCode(value);
    if (code === record.native.channelInterpretation) return;
    callNative("channelInterpretation mutation", () =>
      record.native.setChannelInterpretation(code));
  }

  connect(destination, output = 0, input = 0) {
    const prefix = "Failed to execute 'connect' on 'AudioNode'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    const source = nodeRecord(this);
    output = convertOutput(output, prefix, "Argument 2");
    validatePort(output, source.outputCount, "output");
    let target;
    if (ObjectPrototypeIsPrototypeOf(AudioParamPrototype, destination)) {
      target = paramRecord(destination);
      if (arguments.length > 2) {
        throw new TypeError("AudioParam connections do not accept an input index.");
      }
      input = 0;
    } else {
      target = nodeRecord(destination);
      input = convertOutput(input, prefix, "Argument 3");
      validatePort(input, target.inputCount, "input");
    }
    ensureSameContext(source, target);
    callNative("AudioNode connection", () =>
      source.native.connect(target.native, output, input));
    return destination;
  }

  disconnect(destinationOrOutput = undefined, output = undefined, input = undefined) {
    const prefix = "Failed to execute 'disconnect' on 'AudioNode'";
    const source = nodeRecord(this);
    if (arguments.length === 0) {
      callNative("AudioNode disconnection", () =>
        source.native.disconnect(undefined, -1, -1));
      return;
    }
    const firstType = typeof destinationOrOutput;
    if (
      destinationOrOutput !== null &&
      firstType !== "object" &&
      firstType !== "function"
    ) {
      const onlyOutput = convertOutput(destinationOrOutput, prefix, "Argument 1");
      validatePort(onlyOutput, source.outputCount, "output");
      callNative("AudioNode disconnection", () =>
        source.native.disconnect(undefined, onlyOutput, -1));
      return;
    }
    let target;
    let isParam = false;
    if (ObjectPrototypeIsPrototypeOf(AudioParamPrototype, destinationOrOutput)) {
      target = paramRecord(destinationOrOutput);
      isParam = true;
    } else {
      target = nodeRecord(destinationOrOutput);
    }
    ensureSameContext(source, target);
    let outputIndex = -1;
    let inputIndex = -1;
    if (arguments.length >= 2) {
      outputIndex = convertOutput(output, prefix, "Argument 2");
      validatePort(outputIndex, source.outputCount, "output");
    }
    if (arguments.length >= 3) {
      if (isParam) {
        throw new TypeError("AudioParam disconnections do not accept an input index.");
      }
      inputIndex = convertOutput(input, prefix, "Argument 3");
      validatePort(inputIndex, target.inputCount, "input");
    }
    callNative("AudioNode disconnection", () =>
      source.native.disconnect(target.native, outputIndex, inputIndex));
  }
}

class AudioDestinationNode extends AudioNode {
  constructor(key = undefined) {
    super(key);
  }

  get maxChannelCount() {
    const record = nodeRecord(this);
    return contextRecord(record.context).native.numberOfChannels;
  }
}

class GainNode extends AudioNode {
  constructor(context, options = undefined) {
    super(constructNode);
    const prefix = "Failed to construct 'GainNode'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    const contextState = contextRecord(context);
    options = webidl.converters.GainOptions(options, prefix, "Argument 2");
    validateNodeOptions(options, prefix);
    const native = callNative("GainNode construction", () =>
      contextState.native.createGain(
        options.gain,
        options.channelCount,
        channelCountModeCode(options.channelCountMode),
        channelInterpretationCode(options.channelInterpretation),
      ));
    initializeNode(this, context, native, 1, 1);
    nodeRecord(this).gain = createParam(context, native.gain());
  }

  get gain() {
    return nodeRecord(this).gain;
  }
}

class AudioScheduledSourceNode extends AudioNode {
  constructor(key = undefined) {
    super(key);
  }
}

function retainStartedSource(source, record) {
  record.started = true;
  const contextState = contextRecord(record.context);
  SetPrototypeAdd(contextState.sources, source);
  if (contextState.online) {
    const id = record.native.sourceId;
    MapPrototypeSet(contextState.sourceById, id, source);
  }
}

class AudioBufferSourceNode extends AudioScheduledSourceNode {
  constructor(context, options = undefined) {
    super(constructNode);
    const prefix = "Failed to construct 'AudioBufferSourceNode'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    const contextState = contextRecord(context);
    options = webidl.converters.AudioBufferSourceOptions(
      options,
      prefix,
      "Argument 2",
    );
    validateNodeOptions(options, prefix);
    if (
      !NumberIsFinite(options.detune) ||
      !NumberIsFinite(options.playbackRate) ||
      !NumberIsFinite(options.loopStart) ||
      !NumberIsFinite(options.loopEnd)
    ) {
      throw new TypeError("AudioBufferSourceNode options must be finite.");
    }
    const native = callNative("AudioBufferSourceNode construction", () =>
      contextState.native.createBufferSource(
        options.detune,
        options.loop,
        options.loopStart,
        options.loopEnd,
        options.playbackRate,
        options.channelCount,
        channelCountModeCode(options.channelCountMode),
        channelInterpretationCode(options.channelInterpretation),
      ));
    initializeNode(this, context, native, 0, 1);
    const record = nodeRecord(this);
    record.buffer = null;
    record.detune = createParam(context, native.detune());
    record.ended = false;
    record.playbackRate = createParam(context, native.playbackRate());
    record.started = false;
    if (options.buffer !== null) this.buffer = options.buffer;
  }

  get buffer() {
    return nodeRecord(this).buffer;
  }

  set buffer(value) {
    const prefix = "Failed to set 'buffer' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    value = convertNullableAudioBuffer(value, prefix, "Value");
    if (value === null) {
      if (record.buffer === null) return;
      throw domException(
        "The bounded AudioBufferSourceNode buffer cannot be cleared after assignment.",
        "InvalidStateError",
      );
    }
    if (record.buffer !== null) {
      throw domException(
        "The bounded AudioBufferSourceNode buffer can only be assigned once.",
        "InvalidStateError",
      );
    }
    callNative("AudioBufferSourceNode buffer assignment", () =>
      record.native.setBuffer(getNativeAudioBuffer(value)));
    record.buffer = value;
  }

  get playbackRate() {
    return nodeRecord(this).playbackRate;
  }

  get detune() {
    return nodeRecord(this).detune;
  }

  get loop() {
    return nodeRecord(this).native.loopEnabled;
  }

  set loop(value) {
    const prefix = "Failed to set 'loop' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    value = webidl.converters.boolean(value, prefix, "Value");
    if (value === record.native.loopEnabled) return;
    callNative("AudioBufferSourceNode loop mutation", () =>
      record.native.setLoopEnabled(value));
  }

  get loopStart() {
    return nodeRecord(this).native.loopStart;
  }

  set loopStart(value) {
    const prefix = "Failed to set 'loopStart' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    value = webidl.converters.double(value, prefix, "Value");
    if (value === record.native.loopStart) return;
    callNative("AudioBufferSourceNode loopStart mutation", () =>
      record.native.setLoopStart(value));
  }

  get loopEnd() {
    return nodeRecord(this).native.loopEnd;
  }

  set loopEnd(value) {
    const prefix = "Failed to set 'loopEnd' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    value = webidl.converters.double(value, prefix, "Value");
    if (value === record.native.loopEnd) return;
    callNative("AudioBufferSourceNode loopEnd mutation", () =>
      record.native.setLoopEnd(value));
  }

  start(when = 0, offset = 0, duration = undefined) {
    const prefix = "Failed to execute 'start' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    when = webidl.converters.double(when, prefix, "Argument 1");
    offset = webidl.converters.double(offset, prefix, "Argument 2");
    duration = duration === undefined
      ? 1.7976931348623157e308
      : webidl.converters.double(duration, prefix, "Argument 3");
    if (
      !NumberIsFinite(when) || when < 0 ||
      !NumberIsFinite(offset) || offset < 0 ||
      !NumberIsFinite(duration) || duration < 0
    ) {
      throw new RangeError(
        "The start time, offset, and duration must be finite and non-negative.",
      );
    }
    if (record.started) {
      throw domException(
        "An AudioBufferSourceNode can only be started once.",
        "InvalidStateError",
      );
    }
    callNative("AudioBufferSourceNode start", () =>
      record.native.startBuffer(when, offset, duration));
    retainStartedSource(this, record);
  }

  stop(when = 0) {
    const prefix = "Failed to execute 'stop' on 'AudioBufferSourceNode'";
    const record = nodeRecord(this);
    when = webidl.converters.double(when, prefix, "Argument 1");
    if (!NumberIsFinite(when) || when < 0) {
      throw new RangeError("The stop time must be finite and non-negative.");
    }
    if (!record.started) {
      throw domException(
        "The AudioBufferSourceNode has not been started.",
        "InvalidStateError",
      );
    }
    callNative("AudioBufferSourceNode stop", () => record.native.stop(when));
  }
}

class OscillatorNode extends AudioScheduledSourceNode {
  constructor(context, options = undefined) {
    super(constructNode);
    const prefix = "Failed to construct 'OscillatorNode'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    const contextState = contextRecord(context);
    options = webidl.converters.OscillatorOptions(options, prefix, "Argument 2");
    validateNodeOptions(options, prefix);
    if (options.type === "custom" || options.periodicWave !== undefined) {
      throw domException(
        "Custom PeriodicWave oscillators are not available in this bounded Web Audio slice.",
        "NotSupportedError",
      );
    }
    const native = callNative("OscillatorNode construction", () =>
      contextState.native.createOscillator(
        oscillatorTypeCode(options.type),
        options.frequency,
        options.detune,
        options.channelCount,
        channelCountModeCode(options.channelCountMode),
        channelInterpretationCode(options.channelInterpretation),
      ));
    initializeNode(this, context, native, 0, 1);
    const record = nodeRecord(this);
    record.detune = createParam(context, native.detune());
    record.ended = false;
    record.frequency = createParam(context, native.frequency());
    record.started = false;
  }

  get frequency() {
    return nodeRecord(this).frequency;
  }

  get detune() {
    return nodeRecord(this).detune;
  }

  get type() {
    return oscillatorTypeName(nodeRecord(this).native.oscillatorType);
  }

  set type(value) {
    const prefix = "Failed to set 'type' on 'OscillatorNode'";
    const record = nodeRecord(this);
    value = webidl.converters.OscillatorType(value, prefix, "Value");
    if (value === "custom") {
      throw domException(
        "A custom oscillator type requires a PeriodicWave.",
        "InvalidStateError",
      );
    }
    const code = oscillatorTypeCode(value);
    if (code === record.native.oscillatorType) return;
    callNative("OscillatorNode type mutation", () =>
      record.native.setOscillatorType(code));
  }

  start(when = 0) {
    const prefix = "Failed to execute 'start' on 'OscillatorNode'";
    const record = nodeRecord(this);
    when = webidl.converters.double(when, prefix, "Argument 1");
    if (!NumberIsFinite(when) || when < 0) {
      throw new RangeError("The start time must be finite and non-negative.");
    }
    if (record.started) {
      throw domException("An OscillatorNode can only be started once.", "InvalidStateError");
    }
    callNative("OscillatorNode start", () => record.native.start(when));
    retainStartedSource(this, record);
  }

  stop(when = 0) {
    const prefix = "Failed to execute 'stop' on 'OscillatorNode'";
    const record = nodeRecord(this);
    when = webidl.converters.double(when, prefix, "Argument 1");
    if (!NumberIsFinite(when) || when < 0) {
      throw new RangeError("The stop time must be finite and non-negative.");
    }
    if (!record.started) {
      throw domException("The OscillatorNode has not been started.", "InvalidStateError");
    }
    callNative("OscillatorNode stop", () => record.native.stop(when));
  }

  setPeriodicWave(periodicWave) {
    const prefix = "Failed to execute 'setPeriodicWave' on 'OscillatorNode'";
    nodeRecord(this);
    webidl.requiredArguments(arguments.length, 1, prefix);
    webidl.converters.object(periodicWave, prefix, "Argument 1");
    throw domException(
      "PeriodicWave is not available in this bounded Web Audio slice.",
      "NotSupportedError",
    );
  }
}

class AudioParam {
  constructor() {
    webidl.illegalConstructor();
  }

  get value() {
    return paramRecord(this).native.value;
  }

  set value(value) {
    const prefix = "Failed to set 'value' on 'AudioParam'";
    const record = paramRecord(this);
    value = webidl.converters.float(value, prefix, "Value");
    callNative("AudioParam value mutation", () => record.native.setValue(value));
  }

  get defaultValue() {
    return paramRecord(this).native.defaultValue;
  }

  get minValue() {
    return paramRecord(this).native.minValue;
  }

  get maxValue() {
    return paramRecord(this).native.maxValue;
  }

  get automationRate() {
    return paramRecord(this).native.automationRate === 0 ? "a-rate" : "k-rate";
  }

  set automationRate(value) {
    const prefix = "Failed to set 'automationRate' on 'AudioParam'";
    const record = paramRecord(this);
    value = webidl.converters.AutomationRate(value, prefix, "Value");
    const code = value === "a-rate" ? 0 : 1;
    if (code === record.native.automationRate) return;
    callNative("AudioParam automation-rate mutation", () =>
      record.native.setAutomationRate(code));
  }

  setValueAtTime(value, startTime) {
    const prefix = "Failed to execute 'setValueAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 2, prefix);
    value = webidl.converters.float(value, prefix, "Argument 1");
    startTime = validTime(startTime, prefix, "Argument 2");
    callNative("AudioParam automation", () =>
      record.native.setValueAtTime(value, startTime));
    return this;
  }

  linearRampToValueAtTime(value, endTime) {
    const prefix = "Failed to execute 'linearRampToValueAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 2, prefix);
    value = webidl.converters.float(value, prefix, "Argument 1");
    endTime = validTime(endTime, prefix, "Argument 2");
    callNative("AudioParam automation", () =>
      record.native.linearRampToValueAtTime(value, endTime));
    return this;
  }

  exponentialRampToValueAtTime(value, endTime) {
    const prefix = "Failed to execute 'exponentialRampToValueAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 2, prefix);
    value = webidl.converters.float(value, prefix, "Argument 1");
    if (value === 0) throw new RangeError("The exponential target cannot be zero.");
    endTime = validTime(endTime, prefix, "Argument 2");
    callNative("AudioParam automation", () =>
      record.native.exponentialRampToValueAtTime(value, endTime));
    return this;
  }

  setTargetAtTime(target, startTime, timeConstant) {
    const prefix = "Failed to execute 'setTargetAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 3, prefix);
    target = webidl.converters.float(target, prefix, "Argument 1");
    startTime = validTime(startTime, prefix, "Argument 2");
    timeConstant = validTime(timeConstant, prefix, "Argument 3");
    callNative("AudioParam automation", () =>
      record.native.setTargetAtTime(target, startTime, timeConstant));
    return this;
  }

  setValueCurveAtTime(values, startTime, duration) {
    const prefix = "Failed to execute 'setValueCurveAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 3, prefix);
    values = boundedFloatSequence(
      values,
      prefix,
      "Argument 1",
    );
    if (values.length < 2) {
      throw domException("A value curve needs at least two values.", "InvalidStateError");
    }
    startTime = validTime(startTime, prefix, "Argument 2");
    duration = validTime(duration, prefix, "Argument 3");
    if (duration === 0) throw new RangeError("The curve duration must be positive.");
    const curve = new Float32Array(values);
    callNative("AudioParam automation", () =>
      record.native.setValueCurveAtTime(curve, startTime, duration));
    return this;
  }

  cancelScheduledValues(cancelTime) {
    const prefix = "Failed to execute 'cancelScheduledValues' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 1, prefix);
    cancelTime = validTime(cancelTime, prefix, "Argument 1");
    callNative("AudioParam automation", () =>
      record.native.cancelScheduledValues(cancelTime));
    return this;
  }

  cancelAndHoldAtTime(cancelTime) {
    const prefix = "Failed to execute 'cancelAndHoldAtTime' on 'AudioParam'";
    const record = paramRecord(this);
    webidl.requiredArguments(arguments.length, 1, prefix);
    cancelTime = validTime(cancelTime, prefix, "Argument 1");
    callNative("AudioParam automation", () =>
      record.native.cancelAndHoldAtTime(cancelTime));
    return this;
  }
}

function validTime(value, prefix, position) {
  value = webidl.converters.double(value, prefix, position);
  if (!NumberIsFinite(value) || value < 0) {
    throw new RangeError("AudioParam times must be finite and non-negative.");
  }
  return value;
}

webidl.converters.OfflineAudioCompletionEventInit =
  webidl.createDictionaryConverter("OfflineAudioCompletionEventInit", [{
    key: "renderedBuffer",
    converter: webidl.converters.AudioBuffer,
    required: true,
  }]);

class OfflineAudioCompletionEvent extends Event {
  constructor(type, eventInitDict) {
    const prefix = "Failed to construct 'OfflineAudioCompletionEvent'";
    webidl.requiredArguments(arguments.length, 2, prefix);
    super(type, eventInitDict);
    eventInitDict = webidl.converters.OfflineAudioCompletionEventInit(
      eventInitDict,
      prefix,
      "Argument 2",
    );
    WeakMapPrototypeSet(completionBuffers, this, eventInitDict.renderedBuffer);
    this[webidl.brand] = webidl.brand;
  }

  get renderedBuffer() {
    webidl.assertBranded(this, OfflineAudioCompletionEventPrototype);
    return WeakMapPrototypeGet(completionBuffers, this);
  }
}

webidl.configureInterface(BaseAudioContext);
const BaseAudioContextPrototype = BaseAudioContext.prototype;
webidl.configureInterface(AudioContext);
const AudioContextPrototype = AudioContext.prototype;
webidl.configureInterface(OfflineAudioContext);
const OfflineAudioContextPrototype = OfflineAudioContext.prototype;
webidl.configureInterface(AudioNode);
const AudioNodePrototype = AudioNode.prototype;
webidl.configureInterface(AudioDestinationNode);
const AudioDestinationNodePrototype = AudioDestinationNode.prototype;
webidl.configureInterface(GainNode);
const GainNodePrototype = GainNode.prototype;
webidl.configureInterface(AudioScheduledSourceNode);
const AudioScheduledSourceNodePrototype = AudioScheduledSourceNode.prototype;
webidl.configureInterface(AudioBufferSourceNode);
const AudioBufferSourceNodePrototype = AudioBufferSourceNode.prototype;
webidl.configureInterface(OscillatorNode);
const OscillatorNodePrototype = OscillatorNode.prototype;
webidl.configureInterface(AudioParam);
const AudioParamPrototype = AudioParam.prototype;
webidl.configureInterface(OfflineAudioCompletionEvent);
const OfflineAudioCompletionEventPrototype = OfflineAudioCompletionEvent.prototype;

defineEventHandler(BaseAudioContextPrototype, "statechange");
defineEventHandler(OfflineAudioContextPrototype, "complete");
defineEventHandler(AudioScheduledSourceNodePrototype, "ended");

webidl.converters.BaseAudioContext = webidl.createInterfaceConverter(
  "BaseAudioContext",
  BaseAudioContextPrototype,
);
webidl.converters.AudioNode = webidl.createInterfaceConverter(
  "AudioNode",
  AudioNodePrototype,
);
webidl.converters.AudioParam = webidl.createInterfaceConverter(
  "AudioParam",
  AudioParamPrototype,
);

const publicPrototypes = [
  BaseAudioContextPrototype,
  AudioContextPrototype,
  OfflineAudioContextPrototype,
  AudioNodePrototype,
  AudioDestinationNodePrototype,
  GainNodePrototype,
  AudioScheduledSourceNodePrototype,
  AudioBufferSourceNodePrototype,
  OscillatorNodePrototype,
  AudioParamPrototype,
  OfflineAudioCompletionEventPrototype,
];
for (let i = 0; i < publicPrototypes.length; ++i) {
  markNotSerializable(publicPrototypes[i]);
}

export {
  AudioContext,
  AudioBufferSourceNode,
  AudioDestinationNode,
  AudioNode,
  AudioParam,
  AudioScheduledSourceNode,
  BaseAudioContext,
  GainNode,
  OfflineAudioCompletionEvent,
  OfflineAudioContext,
  OscillatorNode,
};
