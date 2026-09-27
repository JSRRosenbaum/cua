/* Test-only native wl_subsurface/xdg_toplevel mutation oracle. Build with
 * wayland-scanner's generated xdg-shell client header/code and wayland-client.
 * Each stdin command is one actual compositor-side mutation. The JSONL journal
 * records the fixture's own commands, not the compositor's claimed revision.
 * Commands: MOVE, STACK, REGION, REPAINT. EOF destroys only fixture resources.
 */
#define _GNU_SOURCE
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
#include "viewporter-client-protocol.h"

static struct wl_compositor *compositor;
static struct wl_subcompositor *subcompositor;
static struct wl_shm *shm;
static struct xdg_wm_base *shell;
static struct wp_viewporter *viewporter;
static struct wp_viewport *viewport;
static struct wl_display *display;
static struct wl_surface *root, *upper, *lower;
static struct wl_subsurface *upper_role, *lower_role;
static struct xdg_surface *xdg_surface;
static struct xdg_toplevel *toplevel;
static int configured;
static FILE *journal;

static void global(void *data, struct wl_registry *registry, uint32_t name,
                   const char *interface, uint32_t version) {
    (void)data;
    if (!strcmp(interface, "wl_compositor"))
        compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                      version < 6 ? version : 6);
    else if (!strcmp(interface, "wl_subcompositor"))
        subcompositor = wl_registry_bind(registry, name, &wl_subcompositor_interface, 1);
    else if (!strcmp(interface, "wl_shm"))
        shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    else if (!strcmp(interface, "xdg_wm_base"))
        shell = wl_registry_bind(registry, name, &xdg_wm_base_interface,
                                 version < 2 ? version : 2);
    else if (!strcmp(interface, "wp_viewporter"))
        viewporter = wl_registry_bind(registry, name, &wp_viewporter_interface, 1);
}
static void global_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data; (void)registry; (void)name;
}
static const struct wl_registry_listener globals = {global, global_remove};
static void ping(void *data, struct xdg_wm_base *base, uint32_t serial) {
    (void)data; xdg_wm_base_pong(base, serial);
}
static const struct xdg_wm_base_listener shell_events = {ping};
static void surface_configure(void *data, struct xdg_surface *surface, uint32_t serial) {
    (void)data; xdg_surface_ack_configure(surface, serial); configured = 1;
}
static const struct xdg_surface_listener surface_events = {surface_configure};
static void toplevel_configure(void *data, struct xdg_toplevel *surface, int32_t w, int32_t h,
                               struct wl_array *states) {
    (void)data; (void)surface; (void)w; (void)h; (void)states;
}
static void toplevel_close(void *data, struct xdg_toplevel *surface) {
    (void)data; (void)surface; exit(0);
}
static const struct xdg_toplevel_listener toplevel_events = {
    .configure = toplevel_configure,
    .close = toplevel_close,
};

static struct wl_buffer *solid_buffer(int width, int height, uint32_t color) {
    const size_t bytes = (size_t)width * (size_t)height * 4;
    int fd = memfd_create("cua-scene-fixture", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, (off_t)bytes) != 0) exit(1);
    uint32_t *pixels = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) exit(1);
    for (size_t index = 0; index < bytes / 4; ++index) pixels[index] = color;
    munmap(pixels, bytes);
    struct wl_shm_pool *pool = wl_shm_create_pool(shm, fd, (int32_t)bytes);
    struct wl_buffer *buffer = wl_shm_pool_create_buffer(pool, 0, width, height,
                                                         width * 4, WL_SHM_FORMAT_ARGB8888);
    wl_shm_pool_destroy(pool);
    close(fd);
    return buffer;
}

static void record(const char *event) {
    fprintf(journal, "{\"kind\":\"%s\"}\n", event);
    fflush(journal);
}

int main(int argc, char **argv) {
    if (argc != 2) return 2;
    journal = fopen(argv[1], "wx");
    if (!journal) return 2;
    display = wl_display_connect(NULL);
    if (!display) return 1;
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &globals, NULL);
    if (wl_display_roundtrip(display) < 0 || !compositor || !subcompositor || !shm
        || !shell || !viewporter || wl_proxy_get_version((struct wl_proxy *)compositor) < 6)
        return 1;
    xdg_wm_base_add_listener(shell, &shell_events, NULL);
    root = wl_compositor_create_surface(compositor);
    upper = wl_compositor_create_surface(compositor);
    lower = wl_compositor_create_surface(compositor);
    viewport = wp_viewporter_get_viewport(viewporter, root);
    xdg_surface = xdg_wm_base_get_xdg_surface(shell, root);
    xdg_surface_add_listener(xdg_surface, &surface_events, NULL);
    toplevel = xdg_surface_get_toplevel(xdg_surface);
    xdg_toplevel_add_listener(toplevel, &toplevel_events, NULL);
    xdg_toplevel_set_title(toplevel, "Cua Scene Revision Fixture");
    xdg_toplevel_set_app_id(toplevel, "cua-scene-revision-fixture");
    upper_role = wl_subcompositor_get_subsurface(subcompositor, upper, root);
    lower_role = wl_subcompositor_get_subsurface(subcompositor, lower, root);
    wl_subsurface_set_position(upper_role, 20, 30);
    wl_subsurface_set_position(lower_role, 70, 60);
    wl_surface_commit(root);
    while (!configured && wl_display_dispatch(display) >= 0) {}
    if (!configured) return 1;
    struct wl_buffer *background = solid_buffer(400, 280, 0xff223344);
    struct wl_buffer *red = solid_buffer(120, 100, 0xffcc3030);
    struct wl_buffer *blue = solid_buffer(120, 100, 0xff3044cc);
    wl_surface_attach(upper, red, 0, 0);
    wl_surface_damage_buffer(upper, 0, 0, 120, 100);
    wl_surface_commit(upper);
    wl_surface_attach(lower, blue, 0, 0);
    wl_surface_damage_buffer(lower, 0, 0, 120, 100);
    wl_surface_commit(lower);
    wl_surface_attach(root, background, 0, 0);
    wl_surface_damage_buffer(root, 0, 0, 400, 280);
    wl_surface_commit(root);
    if (wl_display_roundtrip(display) < 0) return 1;
    record("ready");

    char command[32];
    int moved = 0, stacked = 0, region_changed = 0;
    for (;;) {
        if (wl_display_dispatch_pending(display) < 0) return 1;
        if (wl_display_flush(display) < 0 && errno != EAGAIN) return 1;
        struct pollfd fds[] = {{STDIN_FILENO, POLLIN, 0},
                               {wl_display_get_fd(display), POLLIN, 0}};
        int ready = poll(fds, 2, 1000);
        if (ready < 0) {
            if (errno == EINTR) continue;
            return 1;
        }
        if (fds[1].revents & (POLLERR | POLLHUP | POLLNVAL)) return 1;
        if (fds[1].revents & POLLIN && wl_display_dispatch(display) < 0) return 1;
        if (!(fds[0].revents & (POLLIN | POLLHUP))) continue;
        if (!fgets(command, sizeof(command), stdin)) break;
        const char *committed = NULL;
        if (!strcmp(command, "MOVE\n")) {
            wl_subsurface_set_position(upper_role, moved ? 20 : 45, moved ? 30 : 55);
            moved = !moved;
            wl_surface_commit(root);
            committed = "move";
        } else if (!strcmp(command, "STACK\n")) {
            if (stacked) wl_subsurface_place_above(lower_role, upper);
            else wl_subsurface_place_below(lower_role, upper);
            stacked = !stacked;
            wl_surface_commit(root);
            committed = "stack";
        } else if (!strcmp(command, "REGION\n")) {
            struct wl_region *region = wl_compositor_create_region(compositor);
            wl_region_add(region, 0, 0, region_changed ? 120 : 40, region_changed ? 100 : 40);
            wl_surface_set_input_region(upper, region);
            wl_region_destroy(region);
            region_changed = !region_changed;
            wl_surface_commit(upper);
            wl_surface_commit(root);
            committed = "input-region";
        } else if (!strcmp(command, "REASSERT\n")) {
            if (!region_changed) return 2;
            struct wl_region *same = wl_compositor_create_region(compositor);
            wl_region_add(same, 0, 0, 40, 40);
            wl_surface_set_input_region(upper, same);
            wl_region_destroy(same);
            wl_surface_commit(upper);
            wl_surface_set_buffer_scale(root, 1);
            wl_surface_set_buffer_transform(root, WL_OUTPUT_TRANSFORM_NORMAL);
            wl_surface_offset(root, 0, 0);
            wp_viewport_set_source(viewport, wl_fixed_from_int(-1), wl_fixed_from_int(-1),
                                   wl_fixed_from_int(-1), wl_fixed_from_int(-1));
            wp_viewport_set_destination(viewport, -1, -1);
            wl_surface_damage_buffer(root, 0, 0, 400, 280);
            wl_surface_commit(root);
            committed = "reassert-unchanged";
        } else if (!strcmp(command, "VIEWPORT\n")) {
            wp_viewport_set_source(viewport, wl_fixed_from_int(0), wl_fixed_from_int(0),
                                   wl_fixed_from_int(300), wl_fixed_from_int(200));
            wp_viewport_set_destination(viewport, 400, 280);
            wl_surface_commit(root);
            committed = "viewport-changed";
        } else if (!strcmp(command, "VIEWPORT_RESET\n")) {
            wp_viewport_set_source(viewport, wl_fixed_from_int(-1), wl_fixed_from_int(-1),
                                   wl_fixed_from_int(-1), wl_fixed_from_int(-1));
            wp_viewport_set_destination(viewport, -1, -1);
            wl_surface_commit(root);
            committed = "viewport-restored";
            wl_surface_damage_buffer(root, 0, 0, 400, 280);
            wl_surface_commit(root);
            committed = "pixel-only-repaint";
        } else {
            record("invalid-command");
            return 2;
        }
        if (wl_display_roundtrip(display) < 0) return 1;
        record(committed);
    }
    wl_subsurface_destroy(upper_role);
    wl_subsurface_destroy(lower_role);
    wl_surface_destroy(upper);
    wl_surface_destroy(lower);
    xdg_toplevel_destroy(toplevel);
    xdg_surface_destroy(xdg_surface);
    wp_viewport_destroy(viewport);
    wp_viewporter_destroy(viewporter);
    wl_surface_destroy(root);
    wl_buffer_destroy(background);
    wl_buffer_destroy(red);
    wl_buffer_destroy(blue);
    xdg_wm_base_destroy(shell);
    wl_shm_destroy(shm);
    wl_subcompositor_destroy(subcompositor);
    wl_compositor_destroy(compositor);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    fclose(journal);
    return 0;
}
