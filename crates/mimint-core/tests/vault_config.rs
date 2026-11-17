//! Vault + config + registry integration: key files, key env vars,
//! config file loading, and the cost-cap lock end to end. No real
//! network is used; providers are only constructed, never called
//! (except against loopback URLs that are never contacted).

use mimint_core::{
    build_chain, build_provider, default_config_path, load_config_file, BackendKind, BackendSpec,
    CostCap, EgressPolicy, EnvVault, FileConfig, FileVault,
};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn temp_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mimint-vc-{}-{nanos}-{tag}.{ext}",
        std::process::id()
    ))
}

fn unique_env_name(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("MIMINT_TEST_{}_{}_{}", std::process::id(), nanos, tag)
}

fn remote_spec(name: &str) -> BackendSpec {
    BackendSpec {
        name: name.to_string(),
        kind: BackendKind::OpenAi,
        model: "m".to_string(),
        base_url: Some("https://example.com/v1".to_string()),
        key_env: None,
        key_file: None,
        key_keychain: None,
        timeout: Duration::from_secs(30),
        max_tokens: None,
        allow_plaintext_auth: false,
    }
}

fn paid_cap() -> CostCap {
    CostCap::new(Some(5.0)).unwrap()
}

#[test]
fn key_env_resolves_and_missing_key_fails_clearly() {
    let var = unique_env_name("keyenv");
    let vault = EnvVault;
    let mut spec = remote_spec("cloud");
    spec.key_env = Some(var.clone());
    // Absent: provider construction must fail naming the key.
    let err = match build_provider(&spec, &vault, EgressPolicy::privacy_off(), paid_cap()) {
        Ok(_) => panic!("missing key must fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("API key"), "{err}");
    // Present: construction succeeds.
    std::env::set_var(&var, "test-key-from-env");
    let provider = build_provider(&spec, &vault, EgressPolicy::privacy_off(), paid_cap())
        .expect("key from env");
    assert!(provider.is_paid());
    std::env::remove_var(&var);
}

#[cfg(unix)]
#[test]
fn key_file_resolves_and_too_open_file_is_refused() {
    let vault = EnvVault;
    let path = temp_path("key", "key");
    let created = FileVault::create(&path, "test-key-from-file\n").expect("create key file");
    assert_eq!(created.path(), path.as_path());
    let mut spec = remote_spec("cloud");
    spec.key_file = Some(path.clone());
    let provider = build_provider(&spec, &vault, EgressPolicy::privacy_off(), paid_cap())
        .expect("key from file");
    assert!(provider.is_paid());

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = match build_provider(&spec, &vault, EgressPolicy::privacy_off(), paid_cap()) {
        Ok(_) => panic!("too-open key file must fail"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("too open"), "{err}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn claude_spec_builds_against_loopback_override() {
    let vault = EnvVault;
    let spec = BackendSpec {
        name: "claude-local".to_string(),
        kind: BackendKind::Claude,
        model: "stub".to_string(),
        base_url: Some("http://127.0.0.1:9".to_string()),
        key_env: None,
        key_file: None,
        key_keychain: None,
        timeout: Duration::from_secs(5),
        max_tokens: Some(77),
        allow_plaintext_auth: false,
    };
    let provider = build_provider(&spec, &vault, EgressPolicy::privacy_on(), CostCap::none())
        .expect("claude loopback build");
    assert_eq!(provider.endpoint_url(), "http://127.0.0.1:9/v1/messages");
    assert_eq!(provider.name(), "claude-local");
    assert!(!provider.is_paid());
}

#[test]
fn config_file_loading_required_and_optional() {
    let missing = temp_path("missing", "toml");
    // Optional: missing file means empty config.
    let config = load_config_file(&missing, false).expect("optional missing");
    assert!(config.backend.is_empty());
    // Required: missing file is an error.
    assert!(load_config_file(&missing, true).is_err());

    let path = temp_path("valid", "toml");
    std::fs::write(
        &path,
        "default_backend = \"local\"\n[[backend]]\nname = \"local\"\nkind = \"ollama\"\nmodel = \"m\"\n",
    )
    .unwrap();
    let resolved = load_config_file(&path, true).unwrap().validate().unwrap();
    assert_eq!(resolved.chain, vec!["local".to_string()]);
    assert!(resolved.privacy_enabled);

    let bad = temp_path("bad", "toml");
    std::fs::write(&bad, "this is not = valid [toml").unwrap();
    let err = load_config_file(&bad, true).expect_err("bad TOML must fail");
    assert!(err.to_string().contains("invalid config TOML"), "{err}");
    assert!(
        err.to_string().contains(&bad.display().to_string()),
        "{err}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn cost_cap_from_config_locks_paid_chain_but_allows_local() {
    let vault = EnvVault;
    let text = r#"
[cost]
cap_usd = 0.0
[[backend]]
name = "local"
kind = "ollama"
model = "m"
[[backend]]
name = "cloud"
kind = "openai"
model = "m"
base_url = "https://example.com/v1"
[fallback]
chain = ["local", "cloud"]
"#;
    let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
    let err = match build_chain(
        &resolved.backends,
        &resolved.chain,
        &vault,
        EgressPolicy::privacy_off(),
        resolved.cost_cap,
    ) {
        Ok(_) => panic!("cap 0 must lock the paid member"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("cost cap is 0"), "{err}");

    let local_only = build_chain(
        &resolved.backends,
        &["local".to_string()],
        &vault,
        EgressPolicy::privacy_on(),
        resolved.cost_cap,
    )
    .expect("local-only chain under cap 0");
    assert_eq!(local_only.len(), 1);
}

#[test]
fn absent_cost_section_locks_paid_chain_but_allows_local() {
    let vault = EnvVault;
    // No [cost] section at all: paid backends are locked (opt-in).
    let text = r#"
[[backend]]
name = "local"
kind = "ollama"
model = "m"
[[backend]]
name = "cloud"
kind = "openai"
model = "m"
base_url = "https://example.com/v1"
[fallback]
chain = ["local", "cloud"]
"#;
    let resolved = FileConfig::parse(text).unwrap().validate().unwrap();
    assert_eq!(resolved.cost_cap.cap_usd, None);
    let err = match build_chain(
        &resolved.backends,
        &resolved.chain,
        &vault,
        EgressPolicy::privacy_off(),
        resolved.cost_cap,
    ) {
        Ok(_) => panic!("absent cap must lock the paid member"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("explicit opt-in"), "{err}");

    let local_only = build_chain(
        &resolved.backends,
        &["local".to_string()],
        &vault,
        EgressPolicy::privacy_on(),
        resolved.cost_cap,
    )
    .expect("local-only chain under absent cap");
    assert_eq!(local_only.len(), 1);
}

#[test]
fn default_config_path_honors_xdg_config_home() {
    let dir = temp_path("xdg", "d");
    std::fs::create_dir_all(&dir).unwrap();
    let saved_xdg = std::env::var_os("XDG_CONFIG_HOME");
    std::env::set_var("XDG_CONFIG_HOME", &dir);
    let path = default_config_path().expect("config path");
    match saved_xdg {
        Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
        None => std::env::remove_var("XDG_CONFIG_HOME"),
    }
    assert_eq!(path, dir.join("mimint").join("config.toml"));
    let _ = std::fs::remove_dir_all(&dir);
}
