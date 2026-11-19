//! Phase 7 §5.5: the hub's driver requests go through the
//! [`BodyActor`], never straight to the body.
//!
//! Proof is by observation, not by mocking the hub: a real
//! [`FriendsContext`] (real hub, real allow-list) drives a
//! [`MockBody`] through [`ActorDriverBackend`], and the mock
//! oracle shows the action actually landed on the body. Fault
//! injection proves a mid-action disconnect propagates as an
//! error instead of a faked success.

use mimint_body::{Fault, MockBody, MockEvent, StopFlag};
use mimint_control::ActorDriverBackend;
use mimint_core::friends::context::FriendsContext;
use mimint_core::friends::hub::{DriverBackend, HubConfig};
use mimint_core::friends::registry::Trust;
use mimint_core::friends::util::Clock;
use std::sync::Arc;

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir =
        std::env::temp_dir().join(format!("mimint-p7-hub-driver-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn open_ctx(dir: &std::path::Path, backend: Arc<ActorDriverBackend>) -> FriendsContext {
    FriendsContext::open(
        dir,
        dir.join("config.toml"),
        dir.join("notebook.db"),
        HubConfig::default(),
        backend,
        None,
        Clock::system(),
    )
    .unwrap()
}

#[test]
fn hub_driver_goes_through_actor() {
    let dir = temp_dir("happy");
    let body = MockBody::new();
    let oracle = body.oracle();
    let backend = Arc::new(ActorDriverBackend::spawn(Box::new(body), StopFlag::new()));
    assert_eq!(backend.name(), "actor");
    let ctx = open_ctx(&dir, backend);
    let hub = &ctx.hub;

    // A screenshot through the hub returns a real frame from
    // the body (dimensions + bytes), via the actor.
    let shot = hub
        .execute_driver_method(Trust::Owner, "screenshot", &serde_json::json!({}))
        .expect("owner screenshot must run through the actor");
    assert!(shot["width"].as_u64().unwrap() > 0, "got {shot}");
    assert!(shot["height"].as_u64().unwrap() > 0, "got {shot}");
    assert!(shot["bytes"].as_u64().unwrap() > 0, "got {shot}");

    // A click through the hub lands on the body: the oracle
    // — the body's own view — shows exactly that click.
    hub.execute_driver_method(
        Trust::Owner,
        "click",
        &serde_json::json!({"x": 10, "y": 20}),
    )
    .expect("owner click must run through the actor");
    assert!(
        oracle.events().contains(&MockEvent::Click { x: 10, y: 20 }),
        "oracle events: {:?}",
        oracle.events()
    );

    // A guest cannot drive input: the allow-list refuses
    // BEFORE the actor, so the body sees nothing new.
    let before = oracle.events().len();
    assert!(
        hub.execute_driver_method(Trust::Guest, "click", &serde_json::json!({"x": 1, "y": 2}))
            .is_err(),
        "guest click must be refused"
    );
    assert_eq!(oracle.events().len(), before);

    // Unknown methods are refused, not passed to the actor.
    assert!(hub
        .execute_driver_method(Trust::Owner, "explode", &serde_json::json!({}))
        .is_err());

    // Fault injection: the body disconnects mid-click. The
    // actor reports UnknownOutcome and the hub surfaces an
    // error — the backend never fakes a success. The partial
    // click IS recorded (it may have landed), which is exactly
    // why the error must propagate instead of a retry.
    let dir2 = temp_dir("fault");
    let faulty = MockBody::with_fault(Fault::DisconnectMidAction);
    let oracle2 = faulty.oracle();
    let backend2 = Arc::new(ActorDriverBackend::spawn(Box::new(faulty), StopFlag::new()));
    let ctx2 = open_ctx(&dir2, backend2);
    assert!(
        ctx2.hub
            .execute_driver_method(
                Trust::Owner,
                "click",
                &serde_json::json!({"x": 10, "y": 10})
            )
            .is_err(),
        "disconnect mid-action must fail, not fake success"
    );
    assert!(
        oracle2
            .events()
            .contains(&MockEvent::Click { x: 10, y: 10 }),
        "partial click must be recorded: {:?}",
        oracle2.events()
    );

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}
