//! Per-user runtime daemon foundation.
//!
//! This crate owns daemon-side session bookkeeping without depending on ADB,
//! Waydroid shell commands, terminal input, GUI toolkits, or privileged helper
//! implementation details. [`ipc`] exposes this state through a private,
//! versioned Unix-socket protocol for the CLI and desktop UI.

pub mod ipc;
mod process;

use std::collections::BTreeMap;

use thiserror::Error;
use wroid_core::profile_v2::ProfileV2;
use wroid_core::ProfileValidationError;
use wroid_runtime::{
    DisplayInfo, PreparedSession, RuntimeControlPlan, RuntimeControlPlanError, SessionId,
    SessionLifecycle, SessionRequest, SessionState, StopReason, StopReport,
};

/// Session metadata owned by the daemon.
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeSession {
    session_id: SessionId,
    state: SessionState,
    active_package: String,
    launch_package: bool,
    control_plan: Option<RuntimeControlPlan>,
    process_id: Option<u32>,
    detail: Option<String>,
}

impl RuntimeSession {
    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub const fn state(&self) -> SessionState {
        self.state
    }

    pub fn active_package(&self) -> &str {
        &self.active_package
    }

    pub const fn launch_package(&self) -> bool {
        self.launch_package
    }

    pub fn control_plan(&self) -> Option<&RuntimeControlPlan> {
        self.control_plan.as_ref()
    }

    pub const fn process_id(&self) -> Option<u32> {
        self.process_id
    }

    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }

    fn prepared(&self) -> PreparedSession {
        PreparedSession {
            session_id: self.session_id.clone(),
            state: self.state,
            active_package: self.active_package.clone(),
        }
    }
}

/// In-memory daemon session manager used before the IPC daemon is introduced.
#[derive(Debug, Default)]
pub struct DaemonSessionManager {
    sessions: BTreeMap<SessionId, RuntimeSession>,
}

impl DaemonSessionManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn session(&self, session_id: &SessionId) -> Option<&RuntimeSession> {
        self.sessions.get(session_id)
    }

    pub fn session_count(&self) -> usize {
        self.sessions.len()
    }

    pub fn has_session(&self, session_id: &SessionId) -> bool {
        self.sessions.contains_key(session_id)
    }

    pub fn sessions(&self) -> impl Iterator<Item = &RuntimeSession> {
        self.sessions.values()
    }

    pub fn prepare_profile_v2(
        &mut self,
        session_id: SessionId,
        profile: ProfileV2,
        display: DisplayInfo,
        launch_package: bool,
    ) -> Result<PreparedSession, DaemonError> {
        if self.sessions.contains_key(&session_id) {
            return Err(DaemonError::SessionAlreadyExists(
                session_id.as_str().to_owned(),
            ));
        }

        let control_plan = RuntimeControlPlan::from_profile_v2(&profile, display.resolution)?;
        let session = RuntimeSession {
            session_id: session_id.clone(),
            state: SessionState::Preparing,
            active_package: control_plan.package_name.clone(),
            launch_package,
            control_plan: Some(control_plan),
            process_id: None,
            detail: None,
        };
        let prepared = session.prepared();
        self.sessions.insert(session_id, session);
        Ok(prepared)
    }

    pub fn mark_running(&mut self, session_id: &SessionId, pid: u32) -> Result<(), DaemonError> {
        let session = self.session_for_transition(session_id, SessionState::Running)?;
        if session.state != SessionState::Preparing {
            return Err(invalid_transition(session, SessionState::Running));
        }
        session.state = SessionState::Running;
        session.process_id = Some(pid);
        session.detail = None;
        Ok(())
    }

    pub fn mark_stopping(&mut self, session_id: &SessionId) -> Result<(), DaemonError> {
        let session = self.session_for_transition(session_id, SessionState::Stopping)?;
        if session.state != SessionState::Running {
            return Err(invalid_transition(session, SessionState::Stopping));
        }
        session.state = SessionState::Stopping;
        Ok(())
    }

    pub fn mark_exited(
        &mut self,
        session_id: &SessionId,
        success: bool,
        detail: &str,
    ) -> Result<(), DaemonError> {
        let target = if success {
            SessionState::Stopped
        } else {
            SessionState::Failed
        };
        let session = self.session_for_transition(session_id, target)?;
        if !matches!(
            session.state,
            SessionState::Running | SessionState::Stopping
        ) {
            return Err(invalid_transition(session, target));
        }
        session.state = target;
        session.process_id = None;
        session.detail = Some(bounded_detail(detail));
        Ok(())
    }

    pub fn mark_failed(&mut self, session_id: &SessionId, detail: &str) -> Result<(), DaemonError> {
        let session = self.session_for_transition(session_id, SessionState::Failed)?;
        if session.state != SessionState::Preparing {
            return Err(invalid_transition(session, SessionState::Failed));
        }
        session.state = SessionState::Failed;
        session.process_id = None;
        session.detail = Some(bounded_detail(detail));
        Ok(())
    }

    fn session_for_transition(
        &mut self,
        session_id: &SessionId,
        _target: SessionState,
    ) -> Result<&mut RuntimeSession, DaemonError> {
        self.sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::SessionNotFound(session_id.as_str().to_owned()))
    }
}

const SESSION_DETAIL_CHARS: usize = 4096;

fn bounded_detail(detail: &str) -> String {
    detail.chars().take(SESSION_DETAIL_CHARS).collect()
}

fn invalid_transition(session: &RuntimeSession, to: SessionState) -> DaemonError {
    DaemonError::InvalidTransition {
        session_id: session.session_id.as_str().to_owned(),
        from: session.state,
        to,
    }
}

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error(transparent)]
    InvalidProfile(#[from] ProfileValidationError),
    #[error(transparent)]
    InvalidRuntimeControls(#[from] RuntimeControlPlanError),
    #[error("session {0} already exists")]
    SessionAlreadyExists(String),
    #[error("session {0} was not found")]
    SessionNotFound(String),
    #[error("session {session_id} cannot transition from {from:?} to {to:?}")]
    InvalidTransition {
        session_id: String,
        from: SessionState,
        to: SessionState,
    },
}

impl SessionLifecycle for DaemonSessionManager {
    type Error = DaemonError;

    fn prepare(&mut self, request: SessionRequest) -> Result<PreparedSession, Self::Error> {
        request.profile.validate()?;

        if self.sessions.contains_key(&request.session_id) {
            return Err(DaemonError::SessionAlreadyExists(
                request.session_id.as_str().to_owned(),
            ));
        }

        let session = RuntimeSession {
            session_id: request.session_id.clone(),
            state: SessionState::Preparing,
            active_package: request.profile.package_name.clone(),
            launch_package: request.launch_package,
            control_plan: None,
            process_id: None,
            detail: None,
        };
        let prepared = session.prepared();
        self.sessions.insert(request.session_id, session);
        Ok(prepared)
    }

    fn start(&mut self, session_id: &SessionId) -> Result<(), Self::Error> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::SessionNotFound(session_id.as_str().to_owned()))?;

        match session.state {
            SessionState::Preparing => {
                session.state = SessionState::Running;
                Ok(())
            }
            other => Err(DaemonError::InvalidTransition {
                session_id: session_id.as_str().to_owned(),
                from: other,
                to: SessionState::Running,
            }),
        }
    }

    fn stop(
        &mut self,
        session_id: &SessionId,
        reason: StopReason,
    ) -> Result<StopReport, Self::Error> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| DaemonError::SessionNotFound(session_id.as_str().to_owned()))?;

        match session.state {
            SessionState::Preparing | SessionState::Running | SessionState::Failed => {
                session.state = SessionState::Stopping;
                let report = stop_report_for_reason(reason);
                session.state = SessionState::Stopped;
                Ok(report)
            }
            SessionState::Stopped => Ok(StopReport::default()),
            SessionState::Stopping => Err(DaemonError::InvalidTransition {
                session_id: session_id.as_str().to_owned(),
                from: SessionState::Stopping,
                to: SessionState::Stopped,
            }),
        }
    }

    fn state(&self, session_id: &SessionId) -> Result<SessionState, Self::Error> {
        self.sessions
            .get(session_id)
            .map(RuntimeSession::state)
            .ok_or_else(|| DaemonError::SessionNotFound(session_id.as_str().to_owned()))
    }
}

fn stop_report_for_reason(_reason: StopReason) -> StopReport {
    StopReport {
        contacts_cancelled: 0,
        leases_released: 0,
        settings_restored: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wroid_core::profile_v2::{ActionV2, BindingV2, InputV2, JoystickMode, NormalizedPoint};
    use wroid_core::{ControlProfile, Point, Resolution};
    use wroid_runtime::{RuntimeControlAction, SessionId};

    fn valid_request(session_id: &str) -> SessionRequest {
        let profile = ControlProfile {
            name: "Test Game".to_owned(),
            package_name: "com.example.game".to_owned(),
            resolution: Resolution {
                width: 1920,
                height: 1080,
            },
            bindings: Vec::new(),
        };
        let resolution = profile.resolution;

        SessionRequest {
            session_id: SessionId::new(session_id).unwrap(),
            profile,
            display: DisplayInfo::new(resolution),
            launch_package: true,
        }
    }

    fn profile_v2() -> ProfileV2 {
        ProfileV2 {
            schema_version: 2,
            name: "Shooter v2".to_owned(),
            package_name: "com.example.shooter".to_owned(),
            orientation: Default::default(),
            layers: Vec::new(),
            bindings: vec![
                BindingV2 {
                    name: "movement".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::KeyCluster {
                        up: "w".to_owned(),
                        left: "a".to_owned(),
                        down: "s".to_owned(),
                        right: "d".to_owned(),
                    },
                    action: ActionV2::VirtualJoystick {
                        center: NormalizedPoint { x: 0.18, y: 0.78 },
                        radius: 0.09,
                        dead_zone: 0.02,
                        mode: JoystickMode::Hold,
                        reaffirm_ms: Some(50),
                        slow_key: None,
                        slow_scale: None,
                    },
                },
                BindingV2 {
                    name: "fire".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseButton {
                        button: "left".to_owned(),
                    },
                    action: ActionV2::Hold {
                        point: NormalizedPoint { x: 0.86, y: 0.50 },
                    },
                },
            ],
        }
    }

    #[test]
    fn prepares_valid_session() {
        let mut manager = DaemonSessionManager::new();
        let request = valid_request("session-1");
        let session_id = request.session_id.clone();

        let prepared = manager.prepare(request).unwrap();

        assert_eq!(prepared.session_id, session_id);
        assert_eq!(prepared.state, SessionState::Preparing);
        assert_eq!(prepared.active_package, "com.example.game");
        assert_eq!(manager.session_count(), 1);
        assert!(manager.has_session(&session_id));
        assert!(manager
            .session(&session_id)
            .unwrap()
            .control_plan()
            .is_none());
    }

    #[test]
    fn prepares_profile_v2_session_with_runtime_controls() {
        let mut manager = DaemonSessionManager::new();
        let session_id = SessionId::new("v2-session").unwrap();
        let resolution = Resolution {
            width: 1920,
            height: 1080,
        };

        let prepared = manager
            .prepare_profile_v2(
                session_id.clone(),
                profile_v2(),
                DisplayInfo::new(resolution),
                true,
            )
            .unwrap();

        assert_eq!(prepared.session_id, session_id);
        assert_eq!(prepared.state, SessionState::Preparing);
        assert_eq!(prepared.active_package, "com.example.shooter");

        let session = manager.session(&session_id).unwrap();
        assert_eq!(session.active_package(), "com.example.shooter");
        assert!(session.launch_package());
        let controls = session.control_plan().unwrap();
        assert_eq!(controls.controls.len(), 2);
        assert_eq!(controls.resolution, resolution);
        assert!(matches!(
            &controls.control("movement").unwrap().action,
            RuntimeControlAction::VirtualJoystick { .. }
        ));
        assert_eq!(
            &controls.control("fire").unwrap().action,
            &RuntimeControlAction::Hold {
                point: Point { x: 1650, y: 540 }
            }
        );
    }

    #[test]
    fn rejects_invalid_profile() {
        let mut request = valid_request("invalid-profile");
        request.profile.package_name.clear();
        let mut manager = DaemonSessionManager::new();

        let error = manager.prepare(request).unwrap_err();

        assert!(matches!(error, DaemonError::InvalidProfile(_)));
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn rejects_profile_v2_with_unsupported_controls() {
        let mut profile = profile_v2();
        profile.bindings[0].action = ActionV2::Macro { steps: Vec::new() };
        let mut manager = DaemonSessionManager::new();

        let error = manager
            .prepare_profile_v2(
                SessionId::new("bad-v2").unwrap(),
                profile,
                DisplayInfo::new(Resolution {
                    width: 1920,
                    height: 1080,
                }),
                true,
            )
            .unwrap_err();

        assert!(matches!(error, DaemonError::InvalidRuntimeControls(_)));
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn start_requires_prepared_state() {
        let mut manager = DaemonSessionManager::new();
        let request = valid_request("session-2");
        let session_id = request.session_id.clone();
        manager.prepare(request).unwrap();

        manager.start(&session_id).unwrap();
        assert_eq!(manager.state(&session_id).unwrap(), SessionState::Running);

        let error = manager.start(&session_id).unwrap_err();
        assert!(matches!(
            error,
            DaemonError::InvalidTransition {
                from: SessionState::Running,
                to: SessionState::Running,
                ..
            }
        ));
    }

    #[test]
    fn stop_moves_running_session_to_stopped() {
        let mut manager = DaemonSessionManager::new();
        let request = valid_request("session-3");
        let session_id = request.session_id.clone();
        manager.prepare(request).unwrap();
        manager.start(&session_id).unwrap();

        let report = manager
            .stop(&session_id, StopReason::UserRequested)
            .unwrap();

        assert!(report.settings_restored);
        assert_eq!(manager.state(&session_id).unwrap(), SessionState::Stopped);
    }

    #[test]
    fn duplicate_prepare_is_rejected() {
        let mut manager = DaemonSessionManager::new();
        manager.prepare(valid_request("dup")).unwrap();

        let error = manager.prepare(valid_request("dup")).unwrap_err();

        assert!(matches!(error, DaemonError::SessionAlreadyExists(_)));
    }

    #[test]
    fn managed_process_state_tracks_pid_stop_and_clean_exit() {
        let mut manager = DaemonSessionManager::new();
        let session_id = SessionId::new("managed-clean").unwrap();
        manager.prepare(valid_request("managed-clean")).unwrap();

        manager.mark_running(&session_id, 4242).unwrap();
        assert_eq!(
            manager.session(&session_id).unwrap().process_id(),
            Some(4242)
        );

        manager.mark_stopping(&session_id).unwrap();
        assert_eq!(manager.state(&session_id).unwrap(), SessionState::Stopping);

        manager
            .mark_exited(&session_id, true, "exit status: 0")
            .unwrap();
        let session = manager.session(&session_id).unwrap();
        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.process_id(), None);
        assert_eq!(session.detail(), Some("exit status: 0"));
    }

    #[test]
    fn managed_process_failure_is_bounded() {
        let mut manager = DaemonSessionManager::new();
        let session_id = SessionId::new("managed-failed").unwrap();
        manager.prepare(valid_request("managed-failed")).unwrap();
        manager.mark_running(&session_id, 4343).unwrap();

        manager
            .mark_exited(&session_id, false, &"x".repeat(10_000))
            .unwrap();

        let session = manager.session(&session_id).unwrap();
        assert_eq!(session.state(), SessionState::Failed);
        assert_eq!(session.process_id(), None);
        assert!(session.detail().unwrap().chars().count() <= 4096);
    }
}
