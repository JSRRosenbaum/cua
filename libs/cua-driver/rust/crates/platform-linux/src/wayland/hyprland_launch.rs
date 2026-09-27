//! Pre-map launch rules without changing global rules or restoring stolen focus.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn executable(command: &Command, cwd: &Path) -> io::Result<PathBuf> {
    let check = |path: PathBuf| -> io::Result<PathBuf> {
        if !path.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "executable not found",
            ));
        }
        let name = CString::new(path.as_os_str().as_bytes())?;
        if unsafe { libc::access(name.as_ptr(), libc::X_OK) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(path)
    };
    let program = command.get_program();
    if program.as_bytes().contains(&b'/') {
        return check(cwd.join(program));
    }
    let mut error = io::Error::new(io::ErrorKind::NotFound, "executable not found on PATH");
    for directory in
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_else(|| "/bin:/usr/bin".into()))
    {
        match check(cwd.join(directory).join(program)) {
            Ok(path) => return Ok(path),
            Err(missing) if missing.kind() == io::ErrorKind::NotFound => {}
            Err(denied) => error = denied,
        }
    }
    Err(error)
}

#[derive(Debug)]
pub(crate) struct LaunchOutcomeUnknown(anyhow::Error);

impl std::fmt::Display for LaunchOutcomeUnknown {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "launch outcome unknown; inspect windows before retrying: {}",
            self.0
        )
    }
}

impl std::error::Error for LaunchOutcomeUnknown {}

fn shell_word(output: &mut Vec<u8>, pieces: &[&[u8]]) {
    output.push(b'\'');
    for byte in pieces.iter().flat_map(|piece| piece.iter()).copied() {
        if byte == b'\'' {
            output.extend_from_slice(b"'\\''");
        } else {
            output.push(byte);
        }
    }
    output.push(b'\'');
}

fn prepare(
    command: &Command,
    program: &Path,
    cwd: &Path,
    root: &Path,
) -> io::Result<(File, PathBuf)> {
    let fifo = root.join("pid");
    let fifo_name = CString::new(fifo.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // Keep a writer open so poll waits for the actual shell acknowledgement,
    // rather than reporting EOF before the compositor's child opens the FIFO.
    let acknowledgement = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&fifo)?;
    let mut script =
        b"#!/bin/sh\nexec </dev/null\n[ -n \"${HL_EXEC_RULE_TOKEN-}\" ] || exit 126\ncd ".to_vec();
    shell_word(&mut script, &[cwd.as_os_str().as_bytes()]);
    script.extend_from_slice(b" || exit 126\nprintf '%s\\n' \"$$\" > ");
    shell_word(&mut script, &[fifo.as_os_str().as_bytes()]);
    script.extend_from_slice(b" || exit 126\nexec /usr/bin/env -i --");
    // Preserve the Driver's environment, not the compositor's potentially
    // different HOME/profile. Never log it in the compositor exec request.
    for (key, value) in std::env::vars_os() {
        if key == "HL_EXEC_RULE_TOKEN"
            || key == "HL_INITIAL_WORKSPACE_TOKEN"
            || command.get_envs().any(|(overridden, _)| overridden == key)
        {
            continue;
        }
        script.push(b' ');
        shell_word(&mut script, &[key.as_bytes(), b"=", value.as_bytes()]);
    }
    for (key, value) in command.get_envs() {
        if key == "HL_EXEC_RULE_TOKEN" || key == "HL_INITIAL_WORKSPACE_TOKEN" {
            continue;
        }
        if let Some(value) = value {
            script.push(b' ');
            shell_word(&mut script, &[key.as_bytes(), b"=", value.as_bytes()]);
        }
    }
    script.extend_from_slice(b" \"HL_EXEC_RULE_TOKEN=$HL_EXEC_RULE_TOKEN\" \"HL_INITIAL_WORKSPACE_TOKEN=${HL_INITIAL_WORKSPACE_TOKEN-}\" ");
    shell_word(&mut script, &[program.as_os_str().as_bytes()]);
    for arg in command.get_args() {
        script.push(b' ');
        shell_word(&mut script, &[arg.as_bytes()]);
    }
    script.push(b'\n');
    let path = root.join("exec.sh");
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .write_all(&script)?;
    Ok((acknowledgement, path))
}

fn read_pid(mut acknowledgement: File) -> io::Result<u32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = [0u8; 32];
    let mut length = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut,
                "compositor launch acknowledgement unavailable; launch may have occurred, do not replay"));
        }
        let mut poll = libc::pollfd {
            fd: acknowledgement.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut poll, 1, remaining.as_millis().max(1) as i32) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if result == 0 {
            continue;
        }
        let count = acknowledgement.read(&mut bytes[length..])?;
        length += count;
        if bytes[..length].ends_with(b"\n") {
            return std::str::from_utf8(&bytes[..length - 1])
                .ok()
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|pid| *pid > 1)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid launch PID acknowledgement",
                    )
                });
        }
        if count == 0 || length == bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete launch PID acknowledgement",
            ));
        }
    }
}

/// This is deliberately not a generic Command runner: the launch tool inherits
/// its environment and sets only explicit overrides/removals. The compositor
/// owns/reaps the child; shell exec preserves the acknowledged PID and rule token.
pub(crate) fn spawn(command: &Command) -> io::Result<u32> {
    if command.get_args().any(|arg| arg.as_bytes().contains(&0)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "launch arguments contain NUL",
        ));
    }
    let cwd = command
        .get_current_dir()
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_dir)
        .map_err(io::Error::other)?;
    // Resolve before dispatch so only a genuinely missing executable can take
    // the installed-desktop-entry resolution path. No action is retried.
    let program = executable(command, &cwd)?;
    let dispatched = || -> anyhow::Result<u32> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| io::Error::other("missing XDG_RUNTIME_DIR"))?;
        let root = tempfile::Builder::new()
            .prefix("cua-launch-")
            .tempdir_in(runtime)?;
        let (acknowledgement, script) = prepare(command, &program, &cwd, root.path())?;
        let mut invocation = b"exec /bin/sh ".to_vec();
        shell_word(&mut invocation, &[script.as_os_str().as_bytes()]);
        super::hyprland::launch_with_initial_focus_suppressed(&String::from_utf8(invocation)?)
            .map_err(LaunchOutcomeUnknown)?;
        // The acknowledgement is emitted only after the shell has opened the
        // private script. Unlinking it now is safe: that open FD retains it.
        read_pid(acknowledgement).map_err(|error| LaunchOutcomeUnknown(error.into()).into())
    };
    dispatched().map_err(|error| match error.downcast::<LaunchOutcomeUnknown>() {
        Ok(unknown) => io::Error::other(unknown),
        Err(error) => io::Error::other(error),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn private_script_prevents_shell_injection_and_preserves_exec_identity() {
        let root = tempfile::tempdir().unwrap();
        let literal = "literal ' $(touch injected) ;\n漢";
        let profile = root.path().join("private-home");
        let mut app = Command::new("/bin/sh");
        app.args(["-c",
            "[ \"$HL_EXEC_RULE_TOKEN\" = fresh-rule ] && [ -z \"${XDG_ACTIVATION_TOKEN+x}\" ] && [ \"$HOME\" = \"$2\" ] || exit 29; printf created > \"$1\"; printf private > \"$CUA_LITERAL_FILE\"",
            "cua-launch-test"])
            .arg(literal).arg(&profile)
            .env("HOME", &profile)
            .env("CUA_LITERAL_FILE", "environment ' $(touch injected-env)")
            .env("HL_EXEC_RULE_TOKEN", "stale-driver-rule")
            .env_remove("XDG_ACTIVATION_TOKEN");
        let (acknowledgement, script) =
            prepare(&app, Path::new("/bin/sh"), root.path(), root.path()).unwrap();
        let mut child = Command::new("/bin/sh")
            .arg(script)
            .env("HL_EXEC_RULE_TOKEN", "fresh-rule")
            .env("XDG_ACTIVATION_TOKEN", "must-not-reach-app")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = read_pid(acknowledgement);
        let status = child.wait().unwrap();
        assert!(status.success(), "{status}");
        assert_eq!(pid.unwrap(), child.id());
        assert_eq!(
            std::fs::read(root.path().join(literal)).unwrap(),
            b"created"
        );
        assert_eq!(
            std::fs::read(root.path().join("environment ' $(touch injected-env)")).unwrap(),
            b"private"
        );
        assert!(!root.path().join("injected").exists());
        assert!(!root.path().join("injected-env").exists());
    }

    #[test]
    fn missing_exec_rule_token_never_runs_the_application() {
        let root = tempfile::tempdir().unwrap();
        let mut app = Command::new("/bin/sh");
        app.args(["-c", "printf launched > application-effect"]);
        let (_acknowledgement, script) =
            prepare(&app, Path::new("/bin/sh"), root.path(), root.path()).unwrap();
        let status = Command::new("/bin/sh")
            .arg(script)
            .env_remove("HL_EXEC_RULE_TOKEN")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(!status.success());
        assert!(!root.path().join("application-effect").exists());
    }
}
