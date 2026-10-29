"""Exercise the real C++ client against a fragmented fake provider."""
import json
import socket
import struct
import subprocess
import sys


def exact(sock, size):
    result = b""
    while len(result) < size:
        chunk = sock.recv(size-len(result))
        if not chunk:
            raise EOFError("bridge closed unexpectedly")
        result += chunk
    return result


def receive(sock):
    size, = struct.unpack("<I", exact(sock, 4))
    assert 0 < size <= 8*1024*1024
    return exact(sock, size)


def send(sock, message):
    data = json.dumps(message).encode()
    packet = struct.pack("<I", len(data)) + data
    for offset in range(0, len(packet), 3):
        sock.sendall(packet[offset:offset+3])


def listener():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    sock.listen()
    sock.settimeout(5)
    return sock


def main():
    with listener() as control, listener() as display, listener() as audio:
        child = subprocess.Popen([sys.argv[1], "--network", str(control.getsockname()[1])])
        try:
            with control.accept()[0] as peer:
                peer.settimeout(5)
                send(peer, dict(type="init", display_width=1080, display_height=678,
                                display_fps=30, input_devices=[], audio={}))
                send(peer, dict(type="display_request", port=display.getsockname()[1]))
                response = json.loads(receive(peer))
                assert response == dict(type="display_response", safe_area=dict(left=0, top=0, width=1080, height=678))
                with display.accept()[0] as video:
                    payload = receive(video)
                    assert struct.unpack("<Q", payload[:8])[0] == 123456
                    assert payload[8:] == b"\0\0\0\1\x65\x88"
                    request = json.loads(receive(peer))
                    assert request["type"] == "audio_start"
                    stream = request["request"]
                    assert stream["microphone"] is None and stream["playback_clock"] == "live"
                    identity = stream["id"]
                    send(peer, dict(type="audio_request", id=identity, port=audio.getsockname()[1]))
                    with audio.accept()[0] as pcm:
                        for position in (42, 1002):
                            block = receive(pcm)
                            assert struct.unpack("<Q", block[:8])[0] == position
                            assert len(block[8:]) == 960*4
                        send(peer, dict(type="audio_status", status=dict(id=identity, state="active")))
                        messages = [json.loads(receive(peer)), json.loads(receive(peer))]
                        assert {m["type"] for m in messages} == {"now_playing_update", "audio_flush"}
                        send(peer, dict(type="input_event", device="IapRemote", events=[]))
                        assert json.loads(receive(peer)) == dict(type="audio_stop", id=identity)
            assert child.wait(timeout=5) == 0
        finally:
            if child.poll() is None:
                child.kill()
                child.wait()
    print("fake-provider display, video, PCM, metadata, and teardown checks passed")


if __name__ == "__main__":
    main()
