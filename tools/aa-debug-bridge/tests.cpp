#include "debug_client.hpp"
#include <iostream>

using namespace bridge;
static uint64_t position(const Bytes &bytes) {
    uint64_t n = 0;
    for (int i = 0; i < 8; ++i)
        n |= uint64_t(bytes.at(4 + i)) << (i * 8);
    return n;
}
static Bytes pcm(int sample, unsigned count) {
    Bytes out;
    while (count--) {
        out.push_back(uint8_t(sample));
        out.push_back(uint8_t(uint16_t(sample) >> 8));
    }
    return out;
}
int main(int argc, char **argv) {
    try {
        if (argc == 3 && std::string(argv[1]) == "--network") {
            boost::asio::io_service io;
            auto client =
                std::make_shared<DebugClient>(io, "127.0.0.1", unsigned(std::stoul(argv[2])));
            boost::asio::steady_timer timeout(io);
            timeout.expires_after(std::chrono::seconds(5));
            timeout.async_wait([&](auto ec) {
                if (!ec) {
                    client->close();
                    io.stop();
                }
            });
            int status = 1;
            uint64_t audio = 0;
            client->display_ready = [&] {
                client->video(packet(123456, {0, 0, 0, 1, 0x65, 0x88}), [] {});
                audio = client->start_audio(false);
                client->pcm(audio, {42, pcm(1234, 3840)});
                client->control({{"type", "now_playing_update"}, {"now_playing", nullptr}});
            };
            client->audio_status = [&](uint64_t id, const std::string &state) {
                if (id == audio && state == "active")
                    client->control({{"type", "audio_flush"}, {"id", id}});
            };
            client->input = [&](const Json &) {
                client->stop_audio(audio);
                status = 0;
                timeout.expires_after(std::chrono::milliseconds(50));
                timeout.async_wait([&](auto ec) {
                    if (!ec) {
                        client->close();
                        io.stop();
                    }
                });
            };
            client->error = [&](auto e) {
                std::cerr << e << '\n';
                timeout.cancel();
                io.stop();
            };
            client->start();
            io.run();
            return status;
        }
        auto g = Geometry::from_init(
            {{"display_width", 1080}, {"display_height", 678}, {"display_fps", 60}});
        require(g.canvas_width == 1280 && g.left == 0 && g.top == 0 && g.fps == 60,
                "crop selection");
        require(g.touch(0, 0) == std::pair<unsigned, unsigned>{0, 0}, "top-left touch");
        require(g.touch(1, 1) == std::pair<unsigned, unsigned>{1079, 677}, "bottom-right touch");
        require(Geometry::from_init(
                    {{"display_width", 800}, {"display_height", 480}, {"display_fps", 30}})
                    .crop()
                    .is_null(),
                "full frame crop");
        for (auto size : {std::pair<int, int>{801, 480}, {2000, 1200}, {0, 480}}) {
            bool rejected = false;
            try {
                Geometry::from_init({{"display_width", size.first},
                                     {"display_height", size.second},
                                     {"display_fps", 30}});
            } catch (...) {
                rejected = true;
            }
            require(rejected, "invalid dimensions accepted");
        }
        VideoTimeline video;
        require(!video.push(std::nullopt, {0, 0, 0, 1, 0x67, 0x42}, 100),
                "codec header emitted independently");
        auto frame = video.push(9000000000, {0, 0, 1, 0x65, 0x99}, 1000);
        require(frame && position(*frame) == 1000 && frame->at(16) == 0x67, "header PTS anchor");
        auto next = video.push(9000033333, {0, 0, 1, 0x41, 0x99}, 1100);
        require(position(*next) == 34333, "phone timing not preserved");
        video.reset();
        auto replug = video.push(1, {0, 0, 1, 0x65, 0x99}, 50000);
        require(position(*replug) == 50000, "replug time did not rebase");
        PromptMixer mixer;
        mixer.push(0, 1000000, pcm(30000, 160));
        mixer.push(1, 1000000, pcm(10000, 160));
        auto mixed = mixer.take();
        require(mixed.position == 0 && mixed.data == pcm(32767, 80), "prompt overlap clipping");
        mixed = mixer.take();
        require(mixed.position == 80 && mixed.data == pcm(32767, 80) && mixer.empty(),
                "prompt sequence");
        mixer.reset();
        mixer.push(0, 1000000, pcm(100, 80));
        mixer.push(1, 1005000, pcm(-100, 80));
        require(mixer.take().data == pcm(100, 80) && mixer.take().data == pcm(-100, 80),
                "prompt timestamp alignment");
        mixer.reset();
        mixer.push(0, 1000000, pcm(12, 2000));
        require(mixer.take().position == 0, "faster-than-real-time prompt was dropped");
        mixer.reset();
        mixer.push(0, 1000000, pcm(12, 10000));
        require(mixer.take().position == 2000, "unbounded prompt latency");
        // A 1 ms timestamp jitter must not leave a gap inside a prompt.
        mixer.reset();
        mixer.push(0, 1000000, pcm(100, 320));
        mixer.push(0, 1021000, pcm(200, 320));
        for (int block = 0; block < 8; ++block)
            require(mixer.take().data == pcm(block < 4 ? 100 : 200, 80), "prompt jitter gap");
        require(mixer.empty(), "prompt jitter overlap");
        require(aligned_position(320, 336, 640) == 320 && aligned_position(320, 1600, 640) == 1600 &&
                    aligned_position(2000, 100, 640) == 100,
                "position alignment tolerance");
        AckPacer pacer(48000, 100000);
        require(pacer.push(4800, 1000000) == 1000000, "first media ack was held");
        require(pacer.push(4800, 1000001) == 1100000, "media ran ahead of real time");
        require(pacer.push(4800, 1150000) == 1200000, "late media lost real time");
        // A phone that fell 300 ms behind is forgiven rather than repaid as a burst.
        require(pacer.push(4800, 1600000) == 1600000 && pacer.push(4800, 1600001) == 1700000,
                "media stall was burst");
        pacer.reset();
        require(pacer.push(4800, 5000000) == 5000000, "media pacer reset");
        auto framed = packet(42, {0x12, 0x34});
        require(framed.size() == 14 && framed[0] == 10 && position(framed) == 42, "binary framing");
        boost::asio::io_service queue_io;
        auto queue = std::make_shared<WireSocket>(queue_io, 100, true, [](auto) {});
        queue->send(Bytes(60));
        queue->send(Bytes(60));
        require(queue->queued_bytes() == 60 && queue->dropped_packets() == 1,
                "PCM queue was not bounded");
        bool finished = false;
        queue->send({}, [&] { finished = true; });
        queue->clear_pending();
        require(queue->queued_bytes() == 0 && !finished, "suspension queue clearing");
        queue->close();
        std::cout << "bridge geometry, timestamp, mixing, and framing tests passed\n";
        return 0;
    } catch (const std::exception &e) {
        std::cerr << e.what() << '\n';
        return 1;
    }
}
