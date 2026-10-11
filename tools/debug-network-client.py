#!/usr/bin/env python3
"""Stream a portal-created KDE virtual monitor to carplayd, with an H.264 preview. (codex testing thing)"""

import argparse
from dataclasses import dataclass
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
REQUEST = "org.freedesktop.portal.Request"
SESSION = "org.freedesktop.portal.Session"


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
    """One portal session, retained across provider screen requests."""

    def __init__(self, on_ready, on_error):
        self.on_ready = on_ready
        self.on_error = on_error
        self.connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        self.session = None
        self.pending_path = None
        self.pending_callback = None
        self.closed = False
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

    def _request(self, method, signature, arguments, options, callback):
        token = "carplayd_" + uuid.uuid4().hex
        options = dict(options, handle_token=GLib.Variant("s", token))
        sender = self.connection.get_unique_name()[1:].replace(".", "_")
        self.pending_path = f"{PORTAL_PATH}/request/{sender}/{token}"
        self.pending_callback = callback
        # Subscribe before calling: Response can arrive before the method returns.
        reply = self._call(
            PORTAL_PATH, SCREENCAST, method,
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
                raise RuntimeError(f"screen sharing {reason} (portal response {response})")
            callback(results)
        except Exception as exc:
            self.on_error(str(exc))

    def _session_closed(self, connection, sender, path, interface, signal_name, parameters):
        if not self.closed and path == self.session:
            self.on_error("screen sharing was stopped by the desktop portal")

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
        token = "carplayd_" + uuid.uuid4().hex
        sender = self.connection.get_unique_name()[1:].replace(".", "_")
        self.session = f"{PORTAL_PATH}/session/{sender}/{token}"
        self._request(
            "CreateSession", "(a{sv})", (),
            {"session_handle_token": GLib.Variant("s", token)}, self._created,
        )

    def _created(self, results):
        self.session = results["session_handle"]
        self._request(
            "SelectSources", "(oa{sv})", (self.session,),
            {
                "types": GLib.Variant("u", 4),
                "multiple": GLib.Variant("b", False),
                # Burn the cursor into the frames sent to both CarPlay and the preview.
                "cursor_mode": GLib.Variant("u", 2),
            },
            self._selected,
        )

    def _selected(self, results):
        self._request("Start", "(osa{sv})", (self.session, ""), {}, self._started)

    def _started(self, results):
        streams = results.get("streams", [])
        if len(streams) != 1:
            raise RuntimeError(f"expected one virtual monitor, got {len(streams)} streams")
        node_id, properties = streams[0]
        if properties.get("source_type", 4) != 4:
            raise RuntimeError("the portal returned a source other than a virtual monitor")
        LOG.info("Virtual monitor: PipeWire node %s, initial properties %s", node_id, properties)
        self.on_ready(node_id, properties)

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
        self.source_ready = args.test_source
        self.stopped = threading.Event()
        self.sockets = set()
        self.socket_lock = threading.Lock()
        self.exit_code = 0
        self.window = Gtk.Window(title="carplayd — transmitted H.264")
        self.window.set_default_size(960, 540)
        self.window.connect("delete-event", lambda *_: self.stop())
        self.window.connect("key-press-event", self._key)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.drawing = Gtk.DrawingArea()
        self.drawing.connect("draw", self._draw)
        box.pack_start(self.drawing, True, True, 0)
        self.status = Gtk.Label(label=f"Connecting to {args.host}:{args.port}…")
        self.status.set_margin_top(6)
        self.status.set_margin_bottom(6)
        box.pack_start(self.status, False, False, 0)
        self.window.add(box)

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
            params = VideoParams.from_message(recv_message(control))
            control.settimeout(None)
            GLib.idle_add(self._initialized, params)
            while not self.stopped.is_set():
                message = recv_message(control)
                port = positive_integer(message.get("port"), "display port", 65535)
                # Connect immediately so the provider can complete get_screen().
                display = self._connect(port)
                GLib.idle_add(self._display_requested, display)
        except Exception as exc:
            if not self.stopped.is_set():
                GLib.idle_add(self.fail, str(exc))

    def _initialized(self, params):
        if self.stopped.is_set():
            return False
        self.params = params
        LOG.info("Provider display: %s×%s at %s fps", params.width, params.height, params.fps)
        try:
            if self.args.test_source:
                self.status.set_text("Test source ready; waiting for a display request…")
            else:
                self.status.set_text("Choose New Virtual Output in the KDE screen sharing dialog…")
                self.portal = ScreenCastPortal(self._portal_ready, self.fail)
                self.portal.start()
        except Exception as exc:
            self.fail(str(exc))
        return False

    def _portal_ready(self, node_id, properties):
        self.node_id, self.properties = node_id, properties
        self.source_ready = True
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
        if self.stream:
            try:
                sample = self.stream.pull_preview()
                if sample:
                    buffer = sample.get_buffer()
                    info = GstVideo.VideoInfo.new_from_caps(sample.get_caps())
                    meta = GstVideo.buffer_get_video_meta(buffer)
                    offset = meta.offset[0] if meta else info.offset[0]
                    stride = meta.stride[0] if meta else info.stride[0]
                    data = buffer.extract_dup(offset, buffer.get_size() - offset)
                    self.pixbuf = GdkPixbuf.Pixbuf.new_from_bytes(
                        GLib.Bytes.new(data), GdkPixbuf.Colorspace.RGB, False, 8,
                        info.width, info.height, stride,
                    )
                    self.drawing.queue_draw()
            except Exception as exc:
                self.fail(str(exc))
                return False
        return True

    def _stats(self):
        if self.stopped.is_set():
            return False
        if self.stream and self.stream.frames:
            stream = self.stream
            elapsed = max(time.monotonic() - stream.started_at, 0.001)
            p = self.params
            self.status.set_text(
                f"{p.width}×{p.height} · {p.fps} fps · {stream.frames} frames sent · "
                f"{stream.bytes_sent * 8 / elapsed / 1000:.0f} kbit/s · Esc to stop"
            )
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
