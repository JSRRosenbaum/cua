#include "seat_selection.hpp"
#include "seat_selection_payload.hpp"

#include <src/Compositor.hpp>
#include <src/protocols/core/Compositor.hpp>
#include <src/protocols/core/DataDevice.hpp>

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <fcntl.h>
#include <memory>
#include <poll.h>
#include <optional>
#include <vector>
#include <string_view>
#include <thread>
#include <unistd.h>

namespace cua::hyprland {
namespace {
constexpr unsigned kMaxRequests = 64;
constexpr unsigned kMaxBlockedSends = 4;
constexpr auto kMaxOwnership = std::chrono::seconds(60);
constexpr auto kSendDeadline = std::chrono::seconds(2);
constexpr std::string_view kUtf8Mime = "text/plain;charset=utf-8";
constexpr std::string_view kPlainMime = "text/plain";

struct Payload {
    explicit Payload(std::string text, SeatSelection::Clock::time_point expiry)
        : bytes(std::move(text)), expires(expiry) {}
    const std::string bytes;
    const SeatSelection::Clock::time_point expires;
    std::atomic<bool> cancelled = false;
    std::atomic<unsigned> requests = 0;
    std::atomic<unsigned> blockedSends = 0;
};

// fd is owned by the Wayland receive request. Never wait for a stalled client
// on the compositor thread; only a short, capped background continuation may
// poll after an immediate nonblocking write would block.
std::optional<std::size_t> serve_fd(Hyprutils::OS::CFileDescriptor& fd,
                                     const std::shared_ptr<Payload>& payload,
                                     std::size_t offset, bool waitForWritable) {
    const auto deadline = std::min(payload->expires, SeatSelection::Clock::now() + kSendDeadline);
    while (offset < payload->bytes.size() && !payload->cancelled.load(std::memory_order_acquire)
           && SeatSelection::Clock::now() < deadline) {
        const auto count = ::write(fd.get(), payload->bytes.data() + offset, payload->bytes.size() - offset);
        if (count > 0) { offset += static_cast<std::size_t>(count); continue; }
        if (count < 0 && errno == EINTR) continue;
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) && waitForWritable) {
            pollfd event{.fd = fd.get(), .events = POLLOUT, .revents = 0};
            const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - SeatSelection::Clock::now());
            const auto timeout = static_cast<int>(std::clamp<std::chrono::milliseconds::rep>(remaining.count(), 0, 25));
            if (::poll(&event, 1, timeout) < 0 && errno != EINTR) break;
            if (event.revents & (POLLERR | POLLHUP | POLLNVAL)) break;
            continue;
        }
        if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK) && !waitForWritable)
            return offset;
        return std::nullopt;
    }
    return std::nullopt;
}

class AgentDataSource final : public IDataSource {
  public:
    AgentDataSource(std::string text, SeatSelection::Clock::time_point expires)
        : payload_(std::make_shared<Payload>(std::move(text), expires)) {}
    ~AgentDataSource() override {
        payload_->cancelled.store(true, std::memory_order_release);
        for (auto& worker : workers_)
            if (worker.joinable()) worker.join();
        m_events.destroy.emit();
    }

    std::vector<std::string> mimes() override {
        return {std::string(kUtf8Mime), std::string(kPlainMime)};
    }
    void send(const std::string& mime, Hyprutils::OS::CFileDescriptor fd) override {
        if ((mime != kUtf8Mime && mime != kPlainMime)
            || payload_->cancelled.load(std::memory_order_acquire)
            || SeatSelection::Clock::now() >= payload_->expires
            || payload_->requests.fetch_add(1, std::memory_order_relaxed) >= kMaxRequests)
            return;
        if (const auto flags = ::fcntl(fd.get(), F_GETFL); flags >= 0
            && ::fcntl(fd.get(), F_SETFL, flags | O_NONBLOCK) >= 0) {
            const auto remaining = serve_fd(fd, payload_, 0, false);
            if (!remaining) return;
            auto pending = payload_->blockedSends.load(std::memory_order_relaxed);
            while (pending < kMaxBlockedSends && !payload_->blockedSends.compare_exchange_weak(pending, pending + 1)) {}
            if (pending >= kMaxBlockedSends) return;
            try {
                if (workers_.capacity() < kMaxRequests) workers_.reserve(kMaxRequests);
                workers_.emplace_back([fd = std::move(fd), payload = payload_, offset = *remaining]() mutable {
                    serve_fd(fd, payload, offset, true);
                    payload->blockedSends.fetch_sub(1, std::memory_order_relaxed);
                });
            } catch (...) {
                payload_->blockedSends.fetch_sub(1, std::memory_order_relaxed);
            }
        }
    }
    void accepted(const std::string&) override {}
    void cancelled() override { payload_->cancelled.store(true, std::memory_order_release); }
    void error(std::uint32_t, const std::string&) override {
        payload_->cancelled.store(true, std::memory_order_release);
    }

  private:
    std::shared_ptr<Payload> payload_;
    std::vector<std::thread> workers_;
};
} // namespace

SeatSelection::SeatSelection(SP<CSeatManager::SCompositorSeat> agentSeat)
    : bound_seat_(std::move(agentSeat)) {}

SeatSelection::~SeatSelection() { clearAll(); }

SeatSelection::Status SeatSelection::set(SP<CSeatManager::SCompositorSeat> seat,
                                         std::uint64_t ownerConnectionId,
                                         SP<CWLSurfaceResource> destination, std::string text,
                                         Clock::time_point expiresAt) {
    if (!seat || seat != bound_seat_ || !g_pSeatManager
        || seat == g_pSeatManager->primarySeat() || !destination || !destination->good()
        || !PROTO::data || !ownerConnectionId)
        return Status::invalid_seat;
    const auto now = Clock::now();
    if (expiresAt <= now || expiresAt > now + kMaxOwnership || !valid_selection_text(text))
        return Status::invalid_payload;
    if (g_pSeatManager->keyboardFocusForSeat(seat) != destination)
        return Status::no_focus;
    if (!PROTO::data->hasDataDeviceForSeat(destination->client(), seat))
        return Status::no_data_device;
    auto source = makeShared<AgentDataSource>(std::move(text), expiresAt);
    // The compositor stores a weak source; this instance keeps its source
    // alive until exactly its transport owner expires, cancels or disconnects.
    g_pSeatManager->setSelectionForSeat(seat, source);
    if (source_ && seat_ && g_pSeatManager->selectionForSeat(seat_) == source_)
        g_pSeatManager->setSelectionForSeat(seat_, nullptr);
    source_ = source;
    seat_ = seat;
    destination_ = destination;
    owner_ = ownerConnectionId;
    expiry_ = expiresAt;
    return Status::delivered;
}

bool SeatSelection::clearIfOwner(SP<CSeatManager::SCompositorSeat> seat,
                                 std::uint64_t ownerConnectionId) {
    if (!source_ || !seat_ || seat_ != seat || !ownerConnectionId || owner_ != ownerConnectionId)
        return false;
    clearAll();
    return true;
}

void SeatSelection::clearBeforeFocus(SP<CSeatManager::SCompositorSeat> seat,
                                     SP<CWLSurfaceResource> nextSurface) {
    if (source_ && seat_ && seat == bound_seat_ && nextSurface
        && nextSurface != destination_.lock())
        clearAll();
}

bool SeatSelection::clearExpired(Clock::time_point now) {
    if (!source_) return false;
    const auto target = destination_.lock();
    // The action releases agent keyboard focus immediately after setting the
    // selection. A null focus does not end socket ownership; only expiry,
    // recipient/device loss, or replacement of our exact source does.
    if (now < expiry_ && target && target->good() && seat_ == bound_seat_
        && g_pSeatManager && PROTO::data
        && PROTO::data->hasDataDeviceForSeat(target->client(), seat_)
        && g_pSeatManager->selectionForSeat(seat_) == source_)
        return false;
    clearAll();
    return true;
}

void SeatSelection::clearAll() {
    if (source_ && seat_ && g_pSeatManager && g_pSeatManager->selectionForSeat(seat_) == source_)
        g_pSeatManager->setSelectionForSeat(seat_, nullptr);
    source_.reset();
    seat_.reset();
    destination_.reset();
    owner_ = 0;
    expiry_ = {};
}

} // namespace cua::hyprland
