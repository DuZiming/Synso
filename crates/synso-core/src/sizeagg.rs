//! Directory size aggregation for the flame chart (Task 11): folds the
//! hot index's interned dir tree and file arenas into a [`SizeNode`]
//! tree whose every node carries the summed byte size and the file count
//! of its subtree, with children sorted by size (descending) and
//! truncated to `max_children` — the overflow merged into one synthetic
//! trailing "(other)" node — so a UI can render the biggest branches
//! first without materializing a whole volume.
//!
//! Shape: one pass over the file slots gathers direct per-dir
//! contributions (and per-dir file-slot lists), one pass over the
//! interned dirs gathers the parent -> children adjacency, then a single
//! iterative post-order fold turns direct contributions into subtree
//! totals. Node building only descends as far as the caller's `depth`
//! allows and only sorts/renames what survives `max_children`.
//!
//! Cost: every call is O(dirs + files) time and scratch memory — the
//! whole index is walked, not just the requested subtree — plus node
//! building along the requested depth/width. Deliberately NOT cached
//! (YAGNI): incremental invalidation on every watcher batch would cost
//! more than the walk, which Task 20 will measure; a cache only earns
//! its keep if that measurement says otherwise.

use std::collections::{HashMap, HashSet};
use std::path::{MAIN_SEPARATOR, MAIN_SEPARATOR_STR};

use crate::hotindex::HotIndex;
use crate::model::FileId;

/// One node of the directory-size tree (see module docs). `size` sums
/// the bytes of ALL descendant files — a directory's own row size is 0 —
/// and `file_count` counts descendant files, not directories. Files
/// appear as leaf nodes (their own size, `file_count` 1) so the flame
/// chart can render the deepest level too.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SizeNode {
    /// Index-relative path in the `Hit::path` spelling (`MAIN_SEPARATOR`
    /// joined, scan-root segment first; "" for the virtual root).
    pub path: String,
    /// Final path component, original spelling ("(all)" / "(other)" for
    /// the synthetic nodes).
    pub name: String,
    /// Summed descendant file bytes (a file node: its own size).
    pub size: u64,
    /// Descendant file count (a file node: 1).
    pub file_count: u64,
    /// Sorted by size descending (ties: name ascending), truncated to
    /// `max_children` with the overflow merged into a trailing "(other)"
    /// node; empty when `depth` is exhausted.
    pub children: Vec<SizeNode>,
}

/// Subtree aggregates per interned dir: direct file contributions folded
/// with the children's totals by the post-order pass.
#[derive(Debug, Default, Clone, Copy)]
struct DirAgg {
    size: u64,
    file_count: u64,
}

/// One candidate child of a node, before sorting and truncation: cheap
/// to sort and discard, unlike a fully built [`SizeNode`].
struct ChildEntry {
    name: String,
    size: u64,
    file_count: u64,
    kind: ChildKind,
}

/// What a kept child builds into: another dir node (recursing) or a
/// file leaf (the hot-index slot).
enum ChildKind {
    Dir(FileId),
    File(usize),
}

/// The per-call view [`build_tree`] works from — subtree aggregates per
/// interned dir, dir-tree adjacency and per-dir file-slot lists. Rebuilt
/// on every call (see module docs: deliberately not cached).
struct Aggregates {
    per_dir: HashMap<FileId, DirAgg>,
    child_dirs: HashMap<FileId, Vec<FileId>>,
    root_dirs: Vec<FileId>,
    file_slots: HashMap<FileId, Vec<usize>>,
}

impl Aggregates {
    /// One pass over the interned dirs (adjacency + roots), one over the
    /// file slots (direct contributions + file lists), then the
    /// post-order fold. Every slot's parent dir is interned (the hot
    /// index maintains that invariant on load, upsert and purge), so no
    /// slot is orphaned by the fold.
    fn collect(hot: &HotIndex) -> Aggregates {
        let mut child_dirs: HashMap<FileId, Vec<FileId>> = HashMap::new();
        let mut root_dirs: Vec<FileId> = Vec::new();
        for (id, parent, _) in hot.all_dir_rows() {
            match parent {
                Some(parent) => child_dirs.entry(parent).or_default().push(id),
                None => root_dirs.push(id),
            }
        }
        // Directory slots carry no size and never count (a dir row's own
        // size is 0); they are covered by the interned `dirs` tree.
        let mut per_dir: HashMap<FileId, DirAgg> = HashMap::new();
        let mut file_slots: HashMap<FileId, Vec<usize>> = HashMap::new();
        for slot in 0..hot.slot_count() {
            if hot.is_dir_entry(slot) {
                continue;
            }
            let parent = hot.parent_dir(slot);
            let agg = per_dir.entry(parent).or_default();
            agg.size = agg.size.saturating_add(hot.size(slot));
            agg.file_count += 1;
            file_slots.entry(parent).or_default().push(slot);
        }
        Self::fold(&child_dirs, &root_dirs, &mut per_dir);
        Aggregates {
            per_dir,
            child_dirs,
            root_dirs,
            file_slots,
        }
    }

    /// The post-order fold: subtree totals per dir. Iterative (an
    /// explicit stack, children emitted before parents) so a deep tree
    /// cannot overflow the call stack; the `entered` set guards the
    /// traversal against a corrupt parent cycle (impossible via the
    /// store's API, mirroring the hot index's own budget-guarded walks)
    /// so even that terminates.
    fn fold(
        child_dirs: &HashMap<FileId, Vec<FileId>>,
        root_dirs: &[FileId],
        per_dir: &mut HashMap<FileId, DirAgg>,
    ) {
        enum Frame {
            Enter(FileId),
            Emit(FileId),
        }
        let mut stack: Vec<Frame> = root_dirs.iter().rev().map(|&id| Frame::Enter(id)).collect();
        let mut entered: HashSet<FileId> = HashSet::new();
        while let Some(frame) = stack.pop() {
            match frame {
                Frame::Enter(id) => {
                    if !entered.insert(id) {
                        continue; // corrupt cycle: already on the path
                    }
                    stack.push(Frame::Emit(id));
                    if let Some(children) = child_dirs.get(&id) {
                        for &child in children {
                            stack.push(Frame::Enter(child));
                        }
                    }
                }
                Frame::Emit(id) => {
                    let mut total = per_dir.get(&id).copied().unwrap_or_default();
                    if let Some(children) = child_dirs.get(&id) {
                        for &child in children {
                            if let Some(child_agg) = per_dir.get(&child) {
                                total.size = total.size.saturating_add(child_agg.size);
                                total.file_count += child_agg.file_count;
                            }
                        }
                    }
                    per_dir.insert(id, total);
                }
            }
        }
    }
}

/// What the root path named: the virtual root, an interned dir row, or a
/// file slot.
enum RootTarget {
    VirtualRoot,
    Dir(FileId),
    File(usize),
}

/// Resolves an index-relative path (the `Hit::path` spelling; both
/// separators accepted) to what it names. The longest dir chain resolves
/// first; only a FILE as the FINAL segment yields a file target — a file
/// mid-path can never have children below it, so anything else is an
/// unknown root.
fn resolve_root(hot: &HotIndex, root: &str) -> Option<RootTarget> {
    let segments: Vec<&str> = root
        .split([MAIN_SEPARATOR, '/'])
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.is_empty() {
        return Some(RootTarget::VirtualRoot);
    }
    let mut parent: Option<FileId> = None;
    let mut matched = 0usize;
    while matched < segments.len() {
        match hot.dir_child(parent, segments[matched]) {
            Some(id) => {
                parent = Some(id);
                matched += 1;
            }
            None => break,
        }
    }
    if matched == segments.len() {
        return parent.map(RootTarget::Dir);
    }
    if matched + 1 == segments.len() {
        let parent = parent?;
        if let Some(slot) = hot.slot_in_dir(parent, segments[matched]) {
            if !hot.is_dir_entry(slot) {
                return Some(RootTarget::File(slot));
            }
        }
    }
    None
}

/// Builds the tree behind `root` over a hot-index snapshot (the pure
/// half of `Engine::size_tree`); `None` for an unknown root. Unknown
/// roots return before any aggregation work happens.
pub fn build_tree(hot: &HotIndex, root: &str, depth: u8, max_children: u8) -> Option<SizeNode> {
    match resolve_root(hot, root)? {
        RootTarget::VirtualRoot => {
            let aggregates = Aggregates::collect(hot);
            let (mut size, mut file_count) = (0u64, 0u64);
            for &id in &aggregates.root_dirs {
                if let Some(agg) = aggregates.per_dir.get(&id) {
                    size = size.saturating_add(agg.size);
                    file_count += agg.file_count;
                }
            }
            Some(SizeNode {
                path: String::new(),
                name: "(all)".to_string(),
                size,
                file_count,
                children: child_nodes(hot, &aggregates, None, "", depth, max_children),
            })
        }
        RootTarget::Dir(id) => {
            let aggregates = Aggregates::collect(hot);
            let path = dir_path(hot, id);
            let name = hot
                .dir_row(id)
                .map_or_else(String::new, |(_, name)| name.to_string());
            Some(dir_node(
                hot,
                &aggregates,
                id,
                name,
                path,
                depth,
                max_children,
            ))
        }
        RootTarget::File(slot) => {
            let parent = hot.parent_dir(slot);
            let mut chain = dir_chain(hot, parent);
            chain.push(hot.name_original(slot));
            Some(file_node(hot, slot, chain.join(MAIN_SEPARATOR_STR)))
        }
    }
}

/// The node for an interned dir: its subtree totals plus, while `levels`
/// remain, its materialized children. `name` comes from the caller — the
/// resolver or the child entry — so it is read from `dir_row` exactly
/// once per node.
fn dir_node(
    hot: &HotIndex,
    aggregates: &Aggregates,
    id: FileId,
    name: String,
    path: String,
    levels: u8,
    max_children: u8,
) -> SizeNode {
    let agg = aggregates.per_dir.get(&id).copied().unwrap_or_default();
    let children = child_nodes(hot, aggregates, Some(id), &path, levels, max_children);
    SizeNode {
        path,
        name,
        size: agg.size,
        file_count: agg.file_count,
        children,
    }
}

/// A file leaf: its own size, `file_count` 1, no children.
fn file_node(hot: &HotIndex, slot: usize, path: String) -> SizeNode {
    SizeNode {
        path,
        name: hot.name_original(slot).to_string(),
        size: hot.size(slot),
        file_count: 1,
        children: Vec::new(),
    }
}

/// Materializes the children of `dir` (`None` = the virtual root's
/// forest of scan-root segments) under `parent_path`, sorted by size
/// descending (ties: name ascending) and truncated to `max_children` —
/// the overflow merged into one trailing "(other)" node, which counts
/// against the budget. `levels` is how many levels may appear BELOW the
/// children (each kept child recurses with `levels - 1`).
fn child_nodes(
    hot: &HotIndex,
    aggregates: &Aggregates,
    dir: Option<FileId>,
    parent_path: &str,
    levels: u8,
    max_children: u8,
) -> Vec<SizeNode> {
    if levels == 0 || max_children == 0 {
        return Vec::new();
    }
    let mut entries: Vec<ChildEntry> = Vec::new();
    let (dir_entries, file_slots): (&[FileId], &[usize]) = match dir {
        Some(id) => (
            aggregates
                .child_dirs
                .get(&id)
                .map_or(&[][..], Vec::as_slice),
            aggregates
                .file_slots
                .get(&id)
                .map_or(&[][..], Vec::as_slice),
        ),
        None => (&aggregates.root_dirs[..], &[][..]),
    };
    for &child in dir_entries {
        let Some((_, name)) = hot.dir_row(child) else {
            continue;
        };
        let agg = aggregates.per_dir.get(&child).copied().unwrap_or_default();
        entries.push(ChildEntry {
            name: name.to_string(),
            size: agg.size,
            file_count: agg.file_count,
            kind: ChildKind::Dir(child),
        });
    }
    for &slot in file_slots {
        entries.push(ChildEntry {
            name: hot.name_original(slot).to_string(),
            size: hot.size(slot),
            file_count: 1,
            kind: ChildKind::File(slot),
        });
    }
    // A total order — same-parent entries cannot share a name — so the
    // sort is deterministic despite being unstable.
    entries.sort_unstable_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));

    let budget = max_children as usize;
    let overflow = entries.len() > budget;
    let kept = if overflow {
        &entries[..budget - 1] // budget >= 1 here; "(other)" takes the last slot
    } else {
        &entries[..]
    };
    let mut nodes: Vec<SizeNode> = kept
        .iter()
        .map(|entry| {
            child_node(
                hot,
                aggregates,
                entry,
                parent_path,
                levels - 1,
                max_children,
            )
        })
        .collect();
    if overflow {
        let (size, file_count) = entries[budget - 1..]
            .iter()
            .fold((0u64, 0u64), |(size, count), entry| {
                (size.saturating_add(entry.size), count + entry.file_count)
            });
        nodes.push(SizeNode {
            path: child_path(parent_path, "(other)"),
            name: "(other)".to_string(),
            size,
            file_count,
            children: Vec::new(),
        });
    }
    nodes
}

/// Builds one kept child: a dir node recursing with `levels - 1`, or a
/// file leaf.
fn child_node(
    hot: &HotIndex,
    aggregates: &Aggregates,
    entry: &ChildEntry,
    parent_path: &str,
    levels: u8,
    max_children: u8,
) -> SizeNode {
    let path = child_path(parent_path, &entry.name);
    match entry.kind {
        ChildKind::Dir(id) => {
            let name = entry.name.clone();
            dir_node(hot, aggregates, id, name, path, levels, max_children)
        }
        ChildKind::File(slot) => file_node(hot, slot, path),
    }
}

/// `parent_path` joined with `name` (`MAIN_SEPARATOR`); the virtual
/// root's empty path makes its children's paths their plain names.
fn child_path(parent_path: &str, name: &str) -> String {
    if parent_path.is_empty() {
        name.to_string()
    } else {
        format!("{parent_path}{MAIN_SEPARATOR}{name}")
    }
}

/// Root-first dir names from the root segment down to `id`'s own name.
/// Budget-guarded like the hot index's own walks so even a corrupt
/// parent cycle terminates.
fn dir_chain(hot: &HotIndex, id: FileId) -> Vec<&str> {
    let mut chain: Vec<&str> = Vec::new();
    let mut budget = hot.dir_tree_len() + 1;
    let mut current = Some(id);
    while let Some(cid) = current {
        if budget == 0 {
            break;
        }
        budget -= 1;
        let Some((parent, name)) = hot.dir_row(cid) else {
            break;
        };
        chain.push(name);
        current = parent;
    }
    chain.reverse();
    chain
}

/// Full index-relative path of an interned dir.
fn dir_path(hot: &HotIndex, id: FileId) -> String {
    dir_chain(hot, id).join(MAIN_SEPARATOR_STR)
}
