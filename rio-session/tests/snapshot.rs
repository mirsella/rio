#![cfg(any(
    target_os = "linux",
    target_os = "macos",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd"
))]

use rio_session::protocol::{EnvVar, MAX_PENDING_INPUT_BYTES};
use rio_session::{FrameUpdate, FullFrame, SessionClient, SessionSpec};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

fn worker_path() -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_rio-session-worker") {
        return path.into();
    }
    let current = std::env::current_exe().unwrap();
    for directory in current.ancestors() {
        let candidate = directory.join("rio-session-worker");
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!(
        "rio-session-worker binary was not found near {}",
        current.display()
    );
}

fn session_spec() -> SessionSpec {
    SessionSpec {
        shell: Some("/bin/sh".into()),
        args: vec!["-c".into(), "stty raw -echo; cat".into()],
        environment: vec![
            EnvVar::new("HOME", std::env::temp_dir().to_str().unwrap()),
            EnvVar::new("PATH", "/usr/bin:/bin"),
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
        if Instant::now() >= deadline {
            panic!("snapshot condition timed out");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn update_until(
    label: &str,
    client: &SessionClient,
    cached: &mut FullFrame,
    predicate: impl Fn(&FrameUpdate) -> bool,
) -> FrameUpdate {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let update = client.snapshot_since(cached.sequence).unwrap();
        let description = match &update {
            FrameUpdate::Full(frame) => format!("full {}x{}", frame.columns, frame.lines),
            FrameUpdate::Delta(delta) => format!(
                "delta base={} seq={} rows={} selection={}",
                delta.base_sequence,
                delta.sequence,
                delta.rows.len(),
                delta.selection.is_some()
            ),
        };
        let ready = predicate(&update);
        let returned = update.clone();
        update.apply_to(cached).unwrap();
        if ready {
            return returned;
        }
        if Instant::now() >= deadline {
            panic!("{label} snapshot update condition timed out; last={description}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        encoded.push(ALPHABET[(first >> 2) as usize] as char);
        encoded.push(
            ALPHABET
                [((first & 0x03) << 4 | chunk.get(1).copied().unwrap_or(0) >> 4) as usize]
                as char,
        );
        if let Some(second) = chunk.get(1) {
            encoded.push(
                ALPHABET[((second & 0x0f) << 2 | chunk.get(2).copied().unwrap_or(0) >> 6)
                    as usize] as char,
            );
        } else {
            encoded.push('=');
        }
        if let Some(third) = chunk.get(2) {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            encoded.push('=');
        }
    }
    encoded
}

fn atlas_asset(frame: &FullFrame) -> (u64, usize) {
    let image = frame
        .graphics
        .images
        .iter()
        .find(|image| image.kind == 1 && !image.pixels.is_empty())
        .expect("Atlas image upload is present");
    assert!(frame
        .graphics
        .atlas_placements
        .iter()
        .any(|placement| placement.key == image.key));
    (image.key, image.pixels.len())
}

fn clear_until_removed(client: &SessionClient, key: u64) {
    let mut erase = Vec::with_capacity(24 * 10);
    erase.extend_from_slice(b"\x1b[3J");
    for row in 1..=24 {
        erase.extend_from_slice(format!("\x1b[{row};1H\x1b[2K").as_bytes());
    }
    client.write(erase).unwrap();
    snapshot_until(client, |frame| {
        frame.graphics.atlas_placements.is_empty()
            && frame.graphics.removed_keys.contains(&key)
    });
}

#[test]
fn graceful_close_reaps_direct_child() {
    let mut spec = session_spec();
    spec.args = vec!["-c".into(), "trap '' HUP; while :; do sleep 1; done".into()];
    let client = SessionClient::spawn_with_worker_path(spec, worker_path()).unwrap();
    let child_pid = client.child_pid().unwrap();

    client.close().unwrap();
    let worker_status = client.wait_worker().unwrap().expect("worker was spawned");
    assert!(worker_status.success());

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let result = unsafe { libc::kill(child_pid as libc::pid_t, 0) };
        if result == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
        {
            break;
        }
        assert!(Instant::now() < deadline, "direct PTY child was not reaped");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn sixel_and_iterm2_assets_survive_snapshot_reattach_and_delete() {
    let client =
        SessionClient::spawn_with_worker_path(session_spec(), worker_path()).unwrap();
    let sixel = include_bytes!("../../rio-vt/tests/sixel/testimage_im6.sixel");
    client.write(sixel.to_vec()).unwrap();
    let frame = snapshot_until(&client, |frame| {
        frame
            .graphics
            .images
            .iter()
            .any(|image| image.kind == 1 && !image.pixels.is_empty())
            && !frame.graphics.atlas_placements.is_empty()
    });
    let (sixel_key, sixel_bytes) = atlas_asset(&frame);
    assert!(sixel_bytes > 0);

    client.write(b"\x1b[?1049h".to_vec()).unwrap();
    snapshot_until(&client, |frame| frame.alternate_screen);
    client.write(b"\x1b[?1049l".to_vec()).unwrap();
    snapshot_until(&client, |frame| {
        !frame.alternate_screen
            && frame
                .graphics
                .atlas_placements
                .iter()
                .any(|placement| placement.key == sixel_key)
    });

    let descriptor = client.descriptor().clone();
    drop(client);
    let attached = SessionClient::attach(descriptor).unwrap();
    let reattached = snapshot_until(&attached, |frame| {
        frame
            .graphics
            .images
            .iter()
            .any(|image| image.key == sixel_key && !image.pixels.is_empty())
            && frame
                .graphics
                .atlas_placements
                .iter()
                .any(|placement| placement.key == sixel_key)
    });
    let (_, reattached_bytes) = atlas_asset(&reattached);
    assert_eq!(reattached_bytes, sixel_bytes);
    clear_until_removed(&attached, sixel_key);

    let png = include_bytes!("../../rio-window/examples/data/icon.png");
    let mut iterm = b"\x1b]1337;File=inline=1:".to_vec();
    iterm.extend_from_slice(base64(png).as_bytes());
    iterm.push(0x07);
    attached.write(iterm).unwrap();
    let frame = snapshot_until(&attached, |frame| {
        frame
            .graphics
            .images
            .iter()
            .any(|image| image.kind == 1 && !image.pixels.is_empty())
            && !frame.graphics.atlas_placements.is_empty()
    });
    let (iterm_key, iterm_bytes) = atlas_asset(&frame);
    assert!(iterm_bytes > 0);

    let descriptor = attached.descriptor().clone();
    drop(attached);
    let attached = SessionClient::attach(descriptor).unwrap();
    let reattached = snapshot_until(&attached, |frame| {
        frame
            .graphics
            .images
            .iter()
            .any(|image| image.key == iterm_key && !image.pixels.is_empty())
            && frame
                .graphics
                .atlas_placements
                .iter()
                .any(|placement| placement.key == iterm_key)
    });
    let (_, reattached_bytes) = atlas_asset(&reattached);
    assert_eq!(reattached_bytes, iterm_bytes);
    clear_until_removed(&attached, iterm_key);
    attached.close().unwrap();
}

#[test]
fn atlas_removal_overflow_resynchronizes_without_restarting_shell() {
    let client =
        SessionClient::spawn_with_worker_path(session_spec(), worker_path()).unwrap();
    let pid = client.child_pid().unwrap();
    // One tiny sixel, then erase its placement. More unique deletions than the
    // librio removal mailbox can retain, without requesting any snapshots.
    let sixel = b"\x1bPq#0;2;100;0;0#0~\x1b\\";
    let mut output = Vec::new();
    for _ in 0..(rio_session::protocol::MAX_GRAPHICS_ITEMS + 32) {
        output.extend_from_slice(b"\x1b[H");
        output.extend_from_slice(sixel);
        output.extend_from_slice(b"\x1b[2J\x1b[3J");
    }
    output.extend_from_slice(b"\x1b]2;atlas-overflow-done\x07");
    client.write(output).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(client.poll_event().unwrap(), Some(rio_session::SessionEvent::Title { title, .. }) if title == "atlas-overflow-done")
        {
            break;
        }
        assert!(Instant::now() < deadline, "Atlas churn did not finish");
        thread::sleep(Duration::from_millis(5));
    }
    let empty = client.snapshot().unwrap();
    assert!(empty.graphics.images.is_empty());
    assert!(empty.graphics.atlas_placements.is_empty());
    client.write(sixel.to_vec()).unwrap();
    let frame =
        snapshot_until(&client, |frame| !frame.graphics.atlas_placements.is_empty());
    let (key, _) = atlas_asset(&frame);
    let descriptor = client.descriptor().clone();
    let owner = client;
    let client = SessionClient::prepare_attach(descriptor)
        .unwrap()
        .commit()
        .unwrap();
    assert_eq!(client.child_pid().unwrap(), pid);
    let recovered = client.snapshot().unwrap();
    assert!(recovered
        .graphics
        .images
        .iter()
        .any(|image| image.key == key));
    clear_until_removed(&client, key);
    let png = include_bytes!("../../rio-window/examples/data/icon.png");
    client
        .write(format!("\x1b]1337;File=inline=1:{}\x07", base64(png)).into_bytes())
        .unwrap();
    let frame =
        snapshot_until(&client, |frame| !frame.graphics.atlas_placements.is_empty());
    let (key, _) = atlas_asset(&frame);
    clear_until_removed(&client, key);
    assert_eq!(client.child_pid().unwrap(), pid);
    client.close().unwrap();
    assert!(owner.wait_worker().unwrap().unwrap().success());
}

#[test]
fn blocked_input_is_bounded_and_does_not_affect_another_worker() {
    let mut blocked_spec = session_spec();
    blocked_spec.args = vec!["-c".into(), "exec sleep 30".into()];
    let blocked =
        SessionClient::spawn_with_worker_path(blocked_spec, worker_path()).unwrap();

    let mut sibling_spec = session_spec();
    sibling_spec.args = vec!["-c".into(), "printf sibling".into()];
    let sibling =
        SessionClient::spawn_with_worker_path(sibling_spec, worker_path()).unwrap();

    let chunk = vec![b'x'; 1024 * 1024];
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut bounded_error = None;
    while Instant::now() < deadline {
        match blocked.write(chunk.clone()) {
            Ok(()) => {}
            Err(error) => {
                bounded_error = Some(error);
                break;
            }
        }
    }
    let error = bounded_error.expect("blocked PTY input must hit the byte budget");
    assert!(error.to_string().contains("pending PTY input"));
    assert!(MAX_PENDING_INPUT_BYTES >= chunk.len());

    let sibling_frame = snapshot_until(&sibling, |frame| {
        frame.rows.iter().any(|row| row.text.contains("sibling"))
    });
    assert!(sibling_frame
        .rows
        .iter()
        .any(|row| row.text.contains("sibling")));

    sibling.close().unwrap();
    assert!(sibling.wait_worker().unwrap().is_some());
    blocked.close().unwrap();
    assert!(blocked.wait_worker().unwrap().is_some());
}

#[test]
fn snapshot_since_covers_damage_metadata_and_full_resynchronization() {
    let client =
        SessionClient::spawn_with_worker_path(session_spec(), worker_path()).unwrap();
    let child_pid = client.child_pid().unwrap();
    let mut cached = client.snapshot().unwrap();

    client.write(b"\x1b[5;5H".to_vec()).unwrap();
    let cursor = update_until(
        "cursor",
        &client,
        &mut cached,
        |update| matches!(update, FrameUpdate::Delta(delta) if delta.rows.is_empty()),
    );
    assert!(matches!(cursor, FrameUpdate::Delta(delta) if delta.rows.is_empty()));

    client.write(b"\x1b[2;1Hpartial-row".to_vec()).unwrap();
    let partial = update_until(
        "partial",
        &client,
        &mut cached,
        |update| matches!(update, FrameUpdate::Delta(delta) if !delta.rows.is_empty()),
    );
    assert!(matches!(partial, FrameUpdate::Delta(delta) if !delta.rows.is_empty()));

    let prior_selection = cached.selection.clone();
    client
        .selection_begin(
            0,
            0,
            rio_session::SelectionKind::Simple,
            rio_session::SelectionSide::Left,
        )
        .unwrap();
    client
        .selection_update(0, 4, rio_session::SelectionSide::Right)
        .unwrap();
    let selection = update_until(
        "selection",
        &client,
        &mut cached,
        |update| matches!(update, FrameUpdate::Delta(delta) if delta.selection != prior_selection),
    );
    assert!(matches!(selection, FrameUpdate::Delta(delta) if delta.selection.is_some()));

    client.scroll(1).unwrap();
    let scroll = update_until("scroll", &client, &mut cached, |update| match update {
        FrameUpdate::Full(_) => true,
        FrameUpdate::Delta(delta) => !delta.rows.is_empty(),
    });
    assert!(matches!(
        scroll,
        FrameUpdate::Delta(_) | FrameUpdate::Full(_)
    ));

    let prior_colors = cached.colors.clone();
    client.write(b"\x1b]4;1;rgb:ff/00/00\x07".to_vec()).unwrap();
    let colors = update_until("colors", &client, &mut cached, |update| match update {
        FrameUpdate::Full(frame) => frame.colors != prior_colors,
        FrameUpdate::Delta(delta) => delta.colors != prior_colors,
    });
    assert!(matches!(
        colors,
        FrameUpdate::Delta(_) | FrameUpdate::Full(_)
    ));

    client.resize(40, 12, 400, 240).unwrap();
    let resized = update_until(
        "resize",
        &client,
        &mut cached,
        |update| matches!(update, FrameUpdate::Full(frame) if frame.columns == 40 && frame.lines == 12),
    );
    assert!(
        matches!(resized, FrameUpdate::Full(frame) if frame.columns == 40 && frame.lines == 12)
    );

    let sixel = b"\x1bPq#0;2;100;0;0#0~\x1b\\";
    client.write(sixel.to_vec()).unwrap();
    let graphics =
        update_until("graphics", &client, &mut cached, |update| match update {
            FrameUpdate::Full(frame) => !frame.graphics.images.is_empty(),
            FrameUpdate::Delta(_) => false,
        });
    assert!(
        matches!(graphics, FrameUpdate::Full(frame) if !frame.graphics.images.is_empty())
    );

    let stale_base = cached.sequence - 1;
    let recovered = client.snapshot_since(stale_base).unwrap();
    assert!(matches!(recovered, FrameUpdate::Full(_)));
    recovered.apply_to(&mut cached).unwrap();
    assert_eq!(client.child_pid().unwrap(), child_pid);

    let descriptor = client.descriptor().clone();
    drop(client);
    let attached = SessionClient::attach(descriptor).unwrap();
    assert_eq!(attached.child_pid().unwrap(), child_pid);
    let reattached = attached.snapshot().unwrap();
    assert_eq!(reattached.columns, 40);
    assert_eq!(reattached.lines, 12);
    attached.close().unwrap();
}
