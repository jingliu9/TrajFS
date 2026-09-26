//! Rule profiles: `no-build-products` leaves build products out of the pack and records each exclusion with
//! the rule that caused it.

mod common;

use common::*;

#[test]
fn rules_exclude_build_products_and_record_them() {
    let e = packed("no-build-products");
    let out = extracted(&e);
    assert_eq!(
        snapshot(&e.src, &[".cache", "node_modules"]),
        snapshot(&out, &[])
    );
    if sql_available() {
        let s = traj_stdout(
            &e.store,
            &["sql", "select path, rule from excluded order by path"],
        );
        assert!(
            s.contains(".cache") && s.contains("node_modules") && s.contains("exclude_dirs"),
            "{s}"
        );
    } else {
        eprintln!("skipping SQL checks: built without the sql feature");
    }
}
