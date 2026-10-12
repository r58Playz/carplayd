#!/usr/bin/env python3
"""Stream a KDE virtual monitor to carplayd, preview video and emulate provider input."""

import argparse
from collections import deque
from dataclasses import dataclass, field
import json
import logging
import math
import os
import signal
import socket
import struct
import threading
import time
import uuid


LOG = logging.getLogger("debug-network-client")
# Match tokio_util::codec::LengthDelimitedCodec's default limit.
MAX_PACKET = 8 * 1024 * 1024
# Match VideoDecoderConfig::default().max_access_unit_size.
MAX_ACCESS_UNIT = 4 * 1024 * 1024
PORTAL_NAME = "org.freedesktop.portal.Desktop"
PORTAL_PATH = "/org/freedesktop/portal/desktop"
SCREENCAST = "org.freedesktop.portal.ScreenCast"
REMOTE_DESKTOP = "org.freedesktop.portal.RemoteDesktop"
REQUEST = "org.freedesktop.portal.Request"
SESSION = "org.freedesktop.portal.Session"
# RemoteDesktop device permission bits and Linux evdev button code.
KEYBOARD, POINTER = 1, 2
BTN_LEFT = 0x110
# Linux evdev keyboard codes, passed without the X11 keycode offset.
# Primary is a keyboard selection (Enter); touch contacts drive pointer clicks.
KEY_CODES = {
    "Play": 207, "Pause": 201, "NextTrack": 163, "PreviousTrack": 165,
    "Shuffle": 0x19A, "Repeat": 0x1B7, "TrackingNormal": 207,
    "TrackingIncrement": 0x1B5, "TrackingDecrement": 0x1B4,
    "PlayPause": 164, "Mute": 113, "VolumeUp": 115, "VolumeDown": 114,
    "Promote": 0x16C, "Demote": 14, "AddToCart": 221,
    "Home": 172, "Back": 158, "NavGuidance": 0x288, "Backspace": 14,
    "HookSwitch": 0x1BD, "Flash": 169, "Drop": 0x1BE, "PhoneMute": 248,
    **{f"PhoneKey{n}": 0x200 + n for n in range(10)},
    "PhoneKeyStar": 0x20A, "PhoneKeyPound": 0x20B, "Primary": 28,
}
DEVICE_COLORS = (
    (0.25, 0.75, 1.0), (1.0, 0.65, 0.25), (0.45, 0.9, 0.55),
    (0.85, 0.5, 1.0), (1.0, 0.45, 0.6),
)


def recv_exact(sock, length):
    data = bytearray()
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise EOFError("provider closed the control connection")
        data.extend(chunk)
    return bytes(data)


def recv_message(sock):
    length, = struct.unpack("<I", recv_exact(sock, 4))
    if not 0 < length <= MAX_PACKET:
        raise ValueError(f"invalid control packet length: {length}")
    message = json.loads(recv_exact(sock, length))
    if not isinstance(message, dict):
        raise ValueError("expected a JSON object from the provider")
    return message


def positive_integer(value, name, maximum):
    if type(value) is not int or not 0 < value <= maximum:
        raise ValueError(f"invalid {name}: {value!r}")
    return value


def message_type(message):
    kind = message.get("type")
    if kind not in ("init", "display_request", "input_event"):
        raise ValueError(f"unknown provider message type: {kind!r}")
    return kind


@dataclass
class ContactState:
    x: float
    y: float
    touching: bool
    updated_at: float
    trail: deque = field(default_factory=lambda: deque(maxlen=32))


@dataclass
class DeviceState:
    identity: str
    descriptor: object
    number: int
    contacts: dict = field(default_factory=dict)
    held_keys: set = field(default_factory=set)
    axes: dict = field(default_factory=dict)

    @property
    def surface(self):
        if isinstance(self.descriptor, dict):
            touch = self.descriptor["Network"].get("touch")
            if touch:
                return next(iter(touch))
        return None

    @property
    def label(self):
        return f"D{self.number} {self.surface or self.identity}"

    def description(self):
        if self.identity == "IapRemote":
            return f"{self.label} — media buttons"
        info = self.descriptor["Network"]
        detail = f"{self.label} — {info['id']}"
        if self.surface:
            touch = info["touch"][self.surface]
            detail += f" · {touch['max_touches']} contacts"
            size = touch.get("physical_size_mm")
            if size:
                detail += f" · {size[0]:g}×{size[1]:g} mm"
        return detail


class InputState:
    """Provider state shared by the visualizer and portal startup replay."""

    def __init__(self):
        self.devices = {}
        self.reports = self.events = 0

    def register(self, descriptor):
        if descriptor == "IapRemote":
            identity = descriptor
        elif isinstance(descriptor, dict) and set(descriptor) == {"Network"}:
            info = descriptor["Network"]
            if not isinstance(info, dict) or not isinstance(info.get("id"), str):
                raise ValueError("invalid network input device")
            identity = "Network:" + info["id"]
            touch = info.get("touch")
            if touch is not None and (
                not isinstance(touch, dict) or len(touch) != 1
                or next(iter(touch)) not in ("Touchscreen", "Touchpad")
            ):
                raise ValueError("invalid input surface capability")
        else:
            raise ValueError(f"unknown input device: {descriptor!r}")
        if identity not in self.devices:
            self.devices[identity] = DeviceState(identity, descriptor, len(self.devices) + 1)
        device = self.devices[identity]
        device.descriptor = descriptor
        return device

    def apply(self, descriptor, events):
        device = self.register(descriptor)
        if not isinstance(events, list):
            raise ValueError("input events must be a list")
        now = time.monotonic()
        for event in events:
            if not isinstance(event, dict) or len(event) != 1:
                raise ValueError("invalid input event")
            kind, value = next(iter(event.items()))
            if not isinstance(value, dict):
                raise ValueError("invalid input event payload")
            if kind == "Key":
                key, pressed = value.get("key"), value.get("pressed")
                if not isinstance(key, str) or type(pressed) is not bool:
                    raise ValueError("invalid key event")
                if pressed:
                    device.held_keys.add(key)
                else:
                    device.held_keys.discard(key)
            elif kind == "TouchEvent":
                contacts = value.get("contacts")
                if not isinstance(contacts, list):
                    raise ValueError("invalid touch contacts")
                for contact in contacts:
                    if not isinstance(contact, dict):
                        raise ValueError("invalid touch contact")
                    cid, x, y = contact.get("id"), contact.get("x"), contact.get("y")
                    touching = contact.get("touching")
                    if (type(cid) is not int or not 0 <= cid <= 0xFFFFFFFF
                        or type(touching) is not bool
                        or type(x) not in (int, float) or not math.isfinite(x)
                        or type(y) not in (int, float) or not math.isfinite(y)):
                        raise ValueError("invalid touch coordinates or state")
                    x, y = min(1.0, max(0.0, x)), min(1.0, max(0.0, y))
                    previous = device.contacts.get(cid)
                    if previous is None or (touching and not previous.touching):
                        previous = ContactState(x, y, touching, now)
                        device.contacts[cid] = previous
                    previous.x, previous.y = x, y
                    previous.touching, previous.updated_at = touching, now
                    previous.trail.append((x, y))
            elif kind == "Axis":
                aid, axis_value = value.get("id"), value.get("value")
                if type(aid) is not int or type(axis_value) is not int:
                    raise ValueError("invalid axis event")
                device.axes[aid] = axis_value
            else:
                raise ValueError(f"unknown input event: {kind!r}")
        self.reports += 1
        self.events += len(events)
        return device

    def snapshot(self, device):
        events = [{"Key": {"key": key, "pressed": True}} for key in sorted(device.held_keys)]
        contacts = [
            {"id": cid, "x": c.x, "y": c.y, "touching": True}
            for cid, c in device.contacts.items() if c.touching
        ]
        if contacts:
            events.append({"TouchEvent": {"contacts": contacts}})
        return events

    def prune(self):
        now = time.monotonic()
        for device in self.devices.values():
            device.contacts = {
                cid: c for cid, c in device.contacts.items()
                if c.touching or now - c.updated_at < 1.5
            }


class InputEmulator:
    """Translate normalized input into stream-local portal input events."""

    def __init__(self, portal, params, on_log):
        self.portal, self.params, self.on_log = portal, params, on_log
        self.keys = {}
        self.pointer_contacts = {}
        self.pointer_down = False
        self.pointer_position = None
        self.warned = set()

    def _warn(self, message):
        if message not in self.warned:
            self.warned.add(message)
            self.on_log(message)

    def _coordinates(self, contact):
        # KDE creates virtual outputs at scale 1. Their initial portal size can
        # be stale after PipeWire negotiation resizes the output to CarPlay.
        width, height = self.portal.logical_size or self.portal.properties.get("size") or (
            self.params.width, self.params.height
        )
        return (
            min(1.0, max(0.0, contact["x"])) * max(0, width - 1),
            min(1.0, max(0.0, contact["y"])) * max(0, height - 1),
        )

    def process(self, device, events):
        for event in events:
            kind, value = next(iter(event.items()))
            if kind == "Key":
                self._key(device, value)
            elif kind == "TouchEvent":
                for contact in value["contacts"]:
                    self._touch(device, contact)
                self._pointer()
            elif kind == "Axis":
                self._warn(f"{device.label}: axis {value['id']} is visualized only")

    def _key(self, device, event):
        key, pressed = event["key"], event["pressed"]
        code = KEY_CODES.get(key)
        if code is None:
            self._warn(f"{device.label}: no keyboard mapping for {key}")
            return
        if not self.portal.devices & KEYBOARD:
            self._warn("Portal keyboard access was not granted")
            return
        source = (device.identity, key)
        holders = self.keys.setdefault(code, set())
        was_pressed = bool(holders)
        if pressed:
            holders.add(source)
        else:
            holders.discard(source)
        if bool(holders) != was_pressed:
            self.portal.notify("NotifyKeyboardKeycode", "(oa{sv}iu)", code, int(bool(holders)))
        if not holders:
            self.keys.pop(code, None)

    def _touch(self, device, contact):
        identity = (device.identity, contact["id"])
        if self.portal.devices & POINTER:
            if not self.pointer_contacts or identity == next(iter(self.pointer_contacts)):
                self.pointer_position = self._coordinates(contact)
            if contact["touching"]:
                self.pointer_contacts[identity] = self._coordinates(contact)
            else:
                self.pointer_contacts.pop(identity, None)
        else:
            self._warn("Portal pointer access was not granted")

    def _pointer(self):
        down = bool(self.pointer_contacts)
        if down:
            self.pointer_position = next(iter(self.pointer_contacts.values()))
        if self.pointer_position is not None:
            x, y = self.pointer_position
            self.portal.notify("NotifyPointerMotionAbsolute", "(oa{sv}udd)", self.portal.node_id, x, y)
        if down != self.pointer_down:
            self.portal.notify("NotifyPointerButton", "(oa{sv}iu)", BTN_LEFT, int(down))
            self.pointer_down = down

    def release(self):
        for code in self.keys:
            self.portal.notify("NotifyKeyboardKeycode", "(oa{sv}iu)", code, 0)
        if self.pointer_down:
            self.portal.notify("NotifyPointerButton", "(oa{sv}iu)", BTN_LEFT, 0)
        self.keys.clear()
        self.pointer_contacts.clear()
        self.pointer_down = False
        self.pointer_position = None


@dataclass(frozen=True)
class VideoParams:
    width: int
    height: int
    fps: int

    @classmethod
    def from_message(cls, message):
        params = cls(
            positive_integer(message.get("display_width"), "display_width", 8192),
            positive_integer(message.get("display_height"), "display_height", 8192),
            positive_integer(message.get("display_fps"), "display_fps", 240),
        )
        if params.width % 2 or params.height % 2:
            raise ValueError("H.264 I420 requires an even display width and height")
        return params


def send_access_unit(sock, pts_us, data):
    length = 8 + len(data)
    if not data or len(data) > MAX_ACCESS_UNIT:
        raise ValueError(f"invalid H.264 packet length: {length}")
    # The length covers both the u64 microsecond PTS and one Annex B access unit.
    sock.sendall(struct.pack("<IQ", length, pts_us) + data)


def close_socket(sock):
    try:
        sock.shutdown(socket.SHUT_RDWR)
    except OSError:
        pass
    sock.close()


def load_runtime():
    # Keep --help and protocol tests usable without desktop dependencies.
    global Gio, GLib, Gdk, GdkPixbuf, Gst, GstVideo, Gtk
    try:
        import gi

        for name, version in (
            ("Gtk", "3.0"), ("Gdk", "3.0"), ("GdkPixbuf", "2.0"),
            ("Gst", "1.0"), ("GstApp", "1.0"), ("GstVideo", "1.0"),
        ):
            gi.require_version(name, version)
        gi.require_foreign("cairo")
        from gi.repository import Gio, GLib, Gdk, GdkPixbuf, Gst, GstVideo, Gtk
    except (ImportError, ValueError) as exc:
        raise RuntimeError(
            "install PyGObject, GTK 3, Python Cairo and GStreamer introspection bindings"
        ) from exc
    Gst.init(None)


def check_plugins(test_source=False):
    required = [
        "videotestsrc" if test_source else "pipewiresrc",
        "videoconvert", "videorate", "x264enc", "h264parse",
        "appsink", "appsrc", "avdec_h264",
    ]
    missing = [name for name in required if Gst.ElementFactory.find(name) is None]
    if missing:
        raise RuntimeError(f"missing GStreamer plugins: {', '.join(missing)}; see README.md")


class ScreenCastPortal:
    """One combined remote desktop/screen cast session, retained across requests."""

    def __init__(self, on_ready, on_error):
        self.on_ready = on_ready
        self.on_error = on_error
        self.connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.session = None
        self.pending_path = None
        self.pending_callback = None
        self.closed = False
        self.devices = 0
        self.node_id = None
        self.properties = {}
        self.logical_size = None
        self.subscriptions = [
            self.connection.signal_subscribe(
                PORTAL_NAME, REQUEST, "Response", None, None,
                Gio.DBusSignalFlags.NONE, self._response,
            ),
            self.connection.signal_subscribe(
                PORTAL_NAME, SESSION, "Closed", None, None,
                Gio.DBusSignalFlags.NONE, self._session_closed,
            ),
        ]

    def _call(self, path, interface, method, parameters, timeout=5000):
        return self.connection.call_sync(
            PORTAL_NAME, path, interface, method, parameters, None,
            Gio.DBusCallFlags.NONE, timeout, None,
        )

    def _request(self, interface, method, signature, arguments, options, callback):
        token = "carplayd_" + uuid.uuid4().hex
        options = dict(options, handle_token=GLib.Variant("s", token))
        sender = self.connection.get_unique_name()[1:].replace(".", "_")
        self.pending_path = f"{PORTAL_PATH}/request/{sender}/{token}"
        self.pending_callback = callback
        # Subscribe before calling: Response can arrive before the method returns.
        reply = self._call(
            PORTAL_PATH, interface, method,
            GLib.Variant(signature, (*arguments, options)),
        )
        self.pending_path = reply.unpack()[0]

    def _response(self, connection, sender, path, interface, signal_name, parameters):
        if self.closed or path != self.pending_path:
            return
        callback = self.pending_callback
        self.pending_path = self.pending_callback = None
        response, results = parameters.unpack()
        try:
            if response:
                reason = "cancelled" if response == 1 else "failed"
                raise RuntimeError(f"screen/input sharing {reason} (portal response {response})")
            callback(results)
        except Exception as exc:
            self.on_error(str(exc))

    def _session_closed(self, connection, sender, path, interface, signal_name, parameters):
        if not self.closed and path == self.session:
            self.on_error("screen/input sharing was stopped by the desktop portal")

    def start(self):
        properties = self._call(
            PORTAL_PATH, "org.freedesktop.DBus.Properties", "GetAll",
            GLib.Variant("(s)", (SCREENCAST,)),
        ).unpack()[0]
        if not properties.get("AvailableSourceTypes", 0) & 4:
            raise RuntimeError("the ScreenCast portal does not support virtual monitors; use KDE Wayland")
        modes = properties.get("AvailableCursorModes", 1)
        if not modes & 2:
            raise RuntimeError("the portal cannot embed the cursor in the video stream")
        input_properties = self._call(
            PORTAL_PATH, "org.freedesktop.DBus.Properties", "GetAll",
            GLib.Variant("(s)", (REMOTE_DESKTOP,)),
        ).unpack()[0]
        self.available_devices = input_properties.get("AvailableDeviceTypes", 0) & (KEYBOARD | POINTER)
        if self.available_devices != KEYBOARD | POINTER:
            raise RuntimeError("the RemoteDesktop portal must support keyboard and pointer input")
        token = "carplayd_" + uuid.uuid4().hex
        sender = self.connection.get_unique_name()[1:].replace(".", "_")
        self.session = f"{PORTAL_PATH}/session/{sender}/{token}"
        self._request(
            REMOTE_DESKTOP, "CreateSession", "(a{sv})", (),
            {"session_handle_token": GLib.Variant("s", token)}, self._created,
        )

    def _created(self, results):
        self.session = results["session_handle"]
        self._request(
            REMOTE_DESKTOP, "SelectDevices", "(oa{sv})", (self.session,),
            {"types": GLib.Variant("u", self.available_devices)}, self._devices_selected,
        )

    def _devices_selected(self, results):
        self._request(
            SCREENCAST, "SelectSources", "(oa{sv})", (self.session,),
            {
                "types": GLib.Variant("u", 4),
                "multiple": GLib.Variant("b", False),
                # Burn the cursor into the frames sent to both CarPlay and the preview.
                "cursor_mode": GLib.Variant("u", 2),
            },
            self._selected,
        )

    def _selected(self, results):
        self._request(REMOTE_DESKTOP, "Start", "(osa{sv})", (self.session, ""), {}, self._started)

    def _started(self, results):
        streams = results.get("streams", [])
        if len(streams) != 1:
            raise RuntimeError(f"expected one virtual monitor, got {len(streams)} streams")
        node_id, properties = streams[0]
        if properties.get("source_type", 4) != 4:
            raise RuntimeError("the portal returned a source other than a virtual monitor")
        self.devices = results.get("devices", 0)
        self.node_id, self.properties = node_id, properties
        LOG.info("Virtual monitor: PipeWire node %s, initial properties %s", node_id, properties)
        LOG.info("Portal input permissions: keyboard=%s pointer=%s",
                 bool(self.devices & KEYBOARD), bool(self.devices & POINTER))
        self.on_ready(node_id, properties)

    def notify(self, method, signature, *arguments):
        if self.closed or self.node_id is None:
            return
        # Asynchronous calls keep a slow portal from blocking video/UI updates.
        # D-Bus preserves their send order, including press/motion/release.
        self.connection.call(
            PORTAL_NAME, PORTAL_PATH, REMOTE_DESKTOP, method,
            GLib.Variant(signature, (self.session, {}, *arguments)), None,
            Gio.DBusCallFlags.NONE, 5000, None, self._notified,
        )

    def _notified(self, connection, result):
        try:
            connection.call_finish(result)
        except GLib.Error as exc:
            if not self.closed:
                self.on_error(f"portal input emulation failed: {exc.message}")

    def open_remote(self):
        reply, fd_list = self.connection.call_with_unix_fd_list_sync(
            PORTAL_NAME, PORTAL_PATH, SCREENCAST, "OpenPipeWireRemote",
            GLib.Variant("(oa{sv})", (self.session, {})), GLib.VariantType.new("(h)"),
            Gio.DBusCallFlags.NONE, 5000, None, None,
        )
        return fd_list.get(reply.unpack()[0])

    def close(self):
        self.closed = True
        for subscription in self.subscriptions:
            self.connection.signal_unsubscribe(subscription)
        for path, interface in ((self.pending_path, REQUEST), (self.session, SESSION)):
            if path:
                try:
                    self._call(path, interface, "Close", None, timeout=1000)
                except GLib.Error:
                    pass


class MediaStream:
    """Encode AUs, send each AU, then decode those same bytes for the preview."""

    def __init__(self, sock, params, bitrate, on_error, fd=None):
        self.socket = sock
        self.params = params
        self.bitrate = bitrate
        self.on_error = on_error
        self.fd = fd
        self.capture = self.preview = None
        self.buses = []
        self.stopped = threading.Event()
        self.reported = threading.Event()
        self.first_pts = None
        self.frames = self.bytes_sent = 0
        self.started_at = time.monotonic()

    def start(self, node_id=None, properties=None, test_source=False):
        self.preview = Gst.parse_launch(
            "appsrc name=encoded is-live=true format=time block=true max-bytes=8388608 "
            "! h264parse ! avdec_h264 max-threads=2 ! videoconvert "
            "! video/x-raw,format=RGB "
            "! appsink name=preview sync=false async=false max-buffers=1 drop=true "
            "enable-last-sample=false"
        )
        self.preview_source = self.preview.get_by_name("encoded")
        self.preview_sink = self.preview.get_by_name("preview")
        source = "videotestsrc is-live=true pattern=ball" if test_source else "pipewiresrc name=screen"
        p = self.params
        # KDE's virtual output follows the negotiated PipeWire size. Constrain
        # the source before conversion so the desktop itself matches CarPlay.
        self.capture = Gst.parse_launch(
            f"{source} ! video/x-raw,width={p.width},height={p.height},pixel-aspect-ratio=1/1 "
            "! videoconvert ! videorate "
            f"! video/x-raw,format=I420,width={p.width},height={p.height},"
            f"framerate={p.fps}/1,pixel-aspect-ratio=1/1 "
            f"! x264enc tune=zerolatency speed-preset=ultrafast bitrate={self.bitrate} "
            f"key-int-max={p.fps} bframes=0 byte-stream=true aud=true "
            "! video/x-h264,profile=constrained-baseline "
            "! h264parse config-interval=-1 "
            "! video/x-h264,stream-format=byte-stream,alignment=au "
            "! appsink name=network emit-signals=true sync=false async=false "
            "max-buffers=2 drop=false enable-last-sample=false"
        )
        if not test_source:
            LOG.info("Requesting virtual monitor at %s×%s with an embedded cursor", p.width, p.height)
            screen = self.capture.get_by_name("screen")
            screen.set_property("fd", self.fd)
            serial = (properties or {}).get("pipewire-serial")
            if serial is not None and screen.find_property("target-object"):
                screen.set_property("target-object", str(serial))
            else:
                screen.set_property("path", str(node_id))
            # Keep a static desktop flowing at the requested cadence.
            screen.set_property("do-timestamp", True)
            if screen.find_property("keepalive-time"):
                screen.set_property("keepalive-time", max(1, 1000 // p.fps))
        self.capture.get_by_name("network").connect("new-sample", self._encoded)
        for pipeline in (self.preview, self.capture):
            bus = pipeline.get_bus()
            bus.add_signal_watch()
            handler = bus.connect("message", self._message)
            self.buses.append((bus, handler))
            if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
                raise RuntimeError("could not start the video pipeline")

    def _report(self, message, disconnected=False):
        if not self.stopped.is_set() and not self.reported.is_set():
            self.reported.set()
            GLib.idle_add(self.on_error, self, message, disconnected)

    def _encoded(self, sink):
        if self.stopped.is_set():
            return Gst.FlowReturn.FLUSHING
        sample = sink.emit("pull-sample")
        if sample is None:
            return Gst.FlowReturn.EOS
        try:
            buffer = sample.get_buffer()
            if buffer.pts == Gst.CLOCK_TIME_NONE:
                raise RuntimeError("encoder produced a frame without a presentation timestamp")
            if self.first_pts is None:
                self.first_pts = buffer.pts
                self.preview_source.set_property("caps", sample.get_caps())
            pts = buffer.pts - self.first_pts
            if pts < 0:
                raise RuntimeError("encoder presentation timestamp precedes the first frame")
            data = buffer.extract_dup(0, buffer.get_size())
            try:
                send_access_unit(self.socket, pts // 1000, data)
            except OSError as exc:
                self._report(f"display connection closed: {exc}", disconnected=True)
                return Gst.FlowReturn.ERROR
            self.frames += 1
            self.bytes_sent += len(data)
            # Only successfully transmitted frames enter the local decoder.
            preview_buffer = buffer.copy()
            preview_buffer.pts = preview_buffer.dts = pts
            result = self.preview_source.emit("push-buffer", preview_buffer)
            if result != Gst.FlowReturn.OK and not self.stopped.is_set():
                self._report("the local preview decoder stopped accepting frames")
            return result
        except Exception as exc:
            self._report(str(exc))
            return Gst.FlowReturn.ERROR

    def _message(self, bus, message):
        if message.type == Gst.MessageType.ERROR:
            error, details = message.parse_error()
            LOG.debug("GStreamer: %s", details)
            self._report(error.message)
        elif message.type == Gst.MessageType.EOS:
            self._report("screen capture ended")

    def pull_preview(self):
        return self.preview_sink.emit("try-pull-sample", 0)

    def stop(self):
        self.stopped.set()
        # Release sendall and push-buffer before waiting for streaming threads.
        close_socket(self.socket)
        if self.preview:
            self.preview.set_state(Gst.State.NULL)
        if self.capture:
            self.capture.set_state(Gst.State.NULL)
        for bus, handler in self.buses:
            bus.disconnect(handler)
            bus.remove_signal_watch()
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


class Client:
    def __init__(self, args):
        self.args = args
        self.params = self.portal = self.stream = self.pending_socket = None
        self.node_id = self.properties = None
        self.pixbuf = None
        self.input = InputState()
        self.emulator = None
        self.source_ready = args.test_source
        self.stopped = threading.Event()
        self.sockets = set()
        self.socket_lock = threading.Lock()
        self.exit_code = 0
        self.window = Gtk.Window(title="carplayd — debug provider")
        self.window.set_default_size(1280, 760)
        self.window.connect("delete-event", lambda *_: self.stop())
        self.window.connect("key-press-event", self._key)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_border_width(10)
        sections = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        box.pack_start(sections, True, True, 0)

        screen_frame = Gtk.Frame(label="Screen — transmitted H.264")
        screen_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        screen_box.set_border_width(8)
        self.drawing = Gtk.DrawingArea()
        self.drawing.set_size_request(480, 300)
        self.drawing.connect("draw", self._draw)
        screen_box.pack_start(self.drawing, True, True, 0)
        self.screen_info = self._label("Waiting for provider display parameters…")
        screen_box.pack_start(self.screen_info, False, False, 0)
        screen_frame.add(screen_box)
        sections.pack_start(screen_frame, True, True, 0)

        input_frame = Gtk.Frame(label="Input — contacts and buttons")
        input_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        input_box.set_border_width(8)
        input_box.set_size_request(360, -1)
        self.device_info = self._label("Waiting for input devices…")
        self.device_info.set_max_width_chars(46)
        device_scroll = Gtk.ScrolledWindow()
        device_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        device_scroll.set_size_request(-1, 60)
        device_scroll.add(self.device_info)
        input_box.pack_start(device_scroll, False, True, 0)
        surface_frame = Gtk.AspectFrame(label="Touchscreen / touchpad · normalized 0–1",
                                        xalign=0.5, yalign=0.5, ratio=1.0, obey_child=False)
        self.input_drawing = Gtk.DrawingArea()
        self.input_drawing.set_size_request(300, 300)
        self.input_drawing.connect("draw", self._draw_input)
        surface_frame.add(self.input_drawing)
        input_box.pack_start(surface_frame, False, True, 0)
        self.input_status = self._label("0 reports · 0 events · 0 active contacts\nHeld buttons: none")
        input_box.pack_start(self.input_status, False, False, 0)
        self.emulation_status = self._label(
            "Emulation disabled for test source" if args.test_source else "Waiting for portal input access…"
        )
        input_box.pack_start(self.emulation_status, False, False, 0)
        log_frame = Gtk.Frame(label="Button / axis log")
        log_scroll = Gtk.ScrolledWindow()
        log_scroll.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.AUTOMATIC)
        log_scroll.set_size_request(-1, 120)
        self.input_log = Gtk.TextView()
        self.input_log.set_editable(False)
        self.input_log.set_cursor_visible(False)
        self.input_log.set_monospace(True)
        self.input_log.set_wrap_mode(Gtk.WrapMode.WORD_CHAR)
        self.log_buffer = self.input_log.get_buffer()
        self.log_end = self.log_buffer.create_mark(None, self.log_buffer.get_end_iter(), False)
        log_scroll.add(self.input_log)
        log_frame.add(log_scroll)
        input_box.pack_start(log_frame, True, True, 0)
        input_frame.add(input_box)
        sections.pack_start(input_frame, False, True, 0)

        self.status = Gtk.Label(label=f"Connecting to {args.host}:{args.port}…")
        box.pack_start(self.status, False, False, 0)
        self.window.add(box)

    @staticmethod
    def _label(text):
        label = Gtk.Label(label=text)
        label.set_xalign(0)
        label.set_selectable(True)
        label.set_line_wrap(True)
        return label

    def _append_log(self, message):
        self.log_buffer.insert(self.log_buffer.get_end_iter(), f"{time.strftime('%H:%M:%S')} {message}\n")
        if self.log_buffer.get_line_count() > 400:
            self.log_buffer.delete(self.log_buffer.get_start_iter(), self.log_buffer.get_iter_at_line(1))
        self.input_log.scroll_mark_onscreen(self.log_end)
        LOG.info("Input: %s", message)

    def _update_input_info(self):
        devices = self.input.devices.values()
        self.device_info.set_text("\n".join(d.description() for d in devices) or "No input devices advertised")
        held = [f"D{d.number}:{key}" for d in devices for key in sorted(d.held_keys)]
        active = sum(c.touching for d in devices for c in d.contacts.values())
        self.input_status.set_text(
            f"{self.input.reports} reports · {self.input.events} events · {active} active contacts\n"
            f"Held buttons: {', '.join(held) or 'none'}"
        )

    def _input_received(self, message):
        if self.stopped.is_set():
            return False
        try:
            events = message.get("events")
            device = self.input.apply(message.get("device"), events)
            for event in events:
                if "Key" in event:
                    key = event["Key"]
                    state = "pressed" if key["pressed"] else "released"
                    code = KEY_CODES.get(key["key"])
                    mapping = f" → keyboard {code}" if code is not None else " (unmapped)"
                    self._append_log(f"D{device.number} {key['key']} {state}{mapping}")
                elif "Axis" in event:
                    axis = event["Axis"]
                    self._append_log(f"D{device.number} axis {axis['id']} = {axis['value']}")
            self._update_input_info()
            self.input_drawing.queue_draw()
            if self.emulator:
                self.emulator.process(device, events)
        except Exception as exc:
            self.fail(str(exc))
        return False

    def _connect(self, port):
        sock = socket.create_connection((self.args.host, port), self.args.timeout)
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        with self.socket_lock:
            if self.stopped.is_set():
                close_socket(sock)
                raise RuntimeError("client stopped")
            self.sockets.add(sock)
        return sock

    def _release(self, sock):
        close_socket(sock)
        with self.socket_lock:
            self.sockets.discard(sock)

    def _control(self):
        try:
            control = self._connect(self.args.port)
            initial = recv_message(control)
            if message_type(initial) != "init":
                raise ValueError("expected an init message from the provider")
            params = VideoParams.from_message(initial)
            control.settimeout(None)
            GLib.idle_add(self._initialized, params, initial.get("input_devices", []))
            while not self.stopped.is_set():
                message = recv_message(control)
                kind = message_type(message)
                if kind == "display_request":
                    port = positive_integer(message.get("port"), "display port", 65535)
                    # Connect immediately so the provider can complete get_screen().
                    display = self._connect(port)
                    GLib.idle_add(self._display_requested, display)
                elif kind == "input_event":
                    GLib.idle_add(self._input_received, message)
                else:
                    raise ValueError("unexpected second init message")
        except Exception as exc:
            if not self.stopped.is_set():
                GLib.idle_add(self.fail, str(exc))

    def _initialized(self, params, input_devices):
        if self.stopped.is_set():
            return False
        self.params = params
        LOG.info("Provider display: %s×%s at %s fps", params.width, params.height, params.fps)
        try:
            if not isinstance(input_devices, list):
                raise ValueError("input_devices must be a list")
            for device in input_devices:
                self.input.register(device)
            self._update_input_info()
            self.screen_info.set_text(
                f"Provider: {params.width}×{params.height} · {params.fps} fps\n"
                f"Encoder: H.264 · {self.args.bitrate} kbit/s target\nNo display stream requested"
            )
            if self.args.test_source:
                self.status.set_text("Test source ready; waiting for a display request…")
            else:
                self.status.set_text("Allow virtual monitor sharing and input in the KDE portal dialog…")
                self.portal = ScreenCastPortal(self._portal_ready, self.fail)
                self.portal.start()
        except Exception as exc:
            self.fail(str(exc))
        return False

    def _portal_ready(self, node_id, properties):
        self.node_id, self.properties = node_id, properties
        self.source_ready = True
        self.emulator = InputEmulator(self.portal, self.params, self._append_log)
        granted = [name for bit, name in ((KEYBOARD, "keyboard"), (POINTER, "pointer"))
                   if self.portal.devices & bit]
        self.emulation_status.set_text(f"Portal emulation: {', '.join(granted) or 'no input access granted'}")
        for device in self.input.devices.values():
            self.emulator.process(device, self.input.snapshot(device))
        self.status.set_text("Virtual monitor ready; waiting for a display request…")
        self._start_stream()

    def _display_requested(self, sock):
        if self.stopped.is_set():
            self._release(sock)
            return False
        if self.pending_socket:
            self._release(self.pending_socket)
        self.pending_socket = sock
        self._start_stream()
        return False

    def _stop_stream(self):
        if self.stream:
            self.stream.stop()
            with self.socket_lock:
                self.sockets.discard(self.stream.socket)
            self.stream = None

    def _start_stream(self):
        if not self.source_ready or self.pending_socket is None or self.stopped.is_set():
            return
        try:
            self._stop_stream()
            fd = None if self.args.test_source else self.portal.open_remote()
            self.stream = MediaStream(
                self.pending_socket, self.params, self.args.bitrate, self._stream_error, fd,
            )
            self.pending_socket = None
            self.pixbuf = None
            self.stream.start(self.node_id, self.properties, self.args.test_source)
            self._stats()
            self.status.set_text("Streaming; waiting for the first encoded frame…")
            LOG.info("Streaming H.264 at %s kbit/s", self.args.bitrate)
        except Exception as exc:
            self.fail(str(exc))

    def _stream_error(self, stream, message, disconnected):
        if stream is self.stream and not self.stopped.is_set():
            if disconnected:
                LOG.info("%s", message)
                self._stop_stream()
                self.status.set_text("Waiting for another display request…")
            else:
                self.fail(message)
        return False

    def _refresh(self):
        if self.stopped.is_set():
            return False
        self.input.prune()
        self.input_drawing.queue_draw()
        if self.stream:
            try:
                sample = self.stream.pull_preview()
                if sample:
                    first_preview = self.pixbuf is None
                    buffer = sample.get_buffer()
                    info = GstVideo.VideoInfo.new_from_caps(sample.get_caps())
                    if self.portal and self.portal.logical_size != (info.width, info.height):
                        self.portal.logical_size = (info.width, info.height)
                        # Reposition contacts held while the virtual output was
                        # still at its initial size, without pressing again.
                        for device in self.input.devices.values():
                            self.emulator.process(device, self.input.snapshot(device))
                    meta = GstVideo.buffer_get_video_meta(buffer)
                    offset = meta.offset[0] if meta else info.offset[0]
                    stride = meta.stride[0] if meta else info.stride[0]
                    data = buffer.extract_dup(offset, buffer.get_size() - offset)
                    self.pixbuf = GdkPixbuf.Pixbuf.new_from_bytes(
                        GLib.Bytes.new(data), GdkPixbuf.Colorspace.RGB, False, 8,
                        info.width, info.height, stride,
                    )
                    self.drawing.queue_draw()
                    if first_preview:
                        self._stats()
            except Exception as exc:
                self.fail(str(exc))
                return False
        return True

    def _stats(self):
        if self.stopped.is_set():
            return False
        if self.stream:
            stream = self.stream
            elapsed = max(time.monotonic() - stream.started_at, 0.001)
            p = self.params
            source = "Test pattern" if self.args.test_source else f"Virtual monitor · PipeWire node {self.node_id}"
            self.screen_info.set_text(
                f"Provider: {p.width}×{p.height} · {p.fps} fps target · {stream.frames / elapsed:.1f} fps sent\n"
                f"H.264: {stream.frames} frames · {stream.bytes_sent / 1024 / 1024:.1f} MiB · "
                f"{stream.bytes_sent * 8 / elapsed / 1000:.0f} kbit/s ({self.args.bitrate} target)\n{source}"
            )
            if stream.frames:
                self.status.set_text("Streaming video and receiving input · Esc to stop")
        return True

    def _draw(self, widget, context):
        context.set_source_rgb(0, 0, 0)
        context.paint()
        if self.pixbuf:
            width, height = widget.get_allocated_width(), widget.get_allocated_height()
            pw, ph = self.pixbuf.get_width(), self.pixbuf.get_height()
            scale = min(width / pw, height / ph)
            context.translate((width - pw * scale) / 2, (height - ph * scale) / 2)
            context.scale(scale, scale)
            Gdk.cairo_set_source_pixbuf(context, self.pixbuf, 0, 0)
            context.paint()
        return False

    def _draw_input(self, widget, context):
        width, height = widget.get_allocated_width(), widget.get_allocated_height()
        context.set_source_rgb(0.08, 0.09, 0.12)
        context.paint()
        side = max(1, min(width, height) - 36)
        left, top = (width - side) / 2, (height - side) / 2
        context.set_line_width(1)
        context.set_source_rgb(0.25, 0.28, 0.33)
        for n in range(5):
            position = side * n / 4
            context.move_to(left + position, top)
            context.line_to(left + position, top + side)
            context.move_to(left, top + position)
            context.line_to(left + side, top + position)
        context.stroke()
        context.set_source_rgb(0.7, 0.73, 0.8)
        context.set_font_size(11)
        context.move_to(left, top - 5)
        context.show_text("0,0")
        context.move_to(left + side - 20, top + side + 14)
        context.show_text("1,1")
        now = time.monotonic()
        for device in self.input.devices.values():
            color = DEVICE_COLORS[(device.number - 1) % len(DEVICE_COLORS)]
            for cid, contact in device.contacts.items():
                opacity = 1.0 if contact.touching else max(0, 1 - (now - contact.updated_at) / 1.5)
                context.set_source_rgba(*color, opacity * 0.5)
                context.set_line_width(2)
                for i, (x, y) in enumerate(contact.trail):
                    if i == 0:
                        context.move_to(left + x * side, top + y * side)
                    else:
                        context.line_to(left + x * side, top + y * side)
                context.stroke()
                x, y = left + contact.x * side, top + contact.y * side
                context.set_source_rgba(*color, opacity)
                context.arc(x, y, 7, 0, 2 * math.pi)
                if contact.touching:
                    context.fill()
                else:
                    context.stroke()
                context.move_to(min(x + 10, left + side - 60), max(y - 10, top + 12))
                context.show_text(f"D{device.number}:{cid}")
        return False

    def _key(self, widget, event):
        if event.keyval == Gdk.KEY_Escape:
            self.stop()
            return True
        return False

    def fail(self, message):
        if not self.stopped.is_set():
            LOG.error("%s", message)
            self.exit_code = 1
            self.stop()
        return False

    def stop(self):
        if self.stopped.is_set():
            return False
        self.stopped.set()
        with self.socket_lock:
            sockets, self.sockets = self.sockets, set()
        for sock in sockets:
            close_socket(sock)
        self._stop_stream()
        if self.emulator:
            try:
                self.emulator.release()
            except Exception as exc:
                LOG.warning("Could not release portal input: %s", exc)
        if self.portal:
            self.portal.close()
        self.window.destroy()
        Gtk.main_quit()
        return False

    def run(self):
        self.window.show_all()
        GLib.timeout_add(16, self._refresh)
        GLib.timeout_add_seconds(1, self._stats)
        for signum in (signal.SIGINT, signal.SIGTERM):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signum, self.stop)
        threading.Thread(target=self._control, name="provider-control", daemon=True).start()
        Gtk.main()
        return self.exit_code


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("host", help="carplayd host, for example pi-carplayd.lan")
    parser.add_argument("--port", type=int, default=6767, help="control port (default: 6767)")
    parser.add_argument("--bitrate", type=int, default=4000, help="H.264 bitrate in kbit/s (default: 4000)")
    parser.add_argument("--timeout", type=float, default=10, help="connection/send timeout in seconds (default: 10)")
    parser.add_argument("--test-source", action="store_true", help="stream a test pattern instead of requesting a virtual monitor")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args()
    if not 0 < args.port <= 65535 or not 0 < args.bitrate <= 2048000:
        parser.error("port must be 1–65535 and bitrate must be 1–2048000 kbit/s")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("timeout must be a positive finite number")
    logging.basicConfig(level=logging.DEBUG if args.verbose else logging.INFO, format="%(levelname)s: %(message)s")
    try:
        load_runtime()
        check_plugins(args.test_source)
        if not Gtk.init_check()[0]:
            raise RuntimeError("could not open a desktop display for the preview window")
        return Client(args).run()
    except Exception as exc:
        LOG.error("%s", exc)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
