//! Daemon-backed session commands.
//!
//! Profile preparation is sent to the private per-user `wroidd` protocol. No
//! input capture, injection, or Waydroid lifecycle work happens here yet:
//! preparation only materializes and records a runtime control plan.

use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(test)]
use std::fmt::Write as _;
#[cfg(test)]
use wroid_core::profile_v2::InputV2;
use wroid_core::profile_v2::ProfileV2;
use wroid_core::Resolution;
use wroid_daemon::ipc::{DaemonRequest, DaemonResult};
#[cfg(test)]
use wroid_daemon::DaemonSessionManager;
#[cfg(test)]
use wroid_runtime::{
    DisplayInfo, HostKeyName, LayerId, PreparedSession, RuntimeControlAction, RuntimeControlPlan,
    SessionId,
};

use crate::output;

use super::runtime_daemon;

/// Load a profile v2 document and prepare a daemon-managed runtime control plan.
pub(crate) fn prepare_v2(
    profile_path: PathBuf,
    resolution: Resolution,
    session_id: String,
    no_launch: bool,
) -> Result<()> {
    let profile = ProfileV2::load_from_path(&profile_path)
        .with_context(|| format!("failed to load profile v2 from {}", profile_path.display()))?;
    if let Err(error) = profile.validate() {
        anyhow::bail!("invalid profile v2:\n  - {}", error.errors.join("\n  - "));
    }
    let client = runtime_daemon::ensure_running()
        .context("failed to start the per-user Wroid runtime daemon")?;
    let result = client
        .request(DaemonRequest::PrepareProfileV2 {
            session_id: session_id.clone(),
            profile,
            width: resolution.width,
            height: resolution.height,
            launch_package: !no_launch,
        })
        .with_context(|| format!("failed to prepare session '{session_id}' through wroidd"))?;
    let DaemonResult::Session { session } = result else {
        anyhow::bail!("wroidd returned an unexpected response to session preparation");
    };
    output::write_stdout(&format!(
        "Prepared session: {}\nState: {:?}\nPackage: {}\nLaunch package: {}\nControls: {}\n",
        session.session_id,
        session.state,
        session.package_name,
        session.launch_package,
        session.control_count,
    ))
}

/// Prepare the session in an in-memory daemon and render a human-readable report.
///
/// Kept separate from [`prepare_v2`] so tests can assert the rendered control
/// plan and unsupported-action failure paths without touching the filesystem.
#[cfg(test)]
fn prepare_v2_summary(
    profile: ProfileV2,
    resolution: Resolution,
    session_id: &str,
    launch_package: bool,
) -> Result<String> {
    if let Err(error) = profile.validate() {
        anyhow::bail!("invalid profile v2:\n  - {}", error.errors.join("\n  - "));
    }

    let session_id = SessionId::new(session_id).context("invalid session id")?;

    let mut manager = DaemonSessionManager::new();
    let prepared = manager.prepare_profile_v2(
        session_id.clone(),
        profile,
        DisplayInfo::new(resolution),
        launch_package,
    )?;

    let plan = manager
        .session(&prepared.session_id)
        .and_then(|session| session.control_plan())
        .context("prepared session is missing its control plan")?;

    Ok(render_prepared(&prepared, plan, launch_package))
}

#[cfg(test)]
fn render_prepared(
    prepared: &PreparedSession,
    plan: &RuntimeControlPlan,
    launch_package: bool,
) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "Prepared session: {}", prepared.session_id.as_str());
    let _ = writeln!(output, "State: {:?}", prepared.state);
    let _ = writeln!(output, "Profile: {}", plan.profile_name);
    let _ = writeln!(output, "Package: {}", plan.package_name);
    let _ = writeln!(output, "Launch package: {launch_package}");
    let _ = writeln!(
        output,
        "Resolution: {}x{}",
        plan.resolution.width, plan.resolution.height
    );
    let _ = writeln!(output, "Controls ({}):", plan.controls.len());
    for control in &plan.controls {
        let layer = if control.layer == LayerId::BASE {
            "base"
        } else {
            plan.layers
                .iter()
                .find(|layer| layer.id == control.layer)
                .map(|layer| layer.name.as_str())
                .unwrap_or("unknown")
        };
        let _ = writeln!(
            output,
            "  {} [layer:{} {}] -> {}",
            control.name,
            layer,
            describe_input(&control.input, control.modifier),
            describe_action(&control.action),
        );
    }
    output
}

#[cfg(test)]
fn describe_input(input: &InputV2, modifier: Option<HostKeyName>) -> String {
    let input = match input {
        InputV2::Key { key } => format!("key:{key}"),
        InputV2::KeyCluster {
            up,
            left,
            down,
            right,
        } => format!("key_cluster:{up}{left}{down}{right}"),
        InputV2::MouseButton { button } => format!("mouse_button:{button}"),
        InputV2::MouseMove => "mouse_move".to_owned(),
    };
    match modifier {
        Some(modifier) => format!("{}+{input}", modifier.profile_name()),
        None => input,
    }
}

#[cfg(test)]
fn describe_action(action: &RuntimeControlAction) -> String {
    match action {
        RuntimeControlAction::Tap { point, .. } => format!("tap ({},{})", point.x, point.y),
        RuntimeControlAction::Hold { point } => format!("hold ({},{})", point.x, point.y),
        RuntimeControlAction::VirtualJoystick {
            joystick,
            mode,
            reaffirm_interval,
            ..
        } => format!(
            "virtual_joystick center=({},{}) radius={} dead_zone={} contact={} mode={mode:?} reaffirm_ms={:?}",
            joystick.center().x,
            joystick.center().y,
            joystick.radius(),
            joystick.dead_zone(),
            joystick.contact_id().get(),
            reaffirm_interval.map(|value| value.as_millis()),
        ),
        RuntimeControlAction::MouseAim { aim, settings } => format!(
            "mouse_aim origin=({},{}) region=({},{})-({},{}) contacts={}/{} toggle={}",
            aim.origin().x,
            aim.origin().y,
            aim.region().left,
            aim.region().top,
            aim.region().right,
            aim.region().bottom,
            aim.contact_id().get(),
            settings.alternate_contact_id.get(),
            settings.toggle_key.as_deref().unwrap_or("always"),
        ),
        RuntimeControlAction::RadialSelect { center, target } => {
            format!(
                "radial_select center=({},{}) target=({},{})",
                center.x, center.y, target.x, target.y
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wroid_core::profile_v2::{
        ActionV2, BindingV2, LayerActivation, LayerV2, NormalizedPoint, NormalizedRect,
    };

    fn supported_profile() -> ProfileV2 {
        ProfileV2 {
            schema_version: 2,
            name: "Shooter v2".to_owned(),
            package_name: "com.example.shooter".to_owned(),
            orientation: Default::default(),
            layers: vec![],
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
                        mode: Default::default(),
                        reaffirm_ms: Some(50),
                        slow_key: None,
                        slow_scale: None,
                        slow_mode: Default::default(),
                    },
                },
                BindingV2 {
                    name: "aim".to_owned(),
                    layer: None,
                    modifier: None,
                    input: InputV2::MouseMove,
                    action: ActionV2::MouseAim {
                        region: NormalizedRect {
                            x: 0.35,
                            y: 0.06,
                            w: 0.60,
                            h: 0.78,
                        },
                        sensitivity: 1.2,
                        toggle_key: Some("tab".to_owned()),
                        recenter_threshold: 0.7,
                        recenter_gap_ms: 0,
                        ads_multiplier: Some(0.6),
                        reaffirm_ms: Some(50),
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

    fn resolution() -> Resolution {
        Resolution {
            width: 1920,
            height: 1080,
        }
    }

    #[test]
    fn prepares_supported_profile_into_control_plan_summary() {
        let summary =
            prepare_v2_summary(supported_profile(), resolution(), "shooter", true).unwrap();

        assert!(summary.contains("Prepared session: shooter"));
        assert!(summary.contains("State: Preparing"));
        assert!(summary.contains("Package: com.example.shooter"));
        assert!(summary.contains("Launch package: true"));
        assert!(summary.contains("Resolution: 1920x1080"));
        assert!(summary.contains("Controls (3):"));
        assert!(summary.contains(
            "movement [layer:base key_cluster:wasd] -> virtual_joystick center=(345,842) radius=97 dead_zone=22 contact=1"
        ));
        assert!(summary.contains(
            "aim [layer:base mouse_move] -> mouse_aim origin=(1247,485) region=(672,65)-(1823,906) contacts=2/3 toggle=tab"
        ));
        assert!(summary.contains("fire [layer:base mouse_button:left] -> hold (1650,540)"));
    }

    #[test]
    fn no_launch_is_reflected_in_summary() {
        let summary =
            prepare_v2_summary(supported_profile(), resolution(), "shooter", false).unwrap();

        assert!(summary.contains("Launch package: false"));
    }

    #[test]
    fn prepared_summary_names_base_layer_and_modifier_chords() {
        let mut profile = supported_profile();
        profile.layers.push(LayerV2 {
            name: "grenades".to_owned(),
            activation: LayerActivation::Hold {
                key: "g".to_owned(),
            },
        });
        profile.bindings.push(BindingV2 {
            name: "frag".to_owned(),
            layer: Some("grenades".to_owned()),
            modifier: Some("shift".to_owned()),
            input: InputV2::Key {
                key: "1".to_owned(),
            },
            action: ActionV2::Tap {
                point: NormalizedPoint { x: 0.7, y: 0.3 },
                hold: false,
            },
        });

        let summary = prepare_v2_summary(profile, resolution(), "layered", true).unwrap();

        assert!(summary.contains("movement [layer:base key_cluster:wasd]"));
        assert!(summary.contains("frag [layer:grenades shift+key:1] -> tap (1343,324)"));
    }

    #[test]
    fn rejects_unsupported_macro_action_with_clear_error() {
        let mut profile = supported_profile();
        profile.bindings[0].action = ActionV2::Macro {
            steps: vec![ActionV2::Tap {
                point: NormalizedPoint { x: 0.5, y: 0.5 },
                hold: false,
            }],
        };

        let error = prepare_v2_summary(profile, resolution(), "shooter", true).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("unsupported runtime action kind: macro"),
            "unexpected error message: {message}"
        );
        assert!(
            message.contains("movement"),
            "error should name the offending binding: {message}"
        );
    }

    #[test]
    fn rejects_invalid_profile_with_detailed_messages() {
        let mut profile = supported_profile();
        profile.package_name.clear();

        let error = prepare_v2_summary(profile, resolution(), "shooter", true).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("invalid profile v2"),
            "unexpected error message: {message}"
        );
    }

    #[test]
    fn rejects_empty_session_id() {
        let error = prepare_v2_summary(supported_profile(), resolution(), "   ", true).unwrap_err();
        let message = format!("{error:#}");

        assert!(
            message.contains("invalid session id"),
            "unexpected error message: {message}"
        );
    }
}
