//! The daemon keeps one graph open per folder it serves, and a Codex user can
//! have dozens of worktrees. Each open database reserves address space for
//! its largest possible size, so that reservation must be small enough for
//! many graphs to be open in one process at once.

use lge_store::Store;

/// More graphs than anyone keeps open: on a Mac, the library's 8 TiB default
/// reservation fails after about a dozen (`Mmap for size 8796093022208 failed`).
const GRAPHS: usize = 40;

#[test]
fn forty_graphs_can_be_open_in_one_process_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut open = Vec::with_capacity(GRAPHS);
    for i in 0..GRAPHS {
        let path = dir.path().join(format!("g{i}")).join("graph.lbug");
        let store = Store::open(&path).unwrap_or_else(|e| panic!("graph {i} of {GRAPHS} did not open: {e:#}"));
        open.push(store);
    }
    assert_eq!(open.len(), GRAPHS);
}
