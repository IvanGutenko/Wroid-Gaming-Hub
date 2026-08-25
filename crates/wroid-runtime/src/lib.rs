//! Runtime primitives for low-latency game input.
//!
//! This crate deliberately does not depend on ADB, Waydroid shell commands, a
//! terminal, or a GUI toolkit. It owns the state machine that turns logical
//! touch contact changes into atomic frames for a persistent injection backend.

use std::collections::BTreeMap;
use std::error::Error;

use thiserror::Error;
use wroid_core::Point;

mod joystick;
mod mouse_aim;
mod profile_controls;
mod services;

pub use joystick::{AnalogInput, DirectionalInput, VirtualJoystick, VirtualJoystickConfigError};
pub use mouse_aim::{
    MouseAim, MouseAimConfigError, MouseAimController, MouseAimDelta, MouseAimRegion,
    MouseAimSensitivity, MouseAimSettings, MouseAimUpdate,
};
pub use profile_controls::{
    HostKeyName, HostMouseButton, LayerId, LayerMode, ModifierMask, RuntimeControlAction,
    RuntimeControlBinding, RuntimeControlPlan, RuntimeControlPlanError, RuntimeLayer,
    RuntimePhysicalInput,
};
pub use services::*;

/// Stable identifier for one logical finger on the Android touch surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContactId(u16);

impl ContactId {
    pub const fn new(value: u16) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

/// Lifecycle transition for a touch contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One contact transition within a synchronized touch frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchEvent {
    pub contact_id: ContactId,
    pub phase: TouchPhase,
    pub position: Point,
}

impl TouchEvent {
    pub const fn new(contact_id: ContactId, phase: TouchPhase, position: Point) -> Self {
        Self {
            contact_id,
            phase,
            position,
        }
    }
}

const INLINE_TOUCH_EVENT_CAPACITY: usize = 4;
const EMPTY_TOUCH_EVENT: TouchEvent =
    TouchEvent::new(ContactId::new(0), TouchPhase::Cancel, Point { x: 0, y: 0 });

#[derive(Debug, Clone, PartialEq, Eq)]
enum TouchFrameEvents {
    Inline {
        events: [TouchEvent; INLINE_TOUCH_EVENT_CAPACITY],
        len: u8,
    },
    Heap(Vec<TouchEvent>),
}

/// Contact transitions that must be submitted to Android as one synchronized frame.
///
/// Normal gameplay frames (one movement/contact update, or the two-event mouse
/// recenter) stay inline and do not allocate. Larger cleanup frames spill to a
/// vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TouchFrame {
    events: TouchFrameEvents,
}

impl TouchFrame {
    pub fn new(events: impl IntoIterator<Item = TouchEvent>) -> Self {
        let mut iterator = events.into_iter();
        let mut inline = [EMPTY_TOUCH_EVENT; INLINE_TOUCH_EVENT_CAPACITY];
        for (index, slot) in inline.iter_mut().enumerate() {
            let Some(event) = iterator.next() else {
                return Self {
                    events: TouchFrameEvents::Inline {
                        events: inline,
                        len: index as u8,
                    },
                };
            };
            *slot = event;
        }

        let Some(first_heap_event) = iterator.next() else {
            return Self {
                events: TouchFrameEvents::Inline {
                    events: inline,
                    len: INLINE_TOUCH_EVENT_CAPACITY as u8,
                },
            };
        };
        let (remaining, _) = iterator.size_hint();
        let mut heap = Vec::with_capacity(
            INLINE_TOUCH_EVENT_CAPACITY
                .saturating_add(1)
                .saturating_add(remaining),
        );
        heap.extend_from_slice(&inline);
        heap.push(first_heap_event);
        heap.extend(iterator);
        Self {
            events: TouchFrameEvents::Heap(heap),
        }
    }

    pub fn single(event: TouchEvent) -> Self {
        let mut events = [EMPTY_TOUCH_EVENT; INLINE_TOUCH_EVENT_CAPACITY];
        events[0] = event;
        Self {
            events: TouchFrameEvents::Inline { events, len: 1 },
        }
    }

    pub fn events(&self) -> &[TouchEvent] {
        match &self.events {
            TouchFrameEvents::Inline { events, len } => &events[..usize::from(*len)],
            TouchFrameEvents::Heap(events) => events,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events().is_empty()
    }
}

/// Error returned by a persistent input injection backend.
#[derive(Debug, Error)]
#[error("touch injection backend failed: {message}")]
pub struct TouchInjectionError {
    message: String,
    #[source]
    source: Option<Box<dyn Error + Send + Sync>>,
}

impl TouchInjectionError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            source: None,
        }
    }

    pub fn with_source(
        message: impl Into<String>,
        source: impl Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            message: message.into(),
            source: Some(Box::new(source)),
        }
    }
}

/// Persistent transport capable of submitting one synchronized multitouch frame.
///
/// Production implementations are expected to keep their device or IPC channel
/// open across calls. Starting a subprocess from this method violates the runtime
/// performance contract and is reserved for an explicitly selected compatibility
/// backend.
pub trait TouchInjector {
    fn inject(&mut self, frame: &TouchFrame) -> Result<(), TouchInjectionError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TouchStateError {
    #[error("touch frame must contain at least one event")]
    EmptyFrame,
    #[error("contact {contact_id} appears more than once in one frame")]
    DuplicateContactInFrame { contact_id: u16 },
    #[error("contact {contact_id} is already active")]
    ContactAlreadyActive { contact_id: u16 },
    #[error("contact {contact_id} is not active")]
    ContactNotActive { contact_id: u16 },
}

/// Last committed multitouch state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TouchState {
    active_contacts: BTreeMap<ContactId, Point>,
}

impl TouchState {
    pub fn active_contact_count(&self) -> usize {
        self.active_contacts.len()
    }

    pub fn is_active(&self, contact_id: ContactId) -> bool {
        self.active_contacts.contains_key(&contact_id)
    }

    pub fn position(&self, contact_id: ContactId) -> Option<Point> {
        self.active_contacts.get(&contact_id).copied()
    }

    pub fn active_contacts(&self) -> impl Iterator<Item = (ContactId, Point)> + '_ {
        self.active_contacts
            .iter()
            .map(|(contact_id, point)| (*contact_id, *point))
    }

    fn validate_frame(&self, frame: &TouchFrame) -> Result<(), TouchStateError> {
        if frame.is_empty() {
            return Err(TouchStateError::EmptyFrame);
        }

        // Contacts touched earlier in this frame: a Down makes later Move
        // events for the same contact valid (the aim recenter relies on a
        // down+move pair landing in one report), every other repeat stays
        // ambiguous and rejected.
        let mut touched_in_frame: Vec<(ContactId, TouchPhase)> = Vec::new();
        for event in frame.events().iter() {
            let previous_phase = touched_in_frame
                .iter()
                .find(|(contact_id, _)| *contact_id == event.contact_id)
                .map(|(_, phase)| *phase);
            if let Some(previous_phase) = previous_phase {
                if !matches!(
                    (previous_phase, event.phase),
                    (TouchPhase::Down, TouchPhase::Move)
                ) {
                    return Err(TouchStateError::DuplicateContactInFrame {
                        contact_id: event.contact_id.get(),
                    });
                }
            }

            let active_in_frame = previous_phase == Some(TouchPhase::Down);
            match event.phase {
                TouchPhase::Down => {
                    if self.active_contacts.contains_key(&event.contact_id)
                        || previous_phase.is_some()
                    {
                        return Err(TouchStateError::ContactAlreadyActive {
                            contact_id: event.contact_id.get(),
                        });
                    }
                }
                TouchPhase::Move => {
                    if !self.active_contacts.contains_key(&event.contact_id) && !active_in_frame {
                        return Err(TouchStateError::ContactNotActive {
                            contact_id: event.contact_id.get(),
                        });
                    }
                }
                TouchPhase::Up | TouchPhase::Cancel => {
                    if !self.active_contacts.contains_key(&event.contact_id) && !active_in_frame {
                        return Err(TouchStateError::ContactNotActive {
                            contact_id: event.contact_id.get(),
                        });
                    }
                }
            }
            touched_in_frame.push((event.contact_id, event.phase));
        }
        Ok(())
    }

    fn apply_frame(&mut self, frame: &TouchFrame) {
        for event in frame.events() {
            match event.phase {
                TouchPhase::Down | TouchPhase::Move => {
                    self.active_contacts
                        .insert(event.contact_id, event.position);
                }
                TouchPhase::Up | TouchPhase::Cancel => {
                    self.active_contacts.remove(&event.contact_id);
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum TouchEngineError {
    #[error(transparent)]
    InvalidState(#[from] TouchStateError),
    #[error(transparent)]
    Injection(#[from] TouchInjectionError),
}

/// Applies validated touch frames to a persistent injector.
///
/// A frame is validated without mutating state, submitted to the backend, then
/// committed in place only after successful injection. This preserves atomic
/// failure semantics without cloning the active-contact map on every movement.
pub struct TouchEngine<I> {
    injector: I,
    state: TouchState,
}

impl<I: TouchInjector> TouchEngine<I> {
    pub fn new(injector: I) -> Self {
        Self {
            injector,
            state: TouchState::default(),
        }
    }

    pub fn state(&self) -> &TouchState {
        &self.state
    }

    pub fn injector(&self) -> &I {
        &self.injector
    }

    pub fn injector_mut(&mut self) -> &mut I {
        &mut self.injector
    }

    pub fn into_injector(self) -> I {
        self.injector
    }

    pub fn submit(&mut self, frame: TouchFrame) -> Result<(), TouchEngineError> {
        self.state.validate_frame(&frame)?;
        self.injector.inject(&frame)?;
        self.state.apply_frame(&frame);
        Ok(())
    }

    pub fn begin_contact(
        &mut self,
        contact_id: ContactId,
        position: Point,
    ) -> Result<(), TouchEngineError> {
        self.submit(TouchFrame::single(TouchEvent::new(
            contact_id,
            TouchPhase::Down,
            position,
        )))
    }

    pub fn move_contact(
        &mut self,
        contact_id: ContactId,
        position: Point,
    ) -> Result<(), TouchEngineError> {
        self.submit(TouchFrame::single(TouchEvent::new(
            contact_id,
            TouchPhase::Move,
            position,
        )))
    }

    pub fn end_contact(&mut self, contact_id: ContactId) -> Result<(), TouchEngineError> {
        let position =
            self.state
                .position(contact_id)
                .ok_or(TouchStateError::ContactNotActive {
                    contact_id: contact_id.get(),
                })?;
        self.submit(TouchFrame::single(TouchEvent::new(
            contact_id,
            TouchPhase::Up,
            position,
        )))
    }

    /// Cancels every active contact in one synchronized frame.
    ///
    /// Returns `false` when there was nothing to cancel. This method is intended
    /// for focus loss, shutdown, and crash-recovery paths.
    pub fn cancel_all(&mut self) -> Result<bool, TouchEngineError> {
        if self.state.active_contact_count() == 0 {
            return Ok(false);
        }

        let frame = TouchFrame::new(self.state.active_contacts().map(|(contact_id, position)| {
            TouchEvent::new(contact_id, TouchPhase::Cancel, position)
        }));
        self.submit(frame)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct RecordingInjector {
        frames: Vec<TouchFrame>,
        fail_next: bool,
    }

    impl TouchInjector for RecordingInjector {
        fn inject(&mut self, frame: &TouchFrame) -> Result<(), TouchInjectionError> {
            if self.fail_next {
                self.fail_next = false;
                return Err(TouchInjectionError::new("injected failure"));
            }
            self.frames.push(frame.clone());
            Ok(())
        }
    }

    fn point(x: u32, y: u32) -> Point {
        Point { x, y }
    }

    #[test]
    fn gameplay_touch_frames_stay_in_inline_storage() {
        let single = TouchFrame::single(TouchEvent::new(
            ContactId::new(1),
            TouchPhase::Move,
            point(10, 20),
        ));
        let recenter = TouchFrame::new([
            TouchEvent::new(ContactId::new(2), TouchPhase::Down, point(30, 40)),
            TouchEvent::new(ContactId::new(1), TouchPhase::Up, point(10, 20)),
        ]);

        assert!(matches!(single.events, TouchFrameEvents::Inline { .. }));
        assert!(matches!(recenter.events, TouchFrameEvents::Inline { .. }));
    }

    #[test]
    fn large_touch_frames_preserve_every_event_in_heap_fallback() {
        let frame = TouchFrame::new(
            (1..=5).map(|id| TouchEvent::new(ContactId::new(id), TouchPhase::Down, point(10, 20))),
        );

        assert!(matches!(frame.events, TouchFrameEvents::Heap(_)));
        assert_eq!(frame.events().len(), 5);
    }

    #[test]
    fn tracks_multiple_contacts_independently() {
        let mut engine = TouchEngine::new(RecordingInjector::default());
        let movement = ContactId::new(1);
        let aim = ContactId::new(2);

        engine.begin_contact(movement, point(100, 600)).unwrap();
        engine.begin_contact(aim, point(900, 400)).unwrap();
        engine.move_contact(movement, point(140, 560)).unwrap();
        engine.end_contact(aim).unwrap();

        assert_eq!(engine.state().active_contact_count(), 1);
        assert_eq!(engine.state().position(movement), Some(point(140, 560)));
        assert!(!engine.state().is_active(aim));
        assert_eq!(engine.injector().frames.len(), 4);
    }

    #[test]
    fn rejects_duplicate_down_before_calling_backend() {
        let mut engine = TouchEngine::new(RecordingInjector::default());
        let contact = ContactId::new(7);
        engine.begin_contact(contact, point(10, 20)).unwrap();

        let error = engine.begin_contact(contact, point(30, 40)).unwrap_err();

        assert!(matches!(
            error,
            TouchEngineError::InvalidState(TouchStateError::ContactAlreadyActive { contact_id: 7 })
        ));
        assert_eq!(engine.injector().frames.len(), 1);
        assert_eq!(engine.state().position(contact), Some(point(10, 20)));
    }

    #[test]
    fn backend_failure_does_not_commit_runtime_state() {
        let injector = RecordingInjector {
            fail_next: true,
            ..RecordingInjector::default()
        };
        let mut engine = TouchEngine::new(injector);
        let contact = ContactId::new(4);

        let error = engine.begin_contact(contact, point(200, 300)).unwrap_err();

        assert!(matches!(error, TouchEngineError::Injection(_)));
        assert!(!engine.state().is_active(contact));
        assert!(engine.injector().frames.is_empty());
    }

    #[test]
    fn cancel_all_releases_every_contact_in_one_frame() {
        let mut engine = TouchEngine::new(RecordingInjector::default());
        engine
            .begin_contact(ContactId::new(1), point(100, 100))
            .unwrap();
        engine
            .begin_contact(ContactId::new(2), point(200, 200))
            .unwrap();

        assert!(engine.cancel_all().unwrap());
        assert_eq!(engine.state().active_contact_count(), 0);

        let frame = engine.injector().frames.last().unwrap();
        assert_eq!(frame.events().len(), 2);
        assert!(frame
            .events()
            .iter()
            .all(|event| event.phase == TouchPhase::Cancel));
        assert!(!engine.cancel_all().unwrap());
    }

    #[test]
    fn allows_down_move_pair_and_rejects_ambiguous_repeats_in_one_frame() {
        let mut engine = TouchEngine::new(RecordingInjector::default());
        let contact = ContactId::new(3);
        // Down followed by Move for the same contact is the aim recenter
        // pattern: valid, and the state lands on the moved position.
        engine
            .submit(TouchFrame::new([
                TouchEvent::new(contact, TouchPhase::Down, point(1, 1)),
                TouchEvent::new(contact, TouchPhase::Move, point(2, 2)),
            ]))
            .unwrap();
        assert_eq!(engine.state().position(contact), Some(point(2, 2)));

        // A second Move for the same contact inside one frame stays
        // ambiguous and rejected.
        let error = engine
            .submit(TouchFrame::new([
                TouchEvent::new(contact, TouchPhase::Move, point(3, 3)),
                TouchEvent::new(contact, TouchPhase::Move, point(4, 4)),
            ]))
            .unwrap_err();

        assert!(matches!(
            error,
            TouchEngineError::InvalidState(TouchStateError::DuplicateContactInFrame {
                contact_id: 3
            })
        ));
        assert_eq!(engine.state().position(contact), Some(point(2, 2)));
        assert!(engine.injector().frames.len() == 1);
    }
}
