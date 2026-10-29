#include "core.hpp"
#include <algorithm>
#include <cmath>
#include <limits>
#include <stdexcept>

namespace bridge {
uint64_t now_us() {
    return std::chrono::duration_cast<std::chrono::microseconds>(
               std::chrono::steady_clock::now().time_since_epoch())
        .count();
}
void require(bool condition, const std::string &message) {
    if (!condition)
        throw std::runtime_error(message);
}
Bytes packet(uint64_t position, const Bytes &data) {
    require(!data.empty() && data.size() <= 4 * 1024 * 1024, "invalid data packet size");
    Bytes out;
    uint32_t size = uint32_t(data.size() + 8);
    for (int n = 0; n < 4; ++n)
        out.push_back(uint8_t(size >> (n * 8)));
    for (int n = 0; n < 8; ++n)
        out.push_back(uint8_t(position >> (n * 8)));
    out.insert(out.end(), data.begin(), data.end());
    return out;
}
Geometry Geometry::from_init(const Json &init) {
    auto dimension = [&](const char *key) {
        const auto &n = init.at(key);
        require(n.is_number_unsigned() || n.is_number_integer(),
                "display dimension must be an integer");
        int64_t value = n.get<int64_t>();
        require(value > 0 && value <= 8192, "display dimension out of range");
        return unsigned(value);
    };
    Geometry g{};
    g.width = dimension("display_width");
    g.height = dimension("display_height");
    auto fps = dimension("display_fps");
    require(g.width % 2 == 0 && g.height % 2 == 0, "display dimensions must be even");
    require(fps >= 30, "Android Auto requires a display supporting at least 30 FPS");
    g.fps = fps >= 60 ? 60 : 30;
    for (auto size : {std::array<unsigned, 3>{800, 480, 1}, {1280, 720, 2}, {1920, 1080, 3}}) {
        if (g.width <= size[0] && g.height <= size[1]) {
            g.canvas_width = size[0];
            g.canvas_height = size[1];
            g.resolution = size[2];
            // The CarPlay screen starts at the canvas origin; AA's UI margins
            // cover the right and bottom, and carplayd crops them away.
            g.left = 0;
            g.top = 0;
            return g;
        }
    }
    throw std::runtime_error("display exceeds supported Android Auto canvases");
}
Json Geometry::crop() const {
    if (width == canvas_width && height == canvas_height)
        return nullptr;
    return {{"left", left}, {"top", top}, {"width", width}, {"height", height}};
}
// AA lays its UI out in the margin-adjusted display and reads touches in that
// space: it does not subtract the UI margins, so positions exclude the crop offset.
std::pair<unsigned, unsigned> Geometry::touch(double x, double y) const {
    require(std::isfinite(x) && std::isfinite(y), "invalid touch coordinate");
    return {unsigned(std::lround(std::clamp(x, 0., 1.) * (width - 1))),
            unsigned(std::lround(std::clamp(y, 0., 1.) * (height - 1)))};
}
void VideoTimeline::reset() {
    source_.reset();
    host_.reset();
    headers_.clear();
}
std::optional<Bytes> VideoTimeline::push(std::optional<uint64_t> stamp, const Bytes &data,
                                         uint64_t host_now) {
    require(!data.empty() && data.size() <= 4 * 1024 * 1024, "invalid AA H.264 packet");
    bool vcl = false, annex = false;
    for (size_t i = 0; i + 3 < data.size(); ++i) {
        if (!data[i] && !data[i + 1] && data[i + 2] == 1) {
            annex = true;
            auto type = data[i + 3] & 31;
            if (type == 1 || type == 5)
                vcl = true;
        }
    }
    require(annex, "AA video is not Annex-B H.264");
    if (!vcl) {
        require(headers_.size() + data.size() <= 4 * 1024 * 1024,
                "AA codec headers exceed decoder limit");
        headers_.insert(headers_.end(), data.begin(), data.end());
        return std::nullopt;
    }
    uint64_t pts = host_now;
    if (stamp) {
        if (!source_) {
            source_ = *stamp;
            host_ = host_now;
        }
        if (*stamp >= *source_) {
            require(*stamp - *source_ <= std::numeric_limits<uint64_t>::max() - *host_,
                    "video timestamp overflow");
            pts = *host_ + (*stamp - *source_);
        }
    }
    // No B frames are negotiated (H.264 baseline), so a reset/backward phone
    // clock must not make the outgoing presentation clock run backwards.
    pts = std::max(pts, last_ + 1);
    last_ = pts;
    Bytes access = std::move(headers_);
    headers_.clear();
    access.insert(access.end(), data.begin(), data.end());
    return packet(pts, access);
}
void PromptMixer::reset() {
    samples_ = {};
    next_ = {};
    started_ = {};
    epoch_.reset();
    cursor_ = 0;
}
uint64_t aligned_position(uint64_t next, uint64_t stamped, uint64_t tolerance) {
    return stamped > next + tolerance || stamped + tolerance < next ? stamped : next;
}
void AckPacer::reset() {
    anchor_.reset();
    frames_ = 0;
}
uint64_t AckPacer::push(uint64_t frames, uint64_t now) {
    auto received = [&] { return frames_ * 1000000 / rate_; };
    // Jitter is caught up, but a phone that stalled past the lead owes no
    // deficit: bursting it afterwards would overflow carplayd's queue.
    if (!anchor_ || *anchor_ + received() + lead_ < now)
        anchor_ = now - received();
    frames_ += frames;
    return std::max(*anchor_ + received(), lead_) - lead_;
}
void PromptMixer::push(unsigned channel, std::optional<uint64_t> stamp, const Bytes &data) {
    require(channel < 2 && data.size() % 2 == 0, "invalid mono PCM packet");
    auto position = next_[channel];
    if (stamp) {
        if (!epoch_)
            epoch_ = *stamp;
        uint64_t stamped = *stamp >= *epoch_ ? (*stamp - *epoch_) * 16000 / 1000000 : 0;
        // Timestamps place each prompt against the other channel. Within a
        // prompt, 40 ms of jitter keeps samples contiguous instead of leaving
        // gaps and overlaps at every AA packet boundary.
        position = started_[channel] ? aligned_position(position, stamped, 640) : stamped;
    }
    started_[channel] = true;
    next_[channel] = position + data.size() / 2;
    // AA can deliver a prompt faster than real time (for example after a focus
    // change). Retain up to 500 ms ahead of playback rather than dropping it.
    auto newest = std::max(next_[0], next_[1]);
    if (newest > cursor_ + 8000)
        cursor_ = newest - 8000;
    for (auto &samples : samples_)
        samples.erase(samples.begin(), samples.lower_bound(cursor_));
    // Trim before insertion, so even a large AA block only allocates the
    // retained 500 ms instead of temporarily growing an unbounded sample map.
    size_t first = cursor_ > position ? std::min<uint64_t>(cursor_ - position, data.size() / 2) : 0;
    for (size_t i = first; i < data.size() / 2; ++i)
        samples_[channel][position + i] =
            int16_t(uint16_t(data[2 * i]) | (uint16_t(data[2 * i + 1]) << 8));
}
PcmBlock PromptMixer::take(unsigned frames) {
    PcmBlock block{cursor_, {}};
    for (unsigned i = 0; i < frames; ++i, ++cursor_) {
        int sum = 0;
        for (auto &samples : samples_) {
            auto it = samples.find(cursor_);
            if (it != samples.end()) {
                sum += it->second;
                samples.erase(it);
            }
        }
        auto sample = int16_t(std::clamp(sum, -32768, 32767));
        block.data.push_back(uint8_t(sample));
        block.data.push_back(uint8_t(uint16_t(sample) >> 8));
    }
    return block;
}
bool PromptMixer::empty() const { return samples_[0].empty() && samples_[1].empty(); }
int keycode(const std::string &key) {
    static const std::map<std::string, int> keys = {{"Home", 3},
                                                    {"Back", 4},
                                                    {"Primary", 23},
                                                    {"PlayPause", 85},
                                                    {"NextTrack", 87},
                                                    {"PreviousTrack", 88},
                                                    {"Play", 126},
                                                    {"Pause", 127},
                                                    {"TrackingIncrement", 90},
                                                    {"TrackingDecrement", 89},
                                                    {"VolumeUp", 24},
                                                    {"VolumeDown", 25},
                                                    {"Mute", 164},
                                                    {"Backspace", 67},
                                                    {"NavGuidance", 65538}};
    auto it = keys.find(key);
    return it == keys.end() ? -1 : it->second;
}
} // namespace bridge
