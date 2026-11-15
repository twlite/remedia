//! Regression test for callsites silenced by a thread that has no dispatcher in scope.
//!
//! This file is deliberately a single-test binary: the defect only exists while exactly one
//! `Dispatch` is registered in the process, so no other test may create a dispatcher here.

use std::thread;
use std::time::Duration;

use remedia_observability::{Observability, ObservabilityConfig};

fn emit_playback_event() {
    tracing::info!(
        name: "test.callsite_interest",
        target: "remedia_core::playback",
        playback_id = 7_u64,
        "probe"
    );
}

#[test]
fn a_callsite_first_reached_outside_the_dispatcher_scope_still_reaches_it() {
    let observability = Observability::new(
        ObservabilityConfig::default()
            .with_filter("info")
            .expect("valid filter")
            .with_console_enabled(false),
    )
    .expect("create observability context");
    let diagnostics = observability.diagnostics();
    let handle = observability.handle();
    let dispatch = handle
        .dispatch()
        .expect("configured handle has a dispatcher");

    // A session with the default handle (or any foreign thread) reaches the callsite first,
    // with no dispatcher in scope.
    thread::spawn(emit_playback_event)
        .join()
        .expect("unscoped emitter finishes");
    assert!(
        diagnostics
            .recv_timeout(Duration::from_millis(100))
            .is_err(),
        "an unscoped thread must not reach the scoped dispatcher"
    );

    tracing::dispatcher::with_default(dispatch, emit_playback_event);

    let event = diagnostics
        .recv_timeout(Duration::from_secs(5))
        .expect("the scoped dispatcher still receives a callsite first hit elsewhere");
    assert_eq!(event.event, "test.callsite_interest");
    assert_eq!(event.playback_id, Some(7));
}
