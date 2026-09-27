use crate::wayland::hyprland::{CapturedDesktop, CapturedWindowScene, DesktopFrame};
use crate::wayland::hyprland_input::{FrameHit, FrameInventory, WindowCaptureProof};
use cua_driver_core::capture_runtime::{
    CaptureActionError, CaptureActionRequest, CaptureLookupError, CapturePublication,
    CaptureService, CaptureTarget, EncodedScreenshotDimensions, NativeActionDimensions,
    ScreenshotToActionTransform,
};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, LazyLock, Mutex};

enum CapturedGeometry {
    Desktop(CapturedDesktop),
    Window {
        window: crate::wayland::hyprland::Window,
        frame: crate::wayland::hyprland::DesktopFrame,
        scene: CapturedWindowScene,
    },
}

// This supplements the shared one-shot registry with compositor-only layout
// metadata. Eviction is fail-closed; no screenshot can act without its frame.
const MAX_GEOMETRY_RECORDS: usize = 128;
static GEOMETRY: LazyLock<Mutex<VecDeque<(String, Arc<CapturedGeometry>)>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));

fn geometry_records() -> &'static Mutex<VecDeque<(String, Arc<CapturedGeometry>)>> {
    &GEOMETRY
}

fn record_geometry(capture_id: &str, geometry: CapturedGeometry) {
    let mut records = geometry_records().lock().unwrap_or_else(|e| e.into_inner());
    if records.len() == MAX_GEOMETRY_RECORDS {
        records.pop_front();
    }
    records.push_back((capture_id.to_owned(), Arc::new(geometry)));
}

fn captured_geometry(capture_id: &str) -> Option<Arc<CapturedGeometry>> {
    geometry_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|(id, _)| id == capture_id)
        .map(|(_, geometry)| geometry.clone())
}

fn remove_geometry(capture_id: &str) {
    let mut records = geometry_records().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(index) = records.iter().position(|(id, _)| id == capture_id) {
        records.remove(index);
    }
}

fn window_target(pid: u32, window_id: u64) -> CaptureTarget {
    CaptureTarget::Window { pid, window_id }
}

fn publish(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    target: CaptureTarget,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    screenshot_to_action: ScreenshotToActionTransform,
) -> anyhow::Result<String> {
    let binding = service.binding_from_args(args)?;
    let capture_id = service.publish(CapturePublication {
        png_bytes: png_bytes.to_vec(),
        target,
        encoded_dimensions: EncodedScreenshotDimensions::new(
            encoded_dimensions.0,
            encoded_dimensions.1,
        )?,
        native_action_dimensions: NativeActionDimensions::new(
            native_action_dimensions.0,
            native_action_dimensions.1,
        )?,
        screenshot_to_action,
        session_id: binding.session_id().into(),
        session_generation: binding.session_generation(),
    })?;
    Ok(capture_id.to_string())
}

pub fn publish_window(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    pid: u32,
    window_id: u64,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
) -> anyhow::Result<String> {
    publish(
        service,
        args,
        png_bytes,
        window_target(pid, window_id),
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
    )
}

/// Publish the same window-local PNG with the compositor identity observed
/// during its export. A moved but same-sized window must not reuse that image.
pub fn publish_window_with_geometry(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    pid: u32,
    window_id: u64,
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    window: crate::wayland::hyprland::Window,
    frame: crate::wayland::hyprland::DesktopFrame,
    scene: CapturedWindowScene,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        window.pid == pid
            && window.address == window_id
            && (window.width, window.height) == native_action_dimensions,
        "window screenshot and action geometry do not match"
    );
    anyhow::ensure!(
        scene.xwayland.is_none() || scene.inventory.is_some(),
        "XWayland screenshot lacks scene inventory"
    );
    if let Some(snapshot) = &scene.inventory {
        anyhow::ensure!(
            snapshot.rows.iter().any(|row| row.pid == pid
                && row.address == window_id
                && !row.popup
                && scene
                    .xwayland
                    .as_ref()
                    .is_none_or(|proof| row == &proof.root)),
            "window root surface absent from captured scene"
        );
        if let Some(proof) = &scene.xwayland {
            anyhow::ensure!(
                snapshot.epoch == proof.epoch && snapshot.tree_revision == proof.tree_revision,
                "XWayland screenshot and root scene differ"
            );
        }
    }
    let capture_id = publish_window(
        service,
        args,
        png_bytes,
        pid,
        window_id,
        encoded_dimensions,
        native_action_dimensions,
    )?;
    record_geometry(
        &capture_id,
        CapturedGeometry::Window {
            window,
            frame,
            scene,
        },
    );
    Ok(capture_id)
}

pub fn publish_desktop(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
) -> anyhow::Result<String> {
    // The desktop screenshot can be downsized below the action frame.
    publish(
        service,
        args,
        png_bytes,
        CaptureTarget::PrimaryDesktop,
        encoded_dimensions,
        native_action_dimensions,
        scaled_transform(encoded_dimensions, native_action_dimensions)?,
    )
}

/// Publish a desktop image and the exact powered output arrangement that
/// produced it. Geometry comes from the capture operation, not a later query.
pub fn publish_desktop_with_frame(
    service: &CaptureService,
    args: &Value,
    png_bytes: &[u8],
    encoded_dimensions: (u32, u32),
    native_action_dimensions: (u32, u32),
    captured: CapturedDesktop,
) -> anyhow::Result<String> {
    anyhow::ensure!(
        native_action_dimensions == (captured.frame.width, captured.frame.height),
        "desktop screenshot and output geometry do not match"
    );
    let capture_id = publish_desktop(
        service,
        args,
        png_bytes,
        encoded_dimensions,
        native_action_dimensions,
    )?;
    record_geometry(&capture_id, CapturedGeometry::Desktop(captured));
    Ok(capture_id)
}

/// Screenshot-to-action scaling for a capture encoded at `encoded` pixels of
/// a `native` action frame. The resizer preserves aspect ratio before rounding
/// each encoded axis, so both ratios are derived independently: a one-pixel
/// rounded height must not skew Y coordinates.
fn scaled_transform(
    encoded: (u32, u32),
    native: (u32, u32),
) -> anyhow::Result<ScreenshotToActionTransform> {
    anyhow::ensure!(
        encoded.0 > 0 && encoded.1 > 0,
        "capture has an empty encoded frame: {}x{}",
        encoded.0,
        encoded.1
    );
    let scale_x = f64::from(native.0) / f64::from(encoded.0);
    let scale_y = f64::from(native.1) / f64::from(encoded.1);
    Ok(ScreenshotToActionTransform::new(
        scale_x, 0.0, 0.0, scale_y, 0.0, 0.0,
    )?)
}

/// The registry checks target/session and pixel bounds; the compositor layout
/// check adds origin, scale, transform, DPMS, and hotplug identity before it
/// consumes the capture. Same-sized reconfigurations must also refuse.
fn validate_captured_geometry(
    captured: &CapturedGeometry,
    live_frame: &DesktopFrame,
    live_window: Option<&crate::wayland::hyprland::Window>,
    encoded: EncodedScreenshotDimensions,
    point: (f64, f64),
) -> Result<NativeActionDimensions, CaptureActionError> {
    let (frame, action_dimensions) = match captured {
        CapturedGeometry::Desktop(captured) => (
            &captured.frame,
            (captured.frame.width, captured.frame.height),
        ),
        CapturedGeometry::Window { window, frame, .. } => {
            if live_window != Some(window) {
                return Err(CaptureActionError::NativeActionFrameMismatch);
            }
            (frame, (window.width, window.height))
        }
    };
    if frame != live_frame {
        return Err(CaptureActionError::NativeActionFrameMismatch);
    }
    if !point.0.is_finite()
        || !point.1.is_finite()
        || point.0 < 0.0
        || point.1 < 0.0
        || point.0 >= f64::from(encoded.width())
        || point.1 >= f64::from(encoded.height())
    {
        return Err(CaptureActionError::InvalidScreenshotPoint);
    }
    if matches!(captured, CapturedGeometry::Desktop(_)) {
        let mapped = (
            point.0 * f64::from(action_dimensions.0) / f64::from(encoded.width()),
            point.1 * f64::from(action_dimensions.1) / f64::from(encoded.height()),
        );
        frame
            .to_layout(mapped.0, mapped.1)
            .map_err(|_| CaptureActionError::InvalidMappedPoint)?;
    }
    NativeActionDimensions::new(action_dimensions.0, action_dimensions.1)
        .map_err(|_| CaptureActionError::NativeActionFrameMismatch)
}

fn require_live_inventory(inventory: &Option<FrameInventory>) -> anyhow::Result<()> {
    anyhow::ensure!(inventory.is_some(),
        "native frame inventory unavailable at capture; refresh with the matching Cua plugin enabled");
    let live = crate::wayland::hyprland_input::frame_inventory_if_available()?;
    validate_inventory_match(inventory, &live)?;
    Ok(())
}

fn validate_inventory_match(
    observed: &Option<FrameInventory>,
    live: &Option<FrameInventory>,
) -> Result<(), CaptureActionError> {
    if observed != live {
        return Err(CaptureActionError::NativeActionFrameMismatch);
    }
    Ok(())
}
fn require_live_xwayland(scene: &CapturedWindowScene, target: (u32, u64)) -> anyhow::Result<()> {
    if let Some(expected) = &scene.xwayland {
        let live = crate::wayland::hyprland_input::inspect_xwayland_capture(target.0, target.1)?;
        if &live != expected {
            return Err(CaptureActionError::NativeActionFrameMismatch.into());
        }
    }
    Ok(())
}

/// The compositor root and exact child observed under a window-local PNG
/// point must both be present in the immutable screenshot scene. Root alone
/// cannot prove a replaced child surface, and a live hit alone cannot prove
/// the child was visible in the returned window export.
fn validate_window_proof(
    captured: &CapturedGeometry,
    pid: u32,
    address: u64,
    proof: &WindowCaptureProof,
) -> Result<(), CaptureActionError> {
    let CapturedGeometry::Window { scene, .. } = captured else {
        return Err(CaptureActionError::NativeActionFrameMismatch);
    };
    let Some(snapshot) = &scene.inventory else {
        return Err(CaptureActionError::NativeActionFrameMismatch);
    };
    if proof.epoch != snapshot.epoch
        || proof.tree_revision != snapshot.tree_revision
        || proof.root.pid != pid
        || proof.root.address != address
        || proof.root.popup
        || proof.hit.pid != pid
        || proof.hit.address != address
        || proof.hit.popup
        || !snapshot.rows.iter().any(|row| row == &proof.root)
        || !snapshot.rows.iter().any(|row| row == &proof.hit)
        || proof.xid != scene.xwayland.as_ref().map(|xwayland| xwayland.xid)
        || scene
            .xwayland
            .as_ref()
            .is_some_and(|xwayland| xwayland.root != proof.root)
        || scene
            .xwayland
            .as_ref()
            .is_some_and(|_| proof.hit != proof.root)
    {
        return Err(CaptureActionError::NativeActionFrameMismatch);
    }
    Ok(())
}

pub struct WindowActionAdmission {
    pub point: (f64, f64),
    /// None only off Hyprland. Native windows require WINDOW_CAPTURE_TARGET;
    /// XWayland windows require its distinct approved foreground proof route.
    pub proof: Option<WindowCaptureProof>,
}

/// A one-shot desktop admission retains the immutable observed scene. A later
/// point-only inspection cannot establish that a popup appeared in the PNG.
pub struct DesktopActionAdmission {
    pub frame_point: (f64, f64),
    captured: Option<Arc<CapturedGeometry>>,
}

impl DesktopActionAdmission {
    fn observed(&self) -> anyhow::Result<(&DesktopFrame, &FrameInventory)> {
        let Some(CapturedGeometry::Desktop(captured)) = self.captured.as_deref() else {
            anyhow::bail!("desktop action has no compositor-attested frame inventory");
        };
        let inventory = captured
            .inventory
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("desktop capture has no native scene inventory"))?;
        Ok((&captured.frame, inventory))
    }

    /// Identify only a popup present in the captured frame at this point.
    /// More than one distinct popup owner is ambiguous without z-order proof.
    pub fn popup_target(&self) -> anyhow::Result<Option<(u32, u64)>> {
        let (frame, inventory) = self.observed()?;
        let global = frame.to_layout(self.frame_point.0, self.frame_point.1)?;
        let mut owner = None;
        for row in &inventory.rows {
            if row.popup && row_contains(row, global) {
                let candidate = (row.pid, row.address);
                anyhow::ensure!(
                    owner.is_none_or(|prior| prior == candidate),
                    "overlapping captured popup owners are ambiguous"
                );
                owner = Some(candidate);
            }
        }
        Ok(owner)
    }

    /// Require the compositor's current topmost hit to match an exact row in
    /// the screenshot's inventory and the same monotonically revised scene.
    pub fn frame_hit(&self, pid: u32, address: u64) -> anyhow::Result<FrameHit> {
        let (frame, inventory) = self.observed()?;
        let global = frame.to_layout(self.frame_point.0, self.frame_point.1)?;
        let mut geometry: Option<[f64; 6]> = None;
        for row in inventory
            .rows
            .iter()
            .filter(|row| row.pid == pid && row.address == address)
        {
            anyhow::ensure!(
                geometry.is_none_or(|prior| prior == row.root_geometry),
                "inconsistent captured root geometry"
            );
            geometry = Some(row.root_geometry);
        }
        let geometry =
            geometry.ok_or_else(|| anyhow::anyhow!("target absent from captured desktop"))?;
        let local = (global.0 - geometry[0], global.1 - geometry[1]);
        let hit = crate::wayland::hyprland_input::inspect_frame_hit(pid, address, local)?;
        validate_observed_hit(inventory, global, &hit)?;
        Ok(hit)
    }
    /// Convert the admitted frame point using the compositor's captured root
    /// surface geometry, never the decorated `hyprctl` window rectangle.
    pub fn local_point(&self, hit: &FrameHit) -> anyhow::Result<(f64, f64)> {
        let (frame, inventory) = self.observed()?;
        let global = frame.to_layout(self.frame_point.0, self.frame_point.1)?;
        validate_observed_hit(inventory, global, hit)?;
        Ok((
            global.0 - hit.hit.root_geometry[0],
            global.1 - hit.hit.root_geometry[1],
        ))
    }
}
fn validate_observed_hit(
    inventory: &FrameInventory,
    global: (f64, f64),
    hit: &FrameHit,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        hit.epoch == inventory.epoch
            && hit.tree_revision == inventory.tree_revision
            && row_contains(&hit.hit, global)
            && inventory.rows.iter().any(|row| row == &hit.hit),
        "captured scene no longer matches the exact visible target; refresh screenshot"
    );
    Ok(())
}

fn row_contains(row: &crate::wayland::hyprland_input::FrameRow, point: (f64, f64)) -> bool {
    point.0 >= row.box_[0]
        && point.0 < row.box_[0] + row.box_[2]
        && point.1 >= row.box_[1]
        && point.1 < row.box_[1] + row.box_[3]
}

fn admit(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    target: CaptureTarget,
    screenshot_x: f64,
    screenshot_y: f64,
    expected_native: Option<NativeActionDimensions>,
) -> anyhow::Result<((f64, f64), Option<Arc<CapturedGeometry>>)> {
    let hyprland = crate::wayland::is_wayland() && crate::wayland::hyprland::is_session();
    let mut observed = None;
    let current_native_action_dimensions = if hyprland {
        let binding = service.binding_from_args(args)?;
        let id = service.parse_capture_id(capture_id)?;
        let published = service
            .read_for_perception(id, &binding)
            .map_err(CaptureActionError::Lookup)?;
        if published.target() != &target {
            return Err(CaptureActionError::Lookup(CaptureLookupError::TargetMismatch).into());
        }
        let captured =
            captured_geometry(capture_id).ok_or(CaptureActionError::NativeActionFrameMismatch)?;
        observed = Some(captured.clone());
        let frame = crate::wayland::hyprland::desktop_frame()?;
        let live_window = match &target {
            CaptureTarget::Window { window_id, .. } => crate::wayland::hyprland::list_windows()?
                .into_iter()
                .find(|window| window.address == *window_id),
            CaptureTarget::PrimaryDesktop => None,
        };
        match captured.as_ref() {
            CapturedGeometry::Desktop(desktop) => require_live_inventory(&desktop.inventory)?,
            CapturedGeometry::Window { scene, .. } => {
                require_live_inventory(&scene.inventory)?;
                if let CaptureTarget::Window { pid, window_id } = &target {
                    require_live_xwayland(scene, (*pid, *window_id))?;
                }
            }
        }
        validate_captured_geometry(
            &captured,
            &frame,
            live_window.as_ref(),
            published.encoded_dimensions(),
            (screenshot_x, screenshot_y),
        )?
    } else {
        live_action_dimensions(&target)?
    };
    if expected_native.is_some_and(|expected| expected != current_native_action_dimensions) {
        return Err(CaptureActionError::NativeActionFrameMismatch.into());
    }
    let result = admit_with_live_dimensions(
        service,
        args,
        capture_id,
        target,
        current_native_action_dimensions,
        screenshot_x,
        screenshot_y,
    );
    if result.is_ok() && hyprland {
        remove_geometry(capture_id);
    }
    result.map(|point| (point, observed))
}

fn admit_with_live_dimensions(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    target: CaptureTarget,
    current_native_action_dimensions: NativeActionDimensions,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<(f64, f64)> {
    let binding = service.binding_from_args(args)?;
    let capture_id = service.parse_capture_id(capture_id)?;
    let admission = service.admit_action(CaptureActionRequest {
        capture_id,
        binding,
        target,
        current_native_action_dimensions,
        screenshot_x,
        screenshot_y,
    })?;
    Ok((admission.action_x, admission.action_y))
}

#[cfg(target_os = "linux")]
fn live_action_dimensions(target: &CaptureTarget) -> anyhow::Result<NativeActionDimensions> {
    let dimensions = match target {
        CaptureTarget::Window { pid, window_id } => {
            let identity_matches = if crate::wayland::is_wayland() {
                crate::wayland::window_was_listed_for_pid(*pid, *window_id)
            } else {
                crate::x11::window_belongs_to_pid(*window_id, *pid)
            };
            anyhow::ensure!(
                identity_matches,
                "native window identity changed after capture"
            );
            let png = crate::wayland::screenshot_dispatch_with_pid(*window_id, *pid)?;
            crate::capture::png_dimensions_pub(&png)?
        }
        CaptureTarget::PrimaryDesktop => {
            let png = crate::capture::screenshot_display_bytes()?;
            let native = crate::capture::png_dimensions_pub(&png)?;
            desktop_action_dimensions(native)?
        }
    };
    Ok(NativeActionDimensions::new(dimensions.0, dimensions.1)?)
}

#[cfg(not(target_os = "linux"))]
fn live_action_dimensions(_target: &CaptureTarget) -> anyhow::Result<NativeActionDimensions> {
    anyhow::bail!("live Linux capture validation is unavailable on this platform")
}

#[cfg(target_os = "linux")]
pub(crate) fn desktop_action_dimensions(native: (u32, u32)) -> anyhow::Result<(u32, u32)> {
    let logical = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let (width, height, _) = crate::wayland::hyprland::screen_size()?;
        Some((width, height))
    } else {
        None
    };
    select_desktop_action_dimensions(native, logical)
}

fn select_desktop_action_dimensions(
    native: (u32, u32),
    compositor_logical: Option<(u32, u32)>,
) -> anyhow::Result<(u32, u32)> {
    let dimensions = compositor_logical.unwrap_or(native);
    anyhow::ensure!(
        dimensions.0 > 0 && dimensions.1 > 0,
        "desktop action frame is empty: {}x{}",
        dimensions.0,
        dimensions.1
    );
    Ok(dimensions)
}

pub fn admit_window_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    pid: u32,
    window_id: u64,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<WindowActionAdmission> {
    let proof = if crate::wayland::is_wayland() && crate::wayland::hyprland::is_session() {
        let binding = service.binding_from_args(args)?;
        let id = service.parse_capture_id(capture_id)?;
        let published = service
            .read_for_perception(id, &binding)
            .map_err(CaptureActionError::Lookup)?;
        if published.target() != &window_target(pid, window_id) {
            return Err(CaptureActionError::Lookup(CaptureLookupError::TargetMismatch).into());
        }
        let captured =
            captured_geometry(capture_id).ok_or(CaptureActionError::NativeActionFrameMismatch)?;
        let CapturedGeometry::Window { scene, .. } = captured.as_ref() else {
            return Err(CaptureActionError::NativeActionFrameMismatch.into());
        };
        require_live_inventory(&scene.inventory)?;
        require_live_xwayland(scene, (pid, window_id))?;
        let frame = crate::wayland::hyprland::desktop_frame()?;
        let live_window = crate::wayland::hyprland::list_windows()?
            .into_iter()
            .find(|window| window.address == window_id && window.pid == pid);
        let encoded = published.encoded_dimensions();
        let native = validate_captured_geometry(
            &captured,
            &frame,
            live_window.as_ref(),
            encoded,
            (screenshot_x, screenshot_y),
        )?;
        let point = scaled_transform(
            (encoded.width(), encoded.height()),
            (native.width(), native.height()),
        )?
        .apply(screenshot_x, screenshot_y);
        let proof = crate::wayland::hyprland_input::inspect_window_hit(pid, window_id, point)?;
        validate_window_proof(&captured, pid, window_id, &proof)?;
        Some((point, native, proof))
    } else {
        None
    };
    let (point, _) = admit(
        service,
        args,
        capture_id,
        window_target(pid, window_id),
        screenshot_x,
        screenshot_y,
        proof.as_ref().map(|(_, native, _)| *native),
    )?;
    if let Some((inspected_point, _, captured_proof)) = proof {
        if point != inspected_point {
            return Err(CaptureActionError::NativeActionFrameMismatch.into());
        }
        Ok(WindowActionAdmission {
            point,
            proof: Some(captured_proof),
        })
    } else {
        Ok(WindowActionAdmission { point, proof: None })
    }
}

pub fn admit_desktop_click(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    screenshot_x: f64,
    screenshot_y: f64,
) -> anyhow::Result<DesktopActionAdmission> {
    let (frame_point, captured) = admit(
        service,
        args,
        capture_id,
        CaptureTarget::PrimaryDesktop,
        screenshot_x,
        screenshot_y,
        None,
    )?;
    Ok(DesktopActionAdmission {
        frame_point,
        captured,
    })
}
/// A drag consumes one capture for its whole gesture, after both endpoints
/// have passed bounds, powered-output and revision checks.
pub fn admit_desktop_drag(
    service: &CaptureService,
    args: &Value,
    capture_id: &str,
    start: (f64, f64),
    end: (f64, f64),
) -> anyhow::Result<((f64, f64), (f64, f64))> {
    let binding = service.binding_from_args(args)?;
    let id = service.parse_capture_id(capture_id)?;
    let published = service
        .read_for_perception(id, &binding)
        .map_err(CaptureActionError::Lookup)?;
    if published.target() != &CaptureTarget::PrimaryDesktop {
        return Err(CaptureActionError::Lookup(CaptureLookupError::TargetMismatch).into());
    }
    let encoded = published.encoded_dimensions();
    validate_encoded_point(encoded, start)?;
    validate_encoded_point(encoded, end)?;
    let hyprland = crate::wayland::is_wayland() && crate::wayland::hyprland::is_session();
    let native =
        if hyprland {
            let captured = captured_geometry(capture_id)
                .ok_or(CaptureActionError::NativeActionFrameMismatch)?;
            let frame = crate::wayland::hyprland::desktop_frame()?;
            if let CapturedGeometry::Desktop(desktop) = captured.as_ref() {
                require_live_inventory(&desktop.inventory)?;
            }
            if let CapturedGeometry::Desktop(desktop) = captured.as_ref() {
                let inventory = desktop
                    .inventory
                    .as_ref()
                    .ok_or(CaptureActionError::NativeActionFrameMismatch)?;
                let source = frame
                    .to_layout(
                        start.0 * f64::from(frame.width) / f64::from(encoded.width()),
                        start.1 * f64::from(frame.height) / f64::from(encoded.height()),
                    )
                    .map_err(|_| CaptureActionError::InvalidMappedPoint)?;
                let destination = frame
                    .to_layout(
                        end.0 * f64::from(frame.width) / f64::from(encoded.width()),
                        end.1 * f64::from(frame.height) / f64::from(encoded.height()),
                    )
                    .map_err(|_| CaptureActionError::InvalidMappedPoint)?;
                anyhow::ensure!(
                !inventory.rows.iter().any(|row| row.popup
                    && segment_intersects_box(source, destination, row.box_)),
                "captured popup intersects desktop drag path; use an explicit target-bound route"
            );
            }
            validate_captured_geometry(&captured, &frame, None, encoded, end)?
        } else {
            live_action_dimensions(&CaptureTarget::PrimaryDesktop)?
        };
    let first = admit(
        service,
        args,
        capture_id,
        CaptureTarget::PrimaryDesktop,
        start.0,
        start.1,
        Some(native),
    )?
    .0;
    let second = (
        end.0 * f64::from(native.width()) / f64::from(encoded.width()),
        end.1 * f64::from(native.height()) / f64::from(encoded.height()),
    );
    Ok((first, second))
}

fn validate_encoded_point(
    encoded: EncodedScreenshotDimensions,
    point: (f64, f64),
) -> Result<(), CaptureActionError> {
    if !point.0.is_finite()
        || !point.1.is_finite()
        || point.0 < 0.0
        || point.1 < 0.0
        || point.0 >= f64::from(encoded.width())
        || point.1 >= f64::from(encoded.height())
    {
        return Err(CaptureActionError::InvalidScreenshotPoint);
    }
    Ok(())
}
/// Conservatively count boundary contact as an intersection: no drag phase
/// may enter a popup that the selected desktop gesture did not bind.
fn segment_intersects_box(start: (f64, f64), end: (f64, f64), box_: [f64; 4]) -> bool {
    let mut entry: f64 = 0.0;
    let mut exit: f64 = 1.0;
    for (start, delta, lower, upper) in [
        (start.0, end.0 - start.0, box_[0], box_[0] + box_[2]),
        (start.1, end.1 - start.1, box_[1], box_[1] + box_[3]),
    ] {
        if delta == 0.0 {
            if start < lower || start > upper {
                return false;
            }
        } else {
            let first = (lower - start) / delta;
            let last = (upper - start) / delta;
            entry = entry.max(first.min(last));
            exit = exit.min(first.max(last));
            if entry > exit {
                return false;
            }
        }
    }
    true
}

pub fn retire_runtime(service: &CaptureService) {
    service.retire_runtime();
    geometry_records()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use cua_driver_core::capture_runtime::admission_error_code;
    use sha2::{Digest, Sha256};

    fn png(width: u32, height: u32, value: u8) -> Vec<u8> {
        let rgba = vec![value; (width * height * 4) as usize];
        cua_driver_core::image_utils::encode_rgba_to_png(&rgba, width, height)
            .expect("encode fixture")
    }

    fn args(session: &str) -> Value {
        serde_json::json!({"_session_id": session})
    }

    fn admit_window(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        pid: u32,
        window_id: u64,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            window_target(pid, window_id),
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
    }

    fn admit_desktop(
        service: &CaptureService,
        args: &Value,
        capture_id: &str,
        point: (f64, f64),
        native: (u32, u32),
    ) -> anyhow::Result<(f64, f64)> {
        admit_with_live_dimensions(
            service,
            args,
            capture_id,
            CaptureTarget::PrimaryDesktop,
            NativeActionDimensions::new(native.0, native.1).unwrap(),
            point.0,
            point.1,
        )
    }

    #[test]
    fn window_publication_digests_the_exact_returned_png() {
        let service = CaptureService::default();
        let delivered = png(2, 2, 0x41);
        let id = publish_window(
            &service,
            &args("digest"),
            &delivered,
            41,
            99,
            (2, 2),
            (4, 4),
        )
        .expect("publish window capture");
        let binding = service.binding_from_args(&args("digest")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .expect("read published capture");

        assert_eq!(capture.png_bytes().as_ref(), delivered.as_slice());
        let expected: [u8; 32] = Sha256::digest(&delivered).into();
        assert_eq!(capture.digest().as_bytes(), &expected);
    }

    #[test]
    fn target_mismatch_does_not_consume_but_successful_admission_does() {
        let service = CaptureService::default();
        let call_args = args("one-action");
        let id = publish_window(
            &service,
            &call_args,
            &png(10, 10, 0x22),
            7,
            11,
            (10, 10),
            (20, 20),
        )
        .unwrap();

        assert!(admit_window(&service, &call_args, &id, 7, 12, (2.0, 3.0), (20, 20)).is_err());
        assert_eq!(
            admit_window(&service, &call_args, &id, 7, 11, (2.0, 3.0), (20, 20)).unwrap(),
            (4.0, 6.0)
        );
        assert!(admit_window(&service, &call_args, &id, 7, 11, (2.0, 3.0), (20, 20)).is_err());
    }

    #[test]
    fn retired_session_capture_is_stale() {
        let service = CaptureService::default();
        let call_args = args("retired");
        let id = publish_desktop(&service, &call_args, &png(3, 2, 0x77), (3, 2), (3, 2)).unwrap();

        let binding = service.binding_from_args(&call_args).unwrap();
        service.retire_session(&binding);

        assert!(admit_desktop(&service, &call_args, &id, (1.0, 1.0), (3, 2)).is_err());
    }

    #[test]
    fn window_transform_preserves_each_rounded_axis() {
        let service = CaptureService::default();
        let call_args = args("rounding");
        let id = publish_window(
            &service,
            &call_args,
            &png(3, 2, 0x33),
            8,
            13,
            (3, 2),
            (5, 3),
        )
        .unwrap();

        let (x, y) = admit_window(&service, &call_args, &id, 8, 13, (1.0, 1.0), (5, 3)).unwrap();
        assert_eq!(x, 5.0 / 3.0);
        assert_eq!(y, 3.0 / 2.0);
    }

    #[test]
    fn another_session_cannot_admit_or_consume_a_capture() {
        let service = CaptureService::default();
        let owner = args("owner");
        let id = publish_desktop(&service, &owner, &png(4, 3, 0x21), (4, 3), (4, 3)).unwrap();

        let refusal = admit_desktop(&service, &args("other"), &id, (2.0, 1.0), (4, 3))
            .expect_err("cross-session admission must fail");
        assert_eq!(
            admission_error_code(&refusal),
            "capture_generation_mismatch"
        );
        assert_eq!(
            admit_desktop(&service, &owner, &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn hidpi_desktop_uses_the_compositors_logical_action_frame() {
        let native = (3200, 2000);
        let logical = select_desktop_action_dimensions(native, Some((1600, 1000))).unwrap();
        assert_eq!(logical, (1600, 1000));

        let service = CaptureService::default();
        let call_args = args("hidpi");
        let id = publish_desktop(
            &service,
            &call_args,
            &png(logical.0, logical.1, 0x31),
            logical,
            logical,
        )
        .unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (800.0, 500.0), logical).unwrap(),
            (800.0, 500.0)
        );
    }

    #[test]
    fn downsized_desktop_capture_maps_back_to_the_action_frame() {
        let service = CaptureService::default();
        let call_args = args("downsized");
        let id = publish_desktop(&service, &call_args, &png(4, 3, 0x42), (4, 3), (8, 6)).unwrap();
        assert_eq!(
            admit_desktop(&service, &call_args, &id, (2.0, 1.5), (8, 6)).unwrap(),
            (4.0, 3.0)
        );
    }

    #[test]
    fn desktop_publication_uses_the_post_normalization_bytes() {
        let service = CaptureService::default();
        let normalized = png(4, 3, 0x18);
        let id = publish_desktop(&service, &args("desktop"), &normalized, (4, 3), (4, 3)).unwrap();
        let binding = service.binding_from_args(&args("desktop")).unwrap();
        let capture = service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .unwrap();

        assert_eq!(capture.png_bytes().as_ref(), normalized.as_slice());
        assert_eq!(
            admit_desktop(&service, &args("desktop"), &id, (2.0, 1.0), (4, 3)).unwrap(),
            (2.0, 1.0)
        );
    }

    #[test]
    fn live_resize_refuses_before_dispatch_without_consuming_capture() {
        let service = CaptureService::default();
        let call_args = args("resize");
        let id = publish_window(
            &service,
            &call_args,
            &png(4, 3, 0x44),
            8,
            13,
            (4, 3),
            (8, 6),
        )
        .unwrap();
        let mut dispatches = 0;
        let refusal = admit_window(&service, &call_args, &id, 8, 13, (1.25, 1.5), (9, 6));
        if refusal.is_ok() {
            dispatches += 1;
        }
        assert_eq!(dispatches, 0);
        assert!(refusal
            .unwrap_err()
            .downcast_ref::<cua_driver_core::capture_runtime::CaptureActionError>()
            .is_some_and(|error| {
                *error
                == cua_driver_core::capture_runtime::CaptureActionError::NativeActionFrameMismatch
            }));
        assert_eq!(
            admit_window(&service, &call_args, &id, 8, 13, (1.25, 1.5), (8, 6),).unwrap(),
            (2.5, 3.0)
        );
    }

    fn mixed_frame() -> crate::wayland::hyprland::DesktopFrame {
        use crate::wayland::hyprland::{DesktopFrame, FrameOutput};
        DesktopFrame {
            x: -2,
            y: 0,
            width: 5,
            height: 3,
            outputs: vec![
                FrameOutput {
                    name: "left".into(),
                    id: Some(1),
                    x: -2,
                    y: 0,
                    width: 2,
                    height: 1,
                    native_width: 4,
                    native_height: 2,
                    scale_bits: 2.0_f64.to_bits(),
                    transform: 0,
                },
                FrameOutput {
                    name: "right".into(),
                    id: Some(2),
                    x: 1,
                    y: 2,
                    width: 2,
                    height: 1,
                    native_width: 2,
                    native_height: 4,
                    scale_bits: 2.0_f64.to_bits(),
                    transform: 1,
                },
            ],
        }
    }

    #[test]
    fn downsampled_gap_point_refuses_without_consuming_a_valid_capture() {
        let service = CaptureService::default();
        let args = args("gap");
        let frame = mixed_frame();
        let id = publish_desktop_with_frame(
            &service,
            &args,
            &png(3, 2, 0x32),
            (3, 2),
            (5, 3),
            CapturedDesktop {
                frame: frame.clone(),
                inventory: None,
            },
        )
        .unwrap();
        let captured = captured_geometry(&id).unwrap();
        let encoded = EncodedScreenshotDimensions::new(3, 2).unwrap();
        let error =
            validate_captured_geometry(&captured, &frame, None, encoded, (1.5, 1.0)).unwrap_err();
        assert_eq!(error.wire_code(), "capture_coordinate_invalid");
        let native =
            validate_captured_geometry(&captured, &frame, None, encoded, (2.0, 1.5)).unwrap();
        assert_eq!(
            admit_with_live_dimensions(
                &service,
                &args,
                &id,
                CaptureTarget::PrimaryDesktop,
                native,
                2.0,
                1.5,
            )
            .unwrap(),
            (10.0 / 3.0, 2.25)
        );
        remove_geometry(&id);
    }

    #[test]
    fn moved_window_refuses_without_translating_its_local_screenshot_coordinates() {
        use crate::wayland::hyprland::Window;
        let service = CaptureService::default();
        let args = args("window-origin");
        let frame = mixed_frame();
        let window = Window {
            address: 13,
            pid: 8,
            title: "Test".into(),
            app_id: "test".into(),
            x: -2,
            y: 0,
            width: 4,
            height: 4,
            workspace: 1,
            visible: true,
            hidden: false,
        };
        let id = publish_window_with_geometry(
            &service,
            &args,
            &png(2, 2, 0x12),
            8,
            13,
            (2, 2),
            (4, 4),
            window.clone(),
            frame.clone(),
            CapturedWindowScene {
                inventory: None,
                xwayland: None,
            },
        )
        .unwrap();
        let captured = captured_geometry(&id).unwrap();
        let encoded = EncodedScreenshotDimensions::new(2, 2).unwrap();
        let mut moved = window.clone();
        moved.x += 1;
        let error =
            validate_captured_geometry(&captured, &frame, Some(&moved), encoded, (1.0, 0.5))
                .unwrap_err();
        assert_eq!(error.wire_code(), "capture_frame_mismatch");
        let native =
            validate_captured_geometry(&captured, &frame, Some(&window), encoded, (1.0, 0.5))
                .unwrap();
        assert_eq!(
            admit_with_live_dimensions(
                &service,
                &args,
                &id,
                window_target(8, 13),
                native,
                1.0,
                0.5,
            )
            .unwrap(),
            (2.0, 1.0)
        );
        remove_geometry(&id);
    }

    #[test]
    fn same_box_recycled_window_surface_does_not_inherit_capture() {
        use crate::wayland::hyprland::Window;
        let service = CaptureService::default();
        let args = args("window-lifetime");
        let frame = mixed_frame();
        let window = Window {
            address: 13,
            pid: 8,
            title: "Test".into(),
            app_id: "test".into(),
            x: -2,
            y: 0,
            width: 4,
            height: 4,
            workspace: 1,
            visible: true,
            hidden: false,
        };
        let observed = Some(FrameInventory {
            epoch: "1234567890abcdef1234567890abcdef".into(),
            tree_revision: 42,
            rows: vec![observed_row(8, 13, false, [-2.0, 0.0, 4.0, 4.0])],
        });
        let id = publish_window_with_geometry(
            &service,
            &args,
            &png(2, 2, 0x12),
            8,
            13,
            (2, 2),
            (4, 4),
            window.clone(),
            frame.clone(),
            CapturedWindowScene {
                inventory: observed.clone(),
                xwayland: None,
            },
        )
        .unwrap();
        let stored = captured_geometry(&id).unwrap();
        let encoded = EncodedScreenshotDimensions::new(2, 2).unwrap();
        let native =
            validate_captured_geometry(&stored, &frame, Some(&window), encoded, (1.0, 1.0))
                .unwrap();
        let mut replacement = observed.clone();
        replacement.as_mut().unwrap().rows[0].lifetime = "abcdef1234567890abcdef1234567890".into();
        assert_eq!(
            validate_inventory_match(&observed, &replacement)
                .unwrap_err()
                .wire_code(),
            "capture_frame_mismatch"
        );
        let binding = service.binding_from_args(&args).unwrap();
        assert!(service
            .read_for_perception(service.parse_capture_id(&id).unwrap(), &binding)
            .is_ok());
        validate_inventory_match(&observed, &observed).unwrap();
        admit_with_live_dimensions(&service, &args, &id, window_target(8, 13), native, 1.0, 1.0)
            .unwrap();
        remove_geometry(&id);
    }

    #[test]
    fn window_child_hit_must_be_in_the_captured_surface_tree() {
        use crate::wayland::hyprland::Window;
        let root = observed_row(8, 13, false, [-2.0, 0.0, 4.0, 4.0]);
        let mut child = observed_row(8, 13, false, [-1.0, 1.0, 2.0, 2.0]);
        child.surface_id = 18;
        child.lifetime = "abcdef1234567890abcdef1234567890".into();
        let captured = CapturedGeometry::Window {
            window: Window {
                address: 13,
                pid: 8,
                title: "Test".into(),
                app_id: "test".into(),
                x: -2,
                y: 0,
                width: 4,
                height: 4,
                workspace: 1,
                visible: true,
                hidden: false,
            },
            frame: mixed_frame(),
            scene: CapturedWindowScene {
                inventory: Some(FrameInventory {
                    epoch: "1234567890abcdef1234567890abcdef".into(),
                    tree_revision: 42,
                    rows: vec![root.clone(), child.clone()],
                }),
                xwayland: None,
            },
        };
        let mut proof = WindowCaptureProof {
            epoch: "1234567890abcdef1234567890abcdef".into(),
            tree_revision: 42,
            root,
            hit: child,
            xid: None,
        };
        validate_window_proof(&captured, 8, 13, &proof).unwrap();
        proof.hit.lifetime = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into();
        assert_eq!(
            validate_window_proof(&captured, 8, 13, &proof)
                .unwrap_err()
                .wire_code(),
            "capture_frame_mismatch"
        );
        proof.hit.lifetime = "abcdef1234567890abcdef1234567890".into();
        proof.hit.popup = true;
        assert_eq!(
            validate_window_proof(&captured, 8, 13, &proof)
                .unwrap_err()
                .wire_code(),
            "capture_frame_mismatch"
        );
    }

    #[test]
    fn xwayland_proof_requires_the_captured_xid_and_real_root_row() {
        use crate::wayland::hyprland::Window;
        use crate::wayland::hyprland_input::XWaylandCaptureProof;
        let root = observed_row(8, 13, false, [-2.0, 0.0, 4.0, 4.0]);
        let epoch = "1234567890abcdef1234567890abcdef".to_owned();
        let captured = CapturedGeometry::Window {
            window: Window {
                address: 13,
                pid: 8,
                title: "X11".into(),
                app_id: "x11".into(),
                x: -2,
                y: 0,
                width: 4,
                height: 4,
                workspace: 1,
                visible: true,
                hidden: false,
            },
            frame: mixed_frame(),
            scene: CapturedWindowScene {
                inventory: Some(FrameInventory {
                    epoch: epoch.clone(),
                    tree_revision: 42,
                    rows: vec![root.clone()],
                }),
                xwayland: Some(XWaylandCaptureProof {
                    epoch: epoch.clone(),
                    tree_revision: 42,
                    xid: 73,
                    root: root.clone(),
                }),
            },
        };
        let mut proof = WindowCaptureProof {
            epoch,
            tree_revision: 42,
            root: root.clone(),
            hit: root,
            xid: Some(73),
        };
        validate_window_proof(&captured, 8, 13, &proof).unwrap();
        proof.xid = Some(74);
        assert_eq!(
            validate_window_proof(&captured, 8, 13, &proof)
                .unwrap_err()
                .wire_code(),
            "capture_frame_mismatch"
        );
        proof.xid = Some(73);
        proof.root.surface_id += 1;
        assert_eq!(
            validate_window_proof(&captured, 8, 13, &proof)
                .unwrap_err()
                .wire_code(),
            "capture_frame_mismatch"
        );
    }

    #[test]
    fn same_size_origin_scale_rotation_or_power_change_refuses_stale_frame() {
        let captured = CapturedGeometry::Desktop(CapturedDesktop {
            frame: mixed_frame(),
            inventory: None,
        });
        let encoded = EncodedScreenshotDimensions::new(5, 3).unwrap();
        let original = mixed_frame();
        let mut changed = original.clone();
        changed.x += 1;
        for current in [
            changed,
            {
                let mut frame = original.clone();
                frame.outputs[0].scale_bits = 1.0_f64.to_bits();
                frame
            },
            {
                let mut frame = original.clone();
                frame.outputs[1].transform = 3;
                frame
            },
            {
                let mut frame = original.clone();
                frame.outputs.remove(0);
                frame
            },
        ] {
            let error = validate_captured_geometry(&captured, &current, None, encoded, (0.5, 0.0))
                .unwrap_err();
            assert_eq!(error.wire_code(), "capture_frame_mismatch");
        }
    }
    fn observed_row(
        pid: u32,
        address: u64,
        popup: bool,
        box_: [f64; 4],
    ) -> crate::wayland::hyprland_input::FrameRow {
        crate::wayland::hyprland_input::FrameRow {
            pid,
            address,
            surface_id: 17,
            lifetime: "1234567890abcdef1234567890abcdef".into(),
            root_geometry: [-2.0, 0.0, 2.0, 1.0, -2.0, 0.0],
            box_,
            popup,
        }
    }

    fn observed_admission(
        rows: Vec<crate::wayland::hyprland_input::FrameRow>,
        point: (f64, f64),
    ) -> DesktopActionAdmission {
        DesktopActionAdmission {
            frame_point: point,
            captured: Some(Arc::new(CapturedGeometry::Desktop(CapturedDesktop {
                frame: mixed_frame(),
                inventory: Some(FrameInventory {
                    epoch: "1234567890abcdef1234567890abcdef".into(),
                    tree_revision: 42,
                    rows,
                }),
            }))),
        }
    }

    #[test]
    fn off_base_popup_requires_a_row_in_the_captured_visible_frame() {
        // Global (1.5, 2.5) is outside the window root box, yet on the
        // right powered output. It is routable only with a captured popup row.
        let popup = observed_row(8, 13, true, [1.0, 2.0, 1.0, 1.0]);
        let admission = observed_admission(vec![popup.clone()], (3.5, 2.5));
        assert_eq!(admission.popup_target().unwrap(), Some((8, 13)));
        let visible = FrameHit {
            epoch: "1234567890abcdef1234567890abcdef".into(),
            tree_revision: 42,
            hit: popup.clone(),
        };
        let (_, snapshot) = admission.observed().unwrap();
        validate_observed_hit(snapshot, (1.5, 2.5), &visible).unwrap();
        assert_eq!(admission.local_point(&visible).unwrap(), (3.5, 2.5));
        let absent = observed_admission(
            vec![observed_row(8, 13, false, [-2.0, 0.0, 2.0, 1.0])],
            (3.5, 2.5),
        );
        assert_eq!(absent.popup_target().unwrap(), None);
        assert!(validate_observed_hit(absent.observed().unwrap().1, (1.5, 2.5), &visible).is_err());
    }

    #[test]
    fn changed_surface_lifetime_revision_or_covered_hit_refuses() {
        let popup = observed_row(8, 13, true, [1.0, 2.0, 1.0, 1.0]);
        let admission = observed_admission(vec![popup.clone()], (3.5, 2.5));
        let (_, snapshot) = admission.observed().unwrap();
        let mut hit = FrameHit {
            epoch: snapshot.epoch.clone(),
            tree_revision: snapshot.tree_revision,
            hit: popup,
        };
        hit.hit.lifetime = "abcdef1234567890abcdef1234567890".into();
        assert!(validate_observed_hit(snapshot, (1.5, 2.5), &hit).is_err());
        hit.hit.lifetime = snapshot.rows[0].lifetime.clone();
        hit.tree_revision += 1;
        assert!(validate_observed_hit(snapshot, (1.5, 2.5), &hit).is_err());
        hit.tree_revision -= 1;
        assert!(validate_observed_hit(snapshot, (1.5, 1.5), &hit).is_err());
    }

    #[test]
    fn overlapping_popup_owners_are_not_selected_by_inventory_order() {
        let first = observed_row(8, 13, true, [1.0, 2.0, 1.0, 1.0]);
        let second = observed_row(9, 14, true, [1.0, 2.0, 1.0, 1.0]);
        let admission = observed_admission(vec![first, second], (3.5, 2.5));
        assert!(admission.popup_target().is_err());
    }
    #[test]
    fn desktop_drag_refuses_popup_crossing_even_when_endpoints_are_clear() {
        let popup = [1.0, 2.0, 1.0, 1.0];
        assert!(segment_intersects_box((0.0, 2.5), (3.0, 2.5), popup));
        assert!(segment_intersects_box((1.0, 2.5), (0.0, 2.5), popup));
        assert!(!segment_intersects_box((0.0, 0.0), (3.0, 1.0), popup));
        assert!(!segment_intersects_box((0.0, 0.0), (0.0, 1.0), popup));
    }
}
