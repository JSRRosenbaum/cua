//! Real Wayland protocol acceptance for the plugin's synthetic agent seat.
//! Imported by hyprland_omarchy_acceptance_test; never run on the physical desktop.

use super::*;
use cua_driver_testkit::ChildReaper;
use std::process::ChildStdin;

const AGENT_TEXT: &str = "Cua agent — 漢字";

struct AgentFixture {
    driver: McpDriver,
    _sibling_driver: McpDriver,
    fixture_reaper: ChildReaper,
    binary: PathBuf,
    journal: PathBuf,
    control: ChildStdin,
    target_id: u64,
    sibling_id: u64,
    pid: u32,
}

fn agent_fixture(dir: &Path, label: &str) -> AgentFixture {
    let binary = build_primary_ime_fixture(dir);
    let journal = dir.join(format!("{label}.jsonl"));
    let mut driver = McpDriver::spawn_named_with_env(
        label,
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("candidate Driver required");
    let mut sibling_driver = McpDriver::spawn_named_with_env(
        &format!("{label}-sibling"),
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("independent private sibling Driver required");
    let sibling = launch(&mut sibling_driver, dir, "Sibling");
    let mut child = spawn_in_job(
        Command::new(&binary)
            .arg("agent-v3")
            .arg(&journal)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .expect("launch real synthetic-seat Wayland fixture");
    let pid = child.id();
    let control = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let target_id = loop {
        let windows = driver.call("list_windows", json!({"pid": pid}));
        if let Some(id) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| {
                        window["pid"].as_u64() == Some(u64::from(pid))
                            && window["title"] == "Cua Primary Text Input agent-v3"
                    })
                    .and_then(|window| window["window_id"].as_u64())
            })
        {
            if journal.is_file()
                && events(&journal)
                    .iter()
                    .any(|event| event["source"] == "fixture" && event["kind"] == "ready")
            {
                break id;
            }
        }
        assert!(
            Instant::now() < deadline,
            "synthetic-seat fixture failed to map: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    };
    let mut fixture_reaper = ChildReaper::new();
    fixture_reaper.push(child);
    let compositor_client = hyprctl(&["-j", "clients"])
        .as_array()
        .unwrap()
        .iter()
        .find(|client| {
            client["pid"].as_u64() == Some(u64::from(pid))
                && client["title"] == "Cua Primary Text Input agent-v3"
        })
        .cloned()
        .expect("independent real Wayland application client");
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
    focus_address(sibling.target.native_id);
    AgentFixture {
        driver,
        _sibling_driver: sibling_driver,
        fixture_reaper,
        binary,
        journal,
        control,
        target_id,
        sibling_id: sibling.target.native_id,
        pid,
    }
}

fn agent_commit(fixture: &mut AgentFixture) -> (ToolResponse, Vec<Value>) {
    let before = events(&fixture.journal).len();
    let result = fixture.driver.call(
        "type_text",
        json!({
            "pid": fixture.pid, "window_id": fixture.target_id,
            "text": AGENT_TEXT, "composition_mode": "commit", "delivery_mode": "background",
        }),
    );
    // The fixture pumps both real Wayland connections while the synchronous
    // Driver call is outstanding; these events are the app's own wire oracle.
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    (result, events(&fixture.journal)[before..].to_vec())
}

struct KeyboardSibling {
    journal: PathBuf,
    control: ChildStdin,
    pid: u32,
    target_id: u64,
}

fn keyboard_sibling(fixture: &mut AgentFixture, dir: &Path) -> KeyboardSibling {
    let journal = dir.join("keyboard-sibling.jsonl");
    let mut child = spawn_in_job(
        Command::new(&fixture.binary)
            .arg("keyboard-sibling")
            .arg(&journal)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit()),
    )
    .expect("launch separate real Wayland keyboard client");
    let pid = child.id();
    let control = child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let target_id = loop {
        let windows = fixture.driver.call("list_windows", json!({"pid": pid}));
        if let Some(id) = windows.structured()["windows"]
            .as_array()
            .and_then(|windows| {
                windows
                    .iter()
                    .find(|window| {
                        window["pid"].as_u64() == Some(u64::from(pid))
                            && window["title"] == "Cua Agent Keyboard Sibling"
                    })
                    .and_then(|window| window["window_id"].as_u64())
            })
        {
            if journal.is_file()
                && events(&journal)
                    .iter()
                    .any(|event| event["source"] == "fixture" && event["kind"] == "ready")
            {
                break id;
            }
        }
        assert!(
            Instant::now() < deadline,
            "independent keyboard fixture not mapped: {}",
            windows.text()
        );
        thread::sleep(Duration::from_millis(50));
    };
    fixture.fixture_reaper.push(child);
    focus_address(fixture.sibling_id);
    KeyboardSibling {
        journal,
        control,
        pid,
        target_id,
    }
}

fn has_keyboard_activity(events: &[Value], source: &str) -> bool {
    events.iter().any(|event| {
        event["source"] == source
            && matches!(
                event["kind"].as_str(),
                Some("enter" | "leave" | "key" | "modifiers")
            )
    })
}

fn assert_no_agent_text(events: &[Value]) {
    assert!(
        !events
            .iter()
            .any(|event| event["source"] == "client" && event["kind"] == "commit-string"),
        "refused agent action reached the application: {events:?}"
    );
}

fn primary_clipboard() -> (Option<i32>, Vec<u8>) {
    let output = Command::new("timeout")
        .args([
            "5s",
            "wl-paste",
            "--no-newline",
            "--type",
            "text/plain;charset=utf-8",
        ])
        .output()
        .expect("private compositor requires wl-paste and timeout");
    assert!(
        matches!(output.status.code(), Some(0 | 1)),
        "private primary clipboard probe failed: {output:?}"
    );
    (output.status.code(), output.stdout)
}

fn wait_client_event(
    control: &mut ChildStdin,
    journal: &Path,
    before: usize,
    kind: &str,
    text: Option<&str>,
) -> Vec<Value> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        primary_command(control, journal, "SYNC");
        let all = events(journal);
        if all[before..].iter().any(|event| {
            event["source"] == "client"
                && event["kind"] == kind
                && text.is_none_or(|expected| event["text"] == expected)
        }) {
            return all[before..].to_vec();
        }
        assert!(
            Instant::now() < deadline,
            "missing real Wayland {kind} {text:?}: {all:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn paste(fixture: &mut AgentFixture, session: &str) -> ToolResponse {
    fixture.driver.call(
        "press_key",
        json!({
            "session": session, "pid": fixture.pid, "window_id": fixture.target_id,
            "key": "v", "modifiers": ["ctrl"], "delivery_mode": "background",
        }),
    )
}

#[test]
#[ignore = "disposable candidate compositor with real synthetic wl_seat v3 and seat-text handshake"]
fn agent_v3_blur_requires_fresh_enable_commit() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut fixture = agent_fixture(&dir, "agent-v3-blur");
    let physical_focus = focus();
    assert_eq!(physical_focus, format!("0x{:x}", fixture.sibling_id));
    let original_cursor = cursor();
    let watch = PrimarySeatWatch::start(dir.join("agent-v3-primary.json"));

    // An enable committed before enter belongs to the previous focus epoch.
    primary_command(&mut fixture.control, &fixture.journal, "ENABLE");
    let (stale, stale_events) = agent_commit(&mut fixture);
    assert!(
        stale.is_error(),
        "pre-enter enabled state was accepted: {}",
        stale.text()
    );
    assert_eq!(
        stale.structured()["reason"],
        "text_input_unavailable",
        "{}",
        stale.raw
    );
    assert_eq!(stale.structured()["effect"], "refused", "{}", stale.raw);
    assert_wire_event(&stale_events, "client", "enter");
    assert_wire_event(&stale_events, "client", "leave");
    assert_no_agent_text(&stale_events);
    assert_eq!(
        focus(),
        physical_focus,
        "synthetic text stole physical focus"
    );

    // The application itself now answers enter with a fresh enable+commit.
    primary_command(&mut fixture.control, &fixture.journal, "AUTO_ENABLE");
    let (fresh, fresh_events) = agent_commit(&mut fixture);
    assert!(
        !fresh.is_error(),
        "freshly enabled text input was refused: {}",
        fresh.text()
    );
    assert_eq!(
        fresh.structured()["native_route"],
        "seat_text_input",
        "{}",
        fresh.raw
    );
    assert!(
        fresh_events.iter().any(|event| event["source"] == "client"
            && event["kind"] == "commit-string"
            && event["text"] == AGENT_TEXT),
        "actual synthetic-seat app did not receive Unicode commit: {fresh_events:?}"
    );
    assert_wire_event(&fresh_events, "client", "leave");
    assert_eq!(focus(), physical_focus);

    // A successful action also blurs the seat; its previous enabled state
    // cannot authorize a later enter without the application's fresh commit.
    primary_command(&mut fixture.control, &fixture.journal, "AUTO_DISABLE");
    let (blurred, blurred_events) = agent_commit(&mut fixture);
    assert!(
        blurred.is_error(),
        "enabled state survived synthetic blur: {}",
        blurred.text()
    );
    assert_eq!(
        blurred.structured()["reason"],
        "text_input_unavailable",
        "{}",
        blurred.raw
    );
    assert_wire_event(&blurred_events, "client", "enter");
    assert_no_agent_text(&blurred_events);
    assert_eq!(focus(), physical_focus);

    primary_command(&mut fixture.control, &fixture.journal, "AUTO_ENABLE");
    let (reenabled, events) = agent_commit(&mut fixture);
    assert!(
        !reenabled.is_error(),
        "fresh re-enable did not restore delivery: {}",
        reenabled.text()
    );
    assert!(
        events.iter().any(|event| event["source"] == "client"
            && event["kind"] == "commit-string"
            && event["text"] == AGENT_TEXT),
        "app did not receive re-enabled commit: {events:?}"
    );
    assert_eq!(focus(), physical_focus);
    assert_stable(&watch.finish(), original_cursor, &physical_focus);
}

#[test]
#[ignore = "disposable candidate compositor with real unavailable IME-v2 child resources on synthetic seat"]
fn unavailable_agent_ime_children_are_inert_and_do_not_disconnect_physical_ime() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut fixture = agent_fixture(&dir, "agent-ime-inert");
    let pipeline = primary_command(
        &mut fixture.control,
        &fixture.journal,
        "UNAVAILABLE_PIPELINE",
    );
    assert_wire_event(&pipeline, "agent-ime", "unavailable");
    assert!(
        !pipeline
            .iter()
            .any(|event| event["source"] == "agent-grab" || event["source"] == "agent-popup"),
        "unavailable IME exposed physical keymap, grab, or popup: {pipeline:?}"
    );

    // Keep both unavailable child IDs alive while the physical IME activates,
    // then release them and prove the same connection still delivers text.
    focus_address(fixture.target_id);
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    let active = primary_command(&mut fixture.control, &fixture.journal, "ACTIVATE");
    assert_wire_event(&active, "ime", "activate");
    assert_wire_event(&active, "ime", "done");
    primary_command(&mut fixture.control, &fixture.journal, "PRIMARY_COMMIT");
    let sent = primary_command(&mut fixture.control, &fixture.journal, "SEND");
    assert!(
        sent.iter().any(|event| event["source"] == "primary-client"
            && event["kind"] == "commit-string"
            && event["text"] == "Cua primary — 漢字"),
        "inert synthetic IME children disconnected or contaminated physical IME: {sent:?}"
    );
    let while_live = events(&fixture.journal);
    assert!(
        !while_live
            .iter()
            .any(|event| event["source"] == "agent-grab"
                || event["source"] == "agent-popup"
                || (event["source"] == "agent-ime" && event["kind"] != "unavailable")),
        "unavailable synthetic IME observed primary-seat state: {while_live:?}"
    );
    primary_command(
        &mut fixture.control,
        &fixture.journal,
        "RELEASE_UNAVAILABLE_CHILDREN",
    );
    primary_command(&mut fixture.control, &fixture.journal, "PRIMARY_COMMIT");
    let after_release = primary_command(&mut fixture.control, &fixture.journal, "SEND");
    assert!(
        after_release
            .iter()
            .any(|event| event["source"] == "primary-client"
                && event["kind"] == "commit-string"
                && event["text"] == "Cua primary — 漢字"),
        "releasing inert children disconnected physical IME: {after_release:?}"
    );
}

#[test]
#[ignore = "disposable candidate compositor, two real synthetic wl_keyboard clients and private physical-seat observer"]
fn agent_keyboard_events_remain_target_scoped_and_release_disables_delivery() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut fixture = agent_fixture(&dir, "agent-keyboard-scope");
    let mut sibling = keyboard_sibling(&mut fixture, &dir);
    let physical_focus = focus();
    let original_cursor = cursor();
    let watch = PrimarySeatWatch::start(dir.join("agent-keyboard-primary.json"));
    let target_before = events(&fixture.journal).len();
    let sibling_before = events(&sibling.journal).len();
    let key = fixture.driver.call(
        "press_key",
        json!({
            "pid": fixture.pid, "window_id": fixture.target_id,
            "key": "a", "modifiers": ["shift"], "delivery_mode": "background",
        }),
    );
    assert!(
        !key.is_error(),
        "exact native key action refused: {}",
        key.text()
    );
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    primary_command(&mut sibling.control, &sibling.journal, "SYNC");
    let target = events(&fixture.journal);
    let delivered = &target[target_before..];
    assert_wire_event(delivered, "agent-keyboard", "enter");
    assert_wire_event(delivered, "agent-keyboard", "leave");
    for (code, state) in [(42, 1), (30, 1), (30, 0), (42, 0)] {
        assert!(
            delivered
                .iter()
                .any(|event| event["source"] == "agent-keyboard"
                    && event["kind"] == "key"
                    && event["key"] == code
                    && event["state"] == state),
            "target application missed a physical evdev key phase ({code}, {state}): {delivered:?}"
        );
    }
    assert!(
        delivered
            .iter()
            .any(|event| event["source"] == "agent-keyboard"
                && event["kind"] == "modifiers"
                && event["depressed"].as_u64().unwrap_or(0) != 0),
        "target did not receive held Shift state: {delivered:?}"
    );
    assert!(
        !has_keyboard_activity(delivered, "primary-keyboard"),
        "primary keyboard received synthetic key phases: {delivered:?}"
    );
    let other = events(&sibling.journal);
    assert!(
        !has_keyboard_activity(&other[sibling_before..], "agent-keyboard"),
        "separate same-seat Wayland client received unowned keyboard events: {other:?}"
    );

    primary_command(
        &mut fixture.control,
        &fixture.journal,
        "RELEASE_AGENT_KEYBOARD",
    );
    let before_refusal = events(&fixture.journal).len();
    let refused = fixture.driver.call(
        "press_key",
        json!({
            "pid": fixture.pid, "window_id": fixture.target_id,
            "key": "a", "delivery_mode": "background",
        }),
    );
    assert!(
        refused.is_error(),
        "released keyboard still delivered keys: {}",
        refused.text()
    );
    assert_eq!(
        refused.structured()["reason"],
        "client_not_bound",
        "{}",
        refused.raw
    );
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    primary_command(&mut sibling.control, &sibling.journal, "SYNC");
    assert!(
        events(&fixture.journal)[before_refusal..]
            .iter()
            .all(|event| { event["source"] != "agent-keyboard" || event["kind"] != "key" }),
        "released keyboard received a key"
    );
    assert!(
        !has_keyboard_activity(
            &events(&sibling.journal)[sibling_before..],
            "agent-keyboard"
        ),
        "released target rerouted key to sibling"
    );

    primary_command(&mut fixture.control, &fixture.journal, "UNMAP");
    let before_unmapped = events(&fixture.journal).len();
    let unmapped = fixture.driver.call(
        "press_key",
        json!({
            "pid": fixture.pid, "window_id": fixture.target_id,
            "key": "a", "delivery_mode": "background",
        }),
    );
    assert!(
        unmapped.is_error(),
        "unmapped exact target still received a key: {}",
        unmapped.text()
    );
    assert_eq!(
        unmapped.structured()["reason"],
        "stale_target",
        "{}",
        unmapped.raw
    );
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    primary_command(&mut sibling.control, &sibling.journal, "SYNC");
    assert!(!has_keyboard_activity(
        &events(&fixture.journal)[before_unmapped..],
        "primary-keyboard"
    ));
    assert!(!has_keyboard_activity(
        &events(&fixture.journal)[before_unmapped..],
        "agent-keyboard"
    ));
    assert!(
        !has_keyboard_activity(
            &events(&sibling.journal)[sibling_before..],
            "agent-keyboard"
        ),
        "unmapped target key escaped into sibling"
    );
    assert_eq!(focus(), physical_focus);
    assert_stable(&watch.finish(), original_cursor, &physical_focus);
}

#[test]
#[ignore = "disposable candidate compositor, real v3 preedit and owned Driver transport EOF"]
fn agent_preedit_is_cleared_on_driver_eof() {
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut fixture = agent_fixture(&dir, "agent-preedit-eof");
    let mut sibling = keyboard_sibling(&mut fixture, &dir);
    let physical_focus = focus();
    let original_cursor = cursor();
    let watch = PrimarySeatWatch::start(dir.join("agent-preedit-primary.json"));
    primary_command(&mut fixture.control, &fixture.journal, "AUTO_ENABLE");
    let sibling_before = events(&sibling.journal).len();
    let target_before = events(&fixture.journal).len();
    let result = fixture.driver.call(
        "type_text",
        json!({
            "pid": fixture.pid, "window_id": fixture.target_id,
            "text": AGENT_TEXT, "composition_mode": "preedit", "delivery_mode": "background",
        }),
    );
    assert!(
        !result.is_error(),
        "real v3 preedit refused: {}",
        result.text()
    );
    assert_eq!(
        result.structured()["native_route"],
        "seat_text_input",
        "{}",
        result.raw
    );
    primary_command(&mut fixture.control, &fixture.journal, "SYNC");
    let before_eof = events(&fixture.journal);
    let entered = &before_eof[target_before..];
    assert_wire_event(entered, "agent-keyboard", "enter");
    assert!(
        entered.iter().any(|event| event["source"] == "client"
            && event["kind"] == "preedit-string"
            && event["text"] == AGENT_TEXT),
        "target application did not receive nonempty preedit: {entered:?}"
    );
    assert_wire_event(entered, "client", "done");
    assert!(
        !has_keyboard_activity(entered, "primary-keyboard"),
        "preedit affected primary keyboard: {entered:?}"
    );

    // The target app, physical-focus sibling and other agent keyboard client
    // are owned independently, so only the Driver's plugin socket reaches EOF.
    drop(fixture.driver);
    let deadline = Instant::now() + Duration::from_secs(1);
    let after_eof = loop {
        primary_command(&mut fixture.control, &fixture.journal, "SYNC");
        let all = events(&fixture.journal);
        let after = &all[before_eof.len()..];
        let cleared = after.iter().position(|event| {
            event["source"] == "client" && event["kind"] == "preedit-string" && event["text"] == ""
        });
        if let Some(index) = cleared {
            if after[index + 1..]
                .iter()
                .any(|event| event["source"] == "client" && event["kind"] == "done")
                && after
                    .iter()
                    .any(|event| event["source"] == "agent-keyboard" && event["kind"] == "leave")
            {
                break all;
            }
        }
        assert!(
            Instant::now() < deadline,
            "Driver EOF failed to clear composition and leave synthetic keyboard: {after:?}"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        !after_eof[before_eof.len()..]
            .iter()
            .any(|event| event["source"] == "client" && event["kind"] == "commit-string"),
        "EOF committed unfinished preedit"
    );
    assert!(
        !has_keyboard_activity(&after_eof[before_eof.len()..], "primary-keyboard"),
        "Driver EOF changed physical keyboard state"
    );
    primary_command(&mut sibling.control, &sibling.journal, "SYNC");
    let other = events(&sibling.journal);
    assert!(
        !has_keyboard_activity(&other[sibling_before..], "agent-keyboard"),
        "Driver EOF affected an independent synthetic-seat client: {other:?}"
    );
    assert_eq!(focus(), physical_focus);
    assert_stable(&watch.finish(), original_cursor, &physical_focus);
}

#[test]
#[ignore = "disposable candidate compositor, real synthetic wl_data_device offer/paste and 60-second owner TTL"]
fn agent_selection_offer_survives_return_then_owner_eof_clears_it() {
    const SESSION: &str = "agent-selection-owner";
    const TEXT: &str = "Cua selection — 漢字";
    preflight();
    let (dir, _cleanup) = evidence_dir();
    let mut fixture = agent_fixture(&dir, "agent-selection-lifecycle");
    let mut sibling = keyboard_sibling(&mut fixture, &dir);
    let session = fixture
        .driver
        .call("start_session", json!({"session": SESSION}));
    assert!(
        !session.is_error(),
        "explicit selection owner session failed: {}",
        session.text()
    );
    let physical_focus = focus();
    let original_cursor = cursor();
    let clipboard = primary_clipboard();
    let watch = PrimarySeatWatch::start(dir.join("agent-selection-primary.json"));

    let set = |fixture: &mut AgentFixture| {
        fixture.driver.call("set_agent_selection", json!({
        "session": SESSION, "pid": fixture.pid, "window_id": fixture.target_id, "text": TEXT,
    }))
    };
    let offered_before = events(&fixture.journal).len();
    let owned = set(&mut fixture);
    assert!(
        !owned.is_error(),
        "exact agent-seat selection refused: {}",
        owned.text()
    );
    assert_eq!(owned.structured()["selection_owned"], true, "{}", owned.raw);
    assert_eq!(
        owned.structured()["offered_to_target"],
        true,
        "{}",
        owned.raw
    );
    let offered = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        offered_before,
        "selection-offer",
        Some("text/plain;charset=utf-8"),
    );
    assert_wire_event(&offered, "client", "selection-mime");
    assert_eq!(
        primary_clipboard(),
        clipboard,
        "agent selection replaced primary-seat clipboard"
    );

    // The tool's ownership reply alone is not an application oracle: the
    // independent Wayland client asks the live offer to write its FD AFTER
    // the tool returns, reads until EOF, then pastes via its own Ctrl+V handler.
    let received_before = events(&fixture.journal).len();
    primary_command(&mut fixture.control, &fixture.journal, "RECEIVE_SELECTION");
    let received = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        received_before,
        "selection-received",
        Some(TEXT),
    );
    assert_wire_event(&received, "client", "selection-receive-requested");
    let paste_before = events(&fixture.journal).len();
    let pasted = paste(&mut fixture, SESSION);
    assert!(
        !pasted.is_error(),
        "agent Ctrl+V refused: {}",
        pasted.text()
    );
    let pasted_events = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        paste_before,
        "paste",
        Some(TEXT),
    );
    assert_wire_event(&pasted_events, "client", "paste-requested");
    assert_eq!(primary_clipboard(), clipboard);

    let cleared = fixture
        .driver
        .call("clear_agent_selection", json!({"session": SESSION}));
    assert!(
        !cleared.is_error() && cleared.structured()["cleared"] == true,
        "owner could not clear own selection: {}",
        cleared.raw
    );
    let clear_before = events(&fixture.journal).len();
    let key = paste(&mut fixture, SESSION);
    assert!(
        !key.is_error(),
        "target reentry after explicit clear refused: {}",
        key.text()
    );
    let no_paste = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        clear_before,
        "paste-unavailable",
        None,
    );
    assert_wire_event(&no_paste, "client", "selection-cleared");
    assert!(
        !no_paste.iter().any(|event| event["kind"] == "paste"),
        "explicit clear left an application-pastable offer: {no_paste:?}"
    );

    let second = set(&mut fixture);
    assert!(
        !second.is_error(),
        "second selection ownership refused: {}",
        second.text()
    );
    let sibling_before = events(&sibling.journal).len();
    let switched = fixture.driver.call(
        "press_key",
        json!({
            "session": SESSION, "pid": sibling.pid, "window_id": sibling.target_id,
            "key": "a", "delivery_mode": "background",
        }),
    );
    assert!(
        !switched.is_error(),
        "different-target agent focus refused: {}",
        switched.text()
    );
    primary_command(&mut sibling.control, &sibling.journal, "SYNC");
    assert_wire_event(
        &events(&sibling.journal)[sibling_before..],
        "agent-keyboard",
        "enter",
    );
    let focus_clear_before = events(&fixture.journal).len();
    let key = paste(&mut fixture, SESSION);
    assert!(
        !key.is_error(),
        "original recipient reentry refused: {}",
        key.text()
    );
    let no_paste = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        focus_clear_before,
        "paste-unavailable",
        None,
    );
    assert_wire_event(&no_paste, "client", "selection-cleared");
    assert!(
        !no_paste.iter().any(|event| event["kind"] == "paste"),
        "different-target transition left a stale offer: {no_paste:?}"
    );

    let timed = set(&mut fixture);
    assert!(
        !timed.is_error(),
        "bounded selection refused: {}",
        timed.text()
    );
    let ttl = timed.structured()["expires_in_ms"]
        .as_u64()
        .expect("selection reports bounded actual ownership TTL");
    assert!(ttl > 0 && ttl <= 60_000);
    thread::sleep(Duration::from_millis(ttl + 100));
    let expiry_before = events(&fixture.journal).len();
    let key = paste(&mut fixture, SESSION);
    assert!(
        !key.is_error(),
        "recipient reentry after expiry refused: {}",
        key.text()
    );
    let expired = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        expiry_before,
        "paste-unavailable",
        None,
    );
    assert_wire_event(&expired, "client", "selection-cleared");
    assert!(
        !expired.iter().any(|event| event["kind"] == "paste"),
        "expired offer still pasted: {expired:?}"
    );

    let last = set(&mut fixture);
    assert!(
        !last.is_error(),
        "pre-EOF selection refused: {}",
        last.text()
    );
    drop(fixture.driver);
    let mut probe = McpDriver::spawn_named_with_env(
        "agent-selection-after-eof",
        &[
            ("CUA_DRIVER_PERMISSION_MODE", "standard"),
            ("CUA_HYPRLAND_COMPATIBILITY_MODE", "general-native"),
        ],
    )
    .expect("independent post-EOF Driver required");
    let started = probe.call("start_session", json!({"session": "agent-selection-probe"}));
    assert!(
        !started.is_error(),
        "post-EOF session failed: {}",
        started.text()
    );
    let eof_before = events(&fixture.journal).len();
    let key = probe.call(
        "press_key",
        json!({
            "session": "agent-selection-probe", "pid": fixture.pid, "window_id": fixture.target_id,
            "key": "v", "modifiers": ["ctrl"], "delivery_mode": "background",
        }),
    );
    assert!(
        !key.is_error(),
        "post-EOF target reentry refused: {}",
        key.text()
    );
    let after_eof = wait_client_event(
        &mut fixture.control,
        &fixture.journal,
        eof_before,
        "paste-unavailable",
        None,
    );
    assert_wire_event(&after_eof, "client", "selection-cleared");
    assert!(
        !after_eof.iter().any(|event| event["kind"] == "paste"),
        "owner EOF left application-visible selection: {after_eof:?}"
    );
    assert_eq!(
        primary_clipboard(),
        clipboard,
        "agent lifecycle modified human clipboard"
    );
    assert_eq!(
        focus(),
        physical_focus,
        "selection lifecycle stole primary focus"
    );
    assert_stable(&watch.finish(), original_cursor, &physical_focus);
}
