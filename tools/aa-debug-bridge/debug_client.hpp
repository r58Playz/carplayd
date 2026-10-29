#pragma once
#include "core.hpp"
#include <boost/asio.hpp>
#include <deque>
#include <functional>
#include <memory>

namespace bridge {
using Tcp = boost::asio::ip::tcp;
using Done = std::function<void()>;
class WireSocket : public std::enable_shared_from_this<WireSocket> {
    struct Write {
        std::shared_ptr<Bytes> bytes;
        Done done;
    };
    boost::asio::io_service &io_;
    Tcp::resolver resolver_;
    Tcp::socket socket_;
    boost::asio::steady_timer connect_timer_, write_timer_;
    std::deque<Write> writes_;
    size_t bytes_ = 0, limit_, dropped_ = 0;
    bool connected_ = false, writing_ = false, closed_ = false, trim_;
    std::function<void(std::string)> error_;
    void pump();
    void fail(const std::string &message);

  public:
    WireSocket(boost::asio::io_service &io, size_t limit, bool trim,
               std::function<void(std::string)> error);
    void connect(const std::string &host, unsigned port, Done ready);
    void send(Bytes bytes, Done done = {});
    void send_json(const Json &json);
    void read_json(std::function<void(Json)> receive);
    void clear_pending();
    void close();
    size_t queued_bytes() const { return bytes_; }
    size_t dropped_packets() const { return dropped_; }
    bool connected() const { return connected_; }
};
class DebugClient : public std::enable_shared_from_this<DebugClient> {
    struct Audio {
        unsigned rate, channels;
        bool suspended = false, stopping = false;
        uint64_t next_position = 0, discontinuities = 0;
        std::shared_ptr<WireSocket> wire;
    };
    boost::asio::io_service &io_;
    std::string host_;
    unsigned port_;
    std::shared_ptr<WireSocket> control_, video_;
    std::map<uint64_t, Audio> audio_;
    uint64_t next_id_ = 1, video_frames_ = 0, previous_frames_ = 0;
    boost::asio::steady_timer stats_timer_;
    std::optional<Geometry> geometry_;
    bool stopped_ = false;
    void receive(Json message);
    void stats();
    void fail(const std::string &error);
    std::shared_ptr<WireSocket> wire(size_t limit, bool trim = false);

  public:
    std::function<void(const Geometry &, const Json &)> initialized;
    Done display_ready;
    std::function<void(const Json &)> input;
    std::function<void(uint64_t, const std::string &)> audio_status;
    std::function<void(std::string)> error;
    DebugClient(boost::asio::io_service &io, std::string host, unsigned port);
    void start();
    void control(const Json &message);
    void video(Bytes data, Done done);
    uint64_t start_audio(bool alternate);
    void pcm(uint64_t id, PcmBlock block);
    void stop_audio(uint64_t id);
    void clear_audio(uint64_t id);
    void close();
};
} // namespace bridge
