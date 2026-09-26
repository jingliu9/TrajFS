//! The catalog verbs `ls`, `tree`, `find`, `du` and `stat`: their output matches the packed tree and the
//! coreutils run on an extracted copy, and they fail cleanly on missing directories and broken stdout.

mod common;

use common::*;

#[test]
fn ls_find_stat_and_du_describe_the_packed_tree() {
    let e = packed("none");
    let s = traj_stdout(&e.store, &["ls", "rounds/round-0001/builder"]);
    assert_eq!(
        s.lines().collect::<Vec<_>>(),
        vec!["logs/", "sp ace/", "empty", "events.jsonl", "link", "run"]
    );
    let s = traj_stdout(&e.store, &["find", "--name", "*.stdout"]);
    assert_eq!(s.lines().count(), 2);
    let s = traj_stdout(
        &e.store,
        &["find", "--attr", "round=2", "--attr", "role=builder"],
    );
    assert_eq!(
        s.lines().collect::<Vec<_>>(),
        vec![
            "rounds/round-0002/builder/big.bin",
            "rounds/round-0002/builder/logs/a.stdout"
        ]
    );
    let s = traj_stdout(&e.store, &["stat", "rounds/round-0001/builder/link"]);
    assert!(
        s.contains("kind:     Symlink") && s.contains("round=1,role=builder"),
        "{s}"
    );
    let s = traj_stdout(&e.store, &["du", "rounds"]);
    assert!(s.contains("10 files") && s.contains("8 blobs"), "{s}");
    traj()
        .arg("-S")
        .arg(&e.store)
        .args(["ls", "no/such/dir"])
        .assert()
        .failure();
}

#[test]
fn catalog_verbs_equal_coreutils_on_the_extracted_tree() {
    let e = packed("none");
    let out = extracted(&e);
    for d in ["rounds/round-0001/builder", "rounds", ""] {
        let mut want = lines_of("ls", &["-A", if d.is_empty() { "." } else { d }], &out);
        want.retain(|n| !n.is_empty());
        let mut got: Vec<String> = traj_lines(&e.store, &["ls", d])
            .into_iter()
            .map(|n| n.trim_end_matches('/').to_string())
            .collect();
        got.sort();
        want.sort();
        assert_eq!(got, want, "ls {d}");
    }
    let mut want: Vec<String> = lines_of("find", &[".", "-type", "f", "-o", "-type", "l"], &out)
        .into_iter()
        .map(|p| p.trim_start_matches("./").to_string())
        .collect();
    want.sort();
    let got = traj_lines(&e.store, &["find"]);
    assert_eq!(got, want, "find");
    let du = lines_of("du", &["-sb", "--apparent-size", "rounds"], &out);
    let want_bytes: i64 = du[0].split_whitespace().next().unwrap().parse().unwrap();
    // du -b counts directories too; compare the file bytes via the catalog sum instead
    if sql_available() {
        let q = traj_stdout(
            &e.store,
            &[
                "sql",
                "--csv",
                "select sum(size) from files where path like 'rounds/%'",
            ],
        );
        let got_bytes: i64 = q.lines().nth(1).unwrap().trim().parse().unwrap();
        assert!(
            got_bytes <= want_bytes && want_bytes - got_bytes < 64 * 1024,
            "du {want_bytes} vs catalog {got_bytes}"
        );
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
    let t = traj_lines(&e.store, &["tree", "rounds", "--depth", "2"]);
    assert!(
        t.iter().any(|l| l.contains("round-0001/")) && t.iter().any(|l| l.contains("builder/")),
        "{t:?}"
    );
}

#[test]
fn tree_rejects_a_missing_directory() {
    let fixture = SmallStore::new();
    fixture.traj().args(["tree", "missing"]).assert().code(2);
}

#[test]
fn find_rejects_a_missing_directory() {
    let fixture = SmallStore::new();
    fixture.traj().args(["find", "missing"]).assert().code(2);
}

#[test]
fn recursive_ls_lists_the_root_once() {
    let fixture = SmallStore::new();
    let output = fixture.traj().args(["ls", "-R"]).output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter(|line| *line == ".:")
            .count(),
        1
    );
}

#[test]
#[cfg(unix)]
fn find_reports_stdout_write_errors() {
    let fixture = SmallStore::new();
    let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
    drop(reader);
    let result = std::process::Command::new(traj_bin())
        .current_dir(fixture.dir.path())
        .env_remove("TRAJ_CONFIG")
        .env_remove("TRAJ_STORE")
        .arg("-S")
        .arg(&fixture.store)
        .arg("find")
        .stdout(std::process::Stdio::from(std::os::fd::OwnedFd::from(
            writer,
        )))
        .output()
        .unwrap();
    assert_eq!(result.status.code(), Some(2), "{result:?}");
}
