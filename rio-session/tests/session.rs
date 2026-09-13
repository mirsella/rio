#![cfg(unix)]

use rio_session::codec;
use rio_session::protocol::{
    CellContentFrame, ClientMessage, EnvVar, ErrorCode, SearchDirection, SelectionKind,
    SelectionSide, ServerMessage, SessionCommand, SessionEvent, SessionReply,
    SessionSpec, ViMotion, MAX_FRAME_SIZE, PROTOCOL_VERSION,
};
use rio_session::readiness;
use rio_session::{FullFrame, SessionClient, SessionDescriptor, SessionError};
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn worker_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_rio-session-worker") {
        return path.into();
    }
    let current = std::env::current_exe().unwrap();
    current
        .ancestors()
        .skip(1)
        .map(|directory| directory.join("rio-session-worker"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| {
            panic!("rio-session-worker not found near {}", current.display())
        })
}

fn control_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_rio-sessionctl") {
        return path.into();
    }
    let current = std::env::current_exe().unwrap();
    current
        .ancestors()
        .skip(1)
        .map(|directory| directory.join("rio-sessionctl"))
        .find(|path| path.is_file())
        .unwrap_or_else(|| panic!("rio-sessionctl not found near {}", current.display()))
}

fn temporary_directory() -> PathBuf {
    let directory = std::env::temp_dir().join(format!(
        "rio-session-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    directory
}

fn spec(command: &str, working_dir: Option<&Path>) -> SessionSpec {
    SessionSpec {
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), command.into()],
        working_dir: working_dir.map(|path| path.to_str().unwrap().to_owned()),
        environment: vec![
            EnvVar::new("HOME", std::env::temp_dir().to_str().unwrap()),
            EnvVar::new("PATH", "/usr/bin:/bin"),
            EnvVar::new("RIO_SESSION_TEST", "preserved"),
            EnvVar::new("TERM", "xterm-rio"),
        ],
        ..Default::default()
    }
}

fn snapshot_until(
    client: &SessionClient,
    predicate: impl Fn(&FullFrame) -> bool,
) -> FullFrame {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = client.snapshot().unwrap();
        if predicate(&frame) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "session snapshot condition timed out"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn frame_text(frame: &FullFrame) -> String {
    frame
        .rows
        .iter()
        .map(|row| row.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn wait_for_child_exit(client: &SessionClient) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = client.snapshot();
        while let Some(event) = client.poll_event().unwrap() {
            if let SessionEvent::ChildExited { status } = event {
                return status;
            }
        }
        assert!(Instant::now() < deadline, "child exit event timed out");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn lifecycle_client_process_helper() {
    let Some(directory) = std::env::var_os("RIO_LIFECYCLE_CLIENT_DIRECTORY") else {
        return;
    };
    let directory = PathBuf::from(directory);
    let client = SessionClient::spawn_with_worker_path(
        spec("stty -echo; printf 'lifecycle-ready\\n'; while IFS= read -r value; do printf 'reply:%s:pid:%s\\n' \"$value\" \"$$\"; done", None),
        worker_path(),
    ).unwrap();
    snapshot_until(&client, |frame| {
        frame_text(frame).contains("lifecycle-ready")
    });
    std::fs::write(
        directory.join("pids"),
        format!(
            "{} {}",
            client.worker_pid().unwrap(),
            client.child_pid().unwrap()
        ),
    )
    .unwrap();
    client
        .descriptor()
        .save(&directory.join("descriptor"))
        .unwrap();
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn lifecycle_sigkill_client_then_prepared_recovery_keeps_shell() {
    // Reap the orphan worker ourselves rather than leaving a zombie to PID 1.
    let mut previous = 0;
    assert_eq!(
        unsafe { libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &mut previous) },
        0
    );
    assert_eq!(unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) }, 0);
    struct Cleanup {
        owner: std::process::Child,
        worker: Option<i32>,
        previous: i32,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.owner.kill();
            let _ = self.owner.wait();
            if let Some(pid) = self.worker {
                unsafe {
                    libc::kill(pid, libc::SIGKILL);
                    libc::waitpid(pid, std::ptr::null_mut(), 0);
                }
            }
            unsafe {
                libc::prctl(libc::PR_SET_CHILD_SUBREAPER, self.previous);
            }
        }
    }
    let directory = temporary_directory();
    let owner = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "lifecycle_client_process_helper", "--nocapture"])
        .env("RIO_LIFECYCLE_CLIENT_DIRECTORY", &directory)
        .spawn()
        .unwrap();
    let mut cleanup = Cleanup {
        owner,
        worker: None,
        previous,
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let descriptor = loop {
        if let Ok(descriptor) = SessionDescriptor::load(&directory.join("descriptor")) {
            break descriptor;
        }
        assert!(
            cleanup.owner.try_wait().unwrap().is_none(),
            "client helper exited before ready"
        );
        assert!(Instant::now() < deadline, "client helper startup timed out");
        thread::sleep(Duration::from_millis(10));
    };
    let pids = std::fs::read_to_string(directory.join("pids")).unwrap();
    let pids: Vec<u32> = pids
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    cleanup.worker = Some(pids[0] as i32);
    cleanup.owner.kill().unwrap(); // SIGKILL only the GUI-equivalent client.
    cleanup.owner.wait().unwrap();

    // Allow the attachment thread's EOF handling to run before preparing.
    thread::sleep(Duration::from_millis(350));
    let delayed = SessionClient::prepare_attach(descriptor.clone()).unwrap();
    assert!(frame_text(delayed.initial_frame()).contains("lifecycle-ready"));
    // Cold rendering may exceed the short handshake deadline. It gets a
    // separate authenticated readiness budget, not an unbounded lease.
    thread::sleep(Duration::from_millis(3300));
    let delayed = delayed.commit().unwrap();
    assert_eq!(delayed.child_pid().unwrap(), pids[1]);
    drop(delayed);
    for cycle in 0..8 {
        let prepared = SessionClient::prepare_attach(descriptor.clone()).unwrap();
        assert!(prepared.initial_frame().sequence > 0);
        let client = prepared.commit().unwrap();
        // Fresh input is the first operation: it must consume deferred Ready.
        client
            .write(format!("cycle-{cycle}\n").into_bytes())
            .unwrap();
        let expected = format!("reply:cycle-{cycle}:pid:{}", pids[1]);
        snapshot_until(&client, |frame| frame_text(frame).contains(&expected));
        assert_eq!(client.child_pid().unwrap(), pids[1]);
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", pids[0])).unwrap();
        assert_ne!(stat.rsplit_once(") ").unwrap().1.chars().next(), Some('Z'));
        if cycle == 7 {
            client.close().unwrap();
        } else {
            // Commit a takeover, then lose its Ready without reading it.
            let replacement = SessionClient::prepare_attach(descriptor.clone())
                .unwrap()
                .commit()
                .unwrap();
            drop(replacement);
        }
    }
    wait_for_endpoint_removal(&descriptor);
    let mut status = 0;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = unsafe { libc::waitpid(pids[0] as i32, &mut status, libc::WNOHANG) };
        if result == pids[0] as i32 {
            break;
        }
        assert_eq!(result, 0);
        assert!(Instant::now() < deadline, "closed worker did not exit");
        thread::sleep(Duration::from_millis(10));
    }
    cleanup.worker = None;
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn worker_io_cwd_env_and_structured_frame() {
    let directory = temporary_directory();
    let command = "printf 'cwd=%s env=%s\\n' \"$PWD\" \"$RIO_SESSION_TEST\"";
    let client = SessionClient::spawn_with_worker_path(
        spec(command, Some(&directory)),
        worker_path(),
    )
    .unwrap();

    let frame = snapshot_until(&client, |frame| {
        // Long approved TMPDIR paths wrap across physical terminal rows.
        let text: String = frame.rows.iter().map(|row| row.text.as_str()).collect();
        text.contains("env=preserved") && text.contains("cwd=")
    });
    let text: String = frame.rows.iter().map(|row| row.text.as_str()).collect();
    assert!(text.contains("env=preserved"));
    assert!(text.contains(directory.to_str().unwrap()));
    assert_eq!(frame.columns, 80);
    assert_eq!(frame.lines, 24);
    assert!(!frame.rows.is_empty());

    client.resize(40, 12, 640, 240).unwrap();
    let resized =
        snapshot_until(&client, |frame| frame.columns == 40 && frame.lines == 12);
    assert_eq!((resized.columns, resized.lines), (40, 12));

    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(target_os = "linux")]
fn process_socket_fds(pid: u32) -> Vec<PathBuf> {
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with("socket:["))
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn inherited_listener_is_not_kept_by_shell_after_worker_exit() {
    struct Cleanup {
        worker: i32,
        child: i32,
        endpoint_dir: PathBuf,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            if self.worker > 0 {
                unsafe {
                    libc::kill(self.worker, libc::SIGKILL);
                }
            }
            if self.child > 0 {
                unsafe {
                    libc::kill(self.child, libc::SIGKILL);
                }
            }
            let _ = std::fs::remove_dir_all(&self.endpoint_dir);
        }
    }

    let client = SessionClient::spawn_with_worker_path(
        spec("trap 'exit 0' TERM; while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();
    let worker = client.worker_pid().unwrap() as i32;
    let child = client.child_pid().unwrap() as i32;
    let mut cleanup = Cleanup {
        worker,
        child,
        endpoint_dir: descriptor.endpoint.parent().unwrap().to_path_buf(),
    };

    assert!(
        process_socket_fds(child as u32).is_empty(),
        "shell inherited a socket before worker exit"
    );
    unsafe {
        assert_eq!(libc::kill(worker, libc::SIGKILL), 0);
    }
    client.wait_worker().unwrap().unwrap();
    cleanup.worker = -1;
    assert!(
        process_socket_fds(child as u32).is_empty(),
        "shell retained the worker accept socket after worker exit"
    );
    unsafe {
        libc::kill(child, libc::SIGTERM);
    }
    drop(client);
    std::fs::remove_dir_all(&cleanup.endpoint_dir).unwrap();
}

#[test]
fn worker_state_survives_takeover_and_reattach() {
    let command = "printf 'ready\\n'; read value; printf 'value=%s\\n' \"$value\"; while :; do sleep 1; done";
    let first = SessionClient::spawn_with_worker_path(spec(command, None), worker_path())
        .unwrap();
    snapshot_until(&first, |frame| frame_text(frame).contains("ready"));
    let shell_pid = first.child_pid().unwrap();

    let second = SessionClient::attach(first.descriptor().clone()).unwrap();
    assert_eq!(second.child_pid().unwrap(), shell_pid);
    assert!(first.write(b"stale\n".to_vec()).is_err());
    second.write(b"persisted\n".to_vec()).unwrap();
    snapshot_until(&second, |frame| {
        frame_text(frame).contains("value=persisted")
    });

    let descriptor = second.descriptor().clone();
    drop(second);
    let third = SessionClient::attach(descriptor).unwrap();
    let frame = snapshot_until(&third, |frame| {
        frame_text(frame).contains("value=persisted")
    });
    assert_eq!(third.child_pid().unwrap(), shell_pid);
    assert!(frame.sequence > 0);
    third.close().unwrap();
    wait_for_endpoint_removal(third.descriptor());
}

#[test]
fn prepared_attach_keeps_source_until_commit() {
    let first = SessionClient::spawn_with_worker_path(
        spec(
            "printf 'prepared-transfer\\n'; while :; do sleep 1; done",
            None,
        ),
        worker_path(),
    )
    .unwrap();
    snapshot_until(&first, |frame| {
        frame_text(frame).contains("prepared-transfer")
    });

    let descriptor = first.descriptor().clone();
    let prepared = SessionClient::prepare_attach(descriptor.clone()).unwrap();
    assert!(prepared.initial_frame().sequence > 0);
    assert!(prepared.had_active_owner());
    assert!(first.child_pid().is_ok());
    drop(prepared);
    assert!(first.child_pid().is_ok());

    let prepared = SessionClient::prepare_attach(descriptor).unwrap();
    let second = prepared.commit().unwrap();
    // commit() sends Commit; the first operation validates deferred Ready.
    assert!(second.child_pid().is_ok());
    assert!(first.child_pid().is_err());

    second.close().unwrap();
    wait_for_endpoint_removal(second.descriptor());
}

#[test]
fn second_process_can_attach_via_descriptor() {
    let directory = temporary_directory();
    let client = SessionClient::spawn_with_worker_path(
        spec("printf 'cross-process\\n'; while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let descriptor_path = directory.join("session.descriptor");
    client.descriptor().save(&descriptor_path).unwrap();

    let output = Command::new(control_path())
        .args(["snapshot", descriptor_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "rio-sessionctl failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("cross-process"));
    assert!(client.write(b"stale\n".to_vec()).is_err());

    let replacement = SessionClient::attach(client.descriptor().clone()).unwrap();
    replacement.close().unwrap();
    wait_for_endpoint_removal(replacement.descriptor());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn child_exit_delivers_final_output_and_status() {
    let client = SessionClient::spawn_with_worker_path(
        spec("printf 'final-output\\n'; exit 7", None),
        worker_path(),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = 'wait: loop {
        // Keep taking snapshots while the PTY drains to exercise terminal-lock
        // contention, but inspect a fresh frame after observing child exit.
        client.snapshot().unwrap();
        while let Some(event) = client.poll_event().unwrap() {
            if let SessionEvent::ChildExited { status } = event {
                let final_output_visible =
                    frame_text(&client.snapshot().unwrap()).contains("final-output");
                assert!(
                    final_output_visible,
                    "child exit was delivered before the final PTY output"
                );
                break 'wait status;
            }
        }
        assert!(Instant::now() < deadline, "child exit event timed out");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status, Some(7));
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn malformed_auth_and_framing_do_not_kill_worker() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();

    let mut bad_auth = UnixStream::connect(&descriptor.endpoint).unwrap();
    bad_auth
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    codec::write_frame(
        &mut bad_auth,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            capability: [1; 32],
            session_id: descriptor.session_id,
            spec: None,
        },
    )
    .unwrap();
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut bad_auth).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::BadAuth,
            ..
        }
    ));

    let mut bad_version = UnixStream::connect(&descriptor.endpoint).unwrap();
    codec::write_frame(
        &mut bad_version,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION + 1,
            capability: descriptor.capability,
            session_id: descriptor.session_id,
            spec: None,
        },
    )
    .unwrap();
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut bad_version).unwrap(),
        ServerMessage::Error {
            code: ErrorCode::BadProtocol,
            ..
        }
    ));

    let mut bad_frame = UnixStream::connect(&descriptor.endpoint).unwrap();
    bad_frame
        .write_all(&((MAX_FRAME_SIZE as u32) + 1).to_le_bytes())
        .unwrap();
    bad_frame.shutdown(std::net::Shutdown::Write).unwrap();
    drop(bad_frame);

    let frame = client.snapshot().unwrap();
    assert_eq!(frame.columns, 80);
    client.close().unwrap();
    wait_for_endpoint_removal(&descriptor);
}

#[test]
fn protocol_failure_detaches_active_attachment() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();
    let mut connection = UnixStream::connect(&descriptor.endpoint).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    codec::write_frame(
        &mut connection,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            capability: descriptor.capability,
            session_id: descriptor.session_id,
            spec: None,
        },
    )
    .unwrap();
    let generation = match codec::read_frame::<_, ServerMessage>(&mut connection).unwrap()
    {
        ServerMessage::Offer { generation } => generation,
        message => panic!("expected offer, got {message:?}"),
    };
    codec::write_frame(&mut connection, &ClientMessage::Claim { generation }).unwrap();
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut connection).unwrap(),
        ServerMessage::Claimed { .. }
    ));
    let initial_generation =
        match codec::read_frame::<_, ServerMessage>(&mut connection).unwrap() {
            ServerMessage::Initial { generation, .. } => generation,
            message => panic!("expected initial frame, got {message:?}"),
        };
    codec::write_frame(
        &mut connection,
        &ClientMessage::Commit {
            generation: initial_generation,
        },
    )
    .unwrap();
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut connection).unwrap(),
        ServerMessage::Ready { .. }
    ));
    codec::write_frame(
        &mut connection,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            capability: descriptor.capability,
            session_id: descriptor.session_id,
            spec: None,
        },
    )
    .unwrap();
    loop {
        match codec::read_frame::<_, ServerMessage>(&mut connection).unwrap() {
            ServerMessage::Error {
                code: ErrorCode::BadRequest,
                ..
            } => break,
            ServerMessage::Event { .. } => continue,
            message => panic!("expected bad-request error, got {message:?}"),
        }
    }
    drop(connection);
    drop(client);

    let replacement = SessionClient::attach(descriptor).unwrap();
    replacement.close().unwrap();
    wait_for_endpoint_removal(replacement.descriptor());
}

#[test]
fn spawned_worker_publishes_private_recovery_descriptor() {
    let client = SessionClient::spawn_with_worker_path(
        spec("printf recovery", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();
    let recovered = SessionDescriptor::discover_recovery().unwrap();
    assert!(recovered
        .iter()
        .any(|candidate| candidate.session_id == descriptor.session_id));

    client.close().unwrap();
    wait_for_endpoint_removal(&descriptor);
    let recovered = SessionDescriptor::discover_recovery().unwrap();
    assert!(!recovered
        .iter()
        .any(|candidate| candidate.session_id == descriptor.session_id));
}

#[test]
fn killing_one_worker_does_not_affect_another() {
    let first = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let second = SessionClient::spawn_with_worker_path(
        spec("printf 'second-alive\\n'; while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let first_descriptor = first.descriptor().clone();
    let first_pid = first.worker_pid().unwrap();
    assert!(first_pid > 0);

    unsafe {
        assert_eq!(libc::kill(first_pid as libc::pid_t, libc::SIGKILL), 0);
    }
    first.wait_worker().unwrap();
    if first_descriptor.endpoint.exists() {
        std::fs::remove_dir_all(first_descriptor.endpoint.parent().unwrap()).unwrap();
    }
    let frame =
        snapshot_until(&second, |frame| frame_text(frame).contains("second-alive"));
    assert!(frame_text(&frame).contains("second-alive"));
    second.close().unwrap();
    wait_for_endpoint_removal(second.descriptor());
}

fn wait_for_endpoint_removal(descriptor: &SessionDescriptor) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while descriptor.endpoint.exists() {
        assert!(
            Instant::now() < deadline,
            "session endpoint was not removed"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn codec_rejects_trailing_and_truncated_frames() {
    let encoded = codec::encode(&ClientMessage::Claim { generation: 1 }).unwrap();
    let mut trailing = encoded.clone();
    trailing.push(0);
    assert!(codec::decode::<ClientMessage>(&trailing).is_err());

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&4u32.to_le_bytes());
    bytes.extend_from_slice(&encoded[..2]);
    let error = codec::read_frame::<_, ClientMessage>(&mut &bytes[..]).unwrap_err();
    assert!(matches!(error, rio_session::SessionError::Io(_)));
}

#[test]
fn native_codec_rejects_huge_collection_lengths() {
    let payload =
        bincode::encode_to_vec(usize::MAX, bincode::config::standard()).unwrap();
    assert!(codec::decode::<Vec<String>>(&payload).is_err());
}

#[test]
fn codec_frame_deadline_does_not_reset_for_partial_reads() {
    let (mut reader, mut writer) = UnixStream::pair().unwrap();
    let writer_thread = thread::spawn(move || {
        writer.write_all(&4u32.to_le_bytes()).unwrap();
        writer.write_all(&[0]).unwrap();
        thread::sleep(Duration::from_millis(250));
        let _ = writer.write_all(&[0, 0, 0]);
    });
    let error = codec::read_frame_until::<ClientMessage>(
        &mut reader,
        Instant::now() + Duration::from_millis(100),
    )
    .unwrap_err();
    assert!(codec::is_timeout(&error));
    let _ = writer_thread.join();
}

#[test]
fn activity_wait_releases_for_worker_event_without_client_query() {
    let client = Arc::new(
        SessionClient::spawn_with_worker_path(
            spec("sleep 1; printf 'activity-event\\n'; sleep 10", None),
            worker_path(),
        )
        .unwrap(),
    );
    let initial_wakeup = readiness::Readiness::new().unwrap();
    client.wait_for_activity(&initial_wakeup).unwrap();
    assert!(matches!(
        client.poll_event().unwrap(),
        Some(SessionEvent::FrameReady)
    ));
    initial_wakeup.clear();
    let wakeup = Arc::new(readiness::Readiness::new().unwrap());
    let waiter_client = Arc::clone(&client);
    let waiter_wakeup = Arc::clone(&wakeup);
    let (started, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (finished, finished_receiver) = std::sync::mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        started.send(()).unwrap();
        finished
            .send(
                waiter_client
                    .wait_for_activity(&waiter_wakeup)
                    .map_err(|error| error.to_string()),
            )
            .unwrap();
    });
    started_receiver.recv().unwrap();
    assert_eq!(
        finished_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap(),
        Ok(())
    );
    waiter.join().unwrap();
    assert!(matches!(
        client.poll_event().unwrap(),
        Some(SessionEvent::FrameReady)
    ));
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn activity_wait_releases_for_caller_command_wakeup() {
    let client = Arc::new(
        SessionClient::spawn_with_worker_path(spec("sleep 10", None), worker_path())
            .unwrap(),
    );
    let wakeup = Arc::new(readiness::Readiness::new().unwrap());
    let waiter_client = Arc::clone(&client);
    let waiter_wakeup = Arc::clone(&wakeup);
    let (started, started_receiver) = std::sync::mpsc::sync_channel(1);
    let (finished, finished_receiver) = std::sync::mpsc::sync_channel(1);
    let waiter = thread::spawn(move || {
        started.send(()).unwrap();
        finished
            .send(
                waiter_client
                    .wait_for_activity(&waiter_wakeup)
                    .map_err(|error| error.to_string()),
            )
            .unwrap();
    });
    started_receiver.recv().unwrap();
    wakeup.signal();
    assert_eq!(
        finished_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap(),
        Ok(())
    );
    waiter.join().unwrap();
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn reply_contract_no_change_and_rejection_preserve_attachment() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let pid = client.child_pid().unwrap();
    for command in [
        SessionCommand::ScrollTop,
        SessionCommand::ScrollBottom,
        SessionCommand::ScrollToPrompt { forward: true },
        SessionCommand::ScrollToPrompt { forward: false },
    ] {
        assert_eq!(client.command(command).unwrap(), SessionReply::NoChange);
    }
    assert!(!client.focus(true).unwrap());
    assert!(!client.mouse_button(0, 0, 0, true, 0).unwrap());
    assert!(!client.mouse_motion(0, 0, 3, 0).unwrap());
    assert!(matches!(
        client.search_next(),
        Err(SessionError::Invalid(_))
    ));
    assert!(!client.is_poisoned());
    assert!(matches!(
        client.vi_motion(ViMotion::Left),
        Err(SessionError::Unsupported(_))
    ));
    assert!(!client.is_poisoned());
    assert!(matches!(
        client.scroll(i32::MIN),
        Err(SessionError::Invalid(_))
    ));
    assert!(!client.is_poisoned());
    assert!(client.snapshot().is_ok());
    assert_eq!(client.child_pid().unwrap(), pid);
    client.close().unwrap();
    assert!(client.is_poisoned());
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn reply_contract_mouse_reports_consumed_only_when_written() {
    let client = SessionClient::spawn_with_worker_path(
        spec("stty -echo; printf '\\033[?1003h\\033[?1006hready'; while :; do sleep 1; done", None),
        worker_path(),
    ).unwrap();
    snapshot_until(&client, |frame| frame_text(frame).contains("ready"));
    assert!(client.mouse_button(0, 0, 0, true, 0).unwrap());
    assert!(client.mouse_motion(0, 0, 3, 0).unwrap());
    assert!(!client.mouse_button(0, 0, 0, true, 1).unwrap());
    assert!(!client.mouse_motion(0, 0, 3, 1).unwrap());
    assert!(!client.is_poisoned());
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn invalid_numeric_input_does_not_reach_terminal_state() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    assert!(client.scroll(i32::MIN).is_err());
    assert!(client.mouse_wheel(i32::MIN, 0, 0, 0).is_err());
    assert!(client.mouse_button(0, 0, 1, true, 0x80).is_err());
    assert!(client.mouse_button(80, 0, 1, true, 0).is_err());
    assert!(client
        .selection_begin(i32::MIN, 0, SelectionKind::Simple, SelectionSide::Left)
        .is_err());
    assert!(client.snapshot().is_ok());
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn failed_takeover_leaves_old_attachment_usable() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();
    let mut replacement = UnixStream::connect(&descriptor.endpoint).unwrap();
    replacement
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    codec::write_frame(
        &mut replacement,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            capability: descriptor.capability,
            session_id: descriptor.session_id,
            spec: None,
        },
    )
    .unwrap();
    let generation =
        match codec::read_frame::<_, ServerMessage>(&mut replacement).unwrap() {
            ServerMessage::Offer { generation } => generation,
            message => panic!("expected offer, got {message:?}"),
        };
    codec::write_frame(&mut replacement, &ClientMessage::Claim { generation }).unwrap();
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut replacement).unwrap(),
        ServerMessage::Claimed { .. }
    ));
    assert!(matches!(
        codec::read_frame::<_, ServerMessage>(&mut replacement).unwrap(),
        ServerMessage::Initial { .. }
    ));
    drop(replacement);
    assert!(client.snapshot().is_ok());
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn stalled_handshakes_do_not_block_active_attachment() {
    let client = SessionClient::spawn_with_worker_path(
        spec("while :; do sleep 1; done", None),
        worker_path(),
    )
    .unwrap();
    let endpoint = client.descriptor().endpoint.clone();
    let stalled = (0..7)
        .map(|_| UnixStream::connect(&endpoint).unwrap())
        .collect::<Vec<_>>();
    assert!(client.snapshot().is_ok());
    drop(stalled);
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn detached_child_exit_retains_final_state_until_reattach() {
    let client = SessionClient::spawn_with_worker_path(
        spec("printf 'detached-final\\n'; exit 3", None),
        worker_path(),
    )
    .unwrap();
    let descriptor = client.descriptor().clone();
    drop(client);
    let deadline = Instant::now() + Duration::from_secs(5);
    let replacement = loop {
        match SessionClient::attach(descriptor.clone()) {
            Ok(client) => break client,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("reattach failed: {error}"),
        }
    };
    let frame = snapshot_until(&replacement, |frame| {
        frame_text(frame).contains("detached-final")
    });
    assert!(frame_text(&frame).contains("detached-final"));
    let status = wait_for_child_exit(&replacement);
    assert_eq!(status, Some(3));
    replacement.close().unwrap();
    wait_for_endpoint_removal(replacement.descriptor());
}

#[test]
fn descriptor_load_rejects_symlink() {
    use std::os::unix::fs::PermissionsExt;

    let directory = temporary_directory();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let descriptor = SessionDescriptor {
        endpoint: directory.join("session.sock"),
        capability: [1; 32],
        session_id: rio_session::SessionId([2; 16]),
    };
    let target = directory.join("descriptor");
    let link = directory.join("descriptor-link");
    descriptor.save(&target).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(SessionDescriptor::load(&link).is_err());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn authenticated_debug_output_redacts_capability() {
    let capability = [0xab; 32];
    let descriptor = SessionDescriptor {
        endpoint: PathBuf::from("/tmp/rio-session.sock"),
        capability,
        session_id: rio_session::SessionId([2; 16]),
    };
    let hello = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        capability,
        session_id: descriptor.session_id,
        spec: Some(SessionSpec::default()),
    };
    let debug = format!("{descriptor:?} {hello:?}");
    assert!(!debug.contains("171"));
    assert!(debug.contains("redacted"));
}

#[test]
fn semantic_frames_and_worker_navigation_are_authoritative() {
    let client = SessionClient::spawn_with_worker_path(
        spec(
            "printf '\\033[?12h\\033[?1004hfirst needle\\nsecond needle\\n'; while :; do sleep 1; done",
            None,
        ),
        worker_path(),
    )
    .unwrap();
    let frame =
        snapshot_until(&client, |frame| frame_text(frame).contains("second needle"));
    assert!(frame.cursor.blinking);
    assert!(frame.rows.iter().flat_map(|row| row.cells.iter()).any(|cell| {
        matches!(cell.content, CellContentFrame::Codepoint(codepoint) if codepoint == b'f' as u32)
    }));

    assert!(matches!(
        client.vi_motion(ViMotion::Right),
        Err(SessionError::Unsupported(_))
    ));
    assert!(client.set_vi_mode(true).unwrap());
    assert!(client.vi_motion(ViMotion::Right).unwrap());
    assert!(client.set_vi_mode(false).unwrap());
    assert!(client.focus(true).unwrap());
    assert!(client.focus(false).unwrap());

    client.selection_clear().unwrap();
    let navigation = client
        .search_begin(
            "needle",
            rio_session::SearchOrigin {
                line: 0,
                column: 0,
                display_offset: frame.display_offset,
            },
            SearchDirection::Forward,
            SelectionSide::Left,
            None,
        )
        .unwrap();
    assert!(navigation.matched.is_some());
    assert!(client.search_next().unwrap().matched.is_some());
    client.search_cancel().unwrap();
    assert!(client
        .selection_text()
        .unwrap()
        .is_some_and(|text| text.contains("needle")));

    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn worker_routes_terminal_requests_without_wire_closures() {
    let client = SessionClient::spawn_with_worker_path(
        spec(
            "stty -echo; i=0; while [ $i -lt 40 ]; do printf 'history line\\n'; i=$((i+1)); done; printf '\\033[?1004h\\033]52;c;?\\007\\033]10;?\\007\\033[14t\\033]777;notify;title;body\\007'; while :; do sleep 1; done",
            None,
        ),
        worker_path(),
    )
    .unwrap();
    snapshot_until(&client, |frame| frame.history_size > 0);
    client.scroll_top().unwrap();
    client
        .selection_begin(0, 0, SelectionKind::Simple, SelectionSide::Left)
        .unwrap();
    client.selection_update(0, 4, SelectionSide::Right).unwrap();
    let before = client.snapshot().unwrap();
    assert!(before.selection.is_some());
    assert!(client.focus(true).unwrap());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut clipboard = false;
    let mut color = false;
    let mut size = false;
    let mut notification = false;
    while !(clipboard && color && size && notification) {
        if let Some(event) = client.poll_event().unwrap() {
            match event {
                SessionEvent::ClipboardLoad {
                    request_id,
                    route_id,
                    kind,
                } => {
                    assert_eq!(kind, 0);
                    assert_ne!(route_id, 0);
                    assert!(matches!(
                        client.clipboard_response(
                            request_id,
                            route_id,
                            "x".repeat(rio_session::protocol::MAX_STRING_BYTES)
                        ),
                        Err(SessionError::Invalid(_))
                    ));
                    assert!(!client.is_poisoned());
                    client
                        .clipboard_response(request_id, route_id, "clipboard")
                        .unwrap();
                    clipboard = true;
                }
                SessionEvent::ColorRequest {
                    request_id,
                    route_id,
                    index,
                } => {
                    assert_eq!(index, 256);
                    assert!(matches!(
                        client.color_response(request_id, route_id, None),
                        Err(SessionError::Unsupported(_))
                    ));
                    assert!(!client.is_poisoned());
                    assert!(matches!(
                        client.color_response(request_id, route_id + 1, Some([1, 2, 3])),
                        Err(SessionError::Invalid(_))
                    ));
                    client
                        .color_response(request_id, route_id, Some([1, 2, 3]))
                        .unwrap();
                    color = true;
                }
                SessionEvent::TextAreaSizeRequest {
                    request_id,
                    route_id,
                } => {
                    client
                        .text_area_size_response(request_id, route_id, 24, 80, 640, 480)
                        .unwrap();
                    size = true;
                }
                SessionEvent::DesktopNotification { title, body } => {
                    assert_eq!(title, "title");
                    assert_eq!(body, "body");
                    notification = true;
                }
                _ => {}
            }
        }
        assert!(
            Instant::now() < deadline,
            "terminal request event timed out"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let after = client.snapshot().unwrap();
    assert_eq!(after.display_offset, before.display_offset);
    assert_eq!(after.selection, before.selection);
    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}

#[test]
fn worker_does_not_send_cpr_ahead_of_held_color_response() {
    let client = SessionClient::spawn_with_worker_path(
        spec(
            "stty -echo -icanon min 1 time 0; printf '\\033]11;?\\007\\033[6n'; first=$(dd bs=1 count=6 2>/dev/null | od -An -tx1 | tr -d '[:space:]'); printf 'first:%s\\n' \"$first\"; second=$(dd bs=1 count=24 2>/dev/null | od -An -tx1 | tr -d '[:space:]'); if [ \"$first\" = 1b5d31313b72 ] && [ \"$second\" = 67623a303130312f303230322f30333033071b5b313b3152 ]; then printf 'ordered\\n'; else printf 'reordered:%s:%s\\n' \"$first\" \"$second\"; fi; stty echo icanon; IFS= read -r value; printf 'input:%s\\n' \"$value\"; while :; do sleep 1; done",
            None,
        ),
        worker_path(),
    )
    .unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let (request_id, route_id) = loop {
        if let Some(SessionEvent::ColorRequest {
            request_id,
            route_id,
            index,
        }) = client.poll_event().unwrap()
        {
            assert_eq!(index, 257);
            break (request_id, route_id);
        }
        assert!(Instant::now() < deadline, "color request event timed out");
        thread::sleep(Duration::from_millis(5));
    };

    thread::sleep(Duration::from_millis(100));
    let held = client.snapshot().unwrap();
    assert!(
        !frame_text(&held).contains("first:1b5b313b3152"),
        "CPR escaped while the color response was held: {}",
        frame_text(&held)
    );

    client
        .color_response(request_id, route_id, Some([1, 2, 3]))
        .unwrap();
    snapshot_until(&client, |frame| frame_text(frame).contains("ordered"));

    client.write(b"ordinary-input\n".to_vec()).unwrap();
    snapshot_until(&client, |frame| {
        frame_text(frame).contains("input:ordinary-input")
    });

    client.close().unwrap();
    wait_for_endpoint_removal(client.descriptor());
}
