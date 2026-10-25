//! The real `agend-git` / `agend-mcp-bridge` binaries must carry the build-id
//! marker the daemon's staleness check (`cli.rs::classify_helper_identity`)
//! reads. Helpers are inspected as bytes, never executed.

const PREFIX: &str = "AGEND-HELPER-BUILD-ID:";

fn assert_marker(bin: &str) {
    let bytes = std::fs::read(bin).expect("read helper binary");
    let expected = format!(
        "{PREFIX}{}:{}{}\0",
        env!("AGEND_BUILD_SHA"),
        env!("AGEND_BUILD_DIRTY"),
        env!("AGEND_BUILD_NONCE_SUFFIX")
    );
    let found = bytes
        .windows(expected.len())
        .any(|w| w == expected.as_bytes());
    assert!(found, "{bin} does not embed {expected:?}");
    // The daemon fails closed on ambiguity: the marker must appear exactly once.
    let count = bytes
        .windows(PREFIX.len())
        .filter(|w| *w == PREFIX.as_bytes())
        .count();
    assert_eq!(count, 1, "{bin} must carry exactly one build-id marker");
}

#[test]
fn agend_git_embeds_build_id_marker() {
    assert_marker(env!("CARGO_BIN_EXE_agend-git"));
}

#[test]
fn agend_mcp_bridge_embeds_build_id_marker() {
    assert_marker(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
}
