# Omarchy system-wide Cua development requirements

User-approved development, 2026-09-26. Local fork branch `omarchy-system-wide` begins at Cua Driver tag `cua-driver-rs-v0.29.1`, commit `7a8f66ad04e62fccb18cca9965f2964fcaee124e`. Upstream is https://github.com/trycua/cua. No public remote or publication is implied by this local fork. Preserve upstream and adapted PR authorship.

Detailed approved research: /home/joseph/cua-system-wide-fork-plan-2026-09-26.md. Current host integration/evidence: /home/joseph/cua-virtual-desktop-research-2026-09-26.md.

Additional necessary source boundary established during implementation: `/home/joseph/Projects/hyprland-cua`, branch `cua-seat-scoped-input`, starts at Hyprland `v0.56.2`, commit `efb50993780079460b0cbed1363e2166a2de1d9f`. Its current global IME relay and data-device drag state cannot provide safe independent seat-bound operations through the existing plugin API. CompositorText and CompositorData own minimal explicit seat-scoped API changes there; no private-member access hacks or temporary swapping of primary globals. This produces a distinct candidate compositor/module ABI and must be built and exercised only in the disposable runtime before any separate production activation decision.

## Delivery contract

The goal is broad native desktop application control with isolated background input where supported, explicit foreground takeover where necessary, and an explicitly owned isolated session for incompatible clients. These modes must never be conflated. Existing windows must not be silently replaced with newly launched instances. No promise of universal background isolation follows from a second cursor or removal of an application gate.

Implement real behavior, not stubs, mock success, arbitrary routing changes, or unimplemented capability advertisements. Surface runtime/host limitations as precise evidence-backed prerequisites. Preserve the working desktop throughout development; never unload/replace its mapped Cua plugin. Changed plugin activation requires a fresh disposable compositor first and a fresh production login when approved. No physical monitor, keyboard layout, desktop channel, or user's application/document changes as a shortcut.

## Requirements and acceptance

R1. Matched source/package provenance: build Driver and plugin from explicit revisions, record protocol and exact Hyprland/header/compiler/runtime identities, keep ABI guards, preserve a matching rollback set. Official host profile Hyprland0.56.2-3/Aquamarine0.15.1-1/GCC16.2.1. No automatic live module replacement.

R2. Compatibility admission: an explicitly selected general-native mode may admit unqualified native apps without falsifying certification or weakening authorization. Default certified mode remains. Preserve process identity, native surface identity/lifetime, geometry revision, ownership, primary-client conflicts, lane conflicts, lock/DPMS restrictions, cancellation and no-replay behavior. Do not enable unrestricted permission mode or impersonate older packages. Review upstream #4230's independent compatibility change; retain attribution, not wholesale four-lane changes unless actually needed.

R3. Public-state correctness: get_agent_cursor_state must return schema-valid idle/unpositioned and positioned state. launch_app must truthfully report observed activation and prevent focus theft through legitimate compositor-supported launch handling where possible; restoration after theft is not proof of background launch. Preserve cross-platform public schema consistency.

R4. Coordinate contract: native logical output geometry, screenshot pixels, cropping/downsampling and surface-local input have deterministic mappings. Support multiple offset outputs (including negative origins), scale/rotation, gaps, DPMS/hotplug and stale geometry. Never change monitor layout to make a test pass. Review upstream #4217/#4244/#3969 individually; their combined success is not assumed.

R5. Surface trees: accurately target/capture native subsurfaces, child windows and popup/transient trees. Preserve pointer enter/leave, serials, implicit grabs and dismiss behavior. Capture/action identity must agree, including off-base popups; no hidden clicks into uncaptured surfaces.

R6. Bounded gestures: modifiers, held keys/buttons, hover, multi-segment drag and precise scroll need correct lifecycle and cancellation. Release only agent-owned state. Preserve truthful partial/unknown outcomes; never automatically replay an action of unknown delivery.

R7. Text: distinguish semantic text insertion from physical key events. Support Unicode through real target-bound available text routes and explicitly bounded IME/preedit functionality; report missing compositor/client capabilities. No universal silent clipboard hack or fake ASCII fallback. Never mutate user clipboard invisibly.

R8. Transfers: implement cross-window drag-and-drop/data offers and explicit clipboard/selection ownership with scope-bound source and destination authority. Pointer endpoint motion alone is not data transfer. Report actual application result separately from delivery.

R9. XWayland/foreground/owned-session: XWayland must use a validated route rather than assuming native-seat compatibility or global XTest isolation. Foreground routes require explicit approval and physical-input conflict checks. Incompatible workloads may run in an explicitly owned session; preserve existing windows/unsaved state and never auto-migrate. Shell/layer surfaces and authentication/lockscreen are separate targets/policy boundaries.

R10. Desktop-agent integration: one canonical user-session service and explicit MCP socket clients, per-agent sessions/cursors, idempotent registration preserving unrelated configs. Local-only transport. Status/revoke/disable controls use existing runtime contracts; stop-one-service is not equivalent to disabling every unrelated runtime. Reuse Omarchy PR11342 install/toggle patterns, consumer compatibility and fresh-login safeguards. No edits inside /usr/share/omarchy.

R11. Verification: actual applications, fresh before/after state and application-owned output oracles, independent continuous primary focus/cursor/input observation with warp-and-return negative control. Native Foot, Chromium, Firefox, Telegram/Qt, current LibreOffice, GTK3/4, Electron/Tauri and canvas, plus XWayland where available. Include popup/modal, Unicode, modifier/drag, cancellation, stale geometry/target, primary/client collision, seat exhaustion, lock/DPMS and lifecycle cases. Tests alone are not proof. Never modify personal logged-in app contents to test.

## Implementation coordination

All workers read AGENTS.md and relevant skills. No build, lint, tests or formatter while implementation is changing; one coordinated integrated verification phase afterwards. Prerequisite environment provisioning and read-only runtime diagnosis are allowed; do not replay already reported failures simply to confirm them. LSP status currently reports no configured server; recheck if availability changes. Exported symbol edits require references before changes when LSP becomes available.

Ownership is assigned by orchestrator. Central integration owns shared dispatch/public schema changes and applies requested callsite patches; specialist owners do not race to edit central files. Communicate exact proposed interfaces and file/ranges to peers. Each implementation includes behavioral regression coverage only where it catches a plausible consumer-visible boundary, and updates existing relevant documentation/changelog after smoke proof. Never create source-text/wiring assertion tests. Do not make commits or pushes unless orchestrator requests them.

## Current measured baseline, not newly claimed fork evidence

Official Driver0.28.2-1/plugin0.26.1-6 works for native Inkscape1.4.4-6 background click/key/save with bounded human-focus/pointer observations. Visible overlay rendered on both physical outputs. Cursor-state schema invalid; launch briefly stole focus. Current LibreOffice26.8.0-2 does not meet upstream hard pin. Existing physical layout: HDMI-A-1 2560x1080(-300,0), DP-1 5120x1440(0,1080). Canonical launcher ~/.local/bin/cua-driver-desktop; loaded plugin must remain untouched during development.

## Source leads

https://github.com/trycua/cua/pull/4230 (general-native experimental admission/four lanes; unmerged)
https://github.com/trycua/cua/pull/4217 (multioutput; unmerged)
https://github.com/trycua/cua/pull/4244 (fractional scale; unmerged, no live proof)
https://github.com/trycua/cua/pull/3970 (foreground keymap/locks; unmerged)
https://github.com/trycua/cua/issues/4216 (Omarchy kit/source mismatch)
https://github.com/omacom/omarchy/pull/11342 (install/input toggle; unmerged)
https://github.com/omacom/omarchy-pkgs/pull/346 (package/activation contract)
