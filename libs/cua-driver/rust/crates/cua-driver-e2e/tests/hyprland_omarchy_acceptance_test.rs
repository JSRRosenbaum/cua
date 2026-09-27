//! Supplemental, manual native-Hyprland acceptance on the disposable candidate only.
//! The selector-free canonical Linux matrix and existing native Hyprland gates still run.
//! This suite uses the ordinary McpDriver/testkit sentinel, not a parallel MCP runner.
//! Launch only with the private candidate endpoint, explicit general-native mode,
//! CUA_DRIVER_RS_ENABLE_WAYLAND=1 and a staged Electron sentinel. This suite
//! explicitly starts the testkit daemon in standard mode, never unrestricted.

#![cfg(target_os = "linux")]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cua_driver_testkit::observer::TargetWindow;
use cua_driver_testkit::sentinel::ForegroundSentinel;
use cua_driver_testkit::{spawn_in_job, workspace_root, Driver, McpDriver, ToolResponse};
use serde_json::{json, Value};

#[path = "seat_isolation_acceptance/mod.rs"]
mod seat_isolation_acceptance;

const UNICODE: &str = "Grüße — 漢字 — مرحبا — 👩🏽‍💻";

fn hyprctl(args: &[&str]) -> Value {
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .expect("private native Hyprland endpoint needs hyprctl");
    assert!(output.status.success(), "hyprctl {args:?}: {output:?}");
    serde_json::from_slice(&output.stdout).expect("Hyprland JSON")
}

fn cursor() -> (f64, f64) {
    let state = hyprctl(&["-j", "cursorpos"]);
    (
        state["x"].as_f64().expect("compositor cursor x"),
        state["y"].as_f64().expect("compositor cursor y"),
    )
}

fn focus() -> String {
    hyprctl(&["-j", "activewindow"])["address"]
        .as_str()
        .expect("compositor active window address")
        .to_owned()
}

fn warp(point: (f64, f64)) {
    let output = Command::new("hyprctl")
        .args([
            "dispatch",
            "movecursor",
            &point.0.round().to_string(),
            &point.1.round().to_string(),
        ])
        .output()
        .expect("independent compositor cursor negative control");
    assert!(
        output.status.success() && output.stdout == b"ok\n",
        "{output:?}"
    );
}

struct RestoreCursor((f64, f64));
impl Drop for RestoreCursor {
    fn drop(&mut self) {
        warp(self.0);
    }
}

#[derive(Clone, serde::Serialize)]
struct Sample {
    elapsed_ms: u128,
    cursor: (f64, f64),
    focus: String,
}

/// Independent compositor-state stream across the entire action interval.
/// Hyprland's focus-event journal and Electron input journal are also checked
/// by ForegroundSentinel::observe_background, including transient focus changes.
struct PrimarySeatWatch {
    stop: Arc<AtomicBool>,
    samples: Vec<Sample>,
    receiver: mpsc::Receiver<Sample>,
    worker: Option<JoinHandle<()>>,
    evidence: PathBuf,
}

impl PrimarySeatWatch {
    fn start(evidence: PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let started = Instant::now();
            while !thread_stop.load(Ordering::Acquire) {
                let current = Sample {
                    elapsed_ms: started.elapsed().as_millis(),
                    cursor: cursor(),
                    focus: focus(),
                };
                if sender.send(current).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let mut watch = Self {
            stop,
            samples: Vec::new(),
            receiver,
            worker: Some(worker),
            evidence,
        };
        watch.wait_for(|samples| samples.len() >= 2);
        watch
    }

    fn wait_for(&mut self, seen: impl Fn(&[Sample]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !seen(&self.samples) {
            assert!(
                !self.worker.as_ref().unwrap().is_finished(),
                "independent primary-seat sampler stopped"
            );
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "primary-seat sampler did not observe control"
            );
            self.samples.push(
                self.receiver
                    .recv_timeout(remaining)
                    .expect("independent primary-seat sampler stopped"),
            );
        }
    }

    fn finish(mut self) -> Vec<Sample> {
        self.stop.store(true, Ordering::Release);
        self.worker
            .take()
            .unwrap()
            .join()
            .expect("sampler panicked");
        self.samples.extend(self.receiver.try_iter());
        std::fs::write(
            &self.evidence,
            serde_json::to_vec_pretty(&self.samples).unwrap(),
        )
        .expect("retain independent primary-seat samples");
        std::mem::take(&mut self.samples)
    }
}

impl Drop for PrimarySeatWatch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn assert_stable(samples: &[Sample], cursor_before: (f64, f64), focused: &str) {
    assert!(samples.len() >= 3, "continuous observer never became live");
    for sample in samples {
        assert_eq!(sample.focus, focused, "primary focus changed transiently");
        assert!(
            (sample.cursor.0 - cursor_before.0).abs() <= 1.0
                && (sample.cursor.1 - cursor_before.1).abs() <= 1.0,
            "primary cursor moved transiently: {}ms {:?} vs {:?}",
            sample.elapsed_ms,
            sample.cursor,
            cursor_before
        );
    }
    for pair in samples.windows(2) {
        assert!(
            pair[1].elapsed_ms.saturating_sub(pair[0].elapsed_ms) < 500,
            "independent primary-seat observation stalled: {}ms -> {}ms",
            pair[0].elapsed_ms,
            pair[1].elapsed_ms
        );
    }
}

/// Equal before/after cursor endpoints alone cannot certify isolation.
/// Warp to a real monitor interior, wait for the *same independent sampler* to
/// observe it, return, and verify both samples survived in the evidence file.
fn logical_extent(monitor: &Value) -> (f64, f64) {
    let scale = monitor["scale"].as_f64().expect("monitor scale");
    assert!(scale.is_finite() && scale > 0.0);
    let width = monitor["width"].as_f64().expect("monitor width");
    let height = monitor["height"].as_f64().expect("monitor height");
    let transform = monitor["transform"].as_u64().expect("monitor rotation");
    assert!(transform <= 7 && width > 0.0 && height > 0.0);
    if transform % 2 == 1 {
        (height / scale, width / scale)
    } else {
        (width / scale, height / scale)
    }
}

fn warp_return_negative_control(dir: &Path) {
    let original = cursor();
    let regions = hyprctl(&["-j", "monitors"]);
    let monitor = regions
        .as_array()
        .unwrap()
        .iter()
        .find(|monitor| {
            let x = monitor["x"].as_f64().unwrap();
            let y = monitor["y"].as_f64().unwrap();
            let (w, h) = logical_extent(monitor);
            original.0 >= x && original.0 < x + w && original.1 >= y && original.1 < y + h
        })
        .expect("cursor must be inside a powered monitor");
    let x = monitor["x"].as_f64().unwrap();
    let y = monitor["y"].as_f64().unwrap();
    let (w, h) = logical_extent(monitor);
    let mut destination = ((x + w * 0.25).round(), (y + h * 0.25).round());
    if (destination.0 - original.0).hypot(destination.1 - original.1) <= 8.0 {
        destination = ((x + w * 0.75).round(), (y + h * 0.75).round());
    }
    assert!((destination.0 - original.0).hypot(destination.1 - original.1) > 8.0);
    let mut watch = PrimarySeatWatch::start(dir.join("warp-return-control.json"));
    let restore = RestoreCursor(original);
    warp(destination);
    watch.wait_for(|samples| {
        samples.iter().any(|sample| {
            (sample.cursor.0 - destination.0).abs() <= 1.0
                && (sample.cursor.1 - destination.1).abs() <= 1.0
        })
    });
    drop(restore);
    watch.wait_for(|samples| {
        samples
            .iter()
            .any(|sample| (sample.cursor.0 - destination.0).abs() <= 1.0)
            && samples.last().is_some_and(|sample| {
                (sample.cursor.0 - original.0).abs() <= 1.0
                    && (sample.cursor.1 - original.1).abs() <= 1.0
            })
    });
    let samples = watch.finish();
    assert_ne!(samples.first().unwrap().cursor, destination);
    assert!(
        (cursor().0 - original.0).abs() <= 1.0 && (cursor().1 - original.1).abs() <= 1.0,
        "negative control did not restore primary pointer"
    );
}

struct Fixture {
    target: TargetWindow,
    journal: PathBuf,
}

fn events(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("fixture journal exists")
        .lines()
        .map(|line| serde_json::from_str(line).expect("fixture-owned event JSON"))
        .collect()
}

fn launch(driver: &mut McpDriver, dir: &Path, actor: &str) -> Fixture {
    let script =
        workspace_root().join("../tests/fixtures/apps/linux/isolated-input/native_capabilities.py");
    assert!(
        script.is_file(),
        "source-owned native fixture missing: {script:?}"
    );
    let journal = dir.join(format!("{actor}.jsonl"));
    let title = format!("Cua Native Capabilities {actor}");
    let child = spawn_in_job(
        Command::new("python3")
            .arg(script)
            .args(["--actor", actor, "--journal"])
            .arg(&journal)
            .env("GDK_BACKEND", "wayland")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .expect("launch native GTK3 application fixture");
    let pid = child.id();
    driver.reaper().push(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let windows = driver.call("list_windows", json!({"pid": pid}));
        if let Some(window_id) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows.iter().find(|window| {
                    window["pid"].as_u64() == Some(u64::from(pid))
                        && window["title"].as_str() == Some(title.as_str())
                })
            })
            .and_then(|window| window["window_id"].as_u64())
        {
            let clients = hyprctl(&["-j", "clients"]);
            let client = clients
                .as_array()
                .unwrap()
                .iter()
                .find(|client| {
                    client["pid"].as_u64() == Some(u64::from(pid))
                        && client["title"].as_str() == Some(title.as_str())
                })
                .expect("independent compositor target identity");
            assert_eq!(client["xwayland"], false, "fixture must use native Wayland");
            assert_eq!(
                u64::from_str_radix(
                    client["address"].as_str().unwrap().trim_start_matches("0x"),
                    16
                )
                .unwrap(),
                window_id,
                "Driver and compositor must agree on target identity"
            );
            assert!(events(&journal)
                .iter()
                .any(|event| event["kind"] == "ready"));
            return Fixture {
                target: TargetWindow {
                    pid,
                    native_id: window_id,
                },
                journal,
            };
        }
        assert!(
            Instant::now() < deadline,
            "native fixture not mapped: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn snapshot(driver: &mut McpDriver, fixture: &Fixture) -> ToolResponse {
    let state = driver.call(
        "get_window_state",
        json!({
            "pid": fixture.target.pid, "window_id": fixture.target.native_id,
        }),
    );
    assert!(!state.is_error(), "native capture failed: {}", state.text());
    assert!(
        state.structured()["screenshot_width"]
            .as_f64()
            .unwrap_or(0.0)
            > 0.0
    );
    state
}

fn token(state: &ToolResponse, name: &str) -> String {
    state.structured()["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["label"].as_str() == Some(name))
        .and_then(|element| element["element_token"].as_str())
        .unwrap_or_else(|| {
            panic!(
                "target {name:?} absent in exact-window AX snapshot: {}",
                state.tree_text()
            )
        })
        .to_owned()
}

fn point(
    driver: &mut McpDriver,
    fixture: &Fixture,
    state: &ToolResponse,
    name: &str,
) -> (f64, f64) {
    let element = state.structured()["elements"]
        .as_array()
        .unwrap()
        .iter()
        .find(|element| element["label"].as_str() == Some(name))
        .unwrap_or_else(|| panic!("missing native control: {name}"));
    let frame = &element["frame"];
    let windows = driver.call("list_windows", json!({"pid": fixture.target.pid}));
    let window = windows.structured()["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["window_id"].as_u64() == Some(fixture.target.native_id))
        .unwrap();
    let width = state.structured()["screenshot_width"].as_f64().unwrap();
    let height = state.structured()["screenshot_height"].as_f64().unwrap();
    let x = (frame["x"].as_f64().unwrap() + frame["w"].as_f64().unwrap() / 2.0
        - window["x"].as_f64().unwrap())
        * width
        / window["width"].as_f64().unwrap();
    let y = (frame["y"].as_f64().unwrap() + frame["h"].as_f64().unwrap() / 2.0
        - window["y"].as_f64().unwrap())
        * height
        / window["height"].as_f64().unwrap();
    assert!(
        x >= 0.0 && x < width && y >= 0.0 && y < height,
        "{name} lies outside captured exact target ({x},{y}) / ({width},{height})"
    );
    (x, y)
}

fn evidence_dir() -> (PathBuf, Option<tempfile::TempDir>) {
    let root = std::env::var_os("CUA_E2E_RECORDINGS_ROOT");
    let dir = if let Some(root) = &root {
        tempfile::Builder::new()
            .prefix("hyprland-omarchy-")
            .tempdir_in(root)
    } else {
        tempfile::tempdir()
    }
    .expect("test-owned evidence directory");
    let path = dir.path().to_path_buf();
    if root.is_some() {
        dir.keep();
        (path, None)
    } else {
        (path, Some(dir))
    }
}

fn preflight() {
    assert!(std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some());
    assert_eq!(
        std::env::var("CUA_DRIVER_RS_ENABLE_WAYLAND").as_deref(),
        Ok("1")
    );
    assert_ne!(
        std::env::var("CUA_DRIVER_PERMISSION_MODE").as_deref(),
        Ok("unrestricted")
    );
    assert_eq!(
        std::env::var("CUA_HYPRLAND_COMPATIBILITY_MODE").as_deref(),
        Ok("general-native")
    );
    assert_ne!(
        std::env::var("HOME").as_deref(),
        Ok("/home/joseph"),
        "never launch acceptance against the physical user profile"
    );
}

#[test]
#[ignore = "candidate disposable native Hyprland plus explicit general-native mode and Electron sentinel"]
fn general_native_px_click_has_application_effect_and_continuous_primary_isolation() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-general-native",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let target = launch(&mut driver, &dir, "Target");
    let state = snapshot(&mut driver, &target);
    let button = point(&mut driver, &target, &state, "native-activate");
    let sentinel = ForegroundSentinel::launch(&mut driver);
    sentinel
        .assert_guard_canaries(&mut driver, target.target)
        .expect("independent sentinel guard canaries");
    driver.start_behavior_recording();
    sentinel
        .prepare_background_observation(&mut driver, target.target)
        .unwrap();
    warp_return_negative_control(&dir);
    let before = events(&target.journal).len();
    let original = cursor();
    let focused = focus();
    let watch = PrimarySeatWatch::start(dir.join("general-native-primary.json"));
    let (result, _oracles) = sentinel
        .observe_background(target.target, || {
            driver.call(
                "click",
                json!({
                    "pid": target.target.pid, "window_id": target.target.native_id,
                    "x": button.0, "y": button.1, "delivery_mode": "background",
                }),
            )
        })
        .expect("primary focus, cursor and sentinel input stay unchanged");
    let samples = watch.finish();
    assert_stable(&samples, original, &focused);
    assert!(
        !result.is_error(),
        "general-native click not delivered: {}",
        result.text()
    );
    assert_eq!(
        events(&target.journal)[before..]
            .iter()
            .filter(|event| event["kind"] == "activated")
            .count(),
        1,
        "actual unqualified native app must activate exactly once"
    );
}

#[test]
#[ignore = "candidate disposable native Hyprland, target-bound semantic text and Electron sentinel"]
fn exact_unicode_text_changes_only_the_named_native_widget() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-unicode-target",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let target = launch(&mut driver, &dir, "Target");
    let sibling = launch(&mut driver, &dir, "Sibling");
    let state = snapshot(&mut driver, &target);
    let text_target = token(&state, "native-exact-text");
    let sentinel = ForegroundSentinel::launch(&mut driver);
    driver.start_behavior_recording();
    sentinel
        .prepare_background_observation(&mut driver, target.target)
        .unwrap();
    let before_sibling = events(&sibling.journal).len();
    let original = cursor();
    let focused = focus();
    let watch = PrimarySeatWatch::start(dir.join("unicode-primary.json"));
    let (result, _oracles) = sentinel
        .observe_background(target.target, || {
            driver.call(
                "type_text",
                json!({
                    "pid": target.target.pid, "window_id": target.target.native_id,
                    "element_token": text_target, "text": UNICODE,
                    "delivery_mode": "background",
                }),
            )
        })
        .expect("target-bound text must preserve the physical seat");
    let samples = watch.finish();
    assert_stable(&samples, original, &focused);
    assert!(
        !result.is_error(),
        "semantic Unicode failed: {}",
        result.text()
    );
    assert_eq!(
        result.structured()["route"],
        "semantic_text",
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["effect"],
        "unverifiable",
        "{}",
        result.raw
    );
    assert_eq!(result.structured()["verified"], false, "{}", result.raw);
    assert!(
        events(&target.journal)
            .iter()
            .any(|event| event["kind"] == "text" && event["value"] == UNICODE),
        "real GTK widget did not contain exact Unicode"
    );
    assert!(
        events(&sibling.journal)[before_sibling..]
            .iter()
            .all(|event| event["kind"] != "text"),
        "unowned sibling received text"
    );
    assert!(!snapshot(&mut driver, &sibling)
        .tree_text()
        .contains(UNICODE));
}

fn guest_window(driver: &mut McpDriver, journal: PathBuf, actor: &str) -> Fixture {
    let title = format!("Cua Native Capabilities {actor}");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let windows = driver.call("list_windows", json!({}));
        if let Some(window) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| window["title"].as_str() == Some(title.as_str()))
            })
        {
            if journal.is_file() {
                let ready = events(&journal)
                    .into_iter()
                    .find(|event| event["kind"] == "ready");
                if let Some(ready) = ready {
                    let pid = ready["pid"].as_u64().expect("guest fixture PID");
                    assert_eq!(
                        window["pid"].as_u64(),
                        Some(pid),
                        "private X11 process identity"
                    );
                    let window_id = window["window_id"].as_u64().expect("private XID");
                    assert_ne!(window_id, 0);
                    return Fixture {
                        target: TargetWindow {
                            pid: pid as u32,
                            native_id: window_id,
                        },
                        journal,
                    };
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "owned Xpra fixture did not map: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn guest_window_geometry(driver: &mut McpDriver, fixture: &Fixture) -> Value {
    let listed = driver.call("list_windows", json!({"pid": fixture.target.pid}));
    listed.structured()["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|window| window["window_id"].as_u64() == Some(fixture.target.native_id))
        .expect("owned X11 exact window disappeared")
        .clone()
}

fn screen_point(
    driver: &mut McpDriver,
    fixture: &Fixture,
    state: &ToolResponse,
    name: &str,
) -> (f64, f64) {
    let (x, y) = point(driver, fixture, state, name);
    let window = guest_window_geometry(driver, fixture);
    (
        window["x"].as_f64().unwrap()
            + x * window["width"].as_f64().unwrap()
                / state.structured()["screenshot_width"].as_f64().unwrap(),
        window["y"].as_f64().unwrap()
            + y * window["height"].as_f64().unwrap()
                / state.structured()["screenshot_height"].as_f64().unwrap(),
    )
}

fn activated_private_environment(socket: &str, display: &str, journal: &Path) {
    let root = Path::new(socket).parent().expect("owned socket directory");
    let home = root.join("home");
    let data = home.join(".local/share");
    let service_directory = data.join("dbus-1/services");
    std::fs::create_dir_all(&service_directory).expect("case-owned D-Bus service directory");
    let probe =
        workspace_root().join("../tests/fixtures/apps/linux/isolated-input/owned_dbus_probe.py");
    assert!(
        probe.is_file(),
        "source-owned activation probe missing: {probe:?}"
    );
    let service = format!(
        "[D-BUS Service]\nName=org.cua.OwnedEnvProbe\nExec=/usr/bin/python3 {} --journal {}\n",
        probe.display(),
        journal.display()
    );
    std::fs::write(
        service_directory.join("org.cua.OwnedEnvProbe.service"),
        service,
    )
    .expect("write service definition in private XDG_DATA_HOME only");
    let bus = format!("unix:path={}", root.join("dbus").display());
    let reloaded = Command::new("gdbus")
        .args([
            "call",
            "--address",
            &bus,
            "--dest",
            "org.freedesktop.DBus",
            "--object-path",
            "/org/freedesktop/DBus",
            "--method",
            "org.freedesktop.DBus.ReloadConfig",
        ])
        .env_remove("DISPLAY")
        .output()
        .expect("private session bus must support gdbus");
    assert!(
        reloaded.status.success(),
        "private bus service reload: {reloaded:?}"
    );
    let activated = Command::new("gdbus")
        .args([
            "call",
            "--address",
            &bus,
            "--dest",
            "org.cua.OwnedEnvProbe",
            "--object-path",
            "/org/cua/OwnedEnvProbe",
            "--method",
            "org.cua.OwnedEnvProbe.Report",
        ])
        .env_remove("DISPLAY")
        .output()
        .expect("activate case-owned service on private bus");
    assert!(
        activated.status.success()
            && String::from_utf8_lossy(&activated.stdout).contains("owned-env-recorded"),
        "private bus did not activate its case-owned service: {activated:?}"
    );
    let record = events(journal);
    assert_eq!(
        record.len(),
        1,
        "activation fixture must write exactly one record"
    );
    assert_eq!(record[0]["kind"], "activated-environment");
    let env = &record[0]["environment"];
    assert_eq!(env["DISPLAY"], display);
    assert_eq!(env["GDK_BACKEND"], "x11");
    assert_eq!(env["QT_QPA_PLATFORM"], "xcb");
    assert_eq!(env["HOME"], home.to_string_lossy().as_ref());
    assert_eq!(env["XDG_RUNTIME_DIR"], root.to_string_lossy().as_ref());
    assert_eq!(env["XDG_DATA_HOME"], data.to_string_lossy().as_ref());
    assert_eq!(
        env["XAUTHORITY"],
        root.join("Xauthority").to_string_lossy().as_ref()
    );
    assert_eq!(env["DBUS_SESSION_BUS_ADDRESS"], bus);
    assert!(
        env["WAYLAND_DISPLAY"].is_null() && env["HYPRLAND_INSTANCE_SIGNATURE"].is_null(),
        "activated service inherited host compositor identity: {env}"
    );
}

#[test]
#[ignore = "requires candidate Driver and a private Xpra/X11 owned session, GTK3 and XTest"]
fn owned_xpra_intra_x11_xdnd_reaches_real_destination_application() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let source_journal = dir.join("OwnedSource.jsonl");
    let destination_journal = dir.join("OwnedDestination.jsonl");
    let script =
        workspace_root().join("../tests/fixtures/apps/linux/isolated-input/native_capabilities.py");
    let mut host = McpDriver::spawn_named_with_env(
        "omarchy-owned-xpra-xdnd",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate host Driver required");
    let focused = focus();
    let cursor_before = cursor();
    let watch = PrimarySeatWatch::start(dir.join("owned-xpra-host-primary.json"));
    let owned = host.call(
        "launch_app",
        json!({
            "launch_path": "/usr/bin/python3",
            "additional_arguments": [script, "--actor", "Source", "--journal", source_journal,
                                     "--x", "50", "--y", "50"],
            "isolation": "owned_session",
        }),
    );
    assert!(
        !owned.is_error(),
        "owned Xpra launch failed: {}",
        owned.text()
    );
    assert_eq!(owned.structured()["host_active"], false);
    let owned_id = owned.structured()["owned_session"]["id"]
        .as_str()
        .expect("owned session id")
        .to_owned();
    let private_socket = owned.structured()["owned_session"]["socket"]
        .as_str()
        .expect("private owned Driver socket")
        .to_owned();
    let private_display = owned.structured()["owned_session"]["display"]
        .as_str()
        .expect("private Xpra DISPLAY")
        .to_owned();
    let proof = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_ne!(
            private_socket,
            std::env::var("CUA_DRIVER_SOCKET").unwrap_or_default(),
            "never use a host Driver socket for owned X11 actions"
        );
        let mut guest =
            McpDriver::spawn_daemon_proxy_named(&private_socket, "omarchy-private-xdnd")
                .expect("private Xpra Driver must be reachable");
        let source = guest_window(&mut guest, source_journal.clone(), "Source");
        // Retained ownership must outlive an idle worker's ordinary keepalive;
        // an expiring launch thread must not parent-death-kill the Xpra session.
        let heartbeat_before_idle = events(&source.journal).len();
        thread::sleep(Duration::from_secs(12));
        assert!(
            std::os::unix::net::UnixStream::connect(&private_socket).is_ok(),
            "owned Driver vanished after launch worker became idle"
        );
        assert!(
            events(&source.journal)[heartbeat_before_idle..]
                .iter()
                .any(|event| event["kind"] == "heartbeat"),
            "owned GTK source died while Xpra should remain live"
        );
        activated_private_environment(
            &private_socket,
            &private_display,
            &dir.join("owned-dbus-env.jsonl"),
        );
        let launched = guest.call(
            "launch_app",
            json!({
                "launch_path": "/usr/bin/python3",
                "additional_arguments": [script, "--actor", "Destination", "--journal",
                                         destination_journal, "--x", "600", "--y", "50"],
            }),
        );
        assert!(
            !launched.is_error(),
            "launch second app in SAME private DISPLAY: {}",
            launched.text()
        );
        let destination = guest_window(&mut guest, destination_journal.clone(), "Destination");
        assert_ne!(source.target.pid, destination.target.pid);
        let source_frame = snapshot(&mut guest, &source);
        let destination_frame = snapshot(&mut guest, &destination);
        let from = screen_point(&mut guest, &source, &source_frame, "native-data-source");
        let to = screen_point(
            &mut guest,
            &destination,
            &destination_frame,
            "native-data-destination",
        );
        let source_bounds = guest_window_geometry(&mut guest, &source);
        let dest_bounds = guest_window_geometry(&mut guest, &destination);
        assert!(
            source_bounds["x"].as_f64().unwrap() + source_bounds["width"].as_f64().unwrap()
                <= dest_bounds["x"].as_f64().unwrap(),
            "owned source/destination must not overlap during actual XDND"
        );
        let desktop = guest.call("get_desktop_state", json!({}));
        assert!(
            !desktop.is_error(),
            "private Xpra desktop capture: {}",
            desktop.text()
        );
        let width = desktop.structured()["screen_width"].as_f64().unwrap();
        let height = desktop.structured()["screen_height"].as_f64().unwrap();
        for (x, y) in [from, to] {
            assert!(
                x >= 0.0 && y >= 0.0 && x < width && y < height,
                "XDND endpoint must be on the private Xpra display"
            );
        }
        guest.start_behavior_recording();
        // Explicit desktop-scope XTest is a foreground gesture in the PRIVATE
        // owned X server only. It is not a host foreground takeover or proof
        // that existing XWayland windows admit a dual-target background route.
        let dragged = guest.call(
            "drag",
            json!({
                "scope": "desktop", "delivery_mode": "foreground",
                "from_x": from.0, "from_y": from.1,
                "to_x": to.0, "to_y": to.1, "duration_ms": 900, "steps": 45,
            }),
        );
        assert!(
            !dragged.is_error(),
            "private Xpra desktop XDND gesture: {}",
            dragged.text()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !events(&destination.journal).iter().any(|event| {
            event["kind"] == "drop-received"
                && event["accepted"] == true
                && event["text"] == "Cua native cross-window data offer — 漢字"
        }) {
            assert!(
                Instant::now() < deadline,
                "destination app did not receive real XDND offer: {:?}",
                events(&destination.journal)
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            events(&source.journal)
                .iter()
                .any(|event| event["kind"] == "offer-sent"),
            "source did not offer GTK text/plain selection data"
        );
    }));
    let stopped = host.call("stop_owned_session", json!({"id": owned_id}));
    let samples = watch.finish();
    assert_stable(&samples, cursor_before, &focused);
    assert!(
        !stopped.is_error() && stopped.structured()["stopped"] == true,
        "private owned session cleanup failed: {}",
        stopped.text()
    );
    assert!(
        std::os::unix::net::UnixStream::connect(&private_socket).is_err(),
        "private Driver socket remained reachable after owned session stop"
    );
    if let Err(failure) = proof {
        std::panic::resume_unwind(failure);
    }
}

#[test]
#[ignore = "candidate disposable native Hyprland seat-scoped data device plus two GTK3 clients"]
fn native_cross_client_transfer_has_actual_destination_state() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-native-dnd",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let source = launch(&mut driver, &dir, "Source");
    let destination = launch(&mut driver, &dir, "Destination");
    let sentinel = ForegroundSentinel::launch(&mut driver);
    driver.start_behavior_recording();
    sentinel
        .prepare_background_observation(&mut driver, source.target)
        .unwrap();
    let source_frame = snapshot(&mut driver, &source);
    let destination_frame = snapshot(&mut driver, &destination);
    let from = point(&mut driver, &source, &source_frame, "native-data-source");
    let to = point(
        &mut driver,
        &destination,
        &destination_frame,
        "native-data-destination",
    );
    let source_capture_id = source_frame.structured()["capture_id"]
        .as_str()
        .expect("source exact PNG binding");
    let destination_capture_id = destination_frame.structured()["capture_id"]
        .as_str()
        .expect("destination exact PNG binding");
    let before_destination = events(&destination.journal).len();
    let original = cursor();
    let focused = focus();
    let watch = PrimarySeatWatch::start(dir.join("native-dnd-primary.json"));
    let (result, _oracles) =
        sentinel
            .observe_background(source.target, || {
                driver.call("transfer_data", json!({
        "source_pid": source.target.pid, "source_window_id": source.target.native_id,
        "source_capture_id": source_capture_id, "source_x": from.0, "source_y": from.1,
        "destination_pid": destination.target.pid,
        "destination_window_id": destination.target.native_id,
        "destination_capture_id": destination_capture_id,
        "destination_x": to.0, "destination_y": to.1,
        "delivery_mode": "background", "duration_ms": 900,
    }))
            })
            .expect("native DnD must preserve physical-seat sentinel");
    let samples = watch.finish();
    assert_stable(&samples, original, &focused);
    assert!(
        !result.is_error(),
        "native two-target transfer failed: {}",
        result.text()
    );
    assert_eq!(
        result.structured()["route"],
        "seat_data_device",
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["protocol_finished"],
        true,
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["source_started"],
        true,
        "{}",
        result.raw
    );
    assert_eq!(result.structured()["offered"], true, "{}", result.raw);
    assert_eq!(result.structured()["accepted"], true, "{}", result.raw);
    assert_eq!(
        result.structured()["receive_requested"],
        true,
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["drop_performed"],
        true,
        "{}",
        result.raw
    );
    assert_eq!(result.structured()["finished"], true, "{}", result.raw);
    assert_eq!(
        result.structured()["application_result"],
        "unverified",
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["verified"],
        false,
        "protocol completion is not application-state verification: {}",
        result.raw
    );
    assert!(
        events(&source.journal)
            .iter()
            .any(|event| event["kind"] == "offer-sent"),
        "source never offered wl_data_device text/plain payload"
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !events(&destination.journal)[before_destination..]
        .iter()
        .any(|event| {
            event["kind"] == "drop-received"
                && event["accepted"] == true
                && event["text"] == "Cua native cross-window data offer — 漢字"
        })
    {
        assert!(
            Instant::now() < deadline,
            "destination did not accept exact GTK data offer: {:?}",
            events(&destination.journal)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[ignore = "candidate disposable native Hyprland certified compatibility default and GTK3"]
fn certified_mode_refuses_unqualified_native_without_side_effects() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-certified-refusal",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "certified"),
        ],
    )
    .expect("candidate Driver required");
    let target = launch(&mut driver, &dir, "Target");
    let state = snapshot(&mut driver, &target);
    let button = point(&mut driver, &target, &state, "native-activate");
    let sentinel = ForegroundSentinel::launch(&mut driver);
    driver.start_behavior_recording();
    sentinel
        .prepare_background_observation(&mut driver, target.target)
        .unwrap();
    let before = events(&target.journal).len();
    let original = cursor();
    let focused = focus();
    let watch = PrimarySeatWatch::start(dir.join("certified-refusal-primary.json"));
    let (result, _oracles) = sentinel
        .observe_background(target.target, || {
            driver.call(
                "click",
                json!({
                    "pid": target.target.pid, "window_id": target.target.native_id,
                    "x": button.0, "y": button.1, "delivery_mode": "background",
                }),
            )
        })
        .expect("certified refusal must preserve primary seat");
    let samples = watch.finish();
    assert_stable(&samples, original, &focused);
    assert!(
        result.is_error(),
        "unqualified fixture must remain excluded in certified mode"
    );
    assert_eq!(
        result.structured()["code"],
        "background_unavailable",
        "{}",
        result.raw
    );
    assert_eq!(
        result.structured()["reason"],
        "client_not_qualified",
        "{}",
        result.raw
    );
    assert_eq!(result.structured()["effect"], "refused", "{}", result.raw);
    assert!(
        events(&target.journal)[before..]
            .iter()
            .all(|event| event["kind"] != "activated"),
        "certified refusal actuated the app"
    );
}

fn build_scene_fixture(dir: &Path) -> PathBuf {
    let xml = std::env::var_os("CUA_TEST_XDG_SHELL_XML")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml")
        });
    assert!(xml.is_file(), "xdg-shell protocol source missing: {xml:?}");
    let header = dir.join("xdg-shell-client-protocol.h");
    let protocol = dir.join("xdg-shell-protocol.c");
    for (mode, output) in [("client-header", &header), ("private-code", &protocol)] {
        let result = Command::new("wayland-scanner")
            .arg(mode)
            .arg(&xml)
            .arg(output)
            .output()
            .expect("wayland-scanner needed for real subsurface fixture");
        assert!(
            result.status.success(),
            "generate xdg-shell {mode}: {result:?}"
        );
    }
    let viewport_xml =
        PathBuf::from("/usr/share/wayland-protocols/stable/viewporter/viewporter.xml");
    assert!(
        viewport_xml.is_file(),
        "viewporter protocol source missing: {viewport_xml:?}"
    );
    let viewport_header = dir.join("viewporter-client-protocol.h");
    let viewport_protocol = dir.join("viewporter-protocol.c");
    for (mode, output) in [
        ("client-header", &viewport_header),
        ("private-code", &viewport_protocol),
    ] {
        let result = Command::new("wayland-scanner")
            .arg(mode)
            .arg(&viewport_xml)
            .arg(output)
            .output()
            .expect("wayland-scanner needed for real viewport reassertion");
        assert!(
            result.status.success(),
            "generate viewporter {mode}: {result:?}"
        );
    }
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "wayland-client"])
        .output()
        .expect("wayland-client development files needed");
    assert!(
        flags.status.success(),
        "wayland-client pkg-config failed: {flags:?}"
    );
    let fixture = workspace_root()
        .join("../tests/fixtures/apps/linux/isolated-input/scene_revision_fixture.c");
    let binary = dir.join("scene-revision-fixture");
    let result = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(dir)
        .arg(fixture)
        .arg(protocol)
        .arg(viewport_protocol)
        .args(String::from_utf8(flags.stdout).unwrap().split_whitespace())
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile real Wayland subsurface fixture");
    assert!(
        result.status.success(),
        "compile Wayland scene fixture: {result:?}"
    );
    binary
}

fn wait_scene_window(driver: &mut McpDriver, pid: u32, journal: &Path) -> TargetWindow {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let windows = driver.call("list_windows", json!({"pid": pid}));
        if let Some(id) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows.iter().find(|window| {
                    window["pid"].as_u64() == Some(u64::from(pid))
                        && window["title"].as_str() == Some("Cua Scene Revision Fixture")
                })
            })
            .and_then(|window| window["window_id"].as_u64())
        {
            if journal.is_file() && events(journal).iter().any(|event| event["kind"] == "ready") {
                let clients = hyprctl(&["-j", "clients"]);
                let client = clients
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|client| {
                        client["pid"].as_u64() == Some(u64::from(pid))
                            && client["title"] == "Cua Scene Revision Fixture"
                    })
                    .expect("independent compositor scene fixture identity");
                assert_eq!(client["xwayland"], false);
                assert_eq!(
                    u64::from_str_radix(
                        client["address"].as_str().unwrap().trim_start_matches("0x"),
                        16
                    )
                    .unwrap(),
                    id
                );
                return TargetWindow { pid, native_id: id };
            }
        }
        assert!(
            Instant::now() < deadline,
            "scene fixture failed to map: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn desktop_scene(driver: &mut McpDriver, dir: &Path, label: &str) -> ToolResponse {
    let state = driver.call(
        "get_desktop_state",
        json!({
            "screenshot_out_file": dir.join(format!("{label}.png")),
        }),
    );
    assert!(
        !state.is_error(),
        "desktop scene capture {label}: {}",
        state.text()
    );
    assert_eq!(
        state.structured()["scene"]["surface_tree_available"],
        true,
        "candidate must publish compositor-attested scene inventory"
    );
    assert!(
        state.structured()["capture_id"].as_str().is_some(),
        "scene image must have one-shot capture binding"
    );
    state
}

fn scene_child_point(state: &ToolResponse, target: TargetWindow) -> (f64, f64) {
    let surfaces = state.structured()["scene"]["captured_surfaces"]
        .as_array()
        .expect("compositor scene inventory");
    let mut rows: Vec<_> = surfaces
        .iter()
        .filter(|row| {
            row["pid"].as_u64() == Some(u64::from(target.pid))
                && row["window_id"].as_u64() == Some(target.native_id)
                && row["popup"] == false
        })
        .collect();
    rows.sort_by(|a, b| {
        let area = |row: &Value| {
            row["screenshot_bounds"]["width"].as_f64().unwrap()
                * row["screenshot_bounds"]["height"].as_f64().unwrap()
        };
        area(a).total_cmp(&area(b))
    });
    assert!(
        rows.len() >= 3,
        "root plus two real subsurfaces required: {surfaces:?}"
    );
    let box_ = &rows[0]["screenshot_bounds"];
    let x = box_["x"].as_f64().unwrap() + box_["width"].as_f64().unwrap() / 2.0;
    let y = box_["y"].as_f64().unwrap() + box_["height"].as_f64().unwrap() / 2.0;
    let width = state.structured()["screenshot_width"].as_f64().unwrap();
    let height = state.structured()["screenshot_height"].as_f64().unwrap();
    assert!(x >= 0.0 && x < width && y >= 0.0 && y < height);
    (x, y)
}

#[test]
#[ignore = "candidate compositor/plugin shared tree revision and live native Wayland subsurfaces"]
fn compositor_scene_revision_tracks_real_child_geometry_stack_and_input_region() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let binary = build_scene_fixture(&dir);
    let journal = dir.join("scene-mutations.jsonl");
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-scene-revision",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let mut child = spawn_in_job(
        Command::new(binary)
            .arg(&journal)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .expect("launch real wl_subsurface/xdg_shell fixture");
    let mut control = child.stdin.take().unwrap();
    let target = wait_scene_window(&mut driver, child.id(), &journal);
    driver.reaper().push(child);
    // Keep primary focus on a distinct private window, without covering the
    // scene surface in the desktop capture. No physical desktop is involved.
    let foreground = launch(&mut driver, &dir, "Sibling");
    let selector =
        serde_json::to_string(&format!("address:0x{:x}", foreground.target.native_id)).unwrap();
    let action = format!("hl.dsp.focus({{ window = {selector} }})");
    let focus_result = Command::new("hyprctl")
        .args(["dispatch", &action])
        .output()
        .unwrap();
    assert!(focus_result.status.success() && focus_result.stdout == b"ok\n");
    let expected_focus = format!("0x{:x}", foreground.target.native_id);
    assert_eq!(focus(), expected_focus);
    driver.start_behavior_recording();
    let mut evidence = std::fs::File::create(dir.join("scene-revisions.jsonl")).unwrap();
    for (index, (command, event)) in [
        ("MOVE", "move"),
        ("STACK", "stack"),
        ("REGION", "input-region"),
        ("REASSERT", "reassert-unchanged"),
        ("REPAINT", "pixel-only-repaint"),
        ("VIEWPORT", "viewport-changed"),
        ("VIEWPORT_RESET", "viewport-restored"),
    ]
    .into_iter()
    .enumerate()
    {
        let before = desktop_scene(&mut driver, &dir, &format!("scene-before-{index}"));
        let revision = before.structured()["scene"]["scene_revision"]
            .as_u64()
            .expect("compositor-origin monotonic revision");
        let epoch = before.structured()["scene"]["scene_epoch"]
            .as_str()
            .unwrap()
            .to_owned();
        let old_capture = before.structured()["capture_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let from = scene_child_point(&before, target);
        let events_before = events(&journal).len();
        control
            .write_all(format!("{command}\n").as_bytes())
            .unwrap();
        control.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !events(&journal)[events_before..]
            .iter()
            .any(|value| value["kind"] == event)
        {
            assert!(
                Instant::now() < deadline,
                "compositor-side {command} did not complete"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let after = desktop_scene(&mut driver, &dir, &format!("scene-after-{index}"));
        let revised = after.structured()["scene"]["scene_revision"]
            .as_u64()
            .unwrap();
        assert_eq!(after.structured()["scene"]["scene_epoch"], epoch);
        if command == "REASSERT" || command == "REPAINT" {
            assert_eq!(
                revised, revision,
                "same committed state/damage invalidated scene topology"
            );
            let usable = driver.call(
                "click",
                json!({
                    "pid": target.pid, "window_id": target.native_id,
                    "coordinate_frame": "desktop", "capture_id": old_capture,
                    "x": from.0, "y": from.1, "delivery_mode": "background",
                }),
            );
            assert!(
                !usable.is_error(),
                "damage-only repaint must preserve the prior capture's action frame: {}",
                usable.raw
            );
            assert_ne!(usable.structured()["effect"], "refused", "{}", usable.raw);
        } else {
            assert!(revised > revision,
                "plugin-local counter failed to observe compositor-side {event}: {revision} -> {revised}");
            let refusal = driver.call(
                "click",
                json!({
                    "pid": target.pid, "window_id": target.native_id,
                    "coordinate_frame": "desktop", "capture_id": old_capture,
                    "x": from.0, "y": from.1, "delivery_mode": "background",
                }),
            );
            assert!(
                refusal.is_error(),
                "old scene must refuse before dispatch: {}",
                refusal.raw
            );
            assert_eq!(
                refusal.structured()["code"],
                "capture_frame_mismatch",
                "{}",
                refusal.raw
            );
            assert_eq!(refusal.structured()["effect"], "refused", "{}", refusal.raw);
        }
        writeln!(
            evidence,
            "{}",
            json!({
                "command":command,"old_revision":revision,"new_revision":revised,
                "epoch":epoch,"old_capture_id":old_capture,
            })
        )
        .unwrap();
    }
    drop(control);
}

fn build_primary_ime_fixture(dir: &Path) -> PathBuf {
    let protocols = [
        (
            "xdg-shell",
            std::env::var_os("CUA_TEST_XDG_SHELL_XML")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from("/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml")
                }),
        ),
        (
            "text-input-unstable-v1",
            PathBuf::from(
                "/usr/share/wayland-protocols/unstable/text-input/text-input-unstable-v1.xml",
            ),
        ),
        (
            "text-input-unstable-v3",
            PathBuf::from(
                "/usr/share/wayland-protocols/unstable/text-input/text-input-unstable-v3.xml",
            ),
        ),
        (
            "input-method-unstable-v2",
            std::env::var_os("CUA_TEST_INPUT_METHOD_V2_XML")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    PathBuf::from(
                        "/home/joseph/Projects/hyprland-cua/protocols/input-method-unstable-v2.xml",
                    )
                }),
        ),
    ];
    let mut generated = Vec::new();
    for (name, xml) in protocols {
        assert!(xml.is_file(), "real Wayland protocol XML missing: {xml:?}");
        for (mode, suffix) in [
            ("client-header", "client-protocol.h"),
            ("private-code", "protocol.c"),
        ] {
            let output = dir.join(format!("{name}-{suffix}"));
            let result = Command::new("wayland-scanner")
                .args([mode])
                .arg(&xml)
                .arg(&output)
                .output()
                .expect("wayland-scanner for physical primary IME protocol");
            assert!(result.status.success(), "scan {name} {mode}: {result:?}");
            if suffix == "protocol.c" {
                generated.push(output);
            }
        }
    }
    let flags = Command::new("pkg-config")
        .args(["--cflags", "--libs", "wayland-client"])
        .output()
        .expect("wayland-client development files");
    assert!(
        flags.status.success(),
        "wayland-client pkg-config: {flags:?}"
    );
    let fixture = workspace_root()
        .join("../tests/fixtures/apps/linux/isolated-input/primary_text_input_fixture.c");
    let binary = dir.join("primary-text-input-fixture");
    let result = Command::new("cc")
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
        .arg("-I")
        .arg(dir)
        .arg(fixture)
        .args(&generated)
        .args(String::from_utf8(flags.stdout).unwrap().split_whitespace())
        .arg("-o")
        .arg(&binary)
        .output()
        .expect("compile real physical-seat text-input and IME clients");
    assert!(
        result.status.success(),
        "compile physical-seat IME fixture: {result:?}"
    );
    binary
}

fn primary_command(control: &mut impl std::io::Write, journal: &Path, command: &str) -> Vec<Value> {
    let before = events(journal).len();
    writeln!(control, "{command}").unwrap();
    control.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let all = events(journal);
        if all[before..].iter().any(|event| {
            event["source"] == "fixture"
                && event["kind"] == "command-complete"
                && event["text"] == command
        }) {
            return all[before..].to_vec();
        }
        assert!(
            Instant::now() < deadline,
            "primary text-input {command} timed out: {all:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn assert_wire_event(events: &[Value], source: &str, kind: &str) {
    assert!(
        events
            .iter()
            .any(|event| event["source"] == source && event["kind"] == kind),
        "missing {source} {kind} protocol event: {events:?}"
    );
}

fn focus_address(id: u64) {
    let selector = serde_json::to_string(&format!("address:0x{id:x}")).unwrap();
    let action = format!("hl.dsp.focus({{ window = {selector} }})");
    let output = Command::new("hyprctl")
        .args(["dispatch", &action])
        .output()
        .unwrap();
    assert!(
        output.status.success() && output.stdout == b"ok\n",
        "{output:?}"
    );
    assert_eq!(focus(), format!("0x{id:x}"));
}

fn primary_text_input_wire(version: &str) {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let binary = build_primary_ime_fixture(&dir);
    let journal = dir.join(format!("primary-{version}.jsonl"));
    let mut driver = McpDriver::spawn_named_with_env(
        &format!("primary-ime-{version}"),
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let sibling = launch(&mut driver, &dir, "Sibling");
    let mut child = spawn_in_job(
        Command::new(binary)
            .args([version])
            .arg(&journal)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .expect("launch real primary text-input/IME clients");
    let pid = child.id();
    let mut control = child.stdin.take().unwrap();
    let title = format!("Cua Primary Text Input {version}");
    let deadline = Instant::now() + Duration::from_secs(15);
    let target_id = loop {
        let windows = driver.call("list_windows", json!({"pid": pid}));
        if let Some(id) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| {
                        window["pid"].as_u64() == Some(u64::from(pid)) && window["title"] == title
                    })
                    .and_then(|window| window["window_id"].as_u64())
            })
        {
            if journal.is_file()
                && events(&journal)
                    .iter()
                    .any(|event| event["kind"] == "ready")
            {
                break id;
            }
        }
        assert!(
            Instant::now() < deadline,
            "physical-seat fixture failed to map: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    };
    driver.reaper().push(child);
    let compositor_client = hyprctl(&["-j", "clients"])
        .as_array()
        .unwrap()
        .iter()
        .find(|client| client["pid"].as_u64() == Some(u64::from(pid)) && client["title"] == title)
        .cloned()
        .expect("independent compositor primary IME client");
    assert_eq!(compositor_client["xwayland"], false);
    assert_eq!(
        u64::from_str_radix(
            compositor_client["address"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
            16
        )
        .unwrap(),
        target_id
    );
    focus_address(target_id);
    let focused = primary_command(&mut control, &journal, "SYNC");
    if version == "v3" {
        assert_wire_event(&focused, "client", "enter");
    }
    let activated = primary_command(&mut control, &journal, "ACTIVATE");
    if version == "v1" {
        assert_wire_event(&activated, "client", "enter");
    }
    assert_wire_event(&activated, "ime", "activate");
    assert_wire_event(&activated, "ime", "done");
    let committed = primary_command(&mut control, &journal, "COMMIT");
    assert_wire_event(&committed, "ime", "done");
    let first = primary_command(&mut control, &journal, "SEND");
    assert!(
        first.iter().any(|event| event["source"] == "client"
            && event["kind"] == "commit-string"
            && event["text"] == "Cua primary — 漢字"
            && (version == "v3" || event["serial"] == 1)),
        "real primary application did not receive correctly serialized Unicode: {first:?}"
    );
    if version == "v3" {
        assert!(
            first.iter().any(|event| event["source"] == "client"
                && event["kind"] == "done"
                && event["serial"] == 2),
            "v3 done must carry latest client commit counter: {first:?}"
        );
    }
    let stale = primary_command(&mut control, &journal, "STALE");
    assert!(
        !stale
            .iter()
            .any(|event| event["source"] == "client" && event["kind"] == "commit-string"),
        "stale input-method-v2 commit was delivered to primary app: {stale:?}"
    );
    let disabled = primary_command(&mut control, &journal, "DISABLE");
    assert_wire_event(&disabled, "ime", "deactivate");
    assert_wire_event(&disabled, "ime", "done");
    if version == "v1" {
        assert_wire_event(&disabled, "client", "leave");
    }
    let reactivated = primary_command(&mut control, &journal, "REACTIVATE");
    assert_wire_event(&reactivated, "ime", "activate");
    assert_wire_event(&reactivated, "ime", "done");
    if version == "v1" {
        assert_wire_event(&reactivated, "client", "enter");
    }
    primary_command(&mut control, &journal, "COMMIT");
    let second = primary_command(&mut control, &journal, "SEND");
    assert!(
        second.iter().any(|event| event["source"] == "client"
            && event["kind"] == "commit-string"
            && event["text"] == "Cua primary — 漢字"
            && (version == "v3" || event["serial"] == 2)),
        "reactivated primary text client lost IME delivery: {second:?}"
    );
    focus_address(sibling.target.native_id);
    let blurred = primary_command(&mut control, &journal, "SYNC");
    assert_wire_event(&blurred, "client", "leave");
    assert_wire_event(&blurred, "ime", "deactivate");
    focus_address(target_id);
    let refocused = primary_command(&mut control, &journal, "SYNC");
    assert_wire_event(&refocused, "client", "enter");
    if version == "v1" {
        assert_wire_event(&refocused, "ime", "activate");
    } else {
        assert!(
            !refocused
                .iter()
                .any(|event| event["source"] == "ime" && event["kind"] == "activate"),
            "v3 enabled state survived leave/enter without fresh commit: {refocused:?}"
        );
        let enabled = primary_command(&mut control, &journal, "REACTIVATE");
        assert_wire_event(&enabled, "ime", "activate");
    }
    primary_command(&mut control, &journal, "COMMIT");
    let third = primary_command(&mut control, &journal, "SEND");
    assert!(
        third.iter().any(|event| event["source"] == "client"
            && event["kind"] == "commit-string"
            && event["text"] == "Cua primary — 漢字"),
        "blur/refocus lost fresh physical IME delivery: {third:?}"
    );
    drop(control);
}

#[test]
#[ignore = "candidate private compositor real primary wl_seat v1/IME-v2 wire regression"]
fn primary_text_input_v1_activation_commit_deactivation_and_refocus() {
    primary_text_input_wire("v1");
}

#[test]
#[ignore = "candidate private compositor real primary wl_seat v3/IME-v2 wire regression"]
fn primary_text_input_v3_enable_commit_disable_and_refocus() {
    primary_text_input_wire("v3");
}

fn popup_point(state: &ToolResponse, target: TargetWindow) -> (f64, f64) {
    let rows = state.structured()["scene"]["captured_surfaces"]
        .as_array()
        .expect("captured compositor popup inventory");
    let popup = rows
        .iter()
        .find(|row| {
            row["pid"].as_u64() == Some(u64::from(target.pid))
                && row["window_id"].as_u64() == Some(target.native_id)
                && row["popup"] == true
        })
        .unwrap_or_else(|| panic!("visible GTK popover absent from attested scene: {rows:?}"));
    let bounds = &popup["screenshot_bounds"];
    let x = bounds["x"].as_f64().unwrap() + bounds["width"].as_f64().unwrap() / 2.0;
    let y = bounds["y"].as_f64().unwrap() + bounds["height"].as_f64().unwrap() / 2.0;
    assert!(
        x >= 0.0
            && y >= 0.0
            && x < state.structured()["screenshot_width"].as_f64().unwrap()
            && y < state.structured()["screenshot_height"].as_f64().unwrap(),
        "popup button center outside the actual captured desktop frame"
    );
    (x, y)
}

#[test]
#[ignore = "candidate compositor real GTK popover attested tree capture and remap refusal"]
fn native_popup_requires_exact_captured_surface_and_fresh_revision() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut driver = McpDriver::spawn_named_with_env(
        "omarchy-popup-capture",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let target = launch(&mut driver, &dir, "Target");
    let state = snapshot(&mut driver, &target);
    let anchor = point(&mut driver, &target, &state, "native-popup-anchor");
    let sentinel = ForegroundSentinel::launch(&mut driver);
    driver.start_behavior_recording();
    sentinel
        .prepare_background_observation(&mut driver, target.target)
        .unwrap();
    let old = desktop_scene(&mut driver, &dir, "popup-closed");
    let before = events(&target.journal).len();
    let original_cursor = cursor();
    let focused = focus();
    let watch = PrimarySeatWatch::start(dir.join("popup-primary.json"));
    let (opened, _) = sentinel
        .observe_background(target.target, || {
            driver.call(
                "click",
                json!({
                    "pid": target.target.pid, "window_id": target.target.native_id,
                    "x": anchor.0, "y": anchor.1, "delivery_mode": "background",
                }),
            )
        })
        .expect("opening popup must not disturb physical seat");
    assert!(
        !opened.is_error(),
        "actual popup anchor refused: {}",
        opened.raw
    );
    assert!(events(&target.journal)[before..]
        .iter()
        .any(|event| event["kind"] == "popup-opened"));
    let visible = desktop_scene(&mut driver, &dir, "popup-open");
    let popup = popup_point(&visible, target.target);
    assert!(
        visible.structured()["scene"]["scene_revision"]
            .as_u64()
            .unwrap()
            > old.structured()["scene"]["scene_revision"]
                .as_u64()
                .unwrap(),
        "compositor popup mapping did not advance scene revision"
    );
    let obsolete = driver.call(
        "click",
        json!({
            "pid": target.target.pid, "window_id": target.target.native_id,
            "coordinate_frame": "desktop", "capture_id": old.structured()["capture_id"],
            "x": popup.0, "y": popup.1, "delivery_mode": "background",
        }),
    );
    assert!(
        obsolete.is_error(),
        "uncaptured popup action succeeded: {}",
        obsolete.raw
    );
    assert_eq!(
        obsolete.structured()["code"],
        "capture_frame_mismatch",
        "{}",
        obsolete.raw
    );
    let activated_before = events(&target.journal)
        .iter()
        .filter(|event| event["kind"] == "popup-activated")
        .count();
    let (clicked, _) =
        sentinel
            .observe_background(target.target, || {
                driver.call("click", json!({
        "pid": target.target.pid, "window_id": target.target.native_id,
        "coordinate_frame": "desktop", "capture_id": visible.structured()["capture_id"],
        "x": popup.0, "y": popup.1, "delivery_mode": "background",
    }))
            })
            .expect("captured exact popup action must preserve physical seat");
    assert!(
        !clicked.is_error(),
        "captured visible popup refused: {}",
        clicked.raw
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while events(&target.journal)
        .iter()
        .filter(|event| event["kind"] == "popup-activated")
        .count()
        <= activated_before
    {
        assert!(
            Instant::now() < deadline,
            "app-owned GTK popup button did not activate"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        events(&target.journal)
            .iter()
            .filter(|event| event["kind"] == "popup-activated")
            .count(),
        activated_before + 1
    );
    let dismissed = desktop_scene(&mut driver, &dir, "popup-dismissed");
    assert!(
        dismissed.structured()["scene"]["scene_revision"]
            .as_u64()
            .unwrap()
            > visible.structured()["scene"]["scene_revision"]
                .as_u64()
                .unwrap(),
        "popup destruction did not advance compositor revision"
    );
    let stale = driver.call(
        "click",
        json!({
            "pid": target.target.pid, "window_id": target.target.native_id,
            "coordinate_frame": "desktop", "capture_id": visible.structured()["capture_id"],
            "x": popup.0, "y": popup.1, "delivery_mode": "background",
        }),
    );
    assert!(
        stale.is_error(),
        "dismissed popup capture hit replacement/base: {}",
        stale.raw
    );
    assert_eq!(
        stale.structured()["code"],
        "capture_frame_mismatch",
        "{}",
        stale.raw
    );
    assert_stable(&watch.finish(), original_cursor, &focused);
}
