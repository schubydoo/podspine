//! Workspace smoke test — proves the test harness compiles and runs in CI from
//! commit #1. Real coverage arrives with each pipeline stage: the synthetic-M4B
//! integration test (Task 1.6) lives here once the splitter/feed crates land.

#[test]
fn workspace_builds_and_tests_run() {
    // Placeholder: replaced by the prober -> splitter -> feed -> self-check
    // pipeline assertion in Sprint 1 (Task 1.6).
    assert_eq!(2 + 2, 4);
}
