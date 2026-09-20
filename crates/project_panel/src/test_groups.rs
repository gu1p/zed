use super::*;
use globset::{Glob, GlobMatcher};

#[derive(Clone)]
pub(super) struct TestGroup {
    pub parent: SelectedEntry,
    pub path: Arc<RelPath>,
    pub files: Vec<GitEntry>,
}

#[derive(Clone, Default)]
pub(super) struct TestGroups {
    pub groups: HashMap<ProjectEntryId, TestGroup>,
    pub members: HashMap<ProjectEntryId, ProjectEntryId>,
    pub expanded: HashSet<SelectedEntry>,
    ids: HashMap<SelectedEntry, ProjectEntryId>,
    next_id: Option<usize>,
}

impl TestGroups {
    pub fn begin_update(&mut self, parent_exists: impl Fn(&SelectedEntry) -> bool) {
        self.ids.retain(|parent, _| parent_exists(parent));
        self.expanded.retain(|parent| parent_exists(parent));
        self.groups.clear();
        self.members.clear();
    }

    pub fn patterns(patterns: &[String]) -> Vec<GlobMatcher> {
        patterns
            .iter()
            .filter_map(|pattern| match Glob::new(pattern) {
                Ok(glob) => Some(glob.compile_matcher()),
                Err(error) => {
                    log::error!(
                        "Invalid project_panel.test_file_patterns glob {pattern:?}: {error}"
                    );
                    None
                }
            })
            .collect()
    }

    pub fn group_entries(
        &mut self,
        entries: &mut Vec<GitEntry>,
        snapshot: &worktree::Snapshot,
        patterns: &[GlobMatcher],
        reveal: Option<ProjectEntryId>,
        id_is_real: impl Fn(ProjectEntryId) -> bool,
    ) {
        let worktree_id = snapshot.id();
        let mut files_by_parent: HashMap<Arc<RelPath>, Vec<GitEntry>> = HashMap::default();
        let mut subtree_ends = HashMap::default();
        for (index, entry) in entries.iter().enumerate() {
            for ancestor in entry.path.ancestors() {
                subtree_ends.insert(ancestor.to_owned(), index);
            }
            if entry.id != NEW_ENTRY_ID
                && entry.is_file()
                && entry
                    .path
                    .file_name()
                    .is_some_and(|name| patterns.iter().any(|pattern| pattern.is_match(name)))
                && let Some(parent) = entry.path.parent()
            {
                files_by_parent
                    .entry(parent.into())
                    .or_default()
                    .push(entry.clone());
            }
        }

        let mut insertions = Vec::new();
        for (path, files) in files_by_parent {
            let Some(parent_entry) = snapshot.entry_for_path(&path) else {
                continue;
            };
            let Some(&index) = subtree_ends.get(path.as_ref()) else {
                continue;
            };
            let parent = SelectedEntry {
                worktree_id,
                entry_id: parent_entry.id,
            };
            // Virtual rows share the list's ID type, but must never alias a real
            // entry (including remote IDs) or the inline new-file editor.
            let id = if let Some(id) = self.ids.get(&parent).copied().filter(|id| !id_is_real(*id))
            {
                id
            } else {
                let mut candidate = self.next_id.unwrap_or(NEW_ENTRY_ID.to_usize());
                let id = loop {
                    let Some(next) = candidate.checked_sub(1) else {
                        return;
                    };
                    candidate = next;
                    self.next_id = Some(next);
                    let id = ProjectEntryId::from_usize(candidate);
                    if !id_is_real(id) {
                        break id;
                    }
                };
                self.ids.insert(parent, id);
                id
            };
            if files.iter().any(|file| Some(file.id) == reveal) {
                self.expanded.insert(parent);
            }
            let Some(mut row) = files.first().cloned() else {
                continue;
            };
            row.entry.id = id;
            row.entry.kind = EntryKind::Dir;
            // A NUL component cannot collide with a directory on disk. The real
            // paths of the files remain untouched, including during file actions.
            let Some(name) = RelPath::from_unix_str("\0tests").log_err() else {
                continue;
            };
            row.entry.path = path.join(name).into();
            row.entry.canonical_path = None;
            row.git_summary = GitSummary::default();
            for file in &files {
                self.members.insert(file.id, id);
                row.git_summary += file.git_summary;
            }
            insertions.push((index, path.components().count(), row));
            self.groups.insert(
                id,
                TestGroup {
                    parent,
                    path,
                    files,
                },
            );
        }
        insertions.sort_by_key(|(index, depth, _)| (*index, cmp::Reverse(*depth)));
        let mut insertions = insertions.into_iter().peekable();
        let mut grouped = Vec::with_capacity(entries.len() + self.groups.len());
        for (index, entry) in std::mem::take(entries).into_iter().enumerate() {
            if !self.members.contains_key(&entry.id) {
                grouped.push(entry);
            }
            while insertions.peek().is_some_and(|(end, _, _)| *end == index) {
                if let Some((_, _, row)) = insertions.next() {
                    let Some(group) = self.groups.get(&row.id) else {
                        continue;
                    };
                    grouped.push(row);
                    if self.expanded.contains(&group.parent) {
                        grouped.extend(group.files.iter().cloned());
                    }
                }
            }
        }
        *entries = grouped;
    }
}
