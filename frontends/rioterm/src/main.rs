// With the default subsystem, 'console', windows creates an additional console
// window for the program.
// This is silently ignored on non-windows systems.
// See https://msdn.microsoft.com/en-us/library/4cc7ya5b.aspx for more details.
#![windows_subsystem = "windows"]

mod application;
mod bindings;
mod cli;
mod constants;
mod context;
mod global_hotkey;
mod hints;
mod ime;
mod layout;
mod mouse;
#[cfg(windows)]
mod panic;
mod platform;
mod renderer;
mod router;
mod scheduler;
mod screen;
#[cfg(all(feature = "wayland", target_os = "linux"))]
mod tab_drag;
mod watcher;

use clap::Parser;
use rio_backend::config::config_dir_path;
use rio_backend::event::EventPayload;
use rio_backend::{crosswords, event, selection};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{
    self, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer,
};

#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    AttachConsole, FreeConsole, ATTACH_PARENT_PROCESS,
};

const LOG_LEVEL_ENV: &str = "RIO_LOG_LEVEL";

// Consumed by the first context manager only. The transfer identity arrives
// over inherited stdin, not argv or a discoverable global rendezvous.
static WINDOW_BOOTSTRAP: std::sync::Mutex<Option<[u8; 16]>> = std::sync::Mutex::new(None);

pub fn setup_environment_variables(config: &rio_backend::config::Config) {
    #[cfg(unix)]
    {
        let terminfo = match (
            teletypewriter::terminfo_exists("xterm-rio"),
            teletypewriter::terminfo_exists("rio"),
        ) {
            // In case `xterm-rio` exists we prioritize it
            (true, _) => "xterm-rio",
            // If is only `rio` installed (which was the default for versions under 0.2.27)
            (false, true) => "rio",
            // If none, then fallback to `xterm-256color`
            (false, false) => "xterm-256color",
        };

        let span = tracing::span!(tracing::Level::INFO, "setup_environment_variables");
        let _guard = span.enter();
        tracing::info!("terminfo: {terminfo}");
        std::env::set_var("TERM", terminfo);
    }

    // https://github.com/raphamorim/rio/issues/200
    std::env::set_var("TERM_PROGRAM", "rio");
    std::env::set_var("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));

    std::env::set_var("COLORTERM", "truecolor");
    std::env::remove_var("DESKTOP_STARTUP_ID");
    std::env::remove_var("XDG_ACTIVATION_TOKEN");
    #[cfg(target_os = "macos")]
    {
        platform::macos::set_locale_environment();
        std::env::set_current_dir(dirs::home_dir().unwrap()).unwrap();
    }

    // Set env vars from config.
    for env_config in config.env_vars.iter() {
        let env_vec: Vec<&str> = env_config.split('=').collect();

        if env_vec.len() == 2 {
            std::env::set_var(env_vec[0], env_vec[1]);
        }
    }
}

fn setup_logs_by_filter_level(
    log_level: &str,
    log_file: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut filter_level = LevelFilter::from_str(log_level).unwrap_or(LevelFilter::OFF);

    if let Ok(data) = std::env::var(LOG_LEVEL_ENV) {
        if !data.is_empty() {
            filter_level = LevelFilter::from_str(&data).unwrap_or(filter_level);
        }
    }

    let env_filter = EnvFilter::builder().with_default_directive(filter_level.into());
    let stdout_subscriber = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(true)
        .with_filter(env_filter.parse("")?);
    let subscriber = tracing_subscriber::registry().with(stdout_subscriber);

    let mut log_file_path = PathBuf::new();
    if log_file {
        let log_dir_path = config_dir_path().join("log");
        log_file_path = log_dir_path.join("rio.log");
        std::fs::create_dir_all(&log_dir_path)?;
        let log_file = std::fs::File::create(&log_file_path)?;
        let file_subscriber = tracing_subscriber::fmt::layer()
            .with_file(true)
            .with_line_number(true)
            .with_writer(log_file)
            .with_target(false)
            .with_ansi(false)
            .with_filter(env_filter.parse("")?);
        subscriber.with(file_subscriber).init();
    } else {
        subscriber.init();
    }

    let span = tracing::span!(tracing::Level::INFO, "logger");
    let _guard = span.enter();
    tracing::info!("logging level: {log_level}");
    if log_file {
        tracing::info!("logging to a file: {}", log_file_path.display());
    }
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `rio-session` can use the packaged executable as its worker.  Dispatch
    // before clap sees the private worker arguments; they contain the endpoint
    // and session id, while the capability itself is read from stdin by the
    // worker and is never present in argv.
    #[cfg(unix)]
    if std::env::args_os()
        .skip(1)
        .any(|argument| argument == "--endpoint")
    {
        return rio_session::worker::run().map_err(Into::into);
    }

    #[cfg(windows)]
    panic::attach_handler();

    // When linked with the windows subsystem windows won't automatically attach
    // to the console of the parent process, so we do it explicitly. This fails
    // silently if the parent has no console.
    #[cfg(windows)]
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }

    // Load command line options.
    let mut arguments: Vec<_> = std::env::args_os().collect();
    if crate::router::window_control::take_window_bootstrap_flag(&mut arguments) {
        let transfer =
            crate::router::window_control::read_bootstrap_identity(std::io::stdin())?;
        *WINDOW_BOOTSTRAP.lock().unwrap() = Some(transfer);
    }
    let args = cli::Cli::parse_from(arguments);

    let write_config_path = args.window_options.terminal_options.write_config.clone();
    if let Some(config_path) = write_config_path {
        let _ = setup_logs_by_filter_level("TRACE", false);
        rio_backend::config::create_config_file(config_path);
        return Ok(());
    }

    let (mut config, config_error) = match rio_backend::config::Config::try_load() {
        Ok(config) => (config, None),
        Err(err) => (rio_backend::config::Config::default(), Some(err)),
    };

    {
        let log_to_file = args.window_options.terminal_options.enable_log_file;
        if let Err(e) = setup_logs_by_filter_level(
            &config.developer.log_level,
            log_to_file || config.developer.enable_log_file,
        ) {
            eprintln!("unable to configure the logger: {e:?}");
        }

        if let Some(command) = args.window_options.terminal_options.command() {
            config.shell = command;
            config.use_fork = false;
        }

        if let Some(working_dir_cli) = args.window_options.terminal_options.working_dir {
            // Use dunce::canonicalize on Windows to avoid UNC paths (\\?\)
            // which break many tools like Neovim and Bun
            #[cfg(target_os = "windows")]
            let canonicalize_fn = dunce::canonicalize;
            #[cfg(not(target_os = "windows"))]
            let canonicalize_fn = std::fs::canonicalize;

            config.working_dir = match canonicalize_fn(&working_dir_cli).and_then(
                |path| {
                    if path.is_dir() {
                        path.into_os_string().into_string().map_err(|_| {
                            std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "Invalid UTF-8 in path",
                            )
                        })
                    } else {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::NotADirectory,
                            "Path is not a directory",
                        ))
                    }
                },
            ) {
                Ok(canonical_path) => Some(canonical_path),
                Err(e) => {
                    tracing::warn!("Failed to set working directory '{}': {}. Using default instead.", working_dir_cli, e);
                    None
                }
            };
        }

        config.title.placeholder = args.window_options.terminal_options.title_placeholder;
    }

    #[cfg(target_os = "linux")]
    {
        // If running inside a flatpak sandbox.
        // Rio will never use use_fork configuration as true
        if std::path::PathBuf::from("/.flatpak-info").exists() {
            config.use_fork = false;
        }
    }

    setup_environment_variables(&config);

    let window_event_loop =
        rio_window::event_loop::EventLoop::<EventPayload>::with_user_event().build()?;

    let app_id = args.window_options.terminal_options.app_id;

    let mut application = crate::application::Application::new(
        config,
        config_error,
        &window_event_loop,
        app_id,
    );
    let _ = application.run(window_event_loop);

    #[cfg(windows)]
    unsafe {
        FreeConsole();
    }

    Ok(())
}
