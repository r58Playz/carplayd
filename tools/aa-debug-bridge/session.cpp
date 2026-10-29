// Session/service sequencing adapted from opencardev/openauto, originally
// Copyright (C) 2018 f1x.studio (Michal Szwaj), Copyright (C) 2024 CubeOne.
// This file is distributed under GPL-3.0-or-later. See COPYING.
#include "session.hpp"
#include "aa_control.hpp"
#include "aa_protocol.hpp"
#include <aap_protobuf/service/media/sink/message/KeyCode.pb.h>
#include <aap_protobuf/service/sensorsource/message/DrivingStatus.pb.h>
#include <aasdk/Channel/Control/ControlServiceChannel.hpp>
#include <aasdk/Channel/Control/IControlServiceChannelEventHandler.hpp>
#include <aasdk/Channel/InputSource/InputSourceService.hpp>
#include <aasdk/Channel/MediaSource/Audio/MicrophoneAudioChannel.hpp>
#include <aasdk/Channel/MediaPlaybackStatus/MediaPlaybackStatusService.hpp>
#include <aasdk/Channel/MediaSink/Audio/Channel/GuidanceAudioChannel.hpp>
#include <aasdk/Channel/MediaSink/Audio/Channel/MediaAudioChannel.hpp>
#include <aasdk/Channel/MediaSink/Audio/Channel/SystemAudioChannel.hpp>
#include <aasdk/Channel/MediaSink/Video/Channel/VideoChannel.hpp>
#include <aasdk/Channel/SensorSource/SensorSourceService.hpp>
#include <aasdk/Messenger/Cryptor.hpp>
#include <aasdk/Messenger/MessageInStream.hpp>
#include <aasdk/Messenger/MessageOutStream.hpp>
#include <aasdk/Messenger/Messenger.hpp>
#include <aasdk/Transport/SSLWrapper.hpp>
#include <aasdk/Transport/USBTransport.hpp>
#include <aasdk/USB/AOAPDevice.hpp>
#include <algorithm>
#include <cmath>
#include <iostream>
#include <set>
#include <type_traits>

namespace bridge {
namespace control = aap_protobuf::service::control::message;
namespace media = aap_protobuf::service::media::shared::message;
namespace sink = aap_protobuf::service::media::sink::message;
namespace inputmsg = aap_protobuf::service::inputsource::message;
namespace sensormsg = aap_protobuf::service::sensorsource::message;
using Send = aasdk::channel::SendPromise;
using Id = aasdk::messenger::ChannelId;
constexpr auto success = aap_protobuf::shared::MessageStatus::STATUS_SUCCESS;

struct Service {
    virtual ~Service() = default;
    virtual void start() = 0;
    virtual void features(control::ServiceDiscoveryResponse &) = 0;
};
class Session : public aasdk::channel::control::IControlServiceChannelEventHandler,
                public std::enable_shared_from_this<Session> {
    aasdk::messenger::ICryptor::Pointer cryptor_;
    std::shared_ptr<aasdk::transport::USBTransport> transport_;
    aasdk::messenger::IMessenger::Pointer messenger_;
    std::shared_ptr<aasdk::channel::control::ControlServiceChannel> control_;
    std::vector<std::shared_ptr<Service>> services_;
    boost::asio::steady_timer ping_timer_, prompt_timer_, handshake_timer_;
    std::optional<uint64_t> pending_ping_;
    bool ping_started_ = false;
    Done closed_;
    VideoTimeline timeline_;
    PromptMixer mixer_;
    std::optional<uint64_t> main_audio_, alternate_audio_;
    std::optional<uint64_t> media_epoch_;
    uint64_t media_next_ = 0;
    // A phone may catch up after a hiccup by as much as carplayd's playback cushion.
    AckPacer media_pacer_{48000, 50000};
    std::array<bool, 2> prompt_active_{};
    bool mixer_running_ = false, media_suspended_ = false;
    std::chrono::steady_clock::time_point prompt_deadline_;
    void ping();
    void mix();
    void receive() {
        if (!stopped)
            control_->receive(shared_from_this());
    }

  public:
    boost::asio::io_service &io;
    boost::asio::io_service::strand strand;
    Geometry geometry;
    Json devices;
    std::shared_ptr<DebugClient> debug;
    bool stopped = false;
    Session(boost::asio::io_service &service, Geometry g, Json inputs,
            std::shared_ptr<DebugClient> client, Done closed)
        : ping_timer_(service), prompt_timer_(service), handshake_timer_(service),
          closed_(std::move(closed)), io(service), strand(service), geometry(g),
          devices(std::move(inputs)), debug(std::move(client)) {}
    void start(aasdk::usb::IUSBWrapper &usb, aasdk::usb::DeviceHandle handle);
    void start(aasdk::messenger::IMessenger::Pointer messenger,
               aasdk::messenger::ICryptor::Pointer cryptor);
    void stop();
    void set_closed(Done closed) { closed_ = std::move(closed); }
    void fail(const std::string &reason) {
        if (!stopped) {
            std::cerr << "AA session: " << reason << '\n';
            stop();
        }
    }
    Send::Pointer promise(Done done = {}) {
        auto result = Send::defer(strand);
        auto weak = weak_from_this();
        result->then(
            [weak, done = std::move(done)] {
                if (auto s = weak.lock(); s && !s->stopped) {
                    try {
                        if (done)
                            done();
                    } catch (const std::exception &e) {
                        s->fail(e.what());
                    }
                }
            },
            [weak](auto e) {
                if (auto s = weak.lock())
                    s->onChannelError(e);
            });
        return result;
    }
    void video(std::optional<uint64_t> stamp, const Bytes &data, Done ack) {
        if (stopped)
            return;
        try {
            auto out = timeline_.push(stamp, data, now_us());
            if (out)
                debug->video(std::move(*out), std::move(ack));
            else
                ack();
        } catch (const std::exception &e) {
            fail(e.what());
        }
    }
    void app_state() {
        debug->control({{"type", "audio_app_state"},
                        {"speech", "none"},
                        {"phone_call", false},
                        {"turn_by_turn", prompt_active_[0]}});
    }
    void audio_start(int kind);
    void audio_stop(int kind);
    void audio(int kind, std::optional<uint64_t> stamp, const Bytes &data, Done ack);
    void focus(control::AudioFocusStateType type) {
        control::AudioFocusNotification response;
        response.set_focus_state(type);
        response.set_unsolicited(true);
        control_->sendAudioFocusResponse(response, promise());
    }
    void onVersionResponse(uint16_t major, uint16_t minor,
                           aap_protobuf::shared::MessageStatus status) override;
    void onHandshake(const aasdk::common::DataConstBuffer &data) override;
    void onServiceDiscoveryRequest(const control::ServiceDiscoveryRequest &) override;
    void onAudioFocusRequest(const control::AudioFocusRequest &request) override {
        auto state = request.audio_focus_type() == control::AUDIO_FOCUS_RELEASE
                         ? control::AUDIO_FOCUS_STATE_LOSS
                     : media_suspended_ ? control::AUDIO_FOCUS_STATE_GAIN_TRANSIENT_GUIDANCE_ONLY
                     : request.audio_focus_type() == control::AUDIO_FOCUS_GAIN
                         ? control::AUDIO_FOCUS_STATE_GAIN
                         : control::AUDIO_FOCUS_STATE_GAIN_TRANSIENT;
        control::AudioFocusNotification response;
        response.set_focus_state(state);
        response.set_unsolicited(false);
        control_->sendAudioFocusResponse(response, promise());
        receive();
    }
    void onByeByeRequest(const control::ByeByeRequest &) override {
        control::ByeByeResponse response;
        auto self = shared_from_this();
        control_->sendShutdownResponse(response, promise([self] { self->stop(); }));
    }
    void onByeByeResponse(const control::ByeByeResponse &) override { stop(); }
    void onBatteryStatusNotification(const control::BatteryStatusNotification &) override {
        receive();
    }
    void onNavigationFocusRequest(const control::NavFocusRequestNotification &) override {
        control::NavFocusNotification response;
        response.set_focus_type(control::NAV_FOCUS_PROJECTED);
        control_->sendNavigationFocusResponse(response, promise());
        receive();
    }
    void onVoiceSessionRequest(const control::VoiceSessionNotification &) override { receive(); }
    void onPingRequest(const control::PingRequest &request) override {
        control::PingResponse response;
        response.set_timestamp(request.timestamp());
        control_->sendPingResponse(response, promise());
        receive();
    }
    void onPingResponse(const control::PingResponse &response) override {
        if (pending_ping_ && response.timestamp() >= 0 &&
            uint64_t(response.timestamp()) == *pending_ping_) {
            pending_ping_.reset();
            ping_timer_.expires_after(std::chrono::seconds(1));
            auto self = shared_from_this();
            ping_timer_.async_wait([self](auto ec) {
                if (!ec)
                    self->ping();
            });
        } else {
            std::cerr << "AA unexpected ping response: " << response.timestamp() << '\n';
        }
        receive();
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (e.getCode() != aasdk::error::ErrorCode::OPERATION_ABORTED)
            fail(e.what());
    }
};

// A sink owns its AA channel while Session owns the service lifetime. AA's
// receive callbacks are one-shot, so each handled event explicitly re-arms it.
template <bool Video>
class MediaSink : public Service,
                  public std::conditional_t<
                      Video, aasdk::channel::mediasink::video::IVideoMediaSinkServiceEventHandler,
                      aasdk::channel::mediasink::audio::IAudioMediaSinkServiceEventHandler>,
                  public std::enable_shared_from_this<MediaSink<Video>> {
    using Channel =
        std::conditional_t<Video, aasdk::channel::mediasink::video::IVideoMediaSinkService,
                           aasdk::channel::mediasink::audio::IAudioMediaSinkService>;
    std::weak_ptr<Session> owner_;
    std::shared_ptr<Channel> channel_;
    int kind_, session_ = -1;
    void video_focus() {
        if constexpr (Video) {
            auto s = owner_.lock();
            if (!s || s->stopped)
                return;
            aap_protobuf::service::media::video::message::VideoFocusNotification response;
            response.set_focus(aap_protobuf::service::media::video::message::VIDEO_FOCUS_PROJECTED);
            response.set_unsolicited(false);
            channel_->sendVideoFocusIndication(response, s->promise());
        }
    }
    void receive() {
        if (auto s = owner_.lock(); s && !s->stopped)
            channel_->receive(this->shared_from_this());
    }
    void data(std::optional<uint64_t> stamp, const aasdk::common::DataConstBuffer &buffer) {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        try {
            require(session_ >= 0 || Video, "AA media arrived without an active session");
            Bytes copy(buffer.cdata, buffer.cdata + buffer.size);
            auto self = this->shared_from_this();
            auto session = session_;
            auto ack = [self, session] {
                if (auto s = self->owner_.lock(); s && !s->stopped && session == self->session_) {
                    aap_protobuf::service::media::source::message::Ack ack;
                    ack.set_session_id(session);
                    ack.set_ack(1);
                    self->channel_->sendMediaAckIndication(ack, s->promise());
                }
            };
            if constexpr (Video)
                s->video(stamp, copy, std::move(ack));
            else
                s->audio(kind_, stamp, copy, std::move(ack));
            receive();
        } catch (const std::exception &e) {
            s->fail(e.what());
        }
    }

  public:
    MediaSink(std::shared_ptr<Session> owner, std::shared_ptr<Channel> channel, int kind)
        : owner_(owner), channel_(std::move(channel)), kind_(kind) {}
    void start() override { receive(); }
    void features(control::ServiceDiscoveryResponse &response) override {
        auto s = owner_.lock();
        auto *entry = response.add_channels();
        entry->set_id(unsigned(channel_->getId()));
        auto *service = entry->mutable_media_sink_service();
        service->set_available_while_in_call(false);
        if constexpr (Video) {
            video_configuration(*service, s->geometry);
        } else {
            service->set_available_type(media::MEDIA_CODEC_AUDIO_PCM);
            service->set_audio_type(kind_ == 0   ? sink::AUDIO_STREAM_MEDIA
                                    : kind_ == 1 ? sink::AUDIO_STREAM_GUIDANCE
                                                 : sink::AUDIO_STREAM_SYSTEM_AUDIO);
            auto *config = service->add_audio_configs();
            config->set_sampling_rate(kind_ ? 16000 : 48000);
            config->set_number_of_channels(kind_ ? 1 : 2);
            config->set_number_of_bits(16);
        }
    }
    void onChannelOpenRequest(const control::ChannelOpenRequest &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        control::ChannelOpenResponse response;
        response.set_status(success);
        std::cerr << "AA " << (Video ? "video" : "audio") << " channel "
                  << unsigned(channel_->getId()) << " opened\n";
        channel_->sendChannelOpenResponse(response, s->promise());
        receive();
    }
    void onMediaChannelSetupRequest(const media::Setup &request) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        if (request.type() !=
            (Video ? media::MEDIA_CODEC_VIDEO_H264_BP : media::MEDIA_CODEC_AUDIO_PCM))
            return s->fail("unsupported AA codec");
        media::Config config;
        config.set_status(media::Config::STATUS_READY);
        config.set_max_unacked(1);
        config.add_configuration_indices(0);
        auto self = this->shared_from_this();
        channel_->sendChannelSetupResponse(config, s->promise([self] {
            // The phone waits for projection focus before starting video.
            self->video_focus();
        }));
        receive();
    }
    void onMediaChannelStartIndication(const media::Start &start) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        session_ = start.session_id();
        if constexpr (!Video)
            s->audio_start(kind_);
        receive();
    }
    void onMediaChannelStopIndication(const media::Stop &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        session_ = -1;
        if constexpr (!Video)
            s->audio_stop(kind_);
        receive();
    }
    void onMediaWithTimestampIndication(uint64_t stamp,
                                        const aasdk::common::DataConstBuffer &buffer) override {
        data(stamp, buffer);
    }
    void onMediaIndication(const aasdk::common::DataConstBuffer &buffer) override {
        data(std::nullopt, buffer);
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (auto s = owner_.lock())
            s->onChannelError(e);
    }
    void onVideoFocusRequest(
        const aap_protobuf::service::media::video::message::VideoFocusRequestNotification &) {
        if constexpr (Video) {
            auto s = owner_.lock();
            if (!s || s->stopped)
                return;
            video_focus();
            receive();
        }
    }
};

class InputService : public Service,
                     public aasdk::channel::inputsource::IInputSourceServiceEventHandler,
                     public std::enable_shared_from_this<InputService> {
    std::weak_ptr<Session> owner_;
    std::shared_ptr<aasdk::channel::inputsource::InputSourceService> channel_;
    std::set<int> keys_;
    std::set<std::pair<std::string, int>> held_;
    struct Contact {
        unsigned id, x, y;
        bool pad;
    };
    std::map<std::string, Contact> contacts_;
    std::map<std::string, int64_t> previous_axes_;
    std::set<std::string> warned_;
    bool touchscreen_ = false, touchpad_ = false, bound_ = false, rotary_ = false;
    void receive() {
        if (auto s = owner_.lock(); s && !s->stopped)
            channel_->receive(shared_from_this());
    }
    void send(inputmsg::InputReport report) {
        auto s = owner_.lock();
        if (!s || s->stopped || !bound_)
            return;
        report.set_timestamp(now_us());
        channel_->sendInputReport(report, s->promise());
    }
    void warn(const std::string &text) {
        if (warned_.insert(text).second)
            std::cerr << "input: " << text << '\n';
    }

  public:
    InputService(std::shared_ptr<Session> owner,
                 aasdk::messenger::IMessenger::Pointer messenger)
        : owner_(owner), channel_(std::make_shared<aasdk::channel::inputsource::InputSourceService>(
                             owner->strand, messenger)) {
        for (const auto &device : owner->devices) {
            if (device.is_string()) {
                for (const auto &name :
                     {"Play", "Pause", "NextTrack", "PreviousTrack", "PlayPause", "Mute",
                      "VolumeUp", "VolumeDown", "TrackingIncrement", "TrackingDecrement"})
                    keys_.insert(keycode(name));
            } else if (device.contains("Network")) {
                const auto &info = device.at("Network");
                for (const auto &name : info.at("keys")) {
                    auto code = keycode(name);
                    if (code >= 0)
                        keys_.insert(code);
                }
                if (!info.at("touch").is_null()) {
                    touchscreen_ |= info.at("touch").contains("Touchscreen");
                    touchpad_ |= info.at("touch").contains("Touchpad");
                }
                for (const auto &axis : info.at("axes")) {
                    auto usage = axis.at("axis").get<std::string>();
                    rotary_ |= usage == "Dial" || usage == "Wheel" || usage == "Rz";
                }
            }
        }
        if (rotary_)
            keys_.insert(sink::KEYCODE_ROTARY_CONTROLLER);
    }
    void start() override {
        auto weak = weak_from_this();
        owner_.lock()->debug->input = [weak](const auto &m) {
            if (auto s = weak.lock())
                s->event(m);
        };
        receive();
    }
    void features(control::ServiceDiscoveryResponse &response) override {
        auto s = owner_.lock();
        auto *service = response.add_channels();
        service->set_id(unsigned(channel_->getId()));
        auto *input = service->mutable_input_source_service();
        for (auto code : keys_)
            input->add_keycodes_supported(code);
        if (touchscreen_) {
            auto *touch = input->add_touchscreen();
            touch->set_width(s->geometry.width);
            touch->set_height(s->geometry.height);
            touch->set_type(inputmsg::CAPACITIVE);
        }
        if (touchpad_) {
            auto *pad = input->add_touchpad();
            pad->set_width(s->geometry.width);
            pad->set_height(s->geometry.height);
            pad->set_ui_absolute(true);
        }
    }
    void onChannelOpenRequest(const control::ChannelOpenRequest &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        control::ChannelOpenResponse response;
        response.set_status(success);
        channel_->sendChannelOpenResponse(response, s->promise());
        receive();
    }
    void onKeyBindingRequest(const sink::KeyBindingRequest &request) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        bound_ = std::all_of(request.keycodes().begin(), request.keycodes().end(),
                             [&](int key) { return keys_.count(key); });
        sink::KeyBindingResponse response;
        response.set_status(bound_ ? success : aap_protobuf::shared::STATUS_KEYCODE_NOT_BOUND);
        channel_->sendKeyBindingResponse(response, s->promise());
        receive();
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (auto s = owner_.lock())
            s->onChannelError(e);
    }
    void event(const Json &message) {
        auto s = owner_.lock();
        if (!s || s->stopped || !bound_)
            return;
        const auto &descriptor = message.at("device");
        std::string device =
            descriptor.is_string()
                ? descriptor.get<std::string>()
                : "Network:" + descriptor.at("Network").at("id").get<std::string>();
        bool pad = !descriptor.is_string() && !descriptor.at("Network").at("touch").is_null() &&
                   descriptor.at("Network").at("touch").contains("Touchpad");
        for (const auto &event : message.at("events")) {
            if (event.contains("Key")) {
                const auto &key = event.at("Key");
                std::string name = key.at("key");
                int code = keycode(name);
                if (code < 0 || !keys_.count(code)) {
                    warn("unsupported key " + name);
                    continue;
                }
                bool previous = std::any_of(held_.begin(), held_.end(),
                                            [&](const auto &k) { return k.second == code; });
                if (key.at("pressed").get<bool>())
                    held_.insert({device, code});
                else
                    held_.erase({device, code});
                bool down = std::any_of(held_.begin(), held_.end(),
                                        [&](const auto &k) { return k.second == code; });
                if (previous == down)
                    continue;
                inputmsg::InputReport report;
                auto *out = report.mutable_key_event()->add_keys();
                out->set_keycode(code);
                out->set_down(down);
                out->set_metastate(0);
                out->set_longpress(false);
                send(std::move(report));
            } else if (event.contains("TouchEvent")) {
                for (const auto &touch : event.at("TouchEvent").at("contacts")) {
                    std::string id = device + ":" + std::to_string(touch.at("id").get<uint32_t>());
                    auto it = contacts_.find(id);
                    bool existing = it != contacts_.end(), down = touch.at("touching");
                    if (!existing && !down)
                        continue;
                    auto [x, y] = s->geometry.touch(touch.at("x"), touch.at("y"));
                    if (!existing) {
                        // Android pointer IDs must stay within 0-31 (MAX_POINTER_ID);
                        // an out-of-range ID crashes AA's input injection. Like
                        // Android, reuse the lowest ID that is not held down.
                        unsigned pointer = 0;
                        while (pointer < 32 &&
                               std::any_of(contacts_.begin(), contacts_.end(),
                                           [&](const auto &c) { return c.second.id == pointer; }))
                            ++pointer;
                        if (pointer == 32) {
                            warn("more than 32 simultaneous touch contacts");
                            continue;
                        }
                        it = contacts_.emplace(id, Contact{pointer, x, y, pad}).first;
                    }
                    it->second.x = x;
                    it->second.y = y;
                    inputmsg::InputReport report;
                    auto *out =
                        pad ? report.mutable_touchpad_event() : report.mutable_touch_event();
                    unsigned index = 0, action_index = 0;
                    for (const auto &entry : contacts_)
                        if (entry.second.pad == pad) {
                            if (entry.first == id)
                                action_index = index;
                            auto *point = out->add_pointer_data();
                            point->set_pointer_id(entry.second.id);
                            point->set_x(entry.second.x);
                            point->set_y(entry.second.y);
                            ++index;
                        }
                    out->set_action_index(action_index);
                    out->set_action(
                        !down ? (index == 1 ? inputmsg::ACTION_UP : inputmsg::ACTION_POINTER_UP)
                        : existing   ? inputmsg::ACTION_MOVED
                        : index == 1 ? inputmsg::ACTION_DOWN
                                     : inputmsg::ACTION_POINTER_DOWN);
                    send(std::move(report));
                    if (!down)
                        contacts_.erase(it);
                }
            } else if (event.contains("Axis")) {
                const auto &axis = event.at("Axis");
                bool handled = false;
                if (!descriptor.is_string())
                    for (const auto &capability : descriptor.at("Network").at("axes"))
                        if (capability.at("id") == axis.at("id")) {
                            std::string usage = capability.at("axis");
                            if (usage != "Dial" && usage != "Wheel" && usage != "Rz")
                                break;
                            int64_t value = axis.at("value");
                            long double delta = value;
                            std::string id = device + ":" + axis.at("id").dump();
                            if (!capability.at("relative").get<bool>()) {
                                auto previous = previous_axes_.find(id);
                                if (previous == previous_axes_.end()) {
                                    previous_axes_[id] = value;
                                    handled = true;
                                    break;
                                }
                                delta = static_cast<long double>(value) - previous->second;
                                previous->second = value;
                                long double range =
                                    capability.at("logical_maximum").get<long double>() -
                                    capability.at("logical_minimum").get<long double>() + 1;
                                if (range > 1 && std::abs(delta) > range / 2)
                                    delta += delta > 0 ? -range : range;
                            }
                            inputmsg::InputReport report;
                            auto *out = report.mutable_relative_event()->add_data();
                            out->set_keycode(sink::KEYCODE_ROTARY_CONTROLLER);
                            out->set_delta(
                                int32_t(std::clamp<long double>(delta, INT32_MIN, INT32_MAX)));
                            send(std::move(report));
                            handled = true;
                        }
                if (!handled)
                    warn("unsupported axis " + axis.at("id").dump());
            }
        }
    }
};

class Sensors : public Service,
                public aasdk::channel::sensorsource::ISensorSourceServiceEventHandler,
                public std::enable_shared_from_this<Sensors> {
    std::weak_ptr<Session> owner_;
    std::shared_ptr<aasdk::channel::sensorsource::SensorSourceService> channel_;
    void receive() {
        if (auto s = owner_.lock(); s && !s->stopped)
            channel_->receive(shared_from_this());
    }

  public:
    Sensors(std::shared_ptr<Session> s, aasdk::messenger::IMessenger::Pointer messenger)
        : owner_(s), channel_(std::make_shared<aasdk::channel::sensorsource::SensorSourceService>(
                         s->strand, messenger)) {}
    void start() override { receive(); }
    void features(control::ServiceDiscoveryResponse &response) override {
        auto *service = response.add_channels();
        service->set_id(unsigned(channel_->getId()));
        auto *sensors = service->mutable_sensor_source_service();
        sensors->add_sensors()->set_sensor_type(sensormsg::SENSOR_DRIVING_STATUS_DATA);
        sensors->add_sensors()->set_sensor_type(sensormsg::SENSOR_NIGHT_MODE);
    }
    void onChannelOpenRequest(const control::ChannelOpenRequest &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        control::ChannelOpenResponse response;
        response.set_status(success);
        channel_->sendChannelOpenResponse(response, s->promise());
        receive();
    }
    void onSensorStartRequest(const sensormsg::SensorRequest &request) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        sensormsg::SensorStartResponseMessage response;
        response.set_status(success);
        auto self = shared_from_this();
        auto type = request.type();
        channel_->sendSensorStartResponse(response, s->promise([self, type] {
            auto s = self->owner_.lock();
            if (!s || s->stopped)
                return;
            sensormsg::SensorBatch batch;
            if (type == sensormsg::SENSOR_DRIVING_STATUS_DATA)
                batch.add_driving_status_data()->set_status(sensormsg::DRIVE_STATUS_UNRESTRICTED);
            else if (type == sensormsg::SENSOR_NIGHT_MODE)
                batch.add_night_mode_data()->set_night_mode(false);
            self->channel_->sendSensorEventIndication(batch, s->promise());
        }));
        receive();
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (auto s = owner_.lock())
            s->onChannelError(e);
    }
};

class Playback
    : public Service,
      public aasdk::channel::mediaplaybackstatus::IMediaPlaybackStatusServiceEventHandler,
      public std::enable_shared_from_this<Playback> {
    std::weak_ptr<Session> owner_;
    std::shared_ptr<aasdk::channel::mediaplaybackstatus::MediaPlaybackStatusService> channel_;
    Json snapshot_ = {{"media_item", Json::object()}};
    void receive() {
        if (auto s = owner_.lock(); s && !s->stopped)
            channel_->receive(shared_from_this());
    }
    void publish() {
        if (auto s = owner_.lock(); s && !s->stopped)
            s->debug->control({{"type", "now_playing_update"}, {"now_playing", snapshot_}});
        receive();
    }

  public:
    Playback(std::shared_ptr<Session> s, aasdk::messenger::IMessenger::Pointer messenger)
        : owner_(s),
          channel_(
              std::make_shared<aasdk::channel::mediaplaybackstatus::MediaPlaybackStatusService>(
                  s->strand, messenger)) {}
    void start() override { receive(); }
    void features(control::ServiceDiscoveryResponse &response) override {
        auto *service = response.add_channels();
        service->set_id(unsigned(channel_->getId()));
        service->mutable_media_playback_service();
    }
    void onChannelOpenRequest(const control::ChannelOpenRequest &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        control::ChannelOpenResponse response;
        response.set_status(success);
        channel_->sendChannelOpenResponse(response, s->promise());
        receive();
    }
    void onMetadataUpdate(const aap_protobuf::service::mediaplayback::message::MediaPlaybackMetadata
                              &metadata) override {
        metadata_update(snapshot_, metadata);
        publish();
    }
    void onPlaybackUpdate(const aap_protobuf::service::mediaplayback::message::MediaPlaybackStatus
                              &playback) override {
        playback_update(snapshot_, playback);
        publish();
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (auto s = owner_.lock())
            s->onChannelError(e);
    }
};

// AA requires an audio input service: without one the phone ends the session
// after service discovery ("Critical error 2 detail: 24 msg: No audio/mic").
// Car microphone audio is not forwarded yet, so an opened microphone streams
// silence in real time rather than leaving voice requests without any input.
class Microphone : public Service,
                   public aasdk::channel::mediasource::IMediaSourceServiceEventHandler,
                   public std::enable_shared_from_this<Microphone> {
    static constexpr unsigned rate = 16000, block = rate / 50;
    std::weak_ptr<Session> owner_;
    std::shared_ptr<aasdk::channel::mediasource::audio::MicrophoneAudioChannel> channel_;
    boost::asio::steady_timer timer_;
    std::chrono::steady_clock::time_point deadline_;
    bool open_ = false;
    void receive() {
        if (auto s = owner_.lock(); s && !s->stopped)
            channel_->receive(shared_from_this());
    }
    void silence() {
        deadline_ += std::chrono::milliseconds(1000 * block / rate);
        auto now = std::chrono::steady_clock::now();
        if (deadline_ + std::chrono::milliseconds(60) < now)
            deadline_ = now;
        timer_.expires_at(deadline_);
        auto self = shared_from_this();
        timer_.async_wait([self](auto ec) {
            auto s = self->owner_.lock();
            if (ec || !s || s->stopped || !self->open_)
                return;
            self->channel_->sendMediaSourceWithTimestampIndication(
                now_us(), aasdk::common::Data(block * 2), s->promise());
            self->silence();
        });
    }

  public:
    Microphone(std::shared_ptr<Session> s, aasdk::messenger::IMessenger::Pointer messenger)
        : owner_(s),
          channel_(std::make_shared<aasdk::channel::mediasource::audio::MicrophoneAudioChannel>(
              s->strand, messenger)),
          timer_(s->io) {}
    void start() override { receive(); }
    void features(control::ServiceDiscoveryResponse &response) override {
        auto *service = response.add_channels();
        service->set_id(unsigned(channel_->getId()));
        auto *source = service->mutable_media_source_service();
        source->set_available_type(media::MEDIA_CODEC_AUDIO_PCM);
        auto *config = source->mutable_audio_config();
        config->set_sampling_rate(rate);
        config->set_number_of_bits(16);
        config->set_number_of_channels(1);
    }
    void onChannelOpenRequest(const control::ChannelOpenRequest &) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        control::ChannelOpenResponse response;
        response.set_status(success);
        std::cerr << "AA microphone channel " << unsigned(channel_->getId()) << " opened\n";
        channel_->sendChannelOpenResponse(response, s->promise());
        receive();
    }
    void onMediaChannelSetupRequest(const media::Setup &request) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        if (request.type() != media::MEDIA_CODEC_AUDIO_PCM)
            return s->fail("unsupported AA microphone codec");
        media::Config config;
        config.set_status(media::Config::STATUS_READY);
        config.set_max_unacked(1);
        config.add_configuration_indices(0);
        channel_->sendChannelSetupResponse(config, s->promise());
        receive();
    }
    void onMediaSourceOpenRequest(
        const aap_protobuf::service::media::source::message::MicrophoneRequest &request) override {
        auto s = owner_.lock();
        if (!s || s->stopped)
            return;
        std::cerr << "AA microphone " << (request.open() ? "opened (sending silence)" : "closed")
                  << '\n';
        aap_protobuf::service::media::source::message::MicrophoneResponse response;
        response.set_status(success);
        response.set_session_id(0);
        channel_->sendMicrophoneOpenResponse(response, s->promise());
        bool was_open = std::exchange(open_, request.open());
        if (open_ && !was_open) {
            deadline_ = std::chrono::steady_clock::now();
            silence();
        } else if (!open_)
            timer_.cancel();
        receive();
    }
    void onMediaChannelAckIndication(
        const aap_protobuf::service::media::source::message::Ack &) override {
        receive();
    }
    void onChannelError(const aasdk::error::Error &e) override {
        if (auto s = owner_.lock())
            s->onChannelError(e);
    }
};

void Session::start(aasdk::usb::IUSBWrapper &usb, aasdk::usb::DeviceHandle handle) {
    auto device = aasdk::usb::AOAPDevice::create(usb, io, std::move(handle));
    transport_ = std::make_shared<aasdk::transport::USBTransport>(io, std::move(device));
    cryptor_ = std::make_shared<aasdk::messenger::Cryptor>(
        std::make_shared<aasdk::transport::SSLWrapper>());
    cryptor_->init();
    messenger_ = std::make_shared<aasdk::messenger::Messenger>(
        io, std::make_shared<aasdk::messenger::MessageInStream>(io, transport_, cryptor_),
        std::make_shared<aasdk::messenger::MessageOutStream>(io, transport_, cryptor_));
    start(messenger_, cryptor_);
}
void Session::start(aasdk::messenger::IMessenger::Pointer messenger,
                    aasdk::messenger::ICryptor::Pointer cryptor) {
    messenger_ = std::move(messenger);
    cryptor_ = std::move(cryptor);
    control_ = std::make_shared<ControlChannel>(strand, messenger_);
    auto self = shared_from_this();
    services_.push_back(std::make_shared<MediaSink<true>>(
        self,
        std::make_shared<aasdk::channel::mediasink::video::channel::VideoChannel>(strand,
                                                                                  messenger_),
        -1));
    services_.push_back(std::make_shared<MediaSink<false>>(
        self,
        std::make_shared<aasdk::channel::mediasink::audio::channel::MediaAudioChannel>(strand,
                                                                                       messenger_),
        0));
    services_.push_back(std::make_shared<MediaSink<false>>(
        self,
        std::make_shared<aasdk::channel::mediasink::audio::channel::GuidanceAudioChannel>(
            strand, messenger_),
        1));
    services_.push_back(std::make_shared<MediaSink<false>>(
        self,
        std::make_shared<aasdk::channel::mediasink::audio::channel::SystemAudioChannel>(strand,
                                                                                        messenger_),
        2));
    services_.push_back(std::make_shared<Microphone>(self, messenger_));
    services_.push_back(std::make_shared<InputService>(self, messenger_));
    services_.push_back(std::make_shared<Sensors>(self, messenger_));
    services_.push_back(std::make_shared<Playback>(self, messenger_));
    for (const auto &service : services_)
        service->start();
    auto weak = weak_from_this();
    debug->audio_status = [weak](uint64_t id, const std::string &state) {
        if (auto s = weak.lock(); s && !s->stopped) {
            if (s->main_audio_ && id == *s->main_audio_) {
                s->media_suspended_ = state != "active";
                s->focus(s->media_suspended_
                             ? control::AUDIO_FOCUS_STATE_GAIN_TRANSIENT_GUIDANCE_ONLY
                             : control::AUDIO_FOCUS_STATE_GAIN);
            }
            if (state == "error")
                s->fail("carplayd rejected audio stream " + std::to_string(id));
        }
    };
    handshake_timer_.expires_after(std::chrono::seconds(15));
    handshake_timer_.async_wait([self](auto ec) {
        if (!ec)
            self->fail("AA handshake timed out");
    });
    control_->sendVersionRequest(promise());
    receive();
}
void Session::onVersionResponse(uint16_t major, uint16_t minor,
                                aap_protobuf::shared::MessageStatus status) {
    std::cerr << "AA version " << major << '.' << minor << ", status " << status << '\n';
    if (status == aap_protobuf::shared::STATUS_NO_COMPATIBLE_VERSION)
        return fail("incompatible AA version");
    try {
        cryptor_->doHandshake();
        control_->sendHandshake(cryptor_->readHandshakeBuffer(), promise());
        receive();
    } catch (const std::exception &e) {
        fail(e.what());
    }
}
void Session::onHandshake(const aasdk::common::DataConstBuffer &data) {
    try {
        cryptor_->writeHandshakeBuffer(data);
        bool complete = cryptor_->doHandshake();
        auto bytes = cryptor_->readHandshakeBuffer();
        if (!bytes.empty())
            control_->sendHandshake(bytes, promise());
        if (complete) {
            control::AuthResponse response;
            response.set_status(success);
            control_->sendAuthComplete(response, promise());
        }
        receive();
    } catch (const std::exception &e) {
        fail(e.what());
    }
}
void Session::onServiceDiscoveryRequest(const control::ServiceDiscoveryRequest &) {
    control::ServiceDiscoveryResponse response;
    response.set_display_name("carplayd AA debug bridge");
    response.set_probe_for_support(false);
    response.set_driver_position(control::DRIVER_POSITION_LEFT);
    auto *info = response.mutable_headunit_info();
    info->set_make("carplayd");
    info->set_model("Debug bridge");
    info->set_year("2026");
    info->set_vehicle_id("carplayd-debug");
    info->set_head_unit_make("carplayd");
    info->set_head_unit_model("aasdk bridge");
    info->set_head_unit_software_build("1");
    info->set_head_unit_software_version("1");
    auto *config = response.mutable_connection_configuration()->mutable_ping_configuration();
    config->set_interval_ms(1000);
    config->set_timeout_ms(5000);
    config->set_tracked_ping_count(5);
    config->set_high_latency_threshold_ms(200);
    for (const auto &service : services_)
        service->features(response);
    auto self = shared_from_this();
    control_->sendServiceDiscoveryResponse(response, promise([self] {
        self->handshake_timer_.cancel();
        std::cerr << "AA service discovery response sent\n";
        if (!self->ping_started_) {
            self->ping_started_ = true;
            self->ping();
        }
    }));
    receive();
}
void Session::ping() {
    if (stopped)
        return;
    auto stamp = now_us();
    pending_ping_ = stamp;
    control::PingRequest request;
    request.set_timestamp(stamp);
    auto self = shared_from_this();
    // The watchdog measures time since the USB write completed, not time
    // queued behind authentication or service discovery.
    control_->sendPingRequest(request, promise([self, stamp] {
        if (self->pending_ping_ != stamp)
            return;
        self->ping_timer_.expires_after(std::chrono::seconds(5));
        self->ping_timer_.async_wait([self, stamp](auto ec) {
            if (!ec && self->pending_ping_ == stamp)
                self->fail("AA ping timed out after USB write completed");
        });
    }));
}
void Session::audio_start(int kind) {
    if (kind == 0) {
        if (main_audio_)
            debug->stop_audio(*main_audio_);
        main_audio_ = debug->start_audio(false);
        media_epoch_.reset();
        media_next_ = 0;
        media_pacer_.reset();
        media_suspended_ = false;
    } else {
        prompt_active_.at(kind - 1) = true;
        app_state();
        if (!alternate_audio_) {
            mixer_.reset();
            alternate_audio_ = debug->start_audio(true);
        }
    }
}
void Session::audio_stop(int kind) {
    if (kind == 0) {
        // The phone may stop its sink after our focus-loss notification.
        // Preserve a suspended CarPlay request so ownership returning can
        // notify AA of focus gain, rather than waiting for a new user action.
        if (main_audio_ && media_suspended_) {
            debug->clear_audio(*main_audio_);
            return;
        }
        if (main_audio_)
            debug->stop_audio(*main_audio_);
        main_audio_.reset();
        media_epoch_.reset();
        media_next_ = 0;
    } else {
        prompt_active_.at(kind - 1) = false;
        app_state();
        if (!prompt_active_[0] && !prompt_active_[1] && mixer_.empty() && alternate_audio_) {
            debug->stop_audio(*alternate_audio_);
            alternate_audio_.reset();
        }
    }
}
void Session::audio(int kind, std::optional<uint64_t> stamp, const Bytes &data, Done ack) {
    require(data.size() % (kind ? 2 : 4) == 0, "partial Android Auto PCM frame");
    if (kind == 0) {
        if (!main_audio_)
            audio_start(0);
        auto position = media_next_;
        if (stamp) {
            if (!media_epoch_)
                media_epoch_ = *stamp;
            if (*stamp >= *media_epoch_)
                position = aligned_position(media_next_, (*stamp - *media_epoch_) * 48000 / 1000000,
                                            48000 * 40 / 1000);
        }
        media_next_ = position + data.size() / 4;
        debug->pcm(*main_audio_, {position, data});
        auto now = now_us();
        auto due = media_pacer_.push(data.size() / 4, now);
        if (due <= now)
            return ack();
        auto timer = std::make_shared<boost::asio::steady_timer>(
            io, std::chrono::microseconds(due - now));
        auto weak = weak_from_this();
        timer->async_wait([timer, weak, ack = std::move(ack)](auto ec) {
            auto s = weak.lock();
            if (ec || !s || s->stopped)
                return;
            try {
                ack();
            } catch (const std::exception &e) {
                s->fail(e.what());
            }
        });
    } else {
        if (!alternate_audio_)
            audio_start(kind);
        mixer_.push(unsigned(kind - 1), stamp, data);
        if (!mixer_running_) {
            mixer_running_ = true;
            // Start 40 ms behind the first samples: later AA packets arriving
            // within that jitter are still played instead of cut by the cursor.
            prompt_deadline_ = std::chrono::steady_clock::now() + std::chrono::milliseconds(35);
            mix();
        }
        // The mixer already retains prompts delivered faster than real time.
        ack();
    }
}
void Session::mix() {
    if (stopped || !alternate_audio_) {
        mixer_running_ = false;
        return;
    }
    auto self = shared_from_this();
    prompt_deadline_ += std::chrono::milliseconds(5);
    auto now = std::chrono::steady_clock::now();
    if (prompt_deadline_ + std::chrono::milliseconds(60) < now)
        prompt_deadline_ = now;
    prompt_timer_.expires_at(prompt_deadline_);
    prompt_timer_.async_wait([self](auto ec) {
        if (ec || self->stopped)
            return;
        if (!self->alternate_audio_) {
            self->mixer_running_ = false;
            return;
        }
        self->debug->pcm(*self->alternate_audio_, self->mixer_.take());
        if (self->mixer_.empty() && !self->prompt_active_[0] && !self->prompt_active_[1]) {
            self->debug->stop_audio(*self->alternate_audio_);
            self->alternate_audio_.reset();
            self->mixer_running_ = false;
        } else
            self->mix();
    });
}
void Session::stop() {
    if (stopped)
        return;
    stopped = true;
    ping_timer_.cancel();
    prompt_timer_.cancel();
    handshake_timer_.cancel();
    if (main_audio_)
        debug->stop_audio(*main_audio_);
    if (alternate_audio_)
        debug->stop_audio(*alternate_audio_);
    main_audio_.reset();
    alternate_audio_.reset();
    prompt_active_ = {};
    app_state();
    debug->input = {};
    debug->audio_status = {};
    debug->control({{"type", "now_playing_update"}, {"now_playing", nullptr}});
    if (messenger_)
        messenger_->stop();
    if (transport_)
        transport_->stop();
    if (cryptor_)
        cryptor_->deinit();
    services_.clear();
    if (closed_) {
        auto done = std::move(closed_);
        boost::asio::post(io, std::move(done));
    }
}
std::shared_ptr<Session> start_session(boost::asio::io_service &io, aasdk::usb::IUSBWrapper &usb,
                                       aasdk::usb::DeviceHandle handle, const Geometry &geometry,
                                       const Json &inputs, std::shared_ptr<DebugClient> debug,
                                       Done closed) {
    auto session = std::make_shared<Session>(io, geometry, inputs, std::move(debug), Done{});
    try {
        session->start(usb, std::move(handle));
    } catch (...) {
        session->stop();
        throw;
    }
    session->set_closed(std::move(closed));
    return session;
}
std::shared_ptr<Session> start_session(boost::asio::io_service &io,
                                       aasdk::messenger::IMessenger::Pointer messenger,
                                       aasdk::messenger::ICryptor::Pointer cryptor,
                                       const Geometry &geometry, const Json &inputs,
                                       std::shared_ptr<DebugClient> debug, Done closed) {
    auto session = std::make_shared<Session>(io, geometry, inputs, std::move(debug), Done{});
    try {
        session->start(std::move(messenger), std::move(cryptor));
    } catch (...) {
        session->stop();
        throw;
    }
    session->set_closed(std::move(closed));
    return session;
}
void stop_session(const std::shared_ptr<Session> &session) {
    if (session)
        session->stop();
}
} // namespace bridge
