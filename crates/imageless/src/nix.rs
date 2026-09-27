//! Bounded Nix subprocess execution and output validation.

use crate::spec::validate_store_path;
use crate::MAX_CAPTURE_BYTES;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn parse_materialized_path(stdout: &str) -> io::Result<String> {
    let paths: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let path = match paths.as_slice() {
        [path] => (*path).to_string(),
        [] => return Err(io::Error::other("Nix produced no store path")),
        _ => return Err(io::Error::other("Nix produced multiple store paths")),
    };
    validate_store_path("materializer output", &path)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(path)
}

/// `nix-store --realise PATH --add-root ROOT` has emitted both forms across
/// supported Nix versions: some print the realized store path, while current
/// Nix prints the indirect root path. Accept exactly one matching line and rely
/// on `validate_registered_root` to authenticate the resulting symlink target.
/// Multiple or unrelated paths remain fail-closed.
pub(crate) fn validate_realise_output(
    stdout: &str,
    store_path: &str,
    root: &Path,
) -> io::Result<()> {
    let paths: Vec<_> = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let path = match paths.as_slice() {
        [path] => *path,
        [] => return Err(io::Error::other("Nix produced no realization path")),
        _ => return Err(io::Error::other("Nix produced multiple realization paths")),
    };
    if path == store_path || root.to_str() == Some(path) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Nix produced an unrelated realization path",
        ))
    }
}

pub(crate) fn run_command(command: &mut Command, timeout: Duration) -> io::Result<String> {
    // Under `runc create` our stdin is the container's stdio: a Nix client,
    // fetcher, or credential prompt must never read bytes meant for the
    // workload, nor hold its pipe open after we return.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() == 1 {
                libc::_exit(125);
            }
            Ok(())
        });
    }
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("materializer stdout was not captured"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("materializer stderr was not captured"))?;
    let stdout_reader = thread::spawn(move || capture_output(stdout, Retention::Head));
    let stderr_reader = thread::spawn(move || capture_output(stderr, Retention::Tail));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() >= timeout {
            kill_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();
            drop(stdout_reader);
            // What Nix last said is the only evidence of where a create that
            // ran out of deadline actually got to — for `--realise`, the path
            // substitution was working on. It was already in memory and
            // discarded here, which made a wedge on the first path and a copy
            // of the six-hundredth produce the same sentence.
            let excerpt = join_capture_within(stderr_reader, KILL_GRACE)
                .map(|stderr| diagnostic_excerpt(&stderr.bytes))
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                timed_out("Nix operation timed out", &excerpt),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    };
    // A helper can outlive the top-level Nix process while retaining an
    // inherited stdout/stderr fd. Keep pipe draining inside the same deadline;
    // otherwise joining the readers could wait forever after `try_wait` reports
    // the parent exited.
    while !stdout_reader.is_finished() || !stderr_reader.is_finished() {
        if started.elapsed() >= timeout {
            kill_process_group(child.id());
            drop(stdout_reader);
            let excerpt = join_capture_within(stderr_reader, KILL_GRACE)
                .map(|stderr| diagnostic_excerpt(&stderr.bytes))
                .unwrap_or_default();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                timed_out(
                    "Nix operation output did not close before the deadline",
                    &excerpt,
                ),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
    let stdout = join_capture(stdout_reader)?;
    let stderr = join_capture(stderr_reader)?;
    if stdout.truncated {
        return Err(io::Error::other("Nix stdout exceeded the capture limit"));
    }
    if !status.success() {
        // The tail is where Nix puts the actual error; without it a failed
        // evaluation is undiagnosable (the engine may discard the runtime's
        // stderr entirely — containerd keeps nothing from a failed create).
        let excerpt = diagnostic_excerpt(&stderr.bytes);
        return Err(io::Error::other(if excerpt.is_empty() {
            "Nix operation failed".to_string()
        } else {
            format!("Nix operation failed: {excerpt}")
        }));
    }
    Ok(String::from_utf8_lossy(&stdout.bytes).into_owned())
}

fn timed_out(reason: &str, excerpt: &str) -> String {
    if excerpt.is_empty() {
        reason.to_string()
    } else {
        format!("{reason}; Nix last reported: {excerpt}")
    }
}

/// How much of another program's stderr a diagnostic may carry.
///
/// Deliberately far below [`MAX_FRAME_BYTES`]. On the daemon deployment this
/// text is serialized into a `ResolveResponse::Error` and written by
/// `write_frame`, which hard-fails past that ceiling — so an over-long
/// diagnostic would cost the caller the response entirely and leave it holding
/// a closed connection instead of a reason. Encoding inflates, too: a byte that
/// is not valid UTF-8 becomes a three-byte U+FFFD, and JSON escapes a quote or
/// a backslash to two. Stripping the control bytes below bounds the worst case
/// at three times this, which leaves the rest of the frame ample room.
const MAX_EXCERPT_BYTES: usize = 2048;

/// The tail of a Nix stderr capture, made safe to carry in a diagnostic.
///
/// Control bytes are dropped rather than escaped. They are the expensive ones
/// to encode, and this text reaches terminals and log records where an escape
/// sequence would rewrite the line around it. Newlines stay: Nix's errors are
/// structured across them, and a diagnosis flattened to one line is harder to
/// read than the thing it describes.
fn diagnostic_excerpt(stderr: &[u8]) -> String {
    let tail = stderr.len().saturating_sub(MAX_EXCERPT_BYTES);
    let mut excerpt = String::new();
    for character in String::from_utf8_lossy(&stderr[tail..]).chars() {
        if excerpt.len() + character.len_utf8() > MAX_EXCERPT_BYTES {
            break;
        }
        if character == '\n' || !character.is_control() {
            excerpt.push(character);
        }
    }
    excerpt.trim().to_string()
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Which end of an over-long stream to keep.
///
/// stdout is parsed for a single store path, so losing its head would silently
/// change the answer — it keeps the head and reports truncation, which the
/// caller turns into a hard error. stderr is only ever read as a diagnosis, and
/// Nix puts the error at the end, so keeping its head meant a truncated log
/// yielded an "excerpt" from a megabyte before the failure.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Retention {
    Head,
    Tail,
}

fn capture_output(mut pipe: impl Read, retention: Retention) -> io::Result<CapturedOutput> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        match retention {
            Retention::Head => {
                let remaining = MAX_CAPTURE_BYTES.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..count.min(remaining)]);
                truncated |= count > remaining;
            }
            Retention::Tail => {
                bytes.extend_from_slice(&chunk[..count]);
                // Drop a whole window at a time rather than trimming on every
                // chunk: a long build log then costs one move per
                // MAX_CAPTURE_BYTES instead of one per 8 KiB read.
                if bytes.len() > MAX_CAPTURE_BYTES.saturating_mul(2) {
                    let excess = bytes.len() - MAX_CAPTURE_BYTES;
                    bytes.drain(..excess);
                    truncated = true;
                }
            }
        }
    }
    if retention == Retention::Tail && bytes.len() > MAX_CAPTURE_BYTES {
        let excess = bytes.len() - MAX_CAPTURE_BYTES;
        bytes.drain(..excess);
        truncated = true;
    }
    Ok(CapturedOutput { bytes, truncated })
}

/// How long a timed-out command's output readers get to finish after the
/// kill. The kill reaches the process group; a helper that left it (setsid,
/// a daemonized fetcher) can hold the pipes open indefinitely, and joining
/// its reader unconditionally made the deadline unbounded.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// Join a reader if it finishes within `grace`; otherwise leave it detached —
/// it ends when the last holder of the pipe does — and report nothing, so
/// the caller's deadline holds.
fn join_capture_within(
    handle: thread::JoinHandle<io::Result<CapturedOutput>>,
    grace: Duration,
) -> Option<CapturedOutput> {
    let started = Instant::now();
    while !handle.is_finished() {
        if started.elapsed() >= grace {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
    join_capture(handle).ok()
}

fn join_capture(
    handle: thread::JoinHandle<io::Result<CapturedOutput>>,
) -> io::Result<CapturedOutput> {
    handle
        .join()
        .map_err(|_| io::Error::other("materializer output reader panicked"))?
}

pub(crate) fn kill_process_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::STORE;

    #[test]
    fn a_timed_out_command_reports_what_nix_last_wrote() {
        // The whole point of the change: a create that runs out of deadline
        // while Nix is working says what Nix was working on. Without it, a
        // wedge on the first store path and a copy of the six-hundredth
        // produce the same sentence.
        let error = run_command(
            Command::new("sh").args([
                "-c",
                "printf 'copying path %s from cache\\n' /nix/store/aaa >&2; sleep 30",
            ]),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let message = error.to_string();
        assert!(message.contains("timed out"), "{message}");
        assert!(message.contains("/nix/store/aaa"), "{message}");
    }

    #[test]
    fn a_timeout_with_a_silent_command_still_names_the_deadline() {
        let error = run_command(
            Command::new("sh").args(["-c", "sleep 30"]),
            Duration::from_millis(300),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // No excerpt to add, so no dangling "Nix last reported:" clause.
        assert_eq!(error.to_string(), "Nix operation timed out");
    }

    #[test]
    fn stderr_keeps_its_tail_and_stdout_keeps_its_head() {
        // stderr is read only as a diagnosis and Nix puts the error last, so
        // retaining its head meant a truncated log yielded an "excerpt" from a
        // megabyte before the failure. stdout is parsed for a store path, so
        // losing its head would silently change the answer instead.
        let long = MAX_CAPTURE_BYTES + 4096;
        let head =
            capture_output(std::io::repeat(b'h').take(long as u64), Retention::Head).unwrap();
        assert_eq!(head.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(head.truncated);

        let mut stream = vec![b'o'; long];
        stream.extend_from_slice(b"THE ACTUAL ERROR");
        let tail = capture_output(stream.as_slice(), Retention::Tail).unwrap();
        assert_eq!(tail.bytes.len(), MAX_CAPTURE_BYTES);
        assert!(tail.truncated);
        assert!(tail.bytes.ends_with(b"THE ACTUAL ERROR"));
        assert!(diagnostic_excerpt(&tail.bytes).ends_with("THE ACTUAL ERROR"));
    }

    #[test]
    fn a_diagnostic_from_a_hostile_log_still_fits_the_protocol_frame() {
        // On the daemon deployment this text is serialized into a
        // ResolveResponse::Error and framed, and `write_frame` hard-fails past
        // MAX_FRAME_BYTES. A diagnostic that could not be written would cost
        // the caller the response entirely — a closed connection instead of a
        // reason, which is worse than the bare timeout this replaces.
        let hostile: Vec<u8> = (0..MAX_CAPTURE_BYTES)
            .map(|index| match index % 3 {
                0 => 0x1b, // escape: would rewrite a terminal line
                1 => 0xff, // invalid UTF-8: becomes a three-byte U+FFFD
                _ => b'"', // JSON-escaped, two bytes out for one in
            })
            .collect();
        let excerpt = diagnostic_excerpt(&hostile);
        assert!(
            !excerpt.contains('\u{1b}'),
            "control bytes must not survive"
        );
        assert!(excerpt.len() <= MAX_EXCERPT_BYTES);

        let response = crate::materialize::ResolveResponse::Error {
            version: crate::PROTOCOL_VERSION,
            error: crate::materialize::ResolutionError::timeout_with(
                "during release materialization",
                &excerpt,
            ),
        };
        let encoded = serde_json::to_vec(&response).unwrap();
        assert!(
            encoded.len() < crate::MAX_FRAME_BYTES,
            "encoded diagnostic was {} bytes, frame ceiling is {}",
            encoded.len(),
            crate::MAX_FRAME_BYTES
        );
    }

    #[test]
    fn newlines_survive_the_excerpt_but_other_control_bytes_do_not() {
        // Nix structures its errors across lines; flattening them makes the
        // diagnosis harder to read than the thing it describes.
        let excerpt = diagnostic_excerpt(b"error: builder failed\n\x07  while evaluating\n");
        assert_eq!(excerpt, "error: builder failed\n  while evaluating");
    }

    #[test]
    fn realise_output_accepts_store_or_indirect_root_but_rejects_ambiguity() {
        let root = Path::new("/tmp/imageless-test/.imageless-rootfs-gcroot");
        validate_realise_output(&format!("{STORE}\n"), STORE, root).unwrap();
        validate_realise_output(
            "/tmp/imageless-test/.imageless-rootfs-gcroot\n",
            STORE,
            root,
        )
        .unwrap();
        assert!(validate_realise_output("/tmp/unrelated\n", STORE, root).is_err());
        assert!(validate_realise_output(
            &format!("{STORE}\n/tmp/imageless-test/.imageless-rootfs-gcroot\n"),
            STORE,
            root,
        )
        .is_err());
    }

    #[test]
    fn a_helper_that_escapes_the_process_group_cannot_stretch_the_deadline() {
        // setsid puts the sleeper in its own session, out of reach of the
        // group kill, still holding the inherited stdout/stderr.
        let started = Instant::now();
        let error = run_command(
            Command::new("sh").args(["-c", "setsid sleep 30 & sleep 30"]),
            Duration::from_millis(200),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "deadline stretched to {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn command_deadline_includes_inherited_output_pipes() {
        let started = Instant::now();
        let error = run_command(
            Command::new("sh").args(["-c", "sleep 30 & exit 0"]),
            Duration::from_millis(100),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
