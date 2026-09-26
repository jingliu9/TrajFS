use std::fs;

#[test]
fn readiness_errors_are_distinct_from_no_ready_marker() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("adapter.toml");
    fs::write(
        &path,
        "name = 'fixture'\n[batch_ready]\nrun_glob = 'run-*'\nmarkers = ['**/DONE']\n",
    )
    .unwrap();
    let adapter = trajfs_adapters::resolve(path.to_str().unwrap(), None).unwrap();
    let source = temp.path().join("run-1");
    assert!(adapter.try_batch_ready(&source, &[]).is_err());
    fs::create_dir(&source).unwrap();
    assert_eq!(adapter.try_batch_ready(&source, &[]).unwrap(), None);
    let deep = source.join("a/b/c/d/e/f/g/episode");
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("DONE"), "").unwrap();
    assert_eq!(
        adapter.try_batch_ready(&source, &[]).unwrap(),
        Some("episode".into())
    );
    assert_eq!(
        adapter
            .try_batch_ready(&source, &["episode".into()])
            .unwrap(),
        None
    );
}
