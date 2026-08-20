use std::borrow::Cow;
use std::cell::Cell;
use std::io::{self, ErrorKind, Read, Write};
use std::mem;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use ahash::{AHashMap, AHashSet};
use calloop::{PostAction, RegistrationToken};
use percent_encoding::percent_decode_str;
use sctk::data_device_manager::data_device::{
    DataDevice, DataDeviceData, DataDeviceHandler,
};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{DataSourceHandler, DragSource};
use sctk::data_device_manager::{DataDeviceManagerState, ReadPipe, WritePipe};
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use crate::dpi::LogicalPosition;
use crate::event::WindowEvent;
use crate::platform::wayland::{
    ToplevelDragError, ToplevelDragEvent, ToplevelDragId, ToplevelDragOfferId,
    TOPLEVEL_DRAG_MAX_PAYLOAD, TOPLEVEL_DRAG_MIME_TYPE,
};

use super::seat::WinitPointerData;
use super::state::WinitState;
use super::{make_wid, root_surface, WindowId};

const URI_LIST_MIME_TYPE: &str = "text/uri-list";
const PRIVATE_FRAME_MAGIC: &[u8; 8] = b"RIOXTD01";
const PRIVATE_FRAME_HEADER: usize = PRIVATE_FRAME_MAGIC.len() + 4;
const MAX_URI_LIST_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const LEFT_BUTTON: u32 = 0x110;

pub(crate) struct DragSeat {
    pub data_device: Arc<DataDevice>,
    pub pointer: Option<Arc<sctk::seat::pointer::ThemedPointer<WinitPointerData>>>,
}

pub(crate) struct FrameDragGrab {
    pub seat_id: ObjectId,
    pub pointer_id: ObjectId,
    pub button: u32,
    pub serial: u32,
    pub origin: WlSurface,
}

pub(crate) enum SourcePhase {
    Prepared,
    Started,
    DropPerformed,
}

pub(crate) struct DragSourceState {
    pub source: DragSource,
    pub window_id: WindowId,
    pub origin: WlSurface,
    pub seat_id: ObjectId,
    pub pointer_id: ObjectId,
    pub button: u32,
    pub serial: u32,
    pub data_device: Arc<DataDevice>,
    pub framed_data: Arc<[u8]>,
    pub phase: SourcePhase,
    pub writer_tokens: Vec<RegistrationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OfferKind {
    Private,
    File,
}

pub(crate) type CancelledOffer = (ToplevelDragOfferId, WindowId, OfferKind, bool);
pub(crate) type CancelledSource = (ToplevelDragId, WindowId);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferPhase {
    Undecided,
    Accepted,
    Receiving,
    Ready,
}

fn can_receive_private(phase: OfferPhase, facts: (bool, DndAction, DndAction)) -> bool {
    let (dropped, source_actions, selected_action) = facts;
    phase == OfferPhase::Accepted
        && dropped
        && source_actions.contains(DndAction::Move)
        && selected_action == DndAction::Move
}

fn can_finish(phase: OfferPhase, facts: (bool, DndAction)) -> bool {
    let (dropped, selected_action) = facts;
    dropped && phase == OfferPhase::Ready && selected_action == DndAction::Move
}

fn can_decide_private_offer(kind: OfferKind, left: bool, dropped: bool) -> bool {
    kind == OfferKind::Private && !left && !dropped
}

fn offer_receive_limit(
    kind: OfferKind,
    phase: OfferPhase,
    facts: (bool, DndAction, DndAction),
) -> Result<usize, ToplevelDragError> {
    match kind {
        OfferKind::Private if can_receive_private(phase, facts) => {
            Ok(PRIVATE_FRAME_HEADER + TOPLEVEL_DRAG_MAX_PAYLOAD)
        }
        OfferKind::File if phase == OfferPhase::Accepted => Ok(MAX_URI_LIST_BYTES),
        _ => Err(ToplevelDragError::InvalidState),
    }
}

fn file_offer_completion_ready(
    phase: OfferPhase,
    facts: (bool, DndAction),
    data_device_v3: bool,
) -> bool {
    let (dropped, selected_action) = facts;
    dropped
        && phase == OfferPhase::Ready
        && (!data_device_v3 || selected_action == DndAction::Copy)
}

pub(crate) struct DragOfferState {
    pub offer: DragOffer,
    pub window_id: WindowId,
    seat_id: ObjectId,
    kind: OfferKind,
    phase: OfferPhase,
    file_paths: Vec<PathBuf>,
    transfer_token: Option<RegistrationToken>,
}

pub(crate) enum DragLoopRequest {
    RegisterOffer {
        offer_id: ToplevelDragOfferId,
        pipe: ReadPipe,
        max_bytes: usize,
    },
    RemoveTransfer(RegistrationToken),
    CleanupWindow(WindowId),
}

pub(crate) struct ToplevelDragBackend {
    pub data_device_manager: Option<Arc<DataDeviceManagerState>>,
    data_device_v3: bool,
    pub seats: AHashMap<ObjectId, DragSeat>,
    pub sources: AHashMap<ToplevelDragId, DragSourceState>,
    source_ids: AHashMap<ObjectId, ToplevelDragId>,
    pub offers: AHashMap<ToplevelDragOfferId, DragOfferState>,
    offer_ids: AHashMap<ObjectId, ToplevelDragOfferId>,
    pub frame_drag_grabs: AHashMap<(WindowId, ObjectId), FrameDragGrab>,
    destroyed_windows: AHashSet<WindowId>,
    loop_requests: Vec<DragLoopRequest>,
    next_id: u64,
}

impl ToplevelDragBackend {
    pub fn new(globals: &GlobalList, qh: &QueueHandle<WinitState>) -> Self {
        let data_device_manager =
            DataDeviceManagerState::bind(globals, qh).ok().map(Arc::new);
        let data_device_v3 = data_device_manager
            .as_ref()
            .is_some_and(|manager| manager.data_device_manager().version() >= 3);
        Self {
            data_device_manager,
            data_device_v3,
            seats: AHashMap::default(),
            sources: AHashMap::default(),
            source_ids: AHashMap::default(),
            offers: AHashMap::default(),
            offer_ids: AHashMap::default(),
            frame_drag_grabs: AHashMap::default(),
            destroyed_windows: AHashSet::default(),
            loop_requests: Vec::new(),
            next_id: 1,
        }
    }

    pub fn supported(&self) -> bool {
        self.data_device_v3
    }

    pub fn register_seat(&mut self, qh: &QueueHandle<WinitState>, seat: &WlSeat) {
        let Some(manager) = self.data_device_manager.as_ref() else {
            return;
        };
        self.seats.insert(
            seat.id(),
            DragSeat {
                data_device: Arc::new(manager.get_data_device(qh, seat)),
                pointer: None,
            },
        );
    }

    fn allocate_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("toplevel drag ID overflow");
        id
    }

    pub fn allocate_drag_id(&mut self) -> ToplevelDragId {
        ToplevelDragId::from_raw(self.allocate_id())
    }

    fn allocate_offer_id(&mut self) -> ToplevelDragOfferId {
        ToplevelDragOfferId::from_raw(self.allocate_id())
    }

    pub fn insert_source(&mut self, drag_id: ToplevelDragId, source: DragSourceState) {
        self.source_ids.insert(source.source.inner().id(), drag_id);
        assert!(self.sources.insert(drag_id, source).is_none());
    }

    pub fn remember_frame_drag(&mut self, window_id: WindowId, grab: FrameDragGrab) {
        self.frame_drag_grabs
            .insert((window_id, grab.seat_id.clone()), grab);
    }

    pub fn forget_frame_drag(&mut self, window_id: WindowId) {
        self.frame_drag_grabs.retain(|(id, _), _| *id != window_id);
    }

    pub fn forget_frame_drag_for_pointer(
        &mut self,
        seat_id: &ObjectId,
        pointer_id: ObjectId,
    ) -> Option<WindowId> {
        let window_id =
            self.frame_drag_grabs
                .iter()
                .find_map(|((window_id, grab_seat), grab)| {
                    (grab_seat == seat_id && grab.pointer_id == pointer_id)
                        .then_some(*window_id)
                });
        self.frame_drag_grabs.retain(|(_, grab_seat), grab| {
            grab_seat != seat_id || grab.pointer_id != pointer_id
        });
        window_id
    }

    pub fn has_frame_drag(
        &self,
        window_id: WindowId,
        seat_id: &ObjectId,
        pointer_id: ObjectId,
    ) -> bool {
        self.frame_drag_grabs
            .get(&(window_id, seat_id.clone()))
            .is_some_and(|grab| grab.pointer_id == pointer_id)
    }

    pub fn cancel_source(
        &mut self,
        drag_id: ToplevelDragId,
        owner: WindowId,
    ) -> Result<(), ToplevelDragError> {
        let source = self
            .sources
            .get(&drag_id)
            .ok_or(ToplevelDragError::InvalidDrag)?;
        if source.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        let source = self
            .sources
            .remove(&drag_id)
            .expect("validated source must remain active");
        self.source_ids.remove(&source.source.inner().id());
        self.loop_requests.extend(
            source
                .writer_tokens
                .into_iter()
                .map(DragLoopRequest::RemoveTransfer),
        );
        drop(source.source);
        Ok(())
    }

    pub fn accept_offer(
        &mut self,
        offer_id: ToplevelDragOfferId,
        owner: WindowId,
    ) -> Result<(), ToplevelDragError> {
        let offer = self
            .offers
            .get_mut(&offer_id)
            .ok_or(ToplevelDragError::InvalidOffer)?;
        if offer.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        if offer.phase == OfferPhase::Accepted {
            return Ok(());
        }
        if !can_decide_private_offer(offer.kind, offer.offer.left, offer.offer.dropped) {
            return Err(ToplevelDragError::InvalidState);
        }
        offer.offer.accept_mime_type(
            offer.offer.serial,
            Some(TOPLEVEL_DRAG_MIME_TYPE.to_owned()),
        );
        offer.offer.set_actions(DndAction::Move, DndAction::Move);
        offer.phase = OfferPhase::Accepted;
        Ok(())
    }

    pub fn reject_offer(
        &mut self,
        offer_id: ToplevelDragOfferId,
        owner: WindowId,
    ) -> Result<(), ToplevelDragError> {
        let offer = self
            .offers
            .get_mut(&offer_id)
            .ok_or(ToplevelDragError::InvalidOffer)?;
        if offer.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        if !can_decide_private_offer(offer.kind, offer.offer.left, offer.offer.dropped) {
            return Err(ToplevelDragError::InvalidState);
        }
        offer.offer.accept_mime_type(offer.offer.serial, None);
        offer
            .offer
            .set_actions(DndAction::empty(), DndAction::empty());
        offer.phase = OfferPhase::Undecided;
        Ok(())
    }

    pub fn finish_offer(
        &mut self,
        offer_id: ToplevelDragOfferId,
        owner: WindowId,
    ) -> Result<DragOffer, ToplevelDragError> {
        let offer = self
            .offers
            .get(&offer_id)
            .ok_or(ToplevelDragError::InvalidOffer)?;
        if offer.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        if offer.kind != OfferKind::Private
            || !can_finish(
                offer.phase,
                (offer.offer.dropped, offer.offer.selected_action),
            )
        {
            return Err(ToplevelDragError::InvalidState);
        }
        let offer = self
            .remove_offer(offer_id)
            .expect("validated offer must remain active");
        if let Some(token) = offer.transfer_token {
            self.loop_requests
                .push(DragLoopRequest::RemoveTransfer(token));
        }
        Ok(offer.offer)
    }

    pub fn cancel_offer(
        &mut self,
        offer_id: ToplevelDragOfferId,
        owner: WindowId,
    ) -> Result<DragOffer, ToplevelDragError> {
        let offer = self
            .offers
            .get(&offer_id)
            .ok_or(ToplevelDragError::InvalidOffer)?;
        if offer.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        if offer.kind != OfferKind::Private {
            return Err(ToplevelDragError::InvalidState);
        }
        let offer = self
            .remove_offer(offer_id)
            .expect("validated offer must remain active");
        if let Some(token) = offer.transfer_token {
            self.loop_requests
                .push(DragLoopRequest::RemoveTransfer(token));
        }
        Ok(offer.offer)
    }

    pub fn cleanup_window(
        &mut self,
        window_id: WindowId,
    ) -> (
        Vec<RegistrationToken>,
        Vec<CancelledSource>,
        Vec<CancelledOffer>,
    ) {
        self.frame_drag_grabs.retain(|(id, _), _| *id != window_id);
        let mut transfer_tokens: Vec<_> = self
            .sources
            .values_mut()
            .filter(|source| source.window_id == window_id)
            .flat_map(|source| source.writer_tokens.drain(..))
            .collect();

        let source_ids: Vec<_> = self
            .sources
            .iter()
            .filter_map(|(id, source)| (source.window_id == window_id).then_some(*id))
            .collect();
        let mut cancelled_sources = Vec::with_capacity(source_ids.len());
        for source_id in source_ids {
            if let Some(source) = self.sources.remove(&source_id) {
                self.source_ids.remove(&source.source.inner().id());
                cancelled_sources.push((source_id, source.window_id));
                drop(source.source);
            }
        }

        let offer_ids: Vec<_> = self
            .offers
            .iter()
            .filter_map(|(id, offer)| (offer.window_id == window_id).then_some(*id))
            .collect();
        let mut cancelled_offers = Vec::with_capacity(offer_ids.len());
        for offer_id in offer_ids {
            if let Some(offer) = self.remove_offer(offer_id) {
                if let Some(token) = offer.transfer_token {
                    transfer_tokens.push(token);
                }
                cancelled_offers.push((
                    offer_id,
                    offer.window_id,
                    offer.kind,
                    offer.phase == OfferPhase::Ready && !offer.offer.dropped,
                ));
                offer.offer.destroy();
            }
        }
        (transfer_tokens, cancelled_sources, cancelled_offers)
    }

    pub fn cancel_sources_for_seat(
        &mut self,
        seat_id: &ObjectId,
    ) -> Vec<CancelledSource> {
        self.frame_drag_grabs.retain(|(_, id), _| id != seat_id);
        let writer_tokens: Vec<_> = self
            .sources
            .values_mut()
            .filter(|source| &source.seat_id == seat_id)
            .flat_map(|source| source.writer_tokens.drain(..))
            .collect();
        self.loop_requests.extend(
            writer_tokens
                .into_iter()
                .map(DragLoopRequest::RemoveTransfer),
        );

        let source_ids: Vec<_> = self
            .sources
            .iter()
            .filter_map(|(id, source)| (&source.seat_id == seat_id).then_some(*id))
            .collect();
        let mut cancelled_sources = Vec::with_capacity(source_ids.len());
        for source_id in source_ids {
            let source = self
                .sources
                .remove(&source_id)
                .expect("selected source must remain active");
            self.source_ids.remove(&source.source.inner().id());
            cancelled_sources.push((source_id, source.window_id));
            drop(source.source);
        }
        cancelled_sources
    }

    pub(crate) fn cancel_offers_for_seat(
        &mut self,
        seat_id: &ObjectId,
    ) -> Vec<CancelledOffer> {
        let offer_ids: Vec<_> = self
            .offers
            .iter()
            .filter_map(|(id, offer)| (&offer.seat_id == seat_id).then_some(*id))
            .collect();
        let mut cancelled_offers = Vec::with_capacity(offer_ids.len());
        for offer_id in offer_ids {
            if let Some(offer) = self.remove_offer(offer_id) {
                if let Some(token) = offer.transfer_token {
                    self.loop_requests
                        .push(DragLoopRequest::RemoveTransfer(token));
                }
                cancelled_offers.push((
                    offer_id,
                    offer.window_id,
                    offer.kind,
                    offer.phase == OfferPhase::Ready && !offer.offer.dropped,
                ));
                offer.offer.destroy();
            }
        }
        cancelled_offers
    }

    fn remove_drag_seat(
        &mut self,
        seat_id: &ObjectId,
    ) -> (Vec<CancelledOffer>, Vec<CancelledSource>) {
        let cancelled_sources = self.cancel_sources_for_seat(seat_id);
        let _ = self.seats.remove(seat_id);
        let cancelled_offers = self.cancel_offers_for_seat(seat_id);
        (cancelled_offers, cancelled_sources)
    }

    fn remove_offer(&mut self, offer_id: ToplevelDragOfferId) -> Option<DragOfferState> {
        self.offer_ids.retain(|_, id| *id != offer_id);
        self.offers.remove(&offer_id)
    }

    pub fn queue_offer_transfer(
        &mut self,
        offer_id: ToplevelDragOfferId,
        owner: WindowId,
    ) -> Result<(), ToplevelDragError> {
        let offer = self
            .offers
            .get_mut(&offer_id)
            .ok_or(ToplevelDragError::InvalidOffer)?;
        if offer.window_id != owner {
            return Err(ToplevelDragError::WrongOwner);
        }
        let max_bytes = offer_receive_limit(
            offer.kind,
            offer.phase,
            (
                offer.offer.dropped,
                offer.offer.source_actions,
                offer.offer.selected_action,
            ),
        )?;
        let pipe = offer
            .offer
            .receive(
                match offer.kind {
                    OfferKind::Private => TOPLEVEL_DRAG_MIME_TYPE,
                    OfferKind::File => URI_LIST_MIME_TYPE,
                }
                .to_owned(),
            )
            .map_err(ToplevelDragError::Io)?;
        offer.phase = OfferPhase::Receiving;
        self.loop_requests.push(DragLoopRequest::RegisterOffer {
            offer_id,
            pipe,
            max_bytes,
        });
        Ok(())
    }

    pub fn mark_window_destroyed(&mut self, window_id: WindowId) {
        self.destroyed_windows.insert(window_id);
        self.loop_requests
            .push(DragLoopRequest::CleanupWindow(window_id));
    }

    pub(crate) fn forget_destroyed_window(&mut self, window_id: WindowId) {
        self.destroyed_windows.remove(&window_id);
    }

    fn source_owner_is_live(&self, drag_id: ToplevelDragId) -> bool {
        self.sources
            .get(&drag_id)
            .is_some_and(|source| !self.destroyed_windows.contains(&source.window_id))
    }

    fn take_loop_requests(&mut self) -> Vec<DragLoopRequest> {
        mem::take(&mut self.loop_requests)
    }
}

pub(crate) fn frame_private_payload(data: &[u8]) -> Result<Arc<[u8]>, ToplevelDragError> {
    if data.len() > TOPLEVEL_DRAG_MAX_PAYLOAD {
        return Err(ToplevelDragError::PayloadTooLarge);
    }
    let mut framed = Vec::with_capacity(PRIVATE_FRAME_HEADER + data.len());
    framed.extend_from_slice(PRIVATE_FRAME_MAGIC);
    framed.extend_from_slice(&(data.len() as u32).to_be_bytes());
    framed.extend_from_slice(data);
    Ok(framed.into())
}

fn unframe_private_payload(data: &[u8]) -> Option<Vec<u8>> {
    if data.len() < PRIVATE_FRAME_HEADER
        || &data[..PRIVATE_FRAME_MAGIC.len()] != PRIVATE_FRAME_MAGIC
    {
        return None;
    }
    let length = u32::from_be_bytes(
        data[PRIVATE_FRAME_MAGIC.len()..PRIVATE_FRAME_HEADER]
            .try_into()
            .ok()?,
    ) as usize;
    if length > TOPLEVEL_DRAG_MAX_PAYLOAD || data.len() != PRIVATE_FRAME_HEADER + length {
        return None;
    }
    Some(data[PRIVATE_FRAME_HEADER..].to_vec())
}

fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    let Ok(text) = std::str::from_utf8(data) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end_matches('\r');
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let rest = line.strip_prefix("file://")?;
            let path = if rest.starts_with('/') {
                Cow::Borrowed(rest)
            } else {
                let (authority, path) = rest.split_once('/')?;
                if !authority.eq_ignore_ascii_case("localhost") {
                    return None;
                }
                Cow::Owned(format!("/{path}"))
            };
            let path = percent_decode_str(&path).decode_utf8().ok()?;
            let path = PathBuf::from(path.as_ref());
            path.is_absolute().then_some(path)
        })
        .collect()
}

impl WinitState {
    fn push_drag_event(&mut self, window_id: WindowId, event: ToplevelDragEvent) {
        if self
            .toplevel_drag
            .lock()
            .unwrap()
            .destroyed_windows
            .contains(&window_id)
        {
            return;
        }
        self.events_sink
            .push_window_event(WindowEvent::ToplevelDrag(event), window_id);
        self.dispatched_events = true;
    }

    fn end_source(
        &mut self,
        source: &WlDataSource,
    ) -> Option<(ToplevelDragId, WindowId)> {
        let source_id = self
            .toplevel_drag
            .lock()
            .unwrap()
            .source_ids
            .get(&source.id())
            .copied()?;
        self.end_source_by_id(source_id)
    }

    fn end_source_by_id(
        &mut self,
        source_id: ToplevelDragId,
    ) -> Option<(ToplevelDragId, WindowId)> {
        let mut backend = self.toplevel_drag.lock().unwrap();
        let source_object_id = backend
            .sources
            .get(&source_id)
            .map(|source| source.source.inner().id())?;
        backend.source_ids.remove(&source_object_id);
        let mut source = backend
            .sources
            .remove(&source_id)
            .expect("source index must refer to an active source");
        for token in source.writer_tokens.drain(..) {
            self.loop_handle.remove(token);
        }
        let window_id = source.window_id;
        drop(source.source);
        Some((source_id, window_id))
    }

    fn complete_offer_transfer(&mut self, offer_id: ToplevelDragOfferId, bytes: Vec<u8>) {
        enum Completion {
            Private(WindowId, Vec<u8>),
            File(WindowId, Vec<PathBuf>, bool),
            Failed,
        }

        let completion = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            let Some(offer) = backend.offers.get_mut(&offer_id) else {
                return;
            };
            offer.transfer_token = None;
            match offer.kind {
                OfferKind::Private => match unframe_private_payload(&bytes) {
                    Some(data) => {
                        offer.phase = OfferPhase::Ready;
                        Completion::Private(offer.window_id, data)
                    }
                    None => Completion::Failed,
                },
                OfferKind::File => {
                    let paths = parse_uri_list(&bytes);
                    if paths.is_empty() {
                        Completion::Failed
                    } else {
                        offer.phase = OfferPhase::Ready;
                        offer.file_paths = paths.clone();
                        let dropped = offer.offer.dropped;
                        Completion::File(offer.window_id, paths, dropped)
                    }
                }
            }
        };

        match completion {
            Completion::Private(window_id, data) => self.push_drag_event(
                window_id,
                ToplevelDragEvent::DataReady { offer_id, data },
            ),
            Completion::File(window_id, paths, dropped) => {
                if dropped {
                    self.maybe_finish_file_offer(offer_id);
                } else {
                    for path in paths {
                        self.events_sink
                            .push_window_event(WindowEvent::HoveredFile(path), window_id);
                    }
                    self.dispatched_events = true;
                }
            }
            Completion::Failed => self.fail_offer_transfer(offer_id),
        }
    }

    fn fail_offer_transfer(&mut self, offer_id: ToplevelDragOfferId) {
        let failed = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            if let Some(offer) = backend.offers.get_mut(&offer_id) {
                // The active callback removes itself with PostAction::Remove.
                offer.transfer_token = None;
            }
            backend.remove_offer(offer_id)
        };
        let Some(offer) = failed else {
            return;
        };
        offer.offer.destroy();
        match offer.kind {
            OfferKind::Private => self.push_drag_event(
                offer.window_id,
                ToplevelDragEvent::OfferDataFailed { offer_id },
            ),
            OfferKind::File => {}
        }
    }

    fn finish_file_offer(&mut self, offer_id: ToplevelDragOfferId) {
        let mut backend = self.toplevel_drag.lock().unwrap();
        if let Some(offer) = backend.remove_offer(offer_id) {
            offer.offer.finish();
            offer.offer.destroy();
        }
    }

    fn maybe_finish_file_offer(&mut self, offer_id: ToplevelDragOfferId) {
        let ready = {
            let backend = self.toplevel_drag.lock().unwrap();
            backend.offers.get(&offer_id).and_then(|offer| {
                (offer.kind == OfferKind::File
                    && file_offer_completion_ready(
                        offer.phase,
                        (offer.offer.dropped, offer.offer.selected_action),
                        backend.data_device_v3,
                    ))
                .then(|| (offer.window_id, offer.file_paths.clone()))
            })
        };
        let Some((window_id, paths)) = ready else {
            return;
        };
        for path in paths {
            self.events_sink
                .push_window_event(WindowEvent::DroppedFile(path), window_id);
        }
        self.dispatched_events = true;
        self.finish_file_offer(offer_id);
    }

    fn cancel_file_offer(&mut self, offer_id: ToplevelDragOfferId) {
        if let Some(offer) = self.toplevel_drag.lock().unwrap().remove_offer(offer_id) {
            if let Some(token) = offer.transfer_token {
                self.loop_handle.remove(token);
            }
            offer.offer.destroy();
        }
    }

    pub(crate) fn process_drag_loop_requests(&mut self) {
        let requests = self.toplevel_drag.lock().unwrap().take_loop_requests();
        for request in requests {
            match request {
                DragLoopRequest::RegisterOffer {
                    offer_id,
                    pipe,
                    max_bytes,
                } => {
                    if !register_offer_transfer(
                        &self.toplevel_drag,
                        &self.loop_handle,
                        offer_id,
                        pipe,
                        max_bytes,
                    ) {
                        self.fail_offer_transfer(offer_id);
                    }
                }
                DragLoopRequest::RemoveTransfer(token) => self.loop_handle.remove(token),
                DragLoopRequest::CleanupWindow(window_id) => {
                    let (tokens, cancelled_sources, cancelled_offers) =
                        self.toplevel_drag.lock().unwrap().cleanup_window(window_id);
                    for token in tokens {
                        self.loop_handle.remove(token);
                    }
                    for source in cancelled_sources {
                        self.notify_cancelled_source(source);
                    }
                    for offer in cancelled_offers {
                        self.notify_cancelled_offer(offer);
                    }
                }
            }
        }
    }

    pub(crate) fn remove_drag_seat(&mut self, seat_id: &ObjectId) {
        let (offers, cancelled_sources) =
            self.toplevel_drag.lock().unwrap().remove_drag_seat(seat_id);
        self.process_drag_loop_requests();

        for source in cancelled_sources {
            self.notify_cancelled_source(source);
        }
        for offer in offers {
            self.notify_cancelled_offer(offer);
        }
    }

    pub(crate) fn notify_cancelled_source(
        &mut self,
        (drag_id, window_id): CancelledSource,
    ) {
        self.push_source_event(window_id, ToplevelDragEvent::Cancelled { drag_id });
    }

    fn push_source_event(&mut self, window_id: WindowId, event: ToplevelDragEvent) {
        self.events_sink
            .push_window_event(WindowEvent::ToplevelDrag(event), window_id);
        self.dispatched_events = true;
    }

    pub(crate) fn notify_cancelled_offer(
        &mut self,
        (offer_id, window_id, kind, hover_emitted): CancelledOffer,
    ) {
        if kind == OfferKind::Private {
            self.push_source_event(
                window_id,
                ToplevelDragEvent::OfferCancelled { offer_id },
            );
        } else if hover_emitted
            && self.windows.borrow().contains_key(&window_id)
            && !self
                .toplevel_drag
                .lock()
                .unwrap()
                .destroyed_windows
                .contains(&window_id)
        {
            self.events_sink
                .push_window_event(WindowEvent::HoveredFileCancelled, window_id);
            self.dispatched_events = true;
        }
    }
}

fn register_offer_transfer(
    backend: &Arc<Mutex<ToplevelDragBackend>>,
    loop_handle: &calloop::LoopHandle<'static, WinitState>,
    offer_id: ToplevelDragOfferId,
    pipe: ReadPipe,
    max_bytes: usize,
) -> bool {
    let mut bytes = Vec::new();
    let result = loop_handle.insert_source(pipe, move |_, pipe, state| {
        let file = unsafe { pipe.get_mut() };
        let mut chunk = [0; 8192];
        loop {
            match file.read(&mut chunk) {
                Ok(0) => {
                    state.complete_offer_transfer(offer_id, mem::take(&mut bytes));
                    return PostAction::Remove;
                }
                Ok(count) if bytes.len() + count <= max_bytes => {
                    bytes.extend_from_slice(&chunk[..count]);
                }
                Ok(_) => {
                    state.fail_offer_transfer(offer_id);
                    return PostAction::Remove;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    return PostAction::Continue;
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    state.fail_offer_transfer(offer_id);
                    return PostAction::Remove;
                }
            }
        }
    });
    match result {
        Ok(token) => {
            if let Some(offer) = backend.lock().unwrap().offers.get_mut(&offer_id) {
                offer.transfer_token = Some(token);
            } else {
                loop_handle.remove(token);
            }
            true
        }
        Err(error) => {
            tracing::warn!(
                "failed to register Wayland offer transfer: {}",
                io::Error::other(error.error)
            );
            false
        }
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let replaced = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            backend
                .offer_ids
                .get(&data_device.id())
                .copied()
                .map(|offer_id| {
                    let offer = backend
                        .remove_offer(offer_id)
                        .expect("offer index must refer to an active offer");
                    (
                        offer_id,
                        offer.window_id,
                        offer.kind,
                        offer.phase == OfferPhase::Ready && !offer.offer.dropped,
                        offer.transfer_token,
                    )
                })
        };
        if let Some((offer_id, window_id, kind, hover_emitted, token)) = replaced {
            if let Some(token) = token {
                self.loop_handle.remove(token);
            }
            // SCTK destroys the previous protocol offer before this callback.
            self.notify_cancelled_offer((offer_id, window_id, kind, hover_emitted));
        }

        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };
        let Some(mut offer) = data.drag_offer() else {
            return;
        };
        let seat_id = data.seat().id();
        if !self
            .toplevel_drag
            .lock()
            .unwrap()
            .seats
            .contains_key(&seat_id)
        {
            tracing::warn!("received drag enter for an unknown data device");
            offer.destroy();
            return;
        }
        let supports_private = self.toplevel_drag.lock().unwrap().supported();
        let kind = offer.with_mime_types(|mimes| {
            if supports_private
                && mimes.iter().any(|mime| mime == TOPLEVEL_DRAG_MIME_TYPE)
            {
                Some(OfferKind::Private)
            } else if mimes.iter().any(|mime| mime == URI_LIST_MIME_TYPE) {
                Some(OfferKind::File)
            } else {
                None
            }
        });
        let Some(kind) = kind else {
            offer.destroy();
            return;
        };

        let window_id = make_wid(&root_surface(surface));
        if !self.windows.borrow().contains_key(&window_id) {
            offer.destroy();
            return;
        }
        let stale_offers = self
            .toplevel_drag
            .lock()
            .unwrap()
            .cancel_offers_for_seat(&seat_id);
        self.process_drag_loop_requests();
        for stale_offer in stale_offers {
            self.notify_cancelled_offer(stale_offer);
        }
        let (offer_id, source_actions) = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            let offer_id = backend.allocate_offer_id();
            let phase = if kind == OfferKind::File {
                offer.accept_mime_type(offer.serial, Some(URI_LIST_MIME_TYPE.to_owned()));
                if backend.data_device_v3 {
                    offer.set_actions(DndAction::Copy, DndAction::Copy);
                } else {
                    offer.source_actions = DndAction::Copy;
                    offer.selected_action = DndAction::Copy;
                }
                OfferPhase::Accepted
            } else {
                // Accept before returning to the application event loop. Wayland
                // may deliver enter and drop in one dispatch cycle, while the
                // application only sees queued window events afterward.
                offer.accept_mime_type(
                    offer.serial,
                    Some(TOPLEVEL_DRAG_MIME_TYPE.to_owned()),
                );
                offer.set_actions(DndAction::Move, DndAction::Move);
                OfferPhase::Accepted
            };
            let source_actions = offer.source_actions;
            backend.offer_ids.insert(data_device.id(), offer_id);
            backend.offers.insert(
                offer_id,
                DragOfferState {
                    offer,
                    window_id,
                    seat_id,
                    kind,
                    phase,
                    file_paths: Vec::new(),
                    transfer_token: None,
                },
            );
            (offer_id, source_actions)
        };

        if kind == OfferKind::Private {
            self.push_drag_event(
                window_id,
                ToplevelDragEvent::Entered {
                    offer_id,
                    position: LogicalPosition::new(x, y),
                },
            );
            // SCTK reports an empty initial bitset before the source actions
            // event arrives; only later action updates are authoritative.
            if !source_actions.is_empty() {
                self.push_drag_event(
                    window_id,
                    ToplevelDragEvent::SourceActionsChanged {
                        offer_id,
                        move_supported: source_actions.contains(DndAction::Move),
                    },
                );
            }
        } else {
            let queued = self
                .toplevel_drag
                .lock()
                .unwrap()
                .queue_offer_transfer(offer_id, window_id);
            match queued {
                Ok(()) => self.process_drag_loop_requests(),
                Err(error) => {
                    tracing::warn!("failed to receive Wayland file offer: {error}");
                    self.cancel_file_offer(offer_id);
                }
            }
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let current = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            let Some(id) = backend.offer_ids.remove(&data_device.id()) else {
                return;
            };
            let offer = backend
                .offers
                .get_mut(&id)
                .expect("offer index must refer to an active offer");
            offer.offer.left = true;
            let info = (
                id,
                offer.window_id,
                offer.kind,
                offer.offer.dropped,
                offer.phase == OfferPhase::Ready && !offer.offer.dropped,
            );
            let removed = (!offer.offer.dropped)
                .then(|| backend.remove_offer(id).expect("offer must remain active"));
            (info, removed)
        };
        let ((offer_id, window_id, kind, dropped, hover_emitted), removed) = current;
        if let Some(offer) = removed {
            if let Some(token) = offer.transfer_token {
                self.loop_handle.remove(token);
            }
        }
        if kind == OfferKind::Private {
            self.push_drag_event(window_id, ToplevelDragEvent::Left { offer_id });
        } else if !dropped && hover_emitted {
            self.events_sink
                .push_window_event(WindowEvent::HoveredFileCancelled, window_id);
            self.dispatched_events = true;
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let current = {
            let backend = self.toplevel_drag.lock().unwrap();
            backend.offer_ids.get(&data_device.id()).and_then(|id| {
                backend
                    .offers
                    .get(id)
                    .map(|offer| (*id, offer.window_id, offer.kind))
            })
        };
        if let Some((offer_id, window_id, OfferKind::Private)) = current {
            self.push_drag_event(
                window_id,
                ToplevelDragEvent::Motion {
                    offer_id,
                    position: LogicalPosition::new(x, y),
                },
            );
        }
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let current = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            let Some(offer_id) = backend.offer_ids.get(&data_device.id()).copied() else {
                return;
            };
            let offer = backend
                .offers
                .get_mut(&offer_id)
                .expect("offer index must refer to an active offer");
            offer.offer.dropped = true;
            (
                offer_id,
                offer.window_id,
                offer.kind,
                offer.phase == OfferPhase::Ready,
                offer.offer.selected_action,
            )
        };
        match current {
            (offer_id, window_id, OfferKind::Private, _, _) => {
                self.push_drag_event(window_id, ToplevelDragEvent::Dropped { offer_id })
            }
            (offer_id, _, OfferKind::File, true, DndAction::Copy) => {
                self.maybe_finish_file_offer(offer_id)
            }
            (offer_id, _, OfferKind::File, _, action) if !action.is_empty() => {
                self.cancel_file_offer(offer_id)
            }
            _ => {}
        }
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        changed: &mut DragOffer,
        actions: DndAction,
    ) {
        let info = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            backend.offers.iter_mut().find_map(|(id, offer)| {
                (offer.offer == *changed).then(|| {
                    offer.offer.source_actions = actions;
                    (*id, offer.window_id, offer.kind)
                })
            })
        };
        if let Some((offer_id, window_id, OfferKind::Private)) = info {
            self.push_drag_event(
                window_id,
                ToplevelDragEvent::SourceActionsChanged {
                    offer_id,
                    move_supported: actions.contains(DndAction::Move),
                },
            );
        }
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        changed: &mut DragOffer,
        action: DndAction,
    ) {
        let info = {
            let mut backend = self.toplevel_drag.lock().unwrap();
            backend.offers.iter_mut().find_map(|(id, offer)| {
                (offer.offer == *changed).then(|| {
                    offer.offer.selected_action = action;
                    (*id, offer.window_id, offer.kind)
                })
            })
        };
        match info {
            Some((offer_id, window_id, OfferKind::Private)) => self.push_drag_event(
                window_id,
                ToplevelDragEvent::SelectedActionChanged {
                    offer_id,
                    selected_move: action == DndAction::Move,
                },
            ),
            Some((offer_id, _, OfferKind::File)) if action == DndAction::Copy => {
                self.maybe_finish_file_offer(offer_id)
            }
            Some((offer_id, _, OfferKind::File)) => {
                let dropped = self
                    .toplevel_drag
                    .lock()
                    .unwrap()
                    .offers
                    .get(&offer_id)
                    .is_some_and(|offer| offer.offer.dropped);
                if dropped {
                    self.cancel_file_offer(offer_id);
                }
            }
            None => {}
        }
    }
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: WritePipe,
    ) {
        if mime != TOPLEVEL_DRAG_MIME_TYPE {
            return;
        }
        let Some((drag_id, data)) = ({
            let backend = self.toplevel_drag.lock().unwrap();
            backend.source_ids.get(&source.id()).and_then(|id| {
                backend
                    .sources
                    .get(id)
                    .map(|source| (*id, source.framed_data.clone()))
            })
        }) else {
            return;
        };

        let mut written = 0;
        let token_slot = Rc::new(Cell::new(None));
        let callback_token = token_slot.clone();
        let result = self.loop_handle.insert_source(pipe, move |_, pipe, state| {
            let owner_is_live = state
                .toplevel_drag
                .lock()
                .unwrap()
                .source_owner_is_live(drag_id);
            if !owner_is_live {
                clear_source_writer_token(state, drag_id, &callback_token);
                if let Some(source) = state.end_source_by_id(drag_id) {
                    state.notify_cancelled_source(source);
                }
                return PostAction::Remove;
            }
            let file = unsafe { pipe.get_mut() };
            while written < data.len() {
                match file.write(&data[written..]) {
                    Ok(0) => {
                        clear_source_writer_token(state, drag_id, &callback_token);
                        if let Some(source) = state.end_source_by_id(drag_id) {
                            state.notify_cancelled_source(source);
                        }
                        return PostAction::Remove;
                    }
                    Ok(count) => written += count,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        return PostAction::Continue;
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => {
                        clear_source_writer_token(state, drag_id, &callback_token);
                        if let Some(source) = state.end_source_by_id(drag_id) {
                            state.notify_cancelled_source(source);
                        }
                        return PostAction::Remove;
                    }
                }
            }
            clear_source_writer_token(state, drag_id, &callback_token);
            PostAction::Remove
        });
        match result {
            Ok(token) => {
                token_slot.set(Some(token));
                let mut backend = self.toplevel_drag.lock().unwrap();
                if let Some(source) = backend.sources.get_mut(&drag_id) {
                    source.writer_tokens.push(token);
                } else {
                    self.loop_handle.remove(token);
                }
            }
            Err(error) => {
                tracing::warn!(
                    "failed to register toplevel drag writer: {}",
                    error.error
                );
                if let Some(source) = self.end_source_by_id(drag_id) {
                    self.notify_cancelled_source(source);
                }
            }
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        if let Some((drag_id, window_id)) = self.end_source(source) {
            self.push_source_event(window_id, ToplevelDragEvent::Cancelled { drag_id });
        }
    }

    fn dnd_dropped(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        let mut backend = self.toplevel_drag.lock().unwrap();
        let Some(id) = backend.source_ids.get(&source.id()).copied() else {
            return;
        };
        let source = backend
            .sources
            .get_mut(&id)
            .expect("source index must refer to an active source");
        if !matches!(source.phase, SourcePhase::Started) {
            tracing::warn!("received drop for a source that was not started");
            return;
        }
        source.phase = SourcePhase::DropPerformed;
    }

    fn dnd_finished(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
    ) {
        if let Some((drag_id, window_id)) = self.end_source(source) {
            self.push_source_event(window_id, ToplevelDragEvent::Finished { drag_id });
        }
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: DndAction,
    ) {
    }
}

fn clear_source_writer_token(
    state: &mut WinitState,
    drag_id: ToplevelDragId,
    token_slot: &Cell<Option<RegistrationToken>>,
) {
    let Some(token) = token_slot.take() else {
        return;
    };
    if let Some(source) = state
        .toplevel_drag
        .lock()
        .unwrap()
        .sources
        .get_mut(&drag_id)
    {
        source.writer_tokens.retain(|candidate| *candidate != token);
    }
}

sctk::delegate_data_device!(WinitState);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_payload_round_trip_and_bounds() {
        let payload = b"tab:42";
        let framed = frame_private_payload(payload).unwrap();
        assert_eq!(unframe_private_payload(&framed), Some(payload.to_vec()));
        assert!(matches!(
            frame_private_payload(&vec![0; TOPLEVEL_DRAG_MAX_PAYLOAD + 1]),
            Err(ToplevelDragError::PayloadTooLarge)
        ));
    }

    #[test]
    fn private_payload_rejects_bad_frames() {
        assert_eq!(unframe_private_payload(b"short"), None);
        let mut framed = frame_private_payload(b"ok").unwrap().to_vec();
        framed.push(0);
        assert_eq!(unframe_private_payload(&framed), None);
    }

    #[test]
    fn offer_requires_move_drop_and_ready_before_finish() {
        let mut phase = OfferPhase::Accepted;
        let move_facts = (true, DndAction::Move, DndAction::Move);
        assert!(!can_receive_private(
            phase,
            (false, DndAction::Move, DndAction::Move)
        ));
        assert!(can_receive_private(phase, move_facts));
        assert!(!can_finish(phase, (true, DndAction::Move)));
        phase = OfferPhase::Ready;
        assert!(can_finish(phase, (true, DndAction::Move)));
    }

    #[test]
    fn legacy_file_completion_is_implicit_copy_without_finish_negotiation() {
        assert!(file_offer_completion_ready(
            OfferPhase::Ready,
            (true, DndAction::empty()),
            false
        ));
        assert!(!file_offer_completion_ready(
            OfferPhase::Ready,
            (true, DndAction::empty()),
            true
        ));
        assert!(file_offer_completion_ready(
            OfferPhase::Ready,
            (true, DndAction::Copy),
            true
        ));
    }

    #[test]
    fn accepted_idle_file_offer_can_queue_receive_before_drop() {
        let phase = OfferPhase::Accepted;
        let facts = (false, DndAction::empty(), DndAction::empty());
        assert_eq!(
            offer_receive_limit(OfferKind::File, phase, facts).unwrap(),
            MAX_URI_LIST_BYTES
        );

        assert!(
            offer_receive_limit(OfferKind::File, OfferPhase::Receiving, facts).is_err()
        );
    }

    #[test]
    fn private_offer_decision_can_change_until_drop() {
        assert!(can_decide_private_offer(OfferKind::Private, false, false));
        assert!(!can_decide_private_offer(OfferKind::Private, true, false));
        assert!(!can_decide_private_offer(OfferKind::Private, false, true));
        assert!(!can_decide_private_offer(OfferKind::File, false, false));
    }

    #[test]
    fn uri_list_parsing_accepts_local_files_only() {
        let paths = parse_uri_list(
            b"# comment\r\nfile:///tmp/one%20two\r\nhttps://example.com/x\nfile://remote/x\nfile://localhost.evil/tmp/x\nfile://localhost\nfile://LOCALHOST/home/u/a\n",
        );
        assert_eq!(
            paths,
            [PathBuf::from("/tmp/one two"), PathBuf::from("/home/u/a")]
        );
    }
}
