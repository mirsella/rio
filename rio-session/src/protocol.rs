use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;

pub const PROTOCOL_VERSION: u16 = 5;
pub const MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;
pub const MAX_ARGUMENTS: usize = 256;
pub const MAX_ENVIRONMENT: usize = 4096;
pub const MAX_STRING_BYTES: usize = 1024 * 1024;
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PENDING_REQUESTS: usize = 64;
pub const MAX_COLUMNS: u16 = 1024;
pub const MAX_LINES: u16 = 1024;
// Keep the worst-case cell payload below the 16 MiB transport limit, leaving
// room for styles, text, metadata, and graphics.
pub const MAX_FRAME_CELLS: usize = 262_144;
pub const MAX_GRAPHICS_ITEMS: usize = 4096;
pub const MAX_FRAME_GRAPHICS_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_GRAPHIC_DIMENSION: u32 = 10_000;
pub const MAX_WHEEL_LINES: i32 = 1024;
pub const MAX_SELECTION_LINES: i32 = 1024;
pub const MAX_PENDING_INPUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_SCROLLBACK: usize = 10_000_000;

#[cfg(unix)]
fn recovery_directory() -> Result<PathBuf, crate::SessionError> {
    use std::os::unix::fs::DirBuilderExt;

    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let directory = base.join("rio-sessions");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700).create(&directory)?;
    crate::private_directory_identity(&directory)?;
    Ok(directory)
}

#[cfg(not(unix))]
fn recovery_directory() -> Result<PathBuf, crate::SessionError> {
    Err(crate::SessionError::unsupported(
        "session recovery is unsupported on this platform",
    ))
}

fn recovery_path(session_id: SessionId) -> Result<PathBuf, crate::SessionError> {
    Ok(recovery_directory()?.join(format!("{}.session", session_id.hex())))
}

#[cfg(unix)]
pub(crate) fn remove_file_if_open_file(
    path: &std::path::Path,
    original: &std::fs::File,
) -> std::io::Result<bool> {
    use std::os::unix::fs::MetadataExt;

    let original_metadata = original.metadata()?;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if (metadata.dev(), metadata.ino())
        != (original_metadata.dev(), original_metadata.ino())
    {
        return Ok(false);
    }
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SessionId(pub [u8; 16]);

impl SessionId {
    pub fn random() -> Result<Self, crate::SessionError> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes).map_err(crate::SessionError::random)?;
        Ok(Self(bytes))
    }

    pub fn hex(self) -> String {
        hex_bytes(&self.0)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct EnvVar {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
}

impl EnvVar {
    pub fn new(key: impl Into<Vec<u8>>, value: impl Into<Vec<u8>>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }

    fn validate(&self) -> Result<(), crate::SessionError> {
        if self.key.is_empty()
            || self.key.contains(&0)
            || self.key.contains(&b'=')
            || self.value.contains(&0)
        {
            return Err(crate::SessionError::invalid(
                "environment keys cannot be empty, contain NUL or '=', and values cannot contain NUL",
            ));
        }
        std::str::from_utf8(&self.key).map_err(|_| Self::non_utf8())?;
        std::str::from_utf8(&self.value).map_err(|_| Self::non_utf8())?;
        Ok(())
    }

    fn non_utf8() -> crate::SessionError {
        crate::SessionError::unsupported(
            "non-UTF-8 environment is not supported by this PTY backend",
        )
    }

    /// The caller validates the containing session spec before moving these
    /// bytes into the PTY environment.
    #[cfg(unix)]
    pub(crate) fn into_utf8(self) -> Result<(String, String), crate::SessionError> {
        let key = String::from_utf8(self.key).map_err(|_| Self::non_utf8())?;
        let value = String::from_utf8(self.value).map_err(|_| Self::non_utf8())?;
        Ok((key, value))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SessionSpec {
    pub shell: Option<String>,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub environment: Vec<EnvVar>,
    pub columns: u16,
    pub lines: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
    pub scrollback: usize,
    pub grapheme_clustering: bool,
}

impl Default for SessionSpec {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            working_dir: None,
            environment: Vec::new(),
            columns: 80,
            lines: 24,
            pixel_width: 720,
            pixel_height: 432,
            scrollback: 10_000,
            grapheme_clustering: true,
        }
    }
}

impl SessionSpec {
    pub fn from_current_environment() -> Result<Self, crate::SessionError> {
        #[cfg(not(unix))]
        {
            return Err(crate::SessionError::unsupported(
                "raw environment capture is currently supported only on Unix",
            ));
        }

        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;

            let mut spec = Self::default();
            for (key, value) in std::env::vars_os() {
                let (key, value) = (key.as_bytes().to_vec(), value.as_bytes().to_vec());
                spec.environment.push(EnvVar::new(key, value));
            }
            spec.validate()?;
            Ok(spec)
        }
    }

    pub fn validate(&self) -> Result<(), crate::SessionError> {
        if self.args.len() > MAX_ARGUMENTS {
            return Err(crate::SessionError::invalid("too many shell arguments"));
        }
        if self.environment.len() > MAX_ENVIRONMENT {
            return Err(crate::SessionError::invalid("too many environment entries"));
        }
        validate_dimensions(self.columns, self.lines)?;
        validate_pixel_dimensions(self.pixel_width, self.pixel_height)?;
        if self.scrollback > MAX_SCROLLBACK {
            return Err(crate::SessionError::invalid(
                "scrollback limit is too large",
            ));
        }
        for value in self.shell.iter().chain(self.working_dir.iter()) {
            validate_string(value)?;
        }
        for value in &self.args {
            validate_string(value)?;
        }
        let mut keys = HashSet::with_capacity(self.environment.len());
        for env in &self.environment {
            if env.key.len() > MAX_STRING_BYTES || env.value.len() > MAX_STRING_BYTES {
                return Err(crate::SessionError::invalid(
                    "environment entry is too large",
                ));
            }
            env.validate()?;
            if !keys.insert(&env.key) {
                return Err(crate::SessionError::invalid(
                    "environment contains duplicate keys",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SessionDescriptor {
    pub endpoint: PathBuf,
    pub capability: [u8; 32],
    pub session_id: SessionId,
}

impl fmt::Debug for SessionDescriptor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionDescriptor")
            .field("endpoint", &self.endpoint)
            .field("capability", &"<redacted>")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl SessionDescriptor {
    pub fn validate(&self) -> Result<(), crate::SessionError> {
        if !self.endpoint.is_absolute() {
            return Err(crate::SessionError::invalid(
                "session endpoint must be an absolute path",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            let endpoint = self.endpoint.as_os_str().as_bytes();
            if endpoint.len() > MAX_STRING_BYTES || endpoint.contains(&0) {
                return Err(crate::SessionError::invalid(
                    "session endpoint is too large or contains NUL",
                ));
            }
        }
        if self.capability.iter().all(|byte| *byte == 0) {
            return Err(crate::SessionError::invalid("session capability is empty"));
        }
        #[cfg(unix)]
        {
            let parent = self.endpoint.parent().ok_or_else(|| {
                crate::SessionError::invalid("session endpoint has no parent")
            })?;
            crate::private_directory_identity(parent)?;
        }
        Ok(())
    }

    pub fn save(&self, path: &std::path::Path) -> Result<(), crate::SessionError> {
        self.validate()?;
        #[cfg(unix)]
        {
            self.save_file_with_open_file(path)?;
        }
        #[cfg(not(unix))]
        {
            let bytes = crate::codec::encode(self)?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            use std::io::Write;
            let mut file = options.open(path)?;
            if let Err(error) = file.write_all(&bytes) {
                drop(file);
                let _ = std::fs::remove_file(path);
                return Err(error.into());
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn save_file_with_open_file(
        &self,
        path: &std::path::Path,
    ) -> Result<std::fs::File, crate::SessionError> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let bytes = crate::codec::encode(self)?;
        let mut options = std::fs::OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        if let Err(error) = file.write_all(&bytes) {
            let _ = remove_file_if_open_file(path, &file);
            return Err(error.into());
        }
        Ok(file)
    }

    pub fn load(path: &std::path::Path) -> Result<Self, crate::SessionError> {
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        #[cfg(unix)]
        if !crate::is_private_file(&metadata) {
            return Err(crate::SessionError::protocol(
                "session descriptor is not private",
            ));
        }
        if metadata.len() > MAX_FRAME_SIZE as u64 {
            return Err(crate::SessionError::protocol(
                "session descriptor is too large",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_FRAME_SIZE + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_FRAME_SIZE {
            return Err(crate::SessionError::protocol(
                "session descriptor is too large",
            ));
        }
        let descriptor: Self = crate::codec::decode(&bytes)?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Persist this descriptor in the private per-user recovery registry.
    ///
    /// The file contains the capability, so it is protected like the session
    /// endpoint itself. It intentionally remains after an attachment drops;
    /// explicit `SessionClient::close` removes it.
    #[cfg(unix)]
    pub fn save_recovery(&self) -> Result<PathBuf, crate::SessionError> {
        self.save_recovery_with_file().map(|(path, _)| path)
    }

    #[cfg(not(unix))]
    pub fn save_recovery(&self) -> Result<PathBuf, crate::SessionError> {
        self.validate()?;
        let directory = recovery_directory()?;
        let path = directory.join(format!("{}.session", self.session_id.hex()));
        self.save(&path)?;
        Ok(path)
    }

    #[cfg(unix)]
    pub(crate) fn save_recovery_with_file(
        &self,
    ) -> Result<(PathBuf, std::fs::File), crate::SessionError> {
        self.validate()?;
        let directory = recovery_directory()?;
        let path = directory.join(format!("{}.session", self.session_id.hex()));
        let file = self.save_file_with_open_file(&path)?;
        Ok((path, file))
    }

    #[cfg(unix)]
    pub(crate) fn open_recovery_file(&self) -> Option<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;

        let path = recovery_path(self.session_id).ok()?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).custom_flags(libc::O_NOFOLLOW);
        let file = options.open(path).ok()?;
        let metadata = file.metadata().ok()?;
        if !crate::is_private_file(&metadata) {
            return None;
        }
        Some(file)
    }

    #[cfg(unix)]
    pub(crate) fn remove_recovery_if_file(&self, file: &std::fs::File) {
        if let Ok(path) = recovery_path(self.session_id) {
            let _ = remove_file_if_open_file(&path, file);
        }
    }

    pub fn remove_recovery(&self) {
        #[cfg(unix)]
        if let Some(file) = self.open_recovery_file() {
            self.remove_recovery_if_file(&file);
        }
        #[cfg(not(unix))]
        let _ = self;
    }

    pub fn discover_recovery() -> Result<Vec<Self>, crate::SessionError> {
        let directory = recovery_directory()?;
        let entries = std::fs::read_dir(&directory)?;
        let mut descriptors = Vec::new();
        for entry in entries.flatten().take(MAX_PENDING_REQUESTS) {
            if !entry.file_name().to_string_lossy().ends_with(".session") {
                continue;
            }
            if let Ok(descriptor) = Self::load(&entry.path()) {
                if descriptor.endpoint.exists() {
                    descriptors.push(descriptor);
                }
            }
        }
        Ok(descriptors)
    }
}

#[derive(Clone, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum ClientMessage {
    Hello {
        version: u16,
        capability: [u8; 32],
        session_id: SessionId,
        spec: Option<SessionSpec>,
    },
    Claim {
        generation: u64,
    },
    Commit {
        generation: u64,
    },
    Command {
        generation: u64,
        request_id: u64,
        command: SessionCommand,
    },
}

impl fmt::Debug for ClientMessage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Hello {
                version,
                session_id,
                spec,
                ..
            } => formatter
                .debug_struct("Hello")
                .field("version", version)
                .field("session_id", session_id)
                .field("spec", &spec.as_ref().map(|_| "<redacted>"))
                .field("capability", &"<redacted>")
                .finish(),
            Self::Claim { generation } => formatter
                .debug_struct("Claim")
                .field("generation", generation)
                .finish(),
            Self::Commit { generation } => formatter
                .debug_struct("Commit")
                .field("generation", generation)
                .finish(),
            Self::Command {
                generation,
                request_id,
                ..
            } => formatter
                .debug_struct("Command")
                .field("generation", generation)
                .field("request_id", request_id)
                .field("command", &"<redacted>")
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum KeyAction {
    Press,
    Repeat,
    Release,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum KeyCode {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    Function(u8),
    CapsLock,
    ShiftLeft,
    ShiftRight,
    ControlLeft,
    ControlRight,
    AltLeft,
    AltRight,
    SuperLeft,
    SuperRight,
}

/// A terminal vi-mode motion.  This is deliberately a semantic wire type;
/// the worker maps it to rio-vt's authoritative motion implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum ViMotion {
    Up,
    Down,
    Left,
    Right,
    First,
    Last,
    FirstOccupied,
    High,
    Middle,
    Low,
    SemanticLeft,
    SemanticRight,
    SemanticLeftEnd,
    SemanticRightEnd,
    WordLeft,
    WordRight,
    WordLeftEnd,
    WordRightEnd,
    Bracket,
    ParagraphUp,
    ParagraphDown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SearchDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchOrigin {
    pub line: i32,
    pub column: u16,
    pub display_offset: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum RequestKind {
    ClipboardLoad,
    ColorRequest,
    TextAreaSizeRequest,
    GlyphProtocolQuery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum RequestRefusalReason {
    Capacity,
    Unsupported,
    PayloadTooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum GlyphStatus {
    Free,
    System,
    Glossary,
    Both,
}

#[derive(Clone, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct KeyInput {
    pub action: KeyAction,
    pub key: Option<KeyCode>,
    pub modifiers: u8,
    pub consumed_modifiers: u8,
    pub text: Option<String>,
    pub composing: bool,
}

impl KeyInput {
    fn validate(&self) -> Result<(), crate::SessionError> {
        validate_modifiers(self.modifiers)?;
        validate_modifiers(self.consumed_modifiers)?;
        if let Some(text) = &self.text {
            validate_string(text)?;
        }
        if matches!(self.key, Some(KeyCode::Function(0))) {
            return Err(crate::SessionError::invalid(
                "function key number must be non-zero",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, bincode::Encode, bincode::Decode)]
pub enum ServerMessage {
    Ready {
        version: u16,
        session_id: SessionId,
        generation: u64,
    },
    Offer {
        generation: u64,
    },
    Claimed {
        generation: u64,
    },
    Reply {
        request_id: u64,
        reply: SessionReply,
    },
    Initial {
        generation: u64,
        frame: FullFrame,
    },
    Event {
        generation: u64,
        event: SessionEvent,
    },
    Detached,
    Error {
        code: ErrorCode,
        message: String,
    },
}

#[cfg(unix)]
impl ClientMessage {
    pub(crate) fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Hello { spec, .. } => {
                if let Some(spec) = spec {
                    spec.validate()?;
                }
            }
            Self::Claim { generation } | Self::Commit { generation } => {
                if *generation == 0 {
                    return Err(crate::SessionError::protocol(
                        "attachment generation must be non-zero",
                    ));
                }
            }
            Self::Command {
                generation,
                request_id,
                command,
            } => {
                if *generation == 0 || *request_id == 0 {
                    return Err(crate::SessionError::protocol(
                        "command generation and request id must be non-zero",
                    ));
                }
                command.validate()?;
            }
        }
        Ok(())
    }
}

#[cfg(unix)]
impl ServerMessage {
    pub(crate) fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Ready {
                generation,
                version,
                ..
            } => {
                if *generation == 0 || *version != PROTOCOL_VERSION {
                    return Err(crate::SessionError::protocol(
                        "invalid worker ready message",
                    ));
                }
            }
            Self::Offer { generation }
            | Self::Claimed { generation }
            | Self::Initial { generation, .. }
            | Self::Event { generation, .. } => {
                if *generation == 0 {
                    return Err(crate::SessionError::protocol(
                        "attachment generation must be non-zero",
                    ));
                }
                match self {
                    Self::Initial { frame, .. } => frame.validate()?,
                    Self::Event { event, .. } => event.validate()?,
                    _ => {}
                }
            }
            Self::Reply { request_id, reply } => {
                if *request_id == 0 {
                    return Err(crate::SessionError::protocol(
                        "reply request id must be non-zero",
                    ));
                }
                reply.validate()?;
            }
            Self::Error { message, .. } => validate_string(message)?,
            Self::Detached => {}
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum ErrorCode {
    BadProtocol,
    BadAuth,
    BadRequest,
    Busy,
    StaleGeneration,
    Unsupported,
    Internal,
}

#[derive(Debug, Clone, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SessionCommand {
    Write(Vec<u8>),
    Paste(String),
    Key(KeyInput),
    /// Report a host focus transition to applications that enabled DEC mode
    /// 1004.  The worker decides whether a report is enabled.
    Focus {
        focused: bool,
    },
    Resize {
        columns: u16,
        lines: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    Scroll {
        delta_lines: i32,
    },
    MouseWheel {
        lines: i32,
        column: u16,
        line: u16,
        modifiers: u8,
    },
    MouseButton {
        column: u16,
        line: u16,
        button: u8,
        pressed: bool,
        modifiers: u8,
    },
    MouseMotion {
        column: u16,
        line: u16,
        button: u8,
        modifiers: u8,
    },
    SelectionBegin {
        line: i32,
        column: usize,
        kind: SelectionKind,
        side: SelectionSide,
    },
    SelectionUpdate {
        line: i32,
        column: usize,
        side: SelectionSide,
    },
    SelectionClear,
    SelectAll,
    /// Scroll and update the active selection endpoint while the worker owns
    /// the terminal lock. `line` and `column` are viewport coordinates.
    SelectionAutoScroll {
        delta_lines: i32,
        line: i32,
        column: usize,
        side: SelectionSide,
    },
    SelectionText,
    Search {
        pattern: String,
        max_matches: usize,
    },
    SearchBegin {
        pattern: String,
        origin_line: i32,
        origin_column: u16,
        origin_display_offset: u32,
        direction: SearchDirection,
        side: SelectionSide,
        max_lines: Option<u32>,
    },
    SearchNext,
    SearchCancel,
    SetViMode(bool),
    ToggleViMode,
    ViMotion(ViMotion),
    ViScroll {
        delta_lines: i32,
    },
    ViGoto {
        line: i32,
        column: u16,
    },
    ScrollToPrompt {
        forward: bool,
    },
    ScrollTop,
    ScrollBottom,
    ClearSavedHistory,
    ChildPid,
    Snapshot,
    SnapshotSince {
        base_sequence: u64,
    },
    SetAltIsMeta(bool),
    SetCursorStyle {
        shape: u8,
        blinking: bool,
    },
    ClipboardResponse {
        request_id: u64,
        route_id: u64,
        text: String,
    },
    ColorResponse {
        request_id: u64,
        route_id: u64,
        color: Option<[u8; 3]>,
    },
    TextAreaSizeResponse {
        request_id: u64,
        route_id: u64,
        rows: u16,
        columns: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
    GlyphProtocolResponse {
        request_id: u64,
        route_id: u64,
        status: GlyphStatus,
    },
    Close,
}

impl SessionCommand {
    pub fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Write(bytes) => {
                if bytes.len() > MAX_STRING_BYTES {
                    return Err(crate::SessionError::invalid("input is too large"));
                }
            }
            Self::Paste(text) | Self::Search { pattern: text, .. } => {
                validate_string(text)?
            }
            Self::SearchBegin {
                pattern,
                origin_line,
                origin_column,
                origin_display_offset,
                max_lines,
                ..
            } => {
                validate_string(pattern)?;
                validate_position(*origin_line, *origin_column as usize)?;
                if *origin_display_offset > MAX_SCROLLBACK as u32 {
                    return Err(crate::SessionError::invalid(
                        "search origin display offset exceeds the session limit",
                    ));
                }
                if max_lines.is_some_and(|lines| lines > MAX_SCROLLBACK as u32) {
                    return Err(crate::SessionError::invalid(
                        "search line limit exceeds the session limit",
                    ));
                }
            }
            Self::Key(input) => input.validate()?,
            Self::Resize {
                columns,
                lines,
                pixel_width,
                pixel_height,
            } => {
                validate_dimensions(*columns, *lines)?;
                validate_pixel_dimensions(*pixel_width, *pixel_height)?;
            }
            Self::Scroll { delta_lines } => validate_scroll(*delta_lines)?,
            Self::SelectionAutoScroll {
                delta_lines,
                line,
                column,
                ..
            } => {
                validate_scroll(*delta_lines)?;
                validate_position(*line, *column)?;
            }
            Self::ViScroll { delta_lines } => validate_scroll(*delta_lines)?,
            Self::ViGoto { line, column } => validate_position(*line, *column as usize)?,
            Self::MouseWheel {
                lines, modifiers, ..
            } => {
                validate_scroll(*lines)?;
                validate_modifiers(*modifiers)?;
            }
            Self::MouseButton {
                button, modifiers, ..
            } => {
                if *button > 2 {
                    return Err(crate::SessionError::invalid(
                        "mouse button is outside the supported range",
                    ));
                }
                validate_modifiers(*modifiers)?;
            }
            Self::MouseMotion {
                button, modifiers, ..
            } => {
                if *button > 3 {
                    return Err(crate::SessionError::invalid(
                        "mouse motion button is outside the supported range",
                    ));
                }
                validate_modifiers(*modifiers)?;
            }
            Self::SelectionBegin { line, column, .. }
            | Self::SelectionUpdate { line, column, .. } => {
                validate_position(*line, *column)?;
            }
            Self::SelectionClear
            | Self::SelectAll
            | Self::SelectionText
            | Self::SearchNext
            | Self::SearchCancel
            | Self::ToggleViMode
            | Self::ScrollTop
            | Self::ScrollBottom
            | Self::ClearSavedHistory
            | Self::ChildPid
            | Self::Snapshot
            | Self::SetAltIsMeta(_)
            | Self::Close => {}
            Self::SnapshotSince { base_sequence } => {
                if *base_sequence == 0 {
                    return Err(crate::SessionError::invalid(
                        "snapshot base sequence must be non-zero",
                    ));
                }
            }
            Self::Focus { .. } | Self::SetViMode(_) | Self::ViMotion(_) => {}
            Self::ScrollToPrompt { .. } => {}
            Self::SetCursorStyle { shape, .. } => {
                if *shape > 3 {
                    return Err(crate::SessionError::invalid("unsupported cursor shape"));
                }
            }
            Self::ClipboardResponse {
                request_id,
                route_id,
                text,
            } => {
                if *request_id == 0 || *route_id == 0 {
                    return Err(crate::SessionError::invalid(
                        "clipboard response ids must be non-zero",
                    ));
                }
                validate_string(text)?;
            }
            Self::ColorResponse {
                request_id,
                route_id,
                ..
            }
            | Self::GlyphProtocolResponse {
                request_id,
                route_id,
                ..
            } => {
                if *request_id == 0 || *route_id == 0 {
                    return Err(crate::SessionError::invalid(
                        "terminal response ids must be non-zero",
                    ));
                }
            }
            Self::TextAreaSizeResponse {
                request_id,
                route_id,
                rows,
                columns,
                pixel_width,
                pixel_height,
            } => {
                if *request_id == 0 || *route_id == 0 {
                    return Err(crate::SessionError::invalid(
                        "terminal response ids must be non-zero",
                    ));
                }
                validate_dimensions(*columns, *rows)?;
                validate_pixel_dimensions(*pixel_width, *pixel_height)?;
            }
        }
        if let Self::Search { max_matches, .. } = self {
            if *max_matches > MAX_ARGUMENTS * 1024 {
                return Err(crate::SessionError::invalid("too many search matches"));
            }
        }
        Ok(())
    }
}

fn validate_position(line: i32, column: usize) -> Result<(), crate::SessionError> {
    if line.unsigned_abs() > MAX_SCROLLBACK as u32 || column > MAX_COLUMNS as usize {
        return Err(crate::SessionError::invalid(
            "terminal position is outside the supported range",
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SelectionKind {
    Simple,
    Word,
    Line,
    Block,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SelectionSide {
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SessionReply {
    Accepted,
    NoChange,
    Frame(FullFrame),
    FrameUpdate(FrameUpdate),
    SelectionText(Option<String>),
    SearchMatches(Vec<SearchMatch>),
    SearchNavigation(SearchNavigation),
    ChildPid(u32),
    Closed,
}

#[cfg(unix)]
impl SessionReply {
    pub(crate) fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Frame(frame) => frame.validate()?,
            Self::FrameUpdate(update) => update.validate()?,
            Self::SelectionText(text) => {
                if let Some(text) = text {
                    validate_string(text)?;
                }
            }
            Self::SearchMatches(matches) => validate_search_matches(matches)?,
            Self::SearchNavigation(navigation) => navigation.validate()?,
            Self::Accepted | Self::NoChange | Self::ChildPid(_) | Self::Closed => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub enum SessionEvent {
    FrameReady,
    Title {
        title: String,
    },
    Bell,
    CursorBlinkingChanged,
    Progress {
        state: u8,
        value: u8,
    },
    ClipboardStore {
        kind: u8,
        text: String,
    },
    ChildExited {
        status: Option<i32>,
    },
    ClipboardOverflow,
    ClipboardLoad {
        request_id: u64,
        route_id: u64,
        kind: u8,
    },
    ColorRequest {
        request_id: u64,
        route_id: u64,
        index: u16,
    },
    TextAreaSizeRequest {
        request_id: u64,
        route_id: u64,
    },
    GlyphProtocolQuery {
        request_id: u64,
        route_id: u64,
        codepoint: u32,
    },
    DesktopNotification {
        title: String,
        body: String,
    },
    ColorChange {
        route_id: u64,
        index: u16,
        color: Option<[u8; 3]>,
    },
    RequestRefused {
        request_id: u64,
        kind: RequestKind,
        reason: RequestRefusalReason,
    },
    RequestExpired {
        request_id: u64,
        kind: RequestKind,
    },
    Closed,
}

#[cfg(unix)]
impl SessionEvent {
    pub(crate) fn is_critical(&self) -> bool {
        matches!(
            self,
            Self::ChildExited { .. }
                | Self::ClipboardOverflow
                | Self::ClipboardLoad { .. }
                | Self::ColorRequest { .. }
                | Self::TextAreaSizeRequest { .. }
                | Self::GlyphProtocolQuery { .. }
                | Self::ColorChange { .. }
                | Self::RequestRefused { .. }
                | Self::RequestExpired { .. }
                | Self::DesktopNotification { .. }
                | Self::Closed
        )
    }

    pub(crate) fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Title { title } => validate_string(title)?,
            Self::ClipboardStore { kind, text } => {
                if *kind > 1 {
                    return Err(crate::SessionError::protocol(
                        "unsupported clipboard kind",
                    ));
                }
                validate_string(text)?;
            }
            Self::ClipboardLoad {
                request_id,
                route_id,
                kind,
            } => {
                if *request_id == 0 || *route_id == 0 || *kind > 1 {
                    return Err(crate::SessionError::protocol(
                        "invalid clipboard request",
                    ));
                }
            }
            Self::ColorRequest {
                request_id,
                route_id,
                index,
            } => {
                if *request_id == 0 || *route_id == 0 || *index >= 269 {
                    return Err(crate::SessionError::protocol("invalid color request"));
                }
            }
            Self::TextAreaSizeRequest {
                request_id,
                route_id,
            } => {
                if *request_id == 0 || *route_id == 0 {
                    return Err(crate::SessionError::protocol(
                        "terminal request id must be non-zero",
                    ));
                }
            }
            Self::GlyphProtocolQuery {
                request_id,
                route_id,
                codepoint,
                ..
            } => {
                if *request_id == 0
                    || *route_id == 0
                    || char::from_u32(*codepoint).is_none()
                {
                    return Err(crate::SessionError::protocol("invalid glyph query"));
                }
            }
            Self::DesktopNotification { title, body } => {
                validate_string(title)?;
                validate_string(body)?;
            }
            Self::ColorChange {
                route_id, index, ..
            } => {
                if *route_id == 0 || *index >= 269 {
                    return Err(crate::SessionError::protocol(
                        "color index is outside the terminal palette",
                    ));
                }
            }
            Self::RequestRefused { request_id, .. } => {
                if *request_id == 0 {
                    return Err(crate::SessionError::protocol(
                        "refused request id must be non-zero",
                    ));
                }
            }
            Self::RequestExpired { request_id, .. } => {
                if *request_id == 0 {
                    return Err(crate::SessionError::protocol(
                        "expired request id must be non-zero",
                    ));
                }
            }
            Self::Progress { state, value } => {
                if *state > 4 || *value > 100 {
                    return Err(crate::SessionError::protocol(
                        "progress values are outside the supported range",
                    ));
                }
            }
            Self::FrameReady
            | Self::Bell
            | Self::CursorBlinkingChanged
            | Self::ChildExited { .. }
            | Self::ClipboardOverflow
            | Self::Closed => {}
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SearchMatch {
    pub start_line: u32,
    pub start_column: u16,
    pub end_line: u32,
    pub end_column: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SearchNavigation {
    pub matched: Option<SearchMatch>,
    pub display_offset: u32,
    pub vi_mode: bool,
}

#[cfg(unix)]
impl SearchNavigation {
    fn validate(&self) -> Result<(), crate::SessionError> {
        if self.display_offset > MAX_SCROLLBACK as u32 {
            return Err(crate::SessionError::protocol(
                "search display offset exceeds the session limit",
            ));
        }
        if let Some(search_match) = &self.matched {
            validate_search_match(search_match)?;
        }
        Ok(())
    }
}

#[cfg(unix)]
fn validate_search_match(search_match: &SearchMatch) -> Result<(), crate::SessionError> {
    let max_line = MAX_SCROLLBACK
        .checked_add(usize::from(MAX_LINES))
        .ok_or_else(|| crate::SessionError::protocol("search line limit overflows"))?;
    if usize::try_from(search_match.start_line).unwrap_or(usize::MAX) > max_line
        || usize::try_from(search_match.end_line).unwrap_or(usize::MAX) > max_line
        || usize::from(search_match.start_column) > MAX_COLUMNS as usize
        || usize::from(search_match.end_column) > MAX_COLUMNS as usize
    {
        return Err(crate::SessionError::protocol(
            "search match is outside the session dimensions",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_search_matches(matches: &[SearchMatch]) -> Result<(), crate::SessionError> {
    if matches.len() > MAX_ARGUMENTS * 1024 {
        return Err(crate::SessionError::protocol("too many search matches"));
    }
    matches.iter().try_for_each(validate_search_match)
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct FullFrame {
    pub sequence: u64,
    pub columns: u16,
    pub lines: u16,
    pub rows: Vec<RowFrame>,
    pub display_offset: u32,
    pub history_size: u32,
    pub lines_evicted: u64,
    pub alternate_screen: bool,
    pub modes: u32,
    pub cursor: CursorFrame,
    pub selection: Option<SelectionFrame>,
    pub colors: Vec<Option<[f32; 4]>>,
    pub graphics: GraphicsFrame,
    pub title: String,
    pub working_dir: Option<String>,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct RowFrame {
    /// Stable semantic cell values.  The packed `rio-vt::Square` layout is
    /// intentionally not part of the session protocol.
    pub cells: Vec<CellFrame>,
    pub styles: Vec<StyleFrame>,
    pub extras: Vec<Option<ExtrasFrame>>,
    pub kitty_virtual_placeholder: bool,
    pub text: String,
}

/// A complete frame or a bounded update against the one frame most recently
/// published by the worker for the active attachment.
#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub enum FrameUpdate {
    Full(FullFrame),
    Delta(FrameDelta),
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct FrameDelta {
    pub base_sequence: u64,
    pub sequence: u64,
    pub columns: u16,
    pub lines: u16,
    pub rows: Vec<RowUpdate>,
    pub display_offset: u32,
    pub history_size: u32,
    pub lines_evicted: u64,
    pub alternate_screen: bool,
    pub modes: u32,
    pub cursor: CursorFrame,
    pub selection: Option<SelectionFrame>,
    pub colors: Vec<Option<[f32; 4]>>,
    pub title: String,
    pub working_dir: Option<String>,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct RowUpdate {
    pub line: u16,
    pub row: RowFrame,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct CellFrame {
    pub content: CellContentFrame,
    pub wide: u8,
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub enum CellContentFrame {
    Codepoint(u32),
    Palette(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

impl CellFrame {
    fn validate(&self) -> Result<(), crate::SessionError> {
        if self.wide > 3 || self.flags & !0x0f != 0 {
            return Err(crate::SessionError::protocol("invalid semantic cell flags"));
        }
        if let CellContentFrame::Codepoint(codepoint) = self.content {
            if char::from_u32(codepoint).is_none() {
                return Err(crate::SessionError::protocol(
                    "cell codepoint is not a Unicode scalar",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct ExtrasFrame {
    pub zero_width: Vec<u32>,
    pub hyperlink: Option<HyperlinkFrame>,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct HyperlinkFrame {
    pub id: String,
    pub uri: String,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct StyleFrame {
    pub foreground: ColorFrame,
    pub background: ColorFrame,
    pub underline: Option<ColorFrame>,
    pub flags: u16,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub enum ColorFrame {
    Named(u16),
    Indexed(u8),
    Rgb { r: u8, g: u8, b: u8 },
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct CursorFrame {
    pub line: u16,
    pub column: u16,
    pub visible: bool,
    /// Whether the terminal requested cursor blinking. `visible` remains the
    /// effective visibility for the current viewport and cursor shape.
    pub blinking: bool,
    pub shape: u8,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct SelectionFrame {
    pub start_line: u16,
    pub start_column: u16,
    pub end_line: u16,
    pub end_column: u16,
    pub block: bool,
}

#[derive(Clone, Debug, Default, PartialEq, bincode::Encode, bincode::Decode)]
pub struct GraphicsFrame {
    pub images: Vec<GraphicFrame>,
    pub kitty_placements: Vec<KittyPlacementFrame>,
    pub virtual_placements: Vec<VirtualPlacementFrame>,
    pub atlas_placements: Vec<AtlasPlacementFrame>,
    pub removed_keys: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct GraphicFrame {
    pub kind: u8,
    pub key: u64,
    pub width: u32,
    pub height: u32,
    pub color_type: u8,
    pub pixels: Vec<u8>,
    pub opacity: bool,
    pub display_width: Option<u32>,
    pub display_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct KittyPlacementFrame {
    pub image_id: u32,
    pub placement_id: u32,
    pub source: [u32; 4],
    pub dest_col: u32,
    pub dest_row: i64,
    pub columns: u32,
    pub rows: u32,
    pub requested_columns: u32,
    pub requested_rows: u32,
    pub cell_offset: [u32; 2],
    pub z_index: i32,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct VirtualPlacementFrame {
    pub image_id: u32,
    pub placement_id: u32,
    pub columns: u32,
    pub rows: u32,
    pub source: [u32; 4],
    pub cell_offset: [u32; 2],
    pub z_index: i32,
}

#[derive(Clone, Debug, PartialEq, bincode::Encode, bincode::Decode)]
pub struct AtlasPlacementFrame {
    pub key: u64,
    pub row: i64,
    pub column: u32,
    pub columns: u32,
    pub rows: u32,
    pub source: [u32; 4],
    pub image_width: u32,
    pub image_height: u32,
    pub cell_width: u32,
    pub cell_height: u32,
}

fn validate_string(value: &str) -> Result<(), crate::SessionError> {
    if value.len() > MAX_STRING_BYTES || value.as_bytes().contains(&0) {
        return Err(crate::SessionError::invalid(
            "string is too large or contains NUL",
        ));
    }
    Ok(())
}

fn validate_dimensions(columns: u16, lines: u16) -> Result<(), crate::SessionError> {
    if columns == 0 || lines == 0 {
        return Err(crate::SessionError::invalid(
            "terminal dimensions must be non-zero",
        ));
    }
    if columns > MAX_COLUMNS || lines > MAX_LINES {
        return Err(crate::SessionError::invalid(
            "terminal dimensions exceed the session limit",
        ));
    }
    if usize::from(columns) * usize::from(lines) > MAX_FRAME_CELLS {
        return Err(crate::SessionError::invalid(
            "terminal dimensions exceed the frame cell limit",
        ));
    }
    Ok(())
}

fn validate_pixel_dimensions(width: u16, height: u16) -> Result<(), crate::SessionError> {
    if width == 0 || height == 0 {
        return Err(crate::SessionError::invalid(
            "terminal pixel dimensions must be non-zero",
        ));
    }
    if u32::from(width) > MAX_GRAPHIC_DIMENSION
        || u32::from(height) > MAX_GRAPHIC_DIMENSION
    {
        return Err(crate::SessionError::invalid(
            "terminal pixel dimensions exceed the session limit",
        ));
    }
    Ok(())
}

fn validate_scroll(lines: i32) -> Result<(), crate::SessionError> {
    if lines.unsigned_abs() > MAX_WHEEL_LINES as u32 {
        return Err(crate::SessionError::invalid(
            "scroll amount exceeds the session limit",
        ));
    }
    Ok(())
}

fn validate_modifiers(modifiers: u8) -> Result<(), crate::SessionError> {
    if modifiers & !0x0f != 0 {
        return Err(crate::SessionError::invalid(
            "unknown modifier bits are set",
        ));
    }
    Ok(())
}

fn validate_graphic_dimensions(
    width: u32,
    height: u32,
) -> Result<(), crate::SessionError> {
    if width == 0
        || height == 0
        || width > MAX_GRAPHIC_DIMENSION
        || height > MAX_GRAPHIC_DIMENSION
    {
        return Err(crate::SessionError::protocol(
            "graphic dimensions are outside the session limit",
        ));
    }
    Ok(())
}

fn validate_color(color: &ColorFrame) -> Result<(), crate::SessionError> {
    if let ColorFrame::Named(value) = color {
        if !(*value <= 17 || (256..=268).contains(value)) {
            return Err(crate::SessionError::protocol(
                "named color is outside the supported range",
            ));
        }
    }
    Ok(())
}

fn validate_colors(colors: &[Option<[f32; 4]>]) -> Result<(), crate::SessionError> {
    if colors.len() != 269 {
        return Err(crate::SessionError::protocol(
            "frame color table does not match the terminal palette",
        ));
    }
    for color in colors.iter().flatten() {
        if color
            .iter()
            .any(|component| !component.is_finite() || !(0.0..=1.0).contains(component))
        {
            return Err(crate::SessionError::protocol(
                "frame color component is outside the supported range",
            ));
        }
    }
    Ok(())
}

fn validate_row(row: &RowFrame, columns: u16) -> Result<usize, crate::SessionError> {
    if row.cells.len() != usize::from(columns)
        || row.cells.len() != row.styles.len()
        || row.cells.len() != row.extras.len()
    {
        return Err(crate::SessionError::protocol(
            "frame row data does not match its dimensions",
        ));
    }
    for cell in &row.cells {
        cell.validate()?;
    }
    validate_string(&row.text)?;
    for extra in row.extras.iter().flatten() {
        if extra.zero_width.len() > MAX_COLUMNS as usize {
            return Err(crate::SessionError::protocol(
                "frame has too many zero-width characters",
            ));
        }
        if extra
            .zero_width
            .iter()
            .any(|value| char::from_u32(*value).is_none())
        {
            return Err(crate::SessionError::protocol(
                "frame contains an invalid zero-width character",
            ));
        }
        if let Some(hyperlink) = &extra.hyperlink {
            validate_string(&hyperlink.id)?;
            validate_string(&hyperlink.uri)?;
        }
    }
    for style in &row.styles {
        validate_color(&style.foreground)?;
        validate_color(&style.background)?;
        if let Some(underline) = &style.underline {
            validate_color(underline)?;
        }
    }
    Ok(row.cells.len())
}

struct FrameMetadata<'a> {
    columns: u16,
    lines: u16,
    display_offset: u32,
    history_size: u32,
    cursor: &'a CursorFrame,
    selection: Option<&'a SelectionFrame>,
    colors: &'a [Option<[f32; 4]>],
    title: &'a str,
    working_dir: Option<&'a str>,
}

fn validate_frame_metadata(
    metadata: FrameMetadata<'_>,
) -> Result<(), crate::SessionError> {
    let FrameMetadata {
        columns,
        lines,
        display_offset,
        history_size,
        cursor,
        selection,
        colors,
        title,
        working_dir,
    } = metadata;
    validate_colors(colors)?;
    if display_offset > history_size {
        return Err(crate::SessionError::protocol(
            "frame display offset exceeds its history",
        ));
    }
    if u64::from(history_size) > MAX_SCROLLBACK as u64 {
        return Err(crate::SessionError::protocol(
            "frame history exceeds the session limit",
        ));
    }
    if cursor.line >= lines || cursor.column >= columns {
        return Err(crate::SessionError::protocol(
            "frame cursor is outside its dimensions",
        ));
    }
    if cursor.shape > 3 {
        return Err(crate::SessionError::protocol(
            "frame cursor shape is unsupported",
        ));
    }
    if let Some(selection) = selection {
        if selection.start_line >= lines
            || selection.end_line >= lines
            || selection.start_column >= columns
            || selection.end_column >= columns
        {
            return Err(crate::SessionError::protocol(
                "frame selection is outside its dimensions",
            ));
        }
    }
    validate_string(title)?;
    if let Some(working_dir) = working_dir {
        validate_string(working_dir)?;
    }
    Ok(())
}

impl FrameUpdate {
    pub fn validate(&self) -> Result<(), crate::SessionError> {
        match self {
            Self::Full(frame) => frame.validate(),
            Self::Delta(delta) => delta.validate(),
        }
    }

    /// Apply a validated update to the one complete frame it follows. Delta
    /// application is transactional: all base and payload checks happen
    /// before the cached frame is changed.
    pub fn apply_to(self, frame: &mut FullFrame) -> Result<(), crate::SessionError> {
        match self {
            Self::Full(next) => {
                next.validate()?;
                *frame = next;
            }
            Self::Delta(delta) => {
                if frame.sequence != delta.base_sequence {
                    return Err(crate::SessionError::protocol(
                        "delta base sequence does not match the cached frame",
                    ));
                }
                if frame.columns != delta.columns || frame.lines != delta.lines {
                    return Err(crate::SessionError::protocol(
                        "delta dimensions do not match the cached frame",
                    ));
                }
                if frame.rows.len() != usize::from(delta.lines) {
                    return Err(crate::SessionError::protocol(
                        "cached frame rows do not match its dimensions",
                    ));
                }
                delta.validate()?;
                for changed in delta.rows {
                    frame.rows[usize::from(changed.line)] = changed.row;
                }
                frame.sequence = delta.sequence;
                frame.display_offset = delta.display_offset;
                frame.history_size = delta.history_size;
                frame.lines_evicted = delta.lines_evicted;
                frame.alternate_screen = delta.alternate_screen;
                frame.modes = delta.modes;
                frame.cursor = delta.cursor;
                frame.selection = delta.selection;
                frame.colors = delta.colors;
                frame.title = delta.title;
                frame.working_dir = delta.working_dir;
            }
        }
        Ok(())
    }
}

impl FrameDelta {
    pub fn validate(&self) -> Result<(), crate::SessionError> {
        if self.base_sequence == 0 {
            return Err(crate::SessionError::protocol(
                "delta base sequence must be non-zero",
            ));
        }
        let expected_sequence = self.base_sequence.checked_add(1).ok_or_else(|| {
            crate::SessionError::protocol("delta base sequence is exhausted")
        })?;
        if self.sequence != expected_sequence {
            return Err(crate::SessionError::protocol(
                "delta sequence must immediately follow its base",
            ));
        }
        validate_dimensions(self.columns, self.lines)?;
        if self.rows.len() > usize::from(self.lines) {
            return Err(crate::SessionError::protocol(
                "delta contains too many changed rows",
            ));
        }
        let mut previous_line = None;
        let mut cells = 0usize;
        for changed in &self.rows {
            if changed.line >= self.lines {
                return Err(crate::SessionError::protocol(
                    "delta row is outside its dimensions",
                ));
            }
            if previous_line.is_some_and(|line| changed.line <= line) {
                return Err(crate::SessionError::protocol(
                    "delta rows must be strictly ordered and unique",
                ));
            }
            previous_line = Some(changed.line);
            cells = cells
                .checked_add(validate_row(&changed.row, self.columns)?)
                .ok_or_else(|| {
                    crate::SessionError::protocol("delta cell count overflows")
                })?;
        }
        if cells > MAX_FRAME_CELLS {
            return Err(crate::SessionError::protocol("delta has too many cells"));
        }
        validate_frame_metadata(FrameMetadata {
            columns: self.columns,
            lines: self.lines,
            display_offset: self.display_offset,
            history_size: self.history_size,
            cursor: &self.cursor,
            selection: self.selection.as_ref(),
            colors: &self.colors,
            title: &self.title,
            working_dir: self.working_dir.as_deref(),
        })
    }
}

impl FullFrame {
    /// Validate semantic frame contents and protocol resource limits. Transports
    /// must additionally enforce their encoded-message byte limit.
    pub fn validate(&self) -> Result<(), crate::SessionError> {
        if self.sequence == 0 {
            return Err(crate::SessionError::protocol(
                "frame sequence must be non-zero",
            ));
        }
        validate_dimensions(self.columns, self.lines)?;
        if self.rows.len() != usize::from(self.lines) {
            return Err(crate::SessionError::protocol(
                "frame row count does not match its dimensions",
            ));
        }
        let cells = self.rows.iter().try_fold(0usize, |cells, row| {
            cells
                .checked_add(validate_row(row, self.columns)?)
                .ok_or_else(|| {
                    crate::SessionError::protocol("frame cell count overflows")
                })
        })?;
        if cells > MAX_FRAME_CELLS {
            return Err(crate::SessionError::protocol("frame has too many cells"));
        }
        validate_frame_metadata(FrameMetadata {
            columns: self.columns,
            lines: self.lines,
            display_offset: self.display_offset,
            history_size: self.history_size,
            cursor: &self.cursor,
            selection: self.selection.as_ref(),
            colors: &self.colors,
            title: &self.title,
            working_dir: self.working_dir.as_deref(),
        })?;
        self.graphics.validate()?;
        Ok(())
    }
}

impl GraphicsFrame {
    fn validate(&self) -> Result<(), crate::SessionError> {
        let item_count = self
            .images
            .len()
            .checked_add(self.kitty_placements.len())
            .and_then(|count| count.checked_add(self.virtual_placements.len()))
            .and_then(|count| count.checked_add(self.atlas_placements.len()))
            .ok_or_else(|| crate::SessionError::protocol("too many graphics items"))?;
        if item_count > MAX_GRAPHICS_ITEMS || self.removed_keys.len() > MAX_GRAPHICS_ITEMS
        {
            return Err(crate::SessionError::protocol("too many graphics items"));
        }
        let mut image_dimensions = HashMap::with_capacity(self.images.len());
        let mut bytes = 0usize;
        for image in &self.images {
            if image.kind > 1 || image.color_type > 1 {
                return Err(crate::SessionError::protocol(
                    "graphic kind or color type is unsupported",
                ));
            }
            validate_graphic_dimensions(image.width, image.height)?;
            if image.pixels.len() > MAX_IMAGE_BYTES {
                return Err(crate::SessionError::protocol("graphic image is too large"));
            }
            let channels = if image.color_type == 0 { 3 } else { 4 };
            let expected_bytes = (image.width as usize)
                .checked_mul(image.height as usize)
                .and_then(|pixels| pixels.checked_mul(channels))
                .ok_or_else(|| crate::SessionError::protocol("graphic size overflows"))?;
            if image.pixels.len() != expected_bytes {
                return Err(crate::SessionError::protocol(
                    "graphic pixel data does not match its dimensions",
                ));
            }
            bytes = bytes.checked_add(image.pixels.len()).ok_or_else(|| {
                crate::SessionError::protocol("graphics size overflows")
            })?;
            if let Some(width) = image.display_width {
                if width == 0 || width > MAX_GRAPHIC_DIMENSION {
                    return Err(crate::SessionError::protocol(
                        "graphic display width is outside the session limit",
                    ));
                }
            }
            if let Some(height) = image.display_height {
                if height == 0 || height > MAX_GRAPHIC_DIMENSION {
                    return Err(crate::SessionError::protocol(
                        "graphic display height is outside the session limit",
                    ));
                }
            }
            if image_dimensions
                .insert((image.kind, image.key), (image.width, image.height))
                .is_some()
            {
                return Err(crate::SessionError::protocol("duplicate graphic image key"));
            }
        }
        if bytes > MAX_FRAME_GRAPHICS_BYTES {
            return Err(crate::SessionError::protocol(
                "frame graphics exceed the session limit",
            ));
        }
        for placement in &self.kitty_placements {
            validate_graphic_row(placement.dest_row)?;
            validate_graphic_extent(placement.dest_col)?;
            validate_graphic_extent(placement.columns)?;
            validate_graphic_extent(placement.rows)?;
            validate_graphic_extent(placement.requested_columns)?;
            validate_graphic_extent(placement.requested_rows)?;
            validate_graphic_source(
                placement.source,
                image_dimensions.get(&(0, u64::from(placement.image_id))),
            )?;
            validate_graphic_extent(placement.cell_offset[0])?;
            validate_graphic_extent(placement.cell_offset[1])?;
        }
        for placement in &self.virtual_placements {
            validate_graphic_extent(placement.columns)?;
            validate_graphic_extent(placement.rows)?;
            validate_graphic_source(
                placement.source,
                image_dimensions.get(&(0, u64::from(placement.image_id))),
            )?;
            validate_graphic_extent(placement.cell_offset[0])?;
            validate_graphic_extent(placement.cell_offset[1])?;
        }
        for placement in &self.atlas_placements {
            validate_graphic_row(placement.row)?;
            validate_graphic_extent(placement.column)?;
            validate_graphic_extent(placement.columns)?;
            validate_graphic_extent(placement.rows)?;
            validate_graphic_source(
                placement.source,
                image_dimensions.get(&(1, placement.key)),
            )?;
            validate_graphic_extent(placement.image_width)?;
            validate_graphic_extent(placement.image_height)?;
            validate_graphic_extent(placement.cell_width)?;
            validate_graphic_extent(placement.cell_height)?;
        }
        Ok(())
    }
}

fn validate_graphic_extent(value: u32) -> Result<(), crate::SessionError> {
    if value > MAX_GRAPHIC_DIMENSION {
        return Err(crate::SessionError::protocol(
            "graphic placement exceeds the session limit",
        ));
    }
    Ok(())
}

fn validate_graphic_rect(rect: [u32; 4]) -> Result<(u32, u32), crate::SessionError> {
    validate_graphic_extent(rect[0])?;
    validate_graphic_extent(rect[1])?;
    let right = rect[0].checked_add(rect[2]).ok_or_else(|| {
        crate::SessionError::protocol("graphic source rectangle overflows")
    })?;
    let bottom = rect[1].checked_add(rect[3]).ok_or_else(|| {
        crate::SessionError::protocol("graphic source rectangle overflows")
    })?;
    if right > MAX_GRAPHIC_DIMENSION || bottom > MAX_GRAPHIC_DIMENSION {
        return Err(crate::SessionError::protocol(
            "graphic source rectangle exceeds the session limit",
        ));
    }
    Ok((right, bottom))
}

fn validate_graphic_source(
    rect: [u32; 4],
    image: Option<&(u32, u32)>,
) -> Result<(), crate::SessionError> {
    let (right, bottom) = validate_graphic_rect(rect)?;
    if rect[2] == 0 || rect[3] == 0 {
        return Err(crate::SessionError::protocol(
            "graphic source rectangle is empty",
        ));
    }
    let Some(&(width, height)) = image else {
        return Err(crate::SessionError::protocol(
            "graphic placement references a missing image",
        ));
    };
    if right > width || bottom > height {
        return Err(crate::SessionError::protocol(
            "graphic source rectangle exceeds its image",
        ));
    }
    Ok(())
}

fn validate_graphic_row(value: i64) -> Result<(), crate::SessionError> {
    if value.unsigned_abs() > MAX_SCROLLBACK as u64 {
        return Err(crate::SessionError::protocol(
            "graphic row is outside the session limit",
        ));
    }
    Ok(())
}

pub(crate) fn hex_bytes(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(ch: char) -> RowFrame {
        RowFrame {
            cells: vec![CellFrame {
                content: CellContentFrame::Codepoint(ch as u32),
                wide: 0,
                flags: 0,
            }],
            styles: vec![StyleFrame {
                foreground: ColorFrame::Named(7),
                background: ColorFrame::Named(0),
                underline: None,
                flags: 0,
            }],
            extras: vec![None],
            kitty_virtual_placeholder: false,
            text: ch.to_string(),
        }
    }

    fn frame() -> FullFrame {
        FullFrame {
            sequence: 1,
            columns: 1,
            lines: 1,
            rows: vec![row('a')],
            display_offset: 0,
            history_size: 0,
            lines_evicted: 0,
            alternate_screen: false,
            modes: 0,
            cursor: CursorFrame {
                line: 0,
                column: 0,
                visible: true,
                blinking: true,
                shape: 0,
            },
            selection: None,
            colors: vec![None; 269],
            graphics: GraphicsFrame::default(),
            title: String::new(),
            working_dir: None,
        }
    }

    fn delta(sequence: u64, rows: Vec<RowUpdate>) -> FrameDelta {
        FrameDelta {
            base_sequence: sequence - 1,
            sequence,
            columns: 1,
            lines: 1,
            rows,
            display_offset: 0,
            history_size: 0,
            lines_evicted: 0,
            alternate_screen: false,
            modes: 0,
            cursor: CursorFrame {
                line: 0,
                column: 0,
                visible: true,
                blinking: true,
                shape: 0,
            },
            selection: None,
            colors: vec![None; 269],
            title: String::new(),
            working_dir: None,
        }
    }

    #[test]
    fn cursor_only_delta_has_no_rows_and_applies_metadata() {
        let mut cached = frame();
        let update = delta(2, Vec::new());
        update.validate().unwrap();
        FrameUpdate::Delta(update).apply_to(&mut cached).unwrap();
        assert_eq!(cached.sequence, 2);
        assert_eq!(cached.rows[0].text, "a");
    }

    #[test]
    fn delta_rejects_duplicate_or_out_of_range_rows() {
        let duplicate = delta(
            2,
            vec![
                RowUpdate {
                    line: 0,
                    row: row('b'),
                },
                RowUpdate {
                    line: 0,
                    row: row('c'),
                },
            ],
        );
        assert!(duplicate.validate().is_err());

        let out_of_range = FrameDelta {
            lines: 1,
            rows: vec![RowUpdate {
                line: 1,
                row: row('b'),
            }],
            ..delta(2, Vec::new())
        };
        assert!(out_of_range.validate().is_err());
    }

    #[test]
    fn delta_base_mismatch_does_not_mutate_cached_frame() {
        let mut cached = frame();
        cached.sequence = 9;
        let update = FrameUpdate::Delta(delta(
            2,
            vec![RowUpdate {
                line: 0,
                row: row('b'),
            }],
        ));
        assert!(update.apply_to(&mut cached).is_err());
        assert_eq!(cached.sequence, 9);
        assert_eq!(cached.rows[0].text, "a");
    }

    #[cfg(unix)]
    #[test]
    fn conditional_file_cleanup_preserves_replacement() {
        let directory = std::env::temp_dir().join(format!(
            "rio-session-recovery-cleanup-{}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("session");
        std::fs::write(&path, b"original").unwrap();
        let original = std::fs::OpenOptions::new().read(true).open(&path).unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        assert!(!remove_file_if_open_file(&path, &original).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");

        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
