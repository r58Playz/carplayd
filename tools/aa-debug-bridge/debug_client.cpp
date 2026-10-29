#include "debug_client.hpp"
#include <boost/asio/read.hpp>
#include <boost/asio/write.hpp>
#include <iostream>

namespace bridge {
WireSocket::WireSocket(boost::asio::io_service &io, size_t limit, bool trim,
                       std::function<void(std::string)> error)
    : io_(io), resolver_(io), socket_(io), connect_timer_(io), write_timer_(io), limit_(limit),
      trim_(trim), error_(std::move(error)) {}
void WireSocket::fail(const std::string &message) {
    if (closed_)
        return;
    close();
    error_(message);
}
void WireSocket::connect(const std::string &host, unsigned port, Done ready) {
    auto self = shared_from_this();
    connect_timer_.expires_after(std::chrono::seconds(10));
    connect_timer_.async_wait([self](auto ec) {
        if (!ec)
            self->fail("TCP connection timed out");
    });
    resolver_.async_resolve(
        host, std::to_string(port), [self, ready = std::move(ready)](auto ec, auto endpoints) {
            if (self->closed_)
                return;
            if (ec)
                return self->fail("resolve: " + ec.message());
            boost::asio::async_connect(self->socket_, endpoints, [self, ready](auto ec, auto) {
                if (self->closed_)
                    return;
                if (ec)
                    return self->fail("connect: " + ec.message());
                self->connect_timer_.cancel();
                self->socket_.set_option(Tcp::no_delay(true));
                self->connected_ = true;
                try {
                    if (ready)
                        ready();
                    self->pump();
                } catch (const std::exception &e) {
                    self->fail(e.what());
                }
            });
        });
}
void WireSocket::send(Bytes bytes, Done done) {
    require(!closed_, "send on closed TCP stream");
    require(bytes.size() <= limit_, "packet exceeds TCP queue limit");
    while (trim_ && bytes_ + bytes.size() > limit_) {
        auto it = writes_.begin();
        if (writing_ && it != writes_.end())
            ++it;
        if (it == writes_.end()) {
            ++dropped_;
            if (done)
                done();
            return;
        }
        bytes_ -= it->bytes->size();
        writes_.erase(it);
        ++dropped_;
    }
    require(bytes_ + bytes.size() <= limit_, "TCP queue limit exceeded");
    bytes_ += bytes.size();
    writes_.push_back({std::make_shared<Bytes>(std::move(bytes)), std::move(done)});
    pump();
}
void WireSocket::send_json(const Json &json) {
    auto text = json.dump();
    require(text.size() <= 1024 * 1024, "control message too large");
    Bytes data;
    for (int n = 0; n < 4; ++n)
        data.push_back(uint8_t(text.size() >> (8 * n)));
    data.insert(data.end(), text.begin(), text.end());
    send(std::move(data));
}
void WireSocket::pump() {
    if (!connected_ || writing_ || closed_ || writes_.empty())
        return;
    writing_ = true;
    auto self = shared_from_this();
    auto buffer = writes_.front().bytes;
    write_timer_.expires_after(std::chrono::seconds(10));
    write_timer_.async_wait([self](auto ec) {
        if (!ec)
            self->fail("TCP write timed out");
    });
    boost::asio::async_write(socket_, boost::asio::buffer(*buffer),
                             [self, buffer](auto ec, size_t) {
                                 if (self->closed_)
                                     return;
                                 self->write_timer_.cancel();
                                 if (ec)
                                     return self->fail("write: " + ec.message());
                                 auto done = std::move(self->writes_.front().done);
                                 self->bytes_ -= buffer->size();
                                 self->writes_.pop_front();
                                 self->writing_ = false;
                                 try {
                                     if (done)
                                         done();
                                     self->pump();
                                 } catch (const std::exception &e) {
                                     self->fail(e.what());
                                 }
                             });
}
void WireSocket::read_json(std::function<void(Json)> receive) {
    auto self = shared_from_this();
    auto header = std::make_shared<std::array<uint8_t, 4>>();
    boost::asio::async_read(
        socket_, boost::asio::buffer(*header), [self, header, receive](auto ec, size_t) {
            if (self->closed_)
                return;
            if (ec)
                return self->fail("control read: " + ec.message());
            uint32_t size = 0;
            for (int i = 0; i < 4; ++i)
                size |= uint32_t((*header)[i]) << (8 * i);
            if (!size || size > 8 * 1024 * 1024)
                return self->fail("invalid control packet length");
            auto data = std::make_shared<Bytes>(size);
            boost::asio::async_read(
                self->socket_, boost::asio::buffer(*data), [self, data, receive](auto ec, size_t) {
                    if (self->closed_)
                        return;
                    if (ec)
                        return self->fail("control body: " + ec.message());
                    try {
                        auto value = Json::parse(*data);
                        require(value.is_object(), "control JSON must be an object");
                        receive(std::move(value));
                    } catch (const std::exception &e) {
                        self->fail(e.what());
                        return;
                    }
                    if (!self->closed_)
                        self->read_json(receive);
                });
        });
}
void WireSocket::clear_pending() {
    auto it = writes_.begin();
    if (writing_ && it != writes_.end())
        ++it;
    while (it != writes_.end()) {
        // Preserve zero-byte completion barriers used to drain stopped audio.
        if (it->bytes->empty()) {
            ++it;
            continue;
        }
        bytes_ -= it->bytes->size();
        ++dropped_;
        it = writes_.erase(it);
    }
}
void WireSocket::close() {
    if (closed_)
        return;
    closed_ = true;
    connected_ = false;
    resolver_.cancel();
    connect_timer_.cancel();
    write_timer_.cancel();
    boost::system::error_code ignored;
    socket_.close(ignored);
    writes_.clear();
    bytes_ = 0;
}
DebugClient::DebugClient(boost::asio::io_service &io, std::string host, unsigned port)
    : io_(io), host_(std::move(host)), port_(port), stats_timer_(io) {}
std::shared_ptr<WireSocket> DebugClient::wire(size_t limit, bool trim) {
    auto weak = weak_from_this();
    return std::make_shared<WireSocket>(io_, limit, trim, [weak](auto e) {
        if (auto self = weak.lock())
            self->fail(e);
    });
}
void DebugClient::start() {
    control_ = wire(1024 * 1024);
    auto self = shared_from_this();
    stats();
    control_->connect(host_, port_, [self] {
        self->control_->read_json([self](auto m) { self->receive(std::move(m)); });
    });
}
void DebugClient::stats() {
    auto self = shared_from_this();
    stats_timer_.expires_after(std::chrono::seconds(10));
    stats_timer_.async_wait([self](auto ec) {
        if (ec || self->stopped_)
            return;
        std::cerr << "video " << (self->video_frames_ - self->previous_frames_) / 10.
                  << " fps, queued " << (self->video_ ? self->video_->queued_bytes() : 0)
                  << " bytes";
        self->previous_frames_ = self->video_frames_;
        for (const auto &entry : self->audio_)
            std::cerr << "; audio " << entry.first << " queued "
                      << entry.second.wire->queued_bytes() << " bytes, dropped "
                      << entry.second.wire->dropped_packets() << ", discontinuities "
                      << entry.second.discontinuities;
        std::cerr << '\n';
        self->stats();
    });
}
void DebugClient::fail(const std::string &message) {
    if (!stopped_) {
        close();
        if (error)
            error(message);
    }
}
void DebugClient::control(const Json &message) {
    if (!stopped_)
        control_->send_json(message);
}
void DebugClient::receive(Json message) {
    std::string type = message.at("type");
    if (!geometry_) {
        require(type == "init", "expected provider init");
        geometry_ = Geometry::from_init(message);
        require(message.contains("audio"), "provider does not advertise PCM audio");
        if (initialized)
            initialized(*geometry_, message.at("input_devices"));
        return;
    }
    if (type == "display_request") {
        require(!video_, "display stream is already connected");
        auto port = message.at("port").get<unsigned>();
        require(port && port <= 65535, "invalid display port");
        video_ = wire(8 * 1024 * 1024);
        // Completion means the response is on the control socket before the
        // separate display connection is initiated. There is no extra reply.
        auto text = Json({{"type", "display_response"}, {"safe_area", geometry_->crop()}}).dump();
        Bytes frame;
        for (int i = 0; i < 4; ++i)
            frame.push_back(uint8_t(text.size() >> (i * 8)));
        frame.insert(frame.end(), text.begin(), text.end());
        auto self = shared_from_this();
        control_->send(std::move(frame), [self, port] {
            self->video_->connect(self->host_, port, [self] {
                if (self->display_ready)
                    self->display_ready();
            });
        });
    } else if (type == "input_event") {
        if (input)
            input(message);
    } else if (type == "audio_request") {
        uint64_t id = message.at("id");
        auto it = audio_.find(id);
        if (it == audio_.end())
            return; // A stop can overtake its pending connection request.
        unsigned port = message.at("port");
        require(port && port <= 65535, "invalid audio port");
        it->second.wire->connect(host_, port, {});
    } else if (type == "audio_status") {
        const auto &status = message.at("status");
        uint64_t id = status.at("id");
        std::string state = status.at("state");
        auto it = audio_.find(id);
        if (it == audio_.end())
            return;
        it->second.suspended = state != "active";
        if (it->second.suspended)
            it->second.wire->clear_pending();
        if (state == "error")
            std::cerr << "audio " << id << ": "
                      << status.value("detail", std::string("unknown error")) << '\n';
        if (audio_status)
            audio_status(id, state);
    } else
        throw std::runtime_error("unexpected provider message: " + type);
}
void DebugClient::video(Bytes data, Done done) {
    require(bool(video_), "Android Auto video arrived before a display request");
    ++video_frames_;
    video_->send(std::move(data), std::move(done));
}
uint64_t DebugClient::start_audio(bool alternate) {
    auto id = next_id_++;
    unsigned rate = alternate ? 16000 : 48000, channels = alternate ? 1 : 2;
    Audio audio{
        rate, channels, false, false, 0, 0, wire(rate * channels * 2 * 60 / 1000 + 144, true)};
    audio_.emplace(id, std::move(audio));
    control(
        {{"type", "audio_start"},
         {"request",
          {{"id", id},
           {"kind", alternate ? "alternate" : "main"},
           {"audio_type", alternate ? "default" : "media"},
           {"playback", {{"sample_rate", rate}, {"channels", channels}, {"sample_type", "s16_le"}}},
           {"microphone", nullptr},
           {"playback_clock", "live"}}}});
    return id;
}
void DebugClient::pcm(uint64_t id, PcmBlock block) {
    auto it = audio_.find(id);
    if (it == audio_.end() || it->second.stopping)
        return;
    auto &stream = it->second;
    size_t frame_bytes = stream.channels * 2;
    require(block.data.size() % frame_bytes == 0, "partial AA PCM frame");
    if (block.position != stream.next_position)
        ++stream.discontinuities;
    stream.next_position = block.position + block.data.size() / frame_bytes;
    if (stream.suspended)
        return;
    size_t maximum = stream.rate / 50 * frame_bytes;
    for (size_t offset = 0; offset < block.data.size(); offset += maximum) {
        auto end = std::min(offset + maximum, block.data.size());
        stream.wire->send(packet(block.position + offset / frame_bytes,
                                 Bytes(block.data.begin() + offset, block.data.begin() + end)));
    }
}
void DebugClient::clear_audio(uint64_t id) {
    auto it = audio_.find(id);
    if (it != audio_.end())
        it->second.wire->clear_pending();
}
void DebugClient::stop_audio(uint64_t id) {
    auto it = audio_.find(id);
    if (it == audio_.end())
        return;
    if (it->second.stopping)
        return;
    it->second.stopping = true;
    auto stream = it->second.wire;
    if (!stream->connected()) {
        control({{"type", "audio_stop"}, {"id", id}});
        stream->close();
        audio_.erase(it);
        return;
    }
    auto weak = weak_from_this();
    stream->send({}, [weak, id, stream] {
        if (auto self = weak.lock()) {
            self->control({{"type", "audio_stop"}, {"id", id}});
            self->audio_.erase(id);
        }
        stream->close();
    });
}
void DebugClient::close() {
    if (stopped_)
        return;
    stopped_ = true;
    stats_timer_.cancel();
    if (control_)
        control_->close();
    if (video_)
        video_->close();
    for (auto &entry : audio_)
        entry.second.wire->close();
    audio_.clear();
}
} // namespace bridge
