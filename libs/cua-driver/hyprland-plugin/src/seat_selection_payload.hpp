#pragma once

#include <cstddef>
#include <cstdint>
#include <string_view>

namespace cua::hyprland {

// A bounded Wayland text/plain selection. Refuse malformed UTF-8 before the
// compositor advertises an offer that a client could consume inconsistently.
constexpr bool valid_selection_text(std::string_view text) {
    if (text.empty() || text.size() > 900) return false;
    for (std::size_t i = 0; i < text.size();) {
        const auto lead = static_cast<unsigned char>(text[i++]);
        if (lead == 0) return false;
        if (lead < 0x80) continue;
        std::size_t following = 0;
        std::uint32_t scalar = 0, minimum = 0;
        if (lead >= 0xC2 && lead <= 0xDF) {
            following = 1; scalar = lead & 0x1F; minimum = 0x80;
        } else if (lead >= 0xE0 && lead <= 0xEF) {
            following = 2; scalar = lead & 0x0F; minimum = 0x800;
        } else if (lead >= 0xF0 && lead <= 0xF4) {
            following = 3; scalar = lead & 0x07; minimum = 0x10000;
        } else return false;
        if (text.size() - i < following) return false;
        for (std::size_t j = 0; j < following; ++j) {
            const auto next = static_cast<unsigned char>(text[i++]);
            if ((next & 0xC0) != 0x80) return false;
            scalar = (scalar << 6) | (next & 0x3F);
        }
        if (scalar < minimum || (scalar >= 0xD800 && scalar <= 0xDFFF) || scalar > 0x10FFFF)
            return false;
    }
    return true;
}

} // namespace cua::hyprland
