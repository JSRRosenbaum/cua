//! Read-only Hyprland identity and geometry, adapted from #3052.
//!
//! Native IDs are full compositor addresses, never title matches or truncated
//! protocol object IDs. IPC and capture must belong to the same compositor.
//!
//! Powered-output framing adapts CARLOSDAVID33's #4217; finite positive
//! fractional-scale logical sizing adapts lorenzozanee's #4244; rotated
//! logical axes adapt Osamah Beig's #3969. The combined per-output capture,
//! gap refusal, and capture-bound layout validation are downstream integration.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const QUERY_TIMEOUT: Duration = Duration::from_secs(1);
const QUERY_TOTAL_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_RETRY_BACKOFF: Duration = Duration::from_millis(50);
const QUERY_MAX_ATTEMPTS: usize = 2;
const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;
const MAX_LOGICAL_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
struct Workspace {
    id: i64,
}

#[derive(Clone, Debug, Deserialize)]
struct Client {
    address: String,
    mapped: bool,
    hidden: bool,
    pid: i64,
    title: String,
    class: String,
    at: [i32; 2],
    size: [i32; 2],
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Monitor {
    #[serde(rename = "activeWorkspace")]
    active_workspace: Workspace,
    #[serde(rename = "specialWorkspace")]
    special_workspace: Workspace,
    #[serde(rename = "dpmsStatus", default = "powered_by_default")]
    dpms_status: bool,
    #[serde(default)]
    disabled: bool,
}

#[derive(Clone, Debug, Deserialize)]
struct DisplayMonitor {
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: Option<i64>,
    width: u32,
    height: u32,
    scale: f64,
    x: i32,
    y: i32,
    transform: u32,
    #[serde(default)]
    disabled: bool,
    #[serde(rename = "dpmsStatus", default = "powered_by_default")]
    dpms_status: bool,
}

fn powered_by_default() -> bool {
    true
}

impl DisplayMonitor {
    fn powered(&self) -> bool {
        !self.disabled && self.dpms_status
    }
}

/// A powered output's logical rectangle and its physical screenshot transform.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameOutput {
    pub name: String,
    pub(crate) id: Option<i64>,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub(crate) native_width: u32,
    pub(crate) native_height: u32,
    pub(crate) scale_bits: u64,
    pub(crate) transform: u32,
}

/// The screenshot/action frame is the bounding rectangle of powered outputs.
/// Output gaps have pixels in the image but are never valid action points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopFrame {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub outputs: Vec<FrameOutput>,
}
/// Layout and compositor scene observed across a single desktop PNG capture.
/// Inventory can be absent only when native input inspection is unavailable.
#[derive(Clone, Debug)]
pub struct CapturedDesktop {
    pub frame: DesktopFrame,
    pub inventory: Option<super::hyprland_input::FrameInventory>,
}

/// Root-surface proof paired with a window-local export. Inventory includes
/// native and XWayland roots; XWayland additionally pins its exact XID so
/// foreground routing cannot substitute a newly mapped X client.
#[derive(Clone, Debug)]
pub struct CapturedWindowScene {
    pub inventory: Option<super::hyprland_input::FrameInventory>,
    pub xwayland: Option<super::hyprland_input::XWaylandCaptureProof>,
}

impl DesktopFrame {
    pub fn from_layout(&self, x: i32, y: i32) -> (i64, i64) {
        (
            i64::from(x) - i64::from(self.x),
            i64::from(y) - i64::from(self.y),
        )
    }

    /// Translate only screenshot points on a powered output; do not clamp a
    /// gap or out-of-bounds point into an adjacent monitor.
    pub fn to_layout(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.0
            || y < 0.0
            || x >= f64::from(self.width)
            || y >= f64::from(self.height)
        {
            bail!("desktop frame point is outside powered output bounds");
        }
        let layout_x = f64::from(self.x) + x;
        let layout_y = f64::from(self.y) + y;
        if !self.outputs.iter().any(|output| {
            layout_x >= f64::from(output.x)
                && layout_x < f64::from(output.x) + f64::from(output.width)
                && layout_y >= f64::from(output.y)
                && layout_y < f64::from(output.y) + f64::from(output.height)
        }) {
            bail!("desktop frame point falls in an output gap or DPMS-off region");
        }
        Ok((layout_x, layout_y))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Window {
    pub address: u64,
    pub pid: u32,
    pub title: String,
    pub app_id: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub workspace: i64,
    pub visible: bool,
    pub(crate) hidden: bool,
}

pub fn is_session() -> bool {
    !super::is_inject_mode()
        && std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some_and(|s| !s.is_empty())
}

fn ipc_path() -> Result<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?;
    let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
        .context("missing HYPRLAND_INSTANCE_SIGNATURE")?;
    if signature.is_empty()
        || !signature
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        bail!("invalid Hyprland instance signature");
    }
    let runtime = PathBuf::from(runtime);
    if !runtime.is_absolute() {
        bail!("XDG_RUNTIME_DIR must be absolute");
    }
    Ok(runtime.join("hypr").join(signature).join(".socket.sock"))
}

pub(super) fn peer_pid(fd: RawFd) -> Result<libc::pid_t> {
    let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if result != 0
        || len as usize != std::mem::size_of::<libc::ucred>()
        || cred.pid <= 0
        || cred.uid != unsafe { libc::geteuid() }
    {
        bail!("could not verify same-user compositor peer");
    }
    Ok(cred.pid)
}

fn ipc_connection() -> Result<UnixStream> {
    ipc_connection_with_timeout(QUERY_TIMEOUT)
}

fn ipc_connection_with_timeout(timeout: Duration) -> Result<UnixStream> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(ipc_path()?)?, timeout)?;
    Ok(socket.into())
}

/// Bound the socket connection as well as protocol dispatch. Inherited
/// WAYLAND_SOCKET descriptors are deliberately unsupported here: this adapter
/// opens independent, attested connections for each observation.
pub(super) fn wayland_connection() -> Result<wayland_client::Connection> {
    wayland_connection_with_timeout(QUERY_TIMEOUT)
}

fn wayland_connection_with_timeout(timeout: Duration) -> Result<wayland_client::Connection> {
    if std::env::var_os("WAYLAND_SOCKET").is_some() {
        bail!("Hyprland observation requires a named WAYLAND_DISPLAY socket");
    }
    let display =
        PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").context("missing WAYLAND_DISPLAY")?);
    let path = if display.is_absolute() {
        display
    } else {
        if display.components().count() != 1 {
            bail!("invalid WAYLAND_DISPLAY socket name");
        }
        let runtime =
            PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").context("missing XDG_RUNTIME_DIR")?);
        if !runtime.is_absolute() {
            bail!("XDG_RUNTIME_DIR must be absolute");
        }
        runtime.join(display)
    };
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.connect_timeout(&socket2::SockAddr::unix(path)?, timeout)?;
    wayland_client::Connection::from_socket(socket.into()).context("Wayland connection failed")
}

/// Bind the Wayland connection to the IPC compositor before accepting pixels.
pub(super) fn verify_capture_peer(connection: &wayland_client::Connection) -> Result<()> {
    let ipc = ipc_connection()?;
    if peer_pid(ipc.as_raw_fd())? != peer_pid(connection.backend().poll_fd().as_raw_fd())? {
        bail!("Hyprland IPC and WAYLAND_DISPLAY name different compositor processes");
    }
    Ok(())
}

fn query<T: serde::de::DeserializeOwned>(command: &str) -> Result<T> {
    let mut expected_peer = None;
    query_with(
        command,
        QUERY_TIMEOUT,
        QUERY_TOTAL_TIMEOUT,
        QUERY_RETRY_BACKOFF,
        |deadline| {
            let ipc = ipc_connection_with_timeout(query_time_remaining(deadline)?)?;
            // A stale inherited instance signature must not supply geometry for
            // a different nested desktop. Re-attest both sockets on every try.
            let wayland = wayland_connection_with_timeout(query_time_remaining(deadline)?)?;
            let peer = peer_pid(ipc.as_raw_fd())?;
            if peer != peer_pid(wayland.backend().poll_fd().as_raw_fd())? {
                bail!("Hyprland IPC and WAYLAND_DISPLAY name different compositor processes");
            }
            if expected_peer.is_some_and(|expected| expected != peer) {
                bail!("Hyprland compositor changed during observation retry");
            }
            expected_peer = Some(peer);
            Ok(ipc)
        },
    )
}

fn query_time_remaining(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "Hyprland IPC query timed out").into());
    }
    Ok(remaining)
}

fn is_query_timeout(error: &anyhow::Error) -> bool {
    error.downcast_ref::<io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    })
}

/// Retry only a timed-out observation, never an action or an identity failure.
/// Each attempt discards all previous bytes and opens newly attested sockets.
fn query_with<T: serde::de::DeserializeOwned>(
    command: &str,
    per_attempt: Duration,
    total: Duration,
    backoff: Duration,
    mut connect: impl FnMut(Instant) -> Result<UnixStream>,
) -> Result<T> {
    let deadline = Instant::now() + total;
    // Keep this a closed list: a generic "j/" prefix also admits JSON-formatted
    // dispatch commands. JSON output does not imply a read-only operation.
    let read_only = matches!(command, "j/monitors" | "j/clients" | "j/activewindow");
    for attempt in 1..=QUERY_MAX_ATTEMPTS {
        query_time_remaining(deadline)?;
        let attempt_deadline = deadline.min(Instant::now() + per_attempt);
        // Connection and peer-attestation errors are permanent. Only read-only
        // request/reply timeouts below are eligible for a new query.
        let mut ipc = connect(attempt_deadline)?;
        let reply = query_time_remaining(attempt_deadline)
            .and_then(|remaining| read_reply(&mut ipc, command.as_bytes(), remaining));
        match reply {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).context("invalid Hyprland IPC JSON")
            }
            Err(error) => {
                if !read_only
                    || !is_query_timeout(&error)
                    || attempt == QUERY_MAX_ATTEMPTS
                    || deadline.saturating_duration_since(Instant::now()) <= backoff
                {
                    return Err(error).with_context(|| {
                        format!("Hyprland IPC observation failed after {attempt} attempt(s)")
                    });
                }
                drop(ipc);
                tracing::warn!(
                    command,
                    attempt,
                    "Hyprland IPC observation timed out; retrying on a fresh connection"
                );
                std::thread::sleep(backoff);
            }
        }
    }
    unreachable!("the final query attempt always returns")
}

fn read_reply(stream: &mut UnixStream, command: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(command)?;
    let mut reply = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let remaining = query_time_remaining(deadline)?;
        stream.set_read_timeout(Some(remaining))?;
        let count = stream
            .read(&mut chunk)
            .context("Hyprland IPC reply unavailable")?;
        if count == 0 {
            break;
        }
        if reply.len() + count > MAX_REPLY_BYTES {
            bail!("Hyprland IPC reply exceeds size limit");
        }
        reply.extend_from_slice(&chunk[..count]);
    }
    Ok(reply)
}

fn windows_from_clients(clients: Vec<Client>, active: &HashSet<i64>) -> Result<Vec<Window>> {
    let mut seen = HashSet::new();
    let mut windows = Vec::new();
    for c in clients.into_iter().filter(|c| c.mapped) {
        let address = u64::from_str_radix(c.address.strip_prefix("0x").unwrap_or(&c.address), 16)?;
        let pid = u32::try_from(c.pid)?;
        let width = u32::try_from(c.size[0])?;
        let height = u32::try_from(c.size[1])?;
        if address == 0 || pid == 0 || !valid_dimensions(width, height) || !seen.insert(address) {
            bail!("invalid or duplicate Hyprland window identity/geometry");
        }
        windows.push(Window {
            address,
            pid,
            title: c.title,
            app_id: c.class,
            x: c.at[0],
            y: c.at[1],
            width,
            height,
            workspace: c.workspace.id,
            visible: !c.hidden && active.contains(&c.workspace.id),
            hidden: c.hidden,
        });
    }
    Ok(windows)
}

fn valid_dimensions(width: u32, height: u32) -> bool {
    width > 0 && height > 0 && u64::from(width) * u64::from(height) <= MAX_LOGICAL_PIXELS
}

/// Content-free logical geometry of the powered desktop action frame.
/// Different outputs may have different physical scales; this frame is 1:1.
pub fn screen_size() -> Result<(u32, u32, f64)> {
    screen_size_from_monitors(query("j/monitors")?)
}

fn screen_size_from_monitors(monitors: Vec<DisplayMonitor>) -> Result<(u32, u32, f64)> {
    let frame = desktop_frame_from_monitors(monitors)?;
    Ok((frame.width, frame.height, 1.0))
}

fn logical_output_size(monitor: &DisplayMonitor) -> Result<(u32, u32)> {
    if !monitor.scale.is_finite() || monitor.scale <= 0.0 || monitor.transform > 7 {
        bail!("invalid Hyprland output scale or transform");
    }
    if !valid_dimensions(monitor.width, monitor.height) {
        bail!("invalid Hyprland physical output dimensions");
    }
    let (width, height) = if monitor.transform % 2 == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    };
    let width = (f64::from(width) / monitor.scale).round();
    let height = (f64::from(height) / monitor.scale).round();
    if !width.is_finite()
        || !height.is_finite()
        || !(1.0..=f64::from(u32::MAX)).contains(&width)
        || !(1.0..=f64::from(u32::MAX)).contains(&height)
    {
        bail!("invalid Hyprland logical output dimensions");
    }
    let (width, height) = (width as u32, height as u32);
    if !valid_dimensions(width, height) {
        bail!("invalid Hyprland logical output dimensions");
    }
    Ok((width, height))
}

fn bounding_box(outputs: &[FrameOutput]) -> Result<(i32, i32, u32, u32)> {
    let min_x = outputs
        .iter()
        .map(|o| i64::from(o.x))
        .min()
        .context("no outputs")?;
    let min_y = outputs
        .iter()
        .map(|o| i64::from(o.y))
        .min()
        .context("no outputs")?;
    let max_x = outputs
        .iter()
        .map(|o| i64::from(o.x) + i64::from(o.width))
        .max()
        .unwrap();
    let max_y = outputs
        .iter()
        .map(|o| i64::from(o.y) + i64::from(o.height))
        .max()
        .unwrap();
    let width = u32::try_from(max_x - min_x)?;
    let height = u32::try_from(max_y - min_y)?;
    if !valid_dimensions(width, height)
        || max_x > i64::from(i32::MAX) + 1
        || max_y > i64::from(i32::MAX) + 1
    {
        bail!("invalid Hyprland desktop frame dimensions");
    }
    Ok((i32::try_from(min_x)?, i32::try_from(min_y)?, width, height))
}

fn output_from_monitor(monitor: DisplayMonitor) -> Result<FrameOutput> {
    let (width, height) = logical_output_size(&monitor)?;
    Ok(FrameOutput {
        name: monitor.name,
        id: monitor.id,
        x: monitor.x,
        y: monitor.y,
        width,
        height,
        native_width: monitor.width,
        native_height: monitor.height,
        scale_bits: monitor.scale.to_bits(),
        transform: monitor.transform,
    })
}

fn desktop_frame_from_monitors(monitors: Vec<DisplayMonitor>) -> Result<DesktopFrame> {
    let mut outputs: Vec<_> = monitors
        .into_iter()
        .filter(DisplayMonitor::powered)
        .map(output_from_monitor)
        .collect::<Result<_>>()?;
    if outputs.is_empty() {
        bail!("Hyprland has no powered output (disabled or DPMS standby)");
    }
    outputs.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    for (index, output) in outputs.iter().enumerate() {
        if output.name.is_empty()
            || outputs[..index].iter().any(|other| {
                other.name == output.name
                    || i64::from(other.x) < i64::from(output.x) + i64::from(output.width)
                        && i64::from(output.x) < i64::from(other.x) + i64::from(other.width)
                        && i64::from(other.y) < i64::from(output.y) + i64::from(output.height)
                        && i64::from(output.y) < i64::from(other.y) + i64::from(other.height)
            })
        {
            bail!("Hyprland outputs have ambiguous names or overlapping logical geometry");
        }
    }
    let (x, y, width, height) = bounding_box(&outputs)?;
    Ok(DesktopFrame {
        x,
        y,
        width,
        height,
        outputs,
    })
}

pub fn desktop_frame() -> Result<DesktopFrame> {
    desktop_frame_from_monitors(query("j/monitors")?)
}

/// Screen points are frame-relative; popup/window coordinates are separate.
pub fn frame_point_to_layout(x: f64, y: f64) -> Result<(f64, f64)> {
    desktop_frame()?.to_layout(x, y)
}

/// Virtual-pointer absolute motion spans enabled outputs, even DPMS-off ones.
fn pointer_layout_from_monitors(monitors: Vec<DisplayMonitor>) -> Result<(i32, i32, u32, u32)> {
    let outputs: Vec<_> = monitors
        .into_iter()
        .filter(|monitor| !monitor.disabled)
        .map(output_from_monitor)
        .collect::<Result<_>>()?;
    bounding_box(&outputs)
}

pub fn pointer_layout() -> Result<(i32, i32, u32, u32)> {
    pointer_layout_from_monitors(query("j/monitors")?)
}

pub fn monitor_report() -> Result<serde_json::Value> {
    let monitors: Vec<DisplayMonitor> = query("j/monitors")?;
    let frame = desktop_frame_from_monitors(monitors.clone())?;
    Ok(serde_json::Value::Array(
        monitors
            .into_iter()
            .filter(|m| !m.disabled)
            .map(|m| {
                let (fx, fy) = frame.from_layout(m.x, m.y);
                let powered = m.powered();
                let logical = logical_output_size(&m);
                serde_json::json!({
                    "name": m.name,
                    "width": logical.as_ref().ok().map(|v| v.0),
                    "height": logical.as_ref().ok().map(|v| v.1),
                    "scale": m.scale,
                    "transform": m.transform,
                    "powered": powered,
                    "frame_x": powered.then_some(fx),
                    "frame_y": powered.then_some(fy),
                })
            })
            .collect(),
    ))
}

pub fn cursor_position() -> Result<(i32, i32)> {
    #[derive(Deserialize)]
    struct CursorPos {
        x: f64,
        y: f64,
    }
    let pos: CursorPos = query("j/cursorpos")?;
    if !pos.x.is_finite() || !pos.y.is_finite() {
        bail!("invalid Hyprland cursor coordinates");
    }
    Ok((pos.x.round() as i32, pos.y.round() as i32))
}

/// `grim -s 1` composes/rotates each wl_output into its logical orientation
/// before encoding. Do not rotate its PNG a second time (see grim render.c).
fn validate_output_png(png: &[u8], output: &FrameOutput) -> Result<()> {
    let (width, height) = cua_driver_core::image_utils::png_dimensions(png)?;
    if (width, height) != (output.width, output.height) {
        bail!(
            "Hyprland output {} screenshot is {}x{}, expected logical {}x{}",
            output.name,
            width,
            height,
            output.width,
            output.height
        );
    }
    Ok(())
}

fn composite_output_png(
    canvas: &mut image::RgbaImage,
    frame: &DesktopFrame,
    output: &FrameOutput,
    png: &[u8],
) -> Result<()> {
    validate_output_png(png, output)?;
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.to_rgba8();
    let (left, top) = frame.from_layout(output.x, output.y);
    image::imageops::replace(canvas, &image, left, top);
    Ok(())
}

fn capture_scene_unchanged(
    before: &Option<super::hyprland_input::FrameInventory>,
    after: &Option<super::hyprland_input::FrameInventory>,
    frame: &DesktopFrame,
    live_frame: &DesktopFrame,
) -> Result<()> {
    if before != after || frame != live_frame {
        bail!("Hyprland scene or output geometry changed during capture; refresh screenshot");
    }
    Ok(())
}

pub fn capture_desktop_frame_png_with_geometry() -> Result<(Vec<u8>, CapturedDesktop)> {
    // Verify the two independently connected sockets name the same process;
    // a subprocess capture must not silently use an unrelated Wayland session.
    let connection = wayland_connection()?;
    verify_capture_peer(&connection)?;
    let before = super::hyprland_input::frame_inventory_if_available()?;
    let frame = desktop_frame()?;
    let mut canvas =
        image::RgbaImage::from_pixel(frame.width, frame.height, image::Rgba([0, 0, 0, 255]));
    for output in &frame.outputs {
        let captured = std::process::Command::new("timeout")
            .args([
                "--signal=KILL",
                "5s",
                "grim",
                "-s",
                "1",
                "-t",
                "png",
                "-o",
                output.name.as_str(),
                "-",
            ])
            .output()
            .context("bounded grim capture requires coreutils timeout and grim")?;
        if !captured.status.success() || captured.stdout.is_empty() {
            bail!(
                "grim could not capture powered output {}: {}",
                output.name,
                String::from_utf8_lossy(&captured.stderr)
            );
        }
        composite_output_png(&mut canvas, &frame, output, &captured.stdout)?;
    }
    let after = super::hyprland_input::frame_inventory_if_available()?;
    capture_scene_unchanged(&before, &after, &frame, &desktop_frame()?)?;
    verify_capture_peer(&connection)?;
    let mut encoded = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(canvas).write_to(&mut encoded, image::ImageFormat::Png)?;
    Ok((
        encoded.into_inner(),
        CapturedDesktop {
            frame,
            inventory: before,
        },
    ))
}

pub fn capture_desktop_frame_png() -> Result<Vec<u8>> {
    Ok(capture_desktop_frame_png_with_geometry()?.0)
}

fn active_workspace_ids(monitors: Vec<Monitor>) -> HashSet<i64> {
    monitors
        .into_iter()
        .filter(|monitor| !monitor.disabled && monitor.dpms_status)
        .flat_map(|monitor| [monitor.active_workspace.id, monitor.special_workspace.id])
        .filter(|id| *id != 0)
        .collect()
}

pub fn list_windows() -> Result<Vec<Window>> {
    let active = active_workspace_ids(query("j/monitors")?);
    windows_from_clients(query("j/clients")?, &active)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    Native,
    XWayland,
}
/// No mapped client has this exact compositor address. A mapped address
/// owned by another PID is an identity conflict, not absence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetAbsent;

impl std::fmt::Display for TargetAbsent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Hyprland target no longer exists")
    }
}

impl std::error::Error for TargetAbsent {}

#[derive(Deserialize)]
struct ClientKind {
    address: String,
    pid: i64,
    mapped: bool,
    // Required: absent metadata must never silently classify an XWayland
    // client as native input compatible.
    xwayland: bool,
}

fn target_kind_from_clients(
    clients: Vec<ClientKind>,
    pid: u32,
    address: u64,
) -> Result<TargetKind> {
    let mut found = None;
    for client in clients {
        if !client.mapped {
            continue;
        }
        let observed = u64::from_str_radix(
            client.address.strip_prefix("0x").unwrap_or(&client.address),
            16,
        )?;
        if observed == address {
            if found.is_some() || client.pid != i64::from(pid) {
                bail!("Hyprland native target identity is ambiguous or belongs to another PID");
            }
            found = Some(if client.xwayland {
                TargetKind::XWayland
            } else {
                TargetKind::Native
            });
        }
    }
    found.ok_or_else(|| anyhow::Error::new(TargetAbsent))
}

/// Re-read exact compositor identity before selecting native or XWayland route.
pub fn target_kind(pid: u32, address: u64) -> Result<TargetKind> {
    target_kind_from_clients(query("j/clients")?, pid, address)
}

/// An exact layer-shell surface, not an application window. The namespace is
/// client-declared, so it cannot prove a protected/authentication role.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerTarget {
    pub address: u64,
    pub namespace: String,
    pub monitor: String,
    pub level: u8,
    pub x: i64,
    pub y: i64,
    pub width: u32,
    pub height: u32,
}

#[derive(Deserialize)]
struct LayerReport {
    levels: HashMap<String, Vec<LayerData>>,
}

#[derive(Deserialize)]
struct LayerData {
    address: String,
    namespace: String,
    // HyprCtl.cpp emits m_geometry after layer arrangement has already
    // positioned it in compositor-global logical coordinates.
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

fn layer_at_from_reports(
    frame: &DesktopFrame,
    layers: &HashMap<String, LayerReport>,
    x: i32,
    y: i32,
) -> Result<Option<LayerTarget>> {
    frame.to_layout(
        (i64::from(x) - i64::from(frame.x)) as f64,
        (i64::from(y) - i64::from(frame.y)) as f64,
    )?;
    let output = frame
        .outputs
        .iter()
        .find(|output| {
            i64::from(output.x) <= i64::from(x)
                && i64::from(x) < i64::from(output.x) + i64::from(output.width)
                && i64::from(output.y) <= i64::from(y)
                && i64::from(y) < i64::from(output.y) + i64::from(output.height)
        })
        .context("layer point is outside powered output geometry")?;
    let report = layers
        .get(&output.name)
        .context("Hyprland layer inventory omits the selected powered output")?;
    let mut highest: Option<LayerTarget> = None;
    for (level, entries) in &report.levels {
        let level: u8 = level.parse().context("invalid Hyprland layer level")?;
        if level > 3 {
            bail!("unsupported Hyprland layer level");
        }
        for layer in entries {
            let width = u32::try_from(layer.w).context("invalid Hyprland layer width")?;
            let height = u32::try_from(layer.h).context("invalid Hyprland layer height")?;
            if width == 0 || height == 0 {
                continue;
            }
            let left = i64::from(layer.x);
            let top = i64::from(layer.y);
            if i64::from(x) < left
                || i64::from(x) >= left + i64::from(width)
                || i64::from(y) < top
                || i64::from(y) >= top + i64::from(height)
            {
                continue;
            }
            let address = u64::from_str_radix(
                layer.address.strip_prefix("0x").unwrap_or(&layer.address),
                16,
            )?;
            if address == 0 {
                bail!("invalid Hyprland layer identity");
            }
            let candidate = LayerTarget {
                address,
                namespace: layer.namespace.clone(),
                monitor: output.name.clone(),
                level,
                x: left,
                y: top,
                width,
                height,
            };
            if highest.as_ref().is_some_and(|prior| prior.level == level) {
                bail!("overlapping Hyprland layers at the same level are ambiguous");
            }
            if highest.as_ref().is_none_or(|prior| level > prior.level) {
                highest = Some(candidate);
            }
        }
    }
    Ok(highest)
}

/// Observe a layer at a compositor-global logical point. This never turns a
/// layer into an app PID or claims its client namespace is a protected role.
pub fn layer_at(x: i32, y: i32) -> Result<Option<LayerTarget>> {
    let frame = desktop_frame()?;
    let layers = query("j/layers")?;
    let layer = layer_at_from_reports(&frame, &layers, x, y)?;
    if frame != desktop_frame()? {
        bail!("Hyprland output geometry changed during layer observation");
    }
    Ok(layer)
}

pub fn window_for_address(address: u64) -> Option<Window> {
    list_windows()
        .ok()?
        .into_iter()
        .find(|w| w.address == address)
}

/// AT-SPI has no native Hyprland handle. Correlate only when the title is
/// unique among this PID's mapped compositor clients, as well as AX roots.
pub fn accessibility_window(address: u64, pid: u32) -> Option<Window> {
    accessibility_target(&list_windows().ok()?, address, pid)
}

fn accessibility_target(windows: &[Window], address: u64, pid: u32) -> Option<Window> {
    let target = windows
        .iter()
        .find(|w| w.address == address && w.pid == pid)?;
    (!target.title.is_empty()
        && windows
            .iter()
            .filter(|w| w.pid == pid && w.title == target.title)
            .count()
            == 1)
        .then(|| target.clone())
}

/// The legacy PID-only bounds caller has no window identity: allow only a
/// single mapped client. Explicit IDs never fall back to this function.
pub fn window_for_pid(pid: u32) -> Option<Window> {
    let mut owned = list_windows().ok()?.into_iter().filter(|w| w.pid == pid);
    let first = owned.next()?;
    owned.next().is_none().then_some(first)
}

/// Process identity attested independently over Wayland and Hyprland IPC.
pub(crate) fn verified_compositor_pid() -> Result<u32> {
    let connection = wayland_connection()?;
    verify_capture_peer(&connection)?;
    u32::try_from(peer_pid(connection.backend().poll_fd().as_raw_fd())?)
        .context("invalid Hyprland compositor PID")
}

fn active_window_from_json(active: &serde_json::Value) -> Result<Option<(u64, u32)>> {
    let object = active
        .as_object()
        .context("invalid Hyprland active window response")?;
    if object.is_empty() {
        return Ok(None);
    }
    let address = object
        .get("address")
        .and_then(|value| value.as_str())
        .context("Hyprland active window has no address")?;
    let pid = object
        .get("pid")
        .and_then(|value| value.as_u64())
        .context("Hyprland active window has no process ID")?;
    let address = u64::from_str_radix(address.strip_prefix("0x").unwrap_or(address), 16)
        .context("invalid Hyprland active window address")?;
    let pid = u32::try_from(pid)?;
    if address == 0 || pid == 0 {
        bail!("invalid Hyprland active window identity");
    }
    Ok(Some((address, pid)))
}

/// Fresh attested focus observation; empty object means no active window.
pub fn active_window_identity() -> Result<Option<(u64, u32)>> {
    active_window_from_json(&query::<serde_json::Value>("j/activewindow")?)
}

pub fn target_is_active(address: u64, pid: Option<u32>) -> Result<bool> {
    let target = window_for_address(address).context("Hyprland target no longer exists")?;
    if pid.is_some_and(|pid| pid != target.pid) {
        bail!("Hyprland target belongs to a different process");
    }
    Ok(active_window_identity()? == Some((address, target.pid)))
}

fn capture_target(windows: &[Window], address: u64, pid: Option<u32>) -> Result<Window> {
    let target = windows
        .iter()
        .find(|w| w.address == address)
        .context("requested Hyprland window no longer exists; refresh list_windows")?;
    if pid.is_some_and(|pid| target.pid != pid) || target.hidden {
        bail!("requested Hyprland target ownership/visibility is unproven");
    }
    // The v1 wire request takes only the low word. Refuse collisions across
    // ALL mapped clients, including other processes and hidden windows.
    if windows
        .iter()
        .filter(|w| w.address as u32 == address as u32)
        .count()
        != 1
    {
        bail!("Hyprland toplevel export handle is ambiguous");
    }
    Ok(target.clone())
}

/// Window-local export contains the root/subsurface buffer, never an
/// off-base xdg popup. Snapshot the same scene protocol before and after
/// copying pixels so a recycled address or same-box replacement cannot
/// inherit screenshot authority.
pub fn capture_with_geometry(
    address: u64,
    pid: Option<u32>,
) -> Result<(Vec<u8>, Window, DesktopFrame, CapturedWindowScene)> {
    let inventory = super::hyprland_input::frame_inventory_if_available()?;
    let frame = desktop_frame()?;
    let before = capture_target(&list_windows()?, address, pid)?;
    let kind = target_kind(before.pid, before.address)?;
    let xwayland = if kind == TargetKind::XWayland && inventory.is_some() {
        Some(super::hyprland_input::inspect_xwayland_capture(
            before.pid,
            before.address,
        )?)
    } else {
        None
    };
    if let Some(snapshot) = &inventory {
        if !snapshot.rows.iter().any(|row| {
            row.pid == before.pid
                && row.address == before.address
                && !row.popup
                && xwayland.as_ref().is_none_or(|proof| row == &proof.root)
        }) || xwayland.as_ref().is_some_and(|proof| {
            proof.epoch != snapshot.epoch || proof.tree_revision != snapshot.tree_revision
        }) {
            bail!("Hyprland root surface absent from capture scene; refresh screenshot");
        }
    }
    let bytes = super::hyprland_capture::capture_toplevel_png(address)?;
    let after = capture_target(&list_windows()?, address, pid)?;
    let live_inventory = super::hyprland_input::frame_inventory_if_available()?;
    let live_xwayland = if xwayland.is_some() {
        Some(super::hyprland_input::inspect_xwayland_capture(
            after.pid,
            after.address,
        )?)
    } else {
        None
    };
    capture_scene_unchanged(&inventory, &live_inventory, &frame, &desktop_frame()?)?;
    if before != after
        || kind != target_kind(after.pid, after.address)?
        || xwayland != live_xwayland
    {
        bail!("Hyprland target or native/XWayland kind changed during capture; refresh screenshot");
    }
    // XWayland's separately inspected XID and root lifetime augment, rather
    // than replace, its observed inventory root row.
    let scene = CapturedWindowScene {
        inventory,
        xwayland,
    };
    // The export buffer belongs to this toplevel only; normalize its render
    // scale into local logical coordinates. Never add the monitor/frame origin.
    if cua_driver_core::image_utils::png_dimensions(&bytes)? == (before.width, before.height) {
        return Ok((bytes, before, frame, scene));
    }
    let image = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)?;
    let image = image.resize_exact(
        before.width,
        before.height,
        image::imageops::FilterType::Triangle,
    );
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png)?;
    Ok((out.into_inner(), before, frame, scene))
}

pub fn capture(address: u64, pid: Option<u32>) -> Result<Vec<u8>> {
    Ok(capture_with_geometry(address, pid)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(address: u64, pid: u32) -> Window {
        Window {
            address,
            pid,
            title: "Fixture".into(),
            app_id: "fixture".into(),
            x: 967,
            y: 38,
            width: 800,
            height: 600,
            workspace: 1,
            visible: true,
            hidden: false,
        }
    }

    #[test]
    fn exact_identity_never_falls_back_to_sibling_or_other_pid() {
        let windows = [window(0x10, 42), window(0x20, 42)];
        assert_eq!(
            capture_target(&windows, 0x10, Some(42)).unwrap().address,
            0x10
        );
        assert!(capture_target(&windows, 0x30, Some(42)).is_err());
        assert!(capture_target(&windows, 0x10, Some(43)).is_err());
    }

    #[test]
    fn accessibility_correlation_rejects_duplicate_compositor_titles() {
        let a = window(0x10, 42);
        let b = window(0x20, 42);
        assert!(accessibility_target(&[a.clone()], a.address, a.pid).is_some());
        assert!(accessibility_target(&[a.clone(), b], a.address, a.pid).is_none());
    }

    #[test]
    fn truncated_handle_collision_refuses_even_across_pids() {
        assert!(capture_target(
            &[window(0x100000010, 42), window(0x200000010, 43)],
            0x100000010,
            Some(42)
        )
        .is_err());
    }

    #[test]
    fn off_workspace_target_is_capturable_but_hidden_target_refuses() {
        let mut target = window(0x10, 42);
        target.visible = false;
        assert!(capture_target(&[target.clone()], 0x10, Some(42)).is_ok());
        target.hidden = true;
        assert!(capture_target(&[target], 0x10, Some(42)).is_err());
    }

    #[test]
    fn stalled_ipc_reply_is_bounded() {
        let (mut client, _server) = UnixStream::pair().unwrap();
        let start = Instant::now();
        assert!(read_reply(&mut client, b"j/clients", Duration::from_millis(30)).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    fn replying_ipc(bytes: Vec<u8>, delay: Duration) -> UnixStream {
        let (client, mut server) = UnixStream::pair().unwrap();
        std::thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0; 128];
            assert!(server.read(&mut request).unwrap() > 0);
            std::thread::sleep(delay);
            // An oversized reply may be rejected before the writer finishes.
            let _ = server.write_all(&bytes);
        });
        client
    }

    #[test]
    fn timed_out_observation_uses_fresh_connection_and_discards_partial_bytes() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let value: serde_json::Value = query_with(
            "j/clients",
            Duration::from_millis(30),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                if attempts == 1 {
                    let (client, mut server) = UnixStream::pair()?;
                    server.write_all(b"{\"stale\":")?;
                    stalled.push(server);
                    Ok(client)
                } else {
                    Ok(replying_ipc(b"[]".to_vec(), Duration::ZERO))
                }
            },
        )
        .unwrap();
        assert_eq!(value, serde_json::json!([]));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn healthy_slow_observation_is_not_retried() {
        let mut attempts = 0;
        let value: serde_json::Value = query_with(
            "j/monitors",
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                Ok(replying_ipc(b"[]".to_vec(), Duration::from_millis(30)))
            },
        )
        .unwrap();
        assert_eq!(value, serde_json::json!([]));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn observation_connection_and_attestation_errors_are_not_retried() {
        for timeout in [false, true] {
            let mut attempts = 0;
            let error = query_with::<serde_json::Value>(
                "j/clients",
                QUERY_TIMEOUT,
                QUERY_TOTAL_TIMEOUT,
                QUERY_RETRY_BACKOFF,
                |_| {
                    attempts += 1;
                    if timeout {
                        return Err(io::Error::from(io::ErrorKind::TimedOut).into());
                    }
                    bail!("could not verify same-user compositor peer");
                },
            )
            .unwrap_err();
            assert_eq!(attempts, 1);
            assert!(timeout || error.to_string().contains("same-user compositor peer"));
        }
    }

    #[test]
    fn observation_retry_must_pass_fresh_attestation() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let error = query_with::<serde_json::Value>(
            "j/activewindow",
            Duration::from_millis(30),
            Duration::from_secs(1),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                if attempts == 2 {
                    bail!("Hyprland compositor changed during observation retry");
                }
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert_eq!(attempts, 2);
        assert!(error.to_string().contains("compositor changed"));
    }

    #[test]
    fn malformed_truncated_and_oversized_observations_are_not_retried() {
        for bytes in [
            b"invalid".to_vec(),
            b"{\"pid\"".to_vec(),
            vec![b' '; MAX_REPLY_BYTES + 1],
        ] {
            let mut attempts = 0;
            assert!(query_with::<serde_json::Value>(
                "j/clients",
                QUERY_TIMEOUT,
                QUERY_TOTAL_TIMEOUT,
                QUERY_RETRY_BACKOFF,
                |_| {
                    attempts += 1;
                    Ok(replying_ipc(bytes.clone(), Duration::ZERO))
                },
            )
            .is_err());
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn observation_total_deadline_includes_connect_and_read() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let start = Instant::now();
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(80),
            Duration::from_millis(110),
            Duration::from_millis(1),
            |deadline| {
                attempts += 1;
                assert!(
                    deadline.saturating_duration_since(Instant::now()) <= Duration::from_millis(80)
                );
                std::thread::sleep(Duration::from_millis(10));
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert!(attempts <= QUERY_MAX_ATTEMPTS);
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn observation_attempt_cap_applies_with_a_generous_total_budget() {
        let mut attempts = 0;
        let mut stalled = Vec::new();
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(30),
            Duration::from_secs(10),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                let (client, server) = UnixStream::pair()?;
                stalled.push(server);
                Ok(client)
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert_eq!(attempts, 2);
    }

    #[test]
    fn observation_never_writes_after_connect_consumes_total_deadline() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let mut client = Some(client);
        let mut attempts = 0;
        let error = query_with::<serde_json::Value>(
            "j/clients",
            Duration::from_millis(10),
            Duration::from_millis(10),
            Duration::from_millis(1),
            |_| {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(30));
                Ok(client.take().unwrap())
            },
        )
        .unwrap_err();
        assert!(is_query_timeout(&error));
        assert_eq!(attempts, 1);
        // The client was dropped without sending a request: EOF, not bytes.
        assert_eq!(server.read(&mut [0; 64]).unwrap(), 0);
    }

    #[test]
    fn input_and_unknown_json_commands_are_never_retried() {
        for command in ["dispatch nop", "j/dispatch nop", "j/unknown"] {
            let mut attempts = 0;
            let mut stalled = Vec::new();
            assert!(query_with::<serde_json::Value>(
                command,
                Duration::from_millis(30),
                Duration::from_secs(1),
                Duration::from_millis(1),
                |_| {
                    attempts += 1;
                    let (client, server) = UnixStream::pair()?;
                    stalled.push(server);
                    Ok(client)
                },
            )
            .is_err());
            assert_eq!(attempts, 1);
        }
    }

    #[test]
    fn observation_timeout_classifier_is_closed() {
        for (kind, expected) in [
            (io::ErrorKind::WouldBlock, true),
            (io::ErrorKind::TimedOut, true),
            (io::ErrorKind::ConnectionRefused, false),
            (io::ErrorKind::ConnectionReset, false),
            (io::ErrorKind::Interrupted, false),
            (io::ErrorKind::UnexpectedEof, false),
            (io::ErrorKind::PermissionDenied, false),
            (io::ErrorKind::InvalidData, false),
        ] {
            let error = anyhow::Error::from(io::Error::from(kind)).context("query");
            assert_eq!(is_query_timeout(&error), expected);
        }
        assert!(!is_query_timeout(&anyhow::anyhow!("identity unproven")));
        assert!(is_query_timeout(
            &query_time_remaining(Instant::now()).unwrap_err()
        ));
    }

    #[test]
    fn logical_resize_is_bounded_before_allocation() {
        assert!(valid_dimensions(3840, 2160));
        assert!(!valid_dimensions(0, 1));
        assert!(!valid_dimensions(u32::MAX, u32::MAX));
    }

    fn display_monitor() -> DisplayMonitor {
        serde_json::from_value(serde_json::json!({
            "name": "main", "id": 1,
            "width": 1920, "height": 1080, "scale": 1.0,
            "x": 0, "y": 0, "transform": 0,
        }))
        .unwrap()
    }

    fn monitor_at(
        name: &str,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        scale: f64,
        transform: u32,
    ) -> DisplayMonitor {
        let mut monitor = display_monitor();
        monitor.name = name.into();
        (
            monitor.x,
            monitor.y,
            monitor.width,
            monitor.height,
            monitor.scale,
            monitor.transform,
        ) = (x, y, width, height, scale, transform);
        monitor
    }

    #[test]
    fn mixed_scale_rotation_and_negative_origin_map_only_powered_output_points() {
        let left = monitor_at("left", -300, 0, 600, 400, 2.0, 0);
        let right = monitor_at("right", 0, 400, 800, 1200, 1.5, 1);
        let frame = desktop_frame_from_monitors(vec![right.clone(), left.clone()]).unwrap();
        assert_eq!(
            (frame.x, frame.y, frame.width, frame.height),
            (-300, 0, 1100, 933)
        );
        assert_eq!(
            (frame.outputs[0].width, frame.outputs[0].height),
            (300, 200)
        );
        assert_eq!(
            (frame.outputs[1].width, frame.outputs[1].height),
            (800, 533)
        );
        assert_eq!(frame.to_layout(299.0, 199.0).unwrap(), (-1.0, 199.0));
        assert_eq!(frame.to_layout(300.0, 400.0).unwrap(), (0.0, 400.0));
        for point in [(300.0, 199.0), (299.0, 400.0), (1100.0, 500.0), (-1.0, 0.0)] {
            assert!(frame.to_layout(point.0, point.1).is_err(), "{point:?}");
        }
        assert_eq!(
            screen_size_from_monitors(vec![left, right]).unwrap(),
            (1100, 933, 1.0)
        );
    }

    #[test]
    fn dpms_hotplug_and_same_dimensions_reconfig_change_frame_identity() {
        let left = monitor_at("left", -300, 0, 600, 400, 2.0, 0);
        let right = monitor_at("right", 0, 400, 800, 1200, 1.5, 1);
        let original = desktop_frame_from_monitors(vec![left.clone(), right.clone()]).unwrap();
        let mut off = left.clone();
        off.dpms_status = false;
        let standby = desktop_frame_from_monitors(vec![off, right.clone()]).unwrap();
        assert_ne!(original, standby);
        assert_eq!(
            (standby.x, standby.y, standby.width, standby.height),
            (0, 400, 800, 533)
        );
        let unplugged = desktop_frame_from_monitors(vec![right.clone()]).unwrap();
        assert_eq!(standby, unplugged);
        let mut changed = right.clone();
        changed.scale = 1.0;
        changed.width = 533;
        changed.height = 800;
        assert_ne!(
            original,
            desktop_frame_from_monitors(vec![left, changed]).unwrap()
        );
        let mut all_off = right;
        all_off.dpms_status = false;
        assert!(desktop_frame_from_monitors(vec![all_off]).is_err());
    }

    #[test]
    fn capture_refuses_scene_change_even_when_output_geometry_is_identical() {
        let frame = desktop_frame_from_monitors(vec![display_monitor()]).unwrap();
        let initial = Some(super::super::hyprland_input::FrameInventory {
            epoch: "1234567890abcdef1234567890abcdef".into(),
            tree_revision: 40,
            rows: vec![],
        });
        assert!(capture_scene_unchanged(&initial, &initial, &frame, &frame).is_ok());
        let mut workspace_switch = initial.clone();
        workspace_switch.as_mut().unwrap().tree_revision += 1;
        assert!(capture_scene_unchanged(&initial, &workspace_switch, &frame, &frame).is_err());
        assert!(capture_scene_unchanged(&initial, &None, &frame, &frame).is_err());
        let mut moved_output = frame.clone();
        moved_output.outputs[0].x += 1;
        assert!(capture_scene_unchanged(&initial, &initial, &frame, &moved_output).is_err());
    }

    #[test]
    fn virtual_pointer_extent_keeps_enabled_standby_output_but_actions_do_not() {
        let mut left = monitor_at("left", -300, 0, 600, 400, 2.0, 0);
        left.dpms_status = false;
        let right = monitor_at("right", 0, 400, 800, 1200, 1.5, 1);
        assert_eq!(
            pointer_layout_from_monitors(vec![left.clone(), right.clone()]).unwrap(),
            (-300, 0, 1100, 933)
        );
        let frame = desktop_frame_from_monitors(vec![left.clone(), right.clone()]).unwrap();
        assert_eq!((frame.x, frame.y), (0, 400));
        assert!(frame.to_layout(-299.0, -390.0).is_err());
        left.disabled = true;
        assert_eq!(
            pointer_layout_from_monitors(vec![left, right]).unwrap(),
            (0, 400, 800, 533)
        );
    }

    #[test]
    fn standby_and_disabled_workspaces_do_not_report_windows_visible() {
        let monitors: Vec<Monitor> = serde_json::from_value(serde_json::json!([
            {"activeWorkspace":{"id":1}, "specialWorkspace":{"id":0},
             "dpmsStatus":true, "disabled":false},
            {"activeWorkspace":{"id":2}, "specialWorkspace":{"id":3},
             "dpmsStatus":false, "disabled":false},
            {"activeWorkspace":{"id":4}, "specialWorkspace":{"id":0},
             "dpmsStatus":true, "disabled":true}
        ]))
        .unwrap();
        assert_eq!(active_workspace_ids(monitors), HashSet::from([1]));
    }

    #[test]
    fn display_identity_refuses_invalid_layouts_without_guessing_origin() {
        assert!(screen_size_from_monitors(vec![]).is_err());
        for scale in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut monitor = display_monitor();
            monitor.scale = scale;
            assert!(screen_size_from_monitors(vec![monitor]).is_err());
        }
        let monitor = display_monitor();
        assert!(screen_size_from_monitors(vec![monitor.clone(), monitor.clone()]).is_err());
        let mut invalid = monitor.clone();
        invalid.transform = 8;
        assert!(screen_size_from_monitors(vec![invalid]).is_err());
        let mut overlapping = monitor.clone();
        overlapping.name = "other".into();
        overlapping.x = 100;
        assert!(screen_size_from_monitors(vec![monitor, overlapping]).is_err());
    }

    #[test]
    fn oriented_scaled_outputs_composite_at_logical_positions_not_physical_offsets() {
        let left = monitor_at("left", -2, 0, 4, 2, 2.0, 0);
        let right = monitor_at("right", 1, 2, 2, 4, 2.0, 1);
        let frame = desktop_frame_from_monitors(vec![left, right]).unwrap();
        assert_eq!((frame.width, frame.height), (5, 3));
        let mut canvas =
            image::RgbaImage::from_pixel(frame.width, frame.height, image::Rgba([0, 0, 0, 255]));
        let png = |color| {
            let mut bytes = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(2, 1, color))
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let red = image::Rgba([255, 0, 0, 255]);
        let blue = image::Rgba([0, 0, 255, 255]);
        let red_png = png(red);
        let blue_png = png(blue);
        composite_output_png(&mut canvas, &frame, &frame.outputs[0], &red_png).unwrap();
        composite_output_png(&mut canvas, &frame, &frame.outputs[1], &blue_png).unwrap();
        assert_eq!(*canvas.get_pixel(0, 0), red);
        assert_eq!(*canvas.get_pixel(1, 0), red);
        assert_eq!(*canvas.get_pixel(3, 2), blue);
        assert_eq!(*canvas.get_pixel(4, 2), blue);
        assert_eq!(*canvas.get_pixel(2, 1), image::Rgba([0, 0, 0, 255]));
        let physical_png = {
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image::RgbaImage::new(4, 2))
                .write_to(&mut out, image::ImageFormat::Png)
                .unwrap();
            out.into_inner()
        };
        assert!(
            composite_output_png(&mut canvas, &frame, &frame.outputs[0], &physical_png).is_err()
        );
    }

    #[test]
    fn exact_target_kind_requires_compositor_xwayland_metadata() {
        let parse = |value| serde_json::from_value::<Vec<ClientKind>>(value);
        assert!(parse(serde_json::json!([
            {"address":"0x10","pid":42,"mapped":true}
        ]))
        .is_err());
        let native = parse(serde_json::json!([
            {"address":"0x10","pid":42,"mapped":true,"xwayland":false}
        ]))
        .unwrap();
        assert_eq!(
            target_kind_from_clients(native, 42, 0x10).unwrap(),
            TargetKind::Native
        );
        let x11 = parse(serde_json::json!([
            {"address":"0x10","pid":42,"mapped":true,"xwayland":true}
        ]))
        .unwrap();
        assert_eq!(
            target_kind_from_clients(x11, 42, 0x10).unwrap(),
            TargetKind::XWayland
        );
        let wrong_pid = parse(serde_json::json!([
            {"address":"0x10","pid":43,"mapped":true,"xwayland":false}
        ]))
        .unwrap();
        assert!(target_kind_from_clients(wrong_pid, 42, 0x10)
            .unwrap_err()
            .downcast_ref::<TargetAbsent>()
            .is_none());
        let unmapped = parse(serde_json::json!([
            {"address":"0x10","pid":42,"mapped":false,"xwayland":false}
        ]))
        .unwrap();
        assert!(target_kind_from_clients(unmapped, 42, 0x10)
            .unwrap_err()
            .downcast_ref::<TargetAbsent>()
            .is_some());
        assert!(target_kind_from_clients(vec![], 42, 0x10)
            .unwrap_err()
            .downcast_ref::<TargetAbsent>()
            .is_some());
    }

    #[test]
    fn active_window_requires_exact_pid_and_address() {
        assert_eq!(
            active_window_from_json(&serde_json::json!({})).unwrap(),
            None
        );
        assert_eq!(
            active_window_from_json(&serde_json::json!({
                "address":"0x2a","pid":7
            }))
            .unwrap(),
            Some((42, 7))
        );
        assert!(active_window_from_json(&serde_json::json!({"address":"0x2a"})).is_err());
        assert!(active_window_from_json(&serde_json::json!({"address":"0x0","pid":7})).is_err());
    }

    #[test]
    fn layer_inventory_uses_global_coordinates_on_offset_outputs() {
        let left = monitor_at("left", -200, 0, 400, 200, 2.0, 0);
        let right = monitor_at("right", 100, 100, 400, 200, 2.0, 0);
        let frame = desktop_frame_from_monitors(vec![left, right]).unwrap();
        let reports: HashMap<String, LayerReport> = serde_json::from_value(serde_json::json!({
            "right": {"levels": {
                "2": [{"address":"0x10","namespace":"client-declared",
                       "x":110,"y":110,"w":30,"h":20}],
                "3": []
            }}
        }))
        .unwrap();
        let target = layer_at_from_reports(&frame, &reports, 115, 115)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                target.address,
                target.x,
                target.y,
                target.width,
                target.height,
                target.level
            ),
            (16, 110, 110, 30, 20, 2)
        );
        assert_eq!(target.namespace, "client-declared");
        assert!(layer_at_from_reports(&frame, &reports, 0, 100).is_err());
        assert!(layer_at_from_reports(&frame, &reports, 155, 115)
            .unwrap()
            .is_none());
        let negative: HashMap<String, LayerReport> = serde_json::from_value(serde_json::json!({
            "left": {"levels": {"2": [
                {"address":"0x30","namespace":"left-layer","x":-190,"y":10,"w":30,"h":20}
            ]}}
        }))
        .unwrap();
        let left_target = layer_at_from_reports(&frame, &negative, -185, 15)
            .unwrap()
            .unwrap();
        assert_eq!((left_target.x, left_target.y), (-190, 10));
        let ambiguous: HashMap<String, LayerReport> = serde_json::from_value(serde_json::json!({
            "right": {"levels": {"2": [
                {"address":"0x10","namespace":"a","x":110,"y":110,"w":30,"h":20},
                {"address":"0x20","namespace":"b","x":112,"y":110,"w":30,"h":20}
            ]}}
        }))
        .unwrap();
        assert!(layer_at_from_reports(&frame, &ambiguous, 115, 115).is_err());
    }

    #[test]
    fn display_identity_rejects_empty_oversized_and_missing_geometry() {
        for (width, height) in [(0, 1080), (1920, 0), (u32::MAX, u32::MAX)] {
            let mut monitor = display_monitor();
            (monitor.width, monitor.height) = (width, height);
            assert!(screen_size_from_monitors(vec![monitor]).is_err());
        }
        let value = serde_json::json!({
            "width": 1920, "height": 1080, "scale": 1.0, "x": 0, "y": 0,
        });
        assert!(serde_json::from_value::<DisplayMonitor>(value).is_err());
    }
}
