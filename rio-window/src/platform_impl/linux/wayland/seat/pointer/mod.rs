//! The pointer events.

use std::ops::Deref;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::warn;

use sctk::reexports::client::delegate_dispatch;
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle, Dispatch};
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_confined_pointer_v1::ZwpConfinedPointerV1;
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_locked_pointer_v1::ZwpLockedPointerV1;
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::WpCursorShapeDeviceV1;
use sctk::reexports::protocols::wp::cursor_shape::v1::client::wp_cursor_shape_manager_v1::WpCursorShapeManagerV1;
use sctk::reexports::protocols::wp::pointer_constraints::zv1::client::zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1};
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::csd_frame::FrameClick;

use sctk::globals::GlobalData;
use sctk::seat::pointer::{
    PointerData, PointerDataExt, PointerEvent, PointerEventKind, PointerHandler,
};
use sctk::seat::SeatState;

use crate::dpi::{LogicalPosition, PhysicalPosition};
use crate::event::{
    ElementState, MouseButton, MouseScrollDelta, TouchPhase, WindowEvent,
};

use crate::platform_impl::wayland::data_device::{FrameDragGrab, LEFT_BUTTON};
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{self, DeviceId, WindowId};

pub mod relative_pointer;

impl PointerHandler for WinitState {
    fn pointer_frame(
        &mut self,
        connection: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        self.mark_input_received();

        let seat = pointer.winit_data().seat();
        let seat_state = match self.seats.get(&seat.id()) {
            Some(seat_state) => seat_state,
            None => {
                warn!("Received pointer event without seat");
                return;
            }
        };

        let themed_pointer = match seat_state.pointer.as_ref() {
            Some(pointer) => pointer,
            None => {
                warn!("Received pointer event without pointer");
                return;
            }
        };

        let device_id =
            crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(DeviceId));

        for (event_index, event) in events.iter().enumerate() {
            let surface = &event.surface;

            match event.kind {
                PointerEventKind::Press { button, serial, .. } => {
                    pointer
                        .winit_data()
                        .button_pressed(button, serial, surface.clone());
                }
                PointerEventKind::Release { button, .. } => {
                    pointer.winit_data().button_released(button);
                }
                _ => {}
            }

            let parent_surface = wayland::root_surface(surface);

            let window_id = wayland::make_wid(&parent_surface);

            let forgotten_frame_window = if matches!(
                event.kind,
                PointerEventKind::Release { button, .. } if button == LEFT_BUTTON
            ) {
                self.toplevel_drag
                    .lock()
                    .unwrap()
                    .forget_frame_drag_for_pointer(&seat.id(), pointer.id())
            } else {
                None
            };

            if let Some(frame_window_id) = forgotten_frame_window {
                if let Some(frame_window) =
                    self.windows.get_mut().get_mut(&frame_window_id)
                {
                    frame_window
                        .lock()
                        .unwrap()
                        .cancel_pending_move_for_seat(&seat.id());
                }
            }

            // Ensure that window exists.
            let mut window = match self.windows.get_mut().get_mut(&window_id) {
                Some(window) => window.lock().unwrap(),
                None => continue,
            };

            if matches!(event.kind, PointerEventKind::Enter { .. }) {
                pointer.winit_data().inner.lock().unwrap().surface = Some(window_id);
            }

            let scale_factor = window.scale_factor();
            let position: PhysicalPosition<f64> =
                LogicalPosition::new(event.position.0, event.position.1)
                    .to_physical(scale_factor);

            match event.kind {
                // Pointer movements on decorations.
                PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. }
                    if &parent_surface != surface =>
                {
                    let custom_drag = self.toplevel_drag.lock().unwrap().has_frame_drag(
                        window_id,
                        &seat.id(),
                        pointer.id(),
                    );
                    let icon = if custom_drag {
                        window.frame_point_moved_without_native_move(
                            &seat.id(),
                            surface,
                            Duration::ZERO,
                            event.position.0,
                            event.position.1,
                        )
                    } else {
                        window.frame_point_moved(
                            seat,
                            surface,
                            Duration::ZERO,
                            event.position.0,
                            event.position.1,
                        )
                    };
                    if let Some(icon) = icon {
                        let _ = themed_pointer.set_cursor(connection, icon);
                    }
                }
                PointerEventKind::Leave { .. } if &parent_surface != surface => {
                    window.frame_point_left(&seat.id());
                }
                ref kind @ PointerEventKind::Press {
                    button,
                    serial,
                    time,
                }
                | ref kind @ PointerEventKind::Release {
                    button,
                    serial,
                    time,
                } if &parent_surface != surface => {
                    let click = match wayland_button_to_winit(button) {
                        MouseButton::Left => FrameClick::Normal,
                        MouseButton::Right => FrameClick::Alternate,
                        _ => continue,
                    };
                    let pressed = matches!(kind, PointerEventKind::Press { .. });

                    // Emulate click on the frame.
                    let moves = window.frame_click(
                        click,
                        pressed,
                        seat,
                        serial,
                        Duration::from_millis(time as u64),
                        window_id,
                        &mut self.window_compositor_updates,
                    ) == Some(true);
                    if pressed
                        && moves
                        && matches!(click, FrameClick::Normal)
                        && self.toplevel_drag.lock().unwrap().supported()
                        // The application callback runs after this protocol
                        // batch. Starting a DND after a queued release would
                        // violate Wayland's implicit-grab requirement, so
                        // leave this press on the native-move path instead.
                        && !events[event_index + 1..].iter().any(|event| {
                            matches!(
                                event.kind,
                                PointerEventKind::Release { button, .. }
                                    if button == LEFT_BUTTON
                            )
                        })
                    {
                        self.toplevel_drag.lock().unwrap().remember_frame_drag(
                            window_id,
                            FrameDragGrab {
                                seat_id: seat.id(),
                                pointer_id: pointer.id(),
                                button,
                                serial,
                                origin: surface.clone(),
                            },
                        );
                        self.events_sink.push_window_event(
                            WindowEvent::ToplevelDrag(
                                crate::platform::wayland::ToplevelDragEvent::FrameDrag {
                                    position: LogicalPosition::new(
                                        event.position.0,
                                        event.position.1,
                                    ),
                                    seat_id: seat.id().protocol_id(),
                                    pointer_id: pointer.id().protocol_id(),
                                },
                            ),
                            window_id,
                        );
                        self.dispatched_events = true;
                    }
                }
                // Regular events on the main surface.
                PointerEventKind::Enter { .. } => {
                    self.events_sink.push_window_event(
                        WindowEvent::CursorEntered { device_id },
                        window_id,
                    );

                    window.pointer_entered(Arc::downgrade(themed_pointer));

                    self.events_sink.push_window_event(
                        WindowEvent::CursorMoved {
                            device_id,
                            position,
                        },
                        window_id,
                    );
                }
                PointerEventKind::Leave { .. } => {
                    window.pointer_left(Arc::downgrade(themed_pointer));

                    // Remove the active surface.
                    pointer.winit_data().inner.lock().unwrap().surface = None;

                    self.events_sink.push_window_event(
                        WindowEvent::CursorLeft { device_id },
                        window_id,
                    );
                }
                PointerEventKind::Motion { .. } => {
                    self.events_sink.push_window_event(
                        WindowEvent::CursorMoved {
                            device_id,
                            position,
                        },
                        window_id,
                    );
                }
                ref kind @ PointerEventKind::Press { button, .. }
                | ref kind @ PointerEventKind::Release { button, .. } => {
                    let pressed = matches!(kind, PointerEventKind::Press { .. });

                    let button = wayland_button_to_winit(button);
                    let state = if pressed {
                        ElementState::Pressed
                    } else {
                        ElementState::Released
                    };
                    self.events_sink.push_window_event(
                        WindowEvent::MouseInput {
                            device_id,
                            state,
                            button,
                        },
                        window_id,
                    );
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    // Get the current phase.
                    let mut pointer_data = pointer.winit_data().inner.lock().unwrap();

                    let has_discrete_scroll =
                        horizontal.discrete != 0 || vertical.discrete != 0;

                    // Figure out what to do about start/ended phases here.
                    //
                    // Figure out how to deal with `Started`. Also the `Ended` is not guaranteed
                    // to be sent for mouse wheels.
                    let phase = if horizontal.stop || vertical.stop {
                        TouchPhase::Ended
                    } else {
                        match pointer_data.phase {
                            // Discrete scroll only results in moved events.
                            _ if has_discrete_scroll => TouchPhase::Moved,
                            TouchPhase::Started | TouchPhase::Moved => TouchPhase::Moved,
                            _ => TouchPhase::Started,
                        }
                    };

                    // Update the phase.
                    pointer_data.phase = phase;

                    // Mice events have both pixel and discrete delta's at the same time. So prefer
                    // the descrite values if they are present.
                    let delta = if has_discrete_scroll {
                        // NOTE: Wayland sign convention is the inverse of winit.
                        MouseScrollDelta::LineDelta(
                            (-horizontal.discrete) as f32,
                            (-vertical.discrete) as f32,
                        )
                    } else {
                        // NOTE: Wayland sign convention is the inverse of winit.
                        MouseScrollDelta::PixelDelta(
                            LogicalPosition::new(
                                -horizontal.absolute,
                                -vertical.absolute,
                            )
                            .to_physical(scale_factor),
                        )
                    };

                    self.events_sink.push_window_event(
                        WindowEvent::MouseWheel {
                            device_id,
                            delta,
                            phase,
                        },
                        window_id,
                    )
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct WinitPointerData {
    /// The inner winit data associated with the pointer.
    inner: Mutex<WinitPointerDataInner>,

    /// The data required by the sctk.
    sctk_data: PointerData,
}

impl WinitPointerData {
    pub fn new(seat: WlSeat) -> Self {
        Self {
            inner: Mutex::new(WinitPointerDataInner::default()),
            sctk_data: PointerData::new(seat),
        }
    }

    pub fn lock_pointer(
        &self,
        pointer_constraints: &PointerConstraintsState,
        surface: &WlSurface,
        pointer: &WlPointer,
        queue_handle: &QueueHandle<WinitState>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.locked_pointer.is_none() {
            inner.locked_pointer = Some(pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                queue_handle,
                GlobalData,
            ));
        }
    }

    pub fn unlock_pointer(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(locked_pointer) = inner.locked_pointer.take() {
            locked_pointer.destroy();
        }
    }

    pub fn confine_pointer(
        &self,
        pointer_constraints: &PointerConstraintsState,
        surface: &WlSurface,
        pointer: &WlPointer,
        queue_handle: &QueueHandle<WinitState>,
    ) {
        self.inner.lock().unwrap().confined_pointer =
            Some(pointer_constraints.confine_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                queue_handle,
                GlobalData,
            ));
    }

    pub fn unconfine_pointer(&self) {
        let inner = self.inner.lock().unwrap();
        if let Some(confined_pointer) = inner.confined_pointer.as_ref() {
            confined_pointer.destroy();
        }
    }

    /// Seat associated with this pointer.
    pub fn seat(&self) -> &WlSeat {
        self.sctk_data.seat()
    }

    /// Active window.
    pub fn focused_window(&self) -> Option<WindowId> {
        self.inner.lock().unwrap().surface
    }

    /// Last button serial.
    pub fn latest_button_serial(&self) -> u32 {
        self.sctk_data.latest_button_serial().unwrap_or_default()
    }

    /// Most recent active grab for `button` and its implicit-grab serial.
    pub fn active_button_grab(&self, button: u32) -> Option<(u32, u32, WlSurface)> {
        let inner = self.inner.lock().unwrap();
        let (pressed, serial, surface) = inner
            .active_buttons
            .iter()
            .rev()
            .find(|(pressed, _, _)| *pressed == button)?;
        Some((*pressed, *serial, surface.clone()))
    }

    fn button_pressed(&self, button: u32, serial: u32, surface: WlSurface) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .active_buttons
            .retain(|(pressed, _, _)| *pressed != button);
        inner.active_buttons.push((button, serial, surface));
    }

    fn button_released(&self, button: u32) {
        let mut inner = self.inner.lock().unwrap();
        inner
            .active_buttons
            .retain(|(pressed, _, _)| *pressed != button);
    }

    /// Last enter serial.
    pub fn latest_enter_serial(&self) -> u32 {
        self.sctk_data.latest_enter_serial().unwrap_or_default()
    }

    pub fn set_locked_cursor_position(&self, surface_x: f64, surface_y: f64) {
        let inner = self.inner.lock().unwrap();
        if let Some(locked_pointer) = inner.locked_pointer.as_ref() {
            locked_pointer.set_cursor_position_hint(surface_x, surface_y);
        }
    }
}

impl PointerDataExt for WinitPointerData {
    fn pointer_data(&self) -> &PointerData {
        &self.sctk_data
    }
}

#[derive(Debug)]
pub struct WinitPointerDataInner {
    /// The associated locked pointer.
    locked_pointer: Option<ZwpLockedPointerV1>,

    /// The associated confined pointer.
    confined_pointer: Option<ZwpConfinedPointerV1>,

    /// Pressed buttons in press order with their implicit-grab serials and surfaces.
    active_buttons: Vec<(u32, u32, WlSurface)>,

    /// Currently focused window.
    surface: Option<WindowId>,

    /// Current axis phase.
    phase: TouchPhase,
}

impl Drop for WinitPointerDataInner {
    fn drop(&mut self) {
        if let Some(locked_pointer) = self.locked_pointer.take() {
            locked_pointer.destroy();
        }

        if let Some(confined_pointer) = self.confined_pointer.take() {
            confined_pointer.destroy();
        }
    }
}

impl Default for WinitPointerDataInner {
    fn default() -> Self {
        Self {
            surface: None,
            locked_pointer: None,
            confined_pointer: None,
            active_buttons: Vec::new(),
            phase: TouchPhase::Ended,
        }
    }
}

/// Convert the Wayland button into winit.
fn wayland_button_to_winit(button: u32) -> MouseButton {
    // These values are coming from <linux/input-event-codes.h>.
    const BTN_LEFT: u32 = 0x110;
    const BTN_RIGHT: u32 = 0x111;
    const BTN_MIDDLE: u32 = 0x112;
    const BTN_SIDE: u32 = 0x113;
    const BTN_EXTRA: u32 = 0x114;
    const BTN_FORWARD: u32 = 0x115;
    const BTN_BACK: u32 = 0x116;

    match button {
        BTN_LEFT => MouseButton::Left,
        BTN_RIGHT => MouseButton::Right,
        BTN_MIDDLE => MouseButton::Middle,
        BTN_BACK | BTN_SIDE => MouseButton::Back,
        BTN_FORWARD | BTN_EXTRA => MouseButton::Forward,
        button => MouseButton::Other(button as u16),
    }
}

pub trait WinitPointerDataExt {
    fn winit_data(&self) -> &WinitPointerData;
}

impl WinitPointerDataExt for WlPointer {
    fn winit_data(&self) -> &WinitPointerData {
        self.data::<WinitPointerData>()
            .expect("failed to get pointer data.")
    }
}

pub struct PointerConstraintsState {
    pointer_constraints: ZwpPointerConstraintsV1,
}

impl PointerConstraintsState {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let pointer_constraints = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self {
            pointer_constraints,
        })
    }
}

impl Deref for PointerConstraintsState {
    type Target = ZwpPointerConstraintsV1;

    fn deref(&self) -> &Self::Target {
        &self.pointer_constraints
    }
}

impl Dispatch<ZwpPointerConstraintsV1, GlobalData, WinitState>
    for PointerConstraintsState
{
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpPointerConstraintsV1,
        _event: <ZwpPointerConstraintsV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpLockedPointerV1, GlobalData, WinitState> for PointerConstraintsState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpLockedPointerV1,
        _event: <ZwpLockedPointerV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpConfinedPointerV1, GlobalData, WinitState> for PointerConstraintsState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpConfinedPointerV1,
        _event: <ZwpConfinedPointerV1 as wayland_client::Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<WpCursorShapeDeviceV1, GlobalData, WinitState> for SeatState {
    fn event(
        _: &mut WinitState,
        _: &WpCursorShapeDeviceV1,
        _: <WpCursorShapeDeviceV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("wp_cursor_shape_manager has no events")
    }
}

impl Dispatch<WpCursorShapeManagerV1, GlobalData, WinitState> for SeatState {
    fn event(
        _: &mut WinitState,
        _: &WpCursorShapeManagerV1,
        _: <WpCursorShapeManagerV1 as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("wp_cursor_device_manager has no events")
    }
}

delegate_dispatch!(WinitState: [ WlPointer: WinitPointerData] => SeatState);
delegate_dispatch!(WinitState: [ WpCursorShapeManagerV1: GlobalData] => SeatState);
delegate_dispatch!(WinitState: [ WpCursorShapeDeviceV1: GlobalData] => SeatState);
delegate_dispatch!(WinitState: [ZwpPointerConstraintsV1: GlobalData] => PointerConstraintsState);
delegate_dispatch!(WinitState: [ZwpLockedPointerV1: GlobalData] => PointerConstraintsState);
delegate_dispatch!(WinitState: [ZwpConfinedPointerV1: GlobalData] => PointerConstraintsState);
