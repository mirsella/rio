#![cfg(unix)]

#[cfg(target_os = "macos")]
mod macos;
mod signals;

extern crate libc;

use crate::{ChildEvent, EventedPty, ProcessReadWrite, Winsize, WinsizeBuilder};
use corcovado::unix::EventedFd;
#[cfg(target_os = "macos")]
use macos::*;
use signal_hook::consts as sigconsts;
use signals::Signals;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io;
use std::io::Error;
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::ptr;

#[cfg(all(target_os = "linux", not(target_env = "musl")))]
const TIOCSWINSZ: libc::c_ulong = 0x5414;
#[cfg(all(target_os = "linux", target_env = "musl"))]
const TIOCSWINSZ: libc::c_int = 0x5414;
#[cfg(target_os = "freebsd")]
const TIOCSWINSZ: libc::c_ulong = 0x80087467;
#[cfg(target_os = "macos")]
const TIOCSWINSZ: libc::c_ulong = 2148037735;

#[link(name = "util")]
extern "C" {
    fn forkpty(
        main: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winsize: *const Winsize,
    ) -> libc::pid_t;

    fn openpty(
        main: *mut libc::c_int,
        child: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winsize: *const Winsize,
    ) -> libc::pid_t;

}

fn reset_child_signals() -> io::Result<()> {
    unsafe {
        for signal in [
            libc::SIGABRT,
            libc::SIGALRM,
            libc::SIGBUS,
            libc::SIGCHLD,
            libc::SIGFPE,
            libc::SIGHUP,
            libc::SIGILL,
            libc::SIGINT,
            libc::SIGPIPE,
            libc::SIGQUIT,
            libc::SIGSEGV,
            libc::SIGTERM,
            libc::SIGTRAP,
            libc::SIGTSTP,
            libc::SIGTTIN,
            libc::SIGTTOU,
        ] {
            if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
        }
        let mut mask = std::mem::zeroed();
        if libc::sigemptyset(&mut mask) == -1
            || libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) == -1
        {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

/// Expand a leading `~` against the current user's home directory.
fn expand_tilde(dir: &str) -> Option<String> {
    if dir == "~" || dir.starts_with("~/") {
        let Some(home) = dirs::home_dir() else {
            tracing::warn!(
                "working-dir {dir:?} needs a home directory; inheriting the current directory"
            );
            return None;
        };
        if dir == "~" {
            Some(home.to_string_lossy().into_owned())
        } else {
            Some(
                home.join(dir[2..].trim_start_matches('/'))
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    } else {
        Some(dir.to_string())
    }
}

/// Expand a leading `~` and validate the configured working
/// directory. Unusable values fall back to inheriting the parent's
/// directory with a warning instead of failing the spawn or silently
/// landing somewhere unexpected.
fn resolve_working_dir(dir: Option<&str>) -> io::Result<Option<String>> {
    let Some(dir) = dir else {
        return Ok(None);
    };
    let Some(expanded) = expand_tilde(dir) else {
        return Ok(None);
    };
    if expanded.as_bytes().contains(&0) {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "working-dir contains a NUL byte",
        ));
    }
    if Path::new(&expanded).is_dir() {
        Ok(Some(expanded))
    } else {
        tracing::warn!(
            "working-dir {expanded:?} is not a directory; inheriting the current directory"
        );
        Ok(None)
    }
}

struct ShellCommand {
    program: CString,
    _argv: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
}

impl ShellCommand {
    fn new(shell: &str, args: &[String]) -> io::Result<Self> {
        let program = CString::new(shell).map_err(|_| {
            Error::new(io::ErrorKind::InvalidInput, "shell contains a NUL byte")
        })?;

        #[allow(unused_mut)]
        let mut arg0 = program.clone();
        #[cfg(target_os = "macos")]
        if args.is_empty() {
            let name = shell.rsplit('/').next().unwrap_or(shell);
            arg0 = CString::new(format!("-{name}")).map_err(|_| {
                Error::new(io::ErrorKind::InvalidInput, "shell contains a NUL byte")
            })?;
        }

        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(arg0);
        for arg in args {
            argv.push(CString::new(arg.as_str()).map_err(|_| {
                Error::new(
                    io::ErrorKind::InvalidInput,
                    "shell argument contains a NUL byte",
                )
            })?);
        }
        let mut argv_ptrs = argv.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
        argv_ptrs.push(std::ptr::null());

        Ok(Self {
            program,
            _argv: argv,
            argv_ptrs,
        })
    }

    fn exec(&self) -> ! {
        if reset_child_signals().is_err() {
            unsafe { libc::_exit(126) };
        }
        unsafe {
            libc::execvp(self.program.as_ptr(), self.argv_ptrs.as_ptr());
            libc::_exit(127);
        }
    }
}

pub struct Pty {
    pub child: Child,
    file: File,
    token: corcovado::Token,
    signals_token: corcovado::Token,
    signals: Signals,
}

impl io::Write for Pty {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.file, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.file)
    }
}

impl io::Read for Pty {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.file, buf)
    }
}

impl ProcessReadWrite for Pty {
    type Reader = File;
    type Writer = File;

    #[inline]
    fn reader(&mut self) -> &mut File {
        &mut self.file
    }

    #[inline]
    fn read_token(&self) -> corcovado::Token {
        self.token
    }

    #[inline]
    fn writer(&mut self) -> &mut File {
        &mut self.file
    }

    #[inline]
    fn write_token(&self) -> corcovado::Token {
        self.token
    }

    #[inline]
    fn set_winsize(&mut self, winsize: WinsizeBuilder) -> Result<(), std::io::Error> {
        self.child.set_winsize(winsize)
    }

    #[inline]
    fn register(
        &mut self,
        poll: &corcovado::Poll,
        token: &mut dyn Iterator<Item = corcovado::Token>,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        self.token = token.next().unwrap();
        poll.register(
            &EventedFd(&self.file.as_raw_fd()),
            self.token,
            interest,
            poll_opts,
        )?;

        self.signals_token = token.next().unwrap();
        poll.register(
            &self.signals,
            self.signals_token,
            corcovado::Ready::readable(),
            corcovado::PollOpt::level(),
        )
    }

    fn reregister(
        &mut self,
        poll: &corcovado::Poll,
        interest: corcovado::Ready,
        poll_opts: corcovado::PollOpt,
    ) -> io::Result<()> {
        poll.reregister(
            &EventedFd(&self.file.as_raw_fd()),
            self.token,
            interest,
            poll_opts,
        )?;

        poll.reregister(
            &self.signals,
            self.signals_token,
            corcovado::Ready::readable(),
            corcovado::PollOpt::level(),
        )
    }

    fn deregister(&mut self, poll: &corcovado::Poll) -> io::Result<()> {
        poll.deregister(&EventedFd(&self.file.as_raw_fd()))?;
        poll.deregister(&self.signals)
    }
}

// From alacritty: https://github.com/alacritty/alacritty/blob/2df8f860b960d7c96efaf4f059fe2fbbdce82bcc/alacritty_terminal/src/tty/mod.rs#L83
/// Check if a terminfo entry exists on the system.
pub fn terminfo_exists(terminfo: &str) -> bool {
    // Get first terminfo character for the parent directory.
    let first = terminfo.get(..1).unwrap_or_default();
    let first_hex = format!("{:x}", first.chars().next().unwrap_or_default() as usize);

    // Return true if the terminfo file exists at the specified location.
    macro_rules! check_path {
        ($path:expr) => {
            if $path.join(first).join(terminfo).exists()
                || $path.join(&first_hex).join(terminfo).exists()
            {
                return true;
            }
        };
    }

    if let Some(dir) = std::env::var_os("TERMINFO") {
        check_path!(PathBuf::from(&dir));
    } else if let Some(home) = dirs::home_dir() {
        check_path!(home.join(".terminfo"));
    }

    if let Ok(dirs) = std::env::var("TERMINFO_DIRS") {
        for dir in dirs.split(':') {
            check_path!(PathBuf::from(dir));
        }
    }

    if let Ok(prefix) = std::env::var("PREFIX") {
        let path = PathBuf::from(prefix);
        check_path!(path.join("etc/terminfo"));
        check_path!(path.join("lib/terminfo"));
        check_path!(path.join("share/terminfo"));
    }

    check_path!(PathBuf::from("/etc/terminfo"));
    check_path!(PathBuf::from("/lib/terminfo"));
    check_path!(PathBuf::from("/usr/share/terminfo"));
    check_path!(PathBuf::from("/boot/system/data/terminfo"));

    // No valid terminfo path has been found.
    false
}

pub fn create_termp(utf8: bool) -> libc::termios {
    // musl libc does not provide c_ispeed and c_ospeed fields in struct termios.
    #[cfg(target_os = "linux")]
    let mut term = libc::termios {
        c_iflag: libc::ICRNL | libc::IXON | libc::IXANY | libc::IMAXBEL | libc::BRKINT,
        c_oflag: libc::OPOST | libc::ONLCR,
        c_cflag: libc::CREAD | libc::CS8 | libc::HUPCL,
        c_lflag: libc::ICANON
            | libc::ISIG
            | libc::IEXTEN
            | libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHOKE
            | libc::ECHOCTL,
        c_cc: Default::default(),
        #[cfg(not(target_env = "musl"))]
        c_ispeed: Default::default(),
        #[cfg(not(target_env = "musl"))]
        c_ospeed: Default::default(),
        #[cfg(target_env = "musl")]
        __c_ispeed: Default::default(),
        #[cfg(target_env = "musl")]
        __c_ospeed: Default::default(),
        c_line: 0,
    };

    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    let mut term = libc::termios {
        c_iflag: libc::ICRNL | libc::IXON | libc::IXANY | libc::IMAXBEL | libc::BRKINT,
        c_oflag: libc::OPOST | libc::ONLCR,
        c_cflag: libc::CREAD | libc::CS8 | libc::HUPCL,
        c_lflag: libc::ICANON
            | libc::ISIG
            | libc::IEXTEN
            | libc::ECHO
            | libc::ECHOE
            | libc::ECHOK
            | libc::ECHOKE
            | libc::ECHOCTL,
        c_cc: Default::default(),
        c_ispeed: Default::default(),
        c_ospeed: Default::default(),
    };

    #[cfg(not(target_os = "freebsd"))]
    {
        // Enable utf8 support if requested
        if utf8 {
            term.c_iflag |= libc::IUTF8;
        }
    }

    // Set supported terminal characters
    term.c_cc[libc::VEOF] = 4;
    term.c_cc[libc::VEOL] = 255;
    term.c_cc[libc::VEOL2] = 255;
    term.c_cc[libc::VERASE] = 0x7f;
    term.c_cc[libc::VWERASE] = 23;
    term.c_cc[libc::VKILL] = 21;
    term.c_cc[libc::VREPRINT] = 18;
    term.c_cc[libc::VINTR] = 3;
    term.c_cc[libc::VQUIT] = 0x1c;
    term.c_cc[libc::VSUSP] = 26;
    term.c_cc[libc::VSTART] = 17;
    term.c_cc[libc::VSTOP] = 19;
    term.c_cc[libc::VLNEXT] = 22;
    term.c_cc[libc::VDISCARD] = 15;
    term.c_cc[libc::VMIN] = 1;
    term.c_cc[libc::VTIME] = 0;

    #[cfg(target_os = "macos")]
    {
        term.c_cc[libc::VDSUSP] = 25;
        term.c_cc[libc::VSTATUS] = 20;
    }

    term
}

#[derive(Default)]
struct ShellUser {
    user: String,
    home: String,
    shell: String,
}

impl ShellUser {
    /// look for shell, username, longname, and home dir in the respective environment variables
    /// before falling back on looking in to `passwd`.
    fn from_env() -> Result<Self, Error> {
        let mut buf = [0; 1024];
        let pw = get_pw_entry(&mut buf);

        let user = match std::env::var("USER") {
            Ok(user) => user,
            Err(_) => match pw {
                Ok(ref pw) => pw.name.to_owned(),
                Err(err) => return Err(err),
            },
        };

        let home = match std::env::var("HOME") {
            Ok(home) => home,
            Err(_) => match pw {
                Ok(ref pw) => pw.dir.to_owned(),
                Err(err) => return Err(err),
            },
        };

        let shell = match std::env::var("SHELL") {
            Ok(env_shell) => env_shell,
            Err(_) => match pw {
                Ok(ref pw) => pw.shell.to_owned(),
                Err(err) => return Err(err),
            },
        };

        Ok(Self { user, home, shell })
    }
}

///
/// Build the argv passed to login(1) on macOS.
///
/// A custom command (non empty args) goes straight into login's argv:
/// login execvp's it, so args pass through as single words with no
/// intermediate shell that could word split them.
///
/// A bare shell becomes a login shell through a bash intermediate that
/// execs it with `-l`, which prepends the dash to argv[0]. bash runs
/// with `--noprofile --norc` so user startup files cannot interfere
/// with the exec.
#[cfg(any(target_os = "macos", test))]
fn login_argv(
    hushlogin: bool,
    username: &str,
    shell_program: &str,
    args: &[String],
) -> Vec<String> {
    let mut argv: Vec<String> = Vec::with_capacity(args.len() + 8);

    // -f: Bypasses authentication for already-logged-in user
    // -l: Skips changing directory to $HOME
    // -p: Preserves environment
    // -q: Act as if .hushlogin exists
    if hushlogin {
        argv.push("-q".to_string());
    }
    argv.push("-flp".to_string());
    argv.push(username.to_string());

    if args.is_empty() {
        let quoted = shell_program.replace('\'', "'\\''");
        argv.push("/bin/bash".to_string());
        argv.push("--noprofile".to_string());
        argv.push("--norc".to_string());
        argv.push("-c".to_string());
        argv.push(format!("exec -l '{quoted}'"));
    } else {
        argv.push(shell_program.to_string());
        argv.extend(args.iter().cloned());
    }

    argv
}

/// `env`, when given, is applied on top of the inherited environment,
/// overriding inherited variables of the same name. `None` inherits as-is.
///
/// `shell` of `None` means no program was configured: the user's default shell
/// is looked up and, on macOS, wrapped in `/usr/bin/login` so the child gets a
/// login session. A caller that names a program gets exactly that program,
/// spawned directly, with no `login` in between.
#[allow(clippy::too_many_arguments)]
pub fn create_pty_with_spawn(
    shell: Option<&str>,
    args: Vec<String>,
    working_directory: &Option<String>,
    env: Option<Vec<(String, String)>>,
    columns: u16,
    rows: u16,
    width: u16,
    height: u16,
) -> Result<Pty, Error> {
    // Only expanded here: the flatpak branch below hands the path to
    // the host, which may see directories this sandbox cannot, so
    // existence is validated at the local use site instead.
    let working_directory = working_directory.as_deref().and_then(expand_tilde);
    if working_directory
        .as_deref()
        .is_some_and(|dir| dir.as_bytes().contains(&0))
    {
        return Err(Error::new(
            io::ErrorKind::InvalidInput,
            "working-dir contains a NUL byte",
        ));
    }

    #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
    let mut is_controling_terminal = true;

    #[cfg(any(target_os = "macos", target_os = "freebsd"))]
    let is_controling_terminal = true;

    let mut main: libc::c_int = 0;
    let mut child: libc::c_int = 0;
    let winsize = Winsize {
        ws_row: rows as libc::c_ushort,
        ws_col: columns as libc::c_ushort,
        ws_xpixel: width as libc::c_ushort,
        ws_ypixel: height as libc::c_ushort,
    };
    let term = create_termp(true);

    let res = unsafe {
        openpty(
            &mut main as *mut _,
            &mut child as *mut _,
            ptr::null_mut(),
            &term as *const libc::termios,
            &winsize as *const _,
        )
    };

    if res < 0 {
        return Err(Error::other("openpty failed"));
    }
    let file = unsafe { File::from_raw_fd(main) };
    let owned_child = unsafe { OwnedFd::from_raw_fd(child) };
    set_cloexec(file.as_raw_fd())?;
    set_cloexec(owned_child.as_raw_fd())?;

    let user = match ShellUser::from_env() {
        Ok(data) => data,
        Err(..) => ShellUser {
            shell: shell.unwrap_or_default().to_string(),
            ..Default::default()
        },
    };

    // No program means the caller wants the user's default shell, which is the
    // only case that goes through `login`. A named program is spawned as given.
    #[cfg(target_os = "macos")]
    let uses_default_shell = shell.is_none();
    let shell_program = shell.unwrap_or(&user.shell);

    tracing::info!("spawn {:?} {:?}", shell_program, args);

    let mut builder = {
        #[cfg(target_os = "macos")]
        {
            if uses_default_shell {
                // On macOS, use /usr/bin/login to ensure proper login shell environment
                // This ensures PATH includes directories like /usr/local/bin
                let hushlogin = Path::new(&user.home).join(".hushlogin").exists();

                let mut login_cmd = Command::new("/usr/bin/login");
                login_cmd.args(login_argv(hushlogin, &user.user, shell_program, &args));

                login_cmd
            } else {
                let mut cmd = Command::new(shell_program);
                cmd.args(args);
                cmd
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let mut cmd = Command::new(shell_program);
            cmd.args(args);
            cmd
        }
    };

    #[cfg(target_os = "linux")]
    {
        // If running inside a flatpak sandbox.
        // Must retrieve $SHELL from outside the sandbox, so ask the host.
        if PathBuf::from("/.flatpak-info").exists() {
            builder = Command::new("flatpak-spawn");

            let mut with_args = vec![
                "--host".to_string(),
                "--watch-bus".to_string(),
                "--env=COLORTERM=truecolor".to_string(),
                "--env=TERM=rio".to_string(),
            ];

            if let Some(directory) = &working_directory {
                with_args.push(format!("--directory={}", Path::new(directory).display()));
            }

            let output = std::process::Command::new("flatpak-spawn")
                .args(["--host", "sh", "-c", "echo $SHELL"])
                .output()?;
            let shell = String::from_utf8_lossy(&output.stdout);

            with_args.push(shell.trim().to_string());
            with_args.push("-l".to_string());

            builder.args(with_args);

            is_controling_terminal = false;
        }
    }

    // Give each standard stream its own slave descriptor.
    builder.stdin(owned_child.try_clone()?);
    builder.stderr(owned_child.try_clone()?);
    builder.stdout(owned_child);

    builder.env("USER", user.user);
    builder.env("HOME", user.home);
    if let Some(env) = env {
        builder.envs(env);
    }

    unsafe {
        builder.pre_exec(move || {
            // Create a new process group.
            let err = libc::setsid();
            if err == -1 {
                return Err(Error::last_os_error());
            }

            if is_controling_terminal {
                set_controlling_terminal(child)?;
            }

            reset_child_signals()?;

            Ok(())
        });
    }

    // Handle set working directory option.
    if let Some(dir) = &working_directory {
        if Path::new(dir).is_dir() {
            builder.current_dir(dir);
        } else {
            tracing::warn!(
                "working-dir {dir:?} is not a directory; inheriting the current directory"
            );
        }
    }

    // Prepare signal handling before spawning child.
    let signals = Signals::new([sigconsts::SIGCHLD])?;

    match builder.spawn() {
        Ok(child_process) => {
            // Establish lifecycle ownership before any fallible parent-side setup.
            let child = Child::new(main, child_process.id() as libc::pid_t);
            set_nonblocking(main)?;

            Ok(Pty {
                child,
                file,
                token: corcovado::Token::from(0),
                signals,
                signals_token: corcovado::Token::from(0),
            })
        }
        Err(err) => Err(Error::new(
            err.kind(),
            format!(
                "Failed to spawn command '{}': {}",
                builder.get_program().to_string_lossy(),
                err
            ),
        )),
    }
}

/// Creates a pseudoterminal using `forkpty`.
pub fn create_pty_with_fork(
    shell: Option<&str>,
    args: &[String],
    working_directory: &Option<String>,
    columns: u16,
    rows: u16,
    width: u16,
    height: u16,
) -> Result<Pty, Error> {
    // Resolved in the parent so the failure path can log; the fork
    // child only consumes the already-validated CString.
    let working_directory = resolve_working_dir(working_directory.as_deref())?
        .map(CString::new)
        .transpose()
        .map_err(|_| {
            Error::new(
                io::ErrorKind::InvalidInput,
                "working-dir contains a NUL byte",
            )
        })?;
    let mut main = 0;
    let winsize = Winsize {
        ws_row: rows as libc::c_ushort,
        ws_col: columns as libc::c_ushort,
        ws_xpixel: width as libc::c_ushort,
        ws_ypixel: height as libc::c_ushort,
    };
    let term = create_termp(true);

    let user = match ShellUser::from_env() {
        Ok(data) => data,
        Err(..) => ShellUser {
            shell: shell.unwrap_or_default().to_string(),
            ..Default::default()
        },
    };

    let shell_program = shell.unwrap_or_else(|| {
        tracing::info!("no shell configured, will retrieve from env");
        &user.shell
    });

    tracing::info!("fork {:?}", shell_program);
    let command = ShellCommand::new(shell_program, args)?;
    let signals = Signals::new([sigconsts::SIGCHLD])?;

    match unsafe {
        forkpty(
            &mut main as *mut _,
            ptr::null_mut(),
            &term as *const libc::termios,
            &winsize as *const _,
        )
    } {
        0 => {
            if let Some(dir) = &working_directory {
                if unsafe { libc::chdir(dir.as_ptr()) } != 0 {
                    unsafe { libc::_exit(126) };
                }
            }
            command.exec()
        }
        id if id > 0 => {
            let file = unsafe { File::from_raw_fd(main) };
            // Establish lifecycle ownership before any fallible parent-side setup.
            let child = Child::new(main, id);
            set_cloexec(main)?;
            set_nonblocking(main)?;
            Ok(Pty {
                child,
                signals,
                file,
                token: corcovado::Token(0),
                signals_token: corcovado::Token(0),
            })
        }
        _ => Err(Error::other(format!(
            "forkpty failed using {shell_program}"
        ))),
    }
}

/// Really only needed on BSD, but should be fine elsewhere.
fn set_controlling_terminal(fd: libc::c_int) -> Result<(), Error> {
    let res = unsafe {
        // TIOSCTTY changes based on platform and the `ioctl` call is different
        // based on architecture (32/64). So a generic cast is used to make sure
        // there are no issues. To allow such a generic cast the clippy warning
        // is disabled.
        #[allow(clippy::cast_lossless)]
        libc::ioctl(fd, libc::TIOCSCTTY as _, 0)
    };

    if res < 0 {
        return Err(Error::last_os_error());
    }

    Ok(())
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug)]
pub struct Child {
    pub id: libc::c_int,
    pub pid: libc::pid_t,
    exited: bool,
}

impl Child {
    fn new(id: RawFd, pid: libc::pid_t) -> Self {
        Self {
            id,
            pid,
            exited: false,
        }
    }

    fn set_winsize(&self, winsize_builder: WinsizeBuilder) -> io::Result<()> {
        let winsize: Winsize = winsize_builder.build();
        match unsafe { libc::ioctl(self.id, TIOCSWINSZ, &winsize as *const _) } {
            -1 => Err(io::Error::last_os_error()),
            _ => Ok(()),
        }
    }

    fn try_wait(&mut self) -> io::Result<Option<i32>> {
        let result = wait_for_child(self.pid, libc::WNOHANG);
        if matches!(&result, Ok(Some(_)))
            || matches!(&result, Err(error) if error.raw_os_error() == Some(libc::ECHILD))
        {
            self.exited = true;
        }
        result
    }
}

fn wait_for_child(pid: libc::pid_t, options: libc::c_int) -> io::Result<Option<i32>> {
    loop {
        let mut status = 0;
        match unsafe { libc::waitpid(pid, &mut status, options) } {
            -1 => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
            0 => return Ok(None),
            _ => return Ok(Some(status)),
        }
    }
}

fn reap_child(pid: libc::pid_t) {
    let _ = wait_for_child(pid, 0);
}

fn terminate_and_reap_child(pid: libc::pid_t) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        match wait_for_child(pid, libc::WNOHANG) {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if std::time::Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
        }
    }

    unsafe { libc::kill(pid, libc::SIGKILL) };
    reap_child(pid);
}

impl Drop for Child {
    fn drop(&mut self) {
        if self.exited {
            return;
        }

        match self.try_wait() {
            Ok(Some(_)) => return,
            Err(_) if self.exited => return,
            Err(error) => {
                tracing::warn!("failed to inspect PTY child before shutdown: {error}");
                return;
            }
            Ok(None) => {}
        }

        let pid = self.pid;
        unsafe { libc::kill(pid, libc::SIGHUP) };
        if let Err(error) = std::thread::Builder::new()
            .name("pty-reaper".into())
            .spawn(move || terminate_and_reap_child(pid))
        {
            tracing::error!("failed to start PTY child reaper: {error}");
            unsafe { libc::kill(pid, libc::SIGKILL) };
            reap_child(pid);
        }
    }
}

impl EventedPty for Pty {
    #[inline]
    fn next_child_event(&mut self) -> Option<ChildEvent> {
        self.signals.pending().next().and_then(|signal| {
            if signal != sigconsts::SIGCHLD {
                return None;
            }

            match self.child.try_wait() {
                Err(_) if self.child.exited => Some(ChildEvent::Exited(None)),
                Err(error) => {
                    tracing::warn!("failed to collect PTY child status: {error}");
                    None
                }
                Ok(None) => None,
                Ok(Some(status)) => Some(ChildEvent::Exited(Some(status))),
            }
        })
    }

    #[inline]
    fn child_event_token(&self) -> corcovado::Token {
        self.signals_token
    }
}

#[derive(Debug)]
struct Passwd<'a> {
    name: &'a str,
    dir: &'a str,
    shell: &'a str,
}

/// Return a Passwd struct with pointers into the provided buf.
///
/// # Unsafety
///
/// If `buf` is changed while `Passwd` is alive, bad thing will almost certainly happen.
fn get_pw_entry(buf: &mut [i8; 1024]) -> Result<Passwd<'_>, Error> {
    // Create zeroed passwd struct.
    let mut entry: MaybeUninit<libc::passwd> = MaybeUninit::uninit();

    let mut res: *mut libc::passwd = ptr::null_mut();

    // Try and read the pw file.
    let uid = unsafe { libc::getuid() };
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            entry.as_mut_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            &mut res,
        )
    };
    let entry = unsafe { entry.assume_init() };

    if status < 0 {
        return Err(Error::other("getpwuid_r failed"));
    }

    if res.is_null() {
        return Err(Error::other("pw not found"));
    }

    // Sanity check.
    assert_eq!(entry.pw_uid, uid);

    // Build a borrowed Passwd struct.
    Ok(Passwd {
        name: unsafe { CStr::from_ptr(entry.pw_name).to_str().unwrap() },
        dir: unsafe { CStr::from_ptr(entry.pw_dir).to_str().unwrap() },
        shell: unsafe { CStr::from_ptr(entry.pw_shell).to_str().unwrap() },
    })
}

pub fn foreground_process_name(main_fd: RawFd, shell_pid: u32) -> String {
    let mut pid = unsafe { libc::tcgetpgrp(main_fd) };
    if pid < 0 {
        pid = shell_pid as libc::pid_t;
    }

    #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
    let comm_path = format!("/proc/{pid}/comm");
    #[cfg(target_os = "freebsd")]
    let comm_path = format!("/compat/linux/proc/{pid}/comm");

    #[cfg(not(target_os = "macos"))]
    let name = match std::fs::read(comm_path) {
        Ok(comm_str) => String::from_utf8_lossy(&comm_str)
            .trim_end()
            .parse()
            .unwrap_or_default(),
        Err(..) => String::from(""),
    };

    #[cfg(target_os = "macos")]
    let name = macos_process_name(pid);

    name
}

pub fn foreground_process_path(
    main_fd: RawFd,
    shell_pid: u32,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let mut pid = unsafe { libc::tcgetpgrp(main_fd) };
    if pid < 0 {
        pid = shell_pid as libc::pid_t;
    }

    #[cfg(not(any(target_os = "macos", target_os = "freebsd")))]
    let link_path = format!("/proc/{pid}/cwd");
    #[cfg(target_os = "freebsd")]
    let link_path = format!("/compat/linux/proc/{pid}/cwd");

    #[cfg(not(target_os = "macos"))]
    let cwd = std::fs::read_link(link_path)?;

    #[cfg(target_os = "macos")]
    let cwd = macos_cwd(pid)?;

    Ok(cwd)
}

/// Start a new process in the background.
pub fn spawn_daemon<I, S>(program: &str, args: I, cwd: Option<&Path>) -> io::Result<()>
where
    I: IntoIterator<Item = S> + Copy,
    S: AsRef<std::ffi::OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    unsafe {
        command
            .pre_exec(|| {
                match libc::fork() {
                    -1 => return Err(io::Error::last_os_error()),
                    0 => (),
                    _ => libc::_exit(0),
                }

                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }

                Ok(())
            })
            .spawn()?
            .wait()
            .map(|_| ())
    }
}

#[cfg(test)]
mod resolve_working_dir_tests {
    use super::{resolve_working_dir, ShellCommand};

    #[test]
    fn shell_command_rejects_nul_bytes_before_fork() {
        assert!(ShellCommand::new("/bin/sh", &["bad\0argument".into()]).is_err());
        assert!(resolve_working_dir(Some("/tmp/bad\0directory")).is_err());
    }

    #[test]
    fn working_directory_resolution() {
        let home = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        for (input, expected) in [
            (None, None),
            (Some("~"), Some(home.clone())),
            (Some("~/."), Some(format!("{home}/."))),
            (Some("~//."), Some(format!("{home}/."))),
            (Some("/"), Some("/".into())),
            (Some("/definitely/not/a/real/dir"), None),
            (Some("/tmp/~x"), None),
        ] {
            assert_eq!(resolve_working_dir(input).unwrap(), expected);
        }
    }
}

#[cfg(test)]
mod child_wait_tests {
    use super::Child;

    fn exited_child(code: libc::c_int) -> libc::pid_t {
        match unsafe { libc::fork() } {
            0 => unsafe { libc::_exit(code) },
            pid if pid > 0 => pid,
            _ => panic!("fork failed: {}", std::io::Error::last_os_error()),
        }
    }

    fn live_child(ignore_hangup: bool) -> libc::pid_t {
        let mut ready = [0; 2];
        if unsafe { libc::pipe(ready.as_mut_ptr()) } == -1 {
            panic!("pipe failed: {}", std::io::Error::last_os_error());
        }

        match unsafe { libc::fork() } {
            0 => unsafe {
                libc::close(ready[0]);
                libc::signal(
                    libc::SIGHUP,
                    if ignore_hangup {
                        libc::SIG_IGN
                    } else {
                        libc::SIG_DFL
                    },
                );
                let ready_byte = 1_u8;
                if libc::write(ready[1], &ready_byte as *const u8 as *const _, 1) != 1 {
                    libc::_exit(127);
                }
                libc::close(ready[1]);
                loop {
                    libc::pause();
                }
            },
            pid if pid > 0 => {
                unsafe {
                    libc::close(ready[1]);
                }
                let mut ready_byte = 0_u8;
                loop {
                    match unsafe {
                        libc::read(ready[0], &mut ready_byte as *mut u8 as *mut _, 1)
                    } {
                        1 => break,
                        -1 if std::io::Error::last_os_error().kind()
                            == std::io::ErrorKind::Interrupted => {}
                        result => panic!("child readiness failed: read={result}"),
                    }
                }
                unsafe {
                    libc::close(ready[0]);
                }
                pid
            }
            _ => {
                unsafe {
                    libc::close(ready[0]);
                    libc::close(ready[1]);
                }
                panic!("fork failed: {}", std::io::Error::last_os_error());
            }
        }
    }

    fn assert_drop_reaps_child(pid: libc::pid_t, timeout: std::time::Duration) {
        drop(Child::new(-1, pid));

        let deadline = std::time::Instant::now() + timeout;
        loop {
            let mut status = 0;
            match unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) } {
                child if child == pid => break,
                -1 if std::io::Error::last_os_error().raw_os_error()
                    == Some(libc::ECHILD) =>
                {
                    break;
                }
                0 if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                result => panic!("child was not reaped before timeout: waitpid={result}"),
            }
        }
    }

    #[test]
    fn waitpid_reports_normal_exit() {
        let pid = exited_child(7);
        let mut child = Child::new(-1, pid);

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(std::time::Instant::now() < deadline, "child did not exit");
            std::thread::yield_now();
        };

        assert!(libc::WIFEXITED(status));
        assert_eq!(libc::WEXITSTATUS(status), 7);
    }

    #[test]
    fn waitpid_marks_externally_reaped_child_exited() {
        let pid = exited_child(0);
        let mut status = 0;
        while unsafe { libc::waitpid(pid, &mut status, 0) } == -1 {
            assert_eq!(
                std::io::Error::last_os_error().kind(),
                std::io::ErrorKind::Interrupted
            );
        }

        let mut child = Child::new(-1, pid);
        let error = child.try_wait().unwrap_err();

        assert_eq!(error.raw_os_error(), Some(libc::ECHILD));
        assert!(child.exited);
    }

    #[test]
    fn dropping_live_child_terminates_and_reaps_it() {
        assert_drop_reaps_child(live_child(false), std::time::Duration::from_secs(2));
    }

    #[test]
    fn dropping_child_that_ignores_hangup_escalates_and_reaps_it() {
        assert_drop_reaps_child(live_child(true), std::time::Duration::from_secs(3));
    }
}

#[cfg(test)]
mod login_argv_tests {
    use super::login_argv;

    #[test]
    fn bare_shell_becomes_login_shell() {
        let argv = login_argv(false, "rapha", "/bin/zsh", &[]);
        assert_eq!(
            argv,
            vec![
                "-flp",
                "rapha",
                "/bin/bash",
                "--noprofile",
                "--norc",
                "-c",
                "exec -l '/bin/zsh'",
            ]
        );
    }

    #[test]
    fn hushlogin_adds_quiet_flag() {
        let argv = login_argv(true, "rapha", "/bin/zsh", &[]);
        assert_eq!(argv[0], "-q");
        assert_eq!(argv[1], "-flp");
    }

    #[test]
    fn custom_command_goes_directly_to_login() {
        let args = vec!["-c".to_string(), "echo hello world; sleep 1".to_string()];
        let argv = login_argv(false, "rapha", "/bin/bash", &args);
        assert_eq!(
            argv,
            vec![
                "-flp",
                "rapha",
                "/bin/bash",
                "-c",
                "echo hello world; sleep 1",
            ]
        );
    }

    #[test]
    fn quotes_in_shell_path_are_escaped() {
        let argv = login_argv(false, "rapha", "/tmp/it's a shell", &[]);
        assert_eq!(argv.last().unwrap(), "exec -l '/tmp/it'\\''s a shell'");
    }
}
