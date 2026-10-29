#include "session.hpp"
#include <aasdk/USB/AccessoryModeQueryChainFactory.hpp>
#include <aasdk/USB/AccessoryModeQueryFactory.hpp>
#include <aasdk/USB/ConnectedAccessoriesEnumerator.hpp>
#include <aasdk/USB/USBHub.hpp>
#include <aasdk/USB/USBWrapper.hpp>
#include <atomic>
#include <boost/asio/signal_set.hpp>
#include <csignal>
#include <iostream>
#include <thread>

namespace bridge {
class UsbHost : public std::enable_shared_from_this<UsbHost> {
    boost::asio::io_service &io_;
    boost::asio::io_service::strand strand_;
    aasdk::usb::USBWrapper &usb_;
    aasdk::usb::AccessoryModeQueryFactory query_;
    aasdk::usb::AccessoryModeQueryChainFactory chain_;
    std::shared_ptr<aasdk::usb::USBHub> hub_;
    std::shared_ptr<aasdk::usb::ConnectedAccessoriesEnumerator> enumerator_;
    std::shared_ptr<Session> session_;
    boost::asio::steady_timer retry_;
    Geometry geometry_;
    Json inputs_;
    std::shared_ptr<DebugClient> debug_;
    bool stopped_ = false;

  public:
    UsbHost(boost::asio::io_service &io, aasdk::usb::USBWrapper &usb, Geometry g, Json inputs,
            std::shared_ptr<DebugClient> debug)
        : io_(io), strand_(io), usb_(usb), query_(usb, io), chain_(usb, io, query_),
          hub_(std::make_shared<aasdk::usb::USBHub>(usb, io, chain_)),
          enumerator_(
              std::make_shared<aasdk::usb::ConnectedAccessoriesEnumerator>(usb, io, chain_)),
          retry_(io), geometry_(g), inputs_(std::move(inputs)), debug_(std::move(debug)) {}
    void start() {
        if (stopped_)
            return;
        std::cerr << "waiting for Android Auto USB device\n";
        auto self = shared_from_this();
        auto promise = aasdk::usb::IUSBHub::Promise::defer(strand_);
        promise->then(
            [self](auto handle) {
                if (self->stopped_ || self->session_)
                    return;
                self->enumerator_->cancel();
                try {
                    self->session_ =
                        start_session(self->io_, self->usb_, std::move(handle), self->geometry_,
                                      self->inputs_, self->debug_, [self] {
                                          self->session_.reset();
                                          self->retry();
                                      });
                } catch (const std::exception &e) {
                    std::cerr << "USB session: " << e.what() << '\n';
                    self->retry();
                }
            },
            [self](auto e) {
                if (!self->stopped_) {
                    std::cerr << "USB discovery: " << e.what() << '\n';
                    self->retry();
                }
            });
        hub_->start(std::move(promise));
        auto scan = aasdk::usb::IConnectedAccessoriesEnumerator::Promise::defer(strand_);
        scan->then([](auto) {},
                   [](auto e) { std::cerr << "USB enumeration: " << e.what() << '\n'; });
        enumerator_->enumerate(std::move(scan));
    }
    void retry() {
        if (stopped_)
            return;
        hub_->cancel();
        enumerator_->cancel();
        retry_.expires_after(std::chrono::milliseconds(500));
        auto self = shared_from_this();
        retry_.async_wait([self](auto ec) {
            if (!ec)
                self->start();
        });
    }
    void stop() {
        if (stopped_)
            return;
        stopped_ = true;
        retry_.cancel();
        hub_->cancel();
        enumerator_->cancel();
        stop_session(session_);
        session_.reset();
    }
};
} // namespace bridge
int main(int argc, char **argv) {
    using namespace bridge;
    try {
        require(argc >= 2, "usage: aa-debug-bridge HOST [--port 6767]");
        std::string host = argv[1];
        unsigned port = 6767;
        if (host == "--help" || host == "-h") {
            std::cout << "usage: aa-debug-bridge HOST [--port 6767]\n";
            return 0;
        }
        if (argc != 2) {
            require(argc == 4 && std::string(argv[2]) == "--port",
                    "usage: aa-debug-bridge HOST [--port 6767]");
            std::string value = argv[3];
            size_t end = 0;
            auto number = std::stoul(value, &end);
            require(end == value.size() && number > 0 && number <= 65535, "invalid port");
            port = unsigned(number);
        }
        libusb_context *context = nullptr;
        require(libusb_init(&context) == 0, "libusb initialization failed");
        auto context_guard =
            std::unique_ptr<libusb_context, decltype(&libusb_exit)>(context, libusb_exit);
        // USB callbacks/handles may survive until io's pending handlers are
        // destroyed. Keep the wrapper and context alive through that teardown.
        aasdk::usb::USBWrapper usb(context);
        boost::asio::io_service io;
        std::atomic<bool> usb_stop{false};
        std::thread usb_thread([&] {
            while (!usb_stop) {
                timeval timeout{0, 20000};
                libusb_handle_events_timeout_completed(context, &timeout, nullptr);
            }
        });
        // Always join before releasing the libusb context, including exceptions.
        struct Join {
            std::atomic<bool> &stop;
            std::thread &thread;
            ~Join() {
                stop = true;
                thread.join();
            }
        } join{usb_stop, usb_thread};
        auto debug = std::make_shared<DebugClient>(io, host, port);
        std::shared_ptr<UsbHost> receiver;
        boost::asio::signal_set signals(io, SIGINT, SIGTERM);
        boost::asio::steady_timer shutdown(io);
        int result = 0;
        bool stopping = false;
        auto stop = [&] {
            if (stopping)
                return;
            stopping = true;
            if (receiver)
                receiver->stop();
            debug->close();
            signals.cancel();
            shutdown.expires_after(std::chrono::milliseconds(100));
            shutdown.async_wait([&](auto) { io.stop(); });
        };
        debug->initialized = [&](auto g, const auto &inputs) {
            std::cerr << "display " << g.width << 'x' << g.height << ", AA canvas "
                      << g.canvas_width << 'x' << g.canvas_height << ", crop " << g.crop().dump()
                      << '\n';
            receiver = std::make_shared<UsbHost>(io, usb, g, inputs, debug);
        };
        debug->display_ready = [&] { receiver->start(); };
        debug->error = [&](auto e) {
            std::cerr << "debug network: " << e << '\n';
            result = 1;
            stop();
        };
        signals.async_wait([&](auto ec, int) {
            if (!ec)
                stop();
        });
        debug->start();
        io.run();
        if (receiver)
            receiver->stop();
        debug->close();
        return result;
    } catch (const std::exception &e) {
        std::cerr << "aa-debug-bridge: " << e.what() << '\n';
        return 1;
    }
}
