pub(crate) mod app;
pub(crate) mod compatibility;
pub(crate) mod desktop;
pub(crate) mod desktop_webview;
pub(crate) mod device;
pub(crate) mod doctor;
pub(crate) mod editor;
pub(crate) mod game_catalog;
pub(crate) mod graphics;
pub(crate) mod hub;
pub(crate) mod input;
pub(crate) mod kwin_focus;
pub(crate) mod kwin_screenshot;
pub(crate) mod launch_v2;
pub(crate) mod local_web_app;
pub(crate) mod play_v2;
pub(crate) mod preferences;
pub(crate) mod profile;
pub(crate) mod run;
pub(crate) mod runtime_daemon;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod system_helper;
pub(crate) mod terminal;

use anyhow::Result;
use wroid_core::Resolution;

use crate::backend::InputExecutor;
use crate::cli::{
    AppCommand, BindingCommand, Cli, Commands, DaemonCommand, DesktopCommand, DeviceCommand,
    HelperCommand, InputCommand, ProfileCommand, SessionCommand,
};
use local_web_app::WebUiMode;

pub(crate) fn run(cli: Cli, input_executor: &impl InputExecutor) -> Result<()> {
    match cli.command {
        Commands::Desktop { command } => match command {
            DesktopCommand::Install => desktop::install(),
            DesktopCommand::Status => desktop::status(),
            DesktopCommand::Uninstall => desktop::uninstall(),
        },
        Commands::Helper { command } => match command {
            HelperCommand::Install => system_helper::install(),
            HelperCommand::Status => system_helper::status(),
        },
        Commands::Performance { json, setup_gpu } => graphics::print_report(json, setup_gpu),
        Commands::Compatibility { json, setup } => compatibility::run(json, setup),
        Commands::SetupWaydroidHelper { installer } => {
            compatibility::install_and_open_helper(&installer)
        }
        Commands::InstallHelperGraphical => system_helper::install_graphical(),
        Commands::InstallApkWorker { ticket } => hub::install_apk_worker(input_executor, &ticket),
        Commands::SetupWaydroidGpu { device } => graphics::setup_gpu_interactive(&device),
        Commands::ConfigureWaydroidGpu { device } => graphics::configure_waydroid_gpu(&device),
        Commands::Hub {
            port,
            browser,
            no_open,
            profiles_dir,
        } => hub::run_hub(port, WebUiMode::from_flags(browser, no_open), profiles_dir),
        Commands::Doctor { backend } => doctor::doctor(input_executor, backend),
        Commands::Profile { command } => match command {
            ProfileCommand::Validate { path } => profile::validate_profile(path),
            ProfileCommand::EditV2 {
                path,
                port,
                browser,
                no_open,
            } => editor::edit_v2(path, port, WebUiMode::from_flags(browser, no_open)),
            ProfileCommand::Example { path } => profile::write_example_profile(path),
            ProfileCommand::New {
                path,
                name,
                package,
                width,
                height,
                force,
            } => profile::create_profile(path, name, package, width, height, force),
            ProfileCommand::NewCurrent {
                path,
                name,
                package,
                backend,
                force,
            } => profile::create_profile_from_current_screen(
                input_executor,
                path,
                name,
                package,
                backend,
                force,
            ),
            ProfileCommand::Scale {
                input_path,
                output_path,
                width,
                height,
                force,
            } => crate::scaling::scale_profile_file(
                input_path,
                output_path,
                Resolution { width, height },
                force,
            ),
            ProfileCommand::ScaleCurrent {
                input_path,
                output_path,
                backend,
                force,
            } => crate::scaling::scale_profile_file_to_current_screen(
                input_executor,
                input_path,
                output_path,
                backend,
                force,
            ),
            ProfileCommand::AddTap {
                path,
                name,
                key,
                x,
                y,
            } => profile::add_tap_binding(path, name, key, x, y),
            ProfileCommand::AddSwipe {
                path,
                name,
                key,
                from,
                to,
                duration_ms,
            } => profile::add_swipe_binding(path, name, key, from, to, duration_ms),
            ProfileCommand::AddJoystick {
                path,
                name,
                up,
                left,
                down,
                right,
                center,
                radius,
                tick_ms,
                swipe_duration_ms,
            } => profile::add_joystick_binding(
                path,
                name,
                up,
                left,
                down,
                right,
                center,
                radius,
                tick_ms,
                swipe_duration_ms,
            ),
            ProfileCommand::RemoveBinding { path, binding_name } => {
                profile::remove_binding(path, &binding_name)
            }
            ProfileCommand::ListBindings { profile_path } => profile::list_bindings(profile_path),
            ProfileCommand::Import { path, id, force } => profile::import_profile(path, id, force),
            ProfileCommand::Export {
                profile_id,
                output_path,
                force,
            } => profile::export_profile(&profile_id, output_path, force),
            ProfileCommand::Remove { profile_id } => profile::remove_profile(&profile_id),
            ProfileCommand::Rename { old_id, new_id } => profile::rename_profile(&old_id, &new_id),
            ProfileCommand::Duplicate {
                source_id,
                target_id,
            } => profile::duplicate_profile(&source_id, &target_id),
            ProfileCommand::RegistryNewCurrent {
                name,
                package,
                backend,
                id,
                force,
            } => profile::registry_create_profile_from_current_screen(
                input_executor,
                name,
                package,
                backend,
                id,
                force,
            ),
            ProfileCommand::List => profile::list_profiles(),
            ProfileCommand::Path { profile_id } => profile::print_profile_path(&profile_id),
            ProfileCommand::Show { profile_id } => profile::show_profile(&profile_id),
        },
        Commands::Device { command } => match command {
            DeviceCommand::Screen { backend } => device::device_screen(input_executor, backend),
            DeviceCommand::Density { backend } => device::device_density(input_executor, backend),
            DeviceCommand::Info { backend } => device::device_info(input_executor, backend),
        },
        Commands::Input { command } => match command {
            InputCommand::Tap { x, y, backend } => input::input_tap(input_executor, backend, x, y),
            InputCommand::Swipe {
                x1,
                y1,
                x2,
                y2,
                duration_ms,
                backend,
            } => input::input_swipe(input_executor, backend, x1, y1, x2, y2, duration_ms),
            InputCommand::Keyevent { code, backend } => {
                input::input_keyevent(input_executor, backend, code)
            }
        },
        Commands::App { command } => match command {
            AppCommand::List { backend } => app::app_list(input_executor, backend),
            AppCommand::Launch {
                package_name,
                backend,
            } => app::app_launch(input_executor, backend, &package_name),
            AppCommand::InstallApk {
                path,
                backend,
                allow_any_extension,
                force_incompatible,
            } => app::app_install_apk(
                input_executor,
                backend,
                path,
                allow_any_extension,
                force_incompatible,
            ),
            AppCommand::Inspect { path, json } => app::app_inspect(path, json),
            AppCommand::Current { backend } => app::app_current(input_executor, backend),
        },
        Commands::Binding { command } => match command {
            BindingCommand::Run {
                profile_path,
                binding_name,
                backend,
                scale_to_current,
            } => input::run_binding(
                input_executor,
                profile_path,
                &binding_name,
                backend,
                scale_to_current,
            ),
        },
        Commands::Session { command } => match command {
            SessionCommand::PrepareV2 {
                profile_path,
                width,
                height,
                session_id,
                no_launch,
            } => session::prepare_v2(
                profile_path,
                Resolution { width, height },
                session_id,
                no_launch,
            ),
        },
        Commands::Daemon { command } => match command {
            DaemonCommand::Start => runtime_daemon::start(),
            DaemonCommand::Status => runtime_daemon::status(),
            DaemonCommand::Sessions => runtime_daemon::sessions(),
        },
        Commands::Play {
            profile_path,
            backend,
            scale_to_current,
        } => run::play(input_executor, profile_path, backend, scale_to_current),
        Commands::PlayV2 {
            profile_path,
            keyboard,
            mouse,
            width,
            height,
            no_grab,
            no_ui,
            no_launch,
            trace_input,
            trace_android_input,
            exit_after_ms,
            focus_socket,
        } => play_v2::play_v2(
            profile_path,
            play_v2::PlayV2Options {
                keyboard,
                mouse,
                resolution: Resolution { width, height },
                grab: !no_grab,
                show_ui: !no_ui,
                launch_package: !no_launch,
                trace_input,
                trace_android_input,
                exit_after: exit_after_ms.map(std::time::Duration::from_millis),
                focus_socket,
            },
        )
        .map(|_| ()),
        Commands::LaunchV2 {
            profile_path,
            keyboard,
            mouse,
            width,
            height,
            no_grab,
            no_ui,
            no_launch,
            trace_input,
            trace_android_input,
            exit_after_seconds,
            daemon_worker,
            bridge_fd,
            daemon_parent_pid,
            exit_after_ms,
        } => launch_v2::launch_v2(
            profile_path,
            play_v2::PlayV2Options {
                keyboard,
                mouse,
                resolution: Resolution { width, height },
                grab: !no_grab,
                show_ui: !no_ui,
                launch_package: !no_launch,
                trace_input,
                trace_android_input,
                exit_after: exit_after_ms
                    .map(std::time::Duration::from_millis)
                    .or_else(|| exit_after_seconds.map(std::time::Duration::from_secs)),
                focus_socket: None,
            },
            daemon_worker.then(|| launch_v2::DaemonWorkerInvocation {
                bridge_fd: bridge_fd.expect("clap requires the daemon bridge descriptor"),
                daemon_parent_pid: daemon_parent_pid.expect("clap requires the daemon parent PID"),
            }),
        ),
        Commands::RestoreDesktopSession { parent_pid, ticket } => {
            launch_v2::restore_desktop_session(parent_pid, &ticket)
        }
        Commands::ResumeStaleDaemon => runtime_daemon::resume_stale_daemon(),
        Commands::Run {
            profile_path,
            backend,
            launch_delay_ms,
            no_launch,
            scale_to_current,
        } => run::run(
            input_executor,
            profile_path,
            backend,
            run::RunOptions {
                launch_delay_ms,
                no_launch,
                scale_to_current,
            },
        ),
        Commands::RunProfile {
            profile_id,
            backend,
            launch_delay_ms,
            no_launch,
            scale_to_current,
        } => run::run_profile(
            input_executor,
            &profile_id,
            backend,
            run::RunOptions {
                launch_delay_ms,
                no_launch,
                scale_to_current,
            },
        ),
    }
}
