//! State for moving a live tab between Rio windows.
//!
//! The source remains in its current route until the target has received the
//! private drag payload. The application owns the actual grid transfer.

use crate::layout::TabId;
use rio_window::window::WindowId;

pub type PlatformDragId = rio_window::platform::wayland::ToplevelDragId;
pub type PlatformOfferId = rio_window::platform::wayland::ToplevelDragOfferId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SessionToken([u8; 16]);

impl SessionToken {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    #[cfg(test)]
    const fn for_test(byte: u8) -> Self {
        Self([byte; 16])
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerRoute {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub route_ids: Vec<usize>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hover<O> {
    pub offer_id: O,
    pub target_window: WindowId,
    pub index: usize,
    pub dropped: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    Moved,
    Outside,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Lifecycle {
    Preparing,
    Starting,
    Dragging,
    MovingToTarget,
    AwaitingFinish,
    Complete(Outcome),
    Cancelled,
}

#[derive(Debug, Eq, PartialEq)]
pub enum Event<D = PlatformDragId, O = PlatformOfferId> {
    Prepared(D),
    PrepareFailed,
    OwnerStarted {
        drag_id: D,
        owner: OwnerRoute,
    },
    OwnerStartFailed(D),
    SourceActionsChanged {
        offer_id: O,
        move_supported: bool,
    },
    SelectedActionChanged {
        offer_id: O,
        selected_move: bool,
    },
    Enter {
        offer_id: O,
        target_window: WindowId,
        index: usize,
    },
    Motion {
        offer_id: O,
        index: usize,
    },
    Leave(O),
    Drop(O),
    DataReady {
        offer_id: O,
        data: Vec<u8>,
    },
    DataFailed(O),
    OfferCancelled(O),
    SourceFinished(D),
    SourceCancelled(D),
    Detach,
    TargetCommitted,
    TargetRejected,
}

impl<D, O> Event<D, O> {
    fn is_offer_progress(&self) -> bool {
        matches!(
            self,
            Self::SourceActionsChanged { .. }
                | Self::SelectedActionChanged { .. }
                | Self::Enter { .. }
                | Self::Motion { .. }
                | Self::Leave(_)
                | Self::Drop(_)
                | Self::DataReady { .. }
                | Self::DataFailed(_)
                | Self::OfferCancelled(_)
                | Self::TargetRejected
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command<O = PlatformOfferId> {
    PrepareSource {
        payload: Vec<u8>,
        frame_grab: Option<(u32, u32)>,
    },
    StartSourceOwner,
    AcceptOffer,
    RejectOffer {
        offer_id: O,
        target_window: WindowId,
    },
    ReceiveData,
    MoveOwnerToTarget,
    FinishOffer,
    CancelOffer,
    CancelSource,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Transition<D = PlatformDragId, O = PlatformOfferId> {
    pub state: TabDrag<D, O>,
    pub commands: Vec<Command<O>>,
}

#[derive(Debug, Eq, PartialEq)]
pub struct TabDrag<D = PlatformDragId, O = PlatformOfferId> {
    pub token: SessionToken,
    pub source_window: WindowId,
    pub tab_id: TabId,
    pub original_index: usize,
    pub tab_count: usize,
    pub whole_window: bool,
    pub drag_id: Option<D>,
    pub owner: Option<OwnerRoute>,
    pub hover: Option<Hover<O>>,
    pub lifecycle: Lifecycle,
    payload_ready: bool,
    source_finished: bool,
    move_supported: Option<bool>,
    selected_move: Option<bool>,
    frame_grab: Option<(u32, u32)>,
}

impl<D: Copy + Eq, O: Copy + Eq> TabDrag<D, O> {
    pub fn begin(
        source_window: WindowId,
        tab_id: TabId,
        original_index: usize,
        source_tab_count: usize,
    ) -> Result<Transition<D, O>, getrandom::Error> {
        Ok(Self::begin_with_token(
            SessionToken::generate()?,
            source_window,
            tab_id,
            original_index,
            source_tab_count,
        ))
    }

    pub fn begin_with_token(
        token: SessionToken,
        source_window: WindowId,
        tab_id: TabId,
        original_index: usize,
        source_tab_count: usize,
    ) -> Transition<D, O> {
        assert!(source_tab_count > 0, "a tab drag requires a source tab");
        assert!(original_index < source_tab_count, "tab index must exist");
        Self::new(
            token,
            source_window,
            tab_id,
            original_index,
            source_tab_count,
            false,
            None,
        )
    }

    pub fn begin_window(
        source_window: WindowId,
        tab_id: TabId,
        tab_count: usize,
        original_index: usize,
    ) -> Result<Transition<D, O>, getrandom::Error> {
        Ok(Self::begin_window_with_token(
            SessionToken::generate()?,
            source_window,
            tab_id,
            tab_count,
            original_index,
            None,
        ))
    }

    pub fn begin_window_from_frame(
        source_window: WindowId,
        tab_id: TabId,
        tab_count: usize,
        original_index: usize,
        seat_id: u32,
        pointer_id: u32,
    ) -> Result<Transition<D, O>, getrandom::Error> {
        Ok(Self::begin_window_with_token(
            SessionToken::generate()?,
            source_window,
            tab_id,
            tab_count,
            original_index,
            Some((seat_id, pointer_id)),
        ))
    }

    pub fn frame_grab(&self) -> Option<(u32, u32)> {
        self.frame_grab
    }

    fn begin_window_with_token(
        token: SessionToken,
        source_window: WindowId,
        tab_id: TabId,
        tab_count: usize,
        original_index: usize,
        frame_grab: Option<(u32, u32)>,
    ) -> Transition<D, O> {
        assert!(tab_count > 0, "a window drag requires a source tab");
        assert!(original_index < tab_count, "active tab index must exist");
        Self::new(
            token,
            source_window,
            tab_id,
            original_index,
            tab_count,
            true,
            frame_grab,
        )
    }

    fn new(
        token: SessionToken,
        source_window: WindowId,
        tab_id: TabId,
        original_index: usize,
        tab_count: usize,
        whole_window: bool,
        frame_grab: Option<(u32, u32)>,
    ) -> Transition<D, O> {
        Transition {
            state: Self {
                token,
                source_window,
                tab_id,
                original_index,
                tab_count,
                whole_window,
                drag_id: None,
                owner: None,
                hover: None,
                lifecycle: Lifecycle::Preparing,
                payload_ready: false,
                source_finished: false,
                move_supported: None,
                selected_move: None,
                frame_grab,
            },
            commands: vec![Command::PrepareSource {
                payload: token.0.to_vec(),
                frame_grab,
            }],
        }
    }

    pub fn reduce(mut self, event: Event<D, O>) -> Transition<D, O> {
        if matches!(
            self.lifecycle,
            Lifecycle::Complete(_) | Lifecycle::Cancelled
        ) {
            return Transition {
                state: self,
                commands: Vec::new(),
            };
        }
        if self.lifecycle == Lifecycle::AwaitingFinish
            && event.is_offer_progress()
            && !matches!(&event, Event::OfferCancelled(_))
        {
            return Transition {
                state: self,
                commands: Vec::new(),
            };
        }

        let mut commands = Vec::new();
        match event {
            Event::Prepared(drag_id) if self.lifecycle == Lifecycle::Preparing => {
                self.drag_id = Some(drag_id);
                self.lifecycle = Lifecycle::Starting;
                commands.push(Command::StartSourceOwner);
            }
            Event::PrepareFailed if self.lifecycle == Lifecycle::Preparing => {
                self.lifecycle = Lifecycle::Cancelled;
            }
            Event::OwnerStarted { drag_id, owner }
                if self.lifecycle == Lifecycle::Starting
                    && self.drag_id == Some(drag_id) =>
            {
                assert_eq!(
                    owner.window_id, self.source_window,
                    "the live source route must own the drag"
                );
                self.owner = Some(owner);
                self.lifecycle = Lifecycle::Dragging;
            }
            Event::OwnerStartFailed(drag_id) if self.drag_id == Some(drag_id) => {
                commands.push(Command::CancelSource);
                self.lifecycle = Lifecycle::Cancelled;
            }
            Event::SourceActionsChanged {
                offer_id,
                move_supported,
            } if self.hover.map(|hover| hover.offer_id) == Some(offer_id) => {
                self.move_supported = Some(move_supported);
                self.continue_after_drop(&mut commands);
            }
            Event::SelectedActionChanged {
                offer_id,
                selected_move,
            } if self.hover.map(|hover| hover.offer_id) == Some(offer_id) => {
                self.selected_move = Some(selected_move);
                self.continue_after_drop(&mut commands);
            }
            Event::Enter {
                offer_id,
                target_window,
                index,
            } => {
                if self.owner.is_none()
                    || self.hover.is_some_and(|hover| hover.offer_id != offer_id)
                {
                    commands.push(Command::RejectOffer {
                        offer_id,
                        target_window,
                    });
                } else if !self.hover.is_some_and(|hover| hover.dropped) {
                    let changed = self.hover.map_or(true, |hover| {
                        hover.offer_id != offer_id || hover.target_window != target_window
                    });
                    self.hover = Some(Hover {
                        offer_id,
                        target_window,
                        index,
                        dropped: false,
                    });
                    if changed {
                        self.clear_negotiation();
                        commands.push(Command::AcceptOffer);
                    }
                }
            }
            Event::Motion { offer_id, index } => {
                if let Some(hover) = self.hover.as_mut() {
                    if hover.offer_id == offer_id && !hover.dropped {
                        hover.index = index;
                    }
                }
            }
            Event::Leave(offer_id) => {
                if self
                    .hover
                    .is_some_and(|hover| hover.offer_id == offer_id && !hover.dropped)
                {
                    self.clear_hover();
                    commands.push(Command::CancelOffer);
                }
            }
            Event::Drop(offer_id) => {
                if let Some(hover) = self.hover.as_mut() {
                    if hover.offer_id == offer_id && !hover.dropped {
                        hover.dropped = true;
                        self.continue_after_drop(&mut commands);
                    }
                }
            }
            Event::DataReady { offer_id, data }
                if self.hover.map(|hover| hover.offer_id) == Some(offer_id) =>
            {
                if data == self.token.0 {
                    self.payload_ready = true;
                    if self.hover.is_some_and(|hover| hover.dropped) {
                        self.start_target_move(&mut commands);
                    }
                } else {
                    self.reject_negotiation(offer_id, &mut commands);
                }
            }
            Event::DataFailed(offer_id)
                if self.hover.map(|hover| hover.offer_id) == Some(offer_id) =>
            {
                commands.push(Command::CancelOffer);
                commands.push(Command::CancelSource);
                self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
            }
            Event::OfferCancelled(offer_id)
                if self.hover.map(|hover| hover.offer_id) == Some(offer_id) =>
            {
                let dropped = self.hover.is_some_and(|hover| hover.dropped);
                self.clear_hover();
                if self.lifecycle == Lifecycle::AwaitingFinish {
                    self.lifecycle = Lifecycle::Complete(Outcome::Moved);
                } else if dropped {
                    commands.push(Command::CancelSource);
                    self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
                } else {
                    self.lifecycle = Lifecycle::Dragging;
                }
            }
            Event::TargetCommitted if self.lifecycle == Lifecycle::MovingToTarget => {
                commands.push(Command::FinishOffer);
                self.lifecycle = if self.source_finished {
                    Lifecycle::Complete(Outcome::Moved)
                } else {
                    Lifecycle::AwaitingFinish
                };
            }
            Event::TargetRejected => {
                if self.hover.is_some_and(|hover| hover.dropped)
                    || self.lifecycle == Lifecycle::MovingToTarget
                {
                    commands.push(Command::CancelOffer);
                    commands.push(Command::CancelSource);
                    self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
                } else if let Some(hover) = self.hover.take() {
                    self.clear_negotiation();
                    commands.push(Command::RejectOffer {
                        offer_id: hover.offer_id,
                        target_window: hover.target_window,
                    });
                }
            }
            Event::Detach
                if self.lifecycle == Lifecycle::Dragging && !self.whole_window =>
            {
                if self.hover.is_none() {
                    commands.push(Command::CancelSource);
                    self.lifecycle = Lifecycle::Complete(Outcome::Outside);
                }
            }
            Event::SourceFinished(drag_id) if self.drag_id == Some(drag_id) => {
                self.source_finished = true;
                if self.lifecycle == Lifecycle::AwaitingFinish {
                    self.lifecycle = Lifecycle::Complete(Outcome::Moved);
                } else if !self.hover.is_some_and(|hover| hover.dropped) {
                    self.lifecycle = Lifecycle::Complete(Outcome::Outside);
                }
            }
            Event::SourceCancelled(drag_id) if self.drag_id == Some(drag_id) => {
                if self.lifecycle == Lifecycle::AwaitingFinish {
                    self.source_finished = true;
                    self.lifecycle = Lifecycle::Complete(Outcome::Moved);
                } else if self.hover.is_some_and(|hover| hover.dropped) {
                    commands.push(Command::CancelOffer);
                    self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
                } else if !self.whole_window {
                    self.lifecycle = Lifecycle::Complete(Outcome::Outside);
                } else {
                    self.lifecycle = Lifecycle::Cancelled;
                }
            }
            _ => {}
        }
        Transition {
            state: self,
            commands,
        }
    }

    fn start_target_move(&mut self, commands: &mut Vec<Command<O>>) {
        if self.lifecycle == Lifecycle::MovingToTarget {
            return;
        }
        assert!(self.owner.is_some(), "target move requires source owner");
        assert!(self.hover.is_some(), "target move requires target hover");
        self.lifecycle = Lifecycle::MovingToTarget;
        commands.push(Command::MoveOwnerToTarget);
    }

    fn continue_after_drop(&mut self, commands: &mut Vec<Command<O>>) {
        if !self.hover.is_some_and(|hover| hover.dropped) {
            return;
        }
        match (self.move_supported, self.selected_move) {
            (Some(false), _) | (_, Some(false)) => {
                commands.push(Command::CancelOffer);
                commands.push(Command::CancelSource);
                self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
            }
            (Some(true), Some(true)) if self.payload_ready => {
                self.start_target_move(commands);
            }
            (Some(true), Some(true)) => commands.push(Command::ReceiveData),
            _ => {}
        }
    }

    fn reject_negotiation(&mut self, offer_id: O, commands: &mut Vec<Command<O>>) {
        let target_window = self
            .hover
            .expect("negotiated offer requires hover")
            .target_window;
        if self.hover.is_some_and(|hover| hover.dropped) {
            commands.push(Command::CancelOffer);
            commands.push(Command::CancelSource);
            self.lifecycle = Lifecycle::Complete(Outcome::RolledBack);
        } else {
            commands.push(Command::RejectOffer {
                offer_id,
                target_window,
            });
            self.clear_hover();
        }
    }

    fn clear_negotiation(&mut self) {
        self.payload_ready = false;
        self.move_supported = None;
        self.selected_move = None;
    }

    fn clear_hover(&mut self) {
        self.hover = None;
        self.clear_negotiation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Drag = TabDrag<u8, u8>;

    fn begin(tab_count: usize) -> Transition<u8, u8> {
        Drag::begin_with_token(
            SessionToken::for_test(7),
            WindowId::from(11),
            TabId::for_test(23),
            if tab_count == 1 { 0 } else { 1 },
            tab_count,
        )
    }

    fn owner() -> OwnerRoute {
        OwnerRoute {
            window_id: WindowId::from(11),
            tab_id: TabId::for_test(23),
            route_ids: vec![4, 9],
        }
    }

    fn active(tab_count: usize) -> Drag {
        begin(tab_count)
            .state
            .reduce(Event::Prepared(1))
            .state
            .reduce(Event::OwnerStarted {
                drag_id: 1,
                owner: owner(),
            })
            .state
    }

    fn accepted() -> Drag {
        active(2)
            .reduce(Event::Enter {
                offer_id: 2,
                target_window: WindowId::from(22),
                index: 3,
            })
            .state
            .reduce(Event::SourceActionsChanged {
                offer_id: 2,
                move_supported: true,
            })
            .state
            .reduce(Event::SelectedActionChanged {
                offer_id: 2,
                selected_move: true,
            })
            .state
    }

    #[test]
    fn every_source_uses_the_live_route() {
        let prepared = begin(3).state.reduce(Event::Prepared(1));
        assert_eq!(prepared.commands, vec![Command::StartSourceOwner]);
        assert_eq!(prepared.state.lifecycle, Lifecycle::Starting);
        let started = prepared.state.reduce(Event::OwnerStarted {
            drag_id: 1,
            owner: owner(),
        });
        assert_eq!(started.state.lifecycle, Lifecycle::Dragging);
    }

    #[test]
    fn valid_drop_waits_for_data_then_moves() {
        let state = accepted();
        let state = state.reduce(Event::Drop(2)).state;
        assert_eq!(state.lifecycle, Lifecycle::Dragging);
        let state = state
            .reduce(Event::DataReady {
                offer_id: 2,
                data: vec![7; 16],
            })
            .state;
        assert_eq!(state.lifecycle, Lifecycle::MovingToTarget);
    }

    #[test]
    fn invalid_payload_never_moves() {
        let state = accepted().reduce(Event::Drop(2)).state;
        let transition = state.reduce(Event::DataReady {
            offer_id: 2,
            data: vec![0; 16],
        });
        assert_eq!(
            transition.state.lifecycle,
            Lifecycle::Complete(Outcome::RolledBack)
        );
        assert!(matches!(transition.commands[0], Command::CancelOffer));
    }

    #[test]
    fn pending_selected_action_does_not_reject_offer() {
        let state = active(2).reduce(Event::Enter {
            offer_id: 2,
            target_window: WindowId::from(22),
            index: 3,
        });
        let transition = state.state.reduce(Event::SelectedActionChanged {
            offer_id: 2,
            selected_move: false,
        });
        assert!(transition.commands.is_empty());
        assert_eq!(transition.state.lifecycle, Lifecycle::Dragging);
    }

    #[test]
    fn dropped_offer_waits_for_late_action_negotiation() {
        let state = active(2)
            .reduce(Event::Enter {
                offer_id: 2,
                target_window: WindowId::from(22),
                index: 3,
            })
            .state
            .reduce(Event::Drop(2))
            .state;
        assert_eq!(state.lifecycle, Lifecycle::Dragging);

        let state = state
            .reduce(Event::SourceActionsChanged {
                offer_id: 2,
                move_supported: true,
            })
            .state;
        let transition = state.reduce(Event::SelectedActionChanged {
            offer_id: 2,
            selected_move: true,
        });
        assert_eq!(transition.commands, vec![Command::ReceiveData]);
    }

    #[test]
    fn offer_cancellation_after_commit_keeps_move_committed() {
        let state = accepted()
            .reduce(Event::Drop(2))
            .state
            .reduce(Event::DataReady {
                offer_id: 2,
                data: vec![7; 16],
            })
            .state
            .reduce(Event::TargetCommitted)
            .state;
        assert_eq!(state.lifecycle, Lifecycle::AwaitingFinish);
        let state = state.reduce(Event::OfferCancelled(2)).state;
        assert_eq!(state.lifecycle, Lifecycle::Complete(Outcome::Moved));
    }

    #[test]
    fn cancelling_an_unaccepted_tab_drag_detaches_it() {
        let state = active(2).reduce(Event::SourceCancelled(1)).state;
        assert_eq!(state.lifecycle, Lifecycle::Complete(Outcome::Outside));
    }

    #[test]
    fn detaching_without_a_target_cancels_the_source() {
        let transition = active(2).reduce(Event::Detach);
        assert_eq!(transition.commands, vec![Command::CancelSource]);
        assert_eq!(
            transition.state.lifecycle,
            Lifecycle::Complete(Outcome::Outside)
        );
    }
}
