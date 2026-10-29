"""debug-receiver protocol checks and end-to-end sessions; GStreamer tests run when GI is installed.

Set BRIDGE_TESTS to aa-debug-bridge's bridge-tests binary to also drive the receiver with the
bridge's C++ DebugClient.
"""
import importlib.util
import itertools
import json
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import wave

spec = importlib.util.spec_from_file_location("debug_receiver", Path(__file__).with_name("debug-receiver.py"))
receiver = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = receiver
spec.loader.exec_module(receiver)

DISPLAY = receiver.DisplayParams(1080, 678, 30)
CROP = dict(left=100, top=20, width=1080, height=678)
# Fake but well-formed Annex-B: SPS, PPS and an IDR slice; only decoded by the GI tests.
KEYFRAME = b"\0\0\0\1\x67\x42\0\x1f" + b"\0\0\0\1\x68\xce" + b"\0\0\1\x65\x88\x84"


class MemorySocket:
    def __init__(self, data):
        self.data = bytearray(data)

    def recv(self, length):
        # Exercise fragmented header and payload reads.
        result = self.data[:min(length, 3)]
        del self.data[:len(result)]
        return bytes(result)


def wait_for(condition, timeout=5, message="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return
        time.sleep(0.01)
    raise AssertionError(f"timed out waiting for {message}")


class Provider:
    """The provider side of the protocol over real sockets."""

    def __init__(self, port):
        self.port = port
        self.control = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.sockets = [self.control]

    def send(self, message):
        self.control.sendall(receiver.encode(message))

    def receive(self):
        payload = receiver.recv_frame(self.control)
        if payload is None:
            raise EOFError("receiver closed the control connection")
        return json.loads(payload)

    def expect(self, kind):
        message = self.receive()
        if message["type"] != kind:
            raise AssertionError(f"expected {kind}, got {message}")
        return message

    def connect(self, port):
        sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        self.sockets.append(sock)
        return sock

    def closed_by_receiver(self):
        try:
            while self.receive():
                pass
        except (EOFError, OSError):
            return True

    def close(self):
        for sock in self.sockets:
            sock.close()


class ReceiverTestCase(unittest.TestCase):
    def provider(self):
        provider = Provider(self.receiver.port)
        self.addCleanup(provider.close)
        return provider

    def start(self, *options):
        args = receiver.parse_args(["--headless", "--port", "0", "--stats", "60", *options])
        self.receiver = receiver.Receiver(args)
        self.thread = threading.Thread(target=self.receiver.serve, daemon=True)
        self.thread.start()
        self.addCleanup(self.thread.join, 5)
        self.addCleanup(self.receiver.stop)
        return self.receiver

    def session(self):
        wait_for(lambda: self.receiver.session is not None, message="a provider session")
        return self.receiver.session


class ValidationTests(unittest.TestCase):
    def test_crop_matches_pending_display_rules(self):
        crop = receiver.parse_crop(CROP)
        receiver.validate_crop(crop, DISPLAY)
        self.assertIsNone(receiver.parse_crop(None))
        for value, reason in [
            (dict(CROP, left=101), "even"), (dict(CROP, width=800), "exactly match"),
            (dict(CROP, height=0), "empty"), (dict(CROP, left=receiver.U32 - 1), "overflow"),
        ]:
            with self.subTest(value=value), self.assertRaisesRegex(receiver.ProtocolError, reason):
                receiver.validate_crop(receiver.parse_crop(value), DISPLAY)
        for value in (dict(CROP, left=-2), dict(CROP, top=1.0), {"left": 0}, [0, 0, 1080, 678]):
            with self.subTest(value=value), self.assertRaises(receiver.ProtocolError):
                receiver.parse_crop(value)

    def test_decoded_frames_must_fit_the_crop_or_match_the_display(self):
        crop = receiver.parse_crop(CROP)
        self.assertIsNone(receiver.frame_problem(1280, 720, crop, DISPLAY))
        self.assertIn("exceeds", receiver.frame_problem(1080, 678, crop, DISPLAY))
        self.assertIsNone(receiver.frame_problem(1080, 678, None, DISPLAY))
        self.assertIn("crop before copying", receiver.frame_problem(1280, 720, None, DISPLAY))

    def test_audio_requests_follow_serde_defaults_and_validation(self):
        request = receiver.parse_audio_request(dict(
            id=1, kind="main", audio_type="media",
            playback=dict(sample_rate=48000, channels=2, sample_type="s16_le")))
        self.assertEqual((request.microphone, request.playback_clock), (None, "live"))
        receiver.validate_audio_request(request)
        self.assertEqual(request.playback.max_packet, 8 + 960 * 4)
        base = dict(id=1, kind="alternate", audio_type="default",
                    playback=dict(sample_rate=16000, channels=1, sample_type="s16_le"))
        for change in (dict(playback_clock=None), dict(kind="other"), dict(id=-1),
                       dict(playback=dict(sample_rate=16000, channels=1, sample_type="u8"))):
            with self.subTest(change=change), self.assertRaises(receiver.ProtocolError):
                receiver.parse_audio_request(dict(base, **change))
        for change, reason in [
            (dict(playback=dict(sample_rate=4000, channels=1, sample_type="s16_le")), "8000"),
            (dict(playback=dict(sample_rate=16000, channels=3, sample_type="s16_le")), "mono or stereo"),
            (dict(microphone=dict(sample_rate=16000, channels=1, sample_type="s16_le")), "alternate"),
        ]:
            with self.subTest(change=change), self.assertRaisesRegex(receiver.ProtocolError, reason):
                receiver.validate_audio_request(receiver.parse_audio_request(dict(base, **change)))

    def test_now_playing_requires_media_item_and_typed_fields(self):
        snapshot = {"media_item": {"title": "Track", "artist": "", "album_title": "",
                                   "playback_duration": 123000},
                    "status": "Playing", "elapsed_time": 12000, "app_name": "Player",
                    "shuffle_mode": "Songs", "repeat_mode": "One"}
        self.assertIs(receiver.validate_now_playing(snapshot), snapshot)
        self.assertIsNone(receiver.validate_now_playing(None))
        self.assertEqual(receiver.describe_now_playing(snapshot), "Playing · Player · Track")
        for value in ({}, {"media_item": None}, {"media_item": {}, "status": "playing"},
                      {"media_item": {"playback_duration": -1}}, {"media_item": {"title": 3}}):
            with self.subTest(value=value), self.assertRaises(receiver.ProtocolError):
                receiver.validate_now_playing(value)

    def test_nal_types_and_frames(self):
        self.assertEqual(receiver.nal_types(KEYFRAME), [7, 8, 5])
        self.assertEqual(receiver.nal_types(b"\x65\x88"), [])
        self.assertEqual(receiver.nal_names([9, 7, 8, 5, 1]), "AUD SPS PPS IDR slice")
        encoded = receiver.encode({"type": "x"}) + receiver.frame(b"")
        sock = MemorySocket(encoded)
        self.assertEqual(json.loads(receiver.recv_frame(sock)), {"type": "x"})
        self.assertEqual(receiver.recv_frame(sock), b"")
        self.assertIsNone(receiver.recv_frame(sock))
        with self.assertRaisesRegex(receiver.ProtocolError, "middle of a frame"):
            receiver.recv_frame(MemorySocket(receiver.frame(b"abc")[:-1]))
        with self.assertRaisesRegex(receiver.ProtocolError, "limit"):
            receiver.recv_frame(MemorySocket(struct.pack("<I", 13) + bytes(13)), limit=12)

    def test_float_pcm_records_as_saturated_s16(self):
        data = struct.pack("<4f", 0.0, 1.0, -2.0, float("nan"))
        self.assertEqual(struct.unpack("<4h", receiver.pcm_to_s16(data, "f32_le")), (0, 32767, -32768, 0))

    def test_input_devices(self):
        devices = receiver.default_input_devices("touchscreen", True)
        self.assertEqual(devices[-1], "IapRemote")
        for device in devices:
            self.assertEqual(receiver.validate_input_device(device), device)
        self.assertEqual(receiver.default_input_devices("none", False), ["IapRemote"])
        minimal = {"Network": {"id": "x", "keys": ["Home"], "touch": None, "axes": []}}
        self.assertIsNone(receiver.validate_input_device(minimal)["Network"]["associated_display"])
        for device in ("Remote", {"Network": {"id": "x", "keys": ["Nope"], "touch": None, "axes": []}},
                       {"Network": {"id": "x", "keys": [], "touch": {"Mouse": {}}, "axes": []}}):
            with self.subTest(device=device), self.assertRaises(ValueError):
                receiver.validate_input_device(device)


class PlaybackModelTests(unittest.TestCase):
    FORMAT = receiver.PcmFormat(48000, 2, "s16_le")

    def model(self, live=True):
        blocks, events = [], []
        model = receiver.PlaybackModel(self.FORMAT, live, blocks.append,
                                       lambda kind, detail: events.append(kind))
        model.start(0.0)
        return model, blocks, events

    @staticmethod
    def pcm(frames=960, value=1):
        return struct.pack("<h", value) * 2 * frames

    def test_contiguous_jittery_stream_plays_without_glitches(self):
        model, blocks, events = self.model()
        for n in range(200):
            model.push(n * 960, self.pcm(), 0.010 + n * 0.020 + (0.004, -0.004, 0)[n % 3])
        model.advance(4.0)
        self.assertEqual((events, model.underruns), ([], 0))
        # After the start delay every 5 ms block carries PCM until the stream ends.
        played = [block != bytes(len(block)) for block in blocks]
        first, last = played.index(True), len(played) - 1 - played[::-1].index(True)
        self.assertNotIn(False, played[first:last + 1])
        self.assertGreater(sum(played), 780)

    def test_rounded_positions_reset_the_converter(self):
        model, _, events = self.model()
        model.push(0, self.pcm(), 0.010)
        model.push(959, self.pcm(), 0.030)
        self.assertEqual((events, model.resets), (["reset"], 1))

    def test_bursts_over_400_ms_trim_the_live_queue_to_the_target(self):
        model, _, events = self.model()
        for n in range(21):
            model.push(n * 960, self.pcm(), 0.010)
        self.assertEqual((events, model.stale_drops, round(model.queued, 3)),
                         (["stale"], 1, receiver.TARGET_BUFFER))
        demand, _, events = self.model(live=False)
        for n in range(21):
            demand.push(n * 960, self.pcm(), 0.010)
        self.assertEqual((events, round(demand.queued, 3)), ([], 0.42))

    def test_live_stream_rides_out_network_stalls(self):
        for stall, underruns in ((receiver.TARGET_BUFFER * 0.6, 0), (receiver.TARGET_BUFFER * 3, 1)):
            model, blocks, events = self.model()
            # 5 ms blocks; the stall holds them back, then TCP delivers the backlog at once.
            for n in range(600):
                sent = 0.005 * n
                model.push(n * 240, self.pcm(240), max(sent, 1.0 + stall) if sent >= 1.0 else sent)
            model.advance(3.0)
            self.assertEqual((events, model.underruns, model.stale_drops),
                             (["underrun"] * underruns, underruns, 0), stall)
            # A stall past the cushion is one short gap; the backlog then restores the cushion.
            played = [block != bytes(len(block)) for block in blocks]
            first, last = played.index(True), len(played) - 1 - played[::-1].index(True)
            gaps = [len(list(run)) for audible, run in itertools.groupby(played[first:last + 1]) if not audible]
            self.assertEqual(len(gaps), underruns, stall)
            # The steady cushion sits a block under the target, plus one block of tick alignment.
            self.assertLessEqual(sum(gaps) * 0.005, max(stall - receiver.TARGET_BUFFER, 0) + 0.015, stall)

    def test_only_gaps_inside_a_stream_count_as_underruns(self):
        model, blocks, events = self.model()
        for n in range(5):
            model.push(n * 960, self.pcm(), 0.010)
        model.advance(0.400)
        self.assertEqual(model.underruns, 0)
        # One gap is one underrun, however many blocks arrive while rebuffering.
        for n in range(5, 10):
            model.push(n * 960, self.pcm(), 0.400 + (n - 5) * 0.010)
        self.assertEqual((events, model.underruns), (["underrun"], 1))
        self.assertTrue(any(block == bytes(len(block)) for block in blocks[20:]))

    def test_playback_waits_for_the_target_after_running_dry(self):
        model, blocks, _ = self.model()
        for n in range(5):
            model.push(n * 960, self.pcm(), 0.010)
        model.advance(0.400)
        silent = len(blocks)
        model.push(5 * 960, self.pcm(), 0.400)
        model.advance(0.450)
        self.assertEqual(blocks[silent:], [bytes(len(blocks[0]))] * (len(blocks) - silent))
        for n in range(6, 10):
            model.push(n * 960, self.pcm(), 0.450)
        model.advance(0.455)
        self.assertNotEqual(blocks[-1], bytes(len(blocks[-1])))

    def test_demand_driven_streams_are_read_below_the_target(self):
        model, _, _ = self.model(live=False)
        for n in range(5):
            model.push(n * 960, self.pcm(), 0.010)
        self.assertFalse(model.wants_data())
        model.advance(0.101)
        self.assertTrue(model.wants_data())

    def test_suspension_stops_output_and_resume_starts_fresh(self):
        model, blocks, _ = self.model()
        model.push(0, self.pcm(), 0.010)
        model.stop()
        model.advance(1.0)
        self.assertEqual(blocks, [])
        model.start(1.0)
        self.assertEqual(model.queued, 0)
        model.advance(1.0 + receiver.TARGET_BUFFER)
        self.assertEqual(len(blocks), 1)


class SessionTests(ReceiverTestCase):
    def test_full_provider_session(self):
        self.start("--no-decode", "--no-audio")
        provider = self.provider()
        init = provider.expect("init")
        self.assertEqual((init["display_width"], init["display_height"], init["display_fps"]), (1080, 678, 30))
        self.assertEqual(init["audio"], receiver.AUDIO_CAPABILITIES)
        touch = init["input_devices"][0]
        self.assertIn("Touchscreen", touch["Network"]["touch"])
        request = provider.expect("display_request")
        provider.send(dict(type="display_response", safe_area=CROP))
        video = provider.connect(request["port"])
        video.sendall(receiver.frame(struct.pack("<Q", 1000) + KEYFRAME))
        video.sendall(receiver.frame(struct.pack("<Q", 34333) + b"\0\0\1\x41\x9a"))
        provider.send(dict(type="audio_start", request=dict(
            id=7, kind="main", audio_type="media",
            playback=dict(sample_rate=48000, channels=2, sample_type="s16_le"),
            microphone=None, playback_clock="live")))
        audio_request = provider.expect("audio_request")
        self.assertEqual(audio_request["id"], 7)
        status = provider.expect("audio_status")["status"]
        self.assertEqual((status["id"], status["state"]), (7, "active"))
        pcm = provider.connect(audio_request["port"])
        for position in (0, 960):
            pcm.sendall(receiver.frame(struct.pack("<Q", position) + bytes(960 * 4)))
        provider.send(dict(type="now_playing_update", now_playing={
            "media_item": {"title": "Track", "artist": "Artist"}, "status": "Playing"}))
        provider.send(dict(type="audio_duck", id=7, duration_ms=100, volume=2))
        status = provider.expect("audio_status")["status"]
        self.assertEqual((status["id"], status["state"], status["detail"]), (7, "error", "invalid duck parameters"))
        provider.send(dict(type="audio_app_state", speech="speaking", phone_call=False, turn_by_turn=True))

        session = self.session()
        wait_for(lambda: session.display and session.display.packets == 2, message="two video packets")
        stream = session.audio[7]
        wait_for(lambda: stream.packets == 2, message="two PCM packets")
        wait_for(lambda: session.app_state is not None, message="app state")
        display = session.display
        self.assertEqual(display.crop, receiver.Crop(100, 20, 1080, 678))
        self.assertEqual((display.keyframes, display.first_pts, display.last_pts), (1, 1000, 34333))
        self.assertEqual((stream.frames, stream.discontinuities), (1920, 0))
        self.assertEqual(session.now_playing["media_item"]["title"], "Track")
        self.assertFalse(session.warnings)

        session.send_input(touch, [{"TouchEvent": {"contacts": [{"id": 0, "x": 0.5, "y": 0.25, "touching": True}]}}])
        event = provider.expect("input_event")
        self.assertEqual(event["device"], touch)
        self.assertEqual(event["events"][0]["TouchEvent"]["contacts"][0]["y"], 0.25)
        self.receiver.set_head_unit_audio(True)
        self.assertEqual(provider.expect("audio_status")["status"]["state"], "suspended")
        self.receiver.set_head_unit_audio(False)
        self.assertEqual(provider.expect("audio_status")["status"]["state"], "active")
        provider.send(dict(type="audio_stop", id=7))
        status = provider.expect("audio_status")["status"]
        self.assertEqual((status["id"], status["state"]), (7, "stopped"))
        self.assertEqual(pcm.recv(1), b"")
        provider.close()
        wait_for(lambda: self.receiver.session is None, message="session end")
        self.assertIsInstance(session.reason, EOFError)
        self.assertEqual(session.errors, 1)

    def test_recording_keeps_trace_video_timing_and_wav(self):
        with tempfile.TemporaryDirectory() as directory:
            self.start("--no-decode", "--no-audio", "--record", directory)
            provider = self.provider()
            provider.expect("init")
            request = provider.expect("display_request")
            provider.send(dict(type="display_response", safe_area=CROP))
            video = provider.connect(request["port"])
            video.sendall(receiver.frame(struct.pack("<Q", 5) + KEYFRAME))
            provider.send(dict(type="audio_start", request=dict(
                id=2, kind="alternate", audio_type="default",
                playback=dict(sample_rate=16000, channels=1, sample_type="f32_le"))))
            port = provider.expect("audio_request")["port"]
            provider.expect("audio_status")
            pcm = provider.connect(port)
            pcm.sendall(receiver.frame(struct.pack("<Q", 0) + struct.pack("<2f", 0.5, -1.0)))
            session = self.session()
            wait_for(lambda: session.display and session.display.packets and session.audio[2].packets,
                     message="recorded media")
            provider.close()
            wait_for(lambda: self.receiver.session is None, message="session end")
            recorded = Path(session.recorder.directory)
            trace = [json.loads(line) for line in (recorded / "control.jsonl").read_text().splitlines()]
            self.assertEqual([(m["dir"], m["message"]["type"]) for m in trace[:4]],
                             [("tx", "init"), ("tx", "display_request"), ("rx", "display_response"),
                              ("rx", "audio_start")])
            self.assertEqual((recorded / "display-1.h264").read_bytes(), KEYFRAME)
            self.assertIn(f"5,{len(KEYFRAME)},SPS PPS IDR", (recorded / "display-1.csv").read_text())
            with wave.open(str(recorded / "audio-2-alternate-default.wav")) as audio:
                self.assertEqual((audio.getframerate(), audio.getnchannels(), audio.getsampwidth()), (16000, 1, 2))
                self.assertEqual(struct.unpack("<2h", audio.readframes(2)), (16384, -32767))

    def test_fatal_violations_end_the_session_and_the_receiver_accepts_another(self):
        self.start("--no-decode", "--no-audio", "--no-display")
        for message, reason in [
            (dict(type="display_response", safe_area=None), "unsolicited display response"),
            (dict(type="display_request", port=1), "unknown message type"),
            (dict(type="now_playing_update", now_playing={"status": "Playing"}), "media_item"),
        ]:
            with self.subTest(message=message):
                provider = self.provider()
                provider.expect("init")
                session = self.session()
                provider.send(message)
                self.assertTrue(provider.closed_by_receiver())
                wait_for(lambda: session.closed.is_set(), message="session close")
                self.assertIsInstance(session.reason, receiver.ProtocolError)
                self.assertIn(reason, str(session.reason))
                provider.close()
                wait_for(lambda: self.receiver.session is None, message="receiver idle")
        provider = self.provider()
        provider.expect("init")
        provider.control.sendall(receiver.frame(b'{"type":"audio_duck","id":3,"volume":NaN}'))
        self.assertTrue(provider.closed_by_receiver())

    def test_display_request_rules(self):
        self.start("--no-decode", "--no-audio")
        receiver.CONNECT_TIMEOUT, timeout = 0.3, receiver.CONNECT_TIMEOUT
        self.addCleanup(setattr, receiver, "CONNECT_TIMEOUT", timeout)
        provider = self.provider()
        provider.expect("init")
        provider.expect("display_request")
        session = self.session()
        wait_for(lambda: session.errors == 1, message="display timeout")
        self.assertIsNone(session.pending_display)
        session.request_display()
        request = provider.expect("display_request")
        provider.send(dict(type="display_response", safe_area=None))
        video = provider.connect(request["port"])
        wait_for(lambda: session.display is not None, message="display connection")
        video.sendall(receiver.frame(struct.pack("<Q", 0)))
        wait_for(lambda: session.errors == 2, message="empty access unit failure")
        self.assertIsNone(session.display)
        self.assertFalse(session.closed.is_set())
        provider.send(dict(type="display_response", safe_area=None))
        self.assertTrue(provider.closed_by_receiver())
        self.assertIn("unsolicited", str(session.reason))

    def test_refused_audio_messages_answer_with_error_status(self):
        self.start("--no-decode", "--no-audio", "--no-display")
        provider = self.provider()
        provider.expect("init")
        playback = dict(sample_rate=16000, channels=1, sample_type="s16_le")
        for message, identity, reason in [
            (dict(type="audio_flush", id=5), 5, "no audio stream"),
            (dict(type="audio_bogus", id=6), 6, "unknown message type"),
            (dict(type="audio_start", request=dict(id=8, kind="main", audio_type="media",
                                                   playback=dict(playback, sample_rate=200000))), 8, "8000"),
            (dict(type="audio_app_state", speech="listening", phone_call=False, turn_by_turn=False), 0,
             "requires owned main audio"),
        ]:
            with self.subTest(message=message):
                provider.send(message)
                status = provider.expect("audio_status")["status"]
                self.assertEqual((status["id"], status["state"]), (identity, "error"))
                self.assertIn(reason, status["detail"])
        provider.send(dict(type="audio_start", request=dict(id=9, kind="alternate", audio_type="default",
                                                            playback=playback)))
        port = provider.expect("audio_request")["port"]
        self.assertEqual(provider.expect("audio_status")["status"]["state"], "active")
        provider.send(dict(type="audio_start", request=dict(id=9, kind="alternate", audio_type="default",
                                                            playback=playback)))
        self.assertIn("already active", provider.expect("audio_status")["status"]["detail"])
        pcm = provider.connect(port)
        pcm.sendall(receiver.frame(struct.pack("<Q", 0) + bytes(321 * 2)))
        status = provider.expect("audio_status")["status"]
        self.assertEqual((status["id"], status["state"]), (9, "error"))
        self.assertIn("limit", status["detail"])
        self.assertFalse(self.session().closed.is_set())


@unittest.skipUnless(os.environ.get("BRIDGE_TESTS"), "set BRIDGE_TESTS to aa-debug-bridge's bridge-tests")
class BridgeClientTests(ReceiverTestCase):
    def test_cpp_debug_client_against_the_receiver(self):
        self.start("--no-decode", "--no-audio", "--once")
        child = subprocess.Popen([os.environ["BRIDGE_TESTS"], "--network", str(self.receiver.port)])
        self.addCleanup(lambda: child.poll() is None and child.kill())
        session = self.session()
        wait_for(lambda: any(s.packets == 2 and s.flushes == 1 for s in list(session.audio.values())),
                 message="bridge PCM and flush")
        wait_for(lambda: session.display and session.display.packets == 1, message="bridge video")
        stream = next(iter(session.audio.values()))
        self.assertEqual((stream.request.kind, stream.request.playback_clock, stream.model.next_position),
                         ("main", "live", 1002 + 960))
        self.assertEqual((session.display.packets, session.display.last_pts), (1, 123456))
        self.assertEqual(session.now_playing_updates, 1)
        # The fake provider stops its audio and exits after any input event.
        session.send_input("IapRemote", [])
        self.assertEqual(child.wait(timeout=5), 0)
        self.thread.join(5)
        self.assertIsInstance(session.reason, EOFError)
        self.assertEqual(session.errors, 0)
        self.assertTrue(any("without a preceding SPS and PPS" in w for w in session.warnings))


def gstreamer():
    try:
        receiver.load_gi(False)
    except (ImportError, ValueError):
        return False
    return not receiver.missing_plugins(["videotestsrc", "x264enc", "h264parse", "avdec_h264",
                                         "appsrc", "appsink", "audioconvert", "audioresample", "fakesink"])


def encode_h264(width, height, frames=8):
    Gst = receiver.Gst
    pipeline = Gst.parse_launch(
        f"videotestsrc num-buffers={frames} ! video/x-raw,width={width},height={height},framerate=30/1 "
        "! x264enc tune=zerolatency speed-preset=ultrafast key-int-max=30 bframes=0 byte-stream=true "
        "! video/x-h264,profile=constrained-baseline ! h264parse config-interval=-1 "
        "! video/x-h264,stream-format=byte-stream,alignment=au ! appsink name=sink sync=false")
    sink = pipeline.get_by_name("sink")
    pipeline.set_state(Gst.State.PLAYING)
    units = []
    while True:
        sample = sink.emit("try-pull-sample", 5 * Gst.SECOND)
        if sample is None:
            break
        buffer = sample.get_buffer()
        units.append(buffer.extract_dup(0, buffer.get_size()))
    pipeline.set_state(Gst.State.NULL)
    return units


@unittest.skipUnless(importlib.util.find_spec("gi") and gstreamer(), "GStreamer bindings/plugins missing")
class GStreamerTests(ReceiverTestCase):
    def decoded_session(self, safe_area, width, height):
        provider = self.provider()
        provider.expect("init")
        request = provider.expect("display_request")
        provider.send(dict(type="display_response", safe_area=safe_area))
        video = provider.connect(request["port"])
        for pts, unit in enumerate(encode_h264(width, height)):
            video.sendall(receiver.frame(struct.pack("<Q", pts * 33333) + unit))
        session = self.session()
        wait_for(lambda: session.display and session.display.decoder and session.display.decoder.frames,
                 message="a decoded frame")
        return provider, session

    def test_decoded_canvas_and_crop(self):
        self.start("--no-audio")
        provider, session = self.decoded_session(CROP, 1280, 720)
        display = session.display
        self.assertEqual(display.decoder.size, (1280, 720))
        self.assertEqual(display.keyframes, 1)
        self.assertIsNone(display.frame_problem)
        self.assertFalse(session.warnings)
        self.assertEqual(session.errors, 0)
        provider.close()
        wait_for(lambda: self.receiver.session is None, message="session end")
        provider, session = self.decoded_session(None, 1280, 720)
        self.assertIn("crop before copying", session.display.frame_problem)
        self.assertEqual(session.errors, 1)

    def test_playback_pipeline_accepts_pcm(self):
        self.start("--no-decode", "--no-display", "--audio-sink", "fakesink")
        provider = self.provider()
        provider.expect("init")
        provider.send(dict(type="audio_start", request=dict(
            id=1, kind="main", audio_type="media",
            playback=dict(sample_rate=44100, channels=2, sample_type="f32_le"))))
        port = provider.expect("audio_request")["port"]
        provider.expect("audio_status")
        pcm = provider.connect(port)
        for position in range(0, 882 * 5, 882):
            pcm.sendall(receiver.frame(struct.pack("<Q", position) + bytes(882 * 8)))
        session = self.session()
        stream = session.audio[1]
        wait_for(lambda: stream.packets == 5, message="PCM packets")
        self.assertIsNotNone(stream.player)
        self.assertEqual(session.errors, 0)


if __name__ == "__main__":
    unittest.main()
