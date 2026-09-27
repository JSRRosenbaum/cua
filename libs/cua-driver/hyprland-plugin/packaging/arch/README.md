# Omarchy paired native candidate package

`PKGBUILD` is a template for the local Driver 0.29.1 / Hyprland 0.56.2 fork
candidate, not a checkout-driven `makepkg -si` recipe. Do not build it against
the loaded desktop or load/unload the live plugin. Its module must be activated
only in a fresh disposable compositor first, and in a fresh production login
only after separate approval.

Prepare the immutable `compositor`, `driver`, and `plugin` kits with
`downstream/omarchy/candidate_profile.py prepare --repo REPO --compositor FORK
--stage STAGED_DESTDIR --output NEW_KIT_DIRECTORY`. Run preparation while the
host still has the exact reviewed stock Hyprland/Driver/plugin profile.
Source provenance records each source file digest and the final Cua revision;
the compositor profile records staged file inventory, header tree, ABI hash,
binary hash and fork revision. The paired profiles bind the actual compiler,
Rust toolchain, native runtime and package versions. Retain the independent
SHA-256 of the plugin kit's `KIT-PROVENANCE.json` for consumer checks.

Build only as an ordinary user in a disposable, dependency-complete Arch root
with the profile's exact packages and matching compiler/Rust toolchain. Bind
the staged compositor read-only at the *same absolute path* used during
preparation, and set a private `XDG_RUNTIME_DIR` before invoking `makepkg`.
The staged fork supplies the plugin's reviewed `hyprland.pc`; `pkgconf` must
select those exact headers. In that isolated root:

1. Build `compositor/` using `makepkg --noconfirm`. Its package reproduces the
   verified stage, with its own fork package/version; it does not claim the
   stock `hyprland=0.56.2-3` package identity.
2. Remove stock Hyprland through a normal ALPM transaction, then install that
   exact locally built compositor package in the disposable root. Never use
   `--nodeps` or resolve a package conflict by installing into the live host.
3. Build `driver/` and `plugin/` using `makepkg --noconfirm`. The latter
   requires the exact installed candidate compositor, checks the configured
   native CTest suite, and declares its direct `libeis.so.1` provider (`libei`)
   in addition to its exact native ABI dependencies.
4. Inspect the resulting package payloads and paired build provenance before
   assembling an isolated candidate root. `disposable_namespace.sh` checks
   those paired binaries, module and metadata and privately binds them over
   the stock filesystem; it does not modify the host `/usr`.

`downstream/omarchy/rollback.py snapshot` retains the signed known-good
package archives and metadata, while `rollback.py check` verifies them
offline. Neither performs a host package transaction or an automatic rollback.
No package recipe, kit preparation, or namespace launch activates the live
plugin. Candidate native acceptance and any eventual production promotion
remain separate evidence/authorization steps.
