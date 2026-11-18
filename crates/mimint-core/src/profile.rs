//! Named profiles (Phase 5, P5.5; decision D13b, single-machine).
//!
//! A profile binds one persona/SOUL file, one backend chain,
//! one voice label, a selected skill set, proactive/budget
//! settings, a Privacy Mode switch, a cost cap, and an avatar
//! label. Switching profiles rebuilds the turn chain and guards
//! from the profile's values and tags the audit trail with the
//! profile name.
//!
//! Profiles live in the config file as `[[profile]]` tables with
//! an optional top-level `default_profile`. TOML shape errors
//! (unknown fields, wrong types) fail hard at parse time, like
//! the rest of the config. SEMANTIC profile errors — unknown
//! backend in a chain, negative cap, duplicate name, escaping
//! SOUL file name — fail CLOSED: the offending profile is
//! dropped with a warning, and the set falls back to the
//! remaining profiles (or the implicit "Mặc định" profile when
//! none survive). A bad profile never takes the whole app down,
//! and never silently becomes a different profile: every drop
//! produces a warning the caller must surface.
//!
//! Stored-but-not-executed: `voice`, `proactive`, `budget_usd`,
//! and `avatar` are stored, displayed, and audited, but their
//! execution engines do not exist yet (no TTS, no proactive
//! scheduler, and the face renderer is the 9-state machine).
//! The profile switch changes what the turn USES — SOUL,
//! chain, skills, privacy, cost cap — and labels the rest.
//! Cross-body profile sync is Phase 7 and out of scope.

use crate::registry::{BackendSpec, CostCap};
use crate::soul::{is_valid_soul_file_name, DEFAULT_SOUL_FILE};
use serde::Deserialize;

/// Name of the implicit profile built from the top-level config
/// when no `[[profile]]` tables exist (or none survive
/// validation).
pub const DEFAULT_PROFILE_NAME: &str = "Mặc định";

/// One `[[profile]]` table, as parsed from TOML. Only `name` is
/// required; every other field inherits from the top-level
/// config or a default when absent.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileEntry {
    /// Profile name, shown in the UI and used by `--profile`.
    pub name: String,
    /// SOUL file name inside the data directory. Default: `SOUL.md`.
    pub soul: Option<String>,
    /// Ordered backend names. Default: the top-level chain.
    pub chain: Option<Vec<String>>,
    /// Voice label (stored and displayed; no TTS engine yet).
    pub voice: Option<String>,
    /// Selected skill names. `None` means all promoted skills;
    /// an empty list means no skills.
    pub skills: Option<Vec<String>>,
    /// Proactive mode switch (stored and displayed; no
    /// scheduler yet). Default: false.
    pub proactive: Option<bool>,
    /// Proactive budget in USD (stored and displayed).
    pub budget_usd: Option<f64>,
    /// Privacy Mode switch. Default: the top-level setting.
    pub privacy: Option<bool>,
    /// Cost cap in USD. Default: the top-level cap.
    pub cost_cap_usd: Option<f64>,
    /// Avatar look label (stored and displayed).
    pub avatar: Option<String>,
}

/// A validated profile, ready to run turns.
#[derive(Debug, Clone, PartialEq)]
pub struct Profile {
    pub name: String,
    pub soul_file: String,
    pub chain: Vec<String>,
    pub voice: Option<String>,
    /// `None` means all promoted skills.
    pub skills: Option<Vec<String>>,
    pub proactive: bool,
    pub budget_usd: Option<f64>,
    pub privacy: bool,
    pub cost_cap: CostCap,
    pub avatar: Option<String>,
}

/// A validated profile set: the profiles, which one is the
/// default, and the warnings produced while resolving them.
/// The set always has at least one profile.
#[derive(Debug, Clone)]
pub struct ProfileSet {
    pub profiles: Vec<Profile>,
    /// Index of the default profile within `profiles`.
    default_index: usize,
    /// Warnings from resolution (dropped profiles, unknown
    /// default, ...). Callers must surface these.
    pub warnings: Vec<String>,
    /// True when the config file defined at least one
    /// `[[profile]]` table (even if all were dropped).
    pub explicit: bool,
    /// True when `[[profile]]` tables existed but NONE survived
    /// validation, so the app runs on a FORCED-SAFE fallback
    /// profile (Privacy ON, paid backends locked, no proactive
    /// budget) instead of inheriting possibly-edited top-level
    /// values (F5-03). False when no `[[profile]]` tables exist
    /// at all: the implicit default then inherits the top-level
    /// values as before. Callers must surface this persistently.
    pub fallback_active: bool,
}

impl ProfileSet {
    /// Resolve profile entries against the top-level config
    /// values (defined backends, top-level chain, privacy, cost
    /// cap). `default_name` is the top-level `default_profile`
    /// value, when present. Never fails: semantic errors drop
    /// profiles with warnings, and an empty result falls back to
    /// the implicit default profile.
    pub fn resolve(
        entries: &[ProfileEntry],
        default_name: Option<&str>,
        backends: &[BackendSpec],
        top_chain: &[String],
        privacy_enabled: bool,
        cost_cap: CostCap,
    ) -> Self {
        let mut warnings = Vec::new();
        let mut profiles: Vec<Profile> = Vec::new();
        let mut seen_names: Vec<String> = Vec::new();
        let defined: Vec<&str> = backends.iter().map(|spec| spec.name.as_str()).collect();
        for entry in entries {
            if entry.name.trim().is_empty() {
                warnings.push("profile with an empty name was dropped".to_string());
                continue;
            }
            if seen_names.iter().any(|seen| seen == &entry.name) {
                warnings.push(format!(
                    "duplicate profile name {:?} was dropped (first definition wins)",
                    entry.name
                ));
                continue;
            }
            seen_names.push(entry.name.clone());
            match resolve_one(entry, top_chain, privacy_enabled, cost_cap, &defined) {
                Ok(profile) => profiles.push(profile),
                Err(reason) => {
                    warnings.push(format!("profile {:?} was dropped: {reason}", entry.name))
                }
            }
        }
        let mut fallback_active = false;
        if profiles.is_empty() {
            if !entries.is_empty() {
                // F5-03: every declared profile failed validation.
                // Do NOT inherit the top-level privacy/cost values
                // (they may be the edited values that broke the
                // profiles): force the safest profile instead.
                fallback_active = true;
                warnings.push(format!(
                    "no valid profiles: running in FORCED-SAFE fallback mode (Privacy ON, paid backends locked, no proactive budget) on the implicit {DEFAULT_PROFILE_NAME:?} profile; fix the profile entries in the config file"
                ));
                profiles.push(forced_safe_default(top_chain));
            } else {
                profiles.push(implicit_default(top_chain, privacy_enabled, cost_cap));
            }
        }
        let default_index = match default_name {
            Some(name) => match profiles.iter().position(|profile| profile.name == name) {
                Some(index) => index,
                None => {
                    warnings.push(format!(
                        "default_profile {name:?} does not match any valid profile; using {:?}",
                        profiles[0].name
                    ));
                    0
                }
            },
            None => 0,
        };
        Self {
            profiles,
            default_index,
            warnings,
            explicit: !entries.is_empty(),
            fallback_active,
        }
    }

    /// The default profile.
    pub fn default_profile(&self) -> &Profile {
        &self.profiles[self.default_index]
    }

    /// Find a profile by name.
    pub fn by_name(&self, name: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.name == name)
    }

    /// Resolve a requested name, falling closed to the default
    /// profile with a warning when the name is unknown. Returns
    /// the profile and the warning, when one applies.
    pub fn resolve_or_default(&self, name: &str) -> (&Profile, Option<String>) {
        match self.by_name(name) {
            Some(profile) => (profile, None),
            None => (
                self.default_profile(),
                Some(format!(
                    "unknown profile {name:?}; falling back to {:?}",
                    self.default_profile().name
                )),
            ),
        }
    }
}

/// The forced-safe fallback profile (F5-03), used ONLY when
/// `[[profile]]` tables existed but none survived validation.
/// Privacy is forced ON, the cost cap is forced to 0.0 USD (paid
/// backends locked), proactive mode is off, and there is no
/// proactive budget — regardless of the top-level values, which
/// may themselves be the misconfiguration. The chain is the
/// top-level chain (its backends are still gated by privacy and
/// the zero cap). The broken config file is never overwritten.
fn forced_safe_default(top_chain: &[String]) -> Profile {
    Profile {
        name: DEFAULT_PROFILE_NAME.to_string(),
        soul_file: DEFAULT_SOUL_FILE.to_string(),
        chain: top_chain.to_vec(),
        voice: None,
        skills: None,
        proactive: false,
        budget_usd: None,
        privacy: true,
        cost_cap: CostCap { cap_usd: Some(0.0) },
        avatar: None,
    }
}

/// The implicit profile from top-level config values.
fn implicit_default(top_chain: &[String], privacy_enabled: bool, cost_cap: CostCap) -> Profile {
    Profile {
        name: DEFAULT_PROFILE_NAME.to_string(),
        soul_file: DEFAULT_SOUL_FILE.to_string(),
        chain: top_chain.to_vec(),
        voice: None,
        skills: None,
        proactive: false,
        budget_usd: None,
        privacy: privacy_enabled,
        cost_cap,
        avatar: None,
    }
}

/// Validate one entry into a runnable profile. `Err` carries the
/// reason the profile must be dropped.
fn resolve_one(
    entry: &ProfileEntry,
    top_chain: &[String],
    privacy_enabled: bool,
    top_cost_cap: CostCap,
    defined: &[&str],
) -> Result<Profile, String> {
    let soul_file = entry
        .soul
        .clone()
        .unwrap_or_else(|| DEFAULT_SOUL_FILE.to_string());
    if !is_valid_soul_file_name(&soul_file) {
        return Err(format!(
            "soul file name {soul_file:?} is not a bare file name inside the data directory"
        ));
    }
    let chain = match &entry.chain {
        Some(chain) => {
            if chain.is_empty() {
                return Err("chain must not be empty: name at least one backend".to_string());
            }
            for name in chain {
                if !defined.contains(&name.as_str()) {
                    return Err(format!(
                        "chain names unknown backend '{name}'; defined backends: {}",
                        defined.join(", ")
                    ));
                }
            }
            chain.clone()
        }
        None => top_chain.to_vec(),
    };
    let cost_cap = match entry.cost_cap_usd {
        Some(cap) => CostCap::new(Some(cap))
            .map_err(|err| format!("invalid cost_cap_usd: {}", err.message()))?,
        None => top_cost_cap,
    };
    if let Some(budget) = entry.budget_usd {
        if budget < 0.0 || !budget.is_finite() {
            return Err(format!(
                "invalid budget_usd {budget}: must be a finite value >= 0"
            ));
        }
    }
    if let Some(skills) = &entry.skills {
        for skill in skills {
            if skill.trim().is_empty() {
                return Err("skills list contains an empty name".to_string());
            }
        }
    }
    Ok(Profile {
        name: entry.name.clone(),
        soul_file,
        chain,
        voice: entry.voice.clone(),
        skills: entry.skills.clone(),
        proactive: entry.proactive.unwrap_or(false),
        budget_usd: entry.budget_usd,
        privacy: entry.privacy.unwrap_or(privacy_enabled),
        cost_cap,
        avatar: entry.avatar.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileConfig, ResolvedConfig};

    fn resolved(toml: &str) -> ResolvedConfig {
        FileConfig::parse(toml).unwrap().validate().unwrap()
    }

    fn resolve_set(
        entries: &[ProfileEntry],
        default_name: Option<&str>,
        config: &ResolvedConfig,
    ) -> ProfileSet {
        ProfileSet::resolve(
            entries,
            default_name,
            &config.backends,
            &config.chain,
            config.privacy_enabled,
            config.cost_cap,
        )
    }

    const TWO_BACKENDS: &str = r#"
default_backend = "local"
[privacy]
enabled = true
[cost]
cap_usd = 0.0
[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"
[[backend]]
name = "cloud"
kind = "claude"
model = "claude-sonnet-4-5"
key_env = "ANTHROPIC_API_KEY"
[fallback]
chain = ["local", "cloud"]
"#;

    fn parse_probe(toml: &str) -> (Vec<ProfileEntry>, Option<String>) {
        #[derive(Deserialize)]
        struct Probe {
            #[serde(default)]
            profile: Vec<ProfileEntry>,
            default_profile: Option<String>,
        }
        let probe: Probe = toml::from_str(toml).unwrap();
        (probe.profile, probe.default_profile)
    }

    #[test]
    fn all_profiles_dropped_forces_safe_fallback() {
        // Top-level values are permissive here (privacy off, no
        // cap): the fallback must NOT inherit them (F5-03).
        let config = resolved(
            r#"
default_backend = "local"
[privacy]
enabled = false
[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"
[fallback]
chain = ["local"]
"#,
        );
        let (entries, _) = parse_probe(
            r#"
[[profile]]
name = "Xấu"
chain = ["ghost"]
"#,
        );
        let set = resolve_set(&entries, None, &config);
        assert!(set.fallback_active);
        assert!(set.explicit);
        let profile = set.default_profile();
        assert_eq!(profile.name, DEFAULT_PROFILE_NAME);
        assert!(profile.privacy, "fallback forces Privacy ON");
        assert_eq!(
            profile.cost_cap.cap_usd,
            Some(0.0),
            "fallback locks paid backends"
        );
        assert!(!profile.proactive);
        assert_eq!(profile.budget_usd, None);
        assert!(set
            .warnings
            .iter()
            .any(|w| w.contains("FORCED-SAFE fallback")));
        assert!(set
            .warnings
            .iter()
            .any(|w| w.contains("profile \"Xấu\" was dropped")));
    }

    #[test]
    fn no_profile_tables_is_not_fallback_mode() {
        let config = resolved(
            r#"
default_backend = "local"
[privacy]
enabled = false
[cost]
cap_usd = 3.0
[[backend]]
name = "local"
kind = "ollama"
model = "qwen2.5:3b"
[fallback]
chain = ["local"]
"#,
        );
        let set = resolve_set(&[], None, &config);
        assert!(!set.fallback_active);
        assert!(!set.explicit);
        // Implicit default keeps inheriting the top-level values.
        assert!(!set.default_profile().privacy);
        assert_eq!(set.default_profile().cost_cap.cap_usd, Some(3.0));
    }

    #[test]
    fn one_surviving_profile_is_not_fallback_mode() {
        let config = resolved(TWO_BACKENDS);
        let (entries, _) = parse_probe(
            r#"
[[profile]]
name = "Xấu"
chain = ["ghost"]
[[profile]]
name = "Tốt"
"#,
        );
        let set = resolve_set(&entries, None, &config);
        assert!(!set.fallback_active);
        assert_eq!(set.profiles.len(), 1);
        assert_eq!(set.profiles[0].name, "Tốt");
    }

    #[test]
    fn no_profiles_yields_implicit_default() {
        let config = resolved(TWO_BACKENDS);
        let set = resolve_set(&[], None, &config);
        assert_eq!(set.profiles.len(), 1);
        assert_eq!(set.default_profile().name, DEFAULT_PROFILE_NAME);
        assert_eq!(
            set.default_profile().chain,
            vec!["local".to_string(), "cloud".to_string()]
        );
        assert!(set.default_profile().privacy);
        assert!(set.warnings.is_empty());
        assert_eq!(set.default_profile().soul_file, "SOUL.md");
    }

    #[test]
    fn profiles_inherit_and_override_top_level() {
        let config = resolved(TWO_BACKENDS);
        let (entries, _) = parse_probe(
            r#"
[[profile]]
name = "Làm việc"
soul = "SOUL-work.md"
chain = ["cloud"]
privacy = false
cost_cap_usd = 5.0
voice = "voice-e"
skills = ["ghi-chu"]
proactive = true
budget_usd = 2.0
avatar = "look-3"
"#,
        );
        let set = resolve_set(&entries, None, &config);
        assert!(set.warnings.is_empty());
        let profile = set.by_name("Làm việc").unwrap();
        assert_eq!(profile.soul_file, "SOUL-work.md");
        assert_eq!(profile.chain, vec!["cloud".to_string()]);
        assert!(!profile.privacy);
        assert_eq!(profile.cost_cap.cap_usd, Some(5.0));
        assert_eq!(profile.voice.as_deref(), Some("voice-e"));
        assert_eq!(profile.skills, Some(vec!["ghi-chu".to_string()]));
        assert!(profile.proactive);
        assert_eq!(profile.budget_usd, Some(2.0));
        assert_eq!(profile.avatar.as_deref(), Some("look-3"));
    }

    #[test]
    fn semantic_errors_drop_profiles_with_warnings_and_fall_closed() {
        let config = resolved(TWO_BACKENDS);
        let (entries, _) = parse_probe(
            r#"
[[profile]]
name = "Xấu"
chain = ["ghost"]
[[profile]]
name = "Xấu2"
cost_cap_usd = -1.0
[[profile]]
name = "Xấu3"
soul = "../escape.md"
[[profile]]
name = "Tốt"
"#,
        );
        let set = resolve_set(&entries, None, &config);
        assert_eq!(set.profiles.len(), 1);
        assert_eq!(set.profiles[0].name, "Tốt");
        assert_eq!(set.warnings.len(), 3);
        // All bad: fall closed to the implicit default.
        let (entries, _) = parse_probe(
            r#"
[[profile]]
name = "Xấu"
chain = ["ghost"]
"#,
        );
        let set = resolve_set(&entries, None, &config);
        assert_eq!(set.profiles.len(), 1);
        assert_eq!(set.profiles[0].name, DEFAULT_PROFILE_NAME);
        assert!(set.fallback_active);
        assert!(set
            .warnings
            .iter()
            .any(|w| w.contains("FORCED-SAFE fallback")));
    }

    #[test]
    fn duplicate_names_and_unknown_default_warn() {
        let config = resolved(TWO_BACKENDS);
        let (entries, default) = parse_probe(
            r#"
default_profile = "Không có"
[[profile]]
name = "A"
[[profile]]
name = "A"
chain = ["cloud"]
"#,
        );
        let set = resolve_set(&entries, default.as_deref(), &config);
        assert_eq!(set.profiles.len(), 1);
        // First definition wins: the chain is the top-level one.
        assert_eq!(set.profiles[0].chain.len(), 2);
        assert_eq!(set.warnings.len(), 2);
        // Unknown requested name falls closed with a warning.
        let (profile, warning) = set.resolve_or_default("Không có");
        assert_eq!(profile.name, "A");
        assert!(warning.unwrap().contains("unknown profile"));
        let (profile, warning) = set.resolve_or_default("A");
        assert_eq!(profile.name, "A");
        assert!(warning.is_none());
    }

    #[test]
    fn profile_toml_rejects_unknown_fields() {
        // The struct itself rejects unknown fields (typos fail loudly).
        let single = "name = \"A\"\ntypo_field = 1\n";
        assert!(toml::from_str::<ProfileEntry>(single).is_err());
        let ok = "name = \"A\"\nprivacy = false\n";
        assert!(toml::from_str::<ProfileEntry>(ok).is_ok());
    }
}
