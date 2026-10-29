#pragma once
#include <boost/asio.hpp>
#include <boost/asio/io_context_strand.hpp>
#include <boost/version.hpp>
// aasdk still uses the io_service facade removed by recent Boost releases.
// Supply the small legacy surface without changing the sibling checkout.
#if !__has_include(<boost/asio/io_service.hpp>)
namespace boost::asio {
class io_service : public io_context {
  public:
    using io_context::io_context;
    class strand : public io_context::strand {
        io_service &service_;

      public:
        explicit strand(io_service &service) : io_context::strand(service), service_(service) {}
        using io_context::strand::dispatch;
        using io_context::strand::post;
        template <class F> void post(F &&f) {
            io_context::strand::post(std::forward<F>(f), std::allocator<void>{});
        }
        template <class F> void dispatch(F &&f) {
            io_context::strand::dispatch(std::forward<F>(f), std::allocator<void>{});
        }
        io_service &context() const { return service_; }
    };
    template <class F> void post(F &&f) { boost::asio::post(*this, std::forward<F>(f)); }
    template <class F> void dispatch(F &&f) { boost::asio::dispatch(*this, std::forward<F>(f)); }
    void reset() { restart(); }
};
} // namespace boost::asio
#endif
