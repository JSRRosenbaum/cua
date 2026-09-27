#pragma once

#include <src/managers/SeatManager.hpp>
#include <src/protocols/types/DataDevice.hpp>

#include <chrono>
#include <cstdint>
#include <string>

class CWLSurfaceResource;

namespace cua::hyprland {

// A selection belongs to one claimed agent seat and one transport connection.
// It is never the primary seat's shared clipboard. Application acceptance is
// separate from compositor ownership and is not inferred from set().
class SeatSelection {
  public:
    enum class Status { delivered, no_focus, no_data_device, invalid_payload, invalid_seat };
    using Clock = std::chrono::steady_clock;

    explicit SeatSelection(SP<CSeatManager::SCompositorSeat> agentSeat);
    ~SeatSelection();
    SeatSelection(const SeatSelection&) = delete;
    SeatSelection& operator=(const SeatSelection&) = delete;

    Status set(SP<CSeatManager::SCompositorSeat> seat, std::uint64_t ownerConnectionId,
               SP<CWLSurfaceResource> destination, std::string text, Clock::time_point expiresAt);
    bool clearIfOwner(SP<CSeatManager::SCompositorSeat> seat, std::uint64_t ownerConnectionId);
    // Called before the compositor offers selection to a newly focused
    // agent-seat surface. Neutral (null) focus preserves the owned source.
    void clearBeforeFocus(SP<CSeatManager::SCompositorSeat> seat,
                          SP<CWLSurfaceResource> nextSurface);
    bool clearExpired(Clock::time_point now);
    void clearAll();

  private:
    SP<CSeatManager::SCompositorSeat> bound_seat_;
    SP<CSeatManager::SCompositorSeat> seat_;
    SP<IDataSource> source_;
    WP<CWLSurfaceResource> destination_;
    std::uint64_t owner_ = 0;
    Clock::time_point expiry_{};
};

} // namespace cua::hyprland
