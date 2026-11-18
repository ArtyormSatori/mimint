//! Broad forbidden zones (P4.6.1).
//!
//! The self-exclusion zone (Phase 3, D-FIX3-2) stops the body from
//! clicking the agent's own window. This module stops it from
//! sending input to *other people's secrets*: password managers,
//! authentication dialogs, banking apps, OS permission prompts and
//! payment flows. The policy is default-deny for those categories:
//! when the active window looks like one of them, input is refused
//! with [`BodyError::ForbiddenWindow`].
//!
//! The check is heuristic and best-effort by necessity: window
//! classes and titles are free-form strings, and a hostile or
//! misnamed window can evade any name list. It is a second gate
//! behind human approval and the self-exclusion zone, never the
//! only gate. Three properties are load-bearing:
//!
//! * **Undeterminable means allow.** When the active window cannot
//!   be determined (no window manager, property missing, property
//!   read error), the check passes and the action proceeds. A
//!   fail-closed rule here would brick the body on every bare X
//!   server, and the primary gates (approval, exclusion) still
//!   apply. The limitation is stated in every body that uses it.
//! * **Matching is case-insensitive substring on the window
//!   class**, plus a small title list. Substring matching trades
//!   false positives (a window class containing "vault" that is
//!   not a vault) for coverage; a false positive only refuses an
//!   action, which the user can retry after switching windows.
//! * **Only input is gated.** Screenshots are not refused by this
//!   policy; screenshot privacy is governed by Privacy Mode and
//!   the FrameStore purge rule (see `mimint-control`).
//!
//! ## Platform status
//!
//! | Platform | Enforcement |
//! |----------|-------------|
//! | X11 | Active-window class + title + executable basename (via `_NET_WM_PID` → `/proc`), AND the window under the target point (same signals), checked for every coordinate action. Verified live under Xvfb+xfwm4 (see the `x11_forbidden_live` ignored tests). |
//! | macOS | Not implemented. There is no equivalent of a window class; the AX `kAXTitleAttribute` / bundle-id check is specified in `docs/body-aci.md` but unimplemented, and there is no window-at-point check either. Bodies must not claim support. |
//! | Windows | Code only, NEVER RUN: the `WindowsBody` reads the foreground window class via `GetForegroundWindow`/`GetClassName` and the point window via `WindowFromPoint`, and applies this same list — but no Windows run has verified it (see `windows-verify/`). |
//! | Android | Not implemented: an AccessibilityService sees the package of the foreground window; the equivalent list is specified in `docs/android-body.md`, unimplemented on-device, and there is no window-at-point API. |
//! | Wayland | Not implemented: portals deliberately do not expose window identity to clients. No enforcement is possible through the portal APIs; documented in `docs/wayland-body.md`. |

use crate::error::BodyError;

/// Case-insensitive substrings of a window class that mark the
/// window forbidden. Password managers, authenticators, secrets
/// stores, OS credential prompts, and payment/wallet apps.
const FORBIDDEN_CLASS_SUBSTRINGS: &[&str] = &[
    "keepass",
    "1password",
    "bitwarden",
    "lastpass",
    "dashlane",
    "enpass",
    "nordpass",
    "protonpass",
    "authenticator",
    "authy",
    "yubikey",
    "yubico",
    "seahorse",
    "kwallet",
    "gcr-prompter",
    "polkit",
    "pinentry",
    "askpass",
    "vault",
    "wallet",
    "secret",
    "password",
    "keychain",
    "gnome-keyring",
    "bank",
    "payment",
    "checkout",
];

/// Case-insensitive substrings of a window title that mark the
/// window forbidden. Kept deliberately short: titles are noisier
/// than classes, and a false positive here refuses input to an
/// innocent window.
const FORBIDDEN_TITLE_SUBSTRINGS: &[&str] = &[
    "password",
    "authentication required",
    "authenticate",
    "unlock",
    "enter your pin",
    "enter pin",
    "payment",
];

/// Decide whether the active window is forbidden, from its class
/// and title. `None` for either input means "could not be
/// determined" and does not by itself forbid.
pub fn is_forbidden_window(class: Option<&str>, title: Option<&str>) -> bool {
    if let Some(class) = class {
        let lower = class.to_ascii_lowercase();
        if FORBIDDEN_CLASS_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
        {
            return true;
        }
    }
    if let Some(title) = title {
        let lower = title.to_ascii_lowercase();
        if FORBIDDEN_TITLE_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
        {
            return true;
        }
    }
    false
}

/// All identity signals available for one window, for the
/// multi-signal forbidden check (F4-03). A signal that could
/// not be determined is `None` and does not forbid on its own.
/// `exe_basename` must be a BASENAME (no path, no command
/// line): callers resolve it from the window's PID and strip
/// the directory before passing it in. A spoofed class
/// (free-form, attacker-controlled) cannot hide a denylisted
/// executable name, and a renamed executable cannot hide a
/// denylisted class — any one matching signal forbids.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowSignals<'a> {
    pub class: Option<&'a str>,
    pub title: Option<&'a str>,
    pub exe_basename: Option<&'a str>,
}

impl<'a> WindowSignals<'a> {
    pub fn new(class: Option<&'a str>, title: Option<&'a str>) -> Self {
        Self {
            class,
            title,
            exe_basename: None,
        }
    }
}

/// Decide whether a window is forbidden from ANY of its
/// signals: class, title, or executable basename (matched
/// against the class list — executable names look like class
/// names, e.g. `keepassxc`).
pub fn is_forbidden_signals(signals: &WindowSignals<'_>) -> bool {
    is_forbidden_window(signals.class, signals.title)
        || signals.exe_basename.is_some_and(|exe| {
            let lower = exe.to_ascii_lowercase();
            FORBIDDEN_CLASS_SUBSTRINGS
                .iter()
                .any(|needle| lower.contains(needle))
        })
}

/// Refuse the action when any signal of the checked window
/// matches. `role` names which window was checked (`"active"`
/// or `"target"`) for the error message. Only the class is
/// ever echoed: titles can contain secrets, and executable
/// paths can contain usernames — a title/exe match is
/// reported by signal kind only, without its value.
pub fn check_forbidden_signals(signals: &WindowSignals<'_>, role: &str) -> Result<(), BodyError> {
    if let Some(class) = signals.class {
        let lower = class.to_ascii_lowercase();
        if FORBIDDEN_CLASS_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
        {
            return Err(BodyError::ForbiddenWindow {
                reason: format!("{role} window class {class:?} is on the forbidden list"),
            });
        }
    }
    if is_forbidden_window(None, signals.title) {
        return Err(BodyError::ForbiddenWindow {
            reason: format!("{role} window title matches the forbidden list"),
        });
    }
    if let Some(exe) = signals.exe_basename {
        let lower = exe.to_ascii_lowercase();
        if FORBIDDEN_CLASS_SUBSTRINGS
            .iter()
            .any(|needle| lower.contains(needle))
        {
            return Err(BodyError::ForbiddenWindow {
                reason: format!("{role} window executable name matches the forbidden list"),
            });
        }
    }
    Ok(())
}

/// Refuse the action when the active window is forbidden, with a
/// typed error naming what matched (class only — titles can
/// contain secrets and are never echoed).
pub fn check_forbidden_window(class: Option<&str>, title: Option<&str>) -> Result<(), BodyError> {
    check_forbidden_signals(&WindowSignals::new(class, title), "active")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_managers_are_forbidden() {
        for class in [
            "KeePassXC",
            "keepassxc",
            "1Password",
            "Bitwarden",
            "LastPass",
            "org.gnome.Secrets",
            "org.kde.kwalletmanager",
            "com.apple.keychain",
        ] {
            assert!(
                is_forbidden_window(Some(class), None),
                "{class} must be forbidden"
            );
        }
    }

    #[test]
    fn credential_prompts_are_forbidden() {
        assert!(is_forbidden_window(Some("gcr-prompter"), None));
        assert!(is_forbidden_window(
            Some("polkit-gnome-authentication-agent-1"),
            None
        ));
        assert!(is_forbidden_window(Some("pinentry-gtk-2"), None));
        assert!(is_forbidden_window(None, Some("Authentication Required")));
        assert!(is_forbidden_window(None, Some("Unlock your keyring")));
    }

    #[test]
    fn ordinary_windows_are_allowed() {
        for class in ["firefox", "code", "org.gnome.Terminal", "testwin", "xfwm4"] {
            assert!(
                !is_forbidden_window(Some(class), Some("Untitled document")),
                "{class} must be allowed"
            );
        }
        assert!(!is_forbidden_window(None, None), "undeterminable allows");
        assert!(!is_forbidden_window(Some(""), Some("")));
    }

    #[test]
    fn error_names_the_class_not_the_title() {
        let err = check_forbidden_window(Some("KeePassXC"), None).unwrap_err();
        assert!(err.message().contains("KeePassXC"));
        let err = check_forbidden_window(None, Some("enter your pin 1234")).unwrap_err();
        assert!(
            !err.message().contains("1234"),
            "titles are never echoed: {}",
            err.message()
        );
    }

    #[test]
    fn spoofed_class_cannot_hide_a_denylisted_executable() {
        // Innocent class, denylisted exe: forbidden by the exe
        // signal alone, and the exe name is never echoed.
        let signals = WindowSignals {
            class: Some("Firefox"),
            title: Some("Inbox"),
            exe_basename: Some("keepassxc-probe"),
        };
        assert!(is_forbidden_signals(&signals));
        let err = check_forbidden_signals(&signals, "target").unwrap_err();
        assert_eq!(err.code(), "forbidden_window");
        assert!(err.message().contains("target window"));
        assert!(
            !err.message().contains("keepassxc-probe"),
            "executable names are never echoed: {}",
            err.message()
        );
        // Renamed exe, denylisted class: forbidden by the class.
        let signals = WindowSignals {
            class: Some("KeePassXC"),
            title: None,
            exe_basename: Some("firefox"),
        };
        assert!(is_forbidden_signals(&signals));
        // All innocent: allowed.
        let signals = WindowSignals {
            class: Some("firefox"),
            title: Some("Inbox"),
            exe_basename: Some("firefox"),
        };
        assert!(!is_forbidden_signals(&signals));
        assert!(check_forbidden_signals(&signals, "target").is_ok());
        // Undeterminable everything: allowed.
        assert!(!is_forbidden_signals(&WindowSignals::default()));
    }
}
