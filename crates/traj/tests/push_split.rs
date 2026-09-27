//! `traj commit --push` uploads in parts below a per-push size limit, leaving history unchanged.

mod common;

use common::*;
use std::fs;

/// The bare remote ends up at the same commit as the work tree, no temporary refs survive on
/// either side, and the second push (small) goes out in one piece.
#[test]
fn push_splits_large_uploads_into_parts_below_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let data = tmp.path().join("data");
    init_repo(&repo, &data, "none");
    let bare = tmp.path().join("remote.git");
    fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]);
    git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()]);
    git(&repo, &["push", "-q", "-u", "origin", "HEAD"]);

    fixture(&data.join("run1"));
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(data.join("run1"))
        .args([
            "--out",
            "stores/run1.trajstore",
            "--adapter",
            "none",
            "--rules",
            "none",
        ])
        .assert()
        .success();
    let out = traj()
        .current_dir(&repo)
        .args([
            "commit",
            "--push",
            "--push-limit",
            "8K",
            "stores/run1.trajstore",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(
        out.status.success(),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let parts: usize = text
        .lines()
        .find_map(|l| {
            l.strip_prefix("pushing ")
                .and_then(|r| r.split(" in ").nth(1))
        })
        .and_then(|r| r.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("a split push reports its part count");
    assert!(
        parts >= 2,
        "expected several parts under an 8 KiB limit: {text}"
    );
    assert!(text.trim_end().ends_with("pushed"), "{text}");

    let local = git(&repo, &["rev-parse", "HEAD"]);
    let remote = git(&bare, &["rev-parse", "HEAD"]);
    assert_eq!(local, remote, "the remote branch is the local commit");
    assert_eq!(git(&bare, &["for-each-ref", "refs/traj-upload"]).trim(), "");
    assert_eq!(git(&repo, &["for-each-ref", "refs/traj-upload"]).trim(), "");
    assert!(
        !fs::read_dir(repo.join(".git")).unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("traj-upload-index")),
        "the private index file is removed"
    );
    // one commit per batch: the upload parts never entered the branch
    assert_eq!(git(&repo, &["rev-list", "--count", "HEAD"]).trim(), "2");
    let remote_store = git(
        &bare,
        &[
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
            "stores/run1.trajstore",
        ],
    );
    assert!(
        remote_store.contains("MANIFEST.json") && remote_store.contains("packs/"),
        "{remote_store}"
    );

    // a second batch under the limit is a single push
    fs::write(
        data.join("run1/rounds/round-0001/builder/extra.txt"),
        "one more\n",
    )
    .unwrap();
    traj()
        .current_dir(&repo)
        .args(["pack"])
        .arg(data.join("run1"))
        .args([
            "--out",
            "stores/run1.trajstore",
            "--adapter",
            "none",
            "--rules",
            "none",
        ])
        .assert()
        .success();
    let out = traj()
        .current_dir(&repo)
        .args([
            "commit",
            "--push",
            "--push-limit",
            "1G",
            "stores/run1.trajstore",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("pushed (") && !text.contains(" parts"),
        "{text}"
    );
    assert_eq!(
        git(&repo, &["rev-parse", "HEAD"]),
        git(&bare, &["rev-parse", "HEAD"])
    );
}
