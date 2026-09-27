use super::*;

#[test]
fn synthetic_pointer_drop_detection_covers_gtk_vcl_and_qt() {
    assert!(maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libgtk-3.so.0\n"
    ));
    assert!(maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libgtk-4.so.1\n"
    ));
    assert!(maps_indicate_synthetic_pointer_dropped(
        "7f /opt/libreoffice/program/libmergedlo.so\n"
    ));
    assert!(maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libvcllo.so\n"
    ));
    assert!(maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libQt5Gui.so.5\n"
    ));
    assert!(!maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libgtk-x11-2.0.so.0\n"
    ));
    assert!(!maps_indicate_synthetic_pointer_dropped(
        "7f /usr/lib/libX11.so.6\n"
    ));
}

#[test]
fn desktop_window_metadata_uses_screenshot_coordinates_with_negative_origin() {
    let windows = vec![crate::x11::WindowInfo {
        xid: 0x2e00003,
        pid: Some(4321),
        app_name: "libreoffice".into(),
        title: "Untitled 1 - LibreOffice Calc".into(),
        is_on_screen: true,
        z_index: None,
        x: 0,
        y: 27,
        width: 1920,
        height: 1053,
    }];
    let native = desktop_window_lines(&windows, (1920, 1080), (1920, 1080), (0, 0));
    assert!(native.contains("pid=4321 window_id=48234499"));
    assert!(native.contains("1920x1053 at (0,27)"), "{native}");
    let mapped = desktop_window_lines(&windows, (1280, 540), (2560, 1080), (-300, -100));
    assert!(mapped.contains("960x527 at (150,64)"), "{mapped}");
}
