//! Bounded, cancellable output collection for optional Git operations.
use std::{
    io::{ErrorKind, Read},
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub(crate) fn collect(
    command: &mut Command,
    limit: usize,
    cancelled: &AtomicBool,
) -> Option<Vec<u8>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let active = || !cancelled.load(Ordering::Relaxed) && Instant::now() < deadline;
    if !active() {
        return None;
    }
    let (mut reader, writer) = UnixStream::pair().ok()?;
    reader
        .set_read_timeout(Some(Duration::from_millis(20)))
        .ok()?;
    let spawned = command
        .stdout(OwnedFd::from(writer))
        .stderr(Stdio::null())
        .spawn();
    // Command retains configured handles for reuse, including after a failed spawn.
    command.stdout(Stdio::null());
    let mut child = spawned.ok()?;
    let result = (|| {
        let mut out = Vec::new();
        let mut buf = [0; 16 * 1024];
        while active() {
            match reader.read(&mut buf) {
                Ok(0) => {
                    while active() {
                        if let Some(status) = child.try_wait().ok()? {
                            return status.success().then_some(out);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    break;
                }
                Ok(n) if n <= limit - out.len() => out.extend_from_slice(&buf[..n]),
                Err(e)
                    if matches!(
                        e.kind(),
                        ErrorKind::Interrupted | ErrorKind::TimedOut | ErrorKind::WouldBlock
                    ) => {}
                _ => break,
            }
        }
        None
    })();
    if result.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancels_a_blocked_reader_and_bounds_output() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("ready");
        let stop = AtomicBool::new(false);
        let mut command = Command::new("sh");
        command
            .args(["-c", "touch \"$1\"; exec sleep 30", "sh"])
            .arg(&marker);
        std::thread::scope(|scope| {
            let task = scope.spawn(|| collect(&mut command, 1024, &stop));
            let deadline = Instant::now() + Duration::from_secs(2);
            while !marker.exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(marker.exists());
            stop.store(true, Ordering::Relaxed);
            assert!(task.join().unwrap().is_none());
        });
        let mut excess = Command::new("printf");
        excess.arg("1234");
        assert!(collect(&mut excess, 3, &AtomicBool::new(false)).is_none());
    }

    #[test]
    fn closed_stdout_does_not_disable_timeout() {
        let mut command = Command::new("sh");
        command.args(["-c", "exec 1>&-; exec sleep 30"]);
        let start = Instant::now();
        assert!(collect(&mut command, 1024, &AtomicBool::new(false)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn collects_output_with_configured_input() {
        use std::io::{Seek, Write};

        let mut command = Command::new("cat");
        let mut input = tempfile::tempfile().unwrap();
        input.write_all(b"input").unwrap();
        input.rewind().unwrap();
        command.stdin(Stdio::from(input));
        assert_eq!(
            collect(&mut command, 1024, &AtomicBool::new(false)),
            Some(b"input".to_vec())
        );
    }
}
