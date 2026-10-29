#pragma once
#include <aasdk/Channel/Control/ControlServiceChannel.hpp>
#include <aasdk/Channel/Control/IControlServiceChannelEventHandler.hpp>

namespace bridge {
// The current aasdk version decoder reads unaligned host-endian uint16_t values.
// Decode the initial response here; all subsequent traffic uses aasdk unchanged.
class ControlChannel : public aasdk::channel::control::ControlServiceChannel {
    bool version_pending_ = true;

  public:
    using ControlServiceChannel::ControlServiceChannel;
    void receive(aasdk::channel::control::IControlServiceChannelEventHandler::Pointer handler)
        override {
        if (!version_pending_)
            return ControlServiceChannel::receive(std::move(handler));
        version_pending_ = false;
        auto promise = aasdk::messenger::ReceivePromise::defer(strand_);
        promise->then(
            [handler](auto message) {
                const auto &data = message->getPayload();
                auto word = [&](size_t i) { return uint16_t((uint16_t(data[i]) << 8) | data[i + 1]); };
                if (data.size() < 8 ||
                    word(0) != aap_protobuf::service::control::message::MESSAGE_VERSION_RESPONSE) {
                    handler->onChannelError(
                        aasdk::error::Error(aasdk::error::ErrorCode::PARSE_PAYLOAD));
                    return;
                }
                handler->onVersionResponse(word(2), word(4),
                                          static_cast<aap_protobuf::shared::MessageStatus>(word(6)));
            },
            [handler](auto error) { handler->onChannelError(error); });
        messenger_->enqueueReceive(channelId_, std::move(promise));
    }
};
} // namespace bridge
