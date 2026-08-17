//! Guided setup for the macOS permissions the hotkeys need.
//!
//! CQ Paster needs two separate grants, and neither is optional:
//!
//! * **Accessibility** — required to create the `CGEventTap` at all.
//! * **Input Monitoring** — required to *read* key events through it.
//!
//! Only the first can be requested with a system prompt. The second has no
//! equivalent that reliably surfaces from a background app, so without guidance
//! the app simply does nothing and never says why.
//!
//! # Why this re-runs after every reinstall
//!
//! The build is ad-hoc signed, so its code identity changes with every rebuild.
//! macOS keys these grants to that identity, so a reinstalled CQ Paster does not
//! match the entry already sitting in System Settings. The entry still *looks*
//! enabled while granting nothing, and from here that is indistinguishable from
//! never having been granted — both simply read as "not trusted". So the alerts
//! tell the user to switch an existing entry off and on again, which is what
//! actually rebinds it to the new build.

#[cfg(target_os = "macos")]
pub fn start(app: tauri::AppHandle) {
    macos::start(app);
}

/// Whether the app may read key events yet.
///
/// The hook needs this as well as Accessibility: a tap created before Input
/// Monitoring is granted is built successfully and then never delivers an
/// event, and granting the permission afterwards does not revive it.
#[cfg(target_os = "macos")]
pub fn input_monitoring_granted() -> bool {
    macos::input_monitoring_granted()
}

/// The app's own view of both permissions, for the diagnostics log.
#[cfg(target_os = "macos")]
pub fn report() -> String {
    macos::report()
}

#[cfg(not(target_os = "macos"))]
pub fn start(_app: tauri::AppHandle) {
    // Windows and Linux need no equivalent: the keyboard hook there requires no
    // user-granted permission.
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Duration;

    use objc2_app_kit::{NSAlert, NSApplication, NSWorkspace};
    use objc2_foundation::{MainThreadMarker, NSString, NSURL};
    use tauri::AppHandle;

    /// How often to re-check whether the user has granted something yet.
    const CHECK_EVERY: Duration = Duration::from_secs(2);
    /// Only ever one alert on screen. `runModal` blocks the main thread, so
    /// without this the polling loop would queue a second alert behind the first.
    static ALERT_OPEN: AtomicBool = AtomicBool::new(false);

    #[derive(Clone, Copy, PartialEq)]
    enum Permission {
        Accessibility,
        InputMonitoring,
    }

    impl Permission {
        fn granted(self) -> bool {
            match self {
                Permission::Accessibility => accessibility_trusted(),
                Permission::InputMonitoring => input_monitoring_granted(),
            }
        }

        fn label(self) -> &'static str {
            match self {
                Permission::Accessibility => "Accessibility",
                Permission::InputMonitoring => "Input Monitoring",
            }
        }

        fn title(self) -> &'static str {
            match self {
                Permission::Accessibility => "CQ Paster needs Accessibility access",
                Permission::InputMonitoring => "CQ Paster needs Input Monitoring access",
            }
        }

        /// The body text. Both cases spell out the reinstall path, because a
        /// stale entry is the most common reason someone sees this twice.
        fn body(self) -> &'static str {
            match self {
                Permission::Accessibility => {
                    "Without it, the Cmd+<N>+C and Cmd+<N>+V hotkeys cannot run.\n\n\
                     1. Click \"Open Settings\" below.\n\
                     2. Turn on CQ Paster in the list.\n\n\
                     Already listed? Switch it OFF and ON again — after an update \
                     macOS keeps the old entry, which looks enabled but no longer \
                     works.\n\n\
                     No restart needed; CQ Paster picks it up on its own."
                }
                Permission::InputMonitoring => {
                    "Accessibility is done. One more: Input Monitoring lets CQ Paster \
                     read the number and letter keys in a hotkey.\n\n\
                     1. Click \"Open Settings\" below.\n\
                     2. Turn on CQ Paster in the list.\n\n\
                     Already listed? Switch it OFF and ON again — after an update \
                     macOS keeps the old entry, which looks enabled but no longer \
                     works.\n\n\
                     No restart needed; CQ Paster picks it up on its own."
                }
            }
        }

        /// Deep link straight to the relevant pane, so the user never has to
        /// hunt through System Settings.
        fn settings_url(self) -> &'static str {
            match self {
                Permission::Accessibility => {
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
                }
                Permission::InputMonitoring => {
                    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent"
                }
            }
        }
    }

    pub fn start(app: AppHandle) {
        thread::spawn(move || {
            crate::diag(&format!("permissions at startup: {}", report()));
            // Strictly in order: Input Monitoring is pointless to ask for while
            // the tap cannot be created at all, and two alerts at once is the
            // fastest way to get both ignored.
            guide(&app, Permission::Accessibility);
            guide(&app, Permission::InputMonitoring);
        });
    }

    /// Nag, politely, until this permission is granted.
    fn guide(app: &AppHandle, which: Permission) {
        if which.granted() {
            return;
        }
        // First ask the system itself. For Accessibility this is the familiar
        // "Open System Settings" prompt; for Input Monitoring it is the IOKit
        // request, which may show nothing at all from a background app — hence
        // the alert that follows either way.
        request_from_system(which);

        // Ask once, then wait quietly. Re-prompting on a timer turned out to be
        // unusable: while a permission reads as ungranted for any reason the
        // alert keeps interrupting, including while the user is in System
        // Settings trying to grant it.
        show_alert(app, which);
        while !which.granted() {
            thread::sleep(CHECK_EVERY);
        }
        crate::diag(&format!("permission granted: {}", which.label()));
    }

    /// Put the alert up on the main thread and act on the button.
    fn show_alert(app: &AppHandle, which: Permission) {
        ALERT_OPEN.store(true, Ordering::SeqCst);
        let result = app.run_on_main_thread(move || {
            // Safe: `run_on_main_thread` guarantees exactly that.
            let mtm = unsafe { MainThreadMarker::new_unchecked() };

            // An Accessory app is never frontmost, so its alert would otherwise
            // open behind whatever the user is looking at.
            let ns_app = NSApplication::sharedApplication(mtm);
            #[allow(deprecated)]
            ns_app.activateIgnoringOtherApps(true);

            let alert = NSAlert::new(mtm);
            alert.setMessageText(&NSString::from_str(which.title()));
            alert.setInformativeText(&NSString::from_str(which.body()));
            alert.addButtonWithTitle(&NSString::from_str("Open Settings"));
            alert.addButtonWithTitle(&NSString::from_str("Later"));

            // First button is 1000; anything else means "Later".
            if alert.runModal() == 1000 {
                open_settings(which);
            }
            ALERT_OPEN.store(false, Ordering::SeqCst);
        });
        if result.is_err() {
            // The event loop is gone; nothing left to prompt on.
            ALERT_OPEN.store(false, Ordering::SeqCst);
        }
    }

    fn open_settings(which: Permission) {
        let url = NSString::from_str(which.settings_url());
        if let Some(url) = NSURL::URLWithString(&url) {
            NSWorkspace::sharedWorkspace().openURL(&url);
        }
    }

    /// Ask the system to prompt, where a prompt exists.
    fn request_from_system(which: Permission) {
        match which {
            Permission::Accessibility => {
                let _ = crate::hook::request_accessibility();
            }
            Permission::InputMonitoring => unsafe {
                IOHIDRequestAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT);
            },
        }
    }

    fn accessibility_trusted() -> bool {
        unsafe { AXIsProcessTrusted() }
    }

    /// Everything the app knows about its own permissions, for the log.
    ///
    /// The raw IOKit value matters: 0 granted, 1 denied, 2 unknown. Treating
    /// anything but 0 as "not granted" is a guess until we have seen the real
    /// number from an installed build.
    pub fn report() -> String {
        let hid = unsafe { IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) };
        format!(
            "accessibility={} input_monitoring_raw={} ({})",
            accessibility_trusted(),
            hid,
            match hid {
                0 => "granted",
                1 => "denied",
                2 => "unknown",
                _ => "unexpected",
            }
        )
    }

    /// Whether the app may read key events. Distinct from Accessibility: the tap
    /// is created with one and delivers keystrokes with the other, so missing
    /// this looks exactly like a hook that installed fine and then went silent.
    pub fn input_monitoring_granted() -> bool {
        unsafe { IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) == K_IOHID_ACCESS_TYPE_GRANTED }
    }

    const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
    const K_IOHID_ACCESS_TYPE_GRANTED: u32 = 0;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOHIDCheckAccess(request: u32) -> u32;
        fn IOHIDRequestAccess(request: u32) -> bool;
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXIsProcessTrusted() -> bool;
    }
}
