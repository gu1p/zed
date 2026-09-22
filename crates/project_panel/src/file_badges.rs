use anyhow::{Context as _, Result, bail};
use collections::{BTreeMap, HashMap, HashSet};
use db::kvp::KeyValueStore;
use fs::{Fs, MTime};
use futures::{StreamExt, channel::mpsc};
use globset::{Glob, GlobMatcher};
use gpui::{App, Context, Entity, EventEmitter};
use project::{Project, WorktreeId};
use regex::Regex;
use serde::{Deserialize, Serialize};
use settings::{FileBadgeRule, Settings, SettingsStore};
use sha2::{Digest, Sha256};
use std::{io::Read as _, path::PathBuf, sync::Arc, time::Duration};
use ui::{Tooltip, prelude::*};
use util::{ResultExt, rel_path::RelPath};
use worktree::{PathChange, Snapshot, UpdatedEntriesSet};

use crate::project_panel_settings::ProjectPanelSettings;

const CACHE_NAMESPACE: &str = "project_panel_file_badges";
const CACHE_VERSION: u32 = 1;
const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Badge {
    Emoji(String),
    Icon(IconName),
}

impl Badge {
    fn from_rule(rule: &FileBadgeRule) -> Result<Self> {
        match (&rule.emoji, &rule.icon) {
            (Some(emoji), None) if !emoji.trim().is_empty() => Ok(Self::Emoji(emoji.clone())),
            (None, Some(icon)) => Ok(Self::Icon(icon.parse().context("Unknown built-in icon")?)),
            _ => bail!("Specify exactly one nonempty emoji or built-in icon"),
        }
    }

    pub fn render(&self, index: usize) -> impl IntoElement {
        h_flex()
            .id(("file-badge", index))
            .debug_selector(|| format!("file-badge-{self:?}"))
            .flex_none()
            .min_w(IconSize::Small.rems())
            .justify_center()
            .ml_1()
            .child(match self {
                Self::Emoji(emoji) => Label::new(emoji.clone()).single_line().into_any_element(),
                Self::Icon(icon) => Icon::new(*icon).size(IconSize::Small).into_any_element(),
            })
            .tooltip(Tooltip::text("Matched a file badge rule"))
    }
}

type Badges = HashMap<(WorktreeId, Arc<RelPath>), Vec<Badge>>;

#[derive(Clone, PartialEq, Eq)]
struct RuleScope {
    worktree_id: Option<WorktreeId>,
    path: Arc<RelPath>,
    rules: Vec<FileBadgeRule>,
}

impl RuleScope {
    fn all(cx: &App) -> Vec<Self> {
        let mut scopes = vec![Self {
            worktree_id: None,
            path: RelPath::empty_arc(),
            rules: ProjectPanelSettings::get_global(cx).file_badges.clone(),
        }];
        scopes.extend(
            cx.global::<SettingsStore>()
                .get_all_locals::<ProjectPanelSettings>()
                .into_iter()
                .map(|(worktree_id, path, settings)| Self {
                    worktree_id: Some(worktree_id),
                    path,
                    rules: settings.file_badges.clone(),
                }),
        );
        scopes
    }
}

struct WorktreeInput {
    snapshot: Snapshot,
    scan_complete: bool,
}

struct Request {
    generation: usize,
    worktrees: Vec<WorktreeInput>,
    scopes: Vec<RuleScope>,
    invalidated: Vec<(WorktreeId, Arc<RelPath>)>,
}

pub(super) struct FileBadges {
    badges: Badges,
    scopes: Vec<RuleScope>,
    generation: usize,
    sender: mpsc::UnboundedSender<Request>,
    reported_errors: HashSet<String>,
}

pub(super) struct BadgeError(pub String);
impl EventEmitter<BadgeError> for FileBadges {}

impl FileBadges {
    pub fn new(project: &Entity<Project>, cx: &mut Context<Self>) -> Self {
        let (sender, mut receiver) = mpsc::unbounded::<Request>();
        let fs = project.read(cx).fs().clone();
        cx.spawn(async move |this, cx| {
            let mut worker = Worker::default();
            while let Some(mut request) = receiver.next().await {
                cx.background_executor().timer(DEBOUNCE).await;
                while let Ok(next) = receiver.try_recv() {
                    let mut invalidated = request.invalidated;
                    invalidated.extend(next.invalidated.iter().cloned());
                    request = Request {
                        invalidated,
                        ..next
                    };
                }
                let database = cx.update(|cx| KeyValueStore::global(cx));
                let generation = request.generation;
                let fs = fs.clone();
                let (next_worker, output) = cx
                    .background_spawn(async move {
                        let output = worker.process(request, fs, &database).await;
                        (worker, output)
                    })
                    .await;
                worker = next_worker;
                // The worker outlives the view to finish persistence, but must not
                // publish matches computed before a newer file/settings event.
                let publish = this
                    .read_with(cx, |this, _| this.generation == generation)
                    .unwrap_or(false);
                if publish {
                    this.update(cx, |this, cx| {
                        if this.badges != output.badges {
                            this.badges = output.badges;
                            cx.notify();
                        }
                        for error in output.errors {
                            if this.reported_errors.insert(error.clone()) {
                                log::error!("File badges: {error}");
                                cx.emit(BadgeError(error));
                            }
                        }
                    })
                    .log_err();
                }
                let database = cx.update(|cx| KeyValueStore::global(cx));
                if publish || !this.is_upgradable() {
                    let (next_worker, errors) = cx
                        .background_spawn(async move {
                            let errors = worker.persist(&database).await;
                            (worker, errors)
                        })
                        .await;
                    worker = next_worker;
                    for error in errors {
                        log::error!("File badge cache: {error}");
                        if this.is_upgradable() {
                            this.update(cx, |this, cx| {
                                if this.reported_errors.insert(error.clone()) {
                                    cx.emit(BadgeError(error));
                                }
                            })
                            .log_err();
                        }
                    }
                }
            }
        })
        .detach();

        cx.subscribe(project, |this, project, event, cx| match event {
            project::Event::WorktreeUpdatedEntries(worktree_id, changes) => {
                this.refresh(&project, Some((*worktree_id, changes)), cx);
            }
            project::Event::WorktreeAdded(_)
            | project::Event::WorktreeRemoved(_)
            | project::Event::WorktreePathsChanged { .. } => {
                this.refresh(&project, None, cx);
            }
            _ => {}
        })
        .detach();
        let project = project.downgrade();
        cx.observe_global::<SettingsStore>(move |this, cx| {
            let scopes = RuleScope::all(cx);
            if scopes != this.scopes {
                this.scopes = scopes;
                this.reported_errors.clear();
                if let Some(project) = project.upgrade() {
                    this.refresh(&project, None, cx);
                }
            }
        })
        .detach();

        Self {
            badges: HashMap::default(),
            scopes: RuleScope::all(cx),
            generation: 0,
            sender,
            reported_errors: HashSet::default(),
        }
    }

    pub fn refresh(
        &mut self,
        project: &Entity<Project>,
        changes: Option<(WorktreeId, &UpdatedEntriesSet)>,
        cx: &mut Context<Self>,
    ) {
        if self.scopes.iter().all(|scope| scope.rules.is_empty()) && self.generation == 0 {
            return;
        }
        self.generation += 1;
        let worktrees: Vec<_> = project
            .read(cx)
            .visible_worktrees(cx)
            .filter_map(|tree| {
                let tree = tree.read(cx);
                tree.as_local()?;
                Some(WorktreeInput {
                    snapshot: tree.snapshot(),
                    scan_complete: tree.scan_id() == tree.completed_scan_id(),
                })
            })
            .collect();
        let mut invalidated = Vec::new();
        if let Some((worktree_id, changes)) = changes {
            let tree = project.read(cx).worktree_for_id(worktree_id, cx);
            for (path, entry_id, change) in changes.iter() {
                let is_file = tree
                    .as_ref()
                    .and_then(|tree| tree.read(cx).entry_for_id(*entry_id))
                    .is_some_and(|entry| entry.is_file());
                if *change != PathChange::Loaded && (is_file || *change == PathChange::Removed) {
                    invalidated.push((worktree_id, path.clone()));
                }
            }
        }
        let scopes = self
            .scopes
            .iter()
            .filter(|scope| {
                scope
                    .worktree_id
                    .is_none_or(|id| worktrees.iter().any(|tree| tree.snapshot.id() == id))
            })
            .cloned()
            .collect();
        self.sender
            .unbounded_send(Request {
                generation: self.generation,
                worktrees,
                scopes,
                invalidated,
            })
            .log_err();
    }

    pub fn for_path(&self, worktree_id: WorktreeId, path: &Arc<RelPath>) -> &[Badge] {
        self.badges
            .get(&(worktree_id, path.clone()))
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    pub fn is_empty(&self) -> bool {
        self.badges.is_empty()
    }
}

struct Matcher {
    fingerprint: String,
    filename: GlobMatcher,
    content: Regex,
}

impl Matcher {
    fn compile(filename: &str, content_regex: &str) -> Result<Self> {
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(filename, content_regex))?)
        );
        Ok(Self {
            fingerprint,
            filename: Glob::new(filename)?.compile_matcher(),
            content: Regex::new(content_regex)?,
        })
    }
}

struct CompiledRule {
    matcher: Arc<Matcher>,
    badge: Badge,
    level: u32,
    position: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct FileVersion {
    inode: u64,
    mtime: Option<MTime>,
    size: u64,
}

impl FileVersion {
    fn for_entry(entry: &project::Entry) -> Self {
        Self {
            inode: entry.inode,
            mtime: entry.mtime,
            size: entry.size,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct CachedFile {
    version: FileVersion,
    matches: BTreeMap<String, bool>,
}

#[derive(Serialize, Deserialize)]
struct PersistedCache {
    version: u32,
    files: BTreeMap<String, CachedFile>,
}

impl Default for PersistedCache {
    fn default() -> Self {
        Self {
            version: CACHE_VERSION,
            files: BTreeMap::default(),
        }
    }
}

struct WorktreeCache {
    root: Arc<std::path::Path>,
    key: String,
    data: PersistedCache,
    dirty: bool,
}

#[derive(Default)]
struct Worker {
    caches: HashMap<WorktreeId, WorktreeCache>,
    matchers: HashMap<(String, String), std::result::Result<Arc<Matcher>, String>>,
    #[cfg(test)]
    reads: usize,
    #[cfg(test)]
    evaluations: usize,
}

#[derive(Default)]
struct Output {
    badges: Badges,
    errors: Vec<String>,
}

impl Worker {
    async fn process(
        &mut self,
        request: Request,
        fs: Arc<dyn Fs>,
        database: &KeyValueStore,
    ) -> Output {
        let mut output = Output::default();
        let mut scopes = Vec::new();
        for scope in request.scopes {
            let mut rules = Vec::new();
            for (position, rule) in scope.rules.iter().enumerate() {
                let result = (|| -> Result<CompiledRule> {
                    let badge = Badge::from_rule(rule)?;
                    let matcher = self
                        .matchers
                        .entry((rule.filename.clone(), rule.content_regex.clone()))
                        .or_insert_with(|| {
                            Matcher::compile(&rule.filename, &rule.content_regex)
                                .map(Arc::new)
                                .map_err(|error| error.to_string())
                        })
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error}"))?
                        .clone();
                    Ok(CompiledRule {
                        matcher,
                        badge,
                        level: rule.icon_level,
                        position,
                    })
                })();
                match result {
                    Ok(rule) => rules.push(rule),
                    Err(error) => output.errors.push(format!(
                        "Rule {} ({:?}): {error}",
                        position + 1,
                        rule.filename
                    )),
                }
            }
            scopes.push((scope.worktree_id, scope.path, rules));
        }
        self.caches.retain(|id, _| {
            request
                .worktrees
                .iter()
                .any(|tree| tree.snapshot.id() == *id)
        });
        for tree in request.worktrees {
            let worktree_id = tree.snapshot.id();
            if scopes.iter().all(|(id, _, rules)| {
                (id.is_some() && *id != Some(worktree_id)) || rules.is_empty()
            }) {
                continue;
            }
            let root = tree.snapshot.abs_path().clone();
            if self
                .caches
                .get(&worktree_id)
                .is_none_or(|cache| cache.root != root)
            {
                let canonical_root = match fs.canonicalize(&root).await {
                    Ok(path) => path,
                    Err(error) => {
                        output.errors.push(format!("{}: {error}", root.display()));
                        continue;
                    }
                };
                let key = canonical_root.to_string_lossy().into_owned();
                let data = match database.scoped(CACHE_NAMESPACE).read(&key) {
                    Ok(Some(value)) => match serde_json::from_str::<PersistedCache>(&value) {
                        Ok(data) if data.version == CACHE_VERSION => data,
                        Ok(_) => PersistedCache::default(),
                        Err(error) => {
                            output.errors.push(format!(
                                "Rebuilding file badge cache for {}: {error}",
                                root.display()
                            ));
                            PersistedCache::default()
                        }
                    },
                    Ok(None) => PersistedCache::default(),
                    Err(error) => {
                        output
                            .errors
                            .push(format!("Could not read file badge cache: {error}"));
                        PersistedCache::default()
                    }
                };
                self.caches.insert(
                    worktree_id,
                    WorktreeCache {
                        root: root.clone(),
                        key,
                        data,
                        dirty: false,
                    },
                );
            }
            let Some(cache) = self.caches.get_mut(&worktree_id) else {
                continue;
            };
            let previous_length = cache.data.files.len();
            let invalidated: HashSet<_> = request
                .invalidated
                .iter()
                .filter(|(id, _)| *id == worktree_id)
                .map(|(_, path)| path.as_ref())
                .collect();
            cache.data.files.retain(|path, _| {
                RelPath::from_unix_str(path).is_ok_and(|path| {
                    !path
                        .ancestors()
                        .any(|ancestor| invalidated.contains(ancestor))
                        && (!tree.scan_complete || tree.snapshot.entry_for_path(path).is_some())
                })
            });
            cache.dirty |= previous_length != cache.data.files.len();

            let mut pending = Vec::new();
            let mut candidates = Vec::new();
            for entry in tree.snapshot.files(false, 0) {
                if entry.is_fifo || entry.size > MAX_FILE_SIZE {
                    continue;
                }
                let Some(filename) = entry.path.file_name() else {
                    continue;
                };
                let Some((_, _, rules)) = scopes
                    .iter()
                    .filter(|(id, path, _)| {
                        id.is_none() || (*id == Some(worktree_id) && entry.path.starts_with(path))
                    })
                    .max_by_key(|(id, path, _)| (id.is_some(), path.components().count()))
                else {
                    continue;
                };
                let rules: Vec<_> = rules
                    .iter()
                    .filter(|rule| rule.matcher.filename.is_match(filename))
                    .collect();
                if rules.is_empty() {
                    continue;
                }
                let version = FileVersion::for_entry(entry);
                let cached = cache
                    .data
                    .files
                    .get(entry.path.as_unix_str())
                    .filter(|cached| cached.version == version);
                let mut missing = Vec::new();
                for rule in &rules {
                    if cached.is_none_or(|cached| {
                        !cached.matches.contains_key(&rule.matcher.fingerprint)
                    }) && !missing.iter().any(|matcher: &Arc<Matcher>| {
                        matcher.fingerprint == rule.matcher.fingerprint
                    }) {
                        missing.push(rule.matcher.clone());
                    }
                }
                if !missing.is_empty() {
                    #[cfg(test)]
                    {
                        self.reads += 1;
                        self.evaluations += missing.len();
                    }
                    pending.push((entry.path.clone(), version, missing));
                }
                candidates.push((entry.path.clone(), rules));
            }

            let reads =
                futures::stream::iter(pending.into_iter().map(|(path, version, matchers)| {
                    let fs = fs.clone();
                    let absolute_path = root.join(path.as_std_path());
                    async move {
                        let result = read_matches(fs, absolute_path, &version, matchers).await;
                        (path, version, result)
                    }
                }))
                .buffer_unordered(4);
            futures::pin_mut!(reads);
            while let Some((path, version, result)) = reads.next().await {
                match result {
                    Ok(matches) => {
                        let cached = cache
                            .data
                            .files
                            .entry(path.as_unix_str().to_owned())
                            .or_insert_with(|| CachedFile {
                                version: version.clone(),
                                matches: BTreeMap::default(),
                            });
                        if cached.version != version {
                            cached.version = version;
                            cached.matches.clear();
                        }
                        cached.matches.extend(matches);
                        cache.dirty = true;
                    }
                    Err(error) => {
                        cache.dirty |= cache.data.files.remove(path.as_unix_str()).is_some();
                        output.errors.push(format!(
                            "{}: {error}",
                            root.join(path.as_std_path()).display()
                        ));
                    }
                }
            }
            let mut contributions = Vec::new();
            for (path, rules) in candidates {
                let Some(cached) = cache.data.files.get(path.as_unix_str()) else {
                    continue;
                };
                for rule in rules {
                    if cached.matches.get(&rule.matcher.fingerprint) != Some(&true) {
                        continue;
                    }
                    if let Some(target) = path.ancestors().nth(rule.level as usize) {
                        contributions.push((
                            rule.position,
                            path.clone(),
                            Arc::<RelPath>::from(target),
                            rule.badge.clone(),
                        ));
                    }
                }
            }
            contributions.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
            for (_, _, target, badge) in contributions {
                let badges = output.badges.entry((worktree_id, target)).or_default();
                if !badges.contains(&badge) {
                    badges.push(badge);
                }
            }
        }
        output
    }

    async fn persist(&mut self, database: &KeyValueStore) -> Vec<String> {
        let mut errors = Vec::new();
        for cache in self.caches.values_mut().filter(|cache| cache.dirty) {
            let result = async {
                database
                    .scoped(CACHE_NAMESPACE)
                    .write(cache.key.clone(), serde_json::to_string(&cache.data)?)
                    .await
            }
            .await;
            match result {
                Ok(()) => cache.dirty = false,
                Err(error) => errors.push(format!("Could not save file badge cache: {error}")),
            }
        }
        errors
    }
}

async fn read_matches(
    fs: Arc<dyn Fs>,
    path: PathBuf,
    expected: &FileVersion,
    matchers: Vec<Arc<Matcher>>,
) -> Result<BTreeMap<String, bool>> {
    let before = fs.metadata(&path).await?.context("File was removed")?;
    if before.is_fifo || before.is_dir || before.len > MAX_FILE_SIZE {
        return Ok(matchers
            .into_iter()
            .map(|matcher| (matcher.fingerprint.clone(), false))
            .collect());
    }
    let mut bytes = Vec::new();
    fs.open_sync(&path)
        .await?
        .take(MAX_FILE_SIZE + 1)
        .read_to_end(&mut bytes)?;
    let after = fs.metadata(&path).await?.context("File was removed")?;
    if before.inode != after.inode
        || before.mtime != after.mtime
        || before.len != after.len
        || expected.inode != after.inode
        || expected.mtime != Some(after.mtime)
        || expected.size != after.len
    {
        bail!("File changed while checking badges; waiting for its next update");
    }
    let text = std::str::from_utf8(&bytes)
        .ok()
        .filter(|text| bytes.len() as u64 <= MAX_FILE_SIZE && !text.contains('\0'));
    Ok(matchers
        .into_iter()
        .map(|matcher| {
            let matches = text.is_some_and(|text| matcher.content.is_match(text));
            (matcher.fingerprint.clone(), matches)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;
    use util::rel_path::rel_path;

    fn rule(level: u32) -> FileBadgeRule {
        FileBadgeRule {
            filename: "{BUILD,*.bazel}".into(),
            content_regex: r"\brust_binary\s*\(".into(),
            emoji: Some("🦀".into()),
            icon: None,
            icon_level: level,
        }
    }

    async fn fixture(
        tree: serde_json::Value,
        cx: &mut TestAppContext,
    ) -> (Arc<FakeFs>, Entity<Project>) {
        crate::project_panel_tests::init_test(cx);
        cx.update(|cx| cx.set_global(db::AppDatabase::test_new()));
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree("/root", tree).await;
        let project = Project::test(fs.clone(), ["/root".as_ref()], cx).await;
        cx.run_until_parked();
        (fs, project)
    }

    fn request(
        project: &Entity<Project>,
        rules: Vec<FileBadgeRule>,
        cx: &TestAppContext,
    ) -> Request {
        cx.read(|cx| Request {
            generation: 0,
            worktrees: project
                .read(cx)
                .visible_worktrees(cx)
                .map(|tree| {
                    let tree = tree.read(cx);
                    WorktreeInput {
                        snapshot: tree.snapshot(),
                        scan_complete: tree.scan_id() == tree.completed_scan_id(),
                    }
                })
                .collect(),
            scopes: vec![RuleScope {
                worktree_id: None,
                path: RelPath::empty_arc(),
                rules,
            }],
            invalidated: vec![],
        })
    }

    fn at(output: &Output, path: &str) -> Vec<Badge> {
        output
            .badges
            .iter()
            .find(|((_, candidate), _)| candidate.as_unix_str() == path)
            .map(|(_, badges)| badges.clone())
            .unwrap_or_default()
    }

    #[test]
    fn test_file_badges_rule_validation() {
        assert_eq!(
            Badge::from_rule(&rule(0)).expect("valid emoji"),
            Badge::Emoji("🦀".into())
        );
        let mut icon_rule = rule(0);
        icon_rule.emoji = None;
        icon_rule.icon = Some("binary".into());
        assert_eq!(
            Badge::from_rule(&icon_rule).expect("valid icon"),
            Badge::Icon(IconName::Binary)
        );
        icon_rule.emoji = Some("🦀".into());
        assert!(Badge::from_rule(&icon_rule).is_err());
        icon_rule.emoji = None;
        icon_rule.icon = Some("nonexistent_icon".into());
        assert!(Badge::from_rule(&icon_rule).is_err());
        icon_rule.icon = None;
        assert!(Badge::from_rule(&icon_rule).is_err());
        icon_rule.emoji = Some(" ".into());
        assert!(Badge::from_rule(&icon_rule).is_err());
        assert!(Matcher::compile("[", "valid").is_err());
        assert!(Matcher::compile("*.bazel", "(").is_err());
        let matcher =
            Matcher::compile("{BUILD,*.bazel}", r"(?im)^rust_binary\s*\(").expect("valid matchers");
        assert!(matcher.filename.is_match("BUILD"));
        assert!(!matcher.filename.is_match("build"));
        assert!(matcher.content.is_match("load(...)\nRUST_BINARY(\n)"));
        let parsed: FileBadgeRule = serde_json::from_value(
            json!({"filename":"*.bazel", "content_regex":"rust_binary", "emoji":"🦀"}),
        )
        .expect("valid rule");
        assert_eq!(parsed.icon_level, 0);
    }

    #[gpui::test]
    async fn test_file_badges_cache_reuse_and_persistence(cx: &mut TestAppContext) {
        let (fs, project) = fixture(
            json!({
                "BUILD.bazel": "rust_binary(name='tool')",
                "negative.bazel": "rust_library(name='lib')",
                "ignored.rs": "rust_binary(name='ignored')",
                "nested": { "BUILD": "rust_binary(name='nested')" }
            }),
            cx,
        )
        .await;
        let database = cx.read(KeyValueStore::global);
        let mut worker = Worker::default();
        let rules = vec![rule(0), rule(1), rule(1), rule(50)];
        let output = worker
            .process(request(&project, rules.clone(), cx), fs.clone(), &database)
            .await;
        assert!(output.errors.is_empty(), "{:?}", output.errors);
        assert_eq!(at(&output, "BUILD.bazel"), [Badge::Emoji("🦀".into())]);
        assert_eq!(at(&output, ""), [Badge::Emoji("🦀".into())]);
        assert_eq!(at(&output, "nested"), [Badge::Emoji("🦀".into())]);
        assert!(at(&output, "negative.bazel").is_empty());
        assert!(at(&output, "ignored.rs").is_empty());
        assert_eq!((worker.reads, worker.evaluations), (3, 3));
        let again = worker
            .process(request(&project, rules.clone(), cx), fs.clone(), &database)
            .await;
        assert_eq!(again.badges, output.badges);
        assert_eq!((worker.reads, worker.evaluations), (3, 3));
        assert!(worker.persist(&database).await.is_empty());

        let mut reopened = Worker::default();
        let restored = reopened
            .process(request(&project, rules, cx), fs.clone(), &database)
            .await;
        assert_eq!(restored.badges, output.badges);
        assert_eq!((reopened.reads, reopened.evaluations), (0, 0));
        let mut changed = rule(2);
        changed.emoji = None;
        changed.icon = Some("code".into());
        let changed_output = reopened
            .process(
                request(&project, vec![changed.clone()], cx),
                fs.clone(),
                &database,
            )
            .await;
        assert_eq!(at(&changed_output, ""), [Badge::Icon(IconName::Code)]);
        assert_eq!((reopened.reads, reopened.evaluations), (0, 0));
        changed.content_regex = "rust_library".into();
        changed.icon_level = 0;
        let changed_output = reopened
            .process(request(&project, vec![changed], cx), fs, &database)
            .await;
        assert_eq!(
            at(&changed_output, "negative.bazel"),
            [Badge::Icon(IconName::Code)]
        );
        assert_eq!((reopened.reads, reopened.evaluations), (3, 3));
    }

    #[gpui::test]
    async fn test_file_badges_changes_and_shared_contributions(cx: &mut TestAppContext) {
        let (fs, project) = fixture(
            json!({"a.bazel":"rust_binary()", "b.bazel":"rust_binary()", "no.bazel":""}),
            cx,
        )
        .await;
        let database = cx.read(KeyValueStore::global);
        let mut worker = Worker::default();
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs.clone(), &database)
            .await;
        assert_eq!(at(&output, ""), [Badge::Emoji("🦀".into())]);
        assert_eq!(worker.reads, 3);
        fs.insert_file("/root/a.bazel", b"rust_library()".to_vec())
            .await;
        cx.run_until_parked();
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs.clone(), &database)
            .await;
        assert_eq!(worker.reads, 4);
        assert_eq!(at(&output, ""), [Badge::Emoji("🦀".into())]);
        fs.rename(
            std::path::Path::new("/root/b.bazel"),
            std::path::Path::new("/root/b.txt"),
            Default::default(),
        )
        .await
        .expect("rename fixture");
        cx.run_until_parked();
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs.clone(), &database)
            .await;
        assert!(output.badges.is_empty());
        assert_eq!(worker.reads, 4);
        fs.insert_file("/root/no.bazel", b"rust_binary()".to_vec())
            .await;
        cx.run_until_parked();
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs.clone(), &database)
            .await;
        assert_eq!(at(&output, ""), [Badge::Emoji("🦀".into())]);
        assert_eq!(worker.reads, 5);
        fs.remove_file(std::path::Path::new("/root/no.bazel"), Default::default())
            .await
            .expect("delete fixture");
        cx.run_until_parked();
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs, &database)
            .await;
        assert!(output.badges.is_empty());
    }

    #[gpui::test]
    async fn test_file_badges_scopes_and_rule_order(cx: &mut TestAppContext) {
        let (fs, project) = fixture(
            json!({"BUILD":"rust_binary()", "nested":{"BUILD":"rust_binary()"}}),
            cx,
        )
        .await;
        let database = cx.read(KeyValueStore::global);
        let mut worker = Worker::default();
        let mut other = rule(1);
        other.emoji = None;
        other.icon = Some("binary".into());
        let mut input = request(&project, vec![other.clone(), rule(1), other.clone()], cx);
        let worktree_id = input
            .worktrees
            .first()
            .expect("fixture worktree")
            .snapshot
            .id();
        input.scopes.push(RuleScope {
            worktree_id: Some(worktree_id),
            path: rel_path("nested").into(),
            rules: vec![],
        });
        let output = worker.process(input, fs.clone(), &database).await;
        assert_eq!(
            at(&output, ""),
            [Badge::Icon(IconName::Binary), Badge::Emoji("🦀".into())]
        );
        assert!(at(&output, "nested").is_empty());
        assert_eq!(worker.reads, 1);
        let mut input = request(&project, vec![rule(1), other], cx);
        input.scopes.push(RuleScope {
            worktree_id: Some(worktree_id),
            path: RelPath::empty_arc(),
            rules: vec![],
        });
        let output = worker.process(input, fs.clone(), &database).await;
        assert!(output.badges.is_empty());
        assert_eq!(worker.reads, 1);
        let output = worker
            .process(request(&project, vec![rule(1)], cx), fs, &database)
            .await;
        assert_eq!(at(&output, "nested"), [Badge::Emoji("🦀".into())]);
        assert_eq!(worker.reads, 2);
    }

    #[gpui::test]
    async fn test_file_badges_invalid_rules_and_cache(cx: &mut TestAppContext) {
        let (fs, project) = fixture(
            json!({"BUILD":"rust_binary()", "binary.bazel":"\u{0}rust_binary()"}),
            cx,
        )
        .await;
        let database = cx.read(KeyValueStore::global);
        database
            .scoped(CACHE_NAMESPACE)
            .write("/root".into(), "broken json".into())
            .await
            .expect("seed broken cache");
        let mut worker = Worker::default();
        let mut invalid = rule(0);
        invalid.content_regex = "(".into();
        let output = worker
            .process(
                request(&project, vec![invalid, rule(0)], cx),
                fs.clone(),
                &database,
            )
            .await;
        assert_eq!(output.errors.len(), 2);
        assert_eq!(at(&output, "BUILD"), [Badge::Emoji("🦀".into())]);
        assert!(at(&output, "binary.bazel").is_empty());
        assert!(worker.persist(&database).await.is_empty());
        let mut reopened = Worker::default();
        let output = reopened
            .process(request(&project, vec![rule(0)], cx), fs, &database)
            .await;
        assert!(output.errors.is_empty());
        assert_eq!(reopened.reads, 0);
    }

    #[gpui::test]
    async fn test_file_badges_limits_and_stale_reads(cx: &mut TestAppContext) {
        let (fs, project) = fixture(json!({"BUILD":"rust_binary()"}), cx).await;
        fs.insert_file("/root/large.bazel", vec![b'a'; MAX_FILE_SIZE as usize + 1])
            .await;
        fs.insert_file("/root/non_utf8.bazel", vec![255]).await;
        cx.run_until_parked();
        let database = cx.read(KeyValueStore::global);
        let mut worker = Worker::default();
        let outdated = request(&project, vec![rule(0)], cx);
        fs.insert_file("/root/BUILD", b"rust_library()".to_vec())
            .await;
        let output = worker.process(outdated, fs.clone(), &database).await;
        assert!(output.badges.is_empty());
        assert_eq!(output.errors.len(), 1);
        assert_eq!(worker.reads, 2, "oversized file is never read");
        cx.run_until_parked();
        let output = worker
            .process(request(&project, vec![rule(0)], cx), fs.clone(), &database)
            .await;
        assert!(output.errors.is_empty());
        assert!(output.badges.is_empty());
        assert_eq!(worker.reads, 3, "stale read was not cached as a nonmatch");
        let mut input = request(&project, vec![rule(0)], cx);
        let worktree_id = input.worktrees.first().expect("worktree").snapshot.id();
        input
            .invalidated
            .push((worktree_id, rel_path("BUILD").into()));
        worker.process(input, fs, &database).await;
        assert_eq!(
            worker.reads, 4,
            "observed changes invalidate unchanged metadata"
        );
    }

    #[gpui::test(iterations = 20)]
    async fn test_file_badges_concurrent_updates(cx: &mut TestAppContext) {
        use gpui::UpdateGlobal;
        let (fs, project) = fixture(json!({"BUILD":"rust_binary()"}), cx).await;
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project_panel.get_or_insert_default().file_badges =
                        Some(vec![rule(0)]);
                });
            })
        });
        let badges = cx.new(|cx| FileBadges::new(&project, cx));
        badges.update(cx, |badges, cx| badges.refresh(&project, None, cx));
        cx.background_executor.timer(DEBOUNCE).await;
        fs.write(std::path::Path::new("/root/BUILD"), b"rust_library()")
            .await
            .expect("save file");
        let mut changed = rule(1);
        changed.emoji = Some("📚".into());
        changed.content_regex = "rust_library".into();
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project_panel.get_or_insert_default().file_badges =
                        Some(vec![changed]);
                });
            })
        });
        cx.condition(&badges, |badges, _| {
            badges
                .badges
                .values()
                .any(|values| values == &[Badge::Emoji("📚".into())])
        })
        .await;
        badges.read_with(cx, |badges, _| {
            assert_eq!(badges.badges.len(), 1);
            assert!(badges.badges.keys().all(|(_, path)| path.is_empty()));
        });
        drop(badges);
        cx.run_until_parked();
        let database = cx.read(KeyValueStore::global);
        let mut reopened = Worker::default();
        let mut changed = rule(1);
        changed.emoji = Some("📚".into());
        changed.content_regex = "rust_library".into();
        let output = reopened
            .process(request(&project, vec![changed], cx), fs, &database)
            .await;
        assert_eq!(at(&output, ""), [Badge::Emoji("📚".into())]);
        assert_eq!(
            reopened.reads, 0,
            "dropping the view still persists completed results"
        );
    }
}
