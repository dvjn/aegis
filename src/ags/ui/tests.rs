use super::*;

#[test]
fn long_paths_keep_their_last_two_components() {
    assert_eq!(short_path("~/Workspace/aegis"), "~/Workspace/aegis");
    assert_eq!(
        short_path("/tmp/claude-1000/some/very/long/scratchpad/e2e/repo"),
        "…/e2e/repo"
    );
}
