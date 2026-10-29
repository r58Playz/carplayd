#!/usr/bin/env python3
"""Stand in for carplayd so a debug network provider (such as aa-debug-bridge) runs at a desk.

The receiver speaks carplayd's side of the debug network protocol: it sends init and
display_request, decodes the provider's H.264 and applies its safe area exactly as
carplayd crops it, plays PCM streams, shows Now Playing, and sends mouse and keyboard
input as provider input events. Messages carplayd would reject are reported with
carplayd's reason; --record keeps the control trace, H.264 and PCM of each session.
"""

import argparse
from array import array
from collections import deque
from dataclasses import dataclass
import json
import logging
import math
import os
import queue
import signal
import socket
import struct
import sys
import threading
import time
import wave


LOG = logging.getLogger("debug-receiver")
# tokio_util's LengthDelimitedCodec default, which carplayd uses for control and video.
MAX_PACKET = 8 * 1024 * 1024
# VideoDecoderConfig::default().max_access_unit_size.
MAX_ACCESS_UNIT = 4 * 1024 * 1024
# PendingDisplay and audio::open() wait this long for the provider to connect.
CONNECT_TIMEOUT = 10
U16, U32, U64 = 0xFFFF, 0xFFFFFFFF, 0xFFFFFFFFFFFFFFFF
SAMPLE_BYTES = {"s16_le": 2, "f32_le": 4}
AUDIO_KINDS = ("main", "alternate")
AUDIO_USES = ("media", "alert", "telephony", "speechRecognition", "default")
AUDIO_CLOCKS = ("live", "demand_driven")
SPEECH = ("none", "speaking", "listening")
# AudioCapabilities::default().
AUDIO_CAPABILITIES = dict(sample_types=["s16_le", "f32_le"], min_sample_rate=8000,
                          max_sample_rate=96000, max_channels=2)
# Reported as the negotiated head-unit format in audio_status; providers only log it.
WIRE_FORMAT = dict(sample_rate=48000, channels=2, sample_type="s16_le")
# provider::resampler::{TARGET_BUFFER_SECONDS, MAX_BUFFER_SECONDS}.
TARGET_BUFFER, MAX_BUFFER = 0.050, 0.400
# Local sink headroom, so the receiver's own scheduling adds no glitches to the modelled output.
LOCAL_PREBUFFER = 0.120
INPUT_KEYS = (
    "Play", "Pause", "NextTrack", "PreviousTrack", "Shuffle", "Repeat", "TrackingNormal",
    "TrackingIncrement", "TrackingDecrement", "PlayPause", "Mute", "VolumeUp", "VolumeDown",
    "Promote", "Demote", "AddToCart", "Home", "Back", "NavGuidance", "Backspace", "HookSwitch",
    "Flash", "Drop", "PhoneMute", *(f"PhoneKey{n}" for n in range(10)), "PhoneKeyStar",
    "PhoneKeyPound", "Primary",
)
# InputDevice::IapRemote.supported_keys().
IAP_REMOTE_KEYS = INPUT_KEYS[:16]
INPUT_AXES = ("X", "Y", "Rz", "Dial", "Wheel")
# Axes aa-debug-bridge forwards as the AA rotary controller.
ROTARY_AXES = ("Dial", "Wheel", "Rz")
NAL_NAMES = {1: "slice", 5: "IDR", 6: "SEI", 7: "SPS", 8: "PPS", 9: "AUD"}
PLAYBACK_STATUS = ("Stopped", "Playing", "Paused", "SeekForward", "SeekBackward")
STRING, BOOL = "string", "bool"
LIKED = ("Unsupported", "No", "Yes")
# NowPlayingMediaItem and NowPlaying fields; all are optional and nullable.
MEDIA_ITEM_FIELDS = {
    "persistent_identifier": U64, "title": STRING, "playback_duration": U32,
    "album_title": STRING, "album_track_number": U16, "album_track_count": U16,
    "album_disc_number": U16, "album_disc_count": U16, "artist": STRING, "genre": STRING,
    "composer": STRING, "liked": LIKED, "banned": LIKED, "chapter_count": U16,
}
NOW_PLAYING_FIELDS = {
    "status": PLAYBACK_STATUS, "elapsed_time": U32, "queue_index": U32, "queue_count": U32,
    "queue_chapter_index": U32, "shuffle_mode": ("Off", "Songs", "Albums"),
    "repeat_mode": ("Off", "One", "All"), "app_name": STRING,
    "media_library_unique_identifier": STRING, "apple_music_radio_ad": BOOL,
    "apple_music_radio_station_name": STRING,
    "apple_music_radio_station_media_playlist_id": U64, "playback_speed": U16,
    "set_elapsed_time_allowed": BOOL,
}
# Bound by load_gi(); the protocol code and tests do not need desktop libraries.
GLib = Gst = GstVideo = Gtk = Gdk = GdkPixbuf = cairo = unix_signal_add = None


class ProtocolError(ValueError):
    """Something carplayd's network provider rejects."""


class ReceiverClosed(Exception):
    """The receiver ended a connection itself."""


def recv_exact(sock, length):
    data = bytearray()
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise ProtocolError("connection closed in the middle of a frame")
        data.extend(chunk)
    return bytes(data)


def recv_frame(sock, limit=MAX_PACKET):
    """One little-endian LengthDelimitedCodec frame, or None at EOF on a frame boundary."""
    header = sock.recv(4)
    if not header:
        return None
    if len(header) < 4:
        header += recv_exact(sock, 4 - len(header))
    length, = struct.unpack("<I", header)
    if length > limit:
        raise ProtocolError(f"frame of {length} bytes exceeds the {limit}-byte limit")
    return recv_exact(sock, length)


def frame(payload):
    return struct.pack("<I", len(payload)) + payload


def encode(message):
    return frame(json.dumps(message, ensure_ascii=False, separators=(",", ":")).encode())


def reject_constant(name):
    raise ValueError(f"{name} is not valid JSON")


def close_socket(sock):
    try:
        sock.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    sock.close()


def unsigned(value, maximum, name):
    if type(value) is not int or not 0 <= value <= maximum:
        raise ProtocolError(f"{name} must be an integer in 0–{maximum}, got {value!r}")
    return value


def choice(value, options, name):
    if not isinstance(value, str) or value not in options:
        raise ProtocolError(f"{name} must be one of {', '.join(options)}, got {value!r}")
    return value


def boolean(value, name):
    if type(value) is not bool:
        raise ProtocolError(f"{name} must be a boolean, got {value!r}")
    return value


@dataclass(frozen=True)
class DisplayParams:
    width: int
    height: int
    fps: int


@dataclass(frozen=True)
class Crop:
    left: int
    top: int
    width: int
    height: int

    def __str__(self):
        return f"{self.width}x{self.height} at {self.left},{self.top}"


def parse_crop(value):
    """FrameCrop deserialization: null, or four u32 fields."""
    if value is None:
        return None
    if not isinstance(value, dict):
        raise ProtocolError(f"failed to read provider message: safe_area must be null or an object, got {value!r}")
    try:
        return Crop(*(unsigned(value.get(key), U32, f"safe_area.{key}")
                      for key in ("left", "top", "width", "height")))
    except ProtocolError as exc:
        raise ProtocolError(f"failed to read provider message: {exc}") from None


def validate_crop(crop, display):
    """PendingDisplay::respond: an even YU12 crop exactly the size of the display."""
    if crop is None:
        return
    if not crop.width or not crop.height:
        raise ProtocolError("empty YU12 crop")
    if any(n % 2 for n in (crop.left, crop.top, crop.width, crop.height)):
        raise ProtocolError("YU12 crop coordinates and dimensions must be even")
    if crop.left + crop.width > U32 or crop.top + crop.height > U32:
        raise ProtocolError("crop right/bottom overflow")
    if (crop.width, crop.height) != (display.width, display.height):
        raise ProtocolError(f"safe area must exactly match display {display.width}x{display.height}")


def frame_problem(width, height, crop, display):
    """Why carplayd's crop/copy_to would fail every decoded frame, or None."""
    if crop:
        if crop.left + crop.width > width or crop.top + crop.height > height:
            return f"YU12 crop exceeds the frame (decoded {width}x{height}, safe area {crop})"
    elif (width, height) != (display.width, display.height):
        return (f"YU12 view is {width}x{height}, destination is {display.width}x{display.height}; "
                "crop before copying")
    return None


@dataclass(frozen=True)
class PcmFormat:
    sample_rate: int
    channels: int
    sample_type: str

    @property
    def frame_bytes(self):
        return self.channels * SAMPLE_BYTES[self.sample_type]

    @property
    def max_packet(self):
        # audio::open() limits each packet to its position and 20 ms of frames.
        return 8 + self.sample_rate // 50 * self.frame_bytes

    def caps(self):
        return (f"audio/x-raw,format={'S16LE' if self.sample_type == 's16_le' else 'F32LE'},"
                f"layout=interleaved,rate={self.sample_rate},channels={self.channels}")

    def __str__(self):
        return f"{self.sample_rate} Hz {'mono' if self.channels == 1 else 'stereo'} {self.sample_type}"


def parse_pcm(value, name):
    if not isinstance(value, dict):
        raise ProtocolError(f"{name} must be a PCM format object, got {value!r}")
    return PcmFormat(unsigned(value.get("sample_rate"), U32, f"{name}.sample_rate"),
                     unsigned(value.get("channels"), 0xFF, f"{name}.channels"),
                     choice(value.get("sample_type"), tuple(SAMPLE_BYTES), f"{name}.sample_type"))


def validate_pcm(pcm):
    """PcmFormat::validate."""
    if not 8000 <= pcm.sample_rate <= 96000:
        raise ProtocolError("PCM sample rate must be 8000–96000 Hz")
    if pcm.channels not in (1, 2):
        raise ProtocolError("PCM must be mono or stereo")


@dataclass(frozen=True)
class AudioRequest:
    id: int
    kind: str
    audio_type: str
    playback: PcmFormat
    microphone: PcmFormat = None
    playback_clock: str = "live"
    microphone_clock: str = "live"


def parse_audio_request(value):
    """AudioRequest deserialization; missing clocks default to live, null is invalid."""
    if not isinstance(value, dict):
        raise ProtocolError(f"request must be an object, got {value!r}")
    microphone = value.get("microphone")
    return AudioRequest(
        unsigned(value.get("id"), U64, "request.id"),
        choice(value.get("kind"), AUDIO_KINDS, "request.kind"),
        choice(value.get("audio_type"), AUDIO_USES, "request.audio_type"),
        parse_pcm(value.get("playback"), "request.playback"),
        None if microphone is None else parse_pcm(microphone, "request.microphone"),
        choice(value.get("playback_clock", "live"), AUDIO_CLOCKS, "request.playback_clock"),
        choice(value.get("microphone_clock", "live"), AUDIO_CLOCKS, "request.microphone_clock"),
    )


def validate_audio_request(request):
    """AudioRequest::validate."""
    validate_pcm(request.playback)
    if request.microphone:
        validate_pcm(request.microphone)
    if request.kind != "main" and request.microphone:
        raise ProtocolError("alternate audio cannot enable microphone input")


def check_fields(value, fields, prefix):
    for key, kind in fields.items():
        item = value.get(key)
        if item is None:
            continue
        if kind == STRING:
            valid = isinstance(item, str)
        elif kind == BOOL:
            valid = type(item) is bool
        elif isinstance(kind, tuple):
            valid = isinstance(item, str) and item in kind
        else:
            valid = type(item) is int and 0 <= item <= kind
        if not valid:
            raise ProtocolError(f"failed to read provider message: invalid {prefix}{key}: {item!r}")


def validate_now_playing(value):
    """NowPlaying deserialization: media_item is required, every other field is optional."""
    if value is None:
        return None
    if not isinstance(value, dict) or not isinstance(value.get("media_item"), dict):
        raise ProtocolError("failed to read provider message: now_playing needs a media_item object")
    check_fields(value["media_item"], MEDIA_ITEM_FIELDS, "now_playing.media_item.")
    check_fields(value, NOW_PLAYING_FIELDS, "now_playing.")
    return value


def describe_now_playing(snapshot):
    """Track and playback state, without the elapsed time that changes every update."""
    if snapshot is None:
        return "no media source"
    item = snapshot["media_item"]
    title = item.get("title") or "untitled"
    artist = item.get("artist")
    app = snapshot.get("app_name")
    return " · ".join(filter(None, (snapshot.get("status") or "no status", app,
                                    f"{title} — {artist}" if artist else title)))


def nal_types(data):
    """NAL unit types in an Annex-B access unit; empty without a start code."""
    types = []
    index = data.find(b"\0\0\1")
    while index != -1 and index + 3 < len(data):
        types.append(data[index + 3] & 0x1F)
        index = data.find(b"\0\0\1", index + 3)
    return types


def nal_names(types):
    return " ".join(NAL_NAMES.get(t, str(t)) for t in types) or "no start code"


def pcm_to_s16(data, sample_type):
    if sample_type == "s16_le":
        return data
    samples = array("f")
    samples.frombytes(data)
    if sys.byteorder == "big":
        samples.byteswap()
    converted = array("h", (max(-32768, min(32767, round(s * 32767))) if math.isfinite(s) else 0
                            for s in samples))
    if sys.byteorder == "big":
        converted.byteswap()
    return converted.tobytes()


def default_input_devices(touch, knob):
    """Network HID devices then IapRemote, in the order carplayd advertises them."""
    devices = []
    if touch != "none":
        surface = "Touchscreen" if touch == "touchscreen" else "Touchpad"
        devices.append({"Network": {
            "id": f"debug-{touch}", "associated_display": None, "keys": [],
            "touch": {surface: {"max_touches": 2, "physical_size_mm": None}}, "axes": [],
        }})
    if knob:
        devices.append({"Network": {
            "id": "debug-knob", "associated_display": None,
            "keys": ["Primary", "Back", "Home", "NavGuidance"], "touch": None,
            "axes": [{"id": 1, "axis": "Dial", "relative": True,
                      "logical_minimum": -127, "logical_maximum": 127}],
        }})
    devices.append("IapRemote")
    return devices


def validate_input_device(device):
    if device == "IapRemote":
        return device
    if not isinstance(device, dict) or set(device) != {"Network"} or not isinstance(device["Network"], dict):
        raise ValueError(f"unknown input device: {device!r}")
    info = dict(device["Network"])
    info.setdefault("associated_display", None)
    if not isinstance(info.get("id"), str):
        raise ValueError("Network.id must be a string")
    if info["associated_display"] is not None and not isinstance(info["associated_display"], str):
        raise ValueError("Network.associated_display must be a string or null")
    keys = info.get("keys")
    if not isinstance(keys, list) or any(key not in INPUT_KEYS for key in keys):
        raise ValueError(f"Network.keys must list carplayd InputKey names, got {keys!r}")
    touch = info.get("touch")
    if touch is not None:
        if not isinstance(touch, dict) or len(touch) != 1 or next(iter(touch)) not in ("Touchscreen", "Touchpad"):
            raise ValueError("Network.touch must be null, {\"Touchscreen\": …} or {\"Touchpad\": …}")
        details = next(iter(touch.values()))
        if not isinstance(details, dict) or type(details.get("max_touches")) is not int:
            raise ValueError("touch capability needs an integer max_touches")
        details.setdefault("physical_size_mm", None)
    axes = info.get("axes")
    if not isinstance(axes, list):
        raise ValueError("Network.axes must be a list")
    for axis in axes:
        if (not isinstance(axis, dict) or type(axis.get("id")) is not int or axis.get("axis") not in INPUT_AXES
                or type(axis.get("relative")) is not bool
                or type(axis.get("logical_minimum")) is not int
                or type(axis.get("logical_maximum")) is not int):
            raise ValueError(f"invalid axis capability: {axis!r}")
    return {"Network": info}


def load_input_devices(path):
    with open(path, encoding="utf-8") as file:
        value = json.load(file)
    if isinstance(value, dict) and "input_devices" in value:
        value = value["input_devices"]
    if not isinstance(value, list):
        raise ValueError("expected a JSON list of carplayd InputDevice values or an init message")
    return [validate_input_device(device) for device in value]


class RateMeter:
    """Amounts per second over a sliding window; safe across threads."""

    def __init__(self, window=2.0):
        self.window = window
        self.events = deque()
        self.lock = threading.Lock()
        self.started = None

    def add(self, amount, now):
        with self.lock:
            if self.started is None:
                self.started = now
            self.events.append((now, amount))
            self._prune(now)

    def _prune(self, now):
        while self.events and self.events[0][0] < now - self.window:
            self.events.popleft()

    def rate(self, now=None):
        now = time.monotonic() if now is None else now
        with self.lock:
            if self.started is None:
                return 0.0
            self._prune(now)
            return sum(amount for _, amount in self.events) / min(self.window, max(now - self.started, 0.5))


class DriftController:
    """provider::resampler::DriftController: nudges the read rate toward the target buffer."""

    def __init__(self):
        self.filtered = self.integral = self.correction = 0.0
        self.initialized = False

    def update(self, queued, seconds):
        dt = min(max(seconds, 0.0), 0.5)
        if not self.initialized:
            self.filtered, self.initialized = queued, True
        self.filtered += (queued - self.filtered) * dt / (1 + dt)
        error_ms = (self.filtered - TARGET_BUFFER) * 1000
        candidate = min(max(self.integral + error_ms * 10 * dt, -2000.0), 2000.0)
        desired = min(max(200 * error_ms + candidate, -2000.0), 2000.0)
        if abs(desired) < 2000 or math.copysign(1, error_ms) != math.copysign(1, desired):
            self.integral = candidate
        self.correction += min(max(desired - self.correction, -100 * dt), 100 * dt)
        return 1 / (1 + self.correction * 1e-6)


class PlaybackModel:
    """carplayd's playback of one stream (CarplayNetSession playback()), from arrival times.

    Output starts 50 ms after the stream starts and reads a 5 ms block per period. It plays
    silence until 50 ms is queued, and again from an underrun until the queue refills. A
    position discontinuity discards the queue; more than 400 ms trims the oldest back to 50 ms.
    Live streams pace with the drift controller; demand-driven ones are only read below 50 ms.
    """

    def __init__(self, format, live, on_block=None, on_event=None):
        self.format, self.live = format, live
        self.on_block = on_block or (lambda data: None)
        self.on_event = on_event or (lambda kind, detail: None)
        self.block = -(-format.sample_rate // 200)
        self.period = self.block / format.sample_rate
        self.max_frames = math.ceil(format.sample_rate * MAX_BUFFER)
        self.target_frames = math.ceil(format.sample_rate * TARGET_BUFFER)
        self.queue = bytearray()
        self.deadline = self.next_position = None
        self.carry = 0.0
        self.primed = self.gap = False
        self.drift = DriftController()
        self.resets = self.stale_drops = self.underruns = 0

    @property
    def queued(self):
        return len(self.queue) / self.format.frame_bytes / self.format.sample_rate

    def start(self, now):
        """A new playback task: resumed live streams also discard what the provider buffered."""
        self.queue.clear()
        self.deadline, self.carry = now + TARGET_BUFFER, 0.0
        self.primed = self.gap = False
        self.drift = DriftController()

    def stop(self):
        self.deadline = None

    def flush(self):
        self.queue.clear()
        self.primed = False
        self.drift = DriftController()

    def wants_data(self):
        return self.live or self.queued < TARGET_BUFFER

    def advance(self, now):
        while self.deadline is not None and self.deadline <= now:
            self.deadline += self.period
            data = bytes(self.block * self.format.frame_bytes)
            self.primed = self.primed or self.queued >= TARGET_BUFFER
            if self.primed:
                frames = self.block
                if self.live:
                    frames = self.block / self.drift.update(self.queued, self.period) + self.carry
                    self.carry = frames - int(frames)
                    frames = int(frames)
                size = frames * self.format.frame_bytes
                if len(self.queue) >= size and size:
                    data = bytes(self.queue[:size])
                    del self.queue[:size]
                else:
                    self.primed, self.gap = False, True
            self.on_block(data)

    def push(self, position, data, now):
        self.advance(now)
        frames = len(data) // self.format.frame_bytes
        if self.next_position is not None and position != self.next_position:
            self.resets += 1
            self.queue.clear()
            self.primed = False
            self.drift = DriftController()
            self.on_event("reset", f"position {position} does not continue {self.next_position}")
        self.next_position = position + frames
        queued = len(self.queue) // self.format.frame_bytes
        if self.live and queued + frames > self.max_frames:
            self.stale_drops += 1
            stale = min(queued + frames - self.target_frames, queued)
            del self.queue[:stale * self.format.frame_bytes]
            self.drift = DriftController()
            dropped = stale * 1000 // self.format.sample_rate
            self.on_event("stale", f"more than 400 ms queued, dropped the oldest {dropped} ms")
        if self.gap and self.deadline is not None:
            # Running dry followed by more PCM was an audible gap mid-stream.
            self.gap = False
            self.underruns += 1
            self.on_event("underrun", "PCM arrived after the queue ran empty")
        self.queue.extend(data)


class Recorder:
    """Control trace, H.264 and PCM of one session under DIR/session-N."""

    def __init__(self, root, number):
        self.directory = os.path.join(root, f"session-{number}")
        os.makedirs(self.directory)
        self.lock = threading.Lock()
        self.control = open(self.path("control.jsonl"), "w", encoding="utf-8")

    def path(self, name):
        return os.path.join(self.directory, name)

    def message(self, elapsed, direction, message):
        line = json.dumps(dict(t=round(elapsed, 6), dir=direction, message=message), ensure_ascii=False)
        with self.lock:
            if not self.control.closed:
                self.control.write(line + "\n")
                self.control.flush()

    def close(self):
        with self.lock:
            self.control.close()


class DisplayFiles:
    def __init__(self, recorder, number):
        self.video = open(recorder.path(f"display-{number}.h264"), "wb")
        self.timing = open(recorder.path(f"display-{number}.csv"), "w", encoding="utf-8", buffering=1)
        self.timing.write("arrival_s,pts_us,bytes,nal_types\n")

    def write(self, elapsed, pts, unit, types):
        # Flush every access unit: a recording matters most when something crashes.
        self.video.write(unit)
        self.video.flush()
        self.timing.write(f"{elapsed:.6f},{pts},{len(unit)},{nal_names(types)}\n")

    def close(self):
        self.video.close()
        self.timing.close()


class VideoDecoder:
    """Decode received access units to the full visible frame, as the Pi's decoder does."""

    def __init__(self, rgb, on_size, on_message):
        convert = "! videoconvert ! video/x-raw,format=RGB " if rgb else ""
        self.pipeline = Gst.parse_launch(
            "appsrc name=source is-live=true format=time max-bytes=16777216 "
            "! h264parse ! avdec_h264 max-threads=2 " + convert +
            "! appsink name=sink emit-signals=true sync=false max-buffers=1 drop=true "
            "enable-last-sample=false"
        )
        self.source = self.pipeline.get_by_name("source")
        self.source.set_property("caps", Gst.Caps.from_string(
            "video/x-h264,stream-format=byte-stream,alignment=au"))
        self.on_size, self.on_message = on_size, on_message
        self.lock = threading.Lock()
        self.sample = self.size = None
        self.frames = 0
        self.pipeline.get_by_name("sink").connect("new-sample", self._sample)
        self.bus = self.pipeline.get_bus()
        if self.pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError("could not start the H.264 decoder pipeline")

    def _sample(self, sink):
        sample = sink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        structure = sample.get_caps().get_structure(0)
        size = (structure.get_value("width"), structure.get_value("height"))
        with self.lock:
            changed = size != self.size
            self.sample, self.size = sample, size
            self.frames += 1
        if changed:
            self.on_size(size)
        return Gst.FlowReturn.OK

    def push(self, data):
        self.source.emit("push-buffer", Gst.Buffer.new_wrapped(data))
        poll_bus(self.bus, self.on_message)

    def stop(self):
        self.pipeline.set_state(Gst.State.NULL)


class AudioPlayer:
    def __init__(self, format, sink, volume, on_message):
        self.pipeline = Gst.parse_launch(
            f"appsrc name=source is-live=true format=time do-timestamp=true "
            f"max-bytes={format.sample_rate * format.frame_bytes} "
            f"! audioconvert ! audioresample ! volume name=volume ! {sink}"
        )
        self.source = self.pipeline.get_by_name("source")
        self.source.set_property("caps", Gst.Caps.from_string(format.caps()))
        if self.source.find_property("leaky-type"):
            self.source.set_property("leaky-type", 2)  # GstAppLeakyType.DOWNSTREAM
        # Play PCM as it arrives; network jitter must not shift a clock-synchronized sink.
        iterator = self.pipeline.iterate_sinks()
        while True:
            result, element = iterator.next()
            if result != Gst.IteratorResult.OK:
                break
            if element.find_property("sync"):
                element.set_property("sync", False)
        self.volume = self.pipeline.get_by_name("volume")
        self.volume.set_property("volume", volume)
        self.on_message = on_message
        self.bus = self.pipeline.get_bus()
        if self.pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError(f"could not start audio sink {sink!r}")
        # Modelled blocks arrive in real time; start the sink ahead of them.
        self.push(bytes(int(format.sample_rate * LOCAL_PREBUFFER) * format.frame_bytes))

    def push(self, data):
        self.source.emit("push-buffer", Gst.Buffer.new_wrapped(data))
        poll_bus(self.bus, self.on_message)

    def set_volume(self, volume):
        self.volume.set_property("volume", volume)

    def stop(self):
        self.pipeline.set_state(Gst.State.NULL)


def poll_bus(bus, report):
    # Polled from the pushing thread, so headless mode needs no GLib main loop.
    while True:
        message = bus.pop_filtered(Gst.MessageType.ERROR | Gst.MessageType.WARNING)
        if message is None:
            return
        if message.type == Gst.MessageType.ERROR:
            error, detail = message.parse_error()
            report(logging.ERROR, error.message)
        else:
            error, detail = message.parse_warning()
            report(logging.WARNING, error.message)
        LOG.debug("GStreamer: %s", detail)


class DisplayStream:
    """One display_request: its response, its connection and the access units received."""

    def __init__(self, session, number):
        self.session, self.number = session, number
        self.listener = session.receiver.listen()
        self.port = self.listener.getsockname()[1]
        self.deadline = time.monotonic() + CONNECT_TIMEOUT
        self.responded = threading.Event()
        self.closed = threading.Event()
        self.crop = None
        self.state = "waiting for display_response"
        self.socket = self.decoder = self.files = self.frame_problem = None
        self.packets = self.bytes = self.keyframes = self.backwards = 0
        self.headers = set()
        self.first_frame_checked = False
        self.first_pts = self.last_pts = None
        self.requested_at = time.monotonic()
        self.connected_at = self.first_packet_at = self.last_packet_at = None
        self.max_gap = 0.0
        self.fps = RateMeter()
        self.bitrate = RateMeter()

    def start(self):
        self.session.spawn(self._run, f"display-{self.number}")

    def respond(self, crop):
        if self.responded.is_set():
            raise ProtocolError("duplicate display response")
        validate_crop(crop, self.session.receiver.display)
        self.crop = crop
        self.state = "waiting for the display connection"
        LOG.info("display %d: safe area %s", self.number, crop or "none (uncropped)")
        self.responded.set()

    def _accept(self):
        while not self.responded.wait(0.1):
            if self.closed.is_set():
                return None
            if time.monotonic() >= self.deadline:
                raise ProtocolError("display response/connection timed out")
        while not self.closed.is_set():
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise ProtocolError("display response/connection timed out")
            self.listener.settimeout(min(remaining, 0.1))
            try:
                return self.listener.accept()
            except socket.timeout:
                pass
        return None

    def _run(self):
        try:
            accepted = self._accept()
            if accepted and accepted[1][0] != self.session.peer:
                accepted[0].close()
                raise ProtocolError("display connection peer does not match control peer")
        except Exception as exc:
            self.session.display_failed(self, exc)
            return
        finally:
            self.listener.close()
        if accepted is None:
            self.session.display_failed(self, ReceiverClosed("display request cancelled"))
            return
        sock = accepted[0]
        sock.settimeout(None)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        self.socket = sock
        if self.closed.is_set():
            close_socket(sock)
            return
        self.connected_at = time.monotonic()
        self.state = "connected, waiting for video"
        self.session.display_connected(self)
        error = None
        try:
            self.decoder = self.session.receiver.make_decoder(self)
            if self.session.recorder:
                self.files = DisplayFiles(self.session.recorder, self.number)
            while not self.closed.is_set():
                payload = recv_frame(sock)
                if payload is None:
                    break
                self._packet(payload, time.monotonic())
        except Exception as exc:
            if not self.closed.is_set():
                error = exc
        finally:
            close_socket(sock)
            if self.decoder:
                self.decoder.stop()
            if self.files:
                self.files.close()
        self.session.display_ended(self, error)

    def _packet(self, payload, now):
        if len(payload) < 8:
            raise ProtocolError("failed to get frame PTS: packet is shorter than 8 bytes")
        pts, = struct.unpack_from("<Q", payload)
        unit = payload[8:]
        if not unit:
            raise ProtocolError("empty H.264 input is not a frame; use finish() to drain")
        if len(unit) > MAX_ACCESS_UNIT:
            raise ProtocolError(f"H.264 access unit has {len(unit)} bytes, decoder capacity is "
                                f"{MAX_ACCESS_UNIT}; do not split access units")
        types = nal_types(unit)
        if not types:
            self.session.warn_once(f"display {self.number}: packet without an Annex-B start code; "
                                   "the decoder expects complete Annex-B access units")
        if self.packets == 0:
            self.first_packet_at = now
            LOG.info("display %d: first packet %.0f ms after connecting: %s", self.number,
                     (now - self.connected_at) * 1000, nal_names(types))
        self._check_first_frame(types)
        if self.last_pts is not None and pts < self.last_pts:
            self.backwards += 1
            self.session.warn_once(f"display {self.number}: PTS went backwards ({self.last_pts} → {pts} µs)")
        if self.last_packet_at is not None:
            self.max_gap = max(self.max_gap, now - self.last_packet_at)
        if self.first_pts is None:
            self.first_pts = pts
        self.last_pts, self.last_packet_at = pts, now
        self.packets += 1
        self.bytes += len(unit)
        self.keyframes += 5 in types
        self.fps.add(1, now)
        self.bitrate.add(len(unit) * 8, now)
        if self.files:
            self.files.write(now - self.session.started, pts, unit, types)
        if self.decoder:
            self.decoder.push(unit)

    def _check_first_frame(self, types):
        slices = [i for i, t in enumerate(types) if t in (1, 5)]
        if not self.first_frame_checked and slices:
            self.first_frame_checked = True
            first = slices[0]
            headers = self.headers | set(types[:first])
            if types[first] != 5:
                self.session.warn_once(f"display {self.number}: the first frame is not an IDR; "
                                       "nothing can be decoded until a keyframe arrives")
            elif not {7, 8} <= headers:
                self.session.warn_once(f"display {self.number}: the first IDR arrived without a "
                                       "preceding SPS and PPS; the decoder cannot start from it")
        self.headers |= {t for t in types if t in (7, 8)}

    def decoded(self, size):
        problem = frame_problem(*size, self.crop, self.session.receiver.display)
        self.frame_problem = problem
        if problem:
            self.session.error(f"display {self.number}: carplayd fails every frame: {problem}")
        else:
            LOG.info("display %d: decoded %dx%d; carplayd keeps %s", self.number, *size,
                     self.crop or "the whole frame")

    def summary(self, now):
        if self.connected_at is None:
            return f"display {self.number}: {self.state}"
        if not self.packets:
            return f"display {self.number}: connected {now - self.connected_at:.0f} s ago, no video yet"
        text = (f"video {self.fps.rate(now):.1f} fps, {self.bitrate.rate(now) / 1000:.0f} kbit/s, "
                f"{self.packets} frames ({self.keyframes} IDR)")
        decoder = self.decoder
        if decoder and decoder.size:
            text += f", decoded {decoder.frames} at {decoder.size[0]}x{decoder.size[1]}"
        idle = now - self.last_packet_at
        if idle > 1:
            text += f", last frame {idle:.0f} s ago"
        return text

    def close(self):
        self.closed.set()
        if self.socket:
            close_socket(self.socket)


class AudioStream:
    """One audio_start: its data connection, received PCM and carplayd's playback of it."""

    CONSEQUENCES = {
        "reset": "carplayd resets its converter and discards buffered audio",
        "stale": "carplayd drops the oldest PCM (\"dropped stale playback PCM\")",
        "underrun": "carplayd played silence in the gap, then rebuffered",
    }

    def __init__(self, session, request):
        self.session, self.request = session, request
        self.label = f"audio {request.id}"
        self.listener = session.receiver.listen()
        self.port = self.listener.getsockname()[1]
        self.deadline = time.monotonic() + CONNECT_TIMEOUT
        self.closed = threading.Event()
        # carplayd stops consuming a suspended stream; the provider sees backpressure.
        self.running = threading.Event()
        self.running.set()
        self.suspended = False
        self.connected = False
        self.socket = self.player = self.wav = self.wav_file = None
        self.packets = self.frames = self.flushes = 0
        self.microphone_sent = self.microphone_consumed = 0
        self.started_at = time.monotonic()
        self.rate = RateMeter()
        self.reported = set()
        # The local sink plays the modelled output, so the receiver sounds like the car.
        self.model_lock = threading.Lock()
        self.model = PlaybackModel(request.playback, request.playback_clock == "live",
                                   self._play, self._model_event)

    @property
    def discontinuities(self):
        return self.model.resets

    def start(self):
        self.session.spawn(self._run, self.label)
        self.session.spawn(self._playout, f"{self.label} playout")

    def start_playback(self):
        with self.model_lock:
            self.model.start(time.monotonic())

    def suspend(self, suspended):
        self.suspended = suspended
        with self.model_lock:
            if suspended:
                self.model.stop()
            else:
                self.model.start(time.monotonic())
        if suspended:
            self.running.clear()
        else:
            self.running.set()

    def flush(self):
        self.flushes += 1
        with self.model_lock:
            self.model.flush()

    def _play(self, data):
        if self.player:
            self.player.push(data)

    def _model_event(self, kind, detail):
        LOG.debug("%s: %s", self.label, detail)
        if kind not in self.reported:
            self.reported.add(kind)
            self.session.warn_once(f"{self.label}: {detail}; {self.CONSEQUENCES[kind]}")

    def _playout(self):
        while not self.closed.wait(0.005):
            with self.model_lock:
                self.model.advance(time.monotonic())

    def _accept(self):
        while not self.closed.is_set():
            remaining = self.deadline - time.monotonic()
            if remaining <= 0:
                raise ProtocolError("audio connection timed out")
            self.listener.settimeout(min(remaining, 0.1))
            try:
                return self.listener.accept()
            except socket.timeout:
                pass
        return None

    def _run(self):
        error = None
        sock = None
        try:
            try:
                accepted = self._accept()
            finally:
                self.listener.close()
            if accepted is None:
                return
            sock, address = accepted
            self.socket = sock
            if self.closed.is_set():
                return
            if address[0] != self.session.peer:
                raise ProtocolError("audio connection peer does not match control peer")
            sock.settimeout(None)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            self.connected = True
            self.session.receiver.notify()
            player = self.session.receiver.make_player(self)
            with self.model_lock:
                self.player = player
            self.wav = self.session.open_wav(self)
            playback = self.request.playback
            while not self.closed.is_set():
                if not self.running.wait(0.1):
                    continue
                # Demand-driven streams are only read while carplayd holds under 50 ms.
                with self.model_lock:
                    wanted = self.model.wants_data()
                if not wanted:
                    self.closed.wait(0.002)
                    continue
                payload = recv_frame(sock, playback.max_packet)
                if payload is None:
                    break
                self._packet(payload, time.monotonic())
        except Exception as exc:
            if not self.closed.is_set():
                error = exc
        finally:
            if sock:
                close_socket(sock)
            with self.model_lock:
                player, self.player = self.player, None
            if player:
                player.stop()
            if self.wav:
                self.wav.close()
                self.wav_file.close()
        self.session.audio_finished(self, error)

    def _packet(self, payload, now):
        if len(payload) < 8:
            raise ProtocolError("missing PCM frame position")
        position, = struct.unpack_from("<Q", payload)
        data = payload[8:]
        frames, remainder = divmod(len(data), self.request.playback.frame_bytes)
        if remainder:
            raise ProtocolError("partial PCM frame")
        if not frames:
            raise ProtocolError("empty PCM packet")
        if position + frames > U64:
            raise ProtocolError("PCM frame position overflow")
        self.packets += 1
        self.frames += frames
        self.rate.add(frames, now)
        with self.model_lock:
            self.model.push(position, data, now)
        if self.wav:
            # writeframes() also rewrites the header lengths, so the file stays playable.
            self.wav.writeframes(pcm_to_s16(data, self.request.playback.sample_type))
            self.wav_file.flush()

    def state(self):
        if not self.connected:
            return "waiting for connection"
        return "suspended" if self.suspended else "active"

    def summary(self, now):
        request = self.request
        text = f"{self.label} {request.kind}/{request.audio_type} {self.state()}"
        if self.packets:
            model = self.model
            text += (f": {self.rate.rate(now) / 1000:.2f} of {request.playback.sample_rate / 1000:g} kHz, "
                     f"{self.packets} packets; carplayd queue {model.queued * 1000:.0f} ms, "
                     f"{model.resets} resets, {model.stale_drops} stale drops, {model.underruns} gaps")
        return text

    def close(self):
        self.closed.set()
        self.running.set()
        if self.socket:
            close_socket(self.socket)


class Session:
    """One provider control connection, handled as CarplayDebugNetworkProvider::run does."""

    QUIET = ("input_event", "now_playing_update")

    def __init__(self, receiver, sock, peer, number):
        self.receiver, self.args = receiver, receiver.args
        self.socket, self.peer, self.number = sock, peer, number
        self.lock = threading.RLock()
        self.writes = queue.Queue()
        self.closed = threading.Event()
        self.started = time.monotonic()
        self.pending_display = self.display = None
        self.displays = 0
        self.audio = {}
        self.now_playing = self.app_state = self.reason = None
        self.now_playing_updates = 0
        self.now_playing_key = None
        self.duck = 1.0
        self.errors = 0
        self.warnings = set()
        self.threads = []
        self.recorder = Recorder(receiver.record_root, number) if receiver.record_root else None
        self.handlers = {
            "display_response": self._display_response,
            "now_playing_update": self._now_playing_update,
            "audio_start": self._audio_start,
            "audio_stop": self._audio_stop,
            "audio_flush": self._audio_flush,
            "audio_duck": self._audio_duck,
            "audio_unduck": self._audio_unduck,
            "audio_app_state": self._audio_app_state,
            "audio_input_progress": self._audio_input_progress,
        }

    def spawn(self, target, name):
        thread = threading.Thread(target=target, name=name, daemon=True)
        with self.lock:
            self.threads.append(thread)
        thread.start()

    def join(self, timeout):
        """Wait for display/audio threads to release sockets, pipelines and recordings."""
        deadline = time.monotonic() + timeout
        with self.lock:
            threads = list(self.threads)
        for thread in threads:
            thread.join(max(0, deadline - time.monotonic()))

    def error(self, text):
        self.errors += 1
        LOG.error("%s", text)
        self.receiver.notify()

    def warn_once(self, text):
        if text not in self.warnings:
            self.warnings.add(text)
            LOG.warning("%s", text)

    def trace(self, arrow, message):
        if self.recorder:
            self.recorder.message(time.monotonic() - self.started, "tx" if arrow == "→" else "rx", message)
        kind = message.get("type") if isinstance(message, dict) else None
        level = logging.DEBUG if kind in self.QUIET else logging.INFO
        if LOG.isEnabledFor(level):
            text = json.dumps(message, ensure_ascii=False, separators=(",", ":"))
            if len(text) > 400 and not self.args.verbose:
                text = text[:400] + "…"
            LOG.log(level, "%s %s", arrow, text)

    def run(self):
        LOG.info("session %d: provider %s connected", self.number, self.peer)
        threading.Thread(target=self._write, name=f"control-{self.number}", daemon=True).start()
        self.send(self.receiver.init_message())
        if not self.args.no_display:
            if self.args.display_delay:
                timer = threading.Timer(self.args.display_delay, self.request_display)
                timer.daemon = True
                timer.start()
            else:
                self.request_display()
        try:
            while not self.closed.is_set():
                payload = recv_frame(self.socket)
                if payload is None:
                    raise EOFError("provider closed the control connection")
                self._receive(payload)
        except Exception as exc:
            self.close(exc)

    def _write(self):
        while True:
            data = self.writes.get()
            if data is None or self.closed.is_set():
                return
            try:
                self.socket.sendall(data)
            except OSError as exc:
                self.close(exc)
                return

    def send(self, message):
        if self.closed.is_set():
            return False
        self.trace("→", message)
        self.writes.put(encode(message))
        return True

    def send_input(self, device, events):
        return self.send(dict(type="input_event", device=device, events=events))

    def audio_status(self, identity, state, wire_format=None, detail=None):
        status = dict(id=identity, state=state)
        if wire_format:
            status["wire_format"] = wire_format
        if detail:
            status["detail"] = detail
        self.send(dict(type="audio_status", status=status))

    def _receive(self, payload):
        try:
            message = json.loads(payload, parse_constant=reject_constant)
        except ValueError as exc:
            raise ProtocolError(f"failed to read provider message: {exc}") from None
        self.trace("←", message)
        kind = message.get("type") if isinstance(message, dict) else None
        audio = isinstance(kind, str) and kind.startswith("audio_")
        handler = self.handlers.get(kind) if isinstance(kind, str) else None
        try:
            if handler is None:
                raise ProtocolError(f"failed to read provider message: unknown message type {kind!r}")
            handler(message)
        except ProtocolError as exc:
            # carplayd answers malformed or refused audio messages with an error status.
            if not audio:
                raise
            identity = message.get("id")
            if identity is None and isinstance(message.get("request"), dict):
                identity = message["request"].get("id")
            self.error(f"{kind}: {exc}")
            self.audio_status(identity if type(identity) is int and 0 <= identity <= U64 else 0,
                              "error", detail=str(exc))

    def _display_response(self, message):
        if "safe_area" not in message:
            raise ProtocolError("failed to read provider message: missing field `safe_area`")
        crop = parse_crop(message["safe_area"])
        with self.lock:
            pending = self.pending_display
        if pending is None:
            raise ProtocolError("unsolicited display response")
        pending.respond(crop)

    def _now_playing_update(self, message):
        snapshot = validate_now_playing(message.get("now_playing"))
        self.now_playing = snapshot
        self.now_playing_updates += 1
        key = describe_now_playing(snapshot)
        if key != self.now_playing_key:
            self.now_playing_key = key
            LOG.info("now playing: %s", key)
        self.receiver.notify()

    def _audio_start(self, message):
        request = parse_audio_request(message.get("request"))
        with self.lock:
            if request.id in self.audio:
                raise ProtocolError("audio request ID is already active")
        validate_audio_request(request)
        stream = AudioStream(self, request)
        with self.lock:
            replaced = [s for s in self.audio.values() if s.request.kind == request.kind]
            self.audio[request.id] = stream
        self.send(dict(type="audio_request", id=request.id, port=stream.port))
        stream.start()
        LOG.info("%s: %s %s, %s, %s clock", stream.label, request.kind, request.audio_type,
                 request.playback, request.playback_clock)
        if request.microphone:
            self.warn_once("microphone input is not emulated; no microphone PCM is sent")
        for previous in replaced:
            self.stop_audio(previous, "replaced by a new request")
        if request.kind == "main" and self.receiver.head_unit_audio:
            stream.suspend(True)
            self.audio_status(request.id, "suspended", WIRE_FORMAT, "main audio belongs to the accessory")
        else:
            stream.start_playback()
            self.audio_status(request.id, "active", WIRE_FORMAT)
        self.receiver.notify()

    def _lookup(self, identity):
        with self.lock:
            stream = self.audio.get(identity)
        if stream is None:
            raise ProtocolError("no audio stream with that ID")
        return stream

    def _audio_stop(self, message):
        identity = unsigned(message.get("id"), U64, "id")
        with self.lock:
            stream = self.audio.get(identity)
        if stream:
            self.stop_audio(stream, "stopped by the provider")
        else:
            self.audio_status(identity, "stopped")

    def _audio_flush(self, message):
        stream = self._lookup(unsigned(message.get("id"), U64, "id"))
        stream.flush()
        LOG.info("%s: flush %d", stream.label, stream.flushes)

    def _audio_duck(self, message):
        identity = unsigned(message.get("id"), U64, "id")
        duration = unsigned(message.get("duration_ms"), U32, "duration_ms")
        volume = message.get("volume")
        if type(volume) not in (int, float):
            raise ProtocolError(f"volume must be a number, got {volume!r}")
        if not (math.isfinite(volume) and 0 <= volume <= 1 and duration <= 10_000):
            raise ProtocolError("invalid duck parameters")
        stream = self._lookup(identity)
        LOG.info("%s: duck main audio to %.0f%% over %d ms", stream.label, volume * 100, duration)
        self.set_duck(volume)

    def _audio_unduck(self, message):
        identity = unsigned(message.get("id"), U64, "id")
        duration = unsigned(message.get("duration_ms"), U32, "duration_ms")
        if duration > 10_000:
            raise ProtocolError("invalid unduck duration")
        stream = self._lookup(identity)
        LOG.info("%s: unduck main audio over %d ms", stream.label, duration)
        self.set_duck(1.0)

    def _audio_app_state(self, message):
        state = dict(speech=choice(message.get("speech"), SPEECH, "speech"),
                     phone_call=boolean(message.get("phone_call"), "phone_call"),
                     turn_by_turn=boolean(message.get("turn_by_turn"), "turn_by_turn"))
        if (state["speech"] != "none" or state["phone_call"]) and not self.owns_main_audio():
            raise ProtocolError("speech/call state requires owned main audio")
        if state != self.app_state:
            LOG.info("app state: speech %s, phone call %s, turn-by-turn %s", state["speech"],
                     state["phone_call"], state["turn_by_turn"])
        self.app_state = state
        self.receiver.notify()

    def _audio_input_progress(self, message):
        identity = unsigned(message.get("id"), U64, "id")
        consumed = unsigned(message.get("consumed"), U64, "consumed")
        stream = self._lookup(identity)
        if consumed > stream.microphone_sent:
            raise ProtocolError("microphone progress exceeds sent frames")
        if consumed < stream.microphone_consumed:
            raise ProtocolError("microphone progress went backwards")
        stream.microphone_consumed = consumed

    def owns_main_audio(self):
        with self.lock:
            return any(s.request.kind == "main" and not s.suspended for s in self.audio.values())

    def set_duck(self, volume):
        self.duck = volume
        with self.lock:
            players = [s.player for s in self.audio.values() if s.request.kind == "main" and s.player]
        for player in players:
            player.set_volume(volume)

    def set_head_unit_audio(self, owned):
        with self.lock:
            streams = [s for s in self.audio.values() if s.request.kind == "main" and s.suspended != owned]
        for stream in streams:
            stream.suspend(owned)
            if owned:
                self.audio_status(stream.request.id, "suspended", WIRE_FORMAT,
                                  "main audio belongs to the accessory")
            else:
                self.audio_status(stream.request.id, "active", WIRE_FORMAT)
        self.receiver.notify()

    def stop_audio(self, stream, why):
        with self.lock:
            if self.audio.get(stream.request.id) is not stream:
                return
            del self.audio[stream.request.id]
        stream.close()
        LOG.info("%s: %s after %d packets", stream.label, why, stream.packets)
        self.audio_status(stream.request.id, "stopped")
        self.receiver.notify()

    def audio_finished(self, stream, error):
        with self.lock:
            if self.audio.get(stream.request.id) is not stream:
                return
            del self.audio[stream.request.id]
        if error:
            self.error(f"{stream.label} failed: {error}")
            self.audio_status(stream.request.id, "error", detail=str(error))
        else:
            LOG.info("%s: provider closed the stream after %d packets", stream.label, stream.packets)
            self.audio_status(stream.request.id, "stopped")
        self.receiver.notify()

    def open_wav(self, stream):
        if not self.recorder:
            return None
        request = stream.request
        stream.wav_file = open(self.recorder.path(f"audio-{request.id}-{request.kind}-{request.audio_type}.wav"), "wb")
        output = wave.open(stream.wav_file, "wb")
        output.setnchannels(request.playback.channels)
        output.setsampwidth(2)
        output.setframerate(request.playback.sample_rate)
        return output

    def request_display(self):
        with self.lock:
            if self.closed.is_set():
                return
            if self.pending_display:
                LOG.warning("display %d is still pending; not requesting another", self.pending_display.number)
                return
            current, self.display = self.display, None
            self.displays += 1
            pending = self.pending_display = DisplayStream(self, self.displays)
        if current:
            current.close()
        pending.start()
        self.send(dict(type="display_request", port=pending.port))
        self.receiver.notify()

    def close_display(self):
        with self.lock:
            displays = [d for d in (self.pending_display, self.display) if d]
        for display in displays:
            display.close()

    def display_connected(self, display):
        with self.lock:
            if self.pending_display is display:
                self.pending_display, self.display = None, display
        LOG.info("display %d: connected after %.0f ms", display.number,
                 (display.connected_at - display.requested_at) * 1000)
        self.receiver.notify()

    def display_failed(self, display, error):
        with self.lock:
            if self.pending_display is display:
                self.pending_display = None
        if not display.closed.is_set():
            self.error(f"display {display.number}: request failed: {error}")
        self.receiver.notify()

    def display_ended(self, display, error):
        with self.lock:
            if self.display is display:
                self.display = None
        if error:
            self.error(f"display {display.number}: stream failed after {display.packets} packets: {error}")
        elif display.closed.is_set():
            LOG.info("display %d: closed by the receiver after %d packets", display.number, display.packets)
        else:
            LOG.info("display %d: provider closed the stream after %d packets", display.number, display.packets)
        self.receiver.notify()

    def summary(self):
        now = time.monotonic()
        with self.lock:
            display = self.display or self.pending_display
            streams = list(self.audio.values())
        parts = [display.summary(now) if display else "no display"]
        parts += [stream.summary(now) for stream in streams]
        return "; ".join(parts)

    def close(self, reason=None):
        with self.lock:
            if self.closed.is_set():
                return
            self.closed.set()
            self.reason = reason
            displays = [d for d in (self.pending_display, self.display) if d]
            streams = list(self.audio.values())
            self.pending_display = self.display = None
            self.audio.clear()
        if isinstance(reason, ProtocolError):
            self.error(f"session {self.number}: carplayd would end this provider session: {reason}")
        elif isinstance(reason, (EOFError, ReceiverClosed)):
            LOG.info("session %d: %s", self.number, reason)
        elif reason is not None:
            self.error(f"session {self.number}: control connection failed: {reason}")
        for display in displays:
            display.close()
        for stream in streams:
            stream.close()
        self.writes.put(None)
        close_socket(self.socket)
        if self.recorder:
            self.recorder.close()
            LOG.info("session %d recorded in %s", self.number, self.recorder.directory)
        LOG.info("session %d ended after %.1f s with %d errors and %d warnings", self.number,
                 time.monotonic() - self.started, self.errors, len(self.warnings))
        self.receiver.notify()


class Receiver:
    """carplayd's provider listener: one provider session at a time."""

    def __init__(self, args, notify=None):
        self.args = args
        self.notify = notify or (lambda: None)
        self.display = DisplayParams(*args.size, args.fps)
        self.input_devices = (load_input_devices(args.input_devices) if args.input_devices
                              else default_input_devices(args.touch, args.knob))
        self.record_root = None
        if args.record:
            # A fresh directory per run, so earlier recordings are never overwritten.
            stamp = time.strftime("%Y%m%d-%H%M%S")
            for attempt in range(1, 100):
                self.record_root = os.path.join(args.record, stamp if attempt == 1 else f"{stamp}-{attempt}")
                try:
                    os.makedirs(self.record_root)
                    break
                except FileExistsError:
                    continue
            LOG.info("recording sessions under %s", self.record_root)
        self.gui = False
        self.session = None
        self.sessions = self.errors = 0
        self.head_unit_audio = False
        self.stopped = threading.Event()
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind((args.bind, args.port))
        self.listener.listen(1)
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]

    def init_message(self):
        return dict(type="init", display_width=self.display.width, display_height=self.display.height,
                    display_fps=self.display.fps, input_devices=self.input_devices, audio=AUDIO_CAPABILITIES)

    def listen(self):
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.bind((self.args.bind, 0))
        listener.listen(1)
        return listener

    def serve(self):
        host = "127.0.0.1" if self.args.bind in ("0.0.0.0", "127.0.0.1") else self.args.bind
        LOG.info("listening on %s:%d as a %dx%d %d fps display; start the provider, for example: "
                 "aa-debug-bridge %s --port %d", self.args.bind, self.port, self.display.width,
                 self.display.height, self.display.fps, host, self.port)
        while not self.stopped.is_set():
            try:
                sock, address = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                if self.stopped.is_set():
                    break
                raise
            sock.settimeout(None)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            self.sessions += 1
            session = Session(self, sock, address[0], self.sessions)
            self.session = session
            self.notify()
            session.run()
            session.join(3)
            self.errors += session.errors
            self.session = None
            self.notify()
            if self.args.once:
                break
        self.listener.close()

    def stats(self):
        while not self.stopped.wait(self.args.stats):
            session = self.session
            if session and not session.closed.is_set():
                LOG.info("stats: %s", session.summary())

    def make_decoder(self, display):
        if self.args.no_decode:
            return None
        try:
            return VideoDecoder(self.gui, display.decoded,
                                lambda level, text: self._media_message(display.session, f"display {display.number} decoder", level, text))
        except Exception as exc:
            display.session.error(f"display {display.number}: could not start the H.264 decoder: {exc}")
            return None

    def make_player(self, stream):
        if self.args.no_audio:
            return None
        session = stream.session
        try:
            return AudioPlayer(stream.request.playback, self.args.audio_sink,
                               session.duck if stream.request.kind == "main" else 1.0,
                               lambda level, text: self._media_message(session, f"{stream.label} playback", level, text))
        except Exception as exc:
            session.error(f"{stream.label}: could not start playback: {exc}")
            return None

    @staticmethod
    def _media_message(session, source, level, text):
        # A failing pipeline repeats its message for every buffer; report each once.
        message = f"{source}: {text}"
        if message not in session.warnings:
            session.warnings.add(message)
            session.errors += level >= logging.ERROR
            LOG.log(level, "%s", message)

    def key_device(self, key):
        for device in self.input_devices:
            keys = IAP_REMOTE_KEYS if device == "IapRemote" else device["Network"]["keys"]
            if key in keys:
                return device
        return None

    def touch_device(self):
        return next((d for d in self.input_devices if d != "IapRemote" and d["Network"]["touch"]), None)

    def knob(self):
        for device in self.input_devices:
            if device != "IapRemote":
                for axis in device["Network"]["axes"]:
                    if axis["axis"] in ROTARY_AXES:
                        return device, axis
        return None, None

    def set_head_unit_audio(self, owned):
        self.head_unit_audio = owned
        LOG.info("head unit %s main audio", "takes" if owned else "returns")
        session = self.session
        if session:
            session.set_head_unit_audio(owned)

    def stop(self):
        if self.stopped.is_set():
            return
        self.stopped.set()
        session = self.session
        if session:
            session.close(ReceiverClosed("receiver stopped"))


class LogHandler(logging.Handler):
    def __init__(self, window):
        super().__init__()
        self.window = window
        self.setFormatter(logging.Formatter("%(asctime)s %(message)s", "%H:%M:%S"))

    def emit(self, record):
        try:
            text = self.format(record)
        except Exception:
            return
        GLib.idle_add(self.window.append_log, text, record.levelno)


class ReceiverWindow:
    """The display as carplayd crops it, input controls, audio, Now Playing and the log."""

    KEY_BUTTONS = (("Home", "Home"), ("Back", "Back"), ("Primary", "Select"), ("NavGuidance", "Nav"),
                   ("PreviousTrack", "⏮"), ("PlayPause", "⏯"), ("NextTrack", "⏭"), ("Mute", "Mute"),
                   ("VolumeDown", "Vol −"), ("VolumeUp", "Vol +"))

    def __init__(self, receiver):
        self.receiver = receiver
        receiver.gui = True
        receiver.notify = self._notify
        self.notified = threading.Event()
        self.stopping = False
        self.pixbuf = self.frame = self.contact = self.last_display = self.session_seen = None
        self.scroll = 0.0
        self.knob_position = 0
        self.held = set()
        self.shortcuts = {
            Gdk.KEY_Return: "Primary", Gdk.KEY_KP_Enter: "Primary", Gdk.KEY_Escape: "Back",
            Gdk.KEY_BackSpace: "Back", Gdk.KEY_h: "Home", Gdk.KEY_Home: "Home",
            Gdk.KEY_space: "PlayPause", Gdk.KEY_n: "NextTrack", Gdk.KEY_p: "PreviousTrack",
            Gdk.KEY_g: "NavGuidance",
        }
        d = receiver.display
        self.window = Gtk.Window(title=f"carplayd debug receiver — port {receiver.port}")
        self.window.set_default_size(1440, 820)
        self.window.connect("delete-event", lambda *_: self.stop())
        root = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        root.set_border_width(10)

        display_frame = Gtk.Frame(label=f"Display — {d.width}×{d.height} @ {d.fps} fps, as carplayd crops it")
        display_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        display_box.set_border_width(8)
        self.drawing = Gtk.DrawingArea()
        self.drawing.set_size_request(540, 340)
        self.drawing.set_can_focus(True)
        self.drawing.add_events(
            Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.BUTTON_RELEASE_MASK
            | Gdk.EventMask.POINTER_MOTION_MASK | Gdk.EventMask.SCROLL_MASK
            | Gdk.EventMask.SMOOTH_SCROLL_MASK | Gdk.EventMask.KEY_PRESS_MASK
            | Gdk.EventMask.KEY_RELEASE_MASK)
        for signal_name, handler in (("draw", self._draw), ("button-press-event", self._press),
                                     ("button-release-event", self._release),
                                     ("motion-notify-event", self._motion), ("scroll-event", self._scroll),
                                     ("key-press-event", self._key_press),
                                     ("key-release-event", self._key_release)):
            self.drawing.connect(signal_name, handler)
        display_box.pack_start(self.drawing, True, True, 0)
        self.video_info = self._label("")
        display_box.pack_start(self.video_info, False, False, 0)
        controls = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.full = Gtk.CheckButton(label="Show full source and crop")
        self.full.connect("toggled", lambda *_: self.drawing.queue_draw())
        controls.pack_start(self.full, False, False, 0)
        for label, action in (("Disconnect provider", self._disconnect), ("Close display", self._close_display),
                              ("Request display", self._request_display)):
            button = Gtk.Button(label=label)
            button.connect("clicked", lambda _, action=action: action())
            controls.pack_end(button, False, False, 0)
        display_box.pack_start(controls, False, False, 0)
        display_frame.add(display_box)
        root.pack_start(display_frame, True, True, 0)

        side = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        side.set_size_request(420, -1)
        self.status = self._label("")
        side.pack_start(self.status, False, False, 0)

        input_frame = Gtk.Frame(label="Input")
        input_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        input_box.set_border_width(8)
        grid = Gtk.Grid(column_spacing=4, row_spacing=4, column_homogeneous=True)
        for index, (key, label) in enumerate(self.KEY_BUTTONS):
            button = Gtk.Button(label=label)
            button.set_tooltip_text(key)
            button.set_sensitive(receiver.key_device(key) is not None)
            button.connect("pressed", lambda _, key=key: self._key(key, True))
            button.connect("released", lambda _, key=key: self._key(key, False))
            grid.attach(button, index % 5, index // 5, 1, 1)
        knob = receiver.knob()[0] is not None
        for column, (label, delta) in enumerate((("Knob ◀", -1), ("Knob ▶", 1))):
            button = Gtk.Button(label=label)
            button.set_sensitive(knob)
            button.connect("clicked", lambda _, delta=delta: self._rotate(delta))
            grid.attach(button, column, 2, 1, 1)
        input_box.pack_start(grid, False, False, 0)
        devices = []
        for device in receiver.input_devices:
            if device == "IapRemote":
                devices.append("IapRemote (media keys)")
            else:
                info = device["Network"]
                parts = [next(iter(info["touch"]))] if info["touch"] else []
                parts += [f"{a['axis']} axis" for a in info["axes"]]
                parts += [f"keys {', '.join(info['keys'])}"] if info["keys"] else []
                devices.append(f"{info['id']}: {'; '.join(parts) or 'no capabilities'}")
        input_box.pack_start(self._label("Advertised: " + " · ".join(devices)), False, False, 0)
        input_box.pack_start(self._label(
            "Click/drag the display to touch · wheel or ←/→: knob · Enter: Select · Esc/Backspace: Back · "
            "H: Home · G: Nav · Space: play/pause · N/P: next/previous · Ctrl+Q: quit"), False, False, 0)
        input_frame.add(input_box)
        side.pack_start(input_frame, False, False, 0)

        audio_frame = Gtk.Frame(label="Audio")
        audio_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        audio_box.set_border_width(8)
        self.audio_info = self._label("")
        audio_box.pack_start(self.audio_info, False, False, 0)
        head_unit = Gtk.ToggleButton(label="Head unit takes main audio")
        head_unit.set_tooltip_text("Suspend main audio as carplayd does when the car's own source plays")
        head_unit.connect("toggled", lambda button: receiver.set_head_unit_audio(button.get_active()))
        audio_box.pack_start(head_unit, False, False, 0)
        audio_frame.add(audio_box)
        side.pack_start(audio_frame, False, False, 0)

        media_frame = Gtk.Frame(label="Now Playing")
        self.media_info = self._label("")
        self.media_info.set_margin_start(8)
        self.media_info.set_margin_end(8)
        self.media_info.set_margin_bottom(8)
        media_frame.add(self.media_info)
        side.pack_start(media_frame, False, False, 0)

        log_frame = Gtk.Frame(label="Log")
        scroll = Gtk.ScrolledWindow()
        scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        self.log = Gtk.TextView()
        self.log.set_editable(False)
        self.log.set_cursor_visible(False)
        self.log.set_monospace(True)
        self.log.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        self.log_buffer = self.log.get_buffer()
        self.log_buffer.create_tag("error", foreground="#e5484d")
        self.log_buffer.create_tag("warning", foreground="#d97706")
        self.log_end = self.log_buffer.create_mark(None, self.log_buffer.get_end_iter(), False)
        scroll.add(self.log)
        log_frame.add(scroll)
        side.pack_start(log_frame, True, True, 0)
        root.pack_start(side, False, True, 0)
        self.window.add(root)
        self.window.connect("key-press-event", self._window_key)
        logging.getLogger().addHandler(LogHandler(self))

    @staticmethod
    def _label(text):
        label = Gtk.Label(label=text)
        label.set_xalign(0)
        label.set_selectable(True)
        label.set_line_wrap(True)
        # Wrapped labels otherwise request their unwrapped width and squeeze the display.
        label.set_max_width_chars(56)
        return label

    def append_log(self, text, level):
        end = self.log_buffer.get_end_iter()
        tags = ["error"] if level >= logging.ERROR else ["warning"] if level >= logging.WARNING else []
        self.log_buffer.insert_with_tags_by_name(end, text + "\n", *tags)
        if self.log_buffer.get_line_count() > 2000:
            self.log_buffer.delete(self.log_buffer.get_start_iter(), self.log_buffer.get_iter_at_line(200))
        self.log.scroll_mark_onscreen(self.log_end)
        return False

    def _notify(self):
        if not self.notified.is_set():
            self.notified.set()
            GLib.idle_add(self._refresh)

    def _display(self):
        session = self.receiver.session
        display = session and (session.display or session.pending_display)
        if display:
            self.last_display = display
        return self.last_display

    def _geometry(self):
        """Shown source region, its widget transform and the display surface within the source."""
        d = self.receiver.display
        display = self._display()
        crop = display.crop if display else None
        surface = (crop.left, crop.top, crop.width, crop.height) if crop else (0, 0, d.width, d.height)
        if self.full.get_active() and self.pixbuf:
            region = (0, 0, self.pixbuf.get_width(), self.pixbuf.get_height())
        else:
            region = surface
        width = max(1, self.drawing.get_allocated_width())
        height = max(1, self.drawing.get_allocated_height())
        scale = min(width / region[2], height / region[3])
        return region, scale, (width - region[2] * scale) / 2, (height - region[3] * scale) / 2, surface

    def _surface_point(self, x, y, clamp):
        region, scale, left, top, surface = self._geometry()
        u = (region[0] + (x - left) / scale - surface[0]) / surface[2]
        v = (region[1] + (y - top) / scale - surface[1]) / surface[3]
        if not clamp and not (0 <= u <= 1 and 0 <= v <= 1):
            return None
        return min(1.0, max(0.0, u)), min(1.0, max(0.0, v))

    def _draw(self, widget, context):
        context.set_source_rgb(0, 0, 0)
        context.paint()
        region, scale, left, top, surface = self._geometry()
        context.save()
        context.translate(left, top)
        context.scale(scale, scale)
        context.rectangle(0, 0, region[2], region[3])
        context.clip()
        if self.pixbuf:
            image, x, y = self.pixbuf, -region[0], -region[1]
            width, height = image.get_width(), image.get_height()
            if region != (0, 0, width, height) and region[0] + region[2] <= width and region[1] + region[3] <= height:
                # Sample only retained pixels, so scaling cannot blend in the discarded margins.
                image, x, y = image.new_subpixbuf(*region), 0, 0
            Gdk.cairo_set_source_pixbuf(context, image, x, y)
            context.get_source().set_extend(cairo.EXTEND_PAD)
        else:
            context.set_source_rgb(0.1, 0.11, 0.13)
        context.paint()
        sx, sy = surface[0] - region[0], surface[1] - region[1]
        if region != surface:
            # Dim what carplayd discards and outline the retained safe area.
            context.set_fill_rule(cairo.FILL_RULE_EVEN_ODD)
            context.rectangle(0, 0, region[2], region[3])
            context.rectangle(sx, sy, surface[2], surface[3])
            context.set_source_rgba(0, 0, 0, 0.55)
            context.fill()
            context.set_source_rgb(1, 0.82, 0.2)
            context.set_line_width(2 / scale)
            context.rectangle(sx, sy, surface[2], surface[3])
            context.stroke()
        if self.contact:
            context.set_source_rgba(0.25, 0.75, 1.0, 0.75)
            context.arc(sx + self.contact[0] * surface[2], sy + self.contact[1] * surface[3],
                        14 / scale, 0, 2 * math.pi)
            context.fill()
        context.restore()
        text, problem = self._overlay()
        if text:
            context.set_font_size(15)
            extents = context.text_extents(text)
            context.set_source_rgba(0, 0, 0, 0.7)
            context.rectangle(12, 12, extents.width + 20, 30)
            context.fill()
            if problem:
                context.set_source_rgb(1, 0.42, 0.42)
            else:
                context.set_source_rgb(0.92, 0.92, 0.92)
            context.move_to(22, 32)
            context.show_text(text)
        return False

    def _overlay(self):
        session = self.receiver.session
        if session is None:
            if self.pixbuf:
                return "Provider disconnected — last frame", False
            return f"Waiting for a provider on port {self.receiver.port}", False
        display = session.display or session.pending_display
        if display is None:
            return ("Display closed — last frame" if self.pixbuf else "No display requested"), False
        if display.frame_problem:
            return f"carplayd rejects this frame: {display.frame_problem}", True
        if display.connected_at is None:
            return display.state.capitalize(), False
        if not display.packets:
            return "Display connected — waiting for video", False
        if not self.pixbuf:
            return ("Receiving video (decoding disabled)" if self.receiver.args.no_decode
                    else "Receiving video — waiting for a decodable frame"), False
        return None, False

    def _press(self, widget, event):
        widget.grab_focus()
        if event.button != 1 or event.type != Gdk.EventType.BUTTON_PRESS:
            return True
        point = self._surface_point(event.x, event.y, clamp=False)
        if point:
            self.contact = point
            self._touch(True)
        return True

    def _motion(self, widget, event):
        if self.contact:
            self.contact = self._surface_point(event.x, event.y, clamp=True)
            self._touch(True)
        return True

    def _release(self, widget, event):
        if event.button == 1 and self.contact:
            self.contact = self._surface_point(event.x, event.y, clamp=True)
            self._touch(False)
            self.contact = None
        return True

    def _touch(self, touching):
        self.drawing.queue_draw()
        device = self.receiver.touch_device()
        session = self.receiver.session
        if device is None:
            if session:
                session.warn_once("no touch device is advertised; touches are not sent")
            return
        if session:
            x, y = self.contact
            session.send_input(device, [{"TouchEvent": {"contacts": [
                {"id": 0, "x": x, "y": y, "touching": touching}]}}])

    def _scroll(self, widget, event):
        if event.direction == Gdk.ScrollDirection.SMOOTH:
            self.scroll += event.get_scroll_deltas()[2]
        elif event.direction in (Gdk.ScrollDirection.DOWN, Gdk.ScrollDirection.UP):
            self.scroll += 1 if event.direction == Gdk.ScrollDirection.DOWN else -1
        steps = int(self.scroll)
        if steps:
            self.scroll -= steps
            self._rotate(steps)
        return True

    def _rotate(self, delta):
        device, axis = self.receiver.knob()
        session = self.receiver.session
        if device is None or session is None:
            if session:
                session.warn_once("no rotary axis is advertised; use --knob")
            return
        value = delta
        if not axis["relative"]:
            low, high = axis["logical_minimum"], axis["logical_maximum"]
            self.knob_position = low + (self.knob_position + delta - low) % (high - low + 1)
            value = self.knob_position
        session.send_input(device, [{"Axis": {"id": axis["id"], "value": value}}])

    def _key(self, key, pressed):
        device = self.receiver.key_device(key)
        session = self.receiver.session
        if session and device is not None:
            session.send_input(device, [{"Key": {"key": key, "pressed": pressed}}])

    def _key_press(self, widget, event):
        if event.keyval in (Gdk.KEY_Left, Gdk.KEY_Right):
            self._rotate(-1 if event.keyval == Gdk.KEY_Left else 1)
            return True
        key = self.shortcuts.get(event.keyval)
        if key is None or event.state & Gdk.ModifierType.CONTROL_MASK:
            return False
        if key not in self.held:
            self.held.add(key)
            self._key(key, True)
        return True

    def _key_release(self, widget, event):
        key = self.shortcuts.get(event.keyval)
        if key in self.held:
            self.held.discard(key)
            self._key(key, False)
            return True
        return False

    def _window_key(self, widget, event):
        if event.state & Gdk.ModifierType.CONTROL_MASK and event.keyval in (Gdk.KEY_q, Gdk.KEY_Q):
            self.stop()
            return True
        return False

    def _request_display(self):
        if self.receiver.session:
            self.receiver.session.request_display()

    def _close_display(self):
        if self.receiver.session:
            self.receiver.session.close_display()

    def _disconnect(self):
        if self.receiver.session:
            self.receiver.session.close(ReceiverClosed("disconnected from the receiver window"))

    def _frame(self):
        display = self._display()
        decoder = display.decoder if display else None
        if decoder:
            with decoder.lock:
                sample, serial = decoder.sample, decoder.frames
            if sample is not None and (decoder, serial) != self.frame:
                self.frame = (decoder, serial)
                self.pixbuf = sample_pixbuf(sample)
                self.drawing.queue_draw()
        return not self.stopping

    def _refresh(self):
        self.notified.clear()
        if self.stopping:
            return False
        receiver, session = self.receiver, self.receiver.session
        now = time.monotonic()
        if session is not self.session_seen:
            self.session_seen = session
            if session:
                self.pixbuf = self.frame = self.last_display = None
        if session is None:
            self.status.set_text(f"Listening on {receiver.args.bind}:{receiver.port} · "
                                 f"{receiver.sessions} sessions so far, {receiver.errors} errors")
        else:
            self.status.set_text(f"Session {session.number}: provider {session.peer} · "
                                 f"{now - session.started:.0f} s · {session.errors} errors, "
                                 f"{len(session.warnings)} warnings")
        display = self._display()
        if display is None:
            self.video_info.set_text("No display stream")
        else:
            lines = [display.summary(now)]
            if display.crop:
                lines.append(f"Safe area {display.crop}")
            if display.packets:
                pts_span = (display.last_pts - display.first_pts) / 1e6
                wall = display.last_packet_at - display.first_packet_at
                lines.append(f"PTS span {pts_span:.1f} s over {wall:.1f} s received · max gap "
                             f"{display.max_gap * 1000:.0f} ms · {display.backwards} backwards PTS")
            self.video_info.set_text("\n".join(lines))
        streams = list(session.audio.values()) if session else []
        lines = [stream.summary(now) for stream in streams] or ["No audio streams"]
        if session and session.duck != 1.0:
            lines.append(f"Main audio ducked to {session.duck:.0%}")
        if session and session.app_state:
            state = session.app_state
            lines.append(f"App state: speech {state['speech']}, call {state['phone_call']}, "
                         f"turn-by-turn {state['turn_by_turn']}")
        self.audio_info.set_text("\n".join(lines))
        snapshot = session.now_playing if session else None
        if snapshot is None:
            self.media_info.set_text("No media source")
        else:
            item = snapshot["media_item"]

            def clock(milliseconds):
                seconds = (milliseconds or 0) // 1000
                return f"{seconds // 60}:{seconds % 60:02d}"

            self.media_info.set_text(
                f"{snapshot.get('status') or '—'} · {snapshot.get('app_name') or 'unknown app'}\n"
                f"{item.get('title') or 'Untitled'}\n"
                f"{item.get('artist') or 'Unknown artist'} · {item.get('album_title') or 'Unknown album'}\n"
                f"{clock(snapshot.get('elapsed_time'))} / {clock(item.get('playback_duration'))} · "
                f"shuffle {snapshot.get('shuffle_mode') or '—'} · repeat {snapshot.get('repeat_mode') or '—'} · "
                f"{session.now_playing_updates} updates")
        self.drawing.queue_draw()
        return False

    def _tick(self):
        self._refresh()
        return not self.stopping

    def stop(self):
        if not self.stopping:
            self.stopping = True
            self.receiver.stop()
            Gtk.main_quit()
        return False

    def run(self):
        self.window.show_all()
        self.drawing.grab_focus()
        GLib.timeout_add(16, self._frame)
        GLib.timeout_add(500, self._tick)
        for signum in (signal.SIGINT, signal.SIGTERM):
            unix_signal_add(GLib.PRIORITY_DEFAULT, signum, self.stop)
        serving = threading.Thread(target=self._serve, name="receiver", daemon=True)
        serving.start()
        threading.Thread(target=self.receiver.stats, name="stats", daemon=True).start()
        Gtk.main()
        # Let the session close its streams and finish any recording before exiting.
        serving.join(4)
        return 1 if self.receiver.errors else 0

    def _serve(self):
        try:
            self.receiver.serve()
        except Exception as exc:
            LOG.error("receiver failed: %s", exc)
            GLib.idle_add(self.stop)


def sample_pixbuf(sample):
    buffer = sample.get_buffer()
    info = GstVideo.VideoInfo.new_from_caps(sample.get_caps())
    meta = GstVideo.buffer_get_video_meta(buffer)
    offset = meta.offset[0] if meta else info.offset[0]
    stride = meta.stride[0] if meta else info.stride[0]
    data = buffer.extract_dup(offset, buffer.get_size() - offset)
    return GdkPixbuf.Pixbuf.new_from_bytes(GLib.Bytes.new(data), GdkPixbuf.Colorspace.RGB, False, 8,
                                           info.width, info.height, stride)


def load_gi(gui):
    global GLib, Gst, GstVideo, Gtk, Gdk, GdkPixbuf, cairo, unix_signal_add
    import gi

    versions = [("Gst", "1.0"), ("GstApp", "1.0"), ("GstVideo", "1.0")]
    if gui:
        versions += [("Gtk", "3.0"), ("Gdk", "3.0"), ("GdkPixbuf", "2.0")]
    for name, version in versions:
        gi.require_version(name, version)
    from gi.repository import GLib, Gst, GstVideo
    if gui:
        gi.require_foreign("cairo")
        import cairo
        from gi.repository import Gdk, GdkPixbuf, Gtk
        try:
            gi.require_version("GLibUnix", "2.0")
            from gi.repository import GLibUnix
            unix_signal_add = GLibUnix.signal_add
        except (ImportError, ValueError):
            unix_signal_add = GLib.unix_signal_add
    Gst.init(None)


def missing_plugins(names):
    return [name for name in names if Gst.ElementFactory.find(name) is None]


def size_argument(value):
    try:
        width, height = (int(n) for n in value.lower().split("x"))
    except ValueError:
        raise argparse.ArgumentTypeError("expected WIDTHxHEIGHT")
    if not (0 < width <= 8192 and 0 < height <= 8192) or width % 2 or height % 2:
        raise argparse.ArgumentTypeError("display dimensions must be even and at most 8192")
    return width, height


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--bind", default="127.0.0.1",
                        help="listen address; carplayd uses 0.0.0.0 (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=6767, help="control port (default: 6767)")
    parser.add_argument("--size", type=size_argument, default=(1080, 678),
                        help="CarPlay display WIDTHxHEIGHT sent in init (default: 1080x678)")
    parser.add_argument("--fps", type=int, default=30,
                        help="display_fps; carplayd uses the head unit's maxFPS, else 30 (default: 30)")
    parser.add_argument("--touch", choices=("touchscreen", "touchpad", "none"), default="touchscreen",
                        help="touch surface to advertise (default: touchscreen)")
    parser.add_argument("--knob", action="store_true",
                        help="also advertise a rotary knob with Select/Back/Home/Nav keys")
    parser.add_argument("--input-devices", metavar="FILE",
                        help="advertise these carplayd InputDevice values (JSON list or init message)")
    parser.add_argument("--no-display", action="store_true",
                        help="do not send display_request automatically; use Request display")
    parser.add_argument("--display-delay", type=float, default=0,
                        help="seconds between init and display_request (default: 0)")
    parser.add_argument("--headless", action="store_true", help="log to stderr without a window")
    parser.add_argument("--no-decode", action="store_true", help="validate H.264 without decoding it")
    parser.add_argument("--no-audio", action="store_true", help="validate PCM without playing it")
    parser.add_argument("--audio-sink", default="autoaudiosink",
                        help="GStreamer sink description for playback (default: autoaudiosink)")
    parser.add_argument("--record", metavar="DIR",
                        help="write each session's control trace, H.264 and PCM under DIR/TIMESTAMP/")
    parser.add_argument("--once", action="store_true",
                        help="exit after the first provider session; status 1 if it reported errors")
    parser.add_argument("--stats", type=float, default=10, help="stats log interval in seconds (default: 10)")
    parser.add_argument("--verbose", action="store_true", help="log input/metadata messages and full JSON")
    args = parser.parse_args(argv)
    if not 0 <= args.port <= 65535:
        parser.error("port must be 0–65535")
    if not 0 < args.fps <= 240:
        parser.error("fps must be 1–240")
    if not math.isfinite(args.display_delay) or args.display_delay < 0:
        parser.error("display delay must be a nonnegative number of seconds")
    if not math.isfinite(args.stats) or args.stats <= 0:
        parser.error("stats interval must be positive")
    return args


def main(argv=None):
    args = parse_args(argv)
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO,
                        format="%(asctime)s.%(msecs)03d %(levelname)s: %(message)s", datefmt="%H:%M:%S")
    gui = not args.headless
    try:
        if gui or not (args.no_decode and args.no_audio):
            try:
                load_gi(gui)
            except (ImportError, ValueError) as exc:
                if gui:
                    raise RuntimeError(f"install PyGObject, GTK 3, pycairo and GStreamer bindings, or use --headless ({exc})")
                LOG.warning("GStreamer bindings unavailable (%s); decoding and playback are disabled", exc)
                args.no_decode = args.no_audio = True
        if not args.no_decode:
            missing = missing_plugins(["appsrc", "appsink", "h264parse", "avdec_h264"]
                                      + (["videoconvert"] if gui else []))
            if missing:
                LOG.warning("missing GStreamer plugins %s; video is validated but not decoded", ", ".join(missing))
                args.no_decode = True
        if not args.no_audio:
            missing = missing_plugins(["appsrc", "audioconvert", "audioresample", "volume",
                                       args.audio_sink.replace("!", " ").split()[0]])
            if missing:
                LOG.warning("missing GStreamer plugins %s; audio is validated but not played", ", ".join(missing))
                args.no_audio = True
        if gui and not Gtk.init_check()[0]:
            raise RuntimeError("could not open a desktop display; use --headless")
        receiver = Receiver(args)
    except Exception as exc:
        LOG.error("%s", exc)
        return 2
    if gui:
        return ReceiverWindow(receiver).run()
    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, lambda *_: receiver.stop())
    threading.Thread(target=receiver.stats, name="stats", daemon=True).start()
    receiver.serve()
    return 1 if receiver.errors else 0


if __name__ == "__main__":
    raise SystemExit(main())
