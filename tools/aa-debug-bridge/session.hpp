#pragma once
#include "debug_client.hpp"
#include <aasdk/USB/IUSBWrapper.hpp>
#include <aasdk/Messenger/IMessenger.hpp>

namespace bridge {
class Session;
// Protocol entry point for an owned messenger/cryptor pair (also permits
// message-level tests without a USB device).
std::shared_ptr<Session> start_session(boost::asio::io_service &io,
                                     aasdk::messenger::IMessenger::Pointer messenger,
                                     aasdk::messenger::ICryptor::Pointer cryptor,
                                     const Geometry &geometry, const Json &inputs,
                                     std::shared_ptr<DebugClient> debug, Done closed);
std::shared_ptr<Session> start_session(boost::asio::io_service &io, aasdk::usb::IUSBWrapper &usb,
                                       aasdk::usb::DeviceHandle handle, const Geometry &geometry,
                                       const Json &inputs, std::shared_ptr<DebugClient> debug,
                                       Done closed);
void stop_session(const std::shared_ptr<Session> &session);
} // namespace bridge
