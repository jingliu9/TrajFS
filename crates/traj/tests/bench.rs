//! `traj bench`: the built-in benchmark writes one JSON report (schema 1) with a measurement for every hot
//! verb and for the mount (or says why the mount was skipped), and leaves no mount behind. Timing bounds
//! belong here, not in the correctness tests.

mod common;

use common::*;

#[test]
fn bench_reports_the_verbs_and_the_mount() {
    let e = packed("none");
    let out = traj()
        .arg("-S")
        .arg(&e.store)
        .args(["bench", "--ls-dir", "rounds"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let j: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(j["schema"], 1, "{j}");
    let runs = j["runs"].as_u64().unwrap() as usize;
    assert!(j["store"]["paths"].as_u64().unwrap() > 0, "{j}");
    // every measurement carries its samples, minimum and median
    let measurements = |section: &serde_json::Value| -> Vec<String> {
        section
            .as_array()
            .unwrap_or_else(|| panic!("not a list: {section}"))
            .iter()
            .map(|m| {
                assert_eq!(m["runs"].as_array().unwrap().len(), runs, "{m}");
                let min = m["min"].as_f64().unwrap();
                let median = m["median"].as_f64().unwrap();
                assert!(min > 0.0 && min <= median, "{m}");
                m["name"].as_str().unwrap().to_string()
            })
            .collect()
    };
    let scenarios = measurements(&j["scenarios"]);
    for k in [
        "ls",
        "stat",
        "cat",
        "find_name",
        "du",
        "tree",
        "grep",
        "verify",
    ] {
        assert!(scenarios.iter().any(|s| s == k), "missing {k}: {j}");
    }
    assert_eq!(
        scenarios.iter().any(|s| s == "sql_count"),
        cfg!(feature = "sql"),
        "sql_count is measured exactly when the build has SQL: {j}"
    );
    for s in j["scenarios"].as_array().unwrap() {
        assert_eq!(s["ok"], true, "scenario {} did not succeed: {j}", s["name"]);
    }
    let in_process = measurements(&j["in_process"]);
    for k in ["children", "read_file"] {
        assert!(in_process.iter().any(|s| s == k), "missing {k}: {j}");
    }
    if fuse_available() {
        assert_eq!(j["mount"]["status"], "measured", "{j}");
        assert!(j["mount"]["mount_s"].as_f64().unwrap() > 0.0, "{j}");
        assert!(j["mount"]["first_ls_s"].as_f64().unwrap() > 0.0, "{j}");
        let on_mount = measurements(&j["mount"]["scenarios"]);
        for k in ["mount_ls", "mount_cat"] {
            assert!(on_mount.iter().any(|s| s == k), "missing {k}: {j}");
        }
    } else {
        assert_eq!(j["mount"]["status"], "skipped", "{j}");
        assert!(j["mount"]["reason"].is_string(), "{j}");
    }
    assert!(!mounted(&std::env::temp_dir()), "bench left a mount behind");
}
