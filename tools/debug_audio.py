"""Desktop audio endpoints for debug-network-client (no GI imports until used)."""
from dataclasses import dataclass
import logging
import os
import socket
import struct
import subprocess
import threading
import wave

LOG = logging.getLogger("debug-audio")


@dataclass(frozen=True)
class PcmFormat:
    sample_rate: int = 48000
    channels: int = 2
    sample_type: str = "s16_le"

    def validate(self):
        if type(self.sample_rate) is not int or not 8000 <= self.sample_rate <= 96000:
            raise ValueError("audio rate must be 8000–96000 Hz")
        if self.channels not in (1, 2) or self.sample_type not in ("s16_le", "f32_le"):
            raise ValueError("audio must be mono/stereo S16LE or F32LE")
        return self

    @property
    def frame_bytes(self):
        return self.channels * (2 if self.sample_type == "s16_le" else 4)

    def message(self):
        return dict(sample_rate=self.sample_rate, channels=self.channels, sample_type=self.sample_type)

    def caps(self):
        return (f"audio/x-raw,format={'S16LE' if self.sample_type == 's16_le' else 'F32LE'},"
                f"layout=interleaved,rate={self.sample_rate},channels={self.channels}")


def packet(position, data, format):
    frames, remainder = divmod(len(data), format.frame_bytes)
    if not frames or remainder or frames > (format.sample_rate + 49) // 50:
        raise ValueError("PCM packet must contain 1–20 ms of complete frames")
    if not 0 <= position <= (1 << 64) - 1 - frames:
        raise ValueError("invalid PCM frame position")
    return struct.pack("<IQ", 8 + len(data), position) + data


def recv_exact(sock, length):
    result = bytearray()
    while len(result) < length:
        part = sock.recv(length - len(result))
        if not part:
            raise EOFError("audio connection closed")
        result.extend(part)
    return result


def receive_packet(sock, format):
    length, = struct.unpack("<I", recv_exact(sock, 4))
    if not 8 < length <= 8 + ((format.sample_rate + 49) // 50) * format.frame_bytes:
        raise ValueError("invalid microphone packet length")
    position, = struct.unpack("<Q", recv_exact(sock, 8))
    data = recv_exact(sock, length - 8)
    if len(data) % format.frame_bytes:
        raise ValueError("partial microphone PCM frame")
    return position, data


def pactl(*args):
    result = subprocess.run(["pactl", *args], check=True, capture_output=True, text=True, timeout=5)
    return result.stdout.strip()


class AudioStream:
    def __init__(self, owner, identity, kind, use, source, filename=None):
        self.owner, self.identity, self.kind, self.use = owner, identity, kind, use
        self.source, self.filename = source, filename
        self.format = PcmFormat(owner.args.audio_rate, owner.args.audio_channels, owner.args.audio_format).validate()
        self.microphone = PcmFormat(owner.args.audio_rate, 1, owner.args.audio_format) if use in ("telephony", "speechRecognition") else None
        self.file = None
        if source == "file":
            self.file = wave.open(filename, "rb")
            if self.file.getsampwidth() != 2 or self.file.getcomptype() != "NONE":
                self.file.close()
                raise ValueError("test files must be uncompressed signed 16-bit PCM WAV")
            self.format = PcmFormat(self.file.getframerate(), self.file.getnchannels()).validate()
        self.socket = self.capture = self.mic_pipeline = None
        self.mic_source = None
        self.modules = []
        self.mic_origin = None
        self.mic_end = 0
        self.mic_lock = threading.Lock()
        self.position = 0
        self.stopped = threading.Event()
        self.suspended = False
        self.progress_timer = None

    def request(self):
        return dict(id=self.identity, kind=self.kind, audio_type=self.use,
                    playback=self.format.message(),
                    microphone=self.microphone.message() if self.microphone else None,
                    playback_clock="demand_driven" if self.source == "file" else "live",
                    microphone_clock="demand_driven" if self.owner.args.microphone_file else "live")

    def connect(self, sock):
        if self.stopped.is_set():
            self.owner.close_socket(sock)
            return False
        self.socket = sock
        sock.settimeout(None)
        try:
            if self.microphone:
                if not self.owner.args.microphone_file:
                    self._make_microphone()
                    self.progress_timer = self.owner.GLib.timeout_add(100, self._progress)
                threading.Thread(target=self._receive, name=f"audio-input-{self.identity}", daemon=True).start()
            if self.source == "file":
                threading.Thread(target=self._send_file, name=f"audio-file-{self.identity}", daemon=True).start()
            else:
                self._make_capture()
        except Exception as exc:
            self.owner.error(self.identity, str(exc))
        return False

    def _make_capture(self):
        Gst = self.owner.Gst
        if self.source == "tone":
            source = Gst.ElementFactory.make("audiotestsrc")
            source.set_property("is-live", True)
            source.set_property("volume", 0.15)
            source.set_property("samplesperbuffer", max(1, self.format.sample_rate // 200))
        elif self.source == "silence":
            source = Gst.ElementFactory.make("audiotestsrc")
            source.set_property("is-live", True)
            source.set_property("wave", 4)  # GstAudioTestSrcWave.SILENCE
        else:
            monitor = self.owner.args.audio_source or pactl("get-default-sink") + ".monitor"
            source = Gst.ElementFactory.make("pulsesrc")
            if source is None:
                raise RuntimeError("install GStreamer's PulseAudio plugin for desktop audio")
            source.set_property("device", monitor)
            source.set_property("buffer-time", 20000)
            source.set_property("latency-time", 5000)
        if source is None:
            raise RuntimeError("missing GStreamer audio source plugin")
        pipeline = Gst.Pipeline.new(None)
        convert = Gst.ElementFactory.make("audioconvert")
        sink = Gst.ElementFactory.make("appsink")
        if convert is None or sink is None:
            raise RuntimeError("missing GStreamer audioconvert/appsink plugin")
        sink.set_property("caps", Gst.Caps.from_string(self.format.caps()))
        sink.set_property("emit-signals", True)
        sink.set_property("sync", False)
        sink.set_property("max-buffers", 1)
        sink.set_property("drop", False)
        pipeline.add(source)
        pipeline.add(convert)
        pipeline.add(sink)
        if not source.link(convert) or not convert.link(sink):
            raise RuntimeError("could not link desktop audio pipeline")
        sink.connect("new-sample", self._sample)
        self.capture = pipeline
        self._watch(pipeline)
        if pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError("could not start desktop audio")

    def _sample(self, sink):
        Gst = self.owner.Gst
        sample = sink.emit("pull-sample")
        if self.stopped.is_set():
            return Gst.FlowReturn.FLUSHING
        if self.suspended:
            return Gst.FlowReturn.OK
        buffer = sample.get_buffer()
        data = buffer.extract_dup(0, buffer.get_size())
        size = max(1, self.format.sample_rate // 200) * self.format.frame_bytes
        try:
            for offset in range(0, len(data), size):
                piece = data[offset:offset + size]
                self.socket.sendall(packet(self.position, piece, self.format))
                self.position += len(piece) // self.format.frame_bytes
        except Exception as exc:
            if not self.stopped.is_set():
                self.owner.GLib.idle_add(self.owner.error, self.identity, str(exc))
            return Gst.FlowReturn.ERROR
        return Gst.FlowReturn.OK

    def _send_file(self):
        try:
            while not self.stopped.is_set():
                data = self.file.readframes(max(1, self.format.sample_rate // 200))
                if not data:
                    self.socket.shutdown(socket.SHUT_WR)
                    return
                self.socket.sendall(packet(self.position, data, self.format))
                self.position += len(data) // self.format.frame_bytes
        except Exception as exc:
            if not self.stopped.is_set():
                self.owner.GLib.idle_add(self.owner.error, self.identity, str(exc))

    def _make_microphone(self):
        Gst = self.owner.Gst
        name = f"carplayd_mic_{os.getpid()}_{self.identity}"
        self.modules.append(pactl("load-module", "module-null-sink", f"sink_name={name}",
                                  f"rate={self.microphone.sample_rate}", "channels=1",
                                  'sink_properties=device.description="CarPlay microphone transport"'))
        self.modules.append(pactl("load-module", "module-remap-source", f"master={name}.monitor",
                                  f"source_name={name}", 'source_properties=device.description="CarPlay microphone"'))
        self.mic_pipeline = Gst.parse_launch(
            f"appsrc name=microphone is-live=true format=time block=true "
            f"max-bytes={self.microphone.sample_rate * self.microphone.frame_bytes * 60 // 1000} "
            f"caps=\"{self.microphone.caps()}\" ! audioconvert "
            f"! pulsesink device={name} buffer-time=20000 latency-time=5000")
        self.mic_source = self.mic_pipeline.get_by_name("microphone")
        self._watch(self.mic_pipeline)
        if self.mic_pipeline.set_state(Gst.State.PLAYING) == Gst.StateChangeReturn.FAILURE:
            raise RuntimeError("could not start virtual microphone")
        self.owner.info.set_text(f"Select 'CarPlay microphone' in your desktop application ({name})")

    def _receive(self):
        output = None
        try:
            if self.owner.args.microphone_file:
                # The requested microphone format is S16LE when recording WAV.
                output = wave.open(self.owner.args.microphone_file, "wb")
                output.setnchannels(self.microphone.channels)
                output.setsampwidth(2)
                output.setframerate(self.microphone.sample_rate)
            while not self.stopped.is_set():
                position, data = receive_packet(self.socket, self.microphone)
                if output:
                    output.writeframesraw(data)
                    continue
                if self.suspended:
                    continue
                Gst = self.owner.Gst
                with self.mic_lock:
                    if self.mic_origin is None:
                        self.mic_origin = position
                    origin = self.mic_origin
                    previous = self.mic_end
                    self.mic_end = position + len(data) // self.microphone.frame_bytes
                buffer = Gst.Buffer.new_wrapped(data)
                buffer.pts = (position - origin) * Gst.SECOND // self.microphone.sample_rate
                buffer.duration = len(data) // self.microphone.frame_bytes * Gst.SECOND // self.microphone.sample_rate
                if position != previous:
                    buffer.set_flags(Gst.BufferFlags.DISCONT)
                result = self.mic_source.emit("push-buffer", buffer)
                if result not in (Gst.FlowReturn.OK, Gst.FlowReturn.FLUSHING):
                    raise RuntimeError(f"microphone pipeline returned {result}")
        except EOFError:
            pass  # Stream status supplies the reason; normal TEARDOWN closes this socket.
        except Exception as exc:
            if not self.stopped.is_set():
                self.owner.GLib.idle_add(self.owner.error, self.identity, str(exc))
        finally:
            if output:
                output.close()

    def _progress(self):
        if self.stopped.is_set():
            return False
        if self.suspended or not self.mic_pipeline:
            return True
        ok, position = self.mic_pipeline.query_position(self.owner.Gst.Format.TIME)
        with self.mic_lock:
            if ok and position >= 0 and self.mic_origin is not None:
                consumed = min(self.mic_end, self.mic_origin + position * self.microphone.sample_rate // self.owner.Gst.SECOND)
                self.owner.send(dict(type="audio_input_progress", id=self.identity, consumed=consumed))
        return True

    def _watch(self, pipeline):
        bus = pipeline.get_bus()
        bus.add_signal_watch()
        bus.connect("message", self._bus)

    def _bus(self, bus, message):
        Gst = self.owner.Gst
        if message.type == Gst.MessageType.ERROR and not self.stopped.is_set():
            error, debug = message.parse_error()
            self.owner.error(self.identity, f"{error}: {debug or ''}")

    def status(self, status):
        state = status["state"]
        if state == "suspended":
            self.suspended = True
            if self.mic_pipeline:
                self.mic_pipeline.set_state(self.owner.Gst.State.NULL)
                with self.mic_lock:
                    self.mic_origin = None
        elif state == "active" and self.suspended:
            if self.mic_pipeline:
                self.mic_pipeline.set_state(self.owner.Gst.State.PLAYING)
            self.suspended = False
        elif state in ("stopped", "error"):
            self.stop()

    def stop(self):
        if self.stopped.is_set():
            return
        self.stopped.set()
        if self.socket:
            self.owner.close_socket(self.socket)
        for pipeline in (self.capture, self.mic_pipeline):
            if pipeline:
                pipeline.set_state(self.owner.Gst.State.NULL)
                pipeline.get_bus().remove_signal_watch()
        if self.progress_timer:
            self.owner.GLib.source_remove(self.progress_timer)
            self.progress_timer = None
        if self.file:
            self.file.close()
        for module in reversed(self.modules):
            try:
                pactl("unload-module", module)
            except Exception as exc:
                LOG.warning("Could not unload audio module %s: %s", module, exc)
        self.modules.clear()


class AudioController:
    def __init__(self, args, Gst, Gtk, GLib, send, connect, close_socket):
        self.args, self.Gst, self.Gtk, self.GLib = args, Gst, Gtk, GLib
        self.send, self.connect_socket, self.close_socket = send, connect, close_socket
        self.streams = {}
        self.next_identity = 1
        self.enabled = False
        self.frame = Gtk.Frame(label="Audio — idle until requested")
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        box.set_border_width(8)
        row = Gtk.Box(spacing=6)
        for label, use, source in (("Media", "media", "desktop"), ("Voice", "speechRecognition", "silence"),
                                   ("Call", "telephony", "desktop"), ("Stop", None, None)):
            button = Gtk.Button(label=label)
            button.connect("clicked", lambda _, u=use, s=source: self.start("main", u, s) if u else self.stop_kind("main"))
            row.pack_start(button, False, False, 0)
        box.pack_start(row, False, False, 0)
        row = Gtk.Box(spacing=6)
        self.filename = Gtk.Entry()
        self.filename.set_placeholder_text("16-bit PCM WAV for alternate playback")
        row.pack_start(self.filename, True, True, 0)
        for label, source in (("File", "file"), ("Tone", "tone"), ("Stop prompt", None)):
            button = Gtk.Button(label=label)
            button.connect("clicked", lambda _, s=source: self.start("alternate", "default", s, self.filename.get_text()) if s else self.stop_kind("alternate"))
            row.pack_start(button, False, False, 0)
        box.pack_start(row, False, False, 0)
        self.duck = Gtk.CheckButton(label="Duck main audio during alternate playback")
        self.duck.set_active(True)
        box.pack_start(self.duck, False, False, 0)
        self.info = Gtk.Label(label="Waiting for audio capabilities…", xalign=0)
        self.info.set_line_wrap(True)
        box.pack_start(self.info, False, False, 0)
        self.frame.add(box)
        self.frame.set_sensitive(False)

    def capabilities(self, capabilities):
        self.enabled = capabilities is not None
        self.frame.set_sensitive(self.enabled)
        self.info.set_text("Audio idle. Voice/Call expose the car microphone." if self.enabled else "This daemon does not advertise audio support.")
        return False

    def start(self, kind, use, source, filename=None):
        if not self.enabled:
            return
        self.stop_kind(kind)
        identity = self.next_identity
        self.next_identity += 1
        try:
            if kind == "main" and self.args.audio_file and use == "media":
                source, filename = "file", self.args.audio_file
            stream = AudioStream(self, identity, kind, use, source, filename)
            if stream.microphone and self.args.microphone_file:
                stream.microphone = PcmFormat(self.args.audio_rate, 1, "s16_le")
            self.streams[identity] = stream
            self.send(dict(type="audio_start", request=stream.request()))
            if kind == "alternate" and self.duck.get_active():
                self.send(dict(type="audio_duck", id=identity, duration_ms=100, volume=0.2))
            if kind == "main":
                self.send(dict(type="audio_app_state", speech="listening" if use == "speechRecognition" else "none",
                               phone_call=use == "telephony", turn_by_turn=False))
            self.info.set_text(f"Requesting {use} audio at {stream.format.sample_rate} Hz…")
        except Exception as exc:
            self.error(identity, str(exc))

    def requested(self, identity, port):
        stream = self.streams.get(identity)
        if stream:
            try:
                sock = self.connect_socket(port)
                self.GLib.idle_add(stream.connect, sock)
            except Exception as exc:
                self.GLib.idle_add(self.error, identity, str(exc))

    def status(self, status):
        identity = status["id"]
        stream = self.streams.get(identity)
        if stream:
            was_suspended = stream.suspended
            stream.status(status)
            if status["state"] == "active" and was_suspended and stream.kind == "main":
                self.send(dict(type="audio_app_state", speech="listening" if stream.use == "speechRecognition" else "none",
                               phone_call=stream.use == "telephony", turn_by_turn=False))
            if status["state"] in ("stopped", "error"):
                self.streams.pop(identity, None)
        format = status.get("wire_format")
        detail = f" · head unit: {format['sample_rate']} Hz / {format['channels']} ch" if format else ""
        self.info.set_text(f"Audio {identity}: {status['state']}{detail}\n{status.get('detail') or ''}")
        return False

    def stop_kind(self, kind):
        for identity, stream in list(self.streams.items()):
            if stream.kind == kind:
                self.send(dict(type="audio_stop", id=identity))
                stream.stop()
                self.streams.pop(identity, None)

    def error(self, identity, detail):
        stream = self.streams.pop(identity, None)
        if stream:
            self.send(dict(type="audio_stop", id=identity))
            stream.stop()
        LOG.warning("Audio %s: %s", identity, detail)
        self.info.set_text(f"Audio {identity}: {detail}")
        return False

    def close(self):
        for stream in list(self.streams.values()):
            stream.stop()
        self.streams.clear()
