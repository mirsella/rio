//! # Wayland
//!
//! **Note:** Windows don't appear on Wayland until you draw/present to them.
//!
//! By default, Winit loads system libraries using `dlopen`. This can be
//! disabled by disabling the `"wayland-dlopen"` cargo feature.
//!
//! ## Client-side decorations
//!
//! Winit provides client-side decorations by default, but the behaviour can
//! be controlled with the following feature flags:
//!
//! * `wayland-csd-adwaita` (default).
//! * `wayland-csd-adwaita-crossfont`.
//! * `wayland-csd-adwaita-notitle`.
use std::fmt;
use std::io;

use crate::dpi::LogicalPosition;
use crate::error::NotSupportedError;
use crate::event_loop::{ActiveEventLoop, EventLoopBuilder};
use crate::monitor::MonitorHandle;
use crate::window::{Window, WindowAttributes};

pub use crate::window::Theme;

/// MIME type used by Rio's private toplevel drag transport.
pub(crate) const TOPLEVEL_DRAG_MIME_TYPE: &str = "application/x-rio-toplevel-drag";

/// Maximum unframed payload accepted by the private transport.
pub(crate) const TOPLEVEL_DRAG_MAX_PAYLOAD: usize = 64 * 1024;

/// Opaque identifier for a toplevel drag source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToplevelDragId(u64);

impl ToplevelDragId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Opaque identifier for an incoming toplevel drag offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToplevelDragOfferId(u64);

impl ToplevelDragOfferId {
    pub(crate) const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Events emitted for Rio's private Wayland toplevel drag transport.
#[derive(Debug, Clone, PartialEq)]
pub enum ToplevelDragEvent {
    FrameDrag {
        position: LogicalPosition<f64>,
        seat_id: u32,
        pointer_id: u32,
    },
    Entered {
        offer_id: ToplevelDragOfferId,
        position: LogicalPosition<f64>,
    },
    Motion {
        offer_id: ToplevelDragOfferId,
        position: LogicalPosition<f64>,
    },
    Left {
        offer_id: ToplevelDragOfferId,
    },
    Dropped {
        offer_id: ToplevelDragOfferId,
    },
    SourceActionsChanged {
        offer_id: ToplevelDragOfferId,
        move_supported: bool,
    },
    SelectedActionChanged {
        offer_id: ToplevelDragOfferId,
        selected_move: bool,
    },
    DataReady {
        offer_id: ToplevelDragOfferId,
        data: Vec<u8>,
    },
    OfferCancelled {
        offer_id: ToplevelDragOfferId,
    },
    OfferDataFailed {
        offer_id: ToplevelDragOfferId,
    },
    Finished {
        drag_id: ToplevelDragId,
    },
    Cancelled {
        drag_id: ToplevelDragId,
    },
}

/// Error returned by the Wayland toplevel drag API.
#[derive(Debug)]
pub enum ToplevelDragError {
    Unsupported(NotSupportedError),
    NoPointerGrab,
    InvalidDrag,
    InvalidOffer,
    InvalidState,
    WrongOwner,
    PayloadTooLarge,
    Io(io::Error),
}

impl fmt::Display for ToplevelDragError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(error) => error.fmt(f),
            Self::NoPointerGrab => {
                f.pad("no pressed pointer button is focused on this window")
            }
            Self::InvalidDrag => f.pad("the toplevel drag is no longer active"),
            Self::InvalidOffer => f.pad("the toplevel drag offer is no longer active"),
            Self::InvalidState => {
                f.pad("the operation is invalid in the current drag state")
            }
            Self::WrongOwner => f.pad("the drag or offer belongs to another window"),
            Self::PayloadTooLarge => {
                f.pad("the private drag payload exceeds the size limit")
            }
            Self::Io(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ToplevelDragError {}

/// Additional methods on [`ActiveEventLoop`] that are specific to Wayland.
pub trait ActiveEventLoopExtWayland {
    /// True if the [`ActiveEventLoop`] uses Wayland.
    fn is_wayland(&self) -> bool;
}

impl ActiveEventLoopExtWayland for ActiveEventLoop {
    #[inline]
    fn is_wayland(&self) -> bool {
        self.p.is_wayland()
    }
}

/// Additional methods on [`EventLoopBuilder`] that are specific to Wayland.
pub trait EventLoopBuilderExtWayland {
    /// Force using Wayland.
    fn with_wayland(&mut self) -> &mut Self;

    /// Whether to allow the event loop to be created off of the main thread.
    ///
    /// By default, the window is only allowed to be created on the main
    /// thread, to make platform compatibility easier.
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self;
}

impl<T> EventLoopBuilderExtWayland for EventLoopBuilder<T> {
    #[inline]
    fn with_wayland(&mut self) -> &mut Self {
        self.platform_specific.forced_backend =
            Some(crate::platform_impl::Backend::Wayland);
        self
    }

    #[inline]
    fn with_any_thread(&mut self, any_thread: bool) -> &mut Self {
        self.platform_specific.any_thread = any_thread;
        self
    }
}

/// Additional methods on [`Window`] that are specific to Wayland.
pub trait WindowExtWayland {
    /// Whether the core Wayland data-device drag protocol is available.
    fn supports_toplevel_drag(&self) -> bool;

    /// Prepare a Move drag from the currently pressed pointer button.
    ///
    /// The returned drag must be started from the same pointer grab. Until then
    /// it remains prepared and can be cancelled locally.
    fn prepare_toplevel_drag(
        &self,
        data: Vec<u8>,
        frame_grab: Option<(u32, u32)>,
    ) -> Result<ToplevelDragId, ToplevelDragError>;

    /// Start a prepared drag from the currently active pointer grab.
    fn start_toplevel_drag(
        &self,
        drag_id: ToplevelDragId,
    ) -> Result<(), ToplevelDragError>;

    /// Accept an incoming private-MIME offer with the Move action.
    fn accept_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError>;

    /// Reject an incoming private-MIME offer.
    fn reject_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError>;

    /// Begin bounded nonblocking receipt of an accepted, dropped Move offer.
    ///
    /// Data is delivered through [`ToplevelDragEvent::DataReady`].
    fn receive_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError>;

    /// Explicitly finish a successfully received offer.
    fn finish_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError>;

    /// Reject and destroy an offer without reporting success.
    fn cancel_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError>;

    /// Cancel a source owned by this window.
    ///
    /// This removes prepared or active source state immediately. Active sources
    /// are locally terminated rather than waiting for a compositor lifecycle
    /// callback.
    fn cancel_toplevel_drag(
        &self,
        drag_id: ToplevelDragId,
    ) -> Result<(), ToplevelDragError>;
}

impl WindowExtWayland for Window {
    fn supports_toplevel_drag(&self) -> bool {
        self.window.supports_toplevel_drag()
    }

    fn prepare_toplevel_drag(
        &self,
        data: Vec<u8>,
        frame_grab: Option<(u32, u32)>,
    ) -> Result<ToplevelDragId, ToplevelDragError> {
        self.window.prepare_toplevel_drag(data, frame_grab)
    }

    fn start_toplevel_drag(
        &self,
        drag_id: ToplevelDragId,
    ) -> Result<(), ToplevelDragError> {
        self.window.start_toplevel_drag(drag_id)
    }

    fn accept_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError> {
        self.window.accept_toplevel_drag_offer(offer_id)
    }

    fn reject_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError> {
        self.window.reject_toplevel_drag_offer(offer_id)
    }

    fn receive_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError> {
        self.window.receive_toplevel_drag_offer(offer_id)
    }

    fn finish_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError> {
        self.window.finish_toplevel_drag_offer(offer_id)
    }

    fn cancel_toplevel_drag_offer(
        &self,
        offer_id: ToplevelDragOfferId,
    ) -> Result<(), ToplevelDragError> {
        self.window.cancel_toplevel_drag_offer(offer_id)
    }

    fn cancel_toplevel_drag(
        &self,
        drag_id: ToplevelDragId,
    ) -> Result<(), ToplevelDragError> {
        self.window.cancel_toplevel_drag(drag_id)
    }
}

/// Additional methods on [`WindowAttributes`] that are specific to Wayland.
pub trait WindowAttributesExtWayland {
    /// Build window with the given name.
    ///
    /// The `general` name sets an application ID, which should match the `.desktop`
    /// file distributed with your program. The `instance` is a `no-op`.
    ///
    /// For details about application ID conventions, see the
    /// [Desktop Entry Spec](https://specifications.freedesktop.org/desktop-entry-spec/desktop-entry-spec-latest.html#desktop-file-id)
    fn with_name(self, general: impl Into<String>, instance: impl Into<String>) -> Self;
}

impl WindowAttributesExtWayland for WindowAttributes {
    #[inline]
    fn with_name(
        mut self,
        general: impl Into<String>,
        instance: impl Into<String>,
    ) -> Self {
        self.platform_specific.name = Some(crate::platform_impl::ApplicationName::new(
            general.into(),
            instance.into(),
        ));
        self
    }
}

/// Additional methods on `MonitorHandle` that are specific to Wayland.
pub trait MonitorHandleExtWayland {
    /// Returns the inner identifier of the monitor.
    fn native_id(&self) -> u32;
}

impl MonitorHandleExtWayland for MonitorHandle {
    #[inline]
    fn native_id(&self) -> u32 {
        self.inner.native_identifier()
    }
}
