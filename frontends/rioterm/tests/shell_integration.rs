#![cfg(unix)]

// Exercise the production launch adapter and its unit tests without a window.
#[path = "../src/shell_integration/mod.rs"]
mod shell_integration;

use rio_session::protocol::{EnvVar, SessionSpec};
use rio_session::{readiness::Readiness, FullFrame, SessionClient};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct Session(SessionClient);

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.0.close();
        let _ = self.0.wait_worker();
    }
}

fn wait_for_frame(client: &SessionClient, predicate: impl Fn(&FullFrame) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let wakeup = Readiness::new().unwrap();
    std::thread::scope(|scope| {
        // Dropping the sender cancels the timer on both success and panic.
        let (_cancel_timeout, receiver) = mpsc::channel::<()>();
        let wakeup = &wakeup;
        scope.spawn(move || {
            if matches!(
                receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())),
                Err(mpsc::RecvTimeoutError::Timeout)
            ) {
                wakeup.signal();
            }
        });
        loop {
            while client.poll_event().unwrap().is_some() {}
            let frame = client.snapshot().unwrap();
            if predicate(&frame) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "shell integration timed out: cwd={:?}, rows={:?}",
                frame.working_dir,
                frame
                    .rows
                    .iter()
                    .map(|row| row.text.as_str())
                    .collect::<Vec<_>>()
            );
            client.wait_for_activity(wakeup).unwrap();
        }
    });
}

fn worker_reports_cwd(shell: &str) {
    match std::process::Command::new(shell).arg("--version").output() {
        Ok(output) => assert!(output.status.success()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping worker integration: {shell} is not installed");
            return;
        }
        Err(error) => panic!("cannot run {shell}: {error}"),
    }
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let destination = home.join("directory with spaces");
    std::fs::create_dir(&destination).unwrap();
    // The initial OSC 7 can arrive before the shell's line editor is ready.
    // Wait for a user-configured prompt before writing interactive commands.
    std::fs::write(home.join(".zshrc"), "PS1='RIO_TEST_READY> '\n").unwrap();
    std::fs::create_dir(home.join("fish")).unwrap();
    std::fs::write(
        home.join("fish/config.fish"),
        "function fish_prompt; printf 'RIO_TEST_READY> '; end\n",
    )
    .unwrap();
    let mut spec = SessionSpec {
        // Cover configured shells and default resolution from the launch env.
        shell: (shell == "zsh").then(|| shell.into()),
        args: vec!["-i".into()],
        working_dir: Some(home.to_str().unwrap().into()),
        columns: 240,
        pixel_width: 2160,
        environment: vec![
            EnvVar::new("HOME", home.to_str().unwrap()),
            EnvVar::new("SHELL", shell),
            EnvVar::new("ZDOTDIR", home.to_str().unwrap()),
            EnvVar::new("XDG_CONFIG_HOME", home.to_str().unwrap()),
            EnvVar::new("XDG_DATA_DIRS", "/usr/share"),
            EnvVar::new("PATH", std::env::var("PATH").unwrap()),
            EnvVar::new("TERM", "xterm-256color"),
        ],
        ..SessionSpec::default()
    };
    shell_integration::apply(&mut spec);
    spec.validate().unwrap();
    let integration_directory = String::from_utf8(
        spec.environment
            .iter()
            .find(|env| env.key == b"RIO_SHELL_INTEGRATION")
            .expect("shell integration was prepared")
            .value
            .clone(),
    )
    .unwrap();
    let session = Session(
        SessionClient::spawn_with_worker_path(spec, env!("CARGO_BIN_EXE_rio")).unwrap(),
    );
    wait_for_frame(&session.0, |frame| {
        frame.working_dir.as_deref() == home.to_str()
            && frame
                .rows
                .iter()
                .any(|row| row.text.contains("RIO_TEST_READY>"))
    });
    session
        .0
        .write(b"cd 'directory with spaces'\n".to_vec())
        .unwrap();
    wait_for_frame(&session.0, |frame| {
        frame.working_dir.as_deref() == destination.to_str()
    });

    let variable = if shell == "zsh" {
        "ZDOTDIR"
    } else {
        "XDG_DATA_DIRS"
    };
    let command = format!("printf 'restored=%s\\n' \"${variable}\"\n");
    session.0.write(command.into_bytes()).unwrap();
    let restored_zdotdir = format!("restored={}", home.display());
    wait_for_frame(&session.0, |frame| {
        frame.rows.iter().any(|row| {
            if shell == "zsh" {
                row.text == restored_zdotdir
            } else {
                // Other installed fish vendor scripts (e.g. Flatpak) may prepend
                // paths after Rio restores XDG_DATA_DIRS. Our injected directory
                // must be gone and the original search path must remain.
                row.text.starts_with("restored=")
                    && row.text.ends_with("/usr/share")
                    && !row.text.contains(&integration_directory)
            }
        })
    });
}

#[test]
fn zsh_worker_reports_cwd_and_restores_environment() {
    worker_reports_cwd("zsh");
}

#[test]
fn fish_worker_reports_cwd_and_restores_environment() {
    worker_reports_cwd("fish");
}
