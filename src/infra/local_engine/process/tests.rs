use super::{OperationCompletion, StopOutcome};

#[test]
fn completion_is_once_only_and_late_subscriber_reads_first_result() {
    let (completion, _initial_rx) = OperationCompletion::new();
    assert!(completion.complete("first".to_string()));
    assert!(!completion.complete("second".to_string()));

    let late_rx = completion.subscribe();
    assert_eq!(late_rx.borrow().as_deref(), Some("first"));
}

#[test]
fn failed_stop_outcome_is_persistent_for_all_waiters() {
    let (completion, mut first_rx) = OperationCompletion::new();
    let second_rx = completion.subscribe();
    assert!(completion.complete(StopOutcome::Failed {
        message: "forced failure".to_string(),
    }));

    let first = first_rx.borrow_and_update().clone();
    let second = second_rx.borrow().clone();
    assert!(matches!(
        first,
        Some(StopOutcome::Failed { ref message }) if message == "forced failure"
    ));
    assert!(matches!(
        second,
        Some(StopOutcome::Failed { ref message }) if message == "forced failure"
    ));

    let _ = StopOutcome::Done;
}
