#pragma once
#include <array>
#include <chrono>
#include <cstdint>
#include <map>
#include <nlohmann/json.hpp>
#include <optional>
#include <string>
#include <vector>

namespace bridge {
using Json = nlohmann::json;
using Bytes = std::vector<uint8_t>;
uint64_t now_us();
void require(bool condition, const std::string &message);
Bytes packet(uint64_t position, const Bytes &data);
struct Geometry {
    unsigned width, height, fps, canvas_width, canvas_height, resolution, left, top;
    static Geometry from_init(const Json &init);
    Json crop() const;
    std::pair<unsigned, unsigned> touch(double x, double y) const;
};

// AA timestamps are microseconds in the phone's clock domain. Each USB session
// receives a new host anchor; the TCP display connection can survive replugging.
class VideoTimeline {
    std::optional<uint64_t> source_, host_;
    uint64_t last_ = 0;
    Bytes headers_;

  public:
    void reset();
    std::optional<Bytes> push(std::optional<uint64_t> stamp, const Bytes &data, uint64_t host_now);
};

struct PcmBlock {
    uint64_t position;
    Bytes data;
};
// AA timestamps jitter and round to whole frames, so a timestamp-derived
// position rarely continues the previous packet exactly. carplayd discards its
// buffered audio on every position discontinuity; follow the timestamp only
// when it differs from the contiguous position by more than the tolerance.
uint64_t aligned_position(uint64_t next, uint64_t stamped, uint64_t tolerance);
// AA sends the next media packet once the previous one is acknowledged, so
// acknowledging on receipt lets the phone run arbitrarily far ahead of real
// time, and carplayd's live playback discards whatever outruns its queue.
// Acknowledge once real time is within the lead of the audio received.
class AckPacer {
    std::optional<uint64_t> anchor_;
    uint64_t frames_ = 0;
    unsigned rate_;
    uint64_t lead_;

  public:
    AckPacer(unsigned rate, uint64_t lead_us) : rate_(rate), lead_(lead_us) {}
    void reset();
    // Host time (microseconds) at which to acknowledge `frames` received at `now`.
    uint64_t push(uint64_t frames, uint64_t now);
};
class PromptMixer {
    std::array<std::map<uint64_t, int16_t>, 2> samples_;
    std::array<uint64_t, 2> next_{};
    std::array<bool, 2> started_{};
    std::optional<uint64_t> epoch_;
    uint64_t cursor_ = 0;

  public:
    void reset();
    void push(unsigned channel, std::optional<uint64_t> stamp, const Bytes &data);
    PcmBlock take(unsigned frames = 80);
    bool empty() const;
};
int keycode(const std::string &key);
} // namespace bridge
