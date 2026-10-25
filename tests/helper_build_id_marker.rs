//! The real `agend-git` / `agend-mcp-bridge` binaries must carry the build-id
//! marker the daemon's staleness check (`cli.rs::classify_helper_identity`)
//! reads. Helpers are inspected as bytes, never executed.

const PREFIX: &str = "AGEND-HELPER-BUILD-ID:";

fn assert_marker(bin: &str) {
    let bytes = std::fs::read(bin).expect("read helper binary");
    let expected = format!(
        "{PREFIX}{}:{}\0",
        env!("AGEND_BUILD_SHA"),
        env!("AGEND_BUILD_DIRTY")
    );
    let found = bytes
        .windows(expected.len())
        .any(|w| w == expected.as_bytes());
    assert!(found, "{bin} does not embed {expected:?}");
}

#[test]
fn agend_git_embeds_build_id_marker() {
    assert_marker(env!("CARGO_BIN_EXE_agend-git"));
}

#[test]
fn agend_mcp_bridge_embeds_build_id_marker() {
    assert_marker(env!("CARGO_BIN_EXE_agend-mcp-bridge"));
}
