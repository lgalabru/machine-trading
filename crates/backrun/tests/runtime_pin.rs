#[test]
fn surfpool_provenance_matches_resolved_workspace_dependency() {
    let lock = include_str!("../../../Cargo.lock");
    let revisions: Vec<_> = lock
        .lines()
        .filter(|line| {
            line.starts_with("source = \"git+https://github.com/solana-foundation/surfpool?")
        })
        .map(|line| line.rsplit('#').next().unwrap().trim_end_matches('"'))
        .collect();
    assert!(!revisions.is_empty());
    assert!(
        revisions
            .iter()
            .all(|rev| *rev == mth_backrun::programs::SURFPOOL_REV)
    );
}
