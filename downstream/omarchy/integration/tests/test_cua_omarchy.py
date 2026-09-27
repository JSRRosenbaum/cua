"""Behavioral boundaries for the opt-in Omarchy session integration."""

import importlib.machinery
import importlib.util
from pathlib import Path
import socket
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "bin/cua-omarchy"
loader = importlib.machinery.SourceFileLoader("cua_omarchy", str(SCRIPT))
spec = importlib.util.spec_from_loader(loader.name, loader)
control = importlib.util.module_from_spec(spec)
loader.exec_module(control)


class OmarchyControlsTest(unittest.TestCase):
    def test_ambiguous_public_session_label_cannot_revoke_a_peer(self):
        rows = {"sessions": [
            {"session": "shared", "state": "active", "owner_short_id": "a"},
            {"session": "shared", "state": "active", "owner_short_id": "b"},
        ]}
        with mock.patch.object(control, "sessions", return_value=rows), \
                mock.patch.object(control, "run") as run:
            with self.assertRaises(RuntimeError):
                control.revoke_one("shared")
            run.assert_not_called()

    def test_foreign_mcp_registration_is_preserved(self):
        existing = mock.Mock(returncode=0, stdout='{"enabled":true,"transport":{'
                             '"type":"stdio","command":"/custom/agent",'
                             '"args":["mcp"]}}')
        with mock.patch.object(control, "run", return_value=existing):
            with self.assertRaises(RuntimeError):
                control.register_codex()

    def test_symlinked_flag_is_not_taken_over(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            target = root / "personal-flag.lua"
            target.write_text("private setting\n")
            flag = root / "cua-input.lua"
            flag.symlink_to(target)
            with mock.patch.object(control, "FLAG", flag):
                with self.assertRaises(RuntimeError):
                    control.owned_flag()
            self.assertEqual(target.read_text(), "private setting\n")
            self.assertTrue(flag.is_symlink())

    def test_direct_loader_migration_preserves_unrelated_lua_and_rollback(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            config = home / ".config/hypr/hyprland.lua"
            config.parent.mkdir(parents=True)
            before = ('require("hypr.monitors")\n'
                      'hl.plugin.load("/usr/lib/cua/hyprland/cua-hyprland-plugin.so")\n'
                      'hl.config({ plugin = { cua = { enabled = true } } })\n'
                      'o.window("unrelated", { tile = true })\n')
            config.write_text(before)
            with mock.patch.object(control.Path, "home", return_value=home), \
                    mock.patch.object(control, "owned_flag", return_value=False), \
                    mock.patch.object(control, "module_loaded", return_value=False), \
                    mock.patch.object(control, "hypr", return_value=mock.Mock(returncode=0)):
                control.migrate_input_config()
                control.migrate_input_config()
            self.assertEqual(config.read_text(),
                             'require("hypr.monitors")\no.window("unrelated", { tile = true })\n')
            self.assertEqual(config.with_name(config.name + ".cua-before-omarchy").read_text(), before)

    def test_menu_install_remove_preserves_foreign_jsonc_byte_for_byte(self):
        with tempfile.TemporaryDirectory() as directory:
            home = Path(directory)
            target = home / ".config/omarchy/extensions/omarchy-menu.jsonc"
            target.parent.mkdir(parents=True)
            original = '{\n  // User-owned menu entry.\n  "personal.notes": {"label": "Notes"},\n}\n'
            target.write_text(original)
            source = SCRIPT.parents[1] / "omarchy/menu-items.json"
            with mock.patch.object(control.Path, "home", return_value=home), \
                    mock.patch.object(control, "MENU_SOURCE", source):
                control.change_menu()
                first = target.read_text()
                control.change_menu()
                self.assertEqual(target.read_text(), first)
                entries, _ = control.parse_jsonc(first)
                self.assertEqual(entries["personal.notes"], {"label": "Notes"})
                control.change_menu(remove=True)
            self.assertEqual(target.read_text(), original)

    def test_disposable_caller_cannot_start_physical_manager_service(self):
        identity = {"environment": {"XDG_RUNTIME_DIR": "/tmp/lab",
                                    "WAYLAND_DISPLAY": "wayland-lab",
                                    "HYPRLAND_INSTANCE_SIGNATURE": "lab"}}
        manager = mock.Mock(returncode=0, stdout='{"XDG_RUNTIME_DIR":"/run/user/1000",'
                            '"WAYLAND_DISPLAY":"wayland-3",'
                            '"HYPRLAND_INSTANCE_SIGNATURE":"physical"}')
        with mock.patch.object(control, "current_session_identity", return_value=identity), \
                mock.patch.object(control, "run", return_value=manager), \
                mock.patch.object(control, "service_active") as active:
            with self.assertRaises(RuntimeError):
                control.setup(retire_legacy=True)
            active.assert_not_called()

    def test_startup_binding_refuses_manager_identity_change_before_exec(self):
        with tempfile.TemporaryDirectory() as directory:
            current = {"environment": {"WAYLAND_DISPLAY": "wayland-lab"}}
            pinned = {"environment": {"WAYLAND_DISPLAY": "wayland-3"}}
            with mock.patch.object(control, "current_session_identity", return_value=current), \
                    mock.patch.object(control, "service_active", return_value=False), \
                    mock.patch.object(control, "run", return_value=mock.Mock(returncode=1)), \
                    mock.patch.object(control, "private_runtime_dir", return_value=Path(directory)), \
                    mock.patch.object(control, "read_expected_session", return_value=pinned), \
                    mock.patch.object(control.os, "execv") as execv:
                with self.assertRaises(RuntimeError):
                    control.serve()
                execv.assert_not_called()

    def test_service_waits_for_delayed_socket_and_cli_readiness(self):
        with tempfile.TemporaryDirectory() as directory:
            location = Path(directory)
            endpoint = location / "driver.sock"
            artifacts_created = False
            transient_seen = False
            ready_observed = False

            def advance(_seconds):
                nonlocal artifacts_created
                if not artifacts_created:
                    artifacts_created = True
                    sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    sock.bind(str(endpoint))
                    sock.close()
                    (location / "driver.pid").write_text("123")

            def probe():
                nonlocal transient_seen, ready_observed
                if not transient_seen:
                    transient_seen = True
                    raise RuntimeError("Omarchy service is not responding: starting")
                ready_observed = True
                return {"sessions": []}

            with mock.patch.object(control, "service_active", return_value=True), \
                    mock.patch.object(control, "socket_path", return_value=endpoint), \
                    mock.patch.object(control, "runtime_dir", return_value=location), \
                    mock.patch.object(control, "attest_socket_owner"), \
                    mock.patch.object(control, "sessions_unchecked", side_effect=probe), \
                    mock.patch.object(control.time, "sleep", side_effect=advance):
                control.await_service_ready({"environment": {"WAYLAND_DISPLAY": "wayland-lab"}})
                self.assertTrue(artifacts_created)
                self.assertTrue(ready_observed)

    def test_foreign_display_socket_cannot_list_sessions(self):
        identity = {"environment": {"WAYLAND_DISPLAY": "wayland-lab"}}
        with tempfile.TemporaryDirectory() as directory:
            (Path(directory) / "driver.pid").write_text("123")
            endpoint = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            endpoint.bind(str(Path(directory) / "driver.sock"))
            endpoint.close()
            with mock.patch.object(control, "current_session_identity", return_value=identity), \
                    mock.patch.object(control, "read_expected_session", return_value=None), \
                    mock.patch.object(control, "private_runtime_dir", return_value=Path(directory)), \
                    mock.patch.object(control, "socket_path", return_value=Path(directory) / "driver.sock"), \
                    mock.patch.object(control.Path, "read_bytes",
                                      return_value=b"WAYLAND_DISPLAY=wayland-3\0"), \
                    mock.patch.object(control, "sessions_unchecked") as request:
                with self.assertRaises(RuntimeError):
                    control.sessions()
                request.assert_not_called()

    def test_legacy_service_requires_explicit_cutover(self):
        def command(*argv, **_kwargs):
            if argv[-2:] == ("--quiet", control.OLD_SERVICE):
                return mock.Mock(returncode=0)
            self.fail(f"unexpected service mutation: {argv}")

        with mock.patch.object(control, "current_session_identity", return_value={"environment": {}}), \
                mock.patch.object(control, "manager_session_matches"), \
                mock.patch.object(control, "service_active", side_effect=[False, True]), \
                mock.patch.object(control, "run", side_effect=command):
            with self.assertRaises(RuntimeError):
                control.setup(retire_legacy=False)


if __name__ == "__main__":
    unittest.main()
