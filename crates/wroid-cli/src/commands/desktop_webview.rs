use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::{anyhow, Result};
use gtk::gio::ApplicationFlags;
use gtk::glib::translate::ToGlibPtr;
use gtk::glib::{self, Cast, ControlFlow, Propagation};
use gtk::prelude::*;
use webkit2gtk::{
    NavigationPolicyDecision, NavigationPolicyDecisionExt, PermissionRequestExt, PolicyDecisionExt,
    PolicyDecisionType, SettingsExt as WebKitSettingsExt, URIRequestExt,
    UserMediaPermissionRequest, UserMediaPermissionRequestExt, WebView, WebViewExt,
};

use super::local_web_app::{LocalOrigin, LocalWebApp};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NativeAppSpec {
    pub(crate) application_id: Option<&'static str>,
    pub(crate) title: &'static str,
    pub(crate) non_unique: bool,
    pub(crate) default_size: (i32, i32),
    pub(crate) minimum_size: (i32, i32),
}

pub(crate) const HUB_WINDOW: NativeAppSpec = NativeAppSpec {
    application_id: Some("io.wroid.GamingHub"),
    title: "Wroid Gaming Hub",
    non_unique: false,
    default_size: (1280, 800),
    minimum_size: (1024, 640),
};

pub(crate) const CONTROLS_WINDOW: NativeAppSpec = NativeAppSpec {
    application_id: None,
    title: "Wroid Controls Studio",
    non_unique: true,
    default_size: (1280, 800),
    minimum_size: (1024, 640),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationDecision {
    Allow,
    Block,
}

fn navigation_decision(origin: &LocalOrigin, uri: &str, new_window: bool) -> NavigationDecision {
    if !new_window && origin.allows_uri(uri) {
        NavigationDecision::Allow
    } else {
        NavigationDecision::Block
    }
}

struct NativeSession {
    window: gtk::ApplicationWindow,
    server: LocalWebApp,
}

pub(crate) fn run_native_app<F>(spec: NativeAppSpec, start_server: F) -> Result<()>
where
    F: FnOnce() -> Result<LocalWebApp> + 'static,
{
    let application = gtk::Application::new(spec.application_id, application_flags(spec));
    let factory = Rc::new(RefCell::new(Some(start_server)));
    let session = Rc::new(RefCell::new(None::<NativeSession>));
    let shell_error = Rc::new(RefCell::new(None::<anyhow::Error>));

    let activate_factory = Rc::clone(&factory);
    let activate_session = Rc::clone(&session);
    let activate_error = Rc::clone(&shell_error);
    application.connect_activate(move |application| {
        if let Some(active) = activate_session.borrow().as_ref() {
            active.window.present();
            return;
        }

        let Some(start_server) = activate_factory.borrow_mut().take() else {
            return;
        };
        let server = match start_server() {
            Ok(server) => server,
            Err(error) => {
                let message = format!("{error:#}");
                *activate_error.borrow_mut() = Some(error);
                show_error_dialog(None, application, "Wroid could not start", &message, None);
                return;
            }
        };

        let window = gtk::ApplicationWindow::new(application);
        window.set_title(spec.title);
        window.set_default_size(spec.default_size.0, spec.default_size.1);

        let webview = WebView::new();
        webview.set_size_request(spec.minimum_size.0, spec.minimum_size.1);
        configure_webview(&webview, server.origin());

        let shutdown = server.shutdown_signal();
        let close_shutdown = shutdown.clone();
        window.connect_delete_event(move |_, _| {
            close_shutdown.store(true, Ordering::Release);
            Propagation::Proceed
        });

        let load_shutdown = shutdown.clone();
        let load_error = Rc::clone(&activate_error);
        let load_application = application.clone();
        let load_window = window.clone();
        webview.connect_load_failed(move |_, _, uri, error| {
            if load_shutdown.load(Ordering::Acquire) {
                return true;
            }
            let error = anyhow!("failed to load native Wroid page {uri}: {error}");
            let message = format!("{error:#}");
            *load_error.borrow_mut() = Some(error);
            load_shutdown.store(true, Ordering::Release);
            show_error_dialog(
                Some(&load_window),
                &load_application,
                "Wroid page failed to load",
                &message,
                Some(load_window.clone()),
            );
            true
        });

        window.add(&webview);
        *activate_session.borrow_mut() = Some(NativeSession {
            window: window.clone(),
            server,
        });

        let poll_window = window.clone();
        let poll_error = Rc::clone(&activate_error);
        let poll_session = Rc::clone(&activate_session);
        glib::timeout_add_local(Duration::from_millis(50), move || {
            if !poll_window.is_visible() {
                return ControlFlow::Break;
            }
            let shutdown = poll_session
                .borrow()
                .as_ref()
                .is_none_or(|session| session.server.is_shutdown());
            if shutdown && poll_error.borrow().is_none() {
                poll_window.close();
                return ControlFlow::Break;
            }
            ControlFlow::Continue
        });

        webview.load_uri(
            &activate_session
                .borrow()
                .as_ref()
                .unwrap()
                .server
                .authenticated_url(),
        );
        window.show_all();
    });

    let _exit_code = application.run_with_args(&["wroid"]);
    let server = session.borrow_mut().take().map(|session| session.server);
    let result = shell_error.borrow_mut().take().map_or(Ok(()), Err);
    finish_shell_session(server, result)
}

fn application_flags(spec: NativeAppSpec) -> ApplicationFlags {
    if spec.non_unique {
        ApplicationFlags::NON_UNIQUE
    } else {
        ApplicationFlags::empty()
    }
}

fn configure_webview(webview: &WebView, origin: &LocalOrigin) {
    if let Some(settings) = WebViewExt::settings(webview) {
        settings.set_enable_developer_extras(cfg!(debug_assertions));
        // navigator.mediaDevices.getDisplayMedia (the controls editor's live
        // window capture) only exists when the WebKit media-stream setting is
        // enabled; without it the capture button reports an unsupported
        // browser.
        settings.set_enable_media_stream(true);
        // The controls editor has no devtools in release builds; surface its
        // JavaScript console (capture failures, portal errors) on the hub
        // process stderr so window-capture misbehavior is diagnosable.
        settings.set_enable_write_console_messages_to_stdout(true);
    }
    webview.connect_create(|_, _| None);
    webview.connect_context_menu(|_, _, _, _| true);
    // WebKitGTK turns the mouse's back/forward buttons (GDK 8/9) into
    // history navigation before the page can see them, which broke binding
    // actions by pressing side buttons in the controls editor. Swallow them
    // at the GTK layer so the page never loses focus or state to navigation;
    // bindings pick mouse buttons through the editor's picker chips.
    webview.connect_button_press_event(|_, event| {
        if event.button() == 8 || event.button() == 9 {
            Propagation::Stop
        } else {
            Propagation::Proceed
        }
    });
    webview.connect_permission_request(|_, request| {
        let user_media = request
            .clone()
            .dynamic_cast::<UserMediaPermissionRequest>()
            .ok();
        if user_media.as_ref().is_some_and(|request| {
            allow_user_media(
                request.is_for_video_device(),
                request.is_for_audio_device(),
                is_for_display_device(request),
            )
        }) {
            request.allow();
        } else {
            request.deny();
        }
        true
    });

    let origin = origin.clone();
    webview.connect_decide_policy(move |_, decision, decision_type| {
        if !matches!(
            decision_type,
            PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction
        ) {
            return false;
        }
        let uri = decision
            .clone()
            .dynamic_cast::<NavigationPolicyDecision>()
            .ok()
            .and_then(|navigation| navigation.navigation_action())
            .and_then(|action| action.request())
            .and_then(|request| request.uri());
        let new_window = decision_type == PolicyDecisionType::NewWindowAction;
        match uri
            .as_deref()
            .map(|uri| navigation_decision(&origin, uri, new_window))
        {
            Some(NavigationDecision::Allow) => decision.use_(),
            _ => decision.ignore(),
        }
        true
    });
}

fn is_for_display_device(request: &UserMediaPermissionRequest) -> bool {
    unsafe {
        webkit2gtk::ffi::webkit_user_media_permission_is_for_display_device(
            request.to_glib_none().0,
        ) != 0
    }
}

fn allow_user_media(_video: bool, audio: bool, display: bool) -> bool {
    display && !audio
}

fn show_error_dialog(
    parent: Option<&gtk::ApplicationWindow>,
    application: &gtk::Application,
    title: &str,
    message: &str,
    close_window: Option<gtk::ApplicationWindow>,
) {
    let mut builder = gtk::MessageDialog::builder()
        .modal(true)
        .message_type(gtk::MessageType::Error)
        .buttons(gtk::ButtonsType::Close)
        .text(title)
        .secondary_text(message);
    if let Some(parent) = parent {
        builder = builder.transient_for(parent);
    }
    let dialog = builder.build();
    application.add_window(&dialog);
    let application = application.clone();
    dialog.connect_response(move |dialog, _| {
        dialog.close();
        if let Some(window) = close_window.as_ref() {
            window.close();
        } else {
            application.quit();
        }
    });
    dialog.show_all();
}

fn finish_shell_session(server: Option<LocalWebApp>, shell_result: Result<()>) -> Result<()> {
    let cleanup_result = server.map_or(Ok(()), LocalWebApp::shutdown_and_join);
    match (shell_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(shell_error), Err(cleanup_error)) => Err(anyhow!(
            "{shell_error:#}; additionally failed to stop local server: {cleanup_error:#}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use anyhow::anyhow;

    use super::{
        allow_user_media, application_flags, finish_shell_session, navigation_decision,
        NavigationDecision, CONTROLS_WINDOW, HUB_WINDOW,
    };
    use crate::commands::local_web_app::{LocalOrigin, LocalWebApp};

    #[test]
    fn hub_is_single_instance_and_editor_is_not() {
        assert_eq!(HUB_WINDOW.application_id, Some("io.wroid.GamingHub"));
        assert!(!application_flags(HUB_WINDOW).contains(gtk::gio::ApplicationFlags::NON_UNIQUE));
        assert_eq!(CONTROLS_WINDOW.application_id, None);
        assert!(application_flags(CONTROLS_WINDOW).contains(gtk::gio::ApplicationFlags::NON_UNIQUE));
    }

    #[test]
    fn native_windows_use_the_desktop_size_contract() {
        assert_eq!(HUB_WINDOW.default_size, (1280, 800));
        assert_eq!(HUB_WINDOW.minimum_size, (1024, 640));
        assert_eq!(CONTROLS_WINDOW.default_size, (1280, 800));
        assert_eq!(CONTROLS_WINDOW.minimum_size, (1024, 640));
    }

    #[test]
    fn navigation_rejects_popups_and_foreign_origins() {
        let origin = LocalOrigin::new("127.0.0.1:37613".parse().unwrap()).unwrap();

        assert_eq!(
            navigation_decision(&origin, "http://127.0.0.1:37613/", false),
            NavigationDecision::Allow
        );
        assert_eq!(
            navigation_decision(&origin, "http://127.0.0.1:37614/", false),
            NavigationDecision::Block
        );
        assert_eq!(
            navigation_decision(&origin, "http://127.0.0.1:37613/", true),
            NavigationDecision::Block
        );
    }

    #[test]
    fn permission_policy_allows_only_silent_display_capture() {
        assert!(allow_user_media(false, false, true));
        assert!(allow_user_media(true, false, true));
        assert!(!allow_user_media(true, false, false));
        assert!(!allow_user_media(false, false, false));
        assert!(!allow_user_media(false, true, true));
    }

    #[test]
    fn shell_failure_still_stops_the_server() {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let app = LocalWebApp::spawn(
            "127.0.0.1:37613".parse().unwrap(),
            "token".to_owned(),
            shutdown,
            move || {
                while !worker_shutdown.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(1));
                }
                worker_stopped.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap();

        let error = finish_shell_session(Some(app), Err(anyhow!("WebView initialization failed")))
            .unwrap_err();

        assert!(error.to_string().contains("WebView initialization failed"));
        assert!(stopped.load(Ordering::Acquire));
    }
}
