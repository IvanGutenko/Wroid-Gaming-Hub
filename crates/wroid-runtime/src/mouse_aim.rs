use std::time::Duration;

use thiserror::Error;
use wroid_core::{Point, Resolution};

use crate::{
    ContactId, TouchEngine, TouchEngineError, TouchEvent, TouchFrame, TouchInjector, TouchPhase,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseAimDelta {
    pub dx: i32,
    pub dy: i32,
}

impl MouseAimDelta {
    pub const fn new(dx: i32, dy: i32) -> Self {
        Self { dx, dy }
    }

    fn is_zero(self) -> bool {
        self.dx == 0 && self.dy == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseAimSensitivity {
    numerator: u32,
    denominator: u32,
}

impl MouseAimSensitivity {
    pub const fn one_to_one() -> Self {
        Self {
            numerator: 1,
            denominator: 1,
        }
    }

    pub fn new(numerator: u32, denominator: u32) -> Result<Self, MouseAimConfigError> {
        if numerator == 0 || denominator == 0 {
            return Err(MouseAimConfigError::InvalidSensitivity {
                numerator,
                denominator,
            });
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    fn scale(self, delta: i32) -> i64 {
        (i64::from(delta) * i64::from(self.numerator)) / i64::from(self.denominator)
    }

    const fn numerator(self) -> i64 {
        self.numerator as i64
    }

    const fn denominator(self) -> i64 {
        self.denominator as i64
    }
}

/// Fixed-point accumulator that preserves the sub-pixel remainder of scaled
/// mouse motion.
///
/// Integer division alone discards every delta smaller than the scale
/// denominator, so a sensitivity below 1.0 silently drops slow aim movement
/// entirely. The accumulator carries the remainder into the next event, which
/// keeps slow tracking proportional and makes total travel match the requested
/// sensitivity.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ScaleAccumulator {
    carry_x: i64,
    carry_y: i64,
}

impl ScaleAccumulator {
    fn scale(&mut self, delta: MouseAimDelta, numerator: i64, denominator: i64) -> (i64, i64) {
        let dx = Self::step(
            &mut self.carry_x,
            i64::from(delta.dx),
            numerator,
            denominator,
        );
        let dy = Self::step(
            &mut self.carry_y,
            i64::from(delta.dy),
            numerator,
            denominator,
        );
        (dx, dy)
    }

    fn step(carry: &mut i64, delta: i64, numerator: i64, denominator: i64) -> i64 {
        if denominator == 0 {
            return 0;
        }
        let scaled = delta.saturating_mul(numerator).saturating_add(*carry);
        let whole = scaled / denominator;
        *carry = scaled % denominator;
        whole
    }

    fn reset(&mut self) {
        self.carry_x = 0;
        self.carry_y = 0;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseAimRegion {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl MouseAimRegion {
    pub fn full_surface(resolution: Resolution) -> Result<Self, MouseAimConfigError> {
        if resolution.width == 0 || resolution.height == 0 {
            return Err(MouseAimConfigError::InvalidResolution {
                width: resolution.width,
                height: resolution.height,
            });
        }
        Ok(Self {
            left: 0,
            top: 0,
            right: resolution.width - 1,
            bottom: resolution.height - 1,
        })
    }

    pub fn validate(self, resolution: Resolution) -> Result<Self, MouseAimConfigError> {
        if resolution.width == 0 || resolution.height == 0 {
            return Err(MouseAimConfigError::InvalidResolution {
                width: resolution.width,
                height: resolution.height,
            });
        }
        if self.left > self.right
            || self.top > self.bottom
            || self.right >= resolution.width
            || self.bottom >= resolution.height
        {
            return Err(MouseAimConfigError::InvalidRegion {
                left: self.left,
                top: self.top,
                right: self.right,
                bottom: self.bottom,
                width: resolution.width,
                height: resolution.height,
            });
        }
        Ok(self)
    }

    fn contains(self, point: Point) -> bool {
        point.x >= self.left
            && point.x <= self.right
            && point.y >= self.top
            && point.y <= self.bottom
    }

    fn clamp(self, point: Point) -> Point {
        Point {
            x: point.x.clamp(self.left, self.right),
            y: point.y.clamp(self.top, self.bottom),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MouseAimConfigError {
    #[error("mouse aim resolution must be non-zero, got {width}x{height}")]
    InvalidResolution { width: u32, height: u32 },
    #[error(
        "mouse aim region {left},{top}-{right},{bottom} is invalid for resolution {width}x{height}"
    )]
    InvalidRegion {
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
        width: u32,
        height: u32,
    },
    #[error("mouse aim origin {x},{y} is outside region {left},{top}-{right},{bottom}")]
    OriginOutOfRegion {
        x: u32,
        y: u32,
        left: u32,
        top: u32,
        right: u32,
        bottom: u32,
    },
    #[error("mouse aim sensitivity must be non-zero, got {numerator}/{denominator}")]
    InvalidSensitivity { numerator: u32, denominator: u32 },
    #[error("mouse aim primary and alternate contact IDs must differ, got {contact_id}")]
    DuplicateContactId { contact_id: u16 },
    #[error("mouse aim recenter threshold must be within 100..=1000 milli, got {milli}")]
    InvalidRecenterThreshold { milli: u16 },
    #[error("mouse aim reaffirm interval must be greater than zero")]
    ZeroReaffirmInterval,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseAim {
    contact_id: ContactId,
    origin: Point,
    region: MouseAimRegion,
    sensitivity: MouseAimSensitivity,
}

impl MouseAim {
    pub fn new(
        contact_id: ContactId,
        origin: Point,
        region: MouseAimRegion,
        resolution: Resolution,
        sensitivity: MouseAimSensitivity,
    ) -> Result<Self, MouseAimConfigError> {
        let region = region.validate(resolution)?;
        if !region.contains(origin) {
            return Err(MouseAimConfigError::OriginOutOfRegion {
                x: origin.x,
                y: origin.y,
                left: region.left,
                top: region.top,
                right: region.right,
                bottom: region.bottom,
            });
        }
        Ok(Self {
            contact_id,
            origin,
            region,
            sensitivity,
        })
    }

    pub const fn contact_id(&self) -> ContactId {
        self.contact_id
    }

    pub const fn origin(&self) -> Point {
        self.origin
    }

    pub const fn region(&self) -> MouseAimRegion {
        self.region
    }

    pub const fn sensitivity(&self) -> MouseAimSensitivity {
        self.sensitivity
    }

    pub fn begin<I: TouchInjector>(
        &self,
        engine: &mut TouchEngine<I>,
    ) -> Result<bool, TouchEngineError> {
        if engine.state().is_active(self.contact_id) {
            return Ok(false);
        }
        engine.begin_contact(self.contact_id, self.origin)?;
        Ok(true)
    }

    pub fn move_by<I: TouchInjector>(
        &self,
        engine: &mut TouchEngine<I>,
        delta: MouseAimDelta,
    ) -> Result<bool, TouchEngineError> {
        if delta.is_zero() {
            return Ok(false);
        }
        let Some(current) = engine.state().position(self.contact_id) else {
            return Ok(false);
        };

        let target = self.region.clamp(offset_point(
            current,
            self.sensitivity.scale(delta.dx),
            self.sensitivity.scale(delta.dy),
        ));
        if target == current {
            return Ok(false);
        }

        engine.move_contact(self.contact_id, target)?;
        Ok(true)
    }

    pub fn end<I: TouchInjector>(
        &self,
        engine: &mut TouchEngine<I>,
    ) -> Result<bool, TouchEngineError> {
        if !engine.state().is_active(self.contact_id) {
            return Ok(false);
        }
        engine.end_contact(self.contact_id)?;
        Ok(true)
    }

    pub fn cancel<I: TouchInjector>(
        &self,
        engine: &mut TouchEngine<I>,
    ) -> Result<bool, TouchEngineError> {
        let Some(position) = engine.state().position(self.contact_id) else {
            return Ok(false);
        };
        engine.submit(TouchFrame::single(TouchEvent::new(
            self.contact_id,
            TouchPhase::Cancel,
            position,
        )))?;
        Ok(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseAimSettings {
    pub alternate_contact_id: ContactId,
    pub toggle_key: Option<String>,
    pub recenter_threshold_milli: u16,
    pub recenter_gap: Duration,
    pub ads_multiplier: Option<MouseAimSensitivity>,
    pub reaffirm_interval: Option<Duration>,
}

impl Default for MouseAimSettings {
    fn default() -> Self {
        Self {
            alternate_contact_id: ContactId::new(2),
            toggle_key: None,
            recenter_threshold_milli: 700,
            recenter_gap: Duration::ZERO,
            ads_multiplier: None,
            reaffirm_interval: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseAimUpdate {
    Ignored,
    Activated,
    Moved,
    Recentered,
    Deactivated,
    Reaffirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingRecenter {
    contact_id: ContactId,
    previous_contact_id: ContactId,
    ready_at: Duration,
    dx: i64,
    dy: i64,
}

/// Stateful mouse-to-touch controller with toggle activation and seamless
/// contact-slot recentering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseAimController {
    aim: MouseAim,
    settings: MouseAimSettings,
    active_contact_id: Option<ContactId>,
    next_contact_id: ContactId,
    pending_recenter: Option<PendingRecenter>,
    ads_active: bool,
    recenter_count: u64,
    last_reaffirm_at: Option<Duration>,
    accumulator: ScaleAccumulator,
    /// Scaled touch travel accumulated by the current contact since its
    /// down event; the relocation budget is measured against this.
    travel_since_down: i64,
}

impl MouseAimController {
    pub fn new(aim: MouseAim, settings: MouseAimSettings) -> Result<Self, MouseAimConfigError> {
        if settings.alternate_contact_id == aim.contact_id {
            return Err(MouseAimConfigError::DuplicateContactId {
                contact_id: aim.contact_id.get(),
            });
        }
        if !(100..=1_000).contains(&settings.recenter_threshold_milli) {
            return Err(MouseAimConfigError::InvalidRecenterThreshold {
                milli: settings.recenter_threshold_milli,
            });
        }
        if matches!(settings.reaffirm_interval, Some(interval) if interval.is_zero()) {
            return Err(MouseAimConfigError::ZeroReaffirmInterval);
        }

        let next_contact_id = settings.alternate_contact_id;
        Ok(Self {
            aim,
            settings,
            active_contact_id: None,
            next_contact_id,
            pending_recenter: None,
            ads_active: false,
            recenter_count: 0,
            last_reaffirm_at: None,
            accumulator: ScaleAccumulator::default(),
            travel_since_down: 0,
        })
    }

    /// Travel budget of one contact: a fraction of the smaller region
    /// half-extent, so the contact can roam the whole clean region before it
    /// is replaced.
    fn travel_budget(&self) -> i64 {
        let half_width = (self.aim.region.right - self.aim.region.left) / 2;
        let half_height = (self.aim.region.bottom - self.aim.region.top) / 2;
        (u64::from(half_width.min(half_height))
            .saturating_mul(u64::from(self.settings.recenter_threshold_milli))
            / 1_000) as i64
    }

    pub const fn aim(&self) -> &MouseAim {
        &self.aim
    }

    pub const fn settings(&self) -> &MouseAimSettings {
        &self.settings
    }

    pub const fn is_active(&self) -> bool {
        self.active_contact_id.is_some() || self.pending_recenter.is_some()
    }

    pub const fn recenter_count(&self) -> u64 {
        self.recenter_count
    }

    pub fn activate<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
        now: Duration,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        if self.is_active() {
            return Ok(MouseAimUpdate::Ignored);
        }
        let contact_id = self.aim.contact_id;
        trace_aim(format!(
            "activate contact={contact_id:?} origin=({},{})",
            self.aim.origin.x, self.aim.origin.y
        ));
        engine.begin_contact(contact_id, self.aim.origin)?;
        self.active_contact_id = Some(contact_id);
        self.next_contact_id = self.settings.alternate_contact_id;
        self.last_reaffirm_at = Some(now);
        self.travel_since_down = 0;
        Ok(MouseAimUpdate::Activated)
    }

    pub fn toggle<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
        now: Duration,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        if self.is_active() {
            self.deactivate(engine)
        } else {
            self.activate(engine, now)
        }
    }

    pub fn set_ads_active(&mut self, active: bool) {
        if self.ads_active != active {
            // Drop the remainder captured under the previous scale so an ADS
            // switch cannot leak accumulated motion at the new sensitivity.
            self.accumulator.reset();
        }
        self.ads_active = active;
    }

    pub fn move_by<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
        delta: MouseAimDelta,
        now: Duration,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        if delta.is_zero() || !self.is_active() {
            return Ok(MouseAimUpdate::Ignored);
        }

        let (dx, dy) = self.scaled_delta(delta);
        if dx == 0 && dy == 0 {
            // The scaled delta vanished into the accumulator; nothing to
            // apply and no travel to budget.
            return Ok(MouseAimUpdate::Ignored);
        }
        if let Some(pending) = &mut self.pending_recenter {
            trace_aim(format!(
                "move during pending recenter: accumulate ({dx},{dy})"
            ));
            pending.dx = pending.dx.saturating_add(dx);
            pending.dy = pending.dy.saturating_add(dy);
            return self.tick(engine, now);
        }

        let Some(contact_id) = self.active_contact_id else {
            trace_aim("move ignored: no active contact".to_owned());
            return Ok(MouseAimUpdate::Ignored);
        };
        let Some(current) = engine.state().position(contact_id) else {
            trace_aim(format!(
                "move ignored: contact {contact_id:?} has no position"
            ));
            return Ok(MouseAimUpdate::Ignored);
        };
        let target = self.aim.region.clamp(offset_point(current, dx, dy));
        let clamped = target == current;

        // Relocate when the region edge swallowed the whole delta (the
        // replacement lands at the origin, where later deltas apply again —
        // without this the contact sticks to the edge and the camera dies
        // for the rest of the swipe), or once the travel budget is spent.
        // Both relocations continue from the new landing with the pending
        // delta applied, so no rotation beyond the clamped event is lost.
        if !clamped && self.travel_since_down < self.travel_budget() {
            trace_aim(format!(
                "move contact=({},{}) -> ({},{})",
                current.x, current.y, target.x, target.y
            ));
            engine.move_contact(contact_id, target)?;
            self.travel_since_down += dx.abs() + dy.abs();
            self.last_reaffirm_at = Some(now);
            return Ok(MouseAimUpdate::Moved);
        }

        trace_aim(format!(
            "relocate contact=({},{}) budget={} clamped={clamped} delta=({dx},{dy})",
            current.x, current.y, self.travel_since_down
        ));
        self.relocate(engine, contact_id, current, dx, dy, now)
    }

    pub fn tick<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
        now: Duration,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        if let Some(pending) = self.pending_recenter {
            if now < pending.ready_at {
                return Ok(MouseAimUpdate::Ignored);
            }
            trace_aim(format!(
                "recenter fires: new contact down at ({},{}) residual=({},{})",
                self.aim.origin.x, self.aim.origin.y, pending.dx, pending.dy
            ));
            engine.begin_contact(pending.contact_id, self.aim.origin)?;
            self.active_contact_id = Some(pending.contact_id);
            self.next_contact_id = pending.previous_contact_id;
            self.pending_recenter = None;
            self.last_reaffirm_at = Some(now);
            self.apply_residual(engine, pending.contact_id, pending.dx, pending.dy)?;
            self.travel_since_down = pending.dx.abs() + pending.dy.abs();
            return Ok(MouseAimUpdate::Recentered);
        }

        let Some(interval) = self.settings.reaffirm_interval else {
            return Ok(MouseAimUpdate::Ignored);
        };
        let Some(contact_id) = self.active_contact_id else {
            return Ok(MouseAimUpdate::Ignored);
        };
        if self
            .last_reaffirm_at
            .is_some_and(|last| now.saturating_sub(last) < interval)
        {
            return Ok(MouseAimUpdate::Ignored);
        }
        let Some(position) = engine.state().position(contact_id) else {
            return Ok(MouseAimUpdate::Ignored);
        };
        engine.move_contact(contact_id, position)?;
        self.last_reaffirm_at = Some(now);
        Ok(MouseAimUpdate::Reaffirmed)
    }

    pub fn deactivate<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        let mut changed = false;
        if let Some(contact_id) = self.active_contact_id.take() {
            if engine.state().is_active(contact_id) {
                engine.end_contact(contact_id)?;
                changed = true;
            }
        }
        if self.pending_recenter.take().is_some() {
            changed = true;
        }
        self.ads_active = false;
        self.last_reaffirm_at = None;
        self.accumulator.reset();
        self.travel_since_down = 0;
        Ok(if changed {
            MouseAimUpdate::Deactivated
        } else {
            MouseAimUpdate::Ignored
        })
    }

    pub fn cancel<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
    ) -> Result<bool, TouchEngineError> {
        let mut changed = self.pending_recenter.take().is_some();
        if let Some(contact_id) = self.active_contact_id.take() {
            if let Some(position) = engine.state().position(contact_id) {
                engine.submit(TouchFrame::single(TouchEvent::new(
                    contact_id,
                    TouchPhase::Cancel,
                    position,
                )))?;
                changed = true;
            }
        }
        self.ads_active = false;
        self.last_reaffirm_at = None;
        self.accumulator.reset();
        self.travel_since_down = 0;
        Ok(changed)
    }

    fn scaled_delta(&mut self, delta: MouseAimDelta) -> (i64, i64) {
        let sensitivity = self.aim.sensitivity;
        let (mut numerator, mut denominator) = (sensitivity.numerator(), sensitivity.denominator());
        if self.ads_active {
            if let Some(multiplier) = self.settings.ads_multiplier {
                numerator = numerator.saturating_mul(multiplier.numerator());
                denominator = denominator.saturating_mul(multiplier.denominator());
            }
        }
        self.accumulator.scale(delta, numerator, denominator)
    }

    /// Replace the active contact without losing rotation.
    ///
    /// `clamped` relocations land the replacement at the origin so the edge-
    /// swallowed delta becomes applicable again; budget relocations land at
    /// the exact position of the old contact, which keeps the game's
    /// per-touch position samples continuous (zero rotation loss). Both
    /// variants carry the pending delta as a same-frame move so the game
    /// rebinds its look gesture immediately.
    #[allow(clippy::too_many_arguments)]
    fn relocate<I: TouchInjector>(
        &mut self,
        engine: &mut TouchEngine<I>,
        old_contact_id: ContactId,
        old_position: Point,
        dx: i64,
        dy: i64,
        now: Duration,
    ) -> Result<MouseAimUpdate, TouchEngineError> {
        let new_contact_id = self.next_contact_id;

        if self.settings.recenter_gap.is_zero() {
            // The replacement always lands at the origin: the one point of
            // the region guaranteed clear of HUD buttons. Landing on the old
            // contact's exact position stacks two touches on one spot for a
            // frame, which games read as a broken gesture. The pending delta
            // rides along as a same-frame move so the game rebinds its look
            // gesture immediately.
            let landed = self.aim.region.clamp(offset_point(self.aim.origin, dx, dy));
            engine.submit(TouchFrame::new([
                TouchEvent::new(new_contact_id, TouchPhase::Down, self.aim.origin),
                TouchEvent::new(new_contact_id, TouchPhase::Move, landed),
                TouchEvent::new(old_contact_id, TouchPhase::Up, old_position),
            ]))?;
            self.active_contact_id = Some(new_contact_id);
            self.next_contact_id = old_contact_id;
            self.last_reaffirm_at = Some(now);
            self.travel_since_down = (i64::from(landed.x) - i64::from(self.aim.origin.x)).abs()
                + (i64::from(landed.y) - i64::from(self.aim.origin.y)).abs();
        } else {
            engine.end_contact(old_contact_id)?;
            self.active_contact_id = None;
            self.pending_recenter = Some(PendingRecenter {
                contact_id: new_contact_id,
                previous_contact_id: old_contact_id,
                ready_at: now.saturating_add(self.settings.recenter_gap),
                dx,
                dy,
            });
        }
        self.recenter_count = self.recenter_count.saturating_add(1);

        Ok(MouseAimUpdate::Recentered)
    }

    fn apply_residual<I: TouchInjector>(
        &self,
        engine: &mut TouchEngine<I>,
        contact_id: ContactId,
        dx: i64,
        dy: i64,
    ) -> Result<(), TouchEngineError> {
        let target = self.aim.region.clamp(offset_point(self.aim.origin, dx, dy));
        if target != self.aim.origin {
            engine.move_contact(contact_id, target)?;
        }
        Ok(())
    }
}

fn trace_aim(message: String) {
    if std::env::var_os("WROID_AIM_TRACE").is_some() {
        eprintln!("[aim-trace] {message}");
    }
}

fn offset_point(point: Point, dx: i64, dy: i64) -> Point {
    Point {
        x: offset_axis(point.x, dx),
        y: offset_axis(point.y, dy),
    }
}

fn offset_axis(value: u32, delta: i64) -> u32 {
    let value = i64::from(value) + delta;
    if value <= 0 {
        0
    } else if value >= i64::from(u32::MAX) {
        u32::MAX
    } else {
        value as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TouchFrame, TouchInjectionError};

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

    fn resolution() -> Resolution {
        Resolution {
            width: 1920,
            height: 1080,
        }
    }

    fn aim() -> MouseAim {
        MouseAim::new(
            ContactId::new(9),
            Point { x: 960, y: 540 },
            MouseAimRegion {
                left: 640,
                top: 240,
                right: 1600,
                bottom: 840,
            },
            resolution(),
            MouseAimSensitivity::one_to_one(),
        )
        .unwrap()
    }

    #[test]
    fn begins_and_moves_one_persistent_aim_contact() {
        let aim = aim();
        let mut engine = TouchEngine::new(RecordingInjector::default());

        assert!(aim.begin(&mut engine).unwrap());
        assert!(!aim.begin(&mut engine).unwrap());
        assert!(aim
            .move_by(&mut engine, MouseAimDelta::new(25, -10))
            .unwrap());

        assert_eq!(
            engine.state().position(aim.contact_id()),
            Some(Point { x: 985, y: 530 })
        );
        let frames = &engine.injector().frames;
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].events()[0].phase, TouchPhase::Down);
        assert_eq!(frames[1].events()[0].phase, TouchPhase::Move);
    }

    #[test]
    fn ignores_motion_before_activation_and_zero_motion() {
        let aim = aim();
        let mut engine = TouchEngine::new(RecordingInjector::default());

        assert!(!aim
            .move_by(&mut engine, MouseAimDelta::new(30, 30))
            .unwrap());
        assert!(aim.begin(&mut engine).unwrap());
        assert!(!aim.move_by(&mut engine, MouseAimDelta::new(0, 0)).unwrap());

        assert_eq!(engine.injector().frames.len(), 1);
    }

    #[test]
    fn applies_sensitivity_and_clamps_to_region() {
        let aim = MouseAim::new(
            ContactId::new(5),
            Point { x: 100, y: 100 },
            MouseAimRegion {
                left: 50,
                top: 50,
                right: 150,
                bottom: 150,
            },
            Resolution {
                width: 200,
                height: 200,
            },
            MouseAimSensitivity::new(2, 1).unwrap(),
        )
        .unwrap();
        let mut engine = TouchEngine::new(RecordingInjector::default());

        assert!(aim.begin(&mut engine).unwrap());
        assert!(aim
            .move_by(&mut engine, MouseAimDelta::new(100, -100))
            .unwrap());

        assert_eq!(
            engine.state().position(aim.contact_id()),
            Some(Point { x: 150, y: 50 })
        );
    }

    #[test]
    fn end_releases_and_focus_loss_cancels() {
        let aim = aim();
        let mut engine = TouchEngine::new(RecordingInjector::default());

        assert!(!aim.end(&mut engine).unwrap());
        assert!(aim.begin(&mut engine).unwrap());
        assert!(aim.end(&mut engine).unwrap());
        assert!(!engine.state().is_active(aim.contact_id()));

        assert!(aim.begin(&mut engine).unwrap());
        assert!(aim.cancel(&mut engine).unwrap());
        assert!(!aim.cancel(&mut engine).unwrap());
        assert_eq!(
            engine.injector().frames.last().unwrap().events()[0].phase,
            TouchPhase::Cancel
        );
    }

    #[test]
    fn failed_begin_does_not_activate_contact() {
        let aim = aim();
        let injector = RecordingInjector {
            fail_next: true,
            ..RecordingInjector::default()
        };
        let mut engine = TouchEngine::new(injector);

        let error = aim.begin(&mut engine).unwrap_err();

        assert!(matches!(error, TouchEngineError::Injection(_)));
        assert!(!engine.state().is_active(aim.contact_id()));
        assert!(engine.injector().frames.is_empty());
    }

    #[test]
    fn rejects_invalid_geometry_and_sensitivity() {
        assert!(matches!(
            MouseAimRegion::full_surface(Resolution {
                width: 0,
                height: 100
            }),
            Err(MouseAimConfigError::InvalidResolution { .. })
        ));
        assert!(matches!(
            MouseAimSensitivity::new(0, 1),
            Err(MouseAimConfigError::InvalidSensitivity { .. })
        ));
        assert!(matches!(
            MouseAim::new(
                ContactId::new(1),
                Point { x: 10, y: 10 },
                MouseAimRegion {
                    left: 20,
                    top: 20,
                    right: 30,
                    bottom: 30,
                },
                Resolution {
                    width: 100,
                    height: 100,
                },
                MouseAimSensitivity::one_to_one(),
            ),
            Err(MouseAimConfigError::OriginOutOfRegion { .. })
        ));
    }

    fn controller(gap: Duration) -> MouseAimController {
        MouseAimController::new(
            aim(),
            MouseAimSettings {
                alternate_contact_id: ContactId::new(10),
                toggle_key: Some("tab".to_owned()),
                recenter_threshold_milli: 500,
                recenter_gap: gap,
                ads_multiplier: Some(MouseAimSensitivity::new(1, 2).unwrap()),
                reaffirm_interval: Some(Duration::from_millis(50)),
            },
        )
        .unwrap()
    }

    #[test]
    fn slow_motion_below_one_pixel_accumulates_instead_of_vanishing() {
        let slow_aim = MouseAim::new(
            ContactId::new(9),
            Point { x: 960, y: 540 },
            MouseAimRegion {
                left: 640,
                top: 240,
                right: 1600,
                bottom: 840,
            },
            resolution(),
            // 0.6 sensitivity: every single-count delta scales below one pixel.
            MouseAimSensitivity::new(600, 1_000).unwrap(),
        )
        .unwrap();
        let mut controller = MouseAimController::new(
            slow_aim,
            MouseAimSettings {
                alternate_contact_id: ContactId::new(10),
                recenter_threshold_milli: 1_000,
                ..MouseAimSettings::default()
            },
        )
        .unwrap();
        let mut engine = TouchEngine::new(RecordingInjector::default());
        controller.activate(&mut engine, Duration::ZERO).unwrap();

        for tick in 0..10 {
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(1, 0),
                    Duration::from_millis(tick + 1),
                )
                .unwrap();
        }

        // Ten counts at 0.6 must land six pixels away, not zero.
        assert_eq!(
            engine.state().position(ContactId::new(9)),
            Some(Point { x: 966, y: 540 })
        );
    }

    #[test]
    fn controller_toggles_and_applies_ads_multiplier() {
        let mut controller = controller(Duration::ZERO);
        let mut engine = TouchEngine::new(RecordingInjector::default());

        assert_eq!(
            controller
                .toggle(&mut engine, Duration::from_millis(0))
                .unwrap(),
            MouseAimUpdate::Activated
        );
        controller.set_ads_active(true);
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(20, -10),
                    Duration::from_millis(1)
                )
                .unwrap(),
            MouseAimUpdate::Moved
        );
        assert_eq!(
            engine.state().position(ContactId::new(9)),
            Some(Point { x: 970, y: 535 })
        );
        assert_eq!(
            controller
                .toggle(&mut engine, Duration::from_millis(2))
                .unwrap(),
            MouseAimUpdate::Deactivated
        );
        assert_eq!(engine.state().active_contact_count(), 0);
    }

    #[test]
    fn controller_recenters_with_two_distinct_contacts_in_one_frame() {
        let mut controller = controller(Duration::ZERO);
        let mut engine = TouchEngine::new(RecordingInjector::default());
        controller.activate(&mut engine, Duration::ZERO).unwrap();

        // Budget is 150 px of travel (half region height 300 x threshold
        // 0.5): the first two moves stay on the same contact.
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(100, 0),
                    Duration::from_millis(1)
                )
                .unwrap(),
            MouseAimUpdate::Moved
        );
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(100, 0),
                    Duration::from_millis(2)
                )
                .unwrap(),
            MouseAimUpdate::Moved
        );

        // The next move relocates: the replacement contact lands at the
        // exact old position (zero rotation loss) and its same-frame move
        // carries the pending delta, so the game rebinds immediately.
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(10, 0),
                    Duration::from_millis(3)
                )
                .unwrap(),
            MouseAimUpdate::Recentered
        );

        let recenter = &engine.injector().frames[3];
        assert_eq!(recenter.events().len(), 3);
        assert_eq!(recenter.events()[0].contact_id, ContactId::new(10));
        assert_eq!(recenter.events()[0].phase, TouchPhase::Down);
        assert_eq!(recenter.events()[0].position, Point { x: 960, y: 540 });
        assert_eq!(recenter.events()[1].contact_id, ContactId::new(10));
        assert_eq!(recenter.events()[1].phase, TouchPhase::Move);
        assert_eq!(recenter.events()[1].position, Point { x: 970, y: 540 });
        assert_eq!(recenter.events()[2].contact_id, ContactId::new(9));
        assert_eq!(recenter.events()[2].phase, TouchPhase::Up);
        assert_eq!(controller.recenter_count(), 1);
        assert!(engine.state().is_active(ContactId::new(10)));
        assert!(!engine.state().is_active(ContactId::new(9)));
        assert_eq!(
            engine.state().position(ContactId::new(10)),
            Some(Point { x: 970, y: 540 })
        );
    }

    #[test]
    fn gap_recenter_buffers_motion_until_tick() {
        let mut controller = controller(Duration::from_millis(10));
        let mut engine = TouchEngine::new(RecordingInjector::default());
        controller.activate(&mut engine, Duration::ZERO).unwrap();

        // Budget 150: this move stays on the active contact.
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(250, 0),
                    Duration::from_millis(1)
                )
                .unwrap(),
            MouseAimUpdate::Moved
        );
        // Budget spent: with a recenter gap the old contact lifts now and
        // the replacement presses after the gap, carrying the buffered
        // delta.
        assert_eq!(
            controller
                .move_by(
                    &mut engine,
                    MouseAimDelta::new(10, 0),
                    Duration::from_millis(5)
                )
                .unwrap(),
            MouseAimUpdate::Recentered
        );

        assert_eq!(engine.state().active_contact_count(), 0);
        assert_eq!(
            controller
                .tick(&mut engine, Duration::from_millis(15))
                .unwrap(),
            MouseAimUpdate::Recentered
        );
        assert_eq!(
            engine.state().position(ContactId::new(10)),
            Some(Point { x: 970, y: 540 })
        );
    }

    #[test]
    fn controller_reaffirms_and_cancels_active_contact() {
        let mut controller = controller(Duration::ZERO);
        let mut engine = TouchEngine::new(RecordingInjector::default());
        controller.activate(&mut engine, Duration::ZERO).unwrap();

        assert_eq!(
            controller
                .tick(&mut engine, Duration::from_millis(49))
                .unwrap(),
            MouseAimUpdate::Ignored
        );
        assert_eq!(
            controller
                .tick(&mut engine, Duration::from_millis(50))
                .unwrap(),
            MouseAimUpdate::Reaffirmed
        );
        assert!(controller.cancel(&mut engine).unwrap());
        assert_eq!(engine.state().active_contact_count(), 0);
        assert_eq!(
            engine.injector().frames.last().unwrap().events()[0].phase,
            TouchPhase::Cancel
        );
    }
}
