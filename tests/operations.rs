#[test]
fn readiness_requires_reconciliation_and_never_recovers_after_stop() {
    let ops = orbit::ops::Operations::default();
    assert!(!ops.ready());
    ops.reconciliation(true);
    assert!(ops.ready());
    ops.reconciliation(false);
    assert!(ops.metrics().contains("orbit_reconciliation_error_total 1"));
    ops.stop();
    ops.reconciliation(true);
    assert!(!ops.ready());
    assert!(ops.metrics().contains("orbit_process_draining 1"));
}
