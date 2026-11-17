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
//! | X11 | Active-window class + title via `_NET_ACTIVE_WINDOW`, `WM_CLASS`, `_NET_WM_NAME`. Verified live under Xvfb+xfwm4 (see the `x11_forbidden_live` ignored test). |
//! | macOS | Not implemented. There is no equivalent of a window class; the AX `kAXTitleAttribute` / bundle-id check is specified in `docs/body-aci.md` but unimplemented. Bodies must not claim support. |
//! | Windows | Not implemented in this tree: the `WindowsBody` reads the foreground window class via `GetForegroundWindow`/`GetClassName` and applies this same list — but no Windows run has verified it (see `windows-verify/`). |
//! | Android | Not implemented: an AccessibilityService sees the package of the foreground window; the equivalent list is specified in `docs/android-body.md`, unimplemented on-device. |
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

/// Refuse the action when the active window is forbidden, with a
/// typed error naming what matched (class only — titles can
/// contain secrets and are never echoed).
pub fn check_forbidden_window(class: Option<&str>, title: Option<&str>) -> Result<(), BodyError> {
    if is_forbidden_window(class, title) {
        return Err(BodyError::ForbiddenWindow {
            reason: match class {
                Some(class) => format!("active window class {class:?} is on the forbidden list"),
                None => "active window title matches the forbidden list".to_string(),
            },
        });
    }
    Ok(())
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
}
