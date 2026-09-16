use crate::context::Context;
#[cfg(unix)]
use std::path::Path;

pub struct ContextTitleExtra {
    pub program: String,
}

pub struct ContextTitle {
    pub content: String,
    pub extra: Option<ContextTitleExtra>,
}

impl Default for ContextTitle {
    fn default() -> Self {
        Self {
            content: String::from("~"),
            extra: None,
        }
    }
}

pub fn create_title_extra_from_context<T: rio_backend::event::EventListener>(
    context: &Context<T>,
) -> Option<ContextTitleExtra> {
    let program = context.foreground_process_name()?;
    Some(ContextTitleExtra { program })
}

// Possible options:

// - `TITLE`: terminal title via OSC sequences for setting terminal title
// - `PROGRAM`: (e.g `fish`, `zsh`, `bash`, `vim`, etc...)
// - `ABSOLUTE_PATH`: (e.g `/Users/rapha/Documents/a/rio`)
// - `RELATIVE_PATH`: (e.g `~/Documents/a/rio` or `…/a/psone/starpsx`)
// - `COLUMNS`: current columns
// - `LINES`: current lines

/// Shorten an absolute path for display:
/// - Replace home directory prefix with `~`
/// - If 4+ components deep, show `…/last/three/components`
fn shorten_path(absolute: &str) -> String {
    #[cfg(unix)]
    let path = Path::new(absolute);

    // Replace home prefix with ~
    #[cfg(unix)]
    let display_path = {
        if let Some(home) = dirs::home_dir() {
            if let Ok(stripped) = path.strip_prefix(&home) {
                let s = stripped.to_string_lossy();
                if s.is_empty() {
                    "~".to_string()
                } else {
                    format!("~/{s}")
                }
            } else {
                absolute.to_string()
            }
        } else {
            absolute.to_string()
        }
    };

    #[cfg(not(unix))]
    let display_path = absolute.to_string();

    // If 4+ components, show …/last3
    let components: Vec<&str> =
        display_path.split('/').filter(|s| !s.is_empty()).collect();
    if components.len() >= 4 {
        format!("…/{}", components[components.len() - 3..].join("/"))
    } else {
        display_path
    }
}

fn current_path<T: rio_backend::event::EventListener>(
    context: &Context<T>,
) -> Option<String> {
    // Take an owned copy first: the lock guard must be dropped before the
    // `or_else` fallback below, because `foreground_process_path` locks the
    // terminal as well and the mutex is not reentrant.
    let directory = context.terminal.lock().current_directory.clone();
    directory
        .and_then(|path| path.into_os_string().into_string().ok())
        .or_else(|| {
            context
                .foreground_process_path()
                .map(|path| path.to_string_lossy().into_owned())
        })
}

fn variable_value<T: rio_backend::event::EventListener>(
    variable: &str,
    context: &Context<T>,
) -> Option<String> {
    match variable.trim().to_ascii_lowercase().as_str() {
        "columns" => Some(context.dimension.columns.to_string()),
        "lines" => Some(context.dimension.lines.to_string()),
        "title" => Some(context.terminal.lock().title.clone()),
        "program" => Some(context.foreground_process_name().unwrap_or_default()),
        "absolute_path" => Some(current_path(context).unwrap_or_default()),
        "relative_path" => Some(
            current_path(context)
                .map(|path| shorten_path(&path))
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

#[inline]
pub fn update_title<T: rio_backend::event::EventListener>(
    template: &str,
    context: &Context<T>,
) -> String {
    if template.is_empty() {
        return template.to_string();
    }

    let mut new_template = template.to_owned();

    let re = regex::Regex::new(r"\{\{(.*?)\}\}").unwrap();
    for (to_replace_str, [variable]) in re.captures_iter(template).map(|c| c.extract()) {
        let mut variables = variable.split("||").peekable();
        while let Some(variable) = variables.next() {
            let Some(value) = variable_value(variable, context) else {
                continue;
            };
            if value.is_empty() && variables.peek().is_some() {
                continue;
            }

            new_template = new_template.replace(to_replace_str, &value);
            break;
        }
    }

    new_template
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::context::create_mock_context;
    use crate::context::ContextDimension;
    use rio_backend::config::layout::Margin;
    use rio_backend::event::VoidListener;
    use rio_backend::event::WindowId;
    use rio_backend::sugarloaf::layout::TextDimensions;

    #[test]
    fn test_update_title() {
        let context_dimension = ContextDimension::build(
            1200.0,
            800.0,
            TextDimensions {
                scale: 2.,
                width: 18.,
                height: 9.,
            },
            rio_backend::sugarloaf::layout::CellMetrics {
                cell_width: 18,
                cell_height: 9,
                cell_baseline: 0,
                face_width: 18.0,
                face_height: 9.0,
                face_y: 0.0,
            },
            1.0,
            14.0,
            Margin::default(),
        );

        assert_eq!(context_dimension.columns, 64);
        assert_eq!(context_dimension.lines, 84);

        let rich_text_id = 0;
        let context = create_mock_context(
            VoidListener {},
            WindowId::from(0),
            rich_text_id,
            context_dimension,
        );
        assert_eq!(update_title("", &context), String::from(""));
        assert_eq!(update_title("{{columns}}", &context), String::from("64"));
        assert_eq!(update_title("{{COLUMNS}}", &context), String::from("64"));
        assert_eq!(update_title("{{ COLUMNS }}", &context), String::from("64"));
        assert_eq!(update_title("{{ columns }}", &context), String::from("64"));
        assert_eq!(
            update_title("hello {{ COLUMNS }} AbC", &context),
            String::from("hello 64 AbC")
        );
        assert_eq!(
            update_title("hello {{ Lines }} AbC", &context),
            String::from("hello 84 AbC")
        );
        assert_eq!(
            update_title("{{ columns }}x{{lines}}", &context),
            String::from("64x84")
        );

        assert_eq!(update_title("{{ title }}", &context), String::from(""));

        // #[cfg(unix)]
        // assert_eq!(
        //     update_title("{{path_absolute}}"), &context)
        //     String::from("")
        // );
    }

    #[test]
    fn test_update_title_with_logical_or() {
        let context_dimension = ContextDimension::build(
            1200.0,
            800.0,
            TextDimensions {
                scale: 2.,
                width: 18.,
                height: 9.,
            },
            rio_backend::sugarloaf::layout::CellMetrics {
                cell_width: 18,
                cell_height: 9,
                cell_baseline: 0,
                face_width: 18.0,
                face_height: 9.0,
                face_y: 0.0,
            },
            1.0,
            14.0,
            Margin::default(),
        );

        assert_eq!(context_dimension.columns, 64);
        assert_eq!(context_dimension.lines, 84);

        let rich_text_id = 0;
        let context = create_mock_context(
            VoidListener {},
            WindowId::from(0),
            rich_text_id,
            context_dimension,
        );
        assert_eq!(update_title("", &context), String::from(""));
        // Title always starts empty
        assert_eq!(update_title("{{title}}", &context), String::from(""));

        assert_eq!(
            update_title("{{ title || columns }}", &context),
            String::from("64")
        );

        assert_eq!(
            update_title("{{ program || columns }}", &context),
            String::from("64")
        );

        assert_eq!(
            update_title("{{ title || title }}", &context),
            String::from("")
        );

        // let's modify title to actually be something
        {
            let mut term = context.terminal.lock();
            term.title = "Something".to_string();
        };

        assert_eq!(
            update_title("{{ title || columns }}", &context),
            String::from("Something")
        );

        assert_eq!(
            update_title("{{ columns || title }}", &context),
            String::from("64")
        );

        // Use a path that can't plausibly be $HOME on any realistic system.
        // Sandboxed builds (e.g. Void's xbps-src) often set HOME=/tmp, so a
        // literal "/tmp" here would get collapsed to "~" and break the test.
        {
            let path = std::path::PathBuf::from("/rio-sandbox-test-dir");
            let mut term = context.terminal.lock();
            term.current_directory = Some(path);
        };

        assert_eq!(
            update_title("{{ absolute_path || title }}", &context),
            String::from("/rio-sandbox-test-dir"),
        );

        assert_eq!(
            update_title("{{ relative_path || title }}", &context),
            String::from("/rio-sandbox-test-dir"),
        );
    }

    #[test]
    fn test_update_title_without_cwd_does_not_deadlock() {
        // Fresh tabs have no CWD yet; resolving a path variable must not
        // re-lock the terminal while the first guard is still held
        // (parking_lot mutexes are not reentrant, so the event loop would
        // park forever and freeze the whole window).
        let context_dimension = ContextDimension::build(
            1200.0,
            800.0,
            TextDimensions {
                scale: 2.,
                width: 18.,
                height: 9.,
            },
            rio_backend::sugarloaf::layout::CellMetrics {
                cell_width: 18,
                cell_height: 9,
                cell_baseline: 0,
                face_width: 18.0,
                face_height: 9.0,
                face_y: 0.0,
            },
            1.0,
            14.0,
            Margin::default(),
        );
        let context =
            create_mock_context(VoidListener {}, WindowId::from(0), 0, context_dimension);
        assert!(context.terminal.lock().current_directory.is_none());

        // `Context` is not `Send`, so the update runs on this thread while a
        // watchdog fails the test run instead of hanging CI forever.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let watchdog_done = std::sync::Arc::clone(&done);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(10));
            if !watchdog_done.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!("title update deadlocked with unset CWD");
                std::process::exit(42);
            }
        });
        let title = update_title("{{ TITLE || RELATIVE_PATH }}", &context);
        done.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(title, String::from(""));
    }

    #[test]
    fn test_shorten_path() {
        // Use a path prefix that can't plausibly be $HOME to keep the test
        // deterministic in build sandboxes that set HOME=/tmp or similar.
        assert_eq!(
            shorten_path("/rio-sandbox-test-dir"),
            "/rio-sandbox-test-dir",
        );
        assert_eq!(
            shorten_path("/rio-sandbox-test-dir/sub"),
            "/rio-sandbox-test-dir/sub",
        );

        // Deep paths get truncated to last 3 components
        assert_eq!(shorten_path("/a/b/c/d/e"), "…/c/d/e");
        assert_eq!(shorten_path("/a/b/c/d"), "…/b/c/d");

        // 3 components stays as-is
        assert_eq!(shorten_path("/a/b/c"), "/a/b/c");
    }
}
