//! Latency of the hot verbs on a synthetic store (docs/PLAN.md §8 targets are for a 2 M-path store and are
//! checked by the slow tests; this bench tracks regressions on the small fixture).

use criterion::{criterion_group, criterion_main, Criterion};
use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_traj"))
}

fn setup() -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    for r in 1..=5 {
        for i in 0..200 {
            let p = src.join(format!(
                "rounds/round-{r:04}/builder/logs/call{i:03}.stdout"
            ));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("run {i} of round {r}\n")).unwrap();
        }
        let p = src.join(format!("rounds/round-{r:04}/reviewer/review.json"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "{\"verdict\":\"continue\"}\n").unwrap();
    }
    let store = tmp.path().join("store");
    assert!(Command::new(bin())
        .arg("pack")
        .arg(&src)
        .arg("--out")
        .arg(&store)
        .status()
        .unwrap()
        .success());
    (tmp, store)
}

fn verbs(c: &mut Criterion) {
    let (_tmp, store) = setup();
    let run = |args: &[&str]| {
        let st = Command::new(bin())
            .arg("-S")
            .arg(&store)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
    };
    c.bench_function("ls", |b| {
        b.iter(|| run(&["ls", "rounds/round-0003/builder/logs"]))
    });
    c.bench_function("stat", |b| {
        b.iter(|| run(&["stat", "rounds/round-0003/reviewer/review.json"]))
    });
    c.bench_function("cat", |b| {
        b.iter(|| run(&["cat", "rounds/round-0003/reviewer/review.json"]))
    });
    c.bench_function("find_name", |b| {
        b.iter(|| run(&["find", "--name", "review.json"]))
    });
}

criterion_group!(benches, verbs);
criterion_main!(benches);
