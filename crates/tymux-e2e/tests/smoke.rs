#[test]
fn workspace_bin_should_resolve_both_binaries() {
    let tymuxd = tymux_e2e::workspace_bin("tymuxd");
    let tymux = tymux_e2e::workspace_bin("tymux");
    assert!(tymuxd.exists());
    assert!(tymux.exists());
}
