// Copyright 2026 the deno_audio authors. MIT license.

import {
  core,
  domException,
  primordials,
  webidl,
} from "ext:deno_audio/00_webidl.js";

const { NativeAudioBuffer } = core.ops;
const { markNotSerializable } = core.loadExtScript(
  "ext:deno_web/13_message_port.js",
);
const {
  ObjectCreate,
  ObjectPrototypeIsPrototypeOf,
  RangeErrorPrototype,
  SafeWeakMap,
  WeakMapPrototypeGet,
  WeakMapPrototypeSet,
} = primordials;

const nativeBuffers = new SafeWeakMap();

function createAudioBuffer(native) {
  const buffer = ObjectCreate(AudioBufferPrototype);
  WeakMapPrototypeSet(nativeBuffers, buffer, native);
  buffer[webidl.brand] = webidl.brand;
  return buffer;
}

webidl.converters.AudioBufferOptions = webidl.createDictionaryConverter(
  "AudioBufferOptions",
  [
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
  ],
);

function nativeOf(value) {
  webidl.assertBranded(value, AudioBufferPrototype);
  return WeakMapPrototypeGet(nativeBuffers, value);
}

function checkChannel(native, channel) {
  if (channel >= native.numberOfChannels) {
    throw domException("The channel index is out of range.", "IndexSizeError");
  }
}

function mapStorageError(error, operation) {
  if (ObjectPrototypeIsPrototypeOf(RangeErrorPrototype, error)) {
    throw domException(
      `${operation} exceeded the configured audio memory limit.`,
      "QuotaExceededError",
    );
  }
  throw error;
}

class AudioBuffer {
  constructor(options) {
    const prefix = "Failed to construct 'AudioBuffer'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    options = webidl.converters.AudioBufferOptions(
      options,
      prefix,
      "Argument 1",
    );

    const { length, numberOfChannels, sampleRate } = options;
    if (
      length === 0 || numberOfChannels === 0 || numberOfChannels > 32 ||
      sampleRate < 3000 || sampleRate > 768000
    ) {
      throw domException(
        "The AudioBuffer options are outside the supported range.",
        "NotSupportedError",
      );
    }

    let native;
    try {
      native = new NativeAudioBuffer(numberOfChannels, length, sampleRate);
    } catch (error) {
      mapStorageError(error, "AudioBuffer construction");
    }
    WeakMapPrototypeSet(nativeBuffers, this, native);
    this[webidl.brand] = webidl.brand;
  }

  get sampleRate() {
    return nativeOf(this).sampleRate;
  }

  get length() {
    return nativeOf(this).length;
  }

  get duration() {
    return nativeOf(this).duration;
  }

  get numberOfChannels() {
    return nativeOf(this).numberOfChannels;
  }

  getChannelData(channel) {
    const prefix = "Failed to execute 'getChannelData' on 'AudioBuffer'";
    webidl.requiredArguments(arguments.length, 1, prefix);
    const native = nativeOf(this);
    channel = webidl.converters["unsigned long"](
      channel,
      prefix,
      "Argument 1",
    );
    checkChannel(native, channel);
    try {
      return native.getChannelData(channel);
    } catch (error) {
      mapStorageError(error, "AudioBuffer channel exposure");
    }
  }

  copyFromChannel(destination, channelNumber, bufferOffset = 0) {
    const prefix = "Failed to execute 'copyFromChannel' on 'AudioBuffer'";
    webidl.requiredArguments(arguments.length, 2, prefix);
    const native = nativeOf(this);
    destination = webidl.converters.Float32Array(
      destination,
      prefix,
      "Argument 1",
    );
    channelNumber = webidl.converters["unsigned long"](
      channelNumber,
      prefix,
      "Argument 2",
    );
    bufferOffset = webidl.converters["unsigned long"](
      bufferOffset,
      prefix,
      "Argument 3",
    );
    checkChannel(native, channelNumber);
    native.copyFromChannel(destination, channelNumber, bufferOffset);
  }

  copyToChannel(source, channelNumber, bufferOffset = 0) {
    const prefix = "Failed to execute 'copyToChannel' on 'AudioBuffer'";
    webidl.requiredArguments(arguments.length, 2, prefix);
    const native = nativeOf(this);
    source = webidl.converters.Float32Array(
      source,
      prefix,
      "Argument 1",
    );
    channelNumber = webidl.converters["unsigned long"](
      channelNumber,
      prefix,
      "Argument 2",
    );
    bufferOffset = webidl.converters["unsigned long"](
      bufferOffset,
      prefix,
      "Argument 3",
    );
    checkChannel(native, channelNumber);
    native.copyToChannel(source, channelNumber, bufferOffset);
  }
}

webidl.configureInterface(AudioBuffer);
const AudioBufferPrototype = AudioBuffer.prototype;
markNotSerializable(AudioBufferPrototype);
webidl.converters.AudioBuffer = webidl.createInterfaceConverter(
  "AudioBuffer",
  AudioBufferPrototype,
);

export {
  AudioBuffer,
  createAudioBuffer,
  nativeOf as getNativeAudioBuffer,
};
