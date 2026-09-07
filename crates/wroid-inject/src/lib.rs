//! Persistent Linux input injection for the Wroid gaming runtime.
//!
//! The hot path keeps one Linux virtual input device open and translates
//! validated touch frames into multitouch protocol Type-B event batches.
//! No subprocess is spawned per event or per frame.

mod bridge_broker;
mod config;
mod cursor_overlay;
mod error;
mod event;
mod game_session;
mod injector;
mod kwin_pointer_isolation;
#[allow(private_interfaces)]
mod live_keyboard;
mod live_mouse_aim;
mod privileged_bridge;
mod sink;
mod state;
mod transition;
mod translate;
mod waydroid_bridge;
mod waydroid_session;

pub use bridge_broker::{
    serve_bridge_broker, BridgeBrokerClient, BridgeHelperFactory, BridgeHelperSession,
    ProductionBridgeHelperFactory, BRIDGE_PROTOCOL_VERSION, BRIDGE_WORKER_FD,
    BRIDGE_WORKER_PROTOCOL_GENERATION,
};
pub use config::{
    DeviceConfig, DeviceConfigError, DEFAULT_SLOT_COUNT, MAX_SLOT_COUNT, WROID_TOUCHSCREEN_NAME,
    WROID_TOUCHSCREEN_PRODUCT, WROID_TOUCHSCREEN_VENDOR,
};
pub use error::UinputFrameError;
pub use event::{EventSink, LinuxInputEvent};
pub use game_session::{
    run_game_session, run_game_session_cli, GameSessionOptions, GameSessionReport,
    GameSessionResult, LatencyMetrics,
};
pub use injector::UinputTouchInjector;
pub use live_keyboard::{
    cleanup_live_keyboard_bridge, default_joystick_center, default_joystick_radius,
    parse_live_keyboard_command, print_live_keyboard_usage, run_live_keyboard_cli,
    run_live_keyboard_session, KeyTapBinding, LiveKeyboardCommand, LiveKeyboardOptions,
    DEFAULT_HOLD_LOG_INTERVAL, DEFAULT_LIVE_HEIGHT, DEFAULT_LIVE_WIDTH, DEFAULT_READY_DELAY,
    DEFAULT_REAFFIRM_INTERVAL,
};
pub use live_mouse_aim::{
    cleanup_live_mouse_aim_bridge, default_mouse_aim_origin, parse_live_mouse_aim_command,
    print_live_mouse_aim_usage, run_live_mouse_aim_cli, run_live_mouse_aim_session,
    LiveMouseAimCommand, LiveMouseAimOptions, MouseAimAction, MouseAimBinding, MouseAimController,
    DEFAULT_MOUSE_AIM_HEIGHT, DEFAULT_MOUSE_AIM_READY_DELAY, DEFAULT_MOUSE_AIM_WIDTH,
};
pub use privileged_bridge::{
    run_privileged_bridge_helper, run_privileged_bridge_helper_check,
    validate_installed_bridge_helper, BridgeHelperCommand, PrivilegedBridgeHelper,
    DEFAULT_PRIVILEGED_BRIDGE_HELPER,
};
pub use sink::EvdevEventSink;
pub use waydroid_bridge::{
    active_bridge_lease_owner, active_default_bridge_lease_owner, remove_bridge,
    remove_default_bridge, render_bridge_config, validate_wroid_touchscreen_node, CgroupMode,
    InputDeviceNode, InstalledWaydroidBridge, WaydroidBridgeLease, WaydroidBridgePaths,
    DEFAULT_WAYDROID_BRIDGE_CONFIG, DEFAULT_WAYDROID_BRIDGE_LOCK, DEFAULT_WAYDROID_CONFIG,
};
pub use waydroid_session::{
    ensure_container_stopped, ensure_root, gamescope_is_available, presentation_for_game,
    spawn_android_getevent_trace, stop_child, wait_for_android_boot_completed,
    wait_for_android_display_size, wait_for_android_input_device, wait_for_android_input_reader,
    DesktopUser, DesktopWaydroidSession, WaydroidPresentation,
};

#[cfg(test)]
mod tests;
