//! Explicitly owned X11 applications on a private Xpra X server.
//!
//! This backend never adopts a host window. It starts a new X server, a
//! separate Driver runtime bound to that server, and the requested executable.
//! Its socket must be selected explicitly by a client; no host action is
//! silently retried here. Dropping/stopping the owner kills only these children.

use anyhow::{bail, Context, Result};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::LazyLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    /// Private MCP service endpoint, not the host desktop's canonical socket.
    pub socket: PathBuf,
    /// A new X server, not the host's XWayland DISPLAY.
    pub display: String,
    /// PID of the newly launched application process (not the Xpra server).
    pub child_pid: u32,
}

struct OwnedSessionController {
    owner: String,
    stop: Sender<()>,
    supervisor: Option<JoinHandle<()>>,
}

impl Drop for OwnedSessionController {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
    }
}

struct OwnedSession {
    app: Child,
    driver: Child,
    xpra: Child,
    bus: Child,
    directory: PathBuf,
}

fn terminate(child: &mut Child, grace: Duration) {
    if child.try_wait().ok().flatten().is_some() {
        return;
    }
    // The unreaped direct child retains its PID, so no unrelated process can
    // receive this signal. Give Xpra time to shut down its own Xorg child.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for OwnedSession {
    fn drop(&mut self) {
        // Only direct children: never a recycled display, group or PID file.
        terminate(&mut self.app, Duration::from_millis(500));
        terminate(&mut self.driver, Duration::from_secs(1));
        terminate(&mut self.xpra, Duration::from_secs(2));
        terminate(&mut self.bus, Duration::from_millis(300));
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn sessions() -> &'static Mutex<HashMap<String, OwnedSessionController>> {
    static SESSIONS: LazyLock<Mutex<HashMap<String, OwnedSessionController>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &SESSIONS
}

fn owned_directory() -> Result<(String, PathBuf)> {
    let runtime = PathBuf::from(
        std::env::var_os("XDG_RUNTIME_DIR")
            .context("XDG_RUNTIME_DIR required for an owned Xpra session")?,
    );
    let metadata = fs::symlink_metadata(&runtime)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.permissions().mode() & 0o077 == 0,
        "owned session requires a private user-owned XDG_RUNTIME_DIR"
    );
    for _ in 0..4 {
        let mut nonce = [0u8; 16];
        getrandom::fill(&mut nonce)
            .map_err(|error| anyhow::anyhow!("owned-session nonce unavailable: {error}"))?;
        let id: String = nonce.iter().map(|byte| format!("{byte:02x}")).collect();
        let directory = runtime.join(format!("cua-xpra-{id}"));
        match DirBuilder::new().mode(0o700).create(&directory) {
            Ok(()) => return Ok((id, directory)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    bail!("could not allocate an owned Xpra runtime directory")
}

fn executable(path: &Path, name: &str) -> Result<()> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("{name} executable is unavailable: {}", path.display()))?;
    anyhow::ensure!(
        path.is_absolute() && metadata.is_file() && metadata.permissions().mode() & 0o111 != 0,
        "{name} requires an absolute executable file path"
    );
    Ok(())
}

struct StartupChild(Option<Child>);

impl Drop for StartupChild {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            terminate(&mut child, Duration::from_secs(2));
        }
    }
}

fn parent_death_signal(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    let parent = std::process::id();
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(std::io::Error::other("owner exited during session launch"));
            }
            Ok(())
        });
    }
}

struct PrivateProfile {
    runtime: PathBuf,
    home: PathBuf,
    config: PathBuf,
    cache: PathBuf,
    data: PathBuf,
    state: PathBuf,
}

impl PrivateProfile {
    fn create(directory: &Path) -> Result<Self> {
        let home = directory.join("home");
        let profile = Self {
            runtime: directory.to_owned(),
            config: home.join(".config"),
            cache: home.join(".cache"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
            home,
        };
        let local = profile.home.join(".local");
        for path in [
            &profile.home,
            &profile.config,
            &profile.cache,
            &local,
            &profile.data,
            &profile.state,
        ] {
            if !path.exists() {
                DirBuilder::new().mode(0o700).create(path)?;
            }
        }
        Ok(profile)
    }
}

fn private_environment(
    command: &mut Command,
    profile: &PrivateProfile,
    auth: &Path,
    bus: &str,
    display: Option<&str>,
) {
    command
        .env("HOME", &profile.home)
        .env("XDG_CONFIG_HOME", &profile.config)
        .env("XDG_CACHE_HOME", &profile.cache)
        .env("XDG_DATA_HOME", &profile.data)
        .env("XDG_STATE_HOME", &profile.state)
        .env("XDG_RUNTIME_DIR", &profile.runtime)
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .env("XAUTHORITY", auth)
        .env("XDG_SESSION_TYPE", "x11")
        .env("XDG_CURRENT_DESKTOP", "Xpra")
        .env("GDK_BACKEND", "x11")
        .env("QT_QPA_PLATFORM", "xcb")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .env_remove("WAYLAND_DISPLAY")
        .env_remove("WAYLAND_SOCKET")
        .env_remove("CUA_DRIVER_ENVELOPE_HTTP_PORT")
        .env_remove("HYPRLAND_INSTANCE_SIGNATURE")
        .env_remove("CUA_INJECT_SOCKET")
        .env_remove("CUA_WAYLAND_NEST")
        .env_remove("AT_SPI_BUS_ADDRESS")
        .env_remove("DBUS_STARTER_ADDRESS")
        .env_remove("DBUS_STARTER_BUS_TYPE");
    match display {
        Some(display) => {
            command.env("DISPLAY", display);
        }
        None => {
            command.env_remove("DISPLAY");
        }
    }
}

/// Child creation occurs on a dedicated supervisor thread that stays alive
/// until stop. Linux PR_SET_PDEATHSIG follows the creating *thread*, so a
/// transient Tokio blocking worker must never spawn these children.
fn launch_in_supervisor(
    owner: &str,
    program: &str,
    args: &[String],
    driver_bin: &Path,
) -> Result<(OwnedSession, SessionInfo)> {
    anyhow::ensure!(!owner.is_empty(), "authenticated owner session required");
    anyhow::ensure!(
        !cua_driver_core::session::is_session_ending(owner),
        "owner session is ending; no owned app was launched"
    );
    executable(Path::new(program), "application")?;
    executable(driver_bin, "Driver")?;
    let (id, directory) = owned_directory()?;
    let startup = (|| -> Result<(OwnedSession, SessionInfo)> {
        let profile = PrivateProfile::create(&directory)?;
        // A private AT-SPI bus keeps this X server's accessibility clients
        // separate from the host user's desktop clients.
        let bus_path = directory.join("dbus");
        let bus_address = format!("unix:path={}", bus_path.display());
        let auth = directory.join("Xauthority");
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&auth)?;
        let mut bus_command = Command::new("dbus-daemon");
        // Activation starts with no physical desktop/profile environment.
        // Once Xpra allocates DISPLAY we update this private bus only.
        bus_command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
            .args(["--session", "--nofork", "--nosyslog"])
            .arg(format!("--address={bus_address}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        private_environment(&mut bus_command, &profile, &auth, &bus_address, None);
        parent_death_signal(&mut bus_command);
        let mut bus_owner = StartupChild(Some(
            bus_command
                .spawn()
                .context("could not start private D-Bus session")?,
        ));
        let bus_deadline = Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&bus_path).is_err() {
            if bus_owner.0.as_mut().unwrap().try_wait()?.is_some() || Instant::now() >= bus_deadline
            {
                bail!("private owned-session D-Bus did not become available");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(directory.join("xpra.log"))?;
        let (mut read_display, write_display) = UnixStream::pair()?;
        read_display.set_read_timeout(Some(Duration::from_secs(15)))?;
        let display_fd = 3;
        let source_fd = std::os::fd::AsRawFd::as_raw_fd(&write_display);
        let xpra = unsafe {
            use std::os::unix::process::CommandExt;
            let mut command = Command::new("xpra");
            command
                .args([
                    "start",
                    "--daemon=no",
                    "--attach=no",
                    "--use-display=no",
                    "--mdns=no",
                    "--html=off",
                    "--dbus=no",
                    "--systemd-run=no",
                    "--clipboard=no",
                    "--notifications=no",
                    "--audio=no",
                    "--file-transfer=no",
                    "--open-url=no",
                    "--printing=no",
                    "--commands=no",
                    "--start-new-commands=no",
                    "--shell=no",
                    "--resize-display=no",
                    "--exit-with-client=no",
                ])
                .arg(format!("--displayfd={display_fd}"))
                .arg(format!("--socket-dir={}", directory.display()))
                .arg(format!("--sessions-dir={}", directory.display()))
                // Xpra must populate the fresh private XAUTHORITY path also passed
                // to Driver and the app; disabling this selects a different file.
                .env("XPRA_SHARED_XAUTHORITY", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log));
            private_environment(&mut command, &profile, &auth, &bus_address, None);
            parent_death_signal(&mut command);
            command.pre_exec(move || {
                if source_fd == display_fd {
                    let flags = libc::fcntl(display_fd, libc::F_GETFD);
                    if flags == -1
                        || libc::fcntl(display_fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(source_fd, display_fd) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
            command.spawn().context("could not start Xpra")?
        };
        drop(write_display);
        let mut xpra_owner = StartupChild(Some(xpra));
        let mut bytes = [0u8; 16];
        let count = read_display
            .read(&mut bytes)
            .context("Xpra did not publish an owned display")?;
        let number = std::str::from_utf8(&bytes[..count])?.trim();
        anyhow::ensure!(
            !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()),
            "Xpra returned an invalid display number"
        );
        let display = format!(":{number}");
        let mut activation = Command::new("dbus-update-activation-environment");
        activation
            .args([
                "DISPLAY",
                "XAUTHORITY",
                "HOME",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "XDG_RUNTIME_DIR",
                "XDG_SESSION_TYPE",
                "XDG_CURRENT_DESKTOP",
                "GDK_BACKEND",
                "QT_QPA_PLATFORM",
                "MOZ_ENABLE_WAYLAND",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        private_environment(
            &mut activation,
            &profile,
            &auth,
            &bus_address,
            Some(&display),
        );
        anyhow::ensure!(
            activation
                .status()
                .context("could not update private D-Bus activation environment")?
                .success(),
            "private D-Bus activation environment was not updated"
        );
        let socket = directory.join("cua-driver.sock");
        let mut driver_command = Command::new(driver_bin);
        driver_command
            .arg("serve")
            .arg("--socket")
            .arg(&socket)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        private_environment(
            &mut driver_command,
            &profile,
            &auth,
            &bus_address,
            Some(&display),
        );
        parent_death_signal(&mut driver_command);
        let mut driver = driver_command
            .spawn()
            .context("could not start the owned Driver")?;
        let deadline = Instant::now() + Duration::from_secs(15);
        while UnixStream::connect(&socket).is_err() {
            if driver.try_wait()?.is_some()
                || xpra_owner.0.as_mut().unwrap().try_wait()?.is_some()
                || Instant::now() >= deadline
            {
                let _ = driver.kill();
                let _ = driver.wait();
                bail!(
                    "owned Driver/Xpra did not become available at {}",
                    socket.display()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut app_command = Command::new(program);
        app_command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        private_environment(
            &mut app_command,
            &profile,
            &auth,
            &bus_address,
            Some(&display),
        );
        parent_death_signal(&mut app_command);
        let app = app_command.spawn();
        let app = match app {
            Ok(app) => app,
            Err(error) => {
                let _ = driver.kill();
                let _ = driver.wait();
                return Err(error).context("could not launch application in owned Xpra display");
            }
        };
        let info = SessionInfo {
            id,
            socket,
            display,
            child_pid: app.id(),
        };
        Ok((
            OwnedSession {
                app,
                driver,
                xpra: xpra_owner.0.take().unwrap(),
                bus: bus_owner.0.take().unwrap(),
                directory: directory.clone(),
            },
            info,
        ))
    })();
    match startup {
        Ok((session, info)) => {
            if cua_driver_core::session::is_session_ending(owner) {
                drop(session);
                bail!("owner session ended during owned app launch");
            }
            Ok((session, info))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(directory);
            Err(error)
        }
    }
}

/// The supervisor owns all child handles and remains their Linux parent-death
/// signal source until owner cleanup. A Tokio blocking worker may retire after
/// returning from this function; this dedicated thread may not.
pub fn launch(
    owner: &str,
    program: &str,
    args: &[String],
    driver_bin: &Path,
) -> Result<SessionInfo> {
    anyhow::ensure!(!owner.is_empty(), "authenticated owner session required");
    anyhow::ensure!(
        !cua_driver_core::session::is_session_ending(owner),
        "owner session is ending; no owned app was launched"
    );
    executable(Path::new(program), "application")?;
    executable(driver_bin, "Driver")?;
    let thread_owner = owner.to_owned();
    let program = program.to_owned();
    let args = args.to_vec();
    let driver_bin = driver_bin.to_owned();
    let (ready_tx, ready_rx) = mpsc::sync_channel::<Result<SessionInfo>>(1);
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let supervisor = std::thread::Builder::new()
        .name("cua-owned-xpra".into())
        .spawn(
            move || match launch_in_supervisor(&thread_owner, &program, &args, &driver_bin) {
                Ok((session, info)) => {
                    if ready_tx.send(Ok(info)).is_ok() {
                        let _ = stop_rx.recv();
                    }
                    drop(session);
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                }
            },
        )
        .context("could not create owned-session supervisor")?;
    let controller = OwnedSessionController {
        owner: owner.to_owned(),
        stop: stop_tx,
        supervisor: Some(supervisor),
    };
    let info = ready_rx
        .recv()
        .context("owned-session supervisor ended before readiness")??;
    if cua_driver_core::session::is_session_ending(owner) {
        drop(controller);
        bail!("owner session ended during owned app launch");
    }
    sessions().lock().insert(info.id.clone(), controller);
    Ok(info)
}

/// Stop exactly one session belonging to this authenticated agent. A caller
/// cannot revoke an unrelated owner's session by learning its random ID.
pub fn stop(id: &str, owner: &str) -> Result<()> {
    let mut sessions = sessions().lock();
    anyhow::ensure!(
        sessions
            .get(id)
            .is_some_and(|session| session.owner == owner),
        "owned Xpra session is absent or belongs to a different agent"
    );
    let owned = sessions.remove(id).unwrap();
    drop(sessions);
    drop(owned);
    Ok(())
}

/// Agent EOF/end_session revokes only sessions launched by this owner.
pub fn stop_owner(owner: &str) {
    let owned: Vec<OwnedSessionController> = {
        let mut sessions = sessions().lock();
        let ids: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| session.owner == owner)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| sessions.remove(&id))
            .collect()
    };
    drop(owned);
}

/// Revoke only one registry runtime's authenticated sessions, even when
/// multiple embedded Driver runtimes share this process.
pub fn stop_runtime_prefix(prefix: &str) {
    if prefix.is_empty() {
        return;
    }
    let owned: Vec<OwnedSessionController> = {
        let mut sessions = sessions().lock();
        let ids: Vec<String> = sessions
            .iter()
            .filter(|(_, session)| session.owner.starts_with(prefix))
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| sessions.remove(&id))
            .collect()
    };
    drop(owned);
}
