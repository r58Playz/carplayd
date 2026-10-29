"""Protocol/file tests; GStreamer tone smoke test runs when GI is installed."""
from types import SimpleNamespace
import importlib.util
import os
import struct
import tempfile
import threading
import unittest
import wave

from debug_audio import AudioStream, PcmFormat, packet, receive_packet


class MemorySocket:
    def __init__(self, data=b""):
        self.data = bytearray(data)
        self.sent = []
        self.ready = threading.Event()

    def recv(self, length):
        # Exercise fragmented length/position/sample reads.
        result = self.data[:min(length, 3)]
        del self.data[:len(result)]
        return bytes(result)

    def sendall(self, data):
        self.sent.append(data)
        self.ready.set()


def owner():
    return SimpleNamespace(args=SimpleNamespace(audio_rate=44100, audio_channels=2,
        audio_format="f32_le", microphone_file=None), close_socket=lambda sock: None)


class ProtocolTests(unittest.TestCase):
    def test_frame_positions_and_fragmented_pcm(self):
        for format in (PcmFormat(44100, 2), PcmFormat(48000, 1, "f32_le")):
            data = bytes(format.frame_bytes * 100)
            encoded = packet(123456, data, format)
            self.assertEqual(receive_packet(MemorySocket(encoded), format), (123456, data))

    def test_invalid_or_truncated_frames(self):
        format = PcmFormat()
        for data in (b"", b"a", bytes(format.sample_rate // 50 * format.frame_bytes + 4)):
            with self.assertRaises(ValueError):
                packet(0, data, format)
        with self.assertRaises(ValueError):
            packet((1 << 64) - 1, bytes(4), format)
        with self.assertRaises(ValueError):
            receive_packet(MemorySocket(struct.pack("<I", 0xFFFFFFFF)), format)
        with self.assertRaises(EOFError):
            receive_packet(MemorySocket(packet(0, bytes(4), format)[:-1]), format)

    def test_client_formats_do_not_follow_wire_voice_profile(self):
        stream = AudioStream(owner(), 2, "main", "speechRecognition", "silence")
        request = stream.request()
        self.assertEqual(request["playback"], dict(sample_rate=44100, channels=2, sample_type="f32_le"))
        self.assertEqual(request["microphone"], dict(sample_rate=44100, channels=1, sample_type="f32_le"))
        self.assertEqual(request["playback_clock"], "live")

    def test_wav_uses_native_format_and_demand_driven_clock(self):
        with tempfile.TemporaryDirectory() as directory:
            path = os.path.join(directory, "audio.wav")
            with wave.open(path, "wb") as output:
                output.setnchannels(1)
                output.setsampwidth(2)
                output.setframerate(24000)
                output.writeframes(bytes(480))
            stream = AudioStream(owner(), 3, "alternate", "default", "file", path)
            self.assertEqual(stream.format, PcmFormat(24000, 1))
            self.assertEqual(stream.request()["playback_clock"], "demand_driven")
            stream.file.close()

    @unittest.skipUnless(importlib.util.find_spec("gi"), "GStreamer introspection is not installed")
    def test_gstreamer_tone_produces_bounded_client_pcm(self):
        import gi
        gi.require_version("Gst", "1.0")
        from gi.repository import Gst, GLib
        Gst.init(None)
        if not Gst.ElementFactory.find("audiotestsrc"):
            self.skipTest("audiotestsrc plugin is not installed")
        errors = []
        holder = owner()
        holder.Gst, holder.GLib = Gst, GLib
        holder.error = lambda identity, error: errors.append(error)
        stream = AudioStream(holder, 4, "alternate", "default", "tone")
        stream.socket = MemorySocket()
        try:
            stream._make_capture()
            self.assertTrue(stream.socket.ready.wait(2), "no PCM from GStreamer tone")
            position, data = receive_packet(MemorySocket(stream.socket.sent[0]), stream.format)
            self.assertEqual(position, 0)
            self.assertGreater(len(data), 0)
            self.assertLessEqual(len(data), stream.format.sample_rate // 50 * stream.format.frame_bytes)
            self.assertFalse(errors)
        finally:
            stream.stop()


if __name__ == "__main__":
    unittest.main()
