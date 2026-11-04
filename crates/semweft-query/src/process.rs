use anyhow::{Context, Result, bail};
use std::{
    io::{Read, Seek},
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

/// Commands managed here must be spawned in their own process group on Unix.
pub(crate) fn terminate_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn output(command: &mut Command, deadline: Instant) -> Result<Output> {
    output_cancellable(command, deadline, || false)?.context("cancelled: external tool cancelled")
}

/// Spool both streams to files so inherited pipes cannot block cancellation.
pub fn output_cancellable(
    command: &mut Command,
    deadline: Instant,
    mut cancelled: impl FnMut() -> bool,
) -> Result<Option<Output>> {
    let deadline = crate::query::bound_deadline(deadline);
    crate::query::check_cancelled()?;
    if cancelled() {
        return Ok(None);
    }
    if Instant::now() >= deadline {
        bail!("timeout: external tool exceeded query deadline");
    }
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .stdout(Stdio::from(stdout.try_clone()?))
        .stderr(Stdio::from(stderr.try_clone()?))
        .spawn()
        .context("indexer_unavailable: cannot start external tool")?;
    let status = loop {
        let request_cancelled = crate::query::check_cancelled().is_err();
        let shared_cancelled = cancelled();
        if request_cancelled || shared_cancelled || Instant::now() >= deadline {
            terminate_tree(&mut child);
            crate::query::check_cancelled()?;
            if shared_cancelled {
                return Ok(None);
            }
            bail!("timeout: external tool exceeded query deadline");
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        thread::sleep(Duration::from_millis(5));
    };
    stdout.rewind()?;
    stderr.rewind()?;
    let mut out = vec![];
    let mut err = vec![];
    stdout.read_to_end(&mut out)?;
    stderr.read_to_end(&mut err)?;
    Ok(Some(Output {
        status,
        stdout: out,
        stderr: err,
    }))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn pre_cancelled_commands_are_not_spawned() {
        assert!(
            output_cancellable(
                &mut Command::new("/semweft-fixture-nonexistent-executable"),
                Instant::now() + Duration::from_secs(5),
                || true,
            )
            .unwrap()
            .is_none()
        );
    }

    #[test]
    fn expired_commands_are_not_spawned() {
        let error = output(
            &mut Command::new("/semweft-fixture-nonexistent-executable"),
            Instant::now() - Duration::from_millis(1),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("timeout:"), "{error:#}");
    }

    #[test]
    fn command_output_preserves_stdout_stderr_and_failure_status() {
        let output = output(
            Command::new("sh").args(["-c", "printf 'output'; printf 'failure' >&2; exit 7"]),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert_eq!(output.stdout, b"output");
        assert_eq!(output.stderr, b"failure");
    }

    #[test]
    fn large_output_on_both_streams_does_not_block() {
        let output = output(
            Command::new("sh").args([
                "-c",
                "head -c 262144 /dev/zero; head -c 262144 /dev/zero >&2",
            ]),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 262144);
        assert_eq!(output.stderr.len(), 262144);
    }

    #[test]
    fn cancellation_stops_running_process_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let started = temp.path().join("started");
        let escaped = temp.path().join("escaped");
        let output = output_cancellable(
            Command::new("sh")
                .current_dir(temp.path())
                .args(["-c", "(touch started; sleep 0.3; touch escaped) & sleep 30"]),
            Instant::now() + Duration::from_secs(5),
            || started.exists(),
        )
        .unwrap();
        assert!(output.is_none());
        std::thread::sleep(Duration::from_millis(400));
        assert!(!escaped.exists());
    }

    #[test]
    fn missing_executable_reports_startup_failure() {
        let error = output(
            &mut Command::new("/semweft-fixture-nonexistent-executable"),
            Instant::now() + Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("indexer_unavailable:"));
    }
    #[test]
    fn timeout_and_shared_cancellation_stop_process_trees() {
        let started = Instant::now();
        let error = output(
            Command::new("sh").args(["-c", "sleep 30 & wait"]),
            started + Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().starts_with("timeout:"));
        assert!(started.elapsed() < Duration::from_secs(2));
        let output = output_cancellable(
            Command::new("sh").args(["-c", "sleep 30 & wait"]),
            Instant::now() + Duration::from_secs(5),
            || true,
        )
        .unwrap();
        assert!(output.is_none());
    }
}
