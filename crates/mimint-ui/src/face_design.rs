//! Face design data (Phase 8, F8-12): the Mint face design lives in
//! a data file (`assets/mint-face.json`, a character asset under the
//! Mimint Character Licence), NOT in code. This module is the generic
//! Apache-2.0 loader: it reads per-state [`FaceParams`] from JSON and
//! falls back to a neutral, generic face when the file is missing or
//! invalid, so a fork can delete the design file and still run —
//! removing the character asset is a one-file operation, and editing
//! the code no longer edits the design.
//!
//! The design file format (version 1):
//! ```json
//! {
//!   "version": 1,
//!   "name": "Mint",
//!   "license": "...",
//!   "states": {
//!     "neutral": {"eyes": "oval", "mouth": "smile", "blush": true,
//!                 "tilt_degrees": 0.0, "bounce": false,
//!                 "star_burst": false},
//!     "...": { "..." }
//!   }
//! }
//! ```
//! All 9 states must be present; unknown states, unknown fields, and
//! unknown eye/mouth names are errors (fail-closed on typos), as is
//! a tilt outside -45..=45 degrees. The top-level `license` / `name`
//! / `version` fields are informational and ignored by the loader,
//! except that `version`, when present, must be the number 1.

use crate::face::{EyeShape, FaceParams, FaceState, MouthShape};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// File name of the face design, searched under `assets/`.
pub const FACE_DESIGN_FILE: &str = "mint-face.json";

/// Environment variable that overrides the design file path outright.
/// Set it to point at a custom design, or at a nonexistent path to
/// force the neutral face (useful for testing and for forks).
pub const FACE_DESIGN_ENV: &str = "MIMINT_FACE_DESIGN";

/// The per-state face parameters for one design: 9 states, indexed
/// in design order ([`FaceState::index`]).
#[derive(Debug, Clone, PartialEq)]
pub struct FaceDesign {
    states: [FaceParams; 9],
}

/// The neutral, generic face (Apache 2.0): plain oval eyes, a flat
/// mouth, no blush, no tilt, no bounce, no star burst. It is not
/// Mint's face — Mint's Neutral smiles and blushes — so a fork that
/// deletes the design file gets a visibly different, character-free
/// face instead of Mint's.
pub fn neutral_params() -> FaceParams {
    FaceParams {
        eyes: EyeShape::Oval,
        mouth: MouthShape::Flat,
        blush: false,
        tilt_degrees: 0.0,
        bounce: false,
        star_burst: false,
    }
}

impl FaceDesign {
    /// The neutral design: every state maps to [`neutral_params`].
    pub fn neutral() -> Self {
        Self {
            states: [neutral_params(); 9],
        }
    }

    /// Parameters for one state under this design.
    pub fn params_for(&self, state: FaceState) -> FaceParams {
        self.states[state.index()]
    }

    /// Parse a design from JSON text. Strict: all 9 states required,
    /// no unknown states or fields, no unknown eye/mouth names, tilt
    /// within -45..=45 degrees.
    pub fn from_json_str(text: &str) -> Result<Self, String> {
        let value: serde_json::Value =
            serde_json::from_str(text).map_err(|e| format!("invalid JSON: {e}"))?;
        let root = value
            .as_object()
            .ok_or_else(|| "design root must be a JSON object".to_string())?;
        if let Some(version) = root.get("version") {
            let n = version
                .as_u64()
                .ok_or_else(|| "design \"version\" must be a number".to_string())?;
            if n != 1 {
                return Err(format!("unsupported design version: {n} (expected 1)"));
            }
        }
        let states = root
            .get("states")
            .ok_or_else(|| "design is missing the \"states\" object".to_string())?
            .as_object()
            .ok_or_else(|| "design \"states\" must be a JSON object".to_string())?;
        for key in states.keys() {
            if FaceState::parse_wire(key).is_none() {
                return Err(format!("design has unknown state: {key:?}"));
            }
        }
        let mut out = [neutral_params(); 9];
        for state in FaceState::ALL {
            let entry = states
                .get(state.wire_name())
                .ok_or_else(|| format!("design is missing state {:?}", state.wire_name()))?;
            out[state.index()] = parse_state_params(state.wire_name(), entry)?;
        }
        Ok(Self { states: out })
    }

    /// Load a design from a file path.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = fs::read_to_string(path)
            .map_err(|e| format!("cannot read design file {}: {e}", path.display()))?;
        Self::from_json_str(&text).map_err(|e| format!("design file {}: {e}", path.display()))
    }

    /// Load a design, falling back to the neutral design (with a
    /// stderr warning) when the file cannot be read or parsed. A
    /// missing or broken design file never crashes the app and never
    /// shows Mint's face by accident.
    pub fn load_or_neutral(path: &Path) -> Self {
        match Self::load(path) {
            Ok(design) => design,
            Err(err) => {
                eprintln!("warning: {err}; using the neutral face design");
                Self::neutral()
            }
        }
    }

    /// Load the design for the running app, in this order:
    /// 1. `$MIMINT_FACE_DESIGN` when set and non-empty (explicit
    ///    override; a missing/unreadable file there warns and falls
    ///    back to neutral).
    /// 2. `assets/mint-face.json` next to the executable (installed
    ///    layout).
    /// 3. `assets/mint-face.json` under the current working
    ///    directory (running from a checkout).
    ///
    /// When no candidate file exists at all, the neutral design is
    /// used silently — that is the normal case for a fork that
    /// removed the character asset.
    pub fn load_default() -> Self {
        match choose_design_path(
            env::var(FACE_DESIGN_ENV).ok().filter(|v| !v.is_empty()),
            &exe_assets_path(),
            &PathBuf::from("assets").join(FACE_DESIGN_FILE),
        ) {
            Some(path) => Self::load_or_neutral(&path),
            None => Self::neutral(),
        }
    }
}

/// Pick which design file to load: the env override wins outright;
/// otherwise the first candidate path that exists wins; `None` when
/// no candidate exists. Pure (no I/O beyond `exists`) so it is
/// unit-testable without touching process env.
fn choose_design_path(
    env_override: Option<String>,
    exe_candidate: &Path,
    cwd_candidate: &Path,
) -> Option<PathBuf> {
    if let Some(path) = env_override {
        return Some(PathBuf::from(path));
    }
    if exe_candidate.exists() {
        return Some(exe_candidate.to_path_buf());
    }
    if cwd_candidate.exists() {
        return Some(cwd_candidate.to_path_buf());
    }
    None
}

/// `assets/mint-face.json` next to the running executable.
fn exe_assets_path() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.parent()
                .map(|dir| dir.join("assets").join(FACE_DESIGN_FILE))
        })
        .unwrap_or_else(|| PathBuf::from("assets").join(FACE_DESIGN_FILE))
}

fn parse_state_params(state: &str, value: &serde_json::Value) -> Result<FaceParams, String> {
    let obj = value
        .as_object()
        .ok_or_else(|| format!("state {state:?} must be a JSON object"))?;
    for key in obj.keys() {
        match key.as_str() {
            "eyes" | "mouth" | "blush" | "tilt_degrees" | "bounce" | "star_burst" => {}
            other => {
                return Err(format!("state {state:?} has unknown field: {other:?}"));
            }
        }
    }
    let eyes = parse_eye_shape(state, obj.get("eyes"))?;
    let mouth = parse_mouth_shape(state, obj.get("mouth"))?;
    let blush = parse_bool(state, "blush", obj.get("blush"))?;
    let tilt_degrees = parse_tilt(state, obj.get("tilt_degrees"))?;
    let bounce = parse_bool(state, "bounce", obj.get("bounce"))?;
    let star_burst = parse_bool(state, "star_burst", obj.get("star_burst"))?;
    Ok(FaceParams {
        eyes,
        mouth,
        blush,
        tilt_degrees,
        bounce,
        star_burst,
    })
}

fn parse_eye_shape(state: &str, value: Option<&serde_json::Value>) -> Result<EyeShape, String> {
    let name = value
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("state {state:?} is missing string field \"eyes\""))?;
    match name {
        "oval" => Ok(EyeShape::Oval),
        "happy_arc" => Ok(EyeShape::HappyArc),
        "closed" => Ok(EyeShape::Closed),
        "wide" => Ok(EyeShape::Wide),
        "dot" => Ok(EyeShape::Dot),
        other => Err(format!("state {state:?} has unknown eye shape: {other:?}")),
    }
}

fn parse_mouth_shape(state: &str, value: Option<&serde_json::Value>) -> Result<MouthShape, String> {
    let name = value
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| format!("state {state:?} is missing string field \"mouth\""))?;
    match name {
        "smile" => Ok(MouthShape::Smile),
        "open_smile" => Ok(MouthShape::OpenSmile),
        "small_o" => Ok(MouthShape::SmallO),
        "flat" => Ok(MouthShape::Flat),
        "wavy" => Ok(MouthShape::Wavy),
        "yawn" => Ok(MouthShape::Yawn),
        other => Err(format!(
            "state {state:?} has unknown mouth shape: {other:?}"
        )),
    }
}

fn parse_bool(state: &str, field: &str, value: Option<&serde_json::Value>) -> Result<bool, String> {
    value
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| format!("state {state:?} is missing boolean field {field:?}"))
}

fn parse_tilt(state: &str, value: Option<&serde_json::Value>) -> Result<f32, String> {
    let n = value
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| format!("state {state:?} is missing numeric field \"tilt_degrees\""))?;
    if !n.is_finite() || !(-45.0..=45.0).contains(&n) {
        return Err(format!(
            "state {state:?} has tilt_degrees out of range: {n} (must be -45..=45)"
        ));
    }
    Ok(n as f32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The shipped Mint design file, resolved from the crate manifest
    /// so the test works no matter which directory cargo runs from.
    fn mint_design_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("assets")
            .join(FACE_DESIGN_FILE)
    }

    fn legacy_params(state: FaceState) -> FaceParams {
        // The exact values `params_for` hardcoded before the design
        // was extracted to a data file (F8-12): the JSON file must
        // reproduce them bit for bit.
        match state {
            FaceState::Neutral => FaceParams {
                eyes: EyeShape::Oval,
                mouth: MouthShape::Smile,
                blush: true,
                tilt_degrees: 0.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Happy => FaceParams {
                eyes: EyeShape::HappyArc,
                mouth: MouthShape::OpenSmile,
                blush: true,
                tilt_degrees: 0.0,
                bounce: true,
                star_burst: false,
            },
            FaceState::Listening => FaceParams {
                eyes: EyeShape::Oval,
                mouth: MouthShape::Smile,
                blush: true,
                tilt_degrees: 0.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Thinking => FaceParams {
                eyes: EyeShape::Dot,
                mouth: MouthShape::Flat,
                blush: false,
                tilt_degrees: 8.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Speaking => FaceParams {
                eyes: EyeShape::Oval,
                mouth: MouthShape::OpenSmile,
                blush: true,
                tilt_degrees: 0.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Surprised => FaceParams {
                eyes: EyeShape::Wide,
                mouth: MouthShape::SmallO,
                blush: false,
                tilt_degrees: 0.0,
                bounce: true,
                star_burst: false,
            },
            FaceState::Confused => FaceParams {
                eyes: EyeShape::Dot,
                mouth: MouthShape::Wavy,
                blush: false,
                tilt_degrees: -8.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Sleepy => FaceParams {
                eyes: EyeShape::Closed,
                mouth: MouthShape::Yawn,
                blush: false,
                tilt_degrees: 0.0,
                bounce: false,
                star_burst: false,
            },
            FaceState::Celebrating => FaceParams {
                eyes: EyeShape::HappyArc,
                mouth: MouthShape::OpenSmile,
                blush: true,
                tilt_degrees: 0.0,
                bounce: true,
                star_burst: true,
            },
        }
    }

    fn write_temp_json(name: &str, text: &str) -> PathBuf {
        let path = env::temp_dir().join(format!("mimint-face-test-{name}.json"));
        let mut f = fs::File::create(&path).expect("create temp design file");
        f.write_all(text.as_bytes())
            .expect("write temp design file");
        path
    }

    fn minimal_json_with(state: &str, body: &str) -> String {
        // A valid design with every state neutral except `state`.
        let mut states = String::from("{");
        for (i, s) in FaceState::ALL.iter().enumerate() {
            if i > 0 {
                states.push(',');
            }
            if s.wire_name() == state {
                states.push_str(&format!("\"{state}\":{body}"));
            } else {
                states.push_str(&format!(
                    "\"{}\":{{\"eyes\":\"oval\",\"mouth\":\"flat\",\"blush\":false,\"tilt_degrees\":0.0,\"bounce\":false,\"star_burst\":false}}",
                    s.wire_name()
                ));
            }
        }
        states.push('}');
        format!("{{\"version\":1,\"states\":{states}}}")
    }

    #[test]
    fn mint_design_file_matches_legacy_params() {
        let path = mint_design_path();
        let design = FaceDesign::load(&path)
            .unwrap_or_else(|e| panic!("shipped design file {} must load: {e}", path.display()));
        for state in FaceState::ALL {
            assert_eq!(
                design.params_for(state),
                legacy_params(state),
                "state {:?} in {} must match the legacy hardcoded params",
                state,
                path.display()
            );
        }
    }

    #[test]
    fn missing_file_falls_back_to_neutral() {
        let design =
            FaceDesign::load_or_neutral(Path::new("/definitely/not/a/real/mint-face.json"));
        assert_eq!(design, FaceDesign::neutral());
        for state in FaceState::ALL {
            assert_eq!(design.params_for(state), neutral_params());
        }
    }

    #[test]
    fn invalid_json_falls_back_to_neutral() {
        let path = write_temp_json("invalid", "{not json");
        let design = FaceDesign::load_or_neutral(&path);
        assert_eq!(design, FaceDesign::neutral());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn invalid_design_is_an_error_not_a_silent_fallback() {
        // load() itself must report the problem; only the
        // load_or_neutral wrapper downgrades it to neutral.
        assert!(FaceDesign::load(Path::new("/definitely/not/a/real/mint-face.json")).is_err());
        let path = write_temp_json(
            "bad-eye",
            &minimal_json_with(
                "happy",
                r#"{"eyes":"starry","mouth":"smile","blush":true,"tilt_degrees":0.0,"bounce":false,"star_burst":false}"#,
            ),
        );
        assert!(FaceDesign::load(&path).is_err());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_state_key_is_an_error() {
        let json = minimal_json_with(
            "happy",
            r#"{"eyes":"oval","mouth":"smile","blush":true,"tilt_degrees":0.0,"bounce":false,"star_burst":false}"#,
        );
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["states"]["ecstatic"] = serde_json::json!({"eyes":"oval","mouth":"smile","blush":false,"tilt_degrees":0.0,"bounce":false,"star_burst":false});
        assert!(FaceDesign::from_json_str(&value.to_string()).is_err());
    }

    #[test]
    fn missing_state_is_an_error() {
        let json = r#"{"version":1,"states":{"neutral":{"eyes":"oval","mouth":"smile","blush":true,"tilt_degrees":0.0,"bounce":false,"star_burst":false}}}"#;
        assert!(FaceDesign::from_json_str(json).is_err());
    }

    #[test]
    fn unknown_field_is_an_error() {
        let json = minimal_json_with(
            "happy",
            r#"{"eyes":"oval","mouth":"smile","blush":true,"tilt_degrees":0.0,"bounce":false,"star_burst":false,"sparkle":true}"#,
        );
        assert!(FaceDesign::from_json_str(&json).is_err());
    }

    #[test]
    fn tilt_out_of_range_is_an_error() {
        let json = minimal_json_with(
            "thinking",
            r#"{"eyes":"dot","mouth":"flat","blush":false,"tilt_degrees":90.0,"bounce":false,"star_burst":false}"#,
        );
        assert!(FaceDesign::from_json_str(&json).is_err());
    }

    #[test]
    fn unsupported_version_is_an_error() {
        let json = r#"{"version":2,"states":{}}"#;
        assert!(FaceDesign::from_json_str(json).is_err());
    }

    #[test]
    fn custom_design_file_is_used() {
        let json = minimal_json_with(
            "happy",
            r#"{"eyes":"wide","mouth":"small_o","blush":false,"tilt_degrees":-8.0,"bounce":true,"star_burst":true}"#,
        );
        let path = write_temp_json("custom", &json);
        let design = FaceDesign::load(&path).expect("custom design must load");
        assert_eq!(
            design.params_for(FaceState::Happy),
            FaceParams {
                eyes: EyeShape::Wide,
                mouth: MouthShape::SmallO,
                blush: false,
                tilt_degrees: -8.0,
                bounce: true,
                star_burst: true,
            }
        );
        // Untouched states stay neutral in this custom file.
        assert_eq!(design.params_for(FaceState::Neutral), neutral_params());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn neutral_design_has_identical_params_for_all_states() {
        let design = FaceDesign::neutral();
        for state in FaceState::ALL {
            assert_eq!(design.params_for(state), neutral_params());
        }
    }

    #[test]
    fn neutral_face_differs_from_mint_face() {
        // The whole point of the fallback: with the design file
        // gone, no state may show Mint's face.
        let mint = FaceDesign::load(&mint_design_path()).expect("mint design loads");
        let neutral = FaceDesign::neutral();
        for state in FaceState::ALL {
            assert_ne!(
                mint.params_for(state),
                neutral.params_for(state),
                "state {state:?} must differ between Mint and neutral"
            );
        }
    }

    #[test]
    fn choose_design_path_prefers_env_then_existing_candidates() {
        let dir = env::temp_dir();
        let existing = dir.join("mimint-face-test-exists.json");
        let _ = fs::File::create(&existing);
        let missing = dir.join("mimint-face-test-missing.json");
        let _ = fs::remove_file(&missing);
        // Env override wins even when it points at a missing file
        // (load_or_neutral then warns and falls back).
        assert_eq!(
            choose_design_path(
                Some(missing.to_string_lossy().into_owned()),
                &existing,
                &existing
            ),
            Some(missing.clone())
        );
        // Without env, the first existing candidate wins.
        assert_eq!(
            choose_design_path(None, &existing, &missing),
            Some(existing.clone())
        );
        assert_eq!(
            choose_design_path(None, &missing, &existing),
            Some(existing.clone())
        );
        assert_eq!(choose_design_path(None, &missing, &missing), None);
        let _ = fs::remove_file(&existing);
    }
}
