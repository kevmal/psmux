//! Pure synthetic tables: no servers, panes, or live process queries.
use super::*;

fn snapshot(rows: &[(u32, u32, &str, Option<u64>)]) -> ProcessSnapshot {
    ProcessSnapshot {
        entries: rows
            .iter()
            .map(|(pid, parent, name, _)| (*pid, *parent, (*name).to_string()))
            .collect(),
        captured_at: u64::MAX,
        creation_times: std::sync::Mutex::new(
            rows.iter().map(|(pid, _, _, born)| (*pid, *born)).collect(),
        ),
    }
}

#[test]
fn older_child_of_reused_root_pid_is_rejected_with_its_subtree() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(500)),
        (90, 10, "tmux.exe", Some(100)),
        (99, 90, "cletus-mcp-shim.exe", Some(600)),
    ]);
    assert_eq!(deepest_descendant(&table, 10), None);
    assert_eq!(foreground_child_pid(&table, 10), None);
}

#[test]
fn newer_child_and_normal_deep_chain_are_selected() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "bash.exe", Some(200)),
        (30, 20, "bash.exe", Some(300)),
        (40, 30, "cat.exe", Some(400)),
    ]);
    assert_eq!(deepest_descendant(&table, 10), Some((40, "cat.exe".into())));
    assert_eq!(foreground_child_pid(&table, 10), Some(30));
}

#[test]
fn reused_intermediate_parent_stops_at_last_proven_ancestor() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "bash.exe", Some(300)),
        (30, 20, "node.exe", Some(200)),
        (40, 30, "shim.exe", Some(400)),
    ]);
    assert_eq!(
        deepest_descendant(&table, 10),
        Some((20, "bash.exe".into()))
    );
    assert_eq!(foreground_child_pid(&table, 10), Some(20));
}

#[test]
fn unreadable_child_age_falls_back_to_root() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "bash.exe", None),
        (30, 20, "node.exe", Some(300)),
    ]);
    assert_eq!(deepest_descendant(&table, 10), None);
    assert_eq!(foreground_child_pid(&table, 10), None);
}

#[test]
fn unreadable_parent_age_cannot_prove_a_child() {
    let table = snapshot(&[(10, 0, "pwsh.exe", None), (20, 10, "node.exe", Some(200))]);
    assert_eq!(deepest_descendant(&table, 10), None);
    assert_eq!(foreground_child_pid(&table, 10), None);
}

#[test]
fn unknown_higher_pid_never_outranks_proven_child() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "node.exe", Some(200)),
        (90, 10, "bash.exe", None),
        (99, 90, "shim.exe", Some(300)),
    ]);
    assert_eq!(
        deepest_descendant(&table, 10),
        Some((20, "node.exe".into()))
    );
    assert_eq!(foreground_child_pid(&table, 10), Some(20));
}

#[test]
fn unknown_grandchild_does_not_replace_proven_wrapper() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "bash.exe", Some(200)),
        (30, 20, "node.exe", None),
    ]);
    assert_eq!(
        deepest_descendant(&table, 10),
        Some((20, "bash.exe".into()))
    );
    assert_eq!(foreground_child_pid(&table, 10), Some(20));
}

#[test]
fn equal_creation_times_are_allowed() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "node.exe", Some(100)),
    ]);
    assert_eq!(
        deepest_descendant(&table, 10),
        Some((20, "node.exe".into()))
    );
}

#[test]
fn proven_siblings_preserve_highest_pid_selection_and_skip_system_exes() {
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "node.exe", Some(200)),
        (30, 10, "python.exe", Some(150)),
        (99, 10, "conhost.exe", Some(300)),
    ]);
    assert_eq!(
        deepest_descendant(&table, 10),
        Some((30, "python.exe".into()))
    );
    assert_eq!(foreground_child_pid(&table, 10), Some(30));
}

#[test]
fn stale_or_unproven_branch_cannot_claim_a_vt_bridge() {
    for bridge_age in [Some(50), None] {
        let table = snapshot(&[
            (10, 0, "pwsh.exe", Some(100)),
            (20, 10, "bash.exe", bridge_age),
            (30, 20, "wsl.exe", Some(300)),
        ]);
        assert!(!tree_has_vt_bridge(&table, 10));
    }
    let table = snapshot(&[
        (10, 0, "pwsh.exe", Some(100)),
        (20, 10, "bash.exe", Some(200)),
        (30, 20, "wsl.exe", Some(300)),
    ]);
    assert!(tree_has_vt_bridge(&table, 10));
}

#[test]
fn creation_queries_are_cached_including_failures_and_post_snapshot_reuse() {
    for queried_time in [Some(50), None, Some(200)] {
        let table = ProcessSnapshot {
            entries: vec![(10, 0, "pwsh.exe".into())],
            captured_at: 100,
            creation_times: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        let expected = queried_time.filter(|born| *born <= 100);
        assert_eq!(table.creation_time_with(10, |_| queried_time), expected);
        assert_eq!(
            table.creation_time_with(10, |_| panic!("queried twice")),
            expected
        );
        assert_eq!(
            table.creation_time_with(20, |_| panic!("PID absent from snapshot")),
            None
        );
    }
}
