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
} from "ext:deno_audio/01_audio_buffer.js";

const { NativeOfflineAudioContext } = core.ops;
const { markNotSerializable } = core.loadExtScript(
  "ext:deno_web/13_message_port.js",
);
const {
  defineEventHandler,
  Event,
  EventTarget,
  EventTargetPrototype,
} = core.loadExtScript("ext:deno_web/02_event.js");
const {
  ArrayPrototypePush,
  Float32Array,
  NumberIsFinite,
  ObjectPrototypeIsPrototypeOf,
  RangeError,
  RangeErrorPrototype,
  ReflectApply,
  SafeSet,
  SafeSetIterator,
  SafeWeakMap,
  SetPrototypeAdd,
  SetPrototypeClear,
  SetPrototypeDelete,
  String,
  StringPrototypeIncludes,
  Symbol,
  SymbolIterator,
  TypeError,
  TypeErrorPrototype,
  WeakMapPrototypeGet,
  WeakMapPrototypeSet,
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
    "NotSupportedError",
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
    return contextRecord(this).native.sampleRate;
  }

  get currentTime() {
    return contextRecord(this).native.currentTime;
  }

  get state() {
    return contextRecord(this).state;
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

  createGain() {
    contextRecord(this);
    return new GainNode(this);
  }

  createOscillator() {
    contextRecord(this);
    return new OscillatorNode(this);
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
      rendering: false,
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
        "The offline destination channelCount cannot be changed.",
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
        "The offline destination channelCountMode cannot be changed.",
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
        "Custom PeriodicWave oscillators are not available in this bounded offline slice.",
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
    record.started = true;
    SetPrototypeAdd(contextRecord(record.context).sources, this);
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
      "PeriodicWave is not available in this bounded offline slice.",
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
  OfflineAudioContextPrototype,
  AudioNodePrototype,
  AudioDestinationNodePrototype,
  GainNodePrototype,
  AudioScheduledSourceNodePrototype,
  OscillatorNodePrototype,
  AudioParamPrototype,
  OfflineAudioCompletionEventPrototype,
];
for (let i = 0; i < publicPrototypes.length; ++i) {
  markNotSerializable(publicPrototypes[i]);
}

export {
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
