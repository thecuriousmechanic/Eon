// Separate process from worktree/overlay tests: a concurrent fork can briefly inherit an
// otherwise-closed writer lock before exec, invalidating a nonblocking immediate-reboot test.
#[path = "recovery_tests/cases.rs"]
mod recovery_tests;
