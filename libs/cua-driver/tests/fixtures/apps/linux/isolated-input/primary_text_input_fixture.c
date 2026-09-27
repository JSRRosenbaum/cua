/* Test-only real Wayland text-input v1/v3 client and separate IME-v2 connection.
 * v1/v3 bind the private compositor's primary seat; agent-v3 binds the named
 * synthetic keyboard and wl_data_device. keyboard-sibling is an independent
 * same-seat client; JSONL records protocol keys, UTF-8 selection FD reads and
 * client-owned paste, not toolkit-filtered events or mock delivery.
 */
#define _GNU_SOURCE
#include <fcntl.h>
#include <errno.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>
#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"
#include "text-input-unstable-v1-client-protocol.h"
#include "text-input-unstable-v3-client-protocol.h"
#include "input-method-unstable-v2-client-protocol.h"

static struct wl_display *ui_display, *ime_display;
static struct wl_compositor *compositor, *ime_compositor;
static struct wl_shm *shm;
static struct xdg_wm_base *xdg;
static struct wl_seat *ui_seat, *ime_seat, *agent_seat, *ime_agent_seat;
static struct wl_keyboard *primary_keyboard, *agent_keyboard;
static struct wl_data_device_manager *data_manager;
static struct wl_data_device *agent_data_device;
static struct wl_data_offer *pending_offer, *selected_offer;
static int pending_mime, selected_mime, offer_fd = -1, paste_pending, control_down;
static char offer_text[1024], editor_text[1024];
static size_t offer_length;
static const char *selection_mime = "text/plain;charset=utf-8";
struct seat_binding { struct wl_seat *seat; char name[64]; };
static struct seat_binding ui_seats[8], ime_seats[8];
static size_t ui_seat_count, ime_seat_count;
static struct zwp_text_input_manager_v1 *manager_v1;
static struct zwp_text_input_manager_v3 *manager_v3;
static struct zwp_input_method_manager_v2 *ime_manager;
static struct zwp_text_input_v1 *text_v1;
static struct zwp_text_input_v3 *text_v3, *agent_text_v3;
static struct zwp_input_method_v2 *ime;
static struct zwp_input_method_v2 *unavailable_ime;
static struct zwp_input_method_keyboard_grab_v2 *unavailable_grab;
static struct zwp_input_popup_surface_v2 *unavailable_popup;
static struct wl_surface *unavailable_popup_surface;
static struct wl_surface *surface;
static struct xdg_surface *xdg_surface;
static struct xdg_toplevel *toplevel;
static struct wl_buffer *buffer;
static FILE *journal;
static int v3, agent_mode, keyboard_only, auto_enable, configured, ime_done, ime_active, unavailable;
static uint32_t v1_serial, v3_commits, client_commits;
static const char *payload = "Cua primary — 漢字";

static void record(const char *source, const char *event, const char *text, uint32_t serial) {
    fprintf(journal, "{\"source\":\"%s\",\"kind\":\"%s\",\"text\":\"%s\",\"serial\":%u}\n",
            source, event, text ? text : "", serial);
    fflush(journal);
}
static int begin_selection_receive(int paste);
static int keyboard_event(const void *implementation, void *data, uint32_t opcode,
                          const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)opcode;
    const char *source = data;
    uint32_t key = 0, state = 0, depressed = 0, latched = 0, locked = 0, group = 0;
    if (!strcmp(message->name, "keymap")) close(args[1].h);
    if (!strcmp(message->name, "key")) {
        key = args[2].u;
        state = args[3].u;
        if (!strcmp(source, "agent-keyboard")) {
            if (key == 29 || key == 97) control_down = state == WL_KEYBOARD_KEY_STATE_PRESSED;
            if (key == 47 && state == WL_KEYBOARD_KEY_STATE_PRESSED && control_down) {
                if (begin_selection_receive(1) <= 0)
                    record("client", "paste-unavailable", NULL, 0);
            }
        }
    }
    if (!strcmp(message->name, "modifiers")) {
        depressed = args[1].u; latched = args[2].u;
        locked = args[3].u; group = args[4].u;
    }
    fprintf(journal, "{\"source\":\"%s\",\"kind\":\"%s\",\"key\":%u,\"state\":%u,"
            "\"depressed\":%u,\"latched\":%u,\"locked\":%u,\"group\":%u}\n",
            source, message->name, key, state, depressed, latched, locked, group);
    fflush(journal);
    return 0;
}
static void bind_keyboard(struct wl_seat *seat, struct wl_keyboard **keyboard, const char *source) {
    *keyboard = wl_seat_get_keyboard(seat);
    wl_proxy_add_dispatcher((struct wl_proxy *)*keyboard, keyboard_event, NULL, (void *)source);
}
static int begin_selection_receive(int paste) {
    if (!selected_offer || !selected_mime || offer_fd >= 0) return 0;
    int pipes[2];
    if (pipe2(pipes, O_CLOEXEC | O_NONBLOCK) < 0) return -1;
    offer_fd = pipes[0];
    offer_length = 0;
    paste_pending = paste;
    wl_data_offer_receive(selected_offer, selection_mime, pipes[1]);
    close(pipes[1]);
    record("client", paste ? "paste-requested" : "selection-receive-requested", NULL, 0);
    return 1;
}
static int offer_event(const void *implementation, void *data, uint32_t opcode,
                       const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)data; (void)opcode;
    if (!strcmp(message->name, "offer") && !strcmp(args[0].s, selection_mime)) {
        pending_mime = 1;
        record("client", "selection-mime", args[0].s, 0);
    }
    return 0;
}
static int device_event(const void *implementation, void *data, uint32_t opcode,
                        const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)data; (void)opcode;
    if (!strcmp(message->name, "data_offer")) {
        pending_offer = (struct wl_data_offer *)args[0].o;
        pending_mime = 0;
        wl_proxy_add_dispatcher((struct wl_proxy *)pending_offer, offer_event, NULL, NULL);
    } else if (!strcmp(message->name, "selection")) {
        selected_offer = (struct wl_data_offer *)args[0].o;
        selected_mime = selected_offer == pending_offer && pending_mime;
        record("client", selected_offer ? "selection-offer" : "selection-cleared",
               selected_mime ? selection_mime : NULL, 0);
    }
    return 0;
}
static int drain_selection_pipe(void) {
    for (;;) {
        ssize_t size = read(offer_fd, offer_text + offer_length,
                            sizeof(offer_text) - 1 - offer_length);
        if (size > 0) {
            offer_length += (size_t)size;
            if (offer_length == sizeof(offer_text) - 1) return -1;
        } else if (size < 0) {
            if (errno == EINTR) continue;
            return errno == EAGAIN ? 0 : -1;
        } else {
            close(offer_fd);
            offer_fd = -1;
            offer_text[offer_length] = '\0';
            record("client", "selection-received", offer_text, 0);
            if (paste_pending) {
                memcpy(editor_text, offer_text, offer_length + 1);
                record("client", "paste", editor_text, 0);
            }
            paste_pending = 0;
            return 0;
        }
    }
}
static void seat_capabilities(void *data, struct wl_seat *seat, uint32_t capabilities) {
    (void)data; (void)seat; (void)capabilities;
}
static void seat_name(void *data, struct wl_seat *seat, const char *name) {
    (void)seat;
    struct seat_binding *binding = data;
    snprintf(binding->name, sizeof(binding->name), "%s", name);
}
static const struct wl_seat_listener seat_events = {seat_capabilities, seat_name};
static struct wl_seat *named_seat(struct seat_binding *seats, size_t count, int agent) {
    for (size_t index = 0; index < count; ++index) {
        if ((!agent && strncmp(seats[index].name, "Cua-Agent", 9))
            || (agent && !strcmp(seats[index].name, "Cua-Agent")))
            return seats[index].seat;
    }
    return NULL;
}

static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    const int client = data == ui_display;
    if (!strcmp(interface, "wl_seat")) {
        struct seat_binding *seats = client ? ui_seats : ime_seats;
        size_t *count = client ? &ui_seat_count : &ime_seat_count;
        if (*count >= 8) return;
        struct seat_binding *binding = &seats[(*count)++];
        binding->seat = wl_registry_bind(registry, name, &wl_seat_interface, version < 5 ? version : 5);
        wl_seat_add_listener(binding->seat, &seat_events, binding);
    } else if (!client && !strcmp(interface, "zwp_input_method_manager_v2")) {
        ime_manager = wl_registry_bind(registry, name, &zwp_input_method_manager_v2_interface, 1);
    } else if (client && !strcmp(interface, "wl_data_device_manager")) {
        data_manager = wl_registry_bind(registry, name, &wl_data_device_manager_interface,
                                        version < 3 ? version : 3);
    } else if (!strcmp(interface, "wl_compositor")) {
        struct wl_compositor **receiver = client ? &compositor : &ime_compositor;
        *receiver = wl_registry_bind(registry, name, &wl_compositor_interface, version < 4 ? version : 4);
    } else if (client && !strcmp(interface, "wl_shm")) {
        shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (client && !strcmp(interface, "xdg_wm_base")) {
        xdg = wl_registry_bind(registry, name, &xdg_wm_base_interface, version < 2 ? version : 2);
    } else if (client && !strcmp(interface, "zwp_text_input_manager_v1")) {
        manager_v1 = wl_registry_bind(registry, name, &zwp_text_input_manager_v1_interface, 1);
    } else if (client && !strcmp(interface, "zwp_text_input_manager_v3")) {
        manager_v3 = wl_registry_bind(registry, name, &zwp_text_input_manager_v3_interface, 1);
    }
}
static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener registry_listener = {global, global_remove};
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial) {
    (void)data; xdg_wm_base_pong(base, serial);
}
static const struct xdg_wm_base_listener shell_events = {ping};
static void configure(void *data, struct xdg_surface *xdg_surface_, uint32_t serial) {
    (void)data; xdg_surface_ack_configure(xdg_surface_, serial); configured = 1;
}
static const struct xdg_surface_listener surface_events = {configure};
static void toplevel_configure(void *data, struct xdg_toplevel *top, int32_t width,
                               int32_t height, struct wl_array *states) {
    (void)data; (void)top; (void)width; (void)height; (void)states;
}
static void close_window(void *data, struct xdg_toplevel *top) {
    (void)data; (void)top; exit(0);
}
static const struct xdg_toplevel_listener toplevel_events = {
    .configure = toplevel_configure,
    .close = close_window,
};

static int text_event(const void *implementation, void *data, uint32_t opcode,
                      const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)opcode;
    const char *source = data ? data : "client";
    if (!strcmp(message->name, "enter")) {
        record(source, "enter", NULL, 0);
        if (agent_mode && !data && auto_enable) {
            zwp_text_input_v3_enable(agent_text_v3);
            zwp_text_input_v3_commit(agent_text_v3);
            record("fixture", "auto-enabled", NULL, 0);
        }
    } else if (!strcmp(message->name, "leave")) record(source, "leave", NULL, 0);
    else if (!strcmp(message->name, "commit_string")) {
        uint32_t serial = v3 ? 0 : args[0].u;
        const char *text = v3 ? args[0].s : args[1].s;
        ++client_commits;
        record(source, "commit-string", text, serial);
    } else if (v3 && !strcmp(message->name, "preedit_string")) {
        record(source, "preedit-string", args[0].s, (uint32_t)args[1].i);
    } else if (v3 && !strcmp(message->name, "done"))
        record(source, "done", NULL, args[0].u);
    return 0;
}
static int ime_event(const void *implementation, void *data, uint32_t opcode,
                     const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)data; (void)opcode; (void)args;
    if (!strcmp(message->name, "activate")) {
        ime_active = 1; record("ime", "activate", NULL, 0);
    } else if (!strcmp(message->name, "deactivate")) {
        ime_active = 0; record("ime", "deactivate", NULL, 0);
    } else if (!strcmp(message->name, "done")) {
        ++ime_done; record("ime", "done", NULL, (uint32_t)ime_done);
    } else if (!strcmp(message->name, "unavailable")) {
        unavailable = 1; record("ime", "unavailable", NULL, 0);
    }
    return 0;
}
static int agent_ime_event(const void *implementation, void *data, uint32_t opcode,
                           const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)data; (void)opcode; (void)args;
    record("agent-ime", message->name, NULL, 0);
    return 0;
}
static int inert_child_event(const void *implementation, void *data, uint32_t opcode,
                             const struct wl_message *message, union wl_argument *args) {
    (void)implementation; (void)opcode;
    if (!strcmp(message->name, "keymap")) close(args[1].h);
    record(data, message->name, NULL, 0);
    return 0;
}

static struct wl_buffer *create_buffer(void) {
    const int width = 400, height = 220;
    const size_t bytes = (size_t)width * height * 4;
    int fd = memfd_create("cua-primary-ime", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, (off_t)bytes)) exit(1);
    uint32_t *pixels = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) exit(1);
    for (size_t index = 0; index < bytes / 4; ++index) pixels[index] = 0xff345678;
    munmap(pixels, bytes);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int32_t)bytes);
    struct wl_buffer *result = wl_shm_pool_create_buffer(pool, 0, width, height,
                                                         width * 4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    return result;
}
static int roundtrips(void) {
    if (wl_display_roundtrip(ui_display) < 0) return -1;
    if (wl_display_roundtrip(ime_display) < 0) return -1;
    if (wl_display_roundtrip(ui_display) < 0) return -1;
    if (wl_display_roundtrip(ime_display) < 0) return -1;
    if (wl_display_roundtrip(ui_display) < 0) return -1;
    return unavailable ? -1 : 0;
}

static int unavailable_pipeline(void) {
    if (unavailable_ime) return -1;
    unavailable_ime = zwp_input_method_manager_v2_get_input_method(ime_manager, ime_agent_seat);
    wl_proxy_add_dispatcher((struct wl_proxy *)unavailable_ime, agent_ime_event, NULL, NULL);
    unavailable_popup_surface = wl_compositor_create_surface(ime_compositor);
    unavailable_grab = zwp_input_method_v2_grab_keyboard(unavailable_ime);
    unavailable_popup =
        zwp_input_method_v2_get_input_popup_surface(unavailable_ime, unavailable_popup_surface);
    wl_proxy_add_dispatcher((struct wl_proxy *)unavailable_grab, inert_child_event, NULL, "agent-grab");
    wl_proxy_add_dispatcher((struct wl_proxy *)unavailable_popup, inert_child_event, NULL, "agent-popup");
    return roundtrips();
}
static int release_unavailable_children(void) {
    if (!unavailable_ime) return -1;
    zwp_input_method_keyboard_grab_v2_release(unavailable_grab);
    zwp_input_popup_surface_v2_destroy(unavailable_popup);
    wl_surface_destroy(unavailable_popup_surface);
    zwp_input_method_v2_destroy(unavailable_ime);
    unavailable_ime = NULL;
    return roundtrips();
}

static int next_command(char *command, size_t length) {
    for (;;) {
        if (wl_display_dispatch_pending(ui_display) < 0
            || wl_display_dispatch_pending(ime_display) < 0) return -1;
        if ((wl_display_flush(ui_display) < 0 && errno != EAGAIN)
            || (wl_display_flush(ime_display) < 0 && errno != EAGAIN)) return -1;
        struct pollfd fds[] = {
            {STDIN_FILENO, POLLIN, 0},
            {wl_display_get_fd(ui_display), POLLIN, 0},
            {wl_display_get_fd(ime_display), POLLIN, 0},
            {offer_fd, POLLIN, 0},
        };
        int ready = poll(fds, offer_fd < 0 ? 3 : 4, 1000);
        if (ready < 0) {
            if (errno == EINTR) continue;
            return -1;
        }
        for (size_t index = 1; index < 3; ++index) {
            if (fds[index].revents & (POLLERR | POLLHUP | POLLNVAL)) return -1;
            if (fds[index].revents & POLLIN) {
                struct wl_display *display = index == 1 ? ui_display : ime_display;
                if (wl_display_dispatch(display) < 0) return -1;
            }
        }
        if (offer_fd >= 0 && (fds[3].revents & (POLLIN | POLLHUP))) {
            if (drain_selection_pipe() < 0) return -1;
        }
        if (!(fds[0].revents & (POLLIN | POLLHUP))) continue;
        if (!fgets(command, (int)length, stdin)) return 0;
        command[strcspn(command, "\n")] = '\0';
        return 1;
    }
}

int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[1], "v1") && strcmp(argv[1], "v3")
        && strcmp(argv[1], "agent-v3") && strcmp(argv[1], "keyboard-sibling"))) return 2;
    keyboard_only = !strcmp(argv[1], "keyboard-sibling");
    agent_mode = keyboard_only || !strcmp(argv[1], "agent-v3");
    v3 = strcmp(argv[1], "v1") != 0;
    journal = fopen(argv[2], "wx");
    if (!journal) return 2;
    ui_display = wl_display_connect(NULL);
    ime_display = wl_display_connect(NULL);
    if (!ui_display || !ime_display) return 1;
    struct wl_registry *ui_registry = wl_display_get_registry(ui_display);
    struct wl_registry *ime_registry = wl_display_get_registry(ime_display);
    wl_registry_add_listener(ui_registry, &registry_listener, ui_display);
    wl_registry_add_listener(ime_registry, &registry_listener, ime_display);
    if (wl_display_roundtrip(ui_display) < 0 || wl_display_roundtrip(ime_display) < 0
        || wl_display_roundtrip(ui_display) < 0 || wl_display_roundtrip(ime_display) < 0)
        return 1;
    ui_seat = named_seat(ui_seats, ui_seat_count, 0);
    ime_seat = named_seat(ime_seats, ime_seat_count, 0);
    agent_seat = named_seat(ui_seats, ui_seat_count, 1);
    ime_agent_seat = named_seat(ime_seats, ime_seat_count, 1);
    if (!compositor || !ime_compositor || !shm || !xdg || !ui_seat || !ime_seat
        || (agent_mode && (!agent_seat || !ime_agent_seat))
        || (!keyboard_only && (!ime_manager || (v3 ? !manager_v3 : !manager_v1)))) return 1;
    if (agent_mode) {
        bind_keyboard(agent_seat, &agent_keyboard, "agent-keyboard");
        if (!keyboard_only) bind_keyboard(ui_seat, &primary_keyboard, "primary-keyboard");
    }
    if (agent_mode && !keyboard_only) {
        if (!data_manager) return 1;
        agent_data_device = wl_data_device_manager_get_data_device(data_manager, agent_seat);
        wl_proxy_add_dispatcher((struct wl_proxy *)agent_data_device, device_event, NULL, NULL);
    }
    if (!keyboard_only) {
        ime = zwp_input_method_manager_v2_get_input_method(ime_manager, ime_seat);
        wl_proxy_add_dispatcher((struct wl_proxy *)ime, ime_event, NULL, NULL);
        if (wl_display_roundtrip(ime_display) < 0 || unavailable) return 1;
        if (v3) {
            text_v3 = zwp_text_input_manager_v3_get_text_input(manager_v3, ui_seat);
            wl_proxy_add_dispatcher((struct wl_proxy *)text_v3, text_event, NULL,
                                    agent_mode ? "primary-client" : NULL);
            if (agent_mode) {
                agent_text_v3 = zwp_text_input_manager_v3_get_text_input(manager_v3, agent_seat);
                wl_proxy_add_dispatcher((struct wl_proxy *)agent_text_v3, text_event, NULL, NULL);
            }
        } else {
            text_v1 = zwp_text_input_manager_v1_create_text_input(manager_v1);
            wl_proxy_add_dispatcher((struct wl_proxy *)text_v1, text_event, NULL, NULL);
        }
    }
    xdg_wm_base_add_listener(xdg, &shell_events, NULL);
    surface = wl_compositor_create_surface(compositor);
    xdg_surface = xdg_wm_base_get_xdg_surface(xdg, surface);
    xdg_surface_add_listener(xdg_surface, &surface_events, NULL);
    toplevel = xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel, &toplevel_events, NULL);
    xdg_toplevel_set_title(toplevel, keyboard_only ? "Cua Agent Keyboard Sibling"
                           : agent_mode ? "Cua Primary Text Input agent-v3"
                           : v3 ? "Cua Primary Text Input v3" : "Cua Primary Text Input v1");
    xdg_toplevel_set_app_id(toplevel, "cua-primary-ime-fixture");
    wl_surface_commit(surface);
    while (!configured && wl_display_dispatch(ui_display) >= 0) {}
    if (!configured) return 1;
    buffer = create_buffer();
    wl_surface_attach(surface, buffer, 0, 0);
    wl_surface_damage_buffer(surface, 0, 0, 400, 220);
    wl_surface_commit(surface);
    if (roundtrips() < 0) return 1;
    record("fixture", "ready", argv[1], 0);

    char command[32];
    for (;;) {
        int next = next_command(command, sizeof(command));
        if (next < 0) return 1;
        if (next == 0) break;
        if (keyboard_only && strcmp(command, "SYNC")
            && strcmp(command, "RELEASE_AGENT_KEYBOARD") && strcmp(command, "UNMAP")) return 2;
        if (!strcmp(command, "ACTIVATE") || !strcmp(command, "REACTIVATE")) {
            if (v3) {
                zwp_text_input_v3_enable(text_v3);
                zwp_text_input_v3_commit(text_v3);
                ++v3_commits;
            } else {
                zwp_text_input_v1_activate(text_v1, ui_seat, surface);
            }
        } else if (!strcmp(command, "COMMIT") || !strcmp(command, "PRIMARY_COMMIT")) {
            if (v3) {
                struct zwp_text_input_v3 *target = agent_mode && !strcmp(command, "COMMIT")
                    ? agent_text_v3 : text_v3;
                zwp_text_input_v3_set_surrounding_text(target, "client", 6, 6);
                zwp_text_input_v3_commit(target);
                if (target == text_v3) ++v3_commits;
            } else {
                zwp_text_input_v1_set_surrounding_text(text_v1, "client", 6, 6);
                zwp_text_input_v1_commit_state(text_v1, ++v1_serial);
            }
        } else if (!strcmp(command, "SEND") || !strcmp(command, "STALE")) {
            const int stale = !strcmp(command, "STALE");
            if (stale && ime_done == 0) return 2;
            zwp_input_method_v2_commit_string(ime, stale ? "STALE" : payload);
            zwp_input_method_v2_commit(ime, stale ? (uint32_t)(ime_done - 1) : (uint32_t)ime_done);
        } else if (!strcmp(command, "DISABLE")) {
            if (v3) {
                struct zwp_text_input_v3 *target = agent_mode ? agent_text_v3 : text_v3;
                zwp_text_input_v3_disable(target);
                zwp_text_input_v3_commit(target);
                if (target == text_v3) ++v3_commits;
            } else zwp_text_input_v1_deactivate(text_v1, ui_seat);
        } else if (agent_mode && !strcmp(command, "ENABLE")) {
            zwp_text_input_v3_enable(agent_text_v3);
            zwp_text_input_v3_commit(agent_text_v3);
        } else if (agent_mode && !strcmp(command, "AUTO_ENABLE")) {
            auto_enable = 1;
        } else if (agent_mode && !strcmp(command, "AUTO_DISABLE")) {
            auto_enable = 0;
        } else if (agent_mode && !strcmp(command, "UNAVAILABLE_PIPELINE")) {
            if (unavailable_pipeline() < 0) return 1;
        } else if (agent_mode && !strcmp(command, "RELEASE_UNAVAILABLE_CHILDREN")) {
            if (release_unavailable_children() < 0) return 1;
        } else if (agent_mode && !strcmp(command, "RELEASE_AGENT_KEYBOARD")) {
            if (!agent_keyboard) return 2;
            wl_keyboard_release(agent_keyboard);
            agent_keyboard = NULL;
            record("fixture", "agent-keyboard-released", NULL, 0);
        } else if (agent_mode && !strcmp(command, "UNMAP")) {
            wl_surface_attach(surface, NULL, 0, 0);
            wl_surface_commit(surface);
            record("fixture", "unmapped", NULL, 0);
        } else if (agent_mode && !keyboard_only && !strcmp(command, "RECEIVE_SELECTION")) {
            if (begin_selection_receive(0) <= 0)
                record("client", "selection-unavailable", NULL, 0);
        } else if (!strcmp(command, "SYNC")) {
            /* Pump both real connections after compositor-driven focus changes. */
        } else return 2;
        if (roundtrips() < 0) return 1;
        record("fixture", "command-complete", command, v3_commits);
    }
    if (unavailable_ime && release_unavailable_children() < 0) return 1;
    if (offer_fd >= 0) close(offer_fd);
    if (agent_data_device) wl_data_device_release(agent_data_device);
    if (pending_offer) wl_data_offer_destroy(pending_offer);
    if (data_manager) wl_data_device_manager_destroy(data_manager);
    if (agent_keyboard) wl_keyboard_release(agent_keyboard);
    if (primary_keyboard) wl_keyboard_release(primary_keyboard);
    if (agent_text_v3) zwp_text_input_v3_destroy(agent_text_v3);
    if (text_v3) zwp_text_input_v3_destroy(text_v3);
    if (text_v1) zwp_text_input_v1_destroy(text_v1);
    if (ime) zwp_input_method_v2_destroy(ime);
    wl_buffer_destroy(buffer);
    xdg_toplevel_destroy(toplevel);
    xdg_surface_destroy(xdg_surface);
    wl_surface_destroy(surface);
    if (ime_manager) zwp_input_method_manager_v2_destroy(ime_manager);
    if (manager_v3) zwp_text_input_manager_v3_destroy(manager_v3);
    if (manager_v1) zwp_text_input_manager_v1_destroy(manager_v1);
    for (size_t index = 0; index < ui_seat_count; ++index) wl_seat_destroy(ui_seats[index].seat);
    for (size_t index = 0; index < ime_seat_count; ++index) wl_seat_destroy(ime_seats[index].seat);
    wl_shm_destroy(shm);
    wl_compositor_destroy(compositor);
    wl_compositor_destroy(ime_compositor);
    xdg_wm_base_destroy(xdg);
    wl_registry_destroy(ui_registry);
    wl_registry_destroy(ime_registry);
    wl_display_disconnect(ui_display);
    wl_display_disconnect(ime_display);
    fclose(journal);
    return 0;
}
