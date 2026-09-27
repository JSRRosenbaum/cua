// Independent-seat design adapted from Dillon DuPont's Hyprland prototype.
// TARGET uses independent resources; FOREGROUND_TARGET explicitly uses the primary seat.
#include "input_experiment.hpp"
#include "drag_geometry.hpp"
#include "input_grant.hpp"
#include "input_client_deadline.hpp"
#include "passive_pointer_target.hpp"
#include "primary_trace.hpp"
#include "seat_lifetime.hpp"
#include "owned_socket_path.hpp"
#include "seat_selection.hpp"
#include "foreground_route.hpp"
#include <src/managers/input/InputMethodRelay.hpp>

#include <src/Compositor.hpp>
#include <src/devices/IKeyboard.hpp>
#include <src/event/EventBus.hpp>
#include <src/managers/SeatManager.hpp>
#include <src/output/Monitor.hpp>
#include <src/protocols/core/Compositor.hpp>
#include <src/state/MonitorState.hpp>
#include <src/desktop/ViewTreeRevision.hpp>
#include <src/desktop/state/WindowState.hpp>
#include <src/desktop/state/ViewStateTracker.hpp>
#include <src/desktop/state/ViewHitTester.hpp>
#include <src/desktop/view/Popup.hpp>
#include <src/desktop/state/FocusState.hpp>
#include <src/managers/input/InputManager.hpp>
#include <src/pointer/PointerManager.hpp>
#include <src/protocols/core/Seat.hpp>
#include <src/protocols/core/DataDevice.hpp>
#include <src/protocols/XDGShell.hpp>
#include <src/protocols/InputCapture.hpp>
#include <src/layout/LayoutManager.hpp>
#include <src/xwayland/XSurface.hpp>
#include <linux/input-event-codes.h>
#include <wayland.hpp>
#include <wlr-layer-shell-unstable-v1.hpp>
#include <wayland-server-core.h>

#ifdef CUA_HYPRLAND_TEST_INPUT
#include <openssl/evp.h>
#endif
#include <sys/random.h>
#include <sys/mman.h>
#include <xkbcommon/xkbcommon.h>

#include <algorithm>
#include <array>
#include <charconv>
#include <cerrno>
#include <chrono>
#include <cmath>
#include <cstring>
#include <cstdlib>
#include <filesystem>
#include <fcntl.h>
#include <format>
#include <memory>
#include <optional>
#include <stdexcept>
#include <string_view>
#include <limits>
#include <tuple>
#include <unordered_map>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#if defined(CUA_HYPRLAND_INPUT) == defined(CUA_HYPRLAND_TEST_INPUT)
#error Select exactly one input admission mode
#endif

namespace cua::hyprland {
namespace {
using Clock = std::chrono::steady_clock;
constexpr std::size_t kMaxPacket = 2048;
constexpr std::size_t kMaxClients = 8;
constexpr std::size_t kMaxResources = 512;
#ifdef CUA_HYPRLAND_INPUT
constexpr bool kProduction = true;
#else
constexpr bool kProduction = false;
#endif

#ifdef CUA_HYPRLAND_TEST_INPUT
std::uint64_t unix_ms() {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
               std::chrono::system_clock::now().time_since_epoch()).count();
}
#endif
std::uint32_t event_ms() {
    return static_cast<std::uint32_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
               Clock::now().time_since_epoch()).count());
}
std::string nonce() {
    std::array<unsigned char, 16> bytes{};
    std::size_t offset = 0;
    while (offset < bytes.size()) {
        const auto n = getrandom(bytes.data() + offset, bytes.size() - offset, 0);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) throw std::runtime_error("input entropy unavailable");
        offset += static_cast<std::size_t>(n);
    }
    std::string result;
    for (auto b : bytes)
        result += std::format("{:02x}", b);
    return result;
}
#ifdef CUA_HYPRLAND_TEST_INPUT
std::vector<unsigned char> unhex(std::string_view value) {
    if (value.size() % 2)
        throw std::runtime_error("invalid hex");
    std::vector<unsigned char> bytes;
    for (std::size_t i = 0; i < value.size(); i += 2) {
        unsigned int n = 0;
        const auto [p, error] = std::from_chars(value.data() + i, value.data() + i + 2, n, 16);
        if (error != std::errc{} || p != value.data() + i + 2)
            throw std::runtime_error("invalid hex");
        bytes.push_back(static_cast<unsigned char>(n));
    }
    return bytes;
}
#endif
std::string text_from_hex(std::string_view value, bool allow_empty = false) {
    if (value == "-" && allow_empty) return {};
    if (value.empty() || value.size() > 1800 || value.size() % 2)
        throw std::runtime_error("invalid text payload");
    std::string result;
    result.reserve(value.size() / 2);
    for (std::size_t i = 0; i < value.size(); i += 2) {
        const auto digit = [](char c) -> unsigned {
            if (c >= '0' && c <= '9') return c - '0';
            if (c >= 'a' && c <= 'f') return c - 'a' + 10;
            throw std::runtime_error("invalid text payload");
        };
        result.push_back(static_cast<char>((digit(value[i]) << 4) | digit(value[i + 1])));
    }
    return result;
}
std::uint64_t number(std::string_view value, int base = 10) {
    std::uint64_t n = 0;
    const auto [p, error] = std::from_chars(value.data(), value.data() + value.size(), n, base);
    if (value.empty() || error != std::errc{} || p != value.data() + value.size())
        throw std::runtime_error("invalid integer");
    return n;
}
std::int32_t preedit_cursor(std::string_view value) {
    if (value == "-1") return -1;
    const auto index = number(value);
    if (index > INT32_MAX) throw std::runtime_error("invalid cursor");
    return static_cast<std::int32_t>(index);
}
double real(std::string_view value) {
    double n = 0;
    const auto [p, error] = std::from_chars(value.data(), value.data() + value.size(), n);
    if (value.empty() || error != std::errc{} || p != value.data() + value.size() || !std::isfinite(n))
        throw std::runtime_error("invalid coordinate");
    return n;
}
std::vector<std::string> fields(std::string_view packet) {
    if (packet.empty() || packet.front() == ' ' || packet.back() == ' ')
        throw std::runtime_error("invalid packet");
    std::vector<std::string> result;
    std::size_t start = 0;
    for (std::size_t i = 0; i <= packet.size(); ++i) {
        if (i < packet.size() && (packet[i] < 32 || packet[i] > 126))
            throw std::runtime_error("non-ASCII packet");
        if (i != packet.size() && packet[i] != ' ')
            continue;
        if (i == start || result.size() >= 48)
            throw std::runtime_error("invalid fields");
        result.emplace_back(packet.substr(start, i - start));
        start = i + 1;
    }
    return result;
}
std::string refusal(std::string_view code) {
    // Only internal fixed identifiers enter JSON. No caller-provided strings.
    return std::format(R"({{"ok":false,"code":"{}","detail":"{}"}})", code, code);
}
std::string refusal(std::string_view code, ForegroundFailure failure) {
    return std::format(R"({{"ok":false,"code":"{}","detail":"{}"}})", code, failure.detail());
}
std::string json_text(std::string_view text) {
    if (text.size() > 128) return "null";
    std::string result = "\"";
    for (const unsigned char c : text) {
        if (c == '"' || c == '\\') { result.push_back('\\'); result.push_back(static_cast<char>(c)); }
        else if (c < 32 || c > 126) result += std::format("\\u{:04x}", c);
        else result.push_back(static_cast<char>(c));
    }
    result.push_back('"');
    return result;
}
constexpr auto kDelivered = R"({"ok":true,"effect":"unverifiable","route":"synthetic_events"})";
constexpr auto kForegroundDelivered = R"({"ok":true,"effect":"unverifiable","route":"primary_foreground"})";
struct SurfaceIdentity {
    WP<CWLSurfaceResource> surface;
    std::string lifetime;
};
std::unordered_map<const CWLSurfaceResource*, SurfaceIdentity> surface_identities;
std::string scene_epoch = nonce();
const std::string& surface_lifetime(const SP<CWLSurfaceResource>& surface) {
    auto& entry = surface_identities[surface.get()];
    if (entry.surface.lock() != surface) {
        entry.surface = surface;
        entry.lifetime = nonce();
    }
    return entry.lifetime;
}
void prune_surface_identities() {
    if (surface_identities.size() < 1024) return;
    std::erase_if(surface_identities, [](const auto& entry) { return entry.second.surface.expired(); });
}
} // namespace

struct InputExperiment::Impl {
    struct Seat { SP<CWlSeat> wl; bool dead = false; };
    struct Pointer { SP<CWlPointer> wl; bool dead = false; WP<CWLSurfaceResource> focus; };
    struct Keyboard { SP<CWlKeyboard> wl; bool dead = false; WP<CWLSurfaceResource> focus; };
    struct Touch { SP<CWlTouch> wl; bool dead = false; };
    struct InventoryRow {
        std::uint64_t pid = 0, address = 0;
        std::uint32_t surface_id = 0;
        std::string lifetime;
        std::array<double, 6> root_geometry{};
        std::array<double, 4> box{};
        bool popup = false;
    };
    struct CaptureProof {
        Vector2D point;
        std::string epoch, root_lifetime, hit_lifetime;
        std::uint64_t tree_revision = 0;
        std::uint32_t root_id = 0, hit_id = 0, xid = 0;
        std::array<double, 6> root_geometry{};
        std::array<double, 4> hit_box{};
    };
    struct Client {
        Impl* owner = nullptr;
        int fd = -1;
        wl_event_source* source = nullptr;
        bool dead = false, hello = false;
        // Sticky for this connection: a bare CLAIM does not own inherited
        // hover. Never reset on STOP/CANCEL, repeated CLAIM, or invalidation.
        bool has_bound_target = false;
        InputRoute route = InputRoute::unbound;
        bool foreground_attempted = false;
        std::uint64_t connection_id = 0;
        std::uint32_t xwayland_xid = 0;
        WP<Desktop::View::CWindow> destination_window;
        WP<CWLSurfaceResource> destination_surface;
        std::array<double, 6> destination_geometry{};
        std::uint64_t destination_revision = 0;
        std::uint32_t destination_xid = 0;
        WP<CWLSeatResource> destination_primary_seat;
        std::optional<CaptureProof> source_capture, destination_capture;
        std::string challenge, token;
        WP<Desktop::View::CWindow> window;
        WP<CWLSurfaceResource> surface;
        std::array<double, 6> geometry{};
        bool frame_scope = false;
        bool window_capture_scope = false;
        WP<CWLSurfaceResource> text_focus;
        std::uint32_t captured_root_id = 0;
        std::string captured_root_lifetime;
        std::array<double, 6> captured_root_geometry{};
        Vector2D frame_point;
        WP<CWLSurfaceResource> frame_hit;
        std::string frame_epoch, frame_lifetime;
        std::uint64_t frame_tree_revision = 0;
        std::uint32_t frame_surface_id = 0;
        std::array<double, 4> frame_box{};
        bool frame_popup = false;
        std::string inventory_token;
        std::uint64_t inventory_tree_revision = 0;
        std::vector<InventoryRow> inventory;
        std::uint64_t revision = 1, sequence = 0, approved_deadline = 0;
        InputClientDeadline deadline;
        CHyprSignalListener unmap, destroy;
        ~Client() {
            if (source) wl_event_source_remove(source);
            if (fd >= 0) close(fd);
        }
    };
    struct Drag {
        Client* client;
        double x1, y1, x2, y2;
        std::vector<Vector2D> points;
        Clock::time_point start;
        unsigned duration;
        DragGeometry geometry;
        bool native_data = false;
        bool offered = false;
        bool dropped = false;
        WP<Desktop::View::CWindow> destination_window{};
        WP<CWLSurfaceResource> destination_surface{};
        std::array<double, 6> destination_geometry{};
        std::uint64_t destination_revision = 0;
        bool threshold_crossed = false;
        Clock::time_point destination_entered{};
    };
    int listener = -1;
    wl_event_source* listen_source = nullptr;
    wl_event_source* timer = nullptr;
    wl_global* global = nullptr;
    OwnedSocketPath socket_path;
    std::string_view socket_cleanup = "not_bound";
    std::string path, epoch = nonce(), keymap_text, physical_keymap_text;
    std::uint64_t next_connection_id = 1;
    std::vector<unsigned char> public_key;
    std::vector<std::unique_ptr<Client>> clients;
    std::vector<std::unique_ptr<Seat>> seats;
    std::vector<std::unique_ptr<Pointer>> pointers;
    std::vector<std::unique_ptr<Keyboard>> keyboards;
    std::vector<std::unique_ptr<Touch>> touches;
    SP<CSeatManager::SCompositorSeat> agent_seat;
    SeatSelection selection;
    WP<CWLSurfaceResource> preedit_surface;
    std::uint64_t preedit_connection_id = 0;
    Clock::time_point preedit_expires{};
    Client* lease = nullptr;
    // Pointer presence is not input authority. Keep the same live surface
    // entered between actions, as a real pointer is, without retaining a grant.
    PassivePointerTarget<WP<Desktop::View::CWindow>, WP<CWLSurfaceResource>> pointer_target;
    CHyprSignalListener pointer_unmap, pointer_destroy;
    Client* reservation = nullptr;
    std::uint64_t desktop_generation = 1;
    std::uint64_t capabilities = 0, dispatches = 0;
    Clock::time_point expires{};
    Clock::time_point agent_serial_expires{};
    InputGrant grant;
    std::string_view pointer_refusal = "client_not_bound";
    std::optional<Drag> drag;
    std::uint32_t held_button = 0;
    std::vector<std::uint32_t> held_keys;
    std::vector<WP<CWLSurfaceResource>> agent_serial_surfaces;
    xkb_context* xkb_context_ = nullptr;
    xkb_keymap* keymap = nullptr;
    xkb_state* keyboard_state = nullptr;
    xkb_context* physical_xkb_context = nullptr;
    xkb_keymap* physical_keymap = nullptr;
    xkb_state* physical_keyboard_state = nullptr;
    int keymap_fd = -1;
    bool retired = false, suspended = true, us_keymap = false, physical_keymap_present = false;
    WP<IKeyboard> physical_keyboard;
    CHyprSignalListener keymap_listener;
    unsigned lane;
    std::array<Impl*, 2> peers{};
    PrimaryTrace* trace = nullptr;
    bool foreground_started = false, foreground_activating = false;
    bool foreground_data_pressed = false;
    bool foreground_data_press_delivered = false;
    WP<CWLSurfaceResource> foreground_data_origin;
    bool foreground_keyboard_used = false;
    bool foreground_needs_keyboard = false;
    bool foreground_needs_pointer = false;
    WP<CWLSurfaceResource> foreground_surface;
    WP<CWLSurfaceResource> foreground_pointer_surface;
    WP<CWLSeatResource> foreground_seat;
    std::vector<WP<CWLPointerResource>> foreground_pointers;
    std::vector<WP<CWLKeyboardResource>> foreground_keyboards;
    std::array<std::uint32_t, 4> foreground_modifiers{};

    explicit Impl(const std::string& directory, unsigned index) :
        agent_seat(g_pSeatManager->createAgentSeat()), selection(agent_seat), lane(index) {
#ifdef CUA_HYPRLAND_TEST_INPUT
        public_key = unhex(CUA_HYPRLAND_TEST_OPERATOR_KEY);
        if (public_key.size() != 32)
            throw std::runtime_error("invalid test operator public key");
#endif
        path = directory + (kProduction ?
            (lane == 0 ? "/cua-input-v3.sock" : "/cua-input-v3-2.sock") :
            (lane == 0 ? "/cua-input-test.sock" : "/cua-input-test-2.sock"));
        if (path.size() >= sizeof(sockaddr_un::sun_path))
            throw std::runtime_error("input socket path too long");
        // No private key or input-enabled default exists in this component.
    }

    static bool canonical_us_keymap(xkb_context* context, xkb_keymap* map) {
        // Compare canonical compiled content, not a layout display name. This
        // deliberately excludes variants, options, remaps, and multiple groups.
        const xkb_rule_names names{kAgentKeymap.rules.data(), kAgentKeymap.model.data(),
            kAgentKeymap.layout.data(), kAgentKeymap.variant.data(), kAgentKeymap.options.data()};
        auto* reference = xkb_keymap_new_from_names(context, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
        if (!reference) return false;
        char* actual = xkb_keymap_get_as_string(map, XKB_KEYMAP_FORMAT_TEXT_V1);
        char* expected = xkb_keymap_get_as_string(reference, XKB_KEYMAP_FORMAT_TEXT_V1);
        const bool matches = actual && expected && std::strcmp(actual, expected) == 0;
        std::free(actual); std::free(expected); xkb_keymap_unref(reference);
        return matches;
    }
    static int create_keymap_file(std::string_view text) {
        const auto fd = memfd_create("cua-agent-keymap", MFD_CLOEXEC);
        std::string payload(text);
        payload.push_back('\0');
        if (fd < 0 || ftruncate(fd, static_cast<off_t>(payload.size())) != 0) {
            if (fd >= 0) close(fd);
            throw std::runtime_error("agent keymap file unavailable");
        }
        std::size_t offset = 0;
        while (offset < payload.size()) {
            const auto written = pwrite(fd, payload.data() + offset, payload.size() - offset, static_cast<off_t>(offset));
            if (written < 0 && errno == EINTR) continue;
            if (written <= 0) { close(fd); throw std::runtime_error("agent keymap write failed"); }
            offset += static_cast<std::size_t>(written);
        }
        return fd;
    }
    void initialize_agent_keymap() {
        if (keyboard_state) return;
        const xkb_rule_names names{kAgentKeymap.rules.data(), kAgentKeymap.model.data(),
            kAgentKeymap.layout.data(), kAgentKeymap.variant.data(), kAgentKeymap.options.data()};
        auto* context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
        auto* map = context ? xkb_keymap_new_from_names(context, &names, XKB_KEYMAP_COMPILE_NO_FLAGS) : nullptr;
        auto* state = map ? xkb_state_new(map) : nullptr;
        char* serialized = map ? xkb_keymap_get_as_string(map, XKB_KEYMAP_FORMAT_TEXT_V1) : nullptr;
        if (!state || !serialized) {
            std::free(serialized);
            if (state) xkb_state_unref(state);
            if (map) xkb_keymap_unref(map);
            if (context) xkb_context_unref(context);
            throw std::runtime_error("agent US keymap unavailable");
        }
        std::string text(serialized);
        std::free(serialized);
        int fd = -1;
        try {
            fd = create_keymap_file(text);
        } catch (...) {
            xkb_state_unref(state);
            xkb_keymap_unref(map);
            xkb_context_unref(context);
            throw;
        }
        xkb_context_ = context;
        keymap = map;
        keyboard_state = state;
        keymap_text = std::move(text);
        keymap_fd = fd;
    }
    bool layout_qualified() const {
        if (!kProduction) return true;
        const auto keyboard = g_pSeatManager->m_keyboard.lock();
        return physical_keymap_present && us_keymap && physical_keyboard_state && keyboard &&
            keyboard->m_xkbKeymapV1FD.get() >= 0 && keyboard->m_xkbKeymapV1String == physical_keymap_text;
    }
    void sync_keymap() {
        initialize_agent_keymap();
        const auto keyboard = g_pSeatManager->m_keyboard.lock();
        if (physical_keyboard != keyboard) {
            if (physical_keymap_present) desktop_transition();
            keymap_listener.reset();
            physical_keyboard = keyboard;
            if (keyboard && kProduction)
                keymap_listener = keyboard->m_keyboardEvents.keymap.listen([this](IKeyboard::SKeymapEvent) {
                    desktop_transition();
                });
        }
        if (!keyboard || keyboard->m_xkbKeymapV1FD.get() < 0 || keyboard->m_xkbKeymapV1String.empty()) {
            if (physical_keymap_present) desktop_transition();
            physical_keymap_present = false;
            return;
        }
        physical_keymap_present = true;
        if (physical_keyboard_state && physical_keymap_text == keyboard->m_xkbKeymapV1String) return;
        // Foreground keyboard delivery uses the primary seat. Keep a separate
        // compiled state for it; the background seats retain their fixed US map.
        auto* context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
        auto* map = context ? xkb_keymap_new_from_string(context, keyboard->m_xkbKeymapV1String.c_str(),
            XKB_KEYMAP_FORMAT_TEXT_V1, XKB_KEYMAP_COMPILE_NO_FLAGS) : nullptr;
        auto* state = map ? xkb_state_new(map) : nullptr;
        if (!state) {
            if (state) xkb_state_unref(state);
            if (map) xkb_keymap_unref(map);
            if (context) xkb_context_unref(context);
            throw std::runtime_error("primary XKB state unavailable");
        }
        desktop_transition();
        if (physical_keyboard_state) xkb_state_unref(physical_keyboard_state);
        if (physical_keymap) xkb_keymap_unref(physical_keymap);
        if (physical_xkb_context) xkb_context_unref(physical_xkb_context);
        physical_keyboard_state = state; physical_keymap = map; physical_xkb_context = context;
        us_keymap = !kProduction || canonical_us_keymap(context, map);
        physical_keymap_text = keyboard->m_xkbKeymapV1String;
    }
    void start() {
        sync_keymap();
        timer = wl_event_loop_add_timer(g_pCompositor->m_wlEventLoop, tick, this);
        if (!timer) throw std::runtime_error("input timer registration failed");
        global = wl_global_create(g_pCompositor->m_wlDisplay, &wl_seat_interface, 9, this, bind_seat);
        if (!global) throw std::runtime_error("synthetic seat unavailable");
        wl_event_source_timer_update(timer, 16);
    }
    void resume() {
        if (retired || desktop_generation == UINT64_MAX)
            throw std::runtime_error("restart desktop before replacing input seats");
        if (!suspended) return;
        sync_keymap();
        // Every new admission period has fresh identity and no inherited grant.
        epoch = nonce();
        listener = socket(AF_UNIX, SOCK_SEQPACKET | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
        if (listener < 0) throw std::runtime_error("input socket unavailable");
        sockaddr_un address{};
        address.sun_family = AF_UNIX;
        std::memcpy(address.sun_path, path.c_str(), path.size() + 1);
        // Never unlink a pre-existing socket owned by another plugin instance.
        if (bind(listener, reinterpret_cast<sockaddr*>(&address), sizeof(address)) != 0)
            throw std::runtime_error("input socket bind refused");
        if (!socket_path.capture(path))
            throw std::runtime_error("input socket ownership unavailable");
        socket_cleanup = "bound";
        if (chmod(path.c_str(), 0600) != 0 || listen(listener, 8) != 0)
            throw std::runtime_error("input socket setup failed");
        listen_source = wl_event_loop_add_fd(g_pCompositor->m_wlEventLoop, listener, WL_EVENT_READABLE, accept_ready, this);
        if (!listen_source) throw std::runtime_error("input event loop registration failed");
        suspended = false;
    }
    void cleanup_socket() {
        using Result = OwnedSocketPath::CleanupResult;
        switch (socket_path.cleanup()) {
            case Result::NotCaptured: break;
            case Result::Removed: socket_cleanup = "removed"; break;
            case Result::AlreadyAbsent: socket_cleanup = "absent"; break;
            case Result::Replaced: socket_cleanup = "replacement_preserved"; break;
            case Result::Failed: socket_cleanup = "failed"; break;
        }
    }
    void suspend(std::string_view reason = "plugin_disabled") {
        suspended = true;
        revoke(reason, reason != "plugin_shutdown");
        reservation = nullptr;
        if (listen_source) wl_event_source_remove(listen_source);
        listen_source = nullptr;
        selection.clearAll();
        clients.clear();
        if (listener >= 0) close(listener);
        listener = -1;
        cleanup_socket();
        // Keep the globals, capabilities, and client resources stable. Removing
        // and recreating them makes existing apps lose their agent input path.
    }
    void retire() {
        // wl_global removal does not revoke existing protocol objects. Destroying
        // those objects immediately disconnects clients that still send release
        // or cursor requests (observed with foot). Retain inert resources and
        // callbacks until compositor exit; input-enabled modules are NODELETE.
        suspend("plugin_shutdown");
        keymap_listener.reset();
        retired = true;
        for (auto& seat : seats)
            if (!seat->dead && seat->wl->resource())
                seat->wl->sendCapabilities(static_cast<wl_seat_capability>(0));
        if (global) wl_global_remove(global);
    }
    ~Impl() {
        revoke("plugin_shutdown");
        if (global) wl_global_destroy(global);
        if (listen_source) wl_event_source_remove(listen_source);
        if (timer) wl_event_source_remove(timer);
        clients.clear();
        // Destroy all plugin-owned resources before unloading code callbacks.
        pointers.clear(); keyboards.clear(); touches.clear(); seats.clear();
        if (listener >= 0) close(listener);
        cleanup_socket();
        if (keyboard_state) xkb_state_unref(keyboard_state);
        if (keymap) xkb_keymap_unref(keymap);
        if (xkb_context_) xkb_context_unref(xkb_context_);
        if (physical_keyboard_state) xkb_state_unref(physical_keyboard_state);
        if (physical_keymap) xkb_keymap_unref(physical_keymap);
        if (physical_xkb_context) xkb_context_unref(physical_xkb_context);
        if (keymap_fd >= 0) close(keymap_fd);
    }
    std::uint32_t serial() const { return wl_display_next_serial(g_pCompositor->m_wlDisplay); }
    bool available() const {
        return !retired && !suspended && g_pCompositor->m_sessionActive && g_pCompositor->m_dpmsStateOn &&
            !g_pCompositor->m_isShuttingDown && !g_pSessionLockManager->isSessionLocked();
    }
    static void bind_seat(wl_client* client, void* data, std::uint32_t version, std::uint32_t id) {
        auto& self = *static_cast<Impl*>(data);
        if (self.seats.size() >= kMaxResources) { wl_client_post_no_memory(client); return; }
        auto seat = std::make_unique<Seat>();
        auto* entry = seat.get();
        seat->wl = makeShared<CWlSeat>(client, std::min(version, 9u), id);
        if (!seat->wl->resource()) { wl_client_post_no_memory(client); return; }
        // Keep stock CWLSeatResource casts inert while registering the distinct
        // compositor-owned agent seat identity for text, popup and data-device
        // protocols. A plugin resource must never become the primary seat.
        seat->wl->setData(nullptr);
        seat->wl->setRelease([entry](CWlSeat*) { entry->dead = true; });
        seat->wl->setOnDestroy([entry](CWlSeat*) { entry->dead = true; });
        seat->wl->setGetPointer([&self](CWlSeat* r, std::uint32_t child) { self.add_pointer(r, child); });
        seat->wl->setGetKeyboard([&self](CWlSeat* r, std::uint32_t child) { self.add_keyboard(r, child); });
        seat->wl->setGetTouch([&self](CWlSeat* r, std::uint32_t child) { self.add_touch(r, child); });
        if (!self.agent_seat || !g_pSeatManager->registerExternalSeat(self.agent_seat, seat->wl->resource())) {
            seat->wl->sendCapabilities(static_cast<wl_seat_capability>(0));
            self.seats.push_back(std::move(seat));
            return;
        }
        if (version >= 2) seat->wl->sendName(kProduction ?
            (self.lane == 0 ? "Cua-Agent" : "Cua-Agent-2") :
            (self.lane == 0 ? "Cua-Test-Agent" : "Cua-Test-Agent-2"));
        seat->wl->sendCapabilities(static_cast<wl_seat_capability>(self.retired ? 0 :
            WL_SEAT_CAPABILITY_POINTER | (self.keyboard_state ? WL_SEAT_CAPABILITY_KEYBOARD : 0)));
        self.seats.push_back(std::move(seat));
    }
    void add_pointer(CWlSeat* seat, std::uint32_t id) {
        if (pointers.size() >= kMaxResources) { seat->noMemory(); return; }
        auto p = std::make_unique<Pointer>(); auto* entry = p.get();
        p->wl = makeShared<CWlPointer>(seat->client(), seat->version(), id);
        if (!p->wl->resource()) { seat->noMemory(); return; }
        p->wl->setData(nullptr);
        p->wl->setRelease([entry](CWlPointer*) { entry->dead = true; });
        p->wl->setOnDestroy([entry](CWlPointer*) { entry->dead = true; });
        p->wl->setSetCursor([](CWlPointer*, std::uint32_t, wl_resource*, std::int32_t, std::int32_t) {});
        pointers.push_back(std::move(p));
    }
    void add_keyboard(CWlSeat* seat, std::uint32_t id) {
        if (!keyboard_state || keyboards.size() >= kMaxResources) { seat->noMemory(); return; }
        auto k = std::make_unique<Keyboard>(); auto* entry = k.get();
        k->wl = makeShared<CWlKeyboard>(seat->client(), seat->version(), id);
        if (!k->wl->resource()) { seat->noMemory(); return; }
        k->wl->setData(nullptr);
        k->wl->setRelease([entry](CWlKeyboard*) { entry->dead = true; });
        k->wl->setOnDestroy([entry](CWlKeyboard*) { entry->dead = true; });
        k->wl->sendKeymap(WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, keymap_fd, keymap_text.size() + 1);
        if (seat->version() >= 4) k->wl->sendRepeatInfo(0, 0);
        keyboards.push_back(std::move(k));
    }
    void add_touch(CWlSeat* seat, std::uint32_t id) {
        // Never advertised. A valid inert resource is safer than a dangling id.
        if (touches.size() >= kMaxResources) { seat->noMemory(); return; }
        auto t = std::make_unique<Touch>(); auto* entry = t.get();
        t->wl = makeShared<CWlTouch>(seat->client(), seat->version(), id);
        if (!t->wl->resource()) { seat->noMemory(); return; }
        t->wl->setData(nullptr);
        t->wl->setRelease([entry](CWlTouch*) { entry->dead = true; });
        t->wl->setOnDestroy([entry](CWlTouch*) { entry->dead = true; });
        touches.push_back(std::move(t));
    }
    static std::optional<std::array<double, 6>> target_geometry(
        const PHLWINDOW& window, const SP<CWLSurfaceResource>& surface) {
        if (!window || !surface || !window->m_isMapped || window->isHidden() ||
            window->resource() != surface || !surface->m_mapped || !surface->good()) return std::nullopt;
        // Match Driver's captured client surface, not the decorated window box
        // (which includes compositor borders and shifts clicks by their width).
        const auto box = window->surfaceLogicalBox(); const auto surface_box = box;
        if (!box || !surface_box || box->w <= 0 || box->h <= 0 || box->w > 32767 || box->h > 32767) return std::nullopt;
        std::array<double, 6> geometry{box->x, box->y, box->w, box->h, surface_box->x, surface_box->y};
        if (!std::ranges::all_of(geometry, [](double value) { return std::isfinite(value); })) return std::nullopt;
        return geometry;
    }
    bool refresh(Client& c) {
        if (c.xwayland_xid) {
            const auto window = c.window.lock();
            const auto x = window ? window->m_xwaylandSurface.lock() : nullptr;
            if (!window || !window->m_isX11 || !x || !x->m_mapped ||
                x->m_xID != c.xwayland_xid || x->m_pid != window->getPID()) return false;
        }
        const auto geometry = target_geometry(c.window.lock(), c.surface.lock());
        if (!geometry) return false;
        if (*geometry != c.geometry) {
            if (c.revision == UINT64_MAX) return false;
            c.geometry = *geometry; ++c.revision;
        }
        return true;
    }
    bool destination_valid(const Client& c) const {
        const auto source = c.surface.lock();
        const auto dest = c.destination_window.lock();
        const auto surface = c.destination_surface.lock();
        const auto geometry = target_geometry(dest, surface);
        const bool foreground = c.route == InputRoute::primary_foreground;
        const auto seat = foreground ? g_pSeatManager->primarySeat() : agent_seat;
        const auto dst_x = dest && dest->m_isX11 ? dest->m_xwaylandSurface.lock() : nullptr;
        return source && dest && surface && (!dest->m_isX11 ||
                (dst_x && dst_x->m_mapped && dst_x->m_xID == c.destination_xid &&
                    dst_x->m_pid == dest->getPID())) &&
            c.destination_revision != 0 && geometry && *geometry == c.destination_geometry &&
            (foreground || !primary_conflict(surface)) && !agent_conflict(surface) &&
            (!foreground || (c.destination_primary_seat.lock() &&
                g_pSeatManager->seatResourceForClient(surface->client()) == c.destination_primary_seat.lock())) &&
            PROTO::data && PROTO::data->hasDataDeviceForSeat(source->client(), seat) &&
            PROTO::data->hasDataDeviceForSeat(surface->client(), seat);
    }
    bool primary_conflict(const Client& c) const {
        if (c.route == InputRoute::primary_foreground) {
            if (foreground_activating) return false;
            if (foreground_data_pressed) return !foreground_transfer_ready(c);
            return foreground_started && !foreground_activating && !foreground_guard(c).can_dispatch(foreground_needs_pointer);
        }
        return primary_conflict(c.surface.lock());
    }
    bool primary_conflict(const SP<CWLSurfaceResource>& surface) const {
        if (!surface) return true;
        const auto pointer = g_pSeatManager->m_state.pointerFocus.lock();
        const auto keyboard = g_pSeatManager->m_state.keyboardFocus.lock();
        // Conservative per-client refusal avoids toolkit-global cross-window state.
        return (pointer && pointer->client() == surface->client()) ||
            (keyboard && keyboard->client() == surface->client());
    }
    bool agent_conflict(const Client& c) const {
        if (c.route == InputRoute::primary_foreground)
            for (const auto* peer : peers)
                if (peer && peer != this && peer->lease && peer->lease->route == InputRoute::primary_foreground) return true;
        return agent_conflict(c.surface.lock());
    }
    bool agent_conflict(const SP<CWLSurfaceResource>& surface) const {
        if (!surface) return true;
        for (const auto* peer : peers) {
            if (!peer || peer == this) continue;
            // Passive hover still participates after its transport owner dies.
            if (peer->pointer_target.same_client(surface)) return true;
            const auto other = peer->lease ? peer->lease->surface.lock() : nullptr;
            if (other && other->client() == surface->client()) return true;
        }
        return false;
    }
    void retire_orphan_hover(const Client& c) {
        for (auto* peer : peers) {
            if (!peer || peer == this) continue;
            const InputLaneActivity activity{
                .reserved = peer->reservation != nullptr,
                .leased = peer->lease != nullptr,
                .dragging = peer->drag.has_value(),
                .button = peer->held_button != 0,
                .keys = !peer->held_keys.empty(),
                .keyboard_focus = std::ranges::any_of(peer->keyboards, [](const auto& k) { return bool(k->focus); }),
                .capabilities = peer->capabilities != 0,
                .grant = peer->grant.deadline() != Clock::time_point{},
                .expiry = peer->expires != Clock::time_point{},
                .reservation_without_target = peer->reservation && !peer->reservation->has_bound_target,
            };
            // Driver claims the first free lane before TARGET. Retire only a
            // matching, completely inert peer hover so opposite-order reuse
            // cannot strand a target on another lane. A fresh claimant that
            // has never bound a target does not adopt inherited hover; an
            // existing target owner remains protected even after STOP.
            if (peer->pointer_target.reclaimable_for(c.surface.lock(), activity)) peer->leave_pointer();
        }
    }
    void invalidate(Client& c, bool clear_pointer = true) {
        if (lease == &c || (clear_pointer && c.window.lock() && pointer_target.window() == c.window.lock()))
            revoke("stale_target", !clear_pointer);
        c.token.clear(); c.window.reset(); c.surface.reset(); c.xwayland_xid = 0;
        c.destination_window.reset(); c.destination_surface.reset(); c.destination_revision = 0;
        c.destination_xid = 0; c.destination_primary_seat.reset();
        c.source_capture.reset(); c.destination_capture.reset();
        c.frame_scope = false; c.window_capture_scope = false; c.frame_hit.reset();
        c.frame_epoch.clear(); c.frame_lifetime.clear(); c.frame_tree_revision = 0;
        c.captured_root_id = 0; c.captured_root_lifetime.clear();
        c.text_focus.reset();
        // Replay high-water belongs to the old target binding. A genuinely
        // new token may receive a shorter grant after Stop or target change.
        c.approved_deadline = 0;
    }
    void clear_agent_serials() {
        for (const auto& weak : agent_serial_surfaces) {
            const auto surface = weak.lock();
            if (!surface || !agent_seat) continue;
            for (std::uint32_t button = 272; button <= 274; ++button)
                g_pSeatManager->clearAgentPointerButtonSerials(agent_seat, surface->client(), surface, button);
        }
        agent_serial_surfaces.clear();
        agent_serial_expires = {};
    }
    void release_pointer_button() {
        if (!held_button) return;
        for (auto& p : pointers) {
            const auto surface = p->focus.lock();
            if (!p->dead && p->wl->resource() && surface && surface->good()) {
                p->wl->sendButton(serial(), event_ms(), held_button, WL_POINTER_BUTTON_STATE_RELEASED);
                // A complete release frame is delivery, not client processing proof.
                if (p->wl->version() >= 5) p->wl->sendFrame();
            }
        }
        held_button = 0;
    }
    void leave_pointer() {
        release_pointer_button();
        for (auto& p : pointers) {
            const auto surface = p->focus.lock();
            if (!p->dead && p->wl->resource() && surface && surface->good()) {
                p->wl->sendLeave(serial(), surface->getResource().get());
                if (p->wl->version() >= 5) p->wl->sendFrame();
            }
            p->focus.reset();
        }
        pointer_target.reset();
        pointer_unmap.reset(); pointer_destroy.reset();
    }
    void leave_keyboard() {
        if (const auto preedit = preedit_surface.lock(); preedit && agent_seat &&
            g_pSeatManager->keyboardFocusForSeat(agent_seat) == preedit)
            (void)g_pInputManager->m_relay.agentText(agent_seat, preedit, "", true);
        preedit_surface.reset();
        preedit_connection_id = 0;
        preedit_expires = {};
        if (lease) lease->text_focus.reset();
        if (agent_seat) g_pSeatManager->setSeatKeyboardFocus(agent_seat, nullptr);
        for (auto& k : keyboards) {
            const auto surface = k->focus.lock();
            if (!k->dead && k->wl->resource() && surface && surface->good()) {
                for (auto key : held_keys) k->wl->sendKey(serial(), event_ms(), key, WL_KEYBOARD_KEY_STATE_RELEASED);
                k->wl->sendModifiers(serial(), 0, 0, 0, 0);
                k->wl->sendLeave(serial(), surface->getResource().get());
            }
            k->focus.reset();
        }
        held_keys.clear();
        if (keyboard_state) xkb_state_unref(keyboard_state);
        keyboard_state = keymap ? xkb_state_new(keymap) : nullptr;
        if (physical_keyboard_state) xkb_state_unref(physical_keyboard_state);
        physical_keyboard_state = physical_keymap ? xkb_state_new(physical_keymap) : nullptr;
    }
    void retire_grant() {
        lease = nullptr; capabilities = 0; grant.reset(); expires = {};
    }
    void complete_action() {
        // All buttons/keys were released by the operation. Keyboard focus is
        // action-scoped; passive pointer focus belongs only to its live target.
        // Never send a gratuitous leave immediately after a drag's release:
        // clients may requeue their release behind that leave while coalescing
        // motion. This is not a client-processing acknowledgement.
        finish_foreground();
        leave_keyboard();
        retire_grant();
    }
    void revoke(std::string_view reason, bool retain_pointer = false, ForegroundFailure failure = {ForegroundFailureReason::none}) {
        if (lease && trace && reason != "completed") trace->mark("agent_cancel", lane + 1);
        if (drag && drag->client && !drag->client->dead && drag->destination_revision &&
            !drag->native_data && drag->client->route == InputRoute::primary_foreground) {
            send(*drag->client, std::format(
                R"({{"ok":false,"code":"foreground_partial_unknown","detail":"{}","effect":"partial","route":"primary_foreground_data_device","press_delivered":true,"motion_delivered":{},"release_delivered":false}})",
                reason, drag->threshold_crossed || drag->offered));
        } else if (drag && drag->client && !drag->client->dead && drag->native_data) {
            send(*drag->client, std::format(
                R"({{"ok":false,"code":"{}","detail":"{}","effect":"partial","press_delivered":true}})",
                reason, reason));
        } else if (drag && drag->client && !drag->client->dead) {
            send(*drag->client, failure.reason == ForegroundFailureReason::none ?
                refusal(foreground_started ? "foreground_partial_unknown" : reason) :
                refusal(foreground_started ? "foreground_partial_unknown" : reason, failure));
        }
        if (agent_seat && PROTO::data && drag && drag->native_data)
            PROTO::data->cancelNativeDrag(agent_seat);
        drag.reset();
        finish_foreground();
        clear_agent_serials();
        if (agent_seat && PROTO::xdgShell) PROTO::xdgShell->cancelAgentGrab(agent_seat);
        if (retain_pointer) release_pointer_button(); else leave_pointer();
        leave_keyboard(); retire_grant();
    }
    void cancel_authority(std::string_view reason, bool retain_pointer = true) {
        revoke(reason, retain_pointer);
        if (reservation) selection.clearIfOwner(agent_seat, reservation->connection_id);
        // Invalidate pending authority as well as active work. This also makes
        // unused signed renewals unusable in experimental builds. Keep the lane
        // reservation; v3 requires fresh TARGET admission for the next action.
        for (auto& c : clients) invalidate(*c, false);
    }
    void desktop_transition() {
        // Signals can fire before Hyprland updates its aggregate state. Revoke
        // unconditionally; an off/on pair between timer ticks must not revive
        // authority. Kill pending connections as well as the active lease so
        // a pre-transition signed grant cannot be approved after unlock.
        // Admission is gone immediately, but an unchanged live target may keep
        // inert hover just as after normal completion. A leave directly after
        // the release can interfere with the client's drag-release processing.
        revoke("desktop_changed", true);
        selection.clearAll();
        for (auto& c : clients) {
            // Observers and operator-control connections own no action target.
            // Keep them alive so Stop/status/evidence survives a transition.
            if (reservation != c.get() && c->token.empty()) continue;
            c->dead = true;
            if (c->source) wl_event_source_remove(c->source);
            c->source = nullptr;
            if (c->fd >= 0) close(c->fd);
            c->fd = -1;
        }
        reservation = nullptr;
        if (desktop_generation == UINT64_MAX)
            suspended = true;
        else
            ++desktop_generation;
    }
    void send(Client& c, const std::string& packet) {
        if (c.dead) return;
        const auto n = ::send(c.fd, packet.data(), packet.size(), MSG_DONTWAIT | MSG_NOSIGNAL);
        if (n < 0 || static_cast<std::size_t>(n) != packet.size()) c.dead = true;
    }
    static int accept_ready(int, std::uint32_t, void* data) {
        auto& self = *static_cast<Impl*>(data);
        for (unsigned i = 0; i < 8; ++i) {
            const auto fd = accept4(self.listener, nullptr, nullptr, SOCK_NONBLOCK | SOCK_CLOEXEC);
            if (fd < 0) break;
            ucred credentials{}; socklen_t size = sizeof(credentials);
            if (getsockopt(fd, SOL_SOCKET, SO_PEERCRED, &credentials, &size) != 0 ||
                size != sizeof(credentials) || credentials.uid != getuid() || self.clients.size() >= kMaxClients) { close(fd); continue; }
            bool owned = false;
            try {
                auto c = std::make_unique<Client>(); c->owner = &self; c->fd = fd; owned = true; c->challenge = nonce();
                if (self.next_connection_id == 0) throw std::runtime_error("connection identity exhausted");
                c->connection_id = self.next_connection_id++;
                c->source = wl_event_loop_add_fd(g_pCompositor->m_wlEventLoop, fd, WL_EVENT_READABLE, client_ready, c.get());
                if (!c->source) continue;
                self.clients.push_back(std::move(c));
            } catch (...) { if (!owned) close(fd); }
        }
        return 0;
    }
    static int client_ready(int fd, std::uint32_t mask, void* data) {
        auto& c = *static_cast<Client*>(data); auto& self = *c.owner;
        if (mask & (WL_EVENT_HANGUP | WL_EVENT_ERROR)) c.dead = true;
        for (unsigned i = 0; i < 8 && !c.dead; ++i) {
            // Check before reading or changing HELLO state, even if the event
            // loop's periodic expiry callback has not run yet.
            if (c.deadline.expired(c.hello, Clock::now())) { c.dead = true; break; }
            std::array<char, kMaxPacket> buffer{};
            const auto n = recv(fd, buffer.data(), buffer.size(), MSG_DONTWAIT | MSG_TRUNC);
            if (n < 0) { if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) c.dead = true; break; }
            if (!n) { c.dead = true; break; }
            const auto received = Clock::now();
            if (c.deadline.expired(c.hello, received)) { c.dead = true; break; }
            c.deadline.touch(received);
            if (static_cast<std::size_t>(n) > buffer.size()) { self.send(c, refusal("invalid_request")); continue; }
            try { self.request(c, fields(std::string_view(buffer.data(), n))); }
            catch (const ForegroundFailure& failure) {
                const bool started = c.foreground_attempted;
                const bool data_pressed = self.foreground_data_pressed;
                const bool delivered = self.foreground_data_press_delivered;
                const bool drag_reply = self.drag && self.drag->client == &c;
                self.revoke("foreground_partial_unknown", false, failure);
                if (!drag_reply) self.send(c, data_pressed ?
                    std::format(R"({{"ok":false,"code":"foreground_partial_unknown","detail":"{}","effect":"partial","press_delivered":{}}})",
                        failure.detail(), delivered ? "true" : "null") :
                    refusal(ForegroundFailure::code(started), failure));
            }
            catch (...) {
                const bool foreground_partial = c.foreground_attempted;
                const bool data_pressed = self.foreground_data_pressed;
                const bool delivered = self.foreground_data_press_delivered;
                const bool had_drag_reply = self.drag && self.drag->client == &c;
                if (kProduction && (self.lease == &c || self.reservation == &c)) self.revoke("invalid_request");
                if (!had_drag_reply) self.send(c, data_pressed ?
                    std::format(R"({{"ok":false,"code":"foreground_partial_unknown","detail":"foreground_partial_unknown","effect":"partial","press_delivered":{}}})",
                        delivered ? "true" : "null") :
                    refusal(foreground_partial ? "foreground_partial_unknown" : "invalid_request"));
            }
        }
        if (c.dead) {
            self.selection.clearIfOwner(self.agent_seat, c.connection_id);
            if (self.lease == &c || self.reservation == &c) self.revoke("disconnected", true);
            else if (self.preedit_connection_id == c.connection_id) self.leave_keyboard();
            if (self.reservation == &c) self.reservation = nullptr;
        }
        return 0;
    }
#ifdef CUA_HYPRLAND_TEST_INPUT
    bool valid_signature(const std::string& message, const std::string& signature) const {
        if (signature.size() != 128) return false;
        const auto bytes = unhex(signature);
        auto* key = EVP_PKEY_new_raw_public_key(EVP_PKEY_ED25519, nullptr, public_key.data(), public_key.size());
        auto* ctx = EVP_MD_CTX_new();
        const bool ok = key && ctx && EVP_DigestVerifyInit(ctx, nullptr, nullptr, nullptr, key) == 1 &&
            EVP_DigestVerify(ctx, bytes.data(), bytes.size(), reinterpret_cast<const unsigned char*>(message.data()), message.size()) == 1;
        EVP_MD_CTX_free(ctx); EVP_PKEY_free(key); return ok;
    }
    void approve(Client& c, const std::vector<std::string>& f) {
        if (f.size() != 6) { send(c, refusal("invalid_grant")); return; }
        const auto deadline = number(f[3]); const auto caps = number(f[4]); const auto now = unix_ms();
        if (deadline <= now || deadline - now > 60000 || caps == 0 || caps > 15) { send(c, refusal("invalid_grant")); return; }
        const auto message = std::format("CUA_TEST_LEASE_1\n{}\n{}\n{}\n{}\n{}\n", epoch, f[1], f[2], deadline, caps);
        if (!valid_signature(message, f[5])) { send(c, refusal("invalid_grant")); return; }
        const auto found = std::ranges::find_if(clients, [&](auto& peer) {
            return !peer->dead && peer->hello && peer->challenge == f[1] && peer->token == f[2] && !peer->token.empty();
        });
        if (found == clients.end() || !refresh(**found)) { send(c, refusal("stale_target")); return; }
        if (reservation != found->get()) { send(c, refusal("lane_not_claimed")); return; }
        if (lease) { send(c, refusal("lease_busy")); return; }
        if (agent_conflict(**found)) { send(c, refusal("agent_target_busy")); return; }
        if (deadline <= (*found)->approved_deadline) { send(c, refusal("invalid_grant")); return; }
        if (!available()) { send(c, refusal("session_unavailable")); return; }
        // Bind steady-clock lifetime once; a clock change cannot extend a lease.
        lease = found->get(); capabilities = caps; expires = Clock::now() + std::chrono::milliseconds(deadline - now);
        lease->approved_deadline = deadline;
        // Approval begins the bounded active period. Time spent waiting for
        // the external operator must not consume the input connection's idle
        // budget while its newly approved lease is still valid.
        lease->deadline.touch(Clock::now());
        if (trace) trace->mark("agent_approved", lane + 1);
        send(c, R"({"ok":true})");
    }
#endif
    static std::optional<std::array<double, 4>> surface_box(const SP<CWLSurfaceResource>& surface,
        const std::optional<CBox>& root_box = std::nullopt) {
        const auto view = surface ? surface->m_hlSurface.lock() : nullptr;
        const auto box = view ? view->getSurfaceBoxGlobal() : root_box;
        if (!box || !std::isfinite(box->x) || !std::isfinite(box->y) ||
            !std::isfinite(box->w) || !std::isfinite(box->h) || box->w <= 0 || box->h <= 0)
            return std::nullopt;
        return std::array<double, 4>{box->x, box->y, box->w, box->h};
    }
    static std::optional<InventoryRow> describe_surface(const PHLWINDOW& window,
        const SP<CWLSurfaceResource>& surface, bool popup, const std::optional<CBox>& root_box = std::nullopt) {
        if (!surface || !surface->good() || !surface->m_mapped || !window)
            return std::nullopt;
        const auto geometry = target_geometry(window, window->resource());
        const auto box = surface_box(surface, root_box);
        if (!geometry || !box || surface->client() != window->resource()->client())
            return std::nullopt;
        return InventoryRow{static_cast<std::uint64_t>(window->getPID()),
            reinterpret_cast<std::uintptr_t>(window.get()), surface->id(),
            surface_lifetime(surface), *geometry, *box, popup};
    }
    static std::string row_json(const InventoryRow& row) {
        const auto& g = row.root_geometry;
        const auto& b = row.box;
        return std::format(
            R"({{"pid":{},"address":"{:x}","surface_id":{},"lifetime":"{}","root_geometry":[{:.17g},{:.17g},{:.17g},{:.17g},{:.17g},{:.17g}],"box":[{:.17g},{:.17g},{:.17g},{:.17g}],"popup":{}}})",
            row.pid, row.address, row.surface_id, row.lifetime,
            g[0], g[1], g[2], g[3], g[4], g[5], b[0], b[1], b[2], b[3], row.popup);
    }
    static std::optional<std::vector<InventoryRow>> scene_inventory() {
        prune_surface_identities();
        std::vector<InventoryRow> rows;
        for (const auto& window : Desktop::windowState()->windows()) {
            if (!window || !window->m_isMapped || window->isHidden() ||
                !window->resource()) continue;
            const auto append_tree = [&](const SP<CWLSurfaceResource>& root, bool popup,
                                         const std::optional<CBox>& root_box) {
                root->breadthfirst([&](SP<CWLSurfaceResource> surface, const Vector2D&, void*) {
                    if (const auto row = describe_surface(window, surface, popup,
                        surface == root ? root_box : std::nullopt)) rows.push_back(*row);
                }, nullptr);
            };
            if (window->m_isX11) {
                const auto root = describe_surface(window, window->resource(), false, window->surfaceLogicalBox());
                if (root) rows.push_back(*root);
                if (rows.size() > kMaxResources) return std::nullopt;
                continue;
            }
            append_tree(window->resource(), false, window->surfaceLogicalBox());
            if (window->m_popupHead)
                window->m_popupHead->breadthfirst([&](SP<Desktop::View::CPopup> popup, void*) {
                    if (!popup || popup->type() != Desktop::View::VIEW_TYPE_POPUP || !popup->visible())
                        return;
                    const auto surface = popup->resource();
                    if (surface && surface->m_mapped) append_tree(surface, true, popup->surfaceLogicalBox());
                }, nullptr);
            if (rows.size() > kMaxResources) return std::nullopt;
        }
        std::ranges::sort(rows, [](const InventoryRow& a, const InventoryRow& b) {
            return std::tie(a.address, a.surface_id, a.lifetime) <
                std::tie(b.address, b.surface_id, b.lifetime);
        });
        return rows;
    }
    std::optional<std::pair<InventoryRow, InventoryRow>> inspect_window_hit(
        std::uint64_t pid, std::uint64_t address, double x, double y) const {
        for (const auto& window : Desktop::windowState()->windows()) {
            if (!window || reinterpret_cast<std::uintptr_t>(window.get()) != address ||
                static_cast<std::uint64_t>(window->getPID()) != pid ||
                !window->m_isMapped || window->isHidden() || !window->resource()) continue;
            const auto root = describe_surface(window, window->resource(), false, window->surfaceLogicalBox());
            if (!root) return std::nullopt;
            Client observer;
            observer.window = window; observer.surface = window->resource(); observer.geometry = root->root_geometry;
            const auto selected = hit(observer, x, y);
            if (!selected || selected->popup) return std::nullopt;
            if (window->m_isX11 && selected->surface != window->resource()) return std::nullopt;
            const auto child = describe_surface(window, selected->surface, false,
                selected->surface == window->resource() ? window->surfaceLogicalBox() : std::nullopt);
            if (!child) return std::nullopt;
            return std::pair{*root, *child};
        }
        return std::nullopt;
    }
    static CaptureProof parse_capture_proof(const std::vector<std::string>& f, std::size_t offset) {
        CaptureProof proof;
        proof.point = {real(f[offset]), real(f[offset + 1])};
        proof.epoch = f[offset + 2];
        proof.tree_revision = number(f[offset + 3]);
        const auto root_id = number(f[offset + 4]), hit_id = number(f[offset + 12]), xid = number(f[offset + 19]);
        if (root_id == 0 || root_id > UINT32_MAX || hit_id == 0 || hit_id > UINT32_MAX || xid > UINT32_MAX)
            throw std::runtime_error("invalid capture identity");
        proof.root_id = static_cast<std::uint32_t>(root_id);
        proof.root_lifetime = f[offset + 5];
        for (unsigned i = 0; i < 6; ++i) proof.root_geometry[i] = real(f[offset + 6 + i]);
        proof.hit_id = static_cast<std::uint32_t>(hit_id);
        proof.hit_lifetime = f[offset + 13];
        for (unsigned i = 0; i < 4; ++i) proof.hit_box[i] = real(f[offset + 14 + i]);
        if (number(f[offset + 18]) != 0) throw std::runtime_error("captured popup unsupported");
        proof.xid = static_cast<std::uint32_t>(xid);
        if (proof.epoch.size() != 32 || proof.root_lifetime.size() != 32 ||
            proof.hit_lifetime.size() != 32) throw std::runtime_error("invalid capture lifetime");
        return proof;
    }
    bool capture_endpoint_valid(const PHLWINDOW& window, const CaptureProof& proof) const {
        if (!window || !window->m_isMapped || window->isHidden() || !window->resource() ||
            proof.epoch != scene_epoch || proof.tree_revision != Desktop::viewTreeRevision) return false;
        const auto x = window->m_isX11 ? window->m_xwaylandSurface.lock() : nullptr;
        if (window->m_isX11 ? (!x || !x->m_mapped || !proof.xid ||
            x->m_xID != proof.xid || x->m_pid != window->getPID()) : proof.xid != 0) return false;
        const auto rows = inspect_window_hit(static_cast<std::uint64_t>(window->getPID()),
            reinterpret_cast<std::uintptr_t>(window.get()), proof.point.x, proof.point.y);
        return rows && rows->first.surface_id == proof.root_id &&
            rows->first.lifetime == proof.root_lifetime &&
            rows->first.root_geometry == proof.root_geometry &&
            rows->second.surface_id == proof.hit_id &&
            rows->second.lifetime == proof.hit_lifetime &&
            rows->second.box == proof.hit_box && !rows->second.popup;
    }
    bool capture_pair_valid(const Client& c) const {
        return (!c.source_capture && !c.destination_capture) ||
            (c.source_capture && c.destination_capture &&
                capture_endpoint_valid(c.window.lock(), *c.source_capture) &&
                capture_endpoint_valid(c.destination_window.lock(), *c.destination_capture));
    }
    struct Hit {
        SP<CWLSurfaceResource> surface;
        Vector2D local;
        bool popup = false;
    };
    std::optional<Hit> hit(Client& c, double x, double y) const {
        const auto window = c.window.lock();
        const auto root = c.surface.lock();
        if (!window || !root || !std::isfinite(x) || !std::isfinite(y)) return std::nullopt;
        const Vector2D global{x + c.geometry[0], y + c.geometry[1]};
        if (c.frame_scope && window->m_popupHead) {
            const auto popup = window->m_popupHead->at(global, true);
            if (popup && popup->type() == Desktop::View::VIEW_TYPE_POPUP && popup->visible()) {
                const auto box = popup->surfaceLogicalBox();
                const auto surface = popup->resource();
                if (box && surface && surface->good() && surface->m_mapped &&
                    surface->client() == root->client()) {
                    const auto [child, local] = surface->at(global - Vector2D{box->x, box->y}, true);
                    if (child && child->good() && child->m_mapped && child->client() == root->client())
                        return Hit{child, local, true};
                }
            }
        }
        if (x < 0 || y < 0 || x >= c.geometry[2] || y >= c.geometry[3]) return std::nullopt;
        const Vector2D local{x + c.geometry[0] - c.geometry[4], y + c.geometry[1] - c.geometry[5]};
        const auto [surface, surface_local] = root->at(local, true);
        if (!surface || !surface->good() || !surface->m_mapped || surface->client() != root->client())
            return std::nullopt;
        // A window export captures the root and subsurfaces, not xdg popups.
        // Without a desktop-frame assertion even an in-bounds popup is hidden.
        if (!c.frame_scope && window->m_popupHead) {
            const auto popup = window->m_popupHead->at(global, true);
            if (popup && popup->type() == Desktop::View::VIEW_TYPE_POPUP && popup->visible())
                return std::nullopt;
        }
        return Hit{surface, surface_local, false};
    }
    bool window_capture_valid(Client& c) const {
        if (!c.window_capture_scope) return true;
        const auto window = c.window.lock();
        const auto root = c.surface.lock();
        if (!window || !root || c.frame_epoch != scene_epoch ||
            c.frame_tree_revision != Desktop::viewTreeRevision) return false;
        const auto observed = describe_surface(window, root, false, window->surfaceLogicalBox());
        if (!observed || observed->surface_id != c.captured_root_id ||
            observed->lifetime != c.captured_root_lifetime ||
            observed->root_geometry != c.captured_root_geometry) return false;
        if (!c.frame_surface_id) return true;
        const auto rows = inspect_window_hit(observed->pid, observed->address, c.frame_point.x, c.frame_point.y);
        return rows && rows->first.surface_id == c.captured_root_id &&
            rows->first.lifetime == c.captured_root_lifetime &&
            rows->first.root_geometry == c.captured_root_geometry &&
            rows->second.surface_id == c.frame_surface_id &&
            rows->second.lifetime == c.frame_lifetime &&
            rows->second.box == c.frame_box && !rows->second.popup &&
            hit(c, c.frame_point.x, c.frame_point.y).transform([](const Hit& h) { return h.surface; }) == c.frame_hit.lock();
    }
    bool point(Client& c, double x, double y) const {
        if (c.window_capture_scope && !window_capture_valid(c)) return false;
        if (!c.frame_scope) {
            if (!std::isfinite(x) || !std::isfinite(y) || x < 0 || y < 0 ||
                x >= c.geometry[2] || y >= c.geometry[3] || !hit(c, x, y)) return false;
            return !c.window_capture_scope || !c.frame_surface_id ||
                (x == c.frame_point.x && y == c.frame_point.y) || capabilities == 8 ||
                (drag && drag->client == &c && !drag->native_data && !drag->destination_revision);
        }
        if (x != c.frame_point.x || y != c.frame_point.y ||
            c.frame_epoch != scene_epoch || c.frame_tree_revision != Desktop::viewTreeRevision)
            return false;
        const auto row = inspect_frame_row(static_cast<std::uint64_t>(c.window.lock()->getPID()),
            reinterpret_cast<std::uintptr_t>(c.window.lock().get()), x, y);
        return row && row->surface_id == c.frame_surface_id && row->lifetime == c.frame_lifetime &&
            row->box == c.frame_box && row->popup == c.frame_popup &&
            hit(c, x, y).transform([](const Hit& result) { return result.surface; }) == c.frame_hit.lock();
    }
    bool foreground_endpoint_visible(const PHLWINDOW& window,
                                     const std::array<double, 6>& geometry, double x, double y) const {
        if (!window || !std::isfinite(x) || !std::isfinite(y) || x < 0 || y < 0 ||
            x >= geometry[2] || y >= geometry[3]) return false;
        const Vector2D global{x + geometry[0], y + geometry[1]};
        const auto monitor = State::monitorState()->query().vec(global).run();
        if (!monitor) return false;
        const auto tester = Desktop::viewState()->hitTest();
        if (tester.windowAt(global, Desktop::View::INPUT_EXTENTS | Desktop::View::ALLOW_FLOATING) != window)
            return false;
        Vector2D local;
        PHLLS layer;
        return !tester.layerPopupSurfaceAt(global, monitor, &local, &layer) &&
            !tester.layerSurfaceAt(global, &monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY], &local, &layer) &&
            !tester.layerSurfaceAt(global, &monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_TOP], &local, &layer);
    }
    std::optional<InventoryRow> inspect_frame_row(std::uint64_t pid, std::uint64_t address,
                                                   double x, double y) const {
        if (!std::isfinite(x) || !std::isfinite(y)) return std::nullopt;
        for (const auto& window : Desktop::windowState()->windows()) {
            if (!window || reinterpret_cast<std::uintptr_t>(window.get()) != address ||
                static_cast<std::uint64_t>(window->getPID()) != pid || window->m_isX11 ||
                !window->m_isMapped || window->isHidden() || !window->resource()) continue;
            const auto geometry = target_geometry(window, window->resource());
            if (!geometry) return std::nullopt;
            const Vector2D global{x + (*geometry)[0], y + (*geometry)[1]};
            const auto monitor = State::monitorState()->query().vec(global).run();
            if (!monitor) return std::nullopt;
            const auto tester = Desktop::viewState()->hitTest();
            if (tester.windowAt(global, Desktop::View::INPUT_EXTENTS | Desktop::View::ALLOW_FLOATING) != window)
                return std::nullopt;
            Vector2D layer_local;
            PHLLS layer;
            if (tester.layerPopupSurfaceAt(global, monitor, &layer_local, &layer) ||
                tester.layerSurfaceAt(global, &monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_OVERLAY], &layer_local, &layer) ||
                tester.layerSurfaceAt(global, &monitor->m_layerSurfaceLayers[ZWLR_LAYER_SHELL_V1_LAYER_TOP], &layer_local, &layer))
                return std::nullopt;
            Client observer;
            observer.window = window;
            observer.surface = window->resource();
            observer.geometry = *geometry;
            observer.frame_scope = true;
            const auto target = hit(observer, x, y);
            if (!target) return std::nullopt;
            return describe_surface(window, target->surface, target->popup,
                target->surface == window->resource() ? window->surfaceLogicalBox() : std::nullopt);
        }
        return std::nullopt;
    }
    bool unique_primary_seat(wl_client* client) const {
        struct Scan {
            const Impl* self;
            ForegroundSeatBindings bindings;
        } scan{this, {}};
        // Hyprland's public lookup returns only the first primary binding.
        // Count classes without casting arbitrary protocol user data. Agent
        // seats are excluded only by exact resource identity, never by name.
        wl_client_for_each_resource(client, [](wl_resource* resource, void* data) {
            auto& scan = *static_cast<Scan*>(data);
            const std::string_view resource_class = wl_resource_get_class(resource);
            if (resource_class != "wl_seat") return WL_ITERATOR_CONTINUE;
            bool owned = false;
            for (const auto* peer : scan.self->peers) {
                if (!peer) continue;
                for (const auto& seat : peer->seats)
                    if (seat->wl->resource() == resource) { owned = true; break; }
                if (owned) break;
            }
            scan.bindings.observe(resource_class, owned);
            return WL_ITERATOR_CONTINUE;
        }, &scan);
        if (!scan.bindings.unique()) return false;
        if (!foreground_started) return true;
        // A release/rebind or new child resource during a drag must not leave
        // our captured delivery set different from the primary seat's set.
        const auto seat = g_pSeatManager->seatResourceForClient(client);
        if (!seat || seat != foreground_seat.lock()) return false;
        return foreground_resources_match(seat->m_pointers, foreground_pointers) &&
            foreground_resources_match(seat->m_keyboards, foreground_keyboards);
    }
    ForegroundGuard foreground_guard(const Client& c) const {
        const auto root = c.surface.lock();
        const auto geometry = target_geometry(c.window.lock(), root);
        bool pressed = !g_pInputManager->getKeysFromAllKBs().empty();
        for (const auto& kb : g_pInputManager->m_keyboards) {
            if (!kb->m_enabled || kb->isVirtual()) continue;
            for (std::uint32_t code = 0; code <= KEY_MAX && !pressed; ++code) pressed = kb->getPressed(code);
        }
        const bool exact_root = geometry && *geometry == c.geometry;
        return {
            .exact_root = exact_root,
            .primary_binding = !exact_root || unique_primary_seat(root->client()),
            .peer_conflict = agent_conflict(c),
            .physical_keys = pressed,
            .physical_buttons = g_pInputManager->hasHeldButtons(),
            .grab = bool(g_pSeatManager->m_seatGrab) || bool(g_layoutManager->dragController()->target()) ||
                !g_pInputManager->m_exclusiveLSes.empty(),
            .dnd = PROTO::data && PROTO::data->dndActive(),
            .constraint = g_pInputManager->isConstrained(),
            .exact_keyboard_focus = root && g_pSeatManager->m_state.keyboardFocus == root &&
                Desktop::focusState()->window() == c.window.lock() && Desktop::focusState()->surface() == root,
            .exact_pointer_focus = root && (g_pSeatManager->m_state.pointerFocus == root ||
                (c.source_capture && foreground_pointer_surface &&
                    g_pSeatManager->m_state.pointerFocus == foreground_pointer_surface.lock())),
        };
    }
    bool foreground_transfer_ready(const Client& c) const {
        const auto root = c.surface.lock(), origin = foreground_data_origin.lock();
        if (!foreground_started || !foreground_data_pressed || !root || !origin ||
            origin->client() != root->client() || lease != &c || c.dead || !available() ||
            Clock::now() >= expires || !destination_valid(c)) return false;
        auto guard = foreground_guard(c);
        // The owned press is in CInputManager's held-button list. Any human
        // event cancels synchronously before reaching that list.
        guard.physical_buttons = false;
        guard.dnd = PROTO::data && PROTO::data->dndActive() &&
            !PROTO::data->primaryDragMatches(origin);
        if (guard.activation_failure() != ForegroundFailureReason::none) return false;
        return (PROTO::data && PROTO::data->primaryDragMatches(origin)) ||
            g_pSeatManager->m_state.pointerFocus == origin;
    }
    void primary_data_button(bool pressed) {
        const auto pointer = g_pSeatManager->m_mouse.lock();
        if (!pointer) throw ForegroundFailure{ForegroundFailureReason::physical_pointer};
        const auto previous = foreground_activating;
        foreground_activating = true;
        try {
            g_pInputManager->onMouseButton(
                {.timeMs = event_ms(), .button = 272,
                 .state = pressed ? WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED,
                 .mouse = false}, pointer);
        } catch (...) {
            foreground_activating = previous;
            throw;
        }
        foreground_activating = previous;
    }
    void primary_data_motion(const Vector2D& global) {
        const auto previous = foreground_activating;
        foreground_activating = true;
        try {
            ::Pointer::mgr()->warpTo(global);
            g_pInputManager->simulateMouseMovement();
            Event::SCallbackInfo info;
            Event::bus()->m_events.input.mouse.move.emit(global, info);
            g_pSeatManager->sendPointerFrame();
        } catch (...) {
            foreground_activating = previous;
            throw;
        }
        foreground_activating = previous;
    }
    void require_foreground(Client& c) {
        if (lease != &c) throw ForegroundFailure{ForegroundFailureReason::lease};
        if (c.dead) throw ForegroundFailure{ForegroundFailureReason::client_dead};
        if (!available()) throw ForegroundFailure{ForegroundFailureReason::session_unavailable};
        if (!input_layout_qualified(c.route, foreground_needs_keyboard, layout_qualified()))
            throw ForegroundFailure{ForegroundFailureReason::unsupported_layout};
        if (Clock::now() >= expires) throw ForegroundFailure{ForegroundFailureReason::lease_expired};
        const auto failure = foreground_guard(c).dispatch_failure(foreground_needs_pointer);
        if (failure != ForegroundFailureReason::none) throw ForegroundFailure{failure};
    }
    void finish_foreground() {
        if (!foreground_started) { foreground_activating = false; return; }
        const auto root = foreground_surface.lock(), origin = foreground_data_origin.lock();
        if (foreground_data_pressed) {
            const bool own_drag = origin && PROTO::data && PROTO::data->primaryDragMatches(origin);
            const bool safe_release = own_drag || (origin && origin->good() && PROTO::data &&
                !PROTO::data->dndActive() && g_pSeatManager->m_state.pointerFocus == origin);
            foreground_activating = true;
            if (own_drag) PROTO::data->abortDndIfPresent();
            if (safe_release && g_pSeatManager->m_mouse)
                g_pInputManager->onMouseButton(
                    {.timeMs = event_ms(), .button = 272, .state = WL_POINTER_BUTTON_STATE_RELEASED, .mouse = false},
                    g_pSeatManager->m_mouse.lock());
            foreground_data_pressed = false;
            foreground_data_origin.reset();
        }
        foreground_data_press_delivered = false;
        foreground_activating = false;
        // Release only our own synthetic state, and only while these resources
        // still address the original root. A focus loss must not send to its successor.
        // The previous release only addresses the agent-owned ordinary
        // compositor drag. Other foreground actions use captured Wayland resources.
        if (root && root->good() && g_pSeatManager->m_state.pointerFocus == root)
            for (const auto& weak : foreground_pointers)
                if (const auto p = weak.lock(); p && p->good() && held_button) {
                    p->sendButton(event_ms(), held_button, WL_POINTER_BUTTON_STATE_RELEASED);
                    p->sendFrame();
                }
        if (foreground_keyboard_used && root && root->good() && g_pSeatManager->m_state.keyboardFocus == root)
            for (const auto& weak : foreground_keyboards)
                if (const auto k = weak.lock(); k && k->good()) {
                    for (auto code : held_keys) k->sendKey(event_ms(), code, WL_KEYBOARD_KEY_STATE_RELEASED);
                    k->sendMods(foreground_modifiers[0], foreground_modifiers[1], foreground_modifiers[2], foreground_modifiers[3]);
                }
        held_button = 0; held_keys.clear();
        foreground_pointers.clear(); foreground_keyboards.clear(); foreground_surface.reset();
        foreground_pointer_surface.reset(); foreground_seat.reset();
        foreground_started = false;
        foreground_keyboard_used = false;
        foreground_needs_keyboard = false;
    }
    void start_foreground(Client& c, double x, double y, bool needs_pointer, bool needs_keyboard) {
        const auto root = c.surface.lock();
        const auto physical = g_pSeatManager->m_keyboard.lock();
        const auto failure = foreground_guard(c).activation_failure();
        if (failure != ForegroundFailureReason::none) throw ForegroundFailure{failure};
        if (needs_keyboard && !physical) throw ForegroundFailure{ForegroundFailureReason::physical_keyboard};
        if (needs_keyboard && !physical_keyboard_state) throw ForegroundFailure{ForegroundFailureReason::keyboard_state};
        if (needs_pointer && !g_pSeatManager->m_mouse) throw ForegroundFailure{ForegroundFailureReason::physical_pointer};
        const Vector2D local{x + c.geometry[0] - c.geometry[4], y + c.geometry[1] - c.geometry[5]};
        const auto selected = needs_pointer ? hit(c, x, y) : std::nullopt;
        if (needs_pointer && (!point(c, x, y) || !selected ||
            (!c.source_capture && selected->surface != root) ||
            root->at(local, true).first != selected->surface ||
            (c.xwayland_xid && !foreground_endpoint_visible(c.window.lock(), c.geometry, x, y))))
            throw ForegroundFailure{ForegroundFailureReason::pointer_target};
        const auto seat = g_pSeatManager->seatResourceForClient(root->client());
        if (!seat || !seat->good()) throw ForegroundFailure{ForegroundFailureReason::seat_resource};
        foreground_pointers.clear(); foreground_keyboards.clear();
        for (const auto& p : seat->m_pointers) if (p && p->good()) foreground_pointers.push_back(p);
        for (const auto& k : seat->m_keyboards) if (k && k->good()) foreground_keyboards.push_back(k);
        if (needs_pointer && foreground_pointers.empty()) throw ForegroundFailure{ForegroundFailureReason::pointer_resources};
        if (needs_keyboard && foreground_keyboards.empty())
            throw ForegroundFailure{ForegroundFailureReason::keyboard_resources};
        foreground_modifiers = {};
        if (needs_keyboard) {
            foreground_modifiers = {physical->m_modifiersState.depressed, physical->m_modifiersState.latched,
                physical->m_modifiersState.locked, physical->m_modifiersState.group};
            for (const auto& kb : g_pInputManager->m_keyboards) {
                if (!kb->m_enabled || !kb->shareStates() ||
                    (kb->isVirtual() && g_pInputManager->shouldIgnoreVirtualKeyboard(kb))) continue;
                foreground_modifiers[0] |= kb->m_modifiersState.depressed;
                foreground_modifiers[1] |= kb->m_modifiersState.latched;
                foreground_modifiers[2] |= kb->m_modifiersState.locked;
            }
            auto compatibility_modifiers = foreground_modifiers;
            const auto num_index = physical_keymap ?
                xkb_keymap_mod_get_index(physical_keymap, XKB_MOD_NAME_NUM) : XKB_MOD_INVALID;
            if (num_index != XKB_MOD_INVALID && num_index < 32)
                compatibility_modifiers[2] &= ~(std::uint32_t{1} << num_index);
            const auto modifier_failure = foreground_key_modifier_failure(compatibility_modifiers);
            if (modifier_failure != ForegroundFailureReason::none) throw ForegroundFailure{modifier_failure};
            xkb_state_update_mask(physical_keyboard_state, foreground_modifiers[0], foreground_modifiers[1],
                foreground_modifiers[2], 0, 0, foreground_modifiers[3]);
        }
        foreground_surface = root;
        foreground_pointer_surface = selected ? selected->surface : root;
        foreground_seat = seat;
        foreground_needs_pointer = needs_pointer;
        foreground_needs_keyboard = needs_keyboard;
        c.foreground_attempted = true;
        foreground_started = true;
        foreground_activating = true;
        // Activation intentionally persists. Never save, borrow, or restore focus.
        if (g_pSeatManager->m_state.keyboardFocus != root || Desktop::focusState()->window() != c.window.lock() ||
            Desktop::focusState()->surface() != root)
            Desktop::focusState()->fullWindowFocus(c.window.lock(), Desktop::FOCUS_REASON_OTHER, root);
        foreground_activating = false;
        if (lease != &c) throw ForegroundFailure{ForegroundFailureReason::lease};
        const auto focus_failure = foreground_guard(c).dispatch_failure(false);
        if (focus_failure != ForegroundFailureReason::none) throw ForegroundFailure{focus_failure};
        if (!needs_pointer) { require_foreground(c); return; }
        foreground_activating = true;
        ::Pointer::mgr()->warpTo({x + c.geometry[0], y + c.geometry[1]});
        if (lease != &c) throw ForegroundFailure{ForegroundFailureReason::lease};
        const auto warp_failure = foreground_guard(c).dispatch_failure(false);
        if (warp_failure != ForegroundFailureReason::none) throw ForegroundFailure{warp_failure};
        g_pSeatManager->setPointerFocus(foreground_pointer_surface.lock(),
            selected->local * (c.xwayland_xid ? c.window.lock()->m_X11SurfaceScaledBy : 1.0));
        foreground_activating = false;
        require_foreground(c);
        foreground_motion(c, x, y);
    }
    void foreground_motion(Client& c, double x, double y) {
        require_foreground(c);
        const auto selected = hit(c, x, y);
        if (!point(c, x, y) || !selected || selected->surface != foreground_pointer_surface.lock())
            throw ForegroundFailure{ForegroundFailureReason::pointer_target};
        ::Pointer::mgr()->warpTo({x + c.geometry[0], y + c.geometry[1]});
        require_foreground(c);
        for (const auto& weak : foreground_pointers) {
            const auto p = weak.lock(); if (!p || !p->good()) throw ForegroundFailure{ForegroundFailureReason::pointer_resources};
            p->sendMotion(event_ms(), selected->local * (c.xwayland_xid ? c.window.lock()->m_X11SurfaceScaledBy : 1.0)); p->sendFrame();
        }
    }
    void foreground_button(Client& c, std::uint32_t code, bool pressed) {
        require_foreground(c);
        // Mark ownership before sending, so a later resource failure can unwind.
        if (pressed) held_button = code;
        for (const auto& weak : foreground_pointers) {
            const auto p = weak.lock(); if (!p || !p->good()) throw ForegroundFailure{ForegroundFailureReason::pointer_resources};
            p->sendButton(event_ms(), code, pressed ? WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED);
            p->sendFrame();
        }
        held_button = pressed ? code : 0;
    }
    void foreground_key(Client& c, std::uint32_t code, bool pressed) {
        require_foreground(c);
        foreground_keyboard_used = true;
        xkb_state_update_key(physical_keyboard_state, code + 8, pressed ? XKB_KEY_DOWN : XKB_KEY_UP);
        if (pressed) held_keys.push_back(code); else std::erase(held_keys, code);
        for (const auto& weak : foreground_keyboards) {
            const auto k = weak.lock(); if (!k || !k->good()) throw ForegroundFailure{ForegroundFailureReason::keyboard_resources};
            k->sendKey(event_ms(), code, pressed ? WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED);
            k->sendMods(xkb_state_serialize_mods(physical_keyboard_state, XKB_STATE_MODS_DEPRESSED),
                xkb_state_serialize_mods(physical_keyboard_state, XKB_STATE_MODS_LATCHED),
                xkb_state_serialize_mods(physical_keyboard_state, XKB_STATE_MODS_LOCKED),
                xkb_state_serialize_layout(physical_keyboard_state, XKB_STATE_LAYOUT_EFFECTIVE));
        }
    }
    bool pointer_enter(Client& c, double x, double y, bool press = false) {
        pointer_refusal = "client_not_bound";
        const auto target = hit(c, x, y);
        const auto root = c.surface.lock();
        if (!target || !root || ((c.frame_scope || c.window_capture_scope) && !point(c, x, y))) return false;
        if (!held_button && agent_seat && PROTO::xdgShell) {
            const auto policy = PROTO::xdgShell->agentPointerTarget(agent_seat, target->surface, press);
            if (policy != CXDGShellProtocol::EAgentPopupTarget::allowed) {
                pointer_refusal = policy == CXDGShellProtocol::EAgentPopupTarget::dismissed ?
                    "popup_dismissed" : "popup_grab_blocked";
                return false;
            }
        }
        // A pressed button owns an implicit pointer grab. Motion continues to
        // its original entered surface; do not send a second enter or steal it.
        auto surface = target->surface;
        auto local = target->local;
        if (held_button && pointer_target.entered()) {
            surface = pointer_target.surface();
            if (!surface || !surface->good()) return false;
            const auto view = surface->m_hlSurface.lock();
            const auto box = view ? view->getSurfaceBoxGlobal() : std::nullopt;
            if (!box) return false;
            local = Vector2D{x + c.geometry[0] - box->x, y + c.geometry[1] - box->y};
        }
        unsigned count = 0;
        for (auto& p : pointers) {
            if (p->dead || !p->wl->resource() || p->wl->client() != root->client()) continue;
            if (p->focus != surface) {
                if (const auto old = p->focus.lock(); old && old->good()) {
                    p->wl->sendLeave(serial(), old->getResource().get());
                }
                p->focus = surface;
                p->wl->sendEnter(serial(), surface->getResource().get(),
                    wl_fixed_from_double(local.x), wl_fixed_from_double(local.y));
            }
            p->wl->sendMotion(event_ms(), wl_fixed_from_double(local.x), wl_fixed_from_double(local.y));
            if (p->wl->version() >= 5) p->wl->sendFrame();
            ++count;
        }
        if (count && !pointer_target.matches(c.window.lock(), surface, c.geometry)) {
            pointer_unmap.reset(); pointer_destroy.reset();
            pointer_target.capture(c.window.lock(), surface, c.geometry);
            const auto window = c.window.lock();
            pointer_unmap = window->m_events.unmap.listen([this] { revoke("stale_target"); });
            pointer_destroy = window->m_events.destroy.listen([this] { revoke("stale_target"); });
        }
        return count > 0;
    }
    void button(std::uint32_t value, bool pressed, std::uint32_t forced_serial = 0) {
        const auto event_serial = forced_serial ? forced_serial : serial();
        if (pressed && agent_seat) {
            const auto surface = pointer_target.surface();
            if (surface && surface->good()) {
                g_pSeatManager->recordAgentPointerButtonSerial(agent_seat, surface->client(), event_serial, surface, value);
                if (agent_serial_expires == Clock::time_point{})
                    agent_serial_expires = Clock::now() + std::chrono::seconds(2);
                if (std::ranges::find_if(agent_serial_surfaces, [&](const auto& weak) { return weak.lock() == surface; }) ==
                    agent_serial_surfaces.end()) agent_serial_surfaces.emplace_back(surface);
            }
        }
        for (auto& p : pointers) {
            if (p->dead || !p->wl->resource() || !p->focus) continue;
            p->wl->sendButton(event_serial, event_ms(), value,
                pressed ? WL_POINTER_BUTTON_STATE_PRESSED : WL_POINTER_BUTTON_STATE_RELEASED);
            if (p->wl->version() >= 5) p->wl->sendFrame();
        }
        held_button = pressed ? value : 0;
    }
    bool keyboard_enter(Client& c) {
        const auto root = c.surface.lock();
        if (!root || !keyboard_state || !agent_seat) return false;
        auto surface = root;
        if (pointer_target.window() == c.window.lock()) {
            const auto hovered = pointer_target.surface();
            if (hovered && hovered->good() && hovered->m_mapped && hovered->client() == root->client())
                surface = hovered;
        }
        if (const auto preedit = preedit_surface.lock(); preedit && preedit != surface)
            leave_keyboard();
        unsigned count = 0;
        for (auto& k : keyboards) {
            if (k->dead || !k->wl->resource() || k->wl->client() != root->client()) continue;
            if (!count) selection.clearBeforeFocus(agent_seat, surface);
            if (k->focus != surface) {
                if (const auto old = k->focus.lock(); old && old->good()) k->wl->sendLeave(serial(), old->getResource().get());
                k->focus = surface;
                wl_array keys{};
                k->wl->sendEnter(serial(), surface->getResource().get(), &keys);
                k->wl->sendModifiers(serial(), 0, 0, 0, 0);
            }
            ++count;
        }
        if (count) g_pSeatManager->setSeatKeyboardFocus(agent_seat, surface);
        return count > 0;
    }
    void key(std::uint32_t code, bool pressed) {
        xkb_state_update_key(keyboard_state, code + 8, pressed ? XKB_KEY_DOWN : XKB_KEY_UP);
        if (pressed) held_keys.push_back(code); else std::erase(held_keys, code);
        for (auto& k : keyboards) {
            if (k->dead || !k->wl->resource()) continue;
            const auto surface = k->focus.lock();
            if (!surface || !surface->good() || !agent_seat ||
                g_pSeatManager->keyboardFocusForSeat(agent_seat) != surface ||
                k->wl->client() != surface->client()) continue;
            const auto event_serial = serial();
            if (pressed) g_pSeatManager->recordAgentInputSerial(agent_seat, surface->client(), event_serial);
            k->wl->sendKey(event_serial, event_ms(), code, pressed ? WL_KEYBOARD_KEY_STATE_PRESSED : WL_KEYBOARD_KEY_STATE_RELEASED);
            k->wl->sendModifiers(serial(), xkb_state_serialize_mods(keyboard_state, XKB_STATE_MODS_DEPRESSED),
                xkb_state_serialize_mods(keyboard_state, XKB_STATE_MODS_LATCHED),
                xkb_state_serialize_mods(keyboard_state, XKB_STATE_MODS_LOCKED),
                xkb_state_serialize_layout(keyboard_state, XKB_STATE_LAYOUT_EFFECTIVE));
        }
    }
    bool press_modifiers(Client& c, std::uint64_t mask) {
        if (mask > 15 || (mask && !keyboard_enter(c))) return false;
        constexpr std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
        for (unsigned i = 0; i < keys.size(); ++i)
            if (mask & (1u << i)) key(keys[i], true);
        return true;
    }
    void release_modifiers(std::uint64_t mask) {
        constexpr std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
        for (int i = 3; i >= 0; --i)
            if (mask & (1u << i)) key(keys[i], false);
    }
    void request(Client& c, const std::vector<std::string>& f) {
        if (!foreground_started) c.foreground_attempted = false;
        sync_keymap();
        if (c.dead) return;
        const auto& command = f[0];
        if (command == "HELLO") {
            if (f.size() != 1 || c.hello) { send(c, refusal("invalid_request")); return; }
            c.hello = true;
            if (kProduction)
                send(c, std::format(R"({{"ok":true,"protocol":3,"epoch":"{}","foreground_target":true,"keyboard_layout_independent":true,"foreground_numlock_compatible":true}})", epoch));
            else
                send(c, std::format(R"({{"ok":true,"protocol":0,"epoch":"{}","challenge":"{}"}})", epoch, c.challenge));
            return;
        }
        if (kProduction && command == "INSPECT_XWAYLAND") {
            if (!c.hello || f.size() != 3) { send(c, refusal("invalid_request")); return; }
            const auto pid = number(f[1]), address = number(f[2], 16);
            for (const auto& window : Desktop::windowState()->windows()) {
                if (reinterpret_cast<std::uintptr_t>(window.get()) != address ||
                    static_cast<std::uint64_t>(window->getPID()) != pid) continue;
                const auto xsurface = window->m_xwaylandSurface.lock();
                if (!window->m_isX11 || !window->m_isMapped || window->isHidden() ||
                    !xsurface || !xsurface->m_mapped || !xsurface->m_xID ||
                    xsurface->m_pid != static_cast<pid_t>(pid)) break;
                send(c, std::format(R"({{"ok":true,"pid":{},"address":"{:x}","xwayland_xid":{}}})",
                    pid, address, xsurface->m_xID));
                return;
            }
            send(c, refusal("stale_target"));
            return;
        }
        if (kProduction && command == "FRAME_INVENTORY_BEGIN") {
            if (!c.hello || f.size() != 1) { send(c, refusal("invalid_request")); return; }
            if (!available() || Desktop::viewTreeRevision == std::numeric_limits<std::uint64_t>::max()) {
                send(c, refusal("session_unavailable")); return;
            }
            const auto revision = Desktop::viewTreeRevision;
            auto rows = scene_inventory();
            if (!rows) { send(c, refusal("inventory_overflow")); return; }
            if (revision != Desktop::viewTreeRevision) { send(c, refusal("stale_frame")); return; }
            c.inventory = std::move(*rows);
            c.inventory_token = nonce();
            c.inventory_tree_revision = revision;
            send(c, std::format(R"({{"ok":true,"epoch":"{}","tree_revision":{},"inventory":"{}","count":{}}})",
                scene_epoch, revision, c.inventory_token, c.inventory.size()));
            return;
        }
        if (kProduction && command == "FRAME_INVENTORY_PAGE") {
            if (!c.hello || f.size() != 3 || c.inventory_token.empty() || f[1] != c.inventory_token) {
                send(c, refusal("invalid_request")); return;
            }
            if (!available() || Desktop::viewTreeRevision != c.inventory_tree_revision) {
                send(c, refusal("stale_frame")); return;
            }
            const auto offset = number(f[2]);
            if (offset > c.inventory.size()) { send(c, refusal("invalid_request")); return; }
            auto result = std::format(R"({{"ok":true,"tree_revision":{},"rows":[)", c.inventory_tree_revision);
            auto next = offset;
            for (; next < c.inventory.size(); ++next) {
                const auto row = row_json(c.inventory[next]);
                if (result.size() + row.size() + 32 > kMaxPacket) break;
                if (next > offset) result.push_back(',');
                result += row;
            }
            if (next == offset && next < c.inventory.size()) { send(c, refusal("inventory_overflow")); return; }
            result += std::format(R"(],"next":{}}})", next);
            send(c, result);
            return;
        }
        if (kProduction && command == "INSPECT_FRAME_HIT") {
            if (!c.hello || f.size() != 5 || !available()) { send(c, refusal("invalid_request")); return; }
            const auto revision = Desktop::viewTreeRevision;
            const auto row = inspect_frame_row(number(f[1]), number(f[2], 16), real(f[3]), real(f[4]));
            if (!row || revision != Desktop::viewTreeRevision) { send(c, refusal("stale_frame")); return; }
            send(c, std::format(R"({{"ok":true,"epoch":"{}","tree_revision":{},"hit":{}}})",
                scene_epoch, revision, row_json(*row)));
            return;
        }
        if (kProduction && command == "INSPECT_WINDOW_HIT") {
            if (!c.hello || f.size() != 5 || !available()) { send(c, refusal("invalid_request")); return; }
            const auto revision = Desktop::viewTreeRevision;
            const auto rows = inspect_window_hit(number(f[1]), number(f[2], 16), real(f[3]), real(f[4]));
            if (!rows || revision != Desktop::viewTreeRevision) { send(c, refusal("stale_frame")); return; }
            std::string xid = "null";
            for (const auto& window : Desktop::windowState()->windows()) {
                if (!window || reinterpret_cast<std::uintptr_t>(window.get()) != rows->first.address ||
                    static_cast<std::uint64_t>(window->getPID()) != rows->first.pid || !window->m_isX11) continue;
                const auto surface = window->m_xwaylandSurface.lock();
                if (!surface || !surface->m_mapped || !surface->m_xID ||
                    surface->m_pid != window->getPID()) { send(c, refusal("stale_frame")); return; }
                xid = std::to_string(surface->m_xID);
                break;
            }
            if (revision != Desktop::viewTreeRevision) { send(c, refusal("stale_frame")); return; }
            send(c, std::format(R"({{"ok":true,"epoch":"{}","tree_revision":{},"root":{},"hit":{},"xid":{}}})",
                scene_epoch, revision, row_json(rows->first), row_json(rows->second), xid));
            return;
        }
        if (kProduction && command == "INSPECT_XWAYLAND_CAPTURE") {
            if (!c.hello || f.size() != 3 || !available()) { send(c, refusal("invalid_request")); return; }
            const auto revision = Desktop::viewTreeRevision;
            const auto pid = number(f[1]), address = number(f[2], 16);
            for (const auto& window : Desktop::windowState()->windows()) {
                if (!window || reinterpret_cast<std::uintptr_t>(window.get()) != address ||
                    static_cast<std::uint64_t>(window->getPID()) != pid || !window->m_isX11 ||
                    !window->m_isMapped || window->isHidden() || !window->resource()) continue;
                const auto x = window->m_xwaylandSurface.lock();
                const auto row = describe_surface(window, window->resource(), false, window->surfaceLogicalBox());
                if (!x || !x->m_mapped || !x->m_xID || x->m_pid != window->getPID() ||
                    !row || revision != Desktop::viewTreeRevision) break;
                send(c, std::format(R"({{"ok":true,"epoch":"{}","tree_revision":{},"xid":{},"root":{}}})",
                    scene_epoch, revision, x->m_xID, row_json(*row)));
                return;
            }
            send(c, refusal("stale_frame")); return;
        }
        if (!c.hello) { send(c, refusal("invalid_request")); return; }
        if (command == "CLAIM" && f.size() == 1) {
            if (reservation && reservation != &c) { send(c, refusal("lane_busy")); return; }
            reservation = &c;
            send(c, std::format(R"({{"ok":true,"lane":{}}})", lane)); return;
        }
        if (command == "STOP" && f.size() == 1) {
            if (kProduction) {
                if (reservation != &c) { send(c, refusal("lane_not_claimed")); return; }
                cancel_authority("stopped");
            } else {
                for (auto* peer : peers) if (peer) peer->cancel_authority("stopped");
            }
            send(c, R"({"ok":true})"); return;
        }
        if (command == "CANCEL" && f.size() == 1) {
            if (kProduction && reservation != &c) { send(c, refusal("lane_not_claimed")); return; }
            cancel_authority("cancelled"); send(c, R"({"ok":true})"); return;
        }
        if (trace && ((command == "TRACE_START" || command == "TRACE_STOP") && f.size() == 1)) {
            send(c, trace->request(command)); return;
        }
        if (trace && command == "TRACE_READ" && f.size() == 2) {
            const auto after = number(f[1]);
            if (after > 32768) { send(c, refusal("invalid_request")); return; }
            send(c, trace->request(command, after)); return;
        }
#ifdef CUA_HYPRLAND_TEST_INPUT
        if (command == "APPROVE") { approve(c, f); return; }
#endif
        if (reservation != &c) { send(c, refusal("lane_not_claimed")); return; }
        if (command == "TARGET" || (kProduction && (command == "FOREGROUND_TARGET" ||
            command == "FRAME_TARGET" || command == "WINDOW_CAPTURE_TARGET" ||
            command == "SEMANTIC_TARGET" || command == "SELECTION_TARGET" ||
            command == "DND_TARGET" || command == "FOREGROUND_DND_TARGET" ||
            command == "DND_CAPTURE_TARGET" || command == "FOREGROUND_DND_CAPTURE_TARGET" ||
            command == "XWAYLAND_FOREGROUND_TARGET" || command == "XWAYLAND_CAPTURE_FOREGROUND_TARGET"))) {
            if (drag) { send(c, refusal("invalid_request")); return; }
            const bool frame = command == "FRAME_TARGET";
            const bool window_capture = command == "WINDOW_CAPTURE_TARGET";
            const bool xwayland_capture = command == "XWAYLAND_CAPTURE_FOREGROUND_TARGET";
            const bool xwayland = command == "XWAYLAND_FOREGROUND_TARGET";
            const bool native_dnd = command == "DND_TARGET" || command == "DND_CAPTURE_TARGET";
            const bool foreground_dnd = command == "FOREGROUND_DND_TARGET" ||
                command == "FOREGROUND_DND_CAPTURE_TARGET";
            const bool dual_capture = command == "DND_CAPTURE_TARGET" || command == "FOREGROUND_DND_CAPTURE_TARGET";
            const bool dual_target = native_dnd || foreground_dnd;
            const auto route = command == "FOREGROUND_TARGET" || xwayland || xwayland_capture || foreground_dnd ?
                InputRoute::primary_foreground : InputRoute::independent;
            if (!bind_input_route(c.route, route)) { invalidate(c); send(c, refusal("route_mismatch")); return; }
            if (f.size() != (dual_capture ? 46u : window_capture ? 23u : xwayland_capture ? 17u : frame ? 15u : dual_target ? 6u : xwayland ? 5u : kProduction ? 4u : 3u)) {
                if (kProduction) invalidate(c);
                send(c, refusal("invalid_request")); return;
            }
            // A new admission attempt always retires any unused old grant.
            // Do not leave an unchanged surface merely to refresh authority.
            if (kProduction) retire_grant();
            const auto requested_cap = kProduction ? number(f[dual_target ? 5 : 3]) : 0;
            if (kProduction && (!InputGrant::single_operation(requested_cap) ||
                (requested_cap == 16 && route != InputRoute::primary_foreground) ||
                ((requested_cap == 32 || requested_cap == 128 || requested_cap == 256) &&
                    route != InputRoute::independent) ||
                (requested_cap == 64 && !dual_target) ||
                ((xwayland || xwayland_capture) && (requested_cap == 16 || requested_cap > 8)) ||
                (dual_target && requested_cap != 64) ||
                (window_capture && requested_cap != 1 && requested_cap != 4 &&
                    requested_cap != 8 && requested_cap != 2 && requested_cap != 32 &&
                    requested_cap != 128 && requested_cap != 256) ||
                (xwayland_capture && requested_cap != 1 && requested_cap != 4 && requested_cap != 8) ||
                (command == "SEMANTIC_TARGET" && requested_cap != 256) ||
                (command == "SELECTION_TARGET" && requested_cap != 128))) {
                invalidate(c); send(c, refusal("unsupported")); return;
            }
            if (kProduction && !available()) { invalidate(c, false); send(c, refusal("session_unavailable")); return; }
            if (!input_layout_qualified(route, requested_cap == 2, layout_qualified())) { invalidate(c, false); send(c, refusal("unsupported_layout")); return; }
            const auto pid = number(f[1]); const auto address = number(f[2], 16);
            PHLWINDOW window;
            for (const auto& w : Desktop::windowState()->windows())
                if (reinterpret_cast<std::uintptr_t>(w.get()) == address && static_cast<std::uint64_t>(w->getPID()) == pid) window = w;
            if (!window || (window->m_isX11 && !xwayland && !xwayland_capture && !foreground_dnd && command != "SEMANTIC_TARGET") ||
                (!window->m_isX11 && (xwayland || xwayland_capture)) || !window->m_isMapped || window->isHidden() || !window->resource()) {
                invalidate(c); send(c, refusal("stale_target")); return;
            }
            const auto xsurface = window->m_isX11 ? window->m_xwaylandSurface.lock() : nullptr;
            const auto xid = xwayland || xwayland_capture ? number(f[4]) : xsurface ? xsurface->m_xID : 0;
            if (window->m_isX11 && (!xsurface || !xsurface->m_mapped || !xid ||
                xid > UINT32_MAX || xsurface->m_xID != xid || xsurface->m_pid != static_cast<pid_t>(pid))) {
                invalidate(c); send(c, refusal("stale_target")); return;
            }
            // Retire the old target before rebinding: even if the new target's
            // geometry validation fails, a reserved lane must not strand it.
            if (pointer_target.entered() && pointer_target.window() != window)
                revoke("stale_target");
            if (c.window != window || c.surface != window->resource() || c.token.empty()) {
                // Rebinding authority (including a new socket owner) may reuse
                // unchanged passive hover. Compare the actual target below.
                invalidate(c, false); c.unmap.reset(); c.destroy.reset();
                c.window = window; c.surface = window->resource(); c.token = nonce(); c.revision = 1;
                c.unmap = window->m_events.unmap.listen([this, &c] { invalidate(c); });
                c.destroy = window->m_events.destroy.listen([this, &c] { invalidate(c); });
            }
            c.xwayland_xid = window->m_isX11 ? static_cast<std::uint32_t>(xid) : 0;
            if (!refresh(c)) { invalidate(c); send(c, refusal("stale_target")); return; }
            c.frame_scope = frame;
            c.window_capture_scope = window_capture || xwayland_capture;
            c.frame_hit.reset();
            if (frame) {
                c.frame_point = {real(f[4]), real(f[5])};
                c.frame_epoch = f[6];
                c.frame_tree_revision = number(f[7]);
                c.frame_surface_id = static_cast<std::uint32_t>(number(f[8]));
                c.frame_lifetime = f[9];
                c.frame_box = {real(f[10]), real(f[11]), real(f[12]), real(f[13])};
                const auto popup = number(f[14]);
                if (popup > 1 || c.frame_surface_id == 0 || c.frame_lifetime.size() != 32 ||
                    c.frame_epoch != scene_epoch || c.frame_tree_revision != Desktop::viewTreeRevision) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
                c.frame_popup = popup == 1;
                const auto captured = inspect_frame_row(pid, address, c.frame_point.x, c.frame_point.y);
                if (!captured || captured->surface_id != c.frame_surface_id ||
                    captured->lifetime != c.frame_lifetime || captured->box != c.frame_box ||
                    captured->popup != c.frame_popup) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
                c.frame_hit = hit(c, c.frame_point.x, c.frame_point.y)->surface;
            }
            if (window_capture) {
                c.frame_epoch = f[6]; c.frame_tree_revision = number(f[7]);
                const auto root_id = number(f[8]);
                c.captured_root_id = root_id <= UINT32_MAX ? static_cast<std::uint32_t>(root_id) : 0;
                c.captured_root_lifetime = f[9];
                for (unsigned i = 0; i < c.captured_root_geometry.size(); ++i)
                    c.captured_root_geometry[i] = real(f[10 + i]);
                const auto hit_id = number(f[16]);
                c.frame_surface_id = hit_id <= UINT32_MAX ? static_cast<std::uint32_t>(hit_id) : 0;
                c.frame_lifetime = f[17];
                c.frame_box = {real(f[18]), real(f[19]), real(f[20]), real(f[21])};
                const auto popup = number(f[22]);
                const bool point_cap = requested_cap == 1 || requested_cap == 4 || requested_cap == 8;
                if (!c.captured_root_id || c.captured_root_lifetime.size() != 32 ||
                    c.frame_epoch != scene_epoch || c.frame_tree_revision != Desktop::viewTreeRevision ||
                    popup != 0 || (point_cap && (!c.frame_surface_id || c.frame_lifetime.size() != 32)) ||
                    (!point_cap && (f[4] != "-" || f[5] != "-" || c.frame_surface_id ||
                        c.frame_lifetime != "-" || c.frame_box != std::array<double, 4>{}))) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
                if (point_cap) {
                    c.frame_point = {real(f[4]), real(f[5])};
                    const auto rows = inspect_window_hit(pid, address, c.frame_point.x, c.frame_point.y);
                    if (!rows || rows->first.surface_id != c.captured_root_id ||
                        rows->first.lifetime != c.captured_root_lifetime ||
                        rows->first.root_geometry != c.captured_root_geometry ||
                        rows->second.surface_id != c.frame_surface_id ||
                        rows->second.lifetime != c.frame_lifetime ||
                        rows->second.box != c.frame_box || rows->second.popup) {
                        invalidate(c); send(c, refusal("stale_frame")); return;
                    }
                    c.frame_hit = hit(c, c.frame_point.x, c.frame_point.y)->surface;
                }
                if (!window_capture_valid(c)) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
            }
            if (xwayland_capture) {
                c.frame_point = {real(f[5]), real(f[6])};
                c.frame_epoch = f[7]; c.frame_tree_revision = number(f[8]);
                const auto root_id = number(f[9]);
                c.captured_root_id = root_id <= UINT32_MAX ? static_cast<std::uint32_t>(root_id) : 0;
                c.captured_root_lifetime = f[10];
                for (unsigned i = 0; i < c.captured_root_geometry.size(); ++i)
                    c.captured_root_geometry[i] = real(f[11 + i]);
                const auto rows = inspect_window_hit(pid, address, c.frame_point.x, c.frame_point.y);
                if (!c.captured_root_id || c.captured_root_lifetime.size() != 32 ||
                    c.frame_epoch != scene_epoch || c.frame_tree_revision != Desktop::viewTreeRevision ||
                    !rows || rows->first.surface_id != c.captured_root_id ||
                    rows->first.lifetime != c.captured_root_lifetime ||
                    rows->first.root_geometry != c.captured_root_geometry ||
                    rows->second.surface_id != c.captured_root_id ||
                    rows->second.lifetime != c.captured_root_lifetime) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
                c.frame_surface_id = c.captured_root_id;
                c.frame_lifetime = c.captured_root_lifetime;
                c.frame_box = rows->second.box;
                c.frame_hit = window->resource();
                if (!window_capture_valid(c)) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
            }
            c.destination_window.reset(); c.destination_surface.reset(); c.destination_revision = 0; c.destination_xid = 0;
            c.source_capture.reset(); c.destination_capture.reset();
            if (dual_target) {
                const auto dst_pid = number(f[3]), dst_address = number(f[4], 16);
                PHLWINDOW dest;
                for (const auto& w : Desktop::windowState()->windows())
                    if (reinterpret_cast<std::uintptr_t>(w.get()) == dst_address &&
                        static_cast<std::uint64_t>(w->getPID()) == dst_pid) dest = w;
                const auto dst_geometry = target_geometry(dest, dest ? dest->resource() : nullptr);
                if (!dest || !dst_geometry || (native_dnd && dest->m_isX11) || !PROTO::data) {
                    invalidate(c); send(c, refusal("stale_target")); return;
                }
                const auto dst_xsurface = dest->m_isX11 ? dest->m_xwaylandSurface.lock() : nullptr;
                if (dest->m_isX11 && (!dst_xsurface || !dst_xsurface->m_mapped || !dst_xsurface->m_xID ||
                    dst_xsurface->m_pid != static_cast<pid_t>(dst_pid))) {
                    invalidate(c); send(c, refusal("stale_target")); return;
                }
                const auto seat = foreground_dnd ? g_pSeatManager->primarySeat() : agent_seat;
                if (!PROTO::data->hasDataDeviceForSeat(c.surface.lock()->client(), seat) ||
                    !PROTO::data->hasDataDeviceForSeat(dest->resource()->client(), seat) ||
                    (foreground_dnd && (!unique_primary_seat(dest->resource()->client()) ||
                        agent_conflict(dest->resource())))) {
                    invalidate(c); send(c, refusal("data_device_unavailable")); return;
                }
                c.destination_window = dest; c.destination_surface = dest->resource();
                c.destination_geometry = *dst_geometry; c.destination_revision = 1;
                c.destination_xid = dst_xsurface ? dst_xsurface->m_xID : 0;
                if (foreground_dnd) c.destination_primary_seat =
                    g_pSeatManager->seatResourceForClient(dest->resource()->client());
            }
            if (dual_capture) {
                c.source_capture = parse_capture_proof(f, 6);
                c.destination_capture = parse_capture_proof(f, 26);
                if (!capture_pair_valid(c) ||
                    (native_dnd && (c.source_capture->xid || c.destination_capture->xid)) ||
                    (foreground_dnd &&
                        (!foreground_endpoint_visible(c.window.lock(), c.geometry,
                            c.source_capture->point.x, c.source_capture->point.y) ||
                         !foreground_endpoint_visible(c.destination_window.lock(), c.destination_geometry,
                            c.destination_capture->point.x, c.destination_capture->point.y)))) {
                    invalidate(c); send(c, refusal("stale_frame")); return;
                }
            }
            if (pointer_target.entered() && (pointer_target.window() != window ||
                pointer_target.geometry() != c.geometry)) revoke("stale_target");
            // Only a valid fresh TARGET can retire an orphan on another lane.
            // Dispatch/conflict checks never evict peer state or replay input.
            if (available() && !primary_conflict(c)) retire_orphan_hover(c);
            if (kProduction) {
                if (primary_conflict(c)) { invalidate(c); send(c, refusal("primary_target_busy")); return; }
                if (agent_conflict(c)) { invalidate(c); send(c, refusal("agent_target_busy")); return; }
                if (route == InputRoute::primary_foreground) {
                    const auto failure = foreground_guard(c).activation_failure();
                    if (failure != ForegroundFailureReason::none) {
                        invalidate(c); send(c, refusal("primary_target_busy", ForegroundFailure{failure})); return;
                    }
                }
                if (route == InputRoute::primary_foreground) leave_pointer();
                c.foreground_attempted = false;
                c.token = nonce();
                grant.arm(requested_cap, Clock::now());
                lease = &c; capabilities = requested_cap; expires = grant.deadline();
                if (trace) trace->mark("agent_admitted", lane + 1);
            }
            c.has_bound_target = true;
            if (dual_target) {
                send(c, std::format(R"({{"ok":true,"target":"{}","source_revision":{},"source_width":{},"source_height":{},"dest_revision":{},"dest_width":{},"dest_height":{},"route":"{}"}})",
                    c.token, c.revision, c.geometry[2], c.geometry[3], c.destination_revision,
                    c.destination_geometry[2], c.destination_geometry[3],
                    foreground_dnd ? "primary_foreground" : "native_data_device")); return;
            }
            if (route == InputRoute::primary_foreground) {
                send(c, xwayland_capture ?
                    std::format(R"({{"ok":true,"target":"{}","revision":{},"width":{},"height":{},"scope":"window_capture","hit_surface":{},"route":"primary_foreground"}})",
                        c.token, c.revision, c.geometry[2], c.geometry[3], c.captured_root_id) :
                    std::format(R"({{"ok":true,"target":"{}","revision":{},"width":{},"height":{},"route":"primary_foreground"}})",
                        c.token, c.revision, c.geometry[2], c.geometry[3]));
                return;
            }
            if (frame) {
                send(c, std::format(R"({{"ok":true,"target":"{}","revision":{},"width":{},"height":{},"scope":"desktop_frame","hit_surface":{}}})",
                    c.token, c.revision, c.geometry[2], c.geometry[3], c.frame_hit.lock()->id())); return;
            }
            if (window_capture) {
                send(c, std::format(R"({{"ok":true,"target":"{}","revision":{},"width":{},"height":{},"scope":"window_capture","hit_surface":{}}})",
                    c.token, c.revision, c.geometry[2], c.geometry[3], c.frame_surface_id)); return;
            }
            send(c, std::format(R"({{"ok":true,"target":"{}","revision":{},"width":{},"height":{}}})", c.token, c.revision, c.geometry[2], c.geometry[3])); return;
        }
        if (kProduction && command == "CLEAR_SELECTION") {
            if (f.size() != 1) { send(c, refusal("invalid_request")); return; }
            send(c, std::format(R"({{"ok":true,"cleared":{}}})",
                selection.clearIfOwner(agent_seat, c.connection_id)));
            return;
        }
        const std::uint64_t cap = command == "CLICK" || (kProduction && (command == "MOD_CLICK" || command == "HOVER")) ? 1 :
            command == "KEY" ? 2 : command == "SCROLL" || (kProduction && command == "SCROLL_PRECISE") ? 4 :
            command == "DRAG" || (kProduction && command == "PATH_DRAG") ? 8 :
            command == "ACTIVATE" && kProduction && c.route == InputRoute::primary_foreground ? 16 :
            command == "TEXT" && kProduction ? 32 :
            kProduction && (command == "TEXT_FOCUS" || command == "TEXT_STATUS") &&
                (capabilities == 32 || capabilities == 256) ? capabilities :
            kProduction && (command == "DND" || command == "FOREGROUND_DND") ? 64 :
            kProduction && command == "SET_SELECTION" ? 128 :
            kProduction && (command == "TEXT_COMMIT" || command == "TEXT_PREEDIT" || command == "SEMANTIC_DONE") ? 256 : 0;
        if (!cap) { send(c, refusal("unsupported")); return; }
        const auto count = command == "CLICK" || command == "SCROLL" || command == "SCROLL_PRECISE" ? 8u :
            command == "MOD_CLICK" ? 9u : command == "HOVER" || command == "KEY" ? 6u :
            command == "ACTIVATE" ? 4u : command == "DRAG" ? 9u :
            command == "DND" || command == "FOREGROUND_DND" ? 10u :
            command == "TEXT" || command == "TEXT_COMMIT" || command == "SET_SELECTION" ? 5u :
            command == "TEXT_PREEDIT" ? 7u :
            command == "TEXT_FOCUS" || command == "TEXT_STATUS" || command == "SEMANTIC_DONE" ? 4u : 0u;
        if (command == "PATH_DRAG") {
            if (f.size() < 11) { send(c, refusal("invalid_request")); return; }
            const auto n = number(f[6]);
            if (n < 2 || n > 16 || f.size() != 7 + 2 * n) { send(c, refusal("invalid_request")); return; }
        } else if (f.size() != count) { send(c, refusal("invalid_request")); return; }
        const auto sequence = number(f[1]);
        if (sequence <= c.sequence) { send(c, refusal("replay")); return; }
        c.sequence = sequence;
        if (c.token.empty() || f[2] != c.token || !refresh(c)) { send(c, refusal("stale_target")); return; }
        if (!capture_pair_valid(c)) { revoke("stale_frame"); send(c, refusal("stale_frame")); return; }
        if (c.window_capture_scope && !window_capture_valid(c)) {
            revoke("stale_frame"); send(c, refusal("stale_frame")); return;
        }
        if (number(f[3]) != c.revision) { if (kProduction) revoke("stale_geometry"); send(c, refusal("stale_geometry")); return; }
        if ((command == "DND" || command == "FOREGROUND_DND") && number(f[4]) != c.destination_revision) {
            revoke("stale_geometry"); send(c, refusal("stale_geometry")); return;
        }
        if (!available()) { revoke("session_unavailable", true); send(c, refusal("session_unavailable")); return; }
        const bool uses_keyboard = cap == 2 || command == "MOD_CLICK" ||
            (command == "PATH_DRAG" && number(f[5]) != 0);
        if (!input_layout_qualified(c.route, uses_keyboard, layout_qualified())) { revoke("unsupported_layout", true); send(c, refusal("unsupported_layout")); return; }
        if (lease && Clock::now() >= expires) revoke("lease_expired");
        if (drag) { send(c, refusal("lease_busy")); return; }
        if (lease != &c || !(capabilities & cap) || (kProduction && !grant.permits(cap, Clock::now()))) {
            if (kProduction) {
                revoke("action_not_admitted");
                send(c, refusal("action_not_admitted"));
            } else {
                send(c, std::format(R"({{"ok":false,"code":"pending_operator_approval","detail":"external test operator approval required","epoch":"{}","challenge":"{}","target":"{}"}})", epoch, c.challenge, c.token));
            }
            return;
        }
        if (primary_conflict(c)) {
            if (kProduction) revoke("primary_target_busy");
            send(c, refusal("primary_target_busy")); return;
        }
        if (agent_conflict(c)) {
            if (kProduction) revoke("agent_target_busy");
            send(c, refusal("agent_target_busy")); return;
        }
        if (command == "TEXT_FOCUS" || command == "TEXT_STATUS") {
            if (c.route != InputRoute::independent || (cap != 32 && cap != 256)) {
                send(c, refusal("unsupported")); return;
            }
            if (command == "TEXT_FOCUS") {
                if (!keyboard_enter(c)) { revoke("client_not_bound"); send(c, refusal("client_not_bound")); return; }
                c.text_focus = g_pSeatManager->keyboardFocusForSeat(agent_seat);
                send(c, R"({"ok":true,"phase":"focus_sent"})");
                return;
            }
            const auto focused = g_pSeatManager->keyboardFocusForSeat(agent_seat);
            if (!focused || focused != c.text_focus.lock() || focused->client() != c.surface.lock()->client()) {
                revoke("text_focus_lost"); send(c, refusal("text_focus_lost")); return;
            }
            send(c, std::format(R"({{"ok":true,"ready":{}}})",
                g_pInputManager->m_relay.agentTextReady(agent_seat, focused)));
            return;
        }
        if (c.window_capture_scope && c.frame_surface_id &&
            ((command == "PATH_DRAG" && (real(f[7]) != c.frame_point.x || real(f[8]) != c.frame_point.y)) ||
            (command != "PATH_DRAG" && (real(f[4]) != c.frame_point.x || real(f[5]) != c.frame_point.y)))) {
            send(c, refusal("stale_frame")); return;
        }
        if (c.source_capture &&
            ((command != "DND" && command != "FOREGROUND_DND") ||
             real(f[5]) != c.source_capture->point.x ||
             real(f[6]) != c.source_capture->point.y ||
             real(f[7]) != c.destination_capture->point.x ||
             real(f[8]) != c.destination_capture->point.y)) {
            send(c, refusal("stale_frame")); return;
        }
        if (command == "FOREGROUND_DND") {
            foreground_transfer_request(c, f, cap);
            return;
        }
        if (command == "DND") {
            const auto src_x = real(f[5]), src_y = real(f[6]);
            const auto dst_x = real(f[7]), dst_y = real(f[8]);
            const auto duration = number(f[9]);
            Client destination_observer;
            destination_observer.window = c.destination_window;
            destination_observer.surface = c.destination_surface;
            destination_observer.geometry = c.destination_geometry;
            const auto source_hit = hit(c, src_x, src_y);
            const auto dest_hit = hit(destination_observer, dst_x, dst_y);
            if (!point(c, src_x, src_y) || !source_hit || !destination_valid(c) ||
                !dest_hit || dst_x < 0 || dst_y < 0 ||
                dst_x >= c.destination_geometry[2] || dst_y >= c.destination_geometry[3] ||
                duration < 100 || duration > 2000 ||
                Clock::now() + std::chrono::milliseconds(duration + 500) >= expires) {
                send(c, refusal("invalid_request")); return;
            }
            if (!consume_grant(c, cap)) return;
            if (!pointer_enter(c, src_x, src_y, true) || pointer_target.surface() != source_hit->surface) {
                const auto code = pointer_refusal;
                revoke(code); send(c, refusal(code)); return;
            }
            if (!capture_pair_valid(c)) {
                revoke("stale_frame"); send(c, refusal("stale_frame")); return;
            }
            const auto press_serial = serial();
            button(272, true, press_serial);
            if (!PROTO::data->beginNativeDrag(agent_seat, source_hit->surface, press_serial, dest_hit->surface)) {
                revoke("data_device_unavailable");
                send(c, R"({"ok":false,"code":"data_device_unavailable","detail":"data_device_unavailable","effect":"partial","press_delivered":true})");
                return;
            }
            drag.emplace(Drag{&c, src_x, src_y, dst_x, dst_y, {}, Clock::now(),
                static_cast<unsigned>(duration), DragGeometry{c.revision}, true, false, false,
                c.destination_window, dest_hit->surface, c.destination_geometry, c.destination_revision});
            send(c, R"({"ok":true,"phase":"started","press_delivered":true})");
            return;
        }
        if (command == "SEMANTIC_DONE") {
            if (!consume_grant(c, cap)) return;
            ++dispatches; complete_action();
            send(c, R"({"ok":true,"effect":"unverifiable","route":"semantic_target_guard"})");
            return;
        }
        if (c.route == InputRoute::primary_foreground) {
            foreground_request(c, f, cap);
            return;
        }
        if (command == "TEXT" || command == "TEXT_COMMIT" || command == "TEXT_PREEDIT") {
            const bool preedit = command == "TEXT_PREEDIT";
            const auto text = text_from_hex(f[4], preedit);
            const auto begin = preedit ? preedit_cursor(f[5]) : -1;
            const auto end = preedit ? preedit_cursor(f[6]) : -1;
            if (!consume_grant(c, cap)) return;
            if (!keyboard_enter(c)) { revoke("client_not_bound"); send(c, refusal("client_not_bound")); return; }
            const auto target = g_pSeatManager->keyboardFocusForSeat(agent_seat);
            const auto result = g_pInputManager->m_relay.agentText(agent_seat, target, text, preedit, begin, end);
            using TextResult = CInputMethodRelay::EAgentTextResult;
            if (result != TextResult::DELIVERED) {
                const auto code = result == TextResult::NO_FOCUS ? "text_focus_lost" :
                    result == TextResult::NO_TEXT_INPUT ? "text_input_unavailable" :
                    result == TextResult::AMBIGUOUS_TEXT_INPUT ? "text_input_ambiguous" :
                    result == TextResult::UNSUPPORTED_PREEDIT_SELECTION ? "preedit_selection_unsupported" :
                    result == TextResult::UNSUPPORTED_SEAT ? "text_seat_unsupported" : "invalid_request";
                revoke(code); send(c, refusal(code)); return;
            }
            if (preedit && !text.empty()) {
                preedit_surface = target;
                preedit_connection_id = c.connection_id;
                preedit_expires = Clock::now() + InputGrant::lifetime;
                ++dispatches; retire_grant();
            } else {
                preedit_surface.reset();
                preedit_connection_id = 0;
                ++dispatches; complete_action();
            }
            send(c, R"({"ok":true,"effect":"unverifiable","route":"seat_text_input"})");
            return;
        }
        if (command == "SET_SELECTION") {
            const auto text = text_from_hex(f[4]);
            if (!consume_grant(c, cap)) return;
            if (!keyboard_enter(c)) { revoke("client_not_bound"); send(c, refusal("client_not_bound")); return; }
            const auto destination = g_pSeatManager->keyboardFocusForSeat(agent_seat);
            const auto result = selection.set(agent_seat, c.connection_id, destination,
                text, Clock::now() + std::chrono::seconds(60));
            using SelectionStatus = SeatSelection::Status;
            if (result != SelectionStatus::delivered) {
                const auto code = result == SelectionStatus::no_data_device ? "data_device_unavailable" :
                    result == SelectionStatus::no_focus ? "text_focus_lost" : "invalid_request";
                revoke(code); send(c, refusal(code)); return;
            }
            ++dispatches; complete_action();
            send(c, R"({"ok":true,"selection_owned":true,"offered_to_target":true,"expires_in_ms":60000})");
            return;
        }
        if (command == "PATH_DRAG") {
            const auto duration = number(f[4]), mods = number(f[5]), n = number(f[6]);
            if (mods > 15 || duration < 50 || duration > 2000 ||
                Clock::now() + std::chrono::milliseconds(duration + 50) >= expires) {
                send(c, refusal("invalid_request")); return;
            }
            std::vector<Vector2D> points;
            points.reserve(n);
            for (std::size_t i = 0; i < n; ++i) {
                const auto x = real(f[7 + 2 * i]), y = real(f[8 + 2 * i]);
                if (!point(c, x, y)) { send(c, refusal("invalid_request")); return; }
                points.emplace_back(x, y);
            }
            if (!consume_grant(c, cap)) return;
            if (!pointer_enter(c, points.front().x, points.front().y, true) || !press_modifiers(c, mods)) {
                const auto code = pointer_refusal;
                revoke(code); send(c, refusal(code)); return;
            }
            if (trace) trace->mark("agent_drag_start", lane + 1);
            button(272, true);
            drag.emplace(Drag{&c, points.front().x, points.front().y, points.back().x, points.back().y,
                std::move(points), Clock::now(), static_cast<unsigned>(duration), DragGeometry{c.revision}});
            send(c, R"({"ok":true,"phase":"started"})"); return;
        }
        if (command == "KEY") {
            const auto code = number(f[4]); const auto mods = number(f[5]);
            if (code == 0 || code > 247 || mods > 15 || code == 58 || code == 69 || code == 70) { send(c, refusal("unsupported")); return; }
            if (!consume_grant(c, cap)) return;
            if (!keyboard_enter(c)) { send(c, refusal("client_not_bound")); return; }
            const std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
            for (unsigned i = 0; i < 4; ++i) if ((mods & (1u << i)) && keys[i] != code) key(keys[i], true);
            key(code, true); key(code, false);
            for (int i = 3; i >= 0; --i) if ((mods & (1u << i)) && keys[i] != code) key(keys[i], false);
        } else {
            const auto x = real(f[4]), y = real(f[5]);
            if (!point(c, x, y)) { send(c, refusal("invalid_request")); return; }
            if (command == "CLICK" || command == "MOD_CLICK") {
                const auto btn = number(f[6]), clicks = number(f[7]);
                const auto mods = command == "MOD_CLICK" ? number(f[8]) : 0;
                if (btn < 272 || btn > 274 || clicks < 1 || clicks > 2 || mods > 15) {
                    send(c, refusal("invalid_request")); return;
                }
                if (!consume_grant(c, cap)) return;
                if (!pointer_enter(c, x, y, true) || !press_modifiers(c, mods)) {
                    const auto code = pointer_refusal;
                    revoke(code); send(c, refusal(code)); return;
                }
                for (unsigned i = 0; i < clicks; ++i) { button(btn, true); button(btn, false); }
                release_modifiers(mods);
            } else if (command == "HOVER") {
                if (!consume_grant(c, cap)) return;
                if (!pointer_enter(c, x, y)) {
                    const auto code = pointer_refusal;
                    revoke(code); send(c, refusal(code)); return;
                }
            } else if (command == "SCROLL" || command == "SCROLL_PRECISE") {
                double dx = 0, dy = 0;
                if (command == "SCROLL") {
                    const auto axis = number(f[6]);
                    const auto value = real(f[7]);
                    if (axis > 1 || value == 0 || std::abs(value) > 1000) {
                        send(c, refusal("invalid_request")); return;
                    }
                    (axis == 0 ? dy : dx) = value;
                } else {
                    dx = real(f[6]); dy = real(f[7]);
                    if ((dx == 0 && dy == 0) || std::abs(dx) > 1000 || std::abs(dy) > 1000) {
                        send(c, refusal("invalid_request")); return;
                    }
                }
                if (!consume_grant(c, cap)) return;
                if (!pointer_enter(c, x, y)) {
                    const auto code = pointer_refusal;
                    revoke(code); send(c, refusal(code)); return;
                }
                for (auto& p : pointers) {
                    if (p->dead || !p->wl->resource() || !p->focus) continue;
                    if (p->wl->version() >= 5) p->wl->sendAxisSource(command == "SCROLL" ?
                        WL_POINTER_AXIS_SOURCE_WHEEL : WL_POINTER_AXIS_SOURCE_FINGER);
                    if (dy) p->wl->sendAxis(event_ms(), WL_POINTER_AXIS_VERTICAL_SCROLL, wl_fixed_from_double(dy));
                    if (dx) p->wl->sendAxis(event_ms(), WL_POINTER_AXIS_HORIZONTAL_SCROLL, wl_fixed_from_double(dx));
                    if (command == "SCROLL_PRECISE" && p->wl->version() >= 5) {
                        if (dy) p->wl->sendAxisStop(event_ms(), WL_POINTER_AXIS_VERTICAL_SCROLL);
                        if (dx) p->wl->sendAxisStop(event_ms(), WL_POINTER_AXIS_HORIZONTAL_SCROLL);
                    }
                    if (p->wl->version() >= 5) p->wl->sendFrame();
                }
            } else {
                const auto x2 = real(f[6]), y2 = real(f[7]); const auto duration = number(f[8]);
                if (!point(c, x2, y2) || duration < 50 || duration > 2000) { send(c, refusal("invalid_request")); return; }
                if (Clock::now() + std::chrono::milliseconds(duration + 50) >= expires) { send(c, refusal("lease_expired")); return; }
                if (!consume_grant(c, cap)) return;
                if (!pointer_enter(c, x, y, true)) {
                    const auto code = pointer_refusal;
                    revoke(code); send(c, refusal(code)); return;
                }
                if (trace) trace->mark("agent_drag_start", lane + 1);
                button(272, true);
                drag.emplace(Drag{&c, x, y, x2, y2, {}, Clock::now(), static_cast<unsigned>(duration), DragGeometry{c.revision}});
                send(c, R"({"ok":true,"phase":"started"})"); return;
            }
        }
        ++dispatches;
        if (trace) trace->mark("agent_action_end", lane + 1);
        if (kProduction) complete_action();
        send(c, kDelivered);
    }
    void foreground_request(Client& c, const std::vector<std::string>& f, std::uint64_t cap) {
        const auto& command = f[0];
        double x = c.geometry[2] / 2, y = c.geometry[3] / 2, x2 = 0, y2 = 0, value = 0;
        std::uint64_t code = 0, mods = 0, clicks = 0, axis = 0, duration = 0;
        std::vector<Vector2D> path;
        if (command == "PATH_DRAG") {
            duration = number(f[4]); mods = number(f[5]);
            if (mods > 15 || duration < 50 || duration > 2000 ||
                Clock::now() + std::chrono::milliseconds(duration + 50) >= expires) {
                send(c, refusal("invalid_request")); return;
            }
            const auto n = number(f[6]);
            path.reserve(n);
            for (std::size_t i = 0; i < n; ++i) {
                const auto px = real(f[7 + 2 * i]), py = real(f[8 + 2 * i]);
                if (!point(c, px, py) ||
                    c.surface.lock()->at({px + c.geometry[0] - c.geometry[4],
                        py + c.geometry[1] - c.geometry[5]}, true).first != c.surface.lock()) {
                    send(c, refusal("invalid_request")); return;
                }
                path.emplace_back(px, py);
            }
            x = path.front().x; y = path.front().y;
            x2 = path.back().x; y2 = path.back().y;
        } else if (command == "KEY") {
            code = number(f[4]); mods = number(f[5]);
            if (code == 0 || code > 247 || mods > 15 || code == 58 || code == 69 || code == 70) {
                send(c, refusal("unsupported")); return;
            }
        } else if (command != "ACTIVATE") {
            x = real(f[4]); y = real(f[5]);
            if (!point(c, x, y)) { send(c, refusal("invalid_request")); return; }
            if (command == "CLICK" || command == "MOD_CLICK") {
                code = number(f[6]); clicks = number(f[7]);
                mods = command == "MOD_CLICK" ? number(f[8]) : 0;
                if (code < 272 || code > 274 || clicks < 1 || clicks > 2 || mods > 15) {
                    send(c, refusal("invalid_request")); return;
                }
            } else if (command == "SCROLL" || command == "SCROLL_PRECISE") {
                if (command == "SCROLL") {
                    axis = number(f[6]); value = real(f[7]);
                    if (axis > 1 || value == 0 || std::abs(value) > 1000) {
                        send(c, refusal("invalid_request")); return;
                    }
                } else {
                    x2 = real(f[6]); y2 = real(f[7]);
                    if ((x2 == 0 && y2 == 0) || std::abs(x2) > 1000 || std::abs(y2) > 1000) {
                        send(c, refusal("invalid_request")); return;
                    }
                }
            } else if (command == "DRAG") {
                x2 = real(f[6]); y2 = real(f[7]); duration = number(f[8]);
                if (!point(c, x2, y2) || duration < 50 || duration > 2000) { send(c, refusal("invalid_request")); return; }
                if (Clock::now() + std::chrono::milliseconds(duration + 50) >= expires) { send(c, refusal("lease_expired")); return; }
            }
        }
        if (!consume_grant(c, cap)) return;
        start_foreground(c, x, y, command != "KEY" && command != "ACTIVATE", command == "KEY" || mods != 0);
        if (command == "KEY") {
            const std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
            for (unsigned i = 0; i < 4; ++i) if ((mods & (1u << i)) && keys[i] != code) foreground_key(c, keys[i], true);
            foreground_key(c, code, true); foreground_key(c, code, false);
            for (int i = 3; i >= 0; --i) if ((mods & (1u << i)) && keys[i] != code) foreground_key(c, keys[i], false);
        } else if (command == "CLICK" || command == "MOD_CLICK") {
            constexpr std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
            for (unsigned i = 0; i < keys.size(); ++i) if (mods & (1u << i)) foreground_key(c, keys[i], true);
            for (unsigned i = 0; i < clicks; ++i) { foreground_button(c, code, true); foreground_button(c, code, false); }
            for (int i = 3; i >= 0; --i) if (mods & (1u << i)) foreground_key(c, keys[i], false);
        } else if (command == "SCROLL" || command == "SCROLL_PRECISE") {
            require_foreground(c);
            const auto dx = command == "SCROLL_PRECISE" ? x2 : (axis == 1 ? value : 0);
            const auto dy = command == "SCROLL_PRECISE" ? y2 : (axis == 0 ? value : 0);
            for (const auto& weak : foreground_pointers) {
                const auto p = weak.lock(); if (!p || !p->good()) throw ForegroundFailure{ForegroundFailureReason::pointer_resources};
                p->sendAxisSource(command == "SCROLL" ? WL_POINTER_AXIS_SOURCE_WHEEL : WL_POINTER_AXIS_SOURCE_FINGER);
                if (dy) p->sendAxis(event_ms(), WL_POINTER_AXIS_VERTICAL_SCROLL, dy);
                if (dx) p->sendAxis(event_ms(), WL_POINTER_AXIS_HORIZONTAL_SCROLL, dx);
                if (command == "SCROLL_PRECISE") {
                    if (dy) p->sendAxisStop(event_ms(), WL_POINTER_AXIS_VERTICAL_SCROLL);
                    if (dx) p->sendAxisStop(event_ms(), WL_POINTER_AXIS_HORIZONTAL_SCROLL);
                }
                p->sendFrame();
            }
        } else if (command == "DRAG" || command == "PATH_DRAG") {
            constexpr std::array<std::uint32_t, 4> keys{42, 29, 56, 125};
            for (unsigned i = 0; i < keys.size(); ++i) if (mods & (1u << i)) foreground_key(c, keys[i], true);
            foreground_button(c, 272, true);
            drag.emplace(Drag{&c, x, y, x2, y2, std::move(path), Clock::now(),
                static_cast<unsigned>(duration), DragGeometry{c.revision}});
            send(c, R"({"ok":true,"phase":"started"})"); return;
        }
        require_foreground(c);
        ++dispatches;
        complete_action();
        send(c, kForegroundDelivered);
    }
    void foreground_transfer_request(Client& c, const std::vector<std::string>& f, std::uint64_t cap) {
        const auto sx = real(f[5]), sy = real(f[6]), dx = real(f[7]), dy = real(f[8]);
        const auto duration = number(f[9]);
        const auto root = c.surface.lock(), dest = c.destination_surface.lock();
        Client destination_observer;
        destination_observer.window = c.destination_window;
        destination_observer.surface = dest;
        destination_observer.geometry = c.destination_geometry;
        const auto source_hit = hit(c, sx, sy);
        const auto dest_hit = hit(destination_observer, dx, dy);
        if (c.route != InputRoute::primary_foreground || !root || !dest ||
            !destination_valid(c) || !point(c, sx, sy) || !source_hit ||
            (!c.source_capture && source_hit->surface != root) || !dest_hit ||
            (!c.destination_capture && dest_hit->surface != dest) ||
            dx < 0 || dy < 0 || dx >= c.destination_geometry[2] || dy >= c.destination_geometry[3] ||
            duration < 100 || duration > 2000 ||
            Clock::now() + std::chrono::milliseconds(duration + 500) >= expires) {
            send(c, refusal("invalid_request")); return;
        }
        if (g_pInputManager->getClickMode() != CLICKMODE_DEFAULT ||
            (PROTO::inputCapture && PROTO::inputCapture->isCaptured())) {
            send(c, refusal("primary_target_busy")); return;
        }
        if (!consume_grant(c, cap)) return;
        start_foreground(c, sx, sy, true, false);
        if (!capture_pair_valid(c) || foreground_pointer_surface.lock() != source_hit->surface)
            throw ForegroundFailure{ForegroundFailureReason::pointer_target};
        if (!foreground_endpoint_visible(c.window.lock(), c.geometry, sx, sy))
            throw ForegroundFailure{ForegroundFailureReason::pointer_target};
        foreground_data_origin = source_hit->surface;
        foreground_data_pressed = true;
        primary_data_button(true);
        if (!g_pInputManager->hasHeldButtons() || !foreground_transfer_ready(c))
            throw ForegroundFailure{ForegroundFailureReason::pointer_target};
        foreground_data_press_delivered = true;
        drag.emplace(Drag{&c, sx, sy, dx, dy, {}, Clock::now(), static_cast<unsigned>(duration),
            DragGeometry{c.revision}, false, false, false,
            c.destination_window, dest_hit->surface, c.destination_geometry, c.destination_revision});
        send(c, R"({"ok":true,"phase":"started","press_delivered":true})");
    }
    bool consume_grant(Client& c, std::uint64_t capability) {
        if (!kProduction) return true;
        capabilities = 0;
        if (grant.consume(capability, Clock::now())) return true;
        revoke("action_not_admitted");
        send(c, refusal("action_not_admitted"));
        return false;
    }
    static int tick(void* data) {
        auto& self = *static_cast<Impl*>(data);
        try { self.step(); } catch (...) { self.revoke("internal_error"); }
        wl_event_source_timer_update(self.timer, self.retired ? 500 : 16); return 0;
    }
    void finish_native_drag(Client& client) {
        const auto status = PROTO::data->nativeDragStatus(agent_seat);
        if (!status.dropPerformed || !status.finished || status.cancelled) {
            revoke(status.sourceStarted ? "native_drag_cancelled" : "source_drag_not_started");
            return;
        }
        drag.reset(); ++dispatches;
        complete_action();
        send(client, std::format(
            R"({{"ok":true,"effect":"unverifiable","route":"native_data_device","source_started":{},"offered":{},"accepted":{},"receive_requested":{},"drop_performed":{},"finished":{},"chosen_action":{},"accepted_mime":{}}})",
            status.sourceStarted, status.offered, status.accepted, status.receiveRequested,
            status.dropPerformed, status.finished, status.chosenAction, json_text(status.acceptedMime)));
    }
    void guard_targets() {
        if (lease) {
            const bool keyboard_action = (capabilities & 2) != 0 || foreground_needs_keyboard;
            if (lease->dead) revoke("disconnected", true);
            else if (Clock::now() >= expires) revoke("lease_expired");
            else if (!available() || !input_layout_qualified(lease->route, keyboard_action, layout_qualified()))
                revoke("cancelled", true);
            else if (!refresh(*lease) || !window_capture_valid(*lease) || !capture_pair_valid(*lease) ||
                primary_conflict(*lease) || agent_conflict(*lease) ||
                (capabilities == 64 && !destination_valid(*lease)) ||
                (drag && (!drag->geometry.matches(lease->revision) ||
                    (drag->native_data && !destination_valid(*lease))))) revoke("cancelled");
        }
        if (pointer_target.entered()) {
            const auto surface = pointer_target.surface();
            const auto window = pointer_target.window();
            const auto geometry = window ? target_geometry(window, window->resource()) : std::nullopt;
            const bool bound = std::ranges::any_of(pointers, [&](const auto& p) {
                return !p->dead && p->wl->resource() && surface && p->focus == surface;
            });
            // Availability controls admission, not passive pointer presence.
            // Retention never excuses a changed target, binding, or conflict.
            if (!geometry || *geometry != pointer_target.geometry() ||
                !bound || primary_conflict(surface) || agent_conflict(surface)) revoke("cancelled");
        }
    }
    void step() {
        if (!retired) sync_keymap();
        for (auto& c : clients) if (c->deadline.expired(c->hello, Clock::now())) c->dead = true;
        selection.clearExpired(Clock::now());
        if (agent_serial_expires != Clock::time_point{} && Clock::now() >= agent_serial_expires)
            clear_agent_serials();
        if (preedit_connection_id) {
            const auto owner = std::ranges::find_if(clients, [this](const auto& client) {
                return client->connection_id == preedit_connection_id;
            });
            const auto surface = preedit_surface.lock();
            if (owner == clients.end() || (*owner)->dead || !surface ||
                Clock::now() >= preedit_expires || !available() ||
                g_pSeatManager->keyboardFocusForSeat(agent_seat) != surface ||
                primary_conflict(surface) || agent_conflict(surface))
                leave_keyboard();
        }
        guard_targets();
        if (drag) {
            const auto& d = *drag;
            auto* client = d.client;
            const auto elapsed = std::chrono::duration<double, std::milli>(Clock::now() - d.start).count();
            const auto progress = std::clamp(elapsed / d.duration, 0.0, 1.0);
            double x = d.x1 + (d.x2 - d.x1) * progress, y = d.y1 + (d.y2 - d.y1) * progress;
            if (!d.points.empty()) {
                const auto scaled = progress * (d.points.size() - 1);
                const auto segment = std::min(static_cast<std::size_t>(scaled), d.points.size() - 2);
                const auto fraction = scaled - segment;
                x = d.points[segment].x + (d.points[segment + 1].x - d.points[segment].x) * fraction;
                y = d.points[segment].y + (d.points[segment + 1].y - d.points[segment].y) * fraction;
            }
            if (d.native_data) {
                const auto status = PROTO::data->nativeDragStatus(agent_seat);
                if (status.cancelled) { revoke("native_drag_cancelled"); return; }
                if (!status.sourceStarted && !d.threshold_crossed) {
                    // Give the source toolkit an actual threshold-crossing motion
                    // under its implicit pointer grab; the client can start its
                    // wl_data_device drag in a later event-loop iteration.
                    const double dx = d.x1 + 14 < client->geometry[2] ? 14 : -14;
                    const double dy = d.y1 + 14 < client->geometry[3] ? 14 : -14;
                    const double next_x = client->geometry[2] >= 28 ? d.x1 + dx : d.x1;
                    const double next_y = client->geometry[3] >= 28 ? d.y1 + dy : d.y1;
                    if ((next_x == d.x1 && next_y == d.y1) ||
                        !pointer_enter(*client, next_x, next_y)) {
                        revoke("source_drag_not_started"); return;
                    }
                    drag->threshold_crossed = true;
                    return;
                }
                if (status.sourceStarted && !d.dropped) {
                    const auto dest = d.destination_surface.lock();
                    const auto window = d.destination_window.lock();
                    const auto box = surface_box(dest,
                        window && dest == window->resource() ? window->surfaceLogicalBox() : std::nullopt);
                    if (!box) { revoke("native_drag_cancelled"); return; }
                    const Vector2D local{d.x2 + d.destination_geometry[0] - (*box)[0],
                        d.y2 + d.destination_geometry[1] - (*box)[1]};
                    if (!PROTO::data->moveNativeDrag(agent_seat, dest, local, event_ms())) {
                        revoke("native_drag_cancelled"); return;
                    }
                    if (!drag->offered) drag->destination_entered = Clock::now();
                    drag->offered = true;
                }
                if (progress >= 1 && !d.dropped &&
                    (!status.sourceStarted || (d.offered &&
                        Clock::now() >= d.destination_entered + std::chrono::milliseconds(150)))) {
                    if (!status.sourceStarted) { revoke("source_drag_not_started"); return; }
                    if (!PROTO::data->dropNativeDrag(agent_seat)) { revoke("native_drag_cancelled"); return; }
                    drag->dropped = true;
                    button(272, false);
                }
                if (drag && drag->dropped && (status.finished || status.cancelled))
                    finish_native_drag(*client);
                return;
            }
            if (client->route == InputRoute::primary_foreground && d.destination_revision) {
                const auto origin = foreground_data_origin.lock();
                const auto dest = d.destination_surface.lock();
                if (!foreground_transfer_ready(*client) || !origin || !dest) {
                    revoke("foreground_interrupted"); return;
                }
                if (!PROTO::data->primaryDragMatches(origin)) {
                    if (!d.threshold_crossed) {
                        const double dx = d.x1 + 14 < client->geometry[2] ? 14 : -14;
                        const double dy = d.y1 + 14 < client->geometry[3] ? 14 : -14;
                        const double next_x = client->geometry[2] >= 28 ? d.x1 + dx : d.x1;
                        const double next_y = client->geometry[3] >= 28 ? d.y1 + dy : d.y1;
                        const auto target = hit(*client, next_x, next_y);
                        if ((next_x == d.x1 && next_y == d.y1) || !target ||
                            target->surface != origin ||
                            !foreground_endpoint_visible(client->window.lock(), client->geometry, next_x, next_y)) {
                            revoke("source_drag_not_started"); return;
                        }
                        primary_data_motion({next_x + client->geometry[0], next_y + client->geometry[1]});
                        drag->threshold_crossed = true;
                        drag->offered = true;
                        return;
                    }
                    if (progress >= 1) revoke("source_drag_not_started");
                    return;
                }
                if (!d.destination_entered.time_since_epoch().count()) {
                    if (!foreground_endpoint_visible(d.destination_window.lock(), d.destination_geometry, d.x2, d.y2)) {
                        revoke("stale_target"); return;
                    }
                    // One direct hop avoids offers to unauthorized intermediate
                    // windows; the stock primary DnD path creates the real
                    // destination offer and XWM bridge where applicable.
                    primary_data_motion({d.x2 + d.destination_geometry[0], d.y2 + d.destination_geometry[1]});
                    if (g_pSeatManager->m_state.dndPointerFocus != dest) {
                        revoke("foreground_interrupted"); return;
                    }
                    drag->destination_entered = Clock::now();
                    drag->offered = true;
                    return;
                }
                if (g_pSeatManager->m_state.dndPointerFocus != dest ||
                    !foreground_endpoint_visible(d.destination_window.lock(), d.destination_geometry, d.x2, d.y2)) {
                    revoke("foreground_interrupted"); return;
                }
                if (progress >= 1 && Clock::now() >= d.destination_entered + std::chrono::milliseconds(150)) {
                    primary_data_button(false);
                    foreground_data_pressed = false;
                    foreground_data_origin.reset();
                    foreground_data_press_delivered = false;
                    drag.reset(); ++dispatches;
                    complete_action();
                    send(*client, R"({"ok":true,"effect":"unverifiable","route":"primary_foreground_data_device","press_delivered":true,"motion_delivered":true,"release_delivered":true,"source_started":true,"drop_dispatched":true,"accepted":null,"finished":null})");
                }
                return;
            }
            if (client->route == InputRoute::primary_foreground) {
                foreground_motion(*client, x, y);
                if (progress >= 1) {
                    foreground_button(*client, 272, false); drag.reset(); ++dispatches;
                    complete_action(); send(*client, kForegroundDelivered);
                }
            } else if (!pointer_enter(*client, x, y)) revoke("client_not_bound");
            else if (progress >= 1) {
                button(272, false); drag.reset(); ++dispatches;
                if (trace) trace->mark("agent_drag_end", lane + 1);
                if (kProduction) complete_action();
                send(*client, kDelivered);
            }
        }
        for (const auto& client : clients)
            if (client->dead) selection.clearIfOwner(agent_seat, client->connection_id);
        if ((lease && lease->dead) || (reservation && reservation->dead))
            revoke("disconnected", true);
        if (reservation && reservation->dead) reservation = nullptr;
        std::erase_if(clients, [](auto& c) { return c->dead; });
        std::erase_if(pointers, [](auto& p) { return p->dead; });
        std::erase_if(keyboards, [](auto& k) { return k->dead; });
        std::erase_if(touches, [](auto& t) { return t->dead; });
        std::erase_if(seats, [](auto& s) { return s->dead; });
    }
};

// Hyprland 0.56.2 signals run synchronously on the compositor thread. Keep
// their ownership explicit and detach before retiring the seats. No polling
// or aggregate-state comparison substitutes for this transition boundary.
struct InputExperiment::DesktopListeners {
    struct MonitorListeners {
        PHLMONITORREF monitor;
        CHyprSignalListener dpms, mode;
    };
    InputExperiment& owner;
    std::vector<MonitorListeners> monitors;
    CHyprSignalListener lock, unlock, active, layout, added, removed, destroyed;
    CHyprSignalListener keyboard_layout, pointer_focus, keyboard_focus;
    CHyprSignalListener mouse_move, mouse_button, mouse_axis, keyboard_key, touch_down, tablet_tip;

    explicit DesktopListeners(InputExperiment& input) : owner(input) {
        lock = g_pSessionLockManager->m_events.lock.listen([this] { changed(); });
        unlock = g_pSessionLockManager->m_events.unlock.listen([this] { changed(); });
        if (g_pCompositor->m_aqBackend->hasSession())
            active = g_pCompositor->m_aqBackend->session->events.changeActive.listen([this] { changed(); });
        layout = Event::bus()->m_events.monitor.layoutChanged.listen([this] { changed(); });
        if (kProduction) {
            keyboard_layout = Event::bus()->m_events.input.keyboard.layout.listen(
                [this](SP<IKeyboard>, const std::string&) { changed(); });
            pointer_focus = g_pSeatManager->m_events.pointerFocusChange.listen([this] { primary_changed(); });
            keyboard_focus = g_pSeatManager->m_events.keyboardFocusChange.listen([this] { primary_changed(); });
            // These notifications precede physical delivery. Retire our owned
            // press before the user's event can become held on the same resource.
            mouse_move = Event::bus()->m_events.input.mouse.move.listen([this](Vector2D, Event::SCallbackInfo&) { external_input(); });
            mouse_button = Event::bus()->m_events.input.mouse.button.listen([this](IPointer::SButtonEvent, Event::SCallbackInfo&) { external_input(); });
            mouse_axis = Event::bus()->m_events.input.mouse.axis.listen([this](IPointer::SAxisEvent, Event::SCallbackInfo&) { external_input(); });
            keyboard_key = Event::bus()->m_events.input.keyboard.key.listen([this](IKeyboard::SKeyEvent, Event::SCallbackInfo&) { external_input(); });
            touch_down = Event::bus()->m_events.input.touch.down.listen([this](ITouch::SDownEvent, Event::SCallbackInfo&) { external_input(); });
            tablet_tip = Event::bus()->m_events.input.tablet.tip.listen([this](CTablet::STipEvent, Event::SCallbackInfo&) { external_input(); });
        }
        added = Event::bus()->m_events.monitor.preAdded.listen([this](PHLMONITOR monitor) {
            changed(); watch(monitor);
        });
        removed = Event::bus()->m_events.monitor.preRemoved.listen([this](PHLMONITOR) { changed(); });
        destroyed = Event::bus()->m_events.monitor.destroyMon.listen([this](PHLMONITOR monitor) {
            changed();
            std::erase_if(monitors, [&](const auto& entry) { return !entry.monitor || entry.monitor == monitor; });
        });
        for (const auto& monitor : State::monitorState()->allMonitors()) watch(monitor);
    }
    void changed() {
        for (auto& lane : owner.lanes_) lane->desktop_transition();
    }
    void external_input() {
        for (auto& lane : owner.lanes_)
            if (lane->lease && lane->lease->route == InputRoute::primary_foreground &&
                !lane->foreground_activating)
                lane->cancel_authority("foreground_interrupted", false);
    }
    void primary_changed() {
        for (auto& lane : owner.lanes_) {
            if ((lane->lease && lane->primary_conflict(*lane->lease)) ||
                (lane->pointer_target.entered() && lane->primary_conflict(lane->pointer_target.surface())))
                lane->cancel_authority("primary_target_busy", false);
        }
    }
    void watch(PHLMONITOR monitor) {
        if (!monitor || std::ranges::any_of(monitors, [&](const auto& entry) { return entry.monitor == monitor; })) return;
        MonitorListeners listeners;
        listeners.monitor = monitor;
        listeners.dpms = monitor->m_events.dpmsChanged.listen([this] { changed(); });
        listeners.mode = monitor->m_events.modeChanged.listen([this] { changed(); });
        monitors.push_back(std::move(listeners));
    }
};

InputExperiment::InputExperiment(const std::string& directory, void* plugin) {
    SeatLifetime lifetime(directory);
    for (unsigned i = 0; i < lanes_.size(); ++i) lanes_[i] = std::make_unique<Impl>(directory, i);
    for (auto& lane : lanes_) { lane->peers = {lanes_[0].get(), lanes_[1].get()}; lane->start(); }
#if defined(CUA_HYPRLAND_TEST_INPUT) || defined(CUA_HYPRLAND_INPUT_TRACE)
    trace_ = std::make_unique<PrimaryTrace>(plugin, [this](wl_resource* resource) {
        for (unsigned i = 0; i < lanes_.size(); ++i) {
            for (const auto& p : lanes_[i]->pointers) if (p->wl->resource() == resource) return i + 1;
            for (const auto& k : lanes_[i]->keyboards) if (k->wl->resource() == resource) return i + 1;
        }
        return 0u;
    });
    for (auto& lane : lanes_) lane->trace = trace_.get();
#else
    (void)plugin;
#endif
    desktop_listeners_ = std::make_unique<DesktopListeners>(*this);
    lifetime.publish();
}
InputExperiment::~InputExperiment() {
    desktop_listeners_.reset();
    for (auto& lane : lanes_) { lane->retire(); lane->trace = nullptr; lane->peers = {}; }
    trace_.reset();
    // Intentional process-lifetime ownership: callbacks, removed global, and
    // remaining client-owned resources cannot outlive their Impl. The instance
    // marker refuses replacement modules, so this retains at most two lanes.
    for (auto& lane : lanes_) (void)lane.release();
}
void InputExperiment::suspend() {
    for (auto& lane : lanes_) lane->suspend();
}
void InputExperiment::resume() {
    try {
        for (auto& lane : lanes_) lane->resume();
    } catch (...) {
        // Partial transport setup must not leave one admitted lane behind.
        suspend();
        throw;
    }
}
std::string InputExperiment::status_json() const {
    std::string states;
    for (const auto& lane : lanes_) {
        if (!states.empty()) states += ',';
        const bool pointer_focus = std::ranges::any_of(lane->pointers, [](const auto& p) { return !p->dead && bool(p->focus); });
        const bool keyboard_focus = std::ranges::any_of(lane->keyboards, [](const auto& k) { return !k->dead && bool(k->focus); });
        states += std::format(R"({{"lane":{},"epoch":"{}","desktop_generation":{},"reserved":{},"socket_cleanup":"{}","lease_active":{},"seat_resources":{},"pointer_resources":{},"keyboard_resources":{},"dispatches":{},"held_button":{},"held_keys":{},"drag_active":{},"pointer_focus":{},"keyboard_focus":{}}})",
            lane->lane, lane->epoch, lane->desktop_generation, lane->reservation != nullptr, lane->socket_cleanup, lane->lease != nullptr, lane->seats.size(), lane->pointers.size(), lane->keyboards.size(), lane->dispatches,
            lane->held_button, lane->held_keys.size(), lane->drag.has_value(), pointer_focus, keyboard_focus);
    }
    // Aggregate legacy fields remain available to existing test probes.
    return std::format(R"({{"protocol":{},"test_only":{},"seat_lifetime":"compositor","upgrade":"desktop_restart","transport_ready":{},"keyboard_layout_independent":true,"foreground_numlock_compatible":true,"epoch":"{}","lease_active":{},"seat_resources":{},"pointer_resources":{},"keyboard_resources":{},"dispatches":{},"lanes":[{}]}})",
        kProduction ? 3 : 0, !kProduction, !lanes_[0]->suspended && !lanes_[1]->suspended, lanes_[0]->epoch, lanes_[0]->lease != nullptr || lanes_[1]->lease != nullptr,
        lanes_[0]->seats.size() + lanes_[1]->seats.size(), lanes_[0]->pointers.size() + lanes_[1]->pointers.size(),
        lanes_[0]->keyboards.size() + lanes_[1]->keyboards.size(), lanes_[0]->dispatches + lanes_[1]->dispatches, states);
}
} // namespace cua::hyprland
