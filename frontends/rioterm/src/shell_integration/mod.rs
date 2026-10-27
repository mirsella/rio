//! Automatic shell integration.
//!
//! Rio embeds small per-shell scripts that report the working
//! directory to the terminal (OSC 7, `kitty-shell-cwd://` flavor) on
//! every prompt and directory change. At spawn time the scripts are
//! written under the user's cache directory (once per rio version)
//! and the child is pointed at them. zsh and fish load through the
//! environment alone: `ZDOTDIR` for zsh (the user's value is preserved
//! in `RIO_ZSH_ZDOTDIR` and restored before any of their configuration
//! runs) and an `XDG_DATA_DIRS` prepend for fish, whose
//! `vendor_conf.d` loads from there (the script restores the original
//! value so the prepend never leaks to child processes). PowerShell
//! has no environment hook, so a bare spawn (no configured args) is
//! rewritten to `-NoExit -EncodedCommand <script>` carrying the script
//! inline, which runs after the user's profile and is exempt from
//! execution policy (the default Windows client policy blocks script
//! FILES, so a dot-sourced file would error in every pane). Shells
//! with neither hook (bash needs `--posix` argv surgery, cmd.exe only
//! has a machine-wide registry key) are left untouched, and every
//! integrated pane exports `RIO_SHELL_INTEGRATION` pointing at the
//! script directory so any shell can source the integration manually.
//!
//! Old versions' script directories are deliberately left in place: a
//! concurrently running older rio still spawns shells whose `ZDOTDIR`
//! points into its own directory, and deleting it would make zsh skip
//! the user's entire configuration.

use rio_session::protocol::{EnvVar, SessionSpec};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const ZSH_ZSHENV: &str = include_str!("zshenv.zsh");
const ZSH_INTEGRATION: &str = include_str!("rio-integration.zsh");
const FISH_INTEGRATION: &str = include_str!("rio.fish");
const POWERSHELL_INTEGRATION: &str = include_str!("rio.ps1");

/// Prepare a fresh worker's launch without changing process-global environment
/// or the wire protocol. Never apply this to a recovered/transferred session.
pub fn apply(spec: &mut SessionSpec) {
    if let Some(dir) = integration_dir() {
        apply_in(spec, dir);
    }
}

fn apply_in(spec: &mut SessionSpec, dir: &Path) {
    let program = resolved_shell(spec);
    let shell_name = shell_display_name(&program);
    let mut env = env_pairs(
        &shell_name,
        dir,
        environment_value(spec, b"ZDOTDIR"),
        environment_value(spec, b"XDG_DATA_DIRS"),
    );
    env.push((
        "RIO_SHELL_INTEGRATION".to_string(),
        dir.to_string_lossy().to_string(),
    ));
    for (key, value) in env {
        // SessionSpec rejects duplicate keys, including inherited hook variables.
        spec.environment.retain(|env| env.key != key.as_bytes());
        spec.environment.push(EnvVar::new(key, value));
    }

    if matches!(shell_name.as_str(), "powershell" | "pwsh") {
        if spec.args.is_empty() {
            // Keep an unnamed shell unnamed on Unix so macOS login(1) still
            // wraps it. Windows needs an explicit program to preserve args.
            #[cfg(target_os = "windows")]
            {
                spec.shell = Some(program);
            }
            spec.args = powershell_args();
        } else {
            tracing::info!(
                "shell integration skipped: PowerShell spawn has configured args"
            );
        }
    }
}

fn environment_value<'a>(spec: &'a SessionSpec, key: &[u8]) -> Option<&'a str> {
    spec.environment
        .iter()
        .find(|env| env.key == key)
        .map(|env| {
            std::str::from_utf8(&env.value).expect("validated session environment")
        })
}

/// The program the PTY will spawn: the configured one, else the same
/// resolution the spawn itself performs (`$SHELL`, then the passwd
/// entry, on unix; `powershell` on Windows).
fn resolved_shell(spec: &SessionSpec) -> String {
    match spec.shell.as_deref() {
        Some(program) if !program.is_empty() => program.to_string(),
        #[cfg(not(target_os = "windows"))]
        _ => environment_value(spec, b"SHELL")
            .map(str::to_owned)
            .unwrap_or_else(teletypewriter::default_shell_program),
        #[cfg(target_os = "windows")]
        _ => String::from("powershell"),
    }
}

/// The shell's identity from its program path: basename with both
/// separator flavors (a Windows path can carry `\`), lowercased,
/// `.exe` dropped, so `C:\W\pwsh.exe` and `/usr/bin/pwsh` compare
/// equal.
fn shell_display_name(program: &str) -> String {
    let mut name = program
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(program)
        .to_ascii_lowercase();
    if name.ends_with(".exe") {
        name.truncate(name.len() - 4);
    }
    name
}

/// The environment that makes `shell_name` load the scripts under
/// `dir`. Pure so the mapping is testable without touching the real
/// process environment or cache directory.
fn env_pairs(
    shell_name: &str,
    dir: &Path,
    current_zdotdir: Option<&str>,
    current_xdg_data_dirs: Option<&str>,
) -> Vec<(String, String)> {
    match shell_name {
        "zsh" => {
            let mut envs = Vec::new();
            if let Some(old) = current_zdotdir {
                envs.push(("RIO_ZSH_ZDOTDIR".to_string(), old.to_owned()));
            }
            envs.push((
                "ZDOTDIR".to_string(),
                dir.join("zsh").to_string_lossy().to_string(),
            ));
            envs
        }
        "fish" => {
            let data_dir = dir.join("data").to_string_lossy().to_string();
            // The original value rides along (empty means "was unset")
            // so the script can RESTORE it after loading: without the
            // restore, every process the pane ever starts inherits the
            // rio-version-specific prepend.
            let restore = current_xdg_data_dirs.unwrap_or_default();
            // The XDG default applies to both unset and empty values.
            let search_path = if restore.is_empty() {
                "/usr/local/share:/usr/share"
            } else {
                restore
            };
            vec![
                ("RIO_FISH_XDG_DATA_DIRS".to_string(), restore.to_owned()),
                (
                    "XDG_DATA_DIRS".to_string(),
                    format!("{data_dir}:{search_path}"),
                ),
            ]
        }
        _ => Vec::new(),
    }
}

/// `-NoExit -EncodedCommand <base64 of the UTF-16LE script>`: the
/// script travels inline, so no file is dot-sourced (script FILES are
/// what the default Windows execution policy blocks) and no quoting
/// can break. Encoded per call: this runs once per PowerShell pane,
/// where microseconds of base64 vanish next to the process spawn.
fn powershell_args() -> Vec<String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine as _;
    let utf16le: Vec<u8> = POWERSHELL_INTEGRATION
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    vec![
        "-NoExit".to_string(),
        "-EncodedCommand".to_string(),
        B64.encode(utf16le),
    ]
}

/// The on-disk script directory, materialized once per process. Keyed
/// by rio's version so upgrades write fresh scripts and downgrades
/// never read newer ones.
fn integration_dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let Some(cache) = dirs::cache_dir() else {
            tracing::warn!("shell integration skipped: no user cache directory");
            return None;
        };
        let base = cache
            .join("rio")
            .join(format!("shell-integration-{}", env!("CARGO_PKG_VERSION")));
        if let Err(err) = materialize(&base) {
            tracing::warn!("shell integration scripts not written: {err}");
            return None;
        }
        Some(base)
    })
    .as_deref()
}

/// Write the embedded scripts under `base`. Rewrites only files whose
/// content differs, so concurrent rio processes racing here write the
/// same bytes and the result is valid either way.
fn materialize(base: &Path) -> std::io::Result<()> {
    let zsh = base.join("zsh");
    std::fs::create_dir_all(&zsh)?;
    write_if_changed(&zsh.join(".zshenv"), ZSH_ZSHENV)?;
    write_if_changed(&zsh.join("rio-integration.zsh"), ZSH_INTEGRATION)?;

    let fish = base.join("data").join("fish").join("vendor_conf.d");
    std::fs::create_dir_all(&fish)?;
    write_if_changed(&fish.join("rio.fish"), FISH_INTEGRATION)?;

    let powershell = base.join("powershell");
    std::fs::create_dir_all(&powershell)?;
    write_if_changed(&powershell.join("rio.ps1"), POWERSHELL_INTEGRATION)?;
    Ok(())
}

fn write_if_changed(path: &Path, content: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path)
        .map(|current| current == content)
        .unwrap_or(false)
    {
        return Ok(());
    }
    std::fs::write(path, content)
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn launch_spec_replaces_hook_variables_and_preserves_user_environment() {
        let dir = Path::new("/cache/rio/shell-integration-test");
        for (shell, hook, original, restore, suffix) in [
            ("zsh", "ZDOTDIR", "/user/zsh", "RIO_ZSH_ZDOTDIR", "zsh"),
            (
                "fish",
                "XDG_DATA_DIRS",
                "/user/data",
                "RIO_FISH_XDG_DATA_DIRS",
                "data",
            ),
        ] {
            let mut spec = SessionSpec {
                shell: Some(shell.into()),
                args: vec!["-l".into()],
                environment: vec![
                    EnvVar::new("HOME", "/user"),
                    EnvVar::new(hook, original),
                    EnvVar::new(restore, "stale"),
                    EnvVar::new("RIO_SHELL_INTEGRATION", "stale"),
                ],
                ..SessionSpec::default()
            };
            apply_in(&mut spec, dir);
            spec.validate().unwrap();
            assert_eq!(spec.shell.as_deref(), Some(shell));
            assert_eq!(spec.args, ["-l"]);
            assert_eq!(environment_value(&spec, b"HOME"), Some("/user"));
            assert_eq!(environment_value(&spec, restore.as_bytes()), Some(original));
            assert!(environment_value(&spec, hook.as_bytes())
                .unwrap()
                .starts_with(dir.join(suffix).to_str().unwrap()));
            assert_eq!(
                environment_value(&spec, b"RIO_SHELL_INTEGRATION"),
                dir.to_str()
            );
        }
    }

    #[test]
    fn launch_spec_rewrites_only_bare_powershell_commands() {
        let dir = Path::new("/cache/rio/shell-integration-test");
        for shell in ["pwsh", "powershell"] {
            let mut bare = SessionSpec {
                shell: Some(shell.into()),
                ..SessionSpec::default()
            };
            apply_in(&mut bare, dir);
            bare.validate().unwrap();
            assert_eq!(bare.args, powershell_args());
            assert_eq!(bare.shell.as_deref(), Some(shell));
        }

        for shell in ["pwsh", "powershell", "bash", "nu"] {
            let mut spec = SessionSpec {
                shell: Some(shell.into()),
                args: vec!["-c".into(), "echo custom".into()],
                ..SessionSpec::default()
            };
            let args = spec.args.clone();
            apply_in(&mut spec, dir);
            spec.validate().unwrap();
            assert_eq!(spec.args, args);
            assert_eq!(spec.shell.as_deref(), Some(shell));
        }
    }

    #[cfg(unix)]
    #[test]
    fn launch_spec_uses_captured_default_shell_without_naming_the_command() {
        let mut spec = SessionSpec {
            environment: vec![EnvVar::new("SHELL", "/usr/bin/fish")],
            ..SessionSpec::default()
        };
        apply_in(&mut spec, Path::new("/cache/rio/integration"));
        spec.validate().unwrap();
        assert_eq!(spec.shell, None);
        assert!(spec.args.is_empty());
        assert_eq!(
            environment_value(&spec, b"XDG_DATA_DIRS"),
            Some("/cache/rio/integration/data:/usr/local/share:/usr/share")
        );
    }

    #[cfg(unix)]
    #[test]
    fn launch_spec_keeps_default_powershell_unnamed() {
        let mut spec = SessionSpec {
            environment: vec![EnvVar::new("SHELL", "/usr/bin/pwsh")],
            ..SessionSpec::default()
        };
        apply_in(&mut spec, Path::new("/cache/rio/integration"));
        spec.validate().unwrap();
        assert_eq!(spec.shell, None);
        assert_eq!(spec.args, powershell_args());
    }

    #[test]
    fn env_pairs_per_shell() {
        let dir = Path::new("/cache/rio/shell-integration-1.0.0");
        // The join separator differs per platform; compare against the
        // same join the implementation performs.
        let zsh_dir = dir.join("zsh").to_string_lossy().to_string();
        let data_dir = dir.join("data").to_string_lossy().to_string();

        let zsh = env_pairs("zsh", dir, None, None);
        assert_eq!(zsh, vec![("ZDOTDIR".to_string(), zsh_dir.clone())]);

        // A user ZDOTDIR is preserved for the .zshenv chain to restore.
        let zsh = env_pairs("zsh", dir, Some("/home/u/.config/zsh"), None);
        assert_eq!(
            zsh[0],
            (
                "RIO_ZSH_ZDOTDIR".to_string(),
                "/home/u/.config/zsh".to_string()
            )
        );
        assert_eq!(zsh[1], ("ZDOTDIR".to_string(), zsh_dir));

        // A user XDG_DATA_DIRS is preserved for rio.fish to restore.
        let fish = env_pairs("fish", dir, None, Some("/usr/share"));
        assert_eq!(
            fish,
            vec![
                (
                    "RIO_FISH_XDG_DATA_DIRS".to_string(),
                    "/usr/share".to_string()
                ),
                (
                    "XDG_DATA_DIRS".to_string(),
                    format!("{data_dir}:/usr/share")
                ),
            ]
        );

        // Unset XDG_DATA_DIRS keeps the spec default visible, and the
        // empty restore value tells the script to unset it again.
        let fish = env_pairs("fish", dir, None, None);
        assert_eq!(
            fish[0],
            ("RIO_FISH_XDG_DATA_DIRS".to_string(), String::new())
        );
        assert!(fish[1].1.ends_with(":/usr/local/share:/usr/share"));
        assert_eq!(env_pairs("fish", dir, None, Some("")), fish);

        // Shells without an environment-only hook are untouched.
        assert!(env_pairs("bash", dir, None, None).is_empty());
        assert!(env_pairs("powershell", dir, None, None).is_empty());
        assert!(env_pairs("nu", dir, None, None).is_empty());
        assert!(env_pairs("", dir, None, None).is_empty());
    }

    #[test]
    fn shell_names_normalize_across_platforms() {
        assert_eq!(shell_display_name("/usr/local/bin/fish"), "fish");
        assert_eq!(shell_display_name("zsh"), "zsh");
        assert_eq!(
            shell_display_name("C:\\Program Files\\PowerShell\\7\\pwsh.exe"),
            "pwsh"
        );
        assert_eq!(
            shell_display_name(
                "C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
            ),
            "powershell"
        );
        assert_eq!(shell_display_name("PWSH.EXE"), "pwsh");
    }

    #[test]
    fn powershell_invocation_carries_the_script_inline() {
        use base64::engine::general_purpose::STANDARD as B64;
        use base64::Engine as _;

        let args = powershell_args();
        assert_eq!(args[0], "-NoExit");
        assert_eq!(args[1], "-EncodedCommand");

        // The payload decodes back to the exact script, UTF-16LE.
        let bytes = B64.decode(&args[2]).unwrap();
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        assert_eq!(String::from_utf16(&units).unwrap(), POWERSHELL_INTEGRATION);
    }

    #[test]
    fn materialize_writes_scripts_idempotently() {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path();

        materialize(base).unwrap();
        let zshenv = base.join("zsh").join(".zshenv");
        let fish = base
            .join("data")
            .join("fish")
            .join("vendor_conf.d")
            .join("rio.fish");
        assert_eq!(std::fs::read_to_string(&zshenv).unwrap(), ZSH_ZSHENV);
        assert!(base.join("zsh").join("rio-integration.zsh").exists());
        assert_eq!(std::fs::read_to_string(&fish).unwrap(), FISH_INTEGRATION);
        assert!(base.join("powershell").join("rio.ps1").exists());

        // A second run over existing content is a no-op, not an error.
        materialize(base).unwrap();
        assert_eq!(std::fs::read_to_string(&zshenv).unwrap(), ZSH_ZSHENV);
    }
}
