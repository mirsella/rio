use crate::context::Context;
use crate::context::ContextDimension;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

#[derive(PartialEq)]
pub struct ContextTitle {
    pub content: String,
}

impl Default for ContextTitle {
    fn default() -> Self {
        Self {
            content: String::from("~"),
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
// - `PROGRAM`: the command the pane spawned (e.g `fish`, `zsh`, `bash`)
// - `ABSOLUTE_PATH`: working directory via OSC 7 (e.g `/Users/rapha/Documents/a/rio`)
// - `RELATIVE_PATH`: OSC 7 directory, home-relative (e.g `~/Documents/a/rio` or `…/a/psone/starpsx`)
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

/// Terminal state a title update resolves variables against.
///
/// Captured once per [`update_title`] call so variable resolution is pure:
/// the terminal mutex is not reentrant, and locking it per variable let a
/// fallback re-lock it while a guard was still held, parking the event loop
/// forever on fresh tabs (no CWD reported yet).
struct TitleSnapshot {
    dimension: ContextDimension,
    title: String,
    program: Option<String>,
    current_directory: Option<PathBuf>,
}

impl TitleSnapshot {
    fn capture<T: rio_backend::event::EventListener>(context: &Context<T>) -> Self {
        let terminal = context.terminal.lock();
        TitleSnapshot {
            dimension: context.dimension,
            title: terminal.title.clone(),
            program: context.foreground_process_name(),
            current_directory: terminal.current_directory.clone(),
        }
    }
}

fn current_path(current_directory: Option<&PathBuf>) -> Option<String> {
    // Lossy conversion is the identity for valid UTF-8, so this covers both
    // cases without branching.
    current_directory.map(|directory| directory.to_string_lossy().into_owned())
}

fn variable_value(variable: &str, snapshot: &TitleSnapshot) -> Option<String> {
    match variable.trim().to_ascii_lowercase().as_str() {
        "columns" => Some(snapshot.dimension.columns.to_string()),
        "lines" => Some(snapshot.dimension.lines.to_string()),
        "title" => Some(snapshot.title.clone()),
        "program" => Some(snapshot.program.clone().unwrap_or_default()),
        "absolute_path" => {
            Some(current_path(snapshot.current_directory.as_ref()).unwrap_or_default())
        }
        "relative_path" => Some(
            current_path(snapshot.current_directory.as_ref())
                .map(|path| shorten_path(&path))
                .unwrap_or_default(),
        ),
        _ => None,
    }
}

#[inline]
/// Render the title template. Every variable is event-known, so this
/// NEVER inspects the foreground process: `{{ title }}` is OSC 0/2,
/// the path variables are OSC 7 (empty for shells without
/// integration), `{{ program }}` is the name of the command the pane
/// spawned, and columns/lines are the pane's own dimensions.
/// `prefetched_title` reuses the OSC title string the caller already
/// holds (a `Title` event carries it), so a `{{ title }}` render off
/// an event never locks the terminal; otherwise one lock fetches
/// title and cwd together.
pub fn update_title<T: rio_backend::event::EventListener>(
    template: &str,
    context: &Context<T>,
    prefetched_title: Option<&str>,
) -> String {
    if template.is_empty() {
        return template.to_string();
    }

    static VARIABLE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = VARIABLE.get_or_init(|| regex::Regex::new(r"\{\{(.*?)\}\}").unwrap());

    let snapshot = TitleSnapshot::capture(context);
    let mut new_template = template.to_owned();

    for (to_replace_str, [variable]) in re.captures_iter(template).map(|c| c.extract()) {
        let mut variables = variable.split("||").peekable();
        while let Some(variable) = variables.next() {
            let Some(value) = variable_value(variable, &snapshot) else {
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
    use rio_backend::config::layout::Margin;
    use rio_backend::event::VoidListener;
    use rio_backend::event::WindowId;
    use rio_backend::sugarloaf::layout::TextDimensions;

    fn test_dimension() -> ContextDimension {
        ContextDimension::build(
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
        )
    }

    #[test]
    fn test_update_title() {
        let context_dimension = test_dimension();

        assert_eq!(context_dimension.columns, 64);
        assert_eq!(context_dimension.lines, 84);

        let rich_text_id = 0;
        let context = create_mock_context(
            VoidListener {},
            WindowId::from(0),
            rich_text_id,
            context_dimension,
        );
        assert_eq!(update_title("", &context, None), String::from(""));
        assert_eq!(
            update_title("{{columns}}", &context, None),
            String::from("64")
        );
        assert_eq!(
            update_title("{{COLUMNS}}", &context, None),
            String::from("64")
        );
        assert_eq!(
            update_title("{{ COLUMNS }}", &context, None),
            String::from("64")
        );
        assert_eq!(
            update_title("{{ columns }}", &context, None),
            String::from("64")
        );
        assert_eq!(
            update_title("hello {{ COLUMNS }} AbC", &context, None),
            String::from("hello 64 AbC")
        );
        assert_eq!(
            update_title("hello {{ Lines }} AbC", &context, None),
            String::from("hello 84 AbC")
        );
        assert_eq!(
            update_title("{{ columns }}x{{lines}}", &context, None),
            String::from("64x84")
        );

        assert_eq!(update_title("{{ title }}", &context), String::from(""));
    }

    #[test]
    fn test_update_title_with_logical_or() {
        let context_dimension = test_dimension();

        assert_eq!(context_dimension.columns, 64);
        assert_eq!(context_dimension.lines, 84);

        let rich_text_id = 0;
        let context = create_mock_context(
            VoidListener {},
            WindowId::from(0),
            rich_text_id,
            context_dimension,
        );
        assert_eq!(update_title("", &context, None), String::from(""));
        // Title always starts empty
        assert_eq!(update_title("{{title}}", &context, None), String::from(""));

        assert_eq!(
            update_title("{{ title || columns }}", &context, None),
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
            update_title("{{ title || columns }}", &context, None),
            String::from("Something")
        );

        assert_eq!(
            update_title("{{ columns || title }}", &context, None),
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
            update_title("{{ absolute_path || title }}", &context, None),
            String::from("/rio-sandbox-test-dir"),
        );

        assert_eq!(
            update_title("{{ relative_path || title }}", &context, None),
            String::from("/rio-sandbox-test-dir"),
        );
    }

    #[test]
    fn test_update_title_without_cwd_resolves_to_empty() {
        // Fresh tabs have no CWD yet; resolving a path variable against the
        // default template must return promptly instead of re-locking the
        // terminal (which used to park the event loop forever and freeze
        // the whole window).
        let context =
            create_mock_context(VoidListener {}, WindowId::from(0), 0, test_dimension());
        assert!(context.terminal.lock().current_directory.is_none());
        assert_eq!(
            update_title("{{ TITLE || RELATIVE_PATH }}", &context),
            String::from("")
        );
    }

    #[test]
    fn test_current_path_without_directory() {
        assert_eq!(current_path(None), None);
    }

    #[test]
    fn test_current_path_prefers_valid_unicode() {
        let directory = PathBuf::from("/tmp/rio-title-test");
        assert_eq!(
            current_path(Some(&directory)),
            Some(String::from("/tmp/rio-title-test"))
        );
    }

    #[test]
    #[cfg(unix)]
    fn test_current_path_falls_back_to_lossy() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let directory = PathBuf::from(OsStr::from_bytes(b"/tmp/rio-\xff"));
        assert_eq!(
            current_path(Some(&directory)),
            Some(String::from("/tmp/rio-\u{fffd}"))
        );
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
