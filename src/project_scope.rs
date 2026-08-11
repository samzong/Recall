//! Single source of truth for "which sessions does this command mean".
//!
//! Two entry points share one precedence ladder: an explicit `--project`
//! selector, and the automatic scope derived from the current directory.
//! Resolution never yields "unknown" — a directory that has no resolvable repo
//! identity degrades to a directory boundary, and a directory outside Git
//! degrades to `Global`, so callers cannot invent their own interpretation.

use std::path::Path;

use crate::db::search::RepoFilter;
use crate::repo_identity::{
    RepoIdentity, git_toplevel, normalize_remote_url, normalize_slug, origin_identity,
};

const GLOBAL_KEYWORDS: [&str; 2] = ["all", "global"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectScope {
    Global,
    /// A directory and its children.
    Directory(String),
    /// A repository identity, spanning every worktree that shares it.
    /// `local_root` is the checkout the scope was derived from, kept so a
    /// query can still reach sessions whose repo identity is not backfilled.
    Repository {
        filter: RepoFilter,
        local_root: Option<String>,
    },
}

impl ProjectScope {
    /// Stable discriminator for structured output.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            ProjectScope::Global => "global",
            ProjectScope::Directory(_) => "directory",
            ProjectScope::Repository { .. } => "repository",
        }
    }

    pub(crate) fn value(&self) -> Option<&str> {
        match self {
            ProjectScope::Global => None,
            ProjectScope::Directory(directory) => Some(directory),
            ProjectScope::Repository { filter, .. } => Some(filter.column_and_value().1),
        }
    }

    /// The write-side twin of the SQL predicate in `apply_project_scope`; the
    /// two must agree, or sync would persist sessions that queries then hide.
    pub(crate) fn matches(&self, session: SessionScopeFields<'_>) -> bool {
        match self {
            ProjectScope::Global => true,
            ProjectScope::Directory(directory) => session.is_under(directory),
            ProjectScope::Repository { filter, local_root } => {
                let identity_matches = match filter {
                    RepoFilter::Remote(remote) => session.repo_remote == Some(remote.as_str()),
                    RepoFilter::Slug(slug) => session.repo_slug == Some(slug.as_str()),
                    RepoFilter::Name(name) => session.repo_name == Some(name.as_str()),
                };
                identity_matches || local_root.as_deref().is_some_and(|root| session.is_under(root))
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SessionScopeFields<'a> {
    pub(crate) directory: Option<&'a str>,
    pub(crate) repo_remote: Option<&'a str>,
    pub(crate) repo_slug: Option<&'a str>,
    pub(crate) repo_name: Option<&'a str>,
}

impl<'a> SessionScopeFields<'a> {
    pub(crate) fn new(directory: Option<&'a str>, identity: Option<&'a RepoIdentity>) -> Self {
        Self {
            directory,
            repo_remote: identity.map(|identity| identity.remote.as_str()),
            repo_slug: identity.map(|identity| identity.slug.as_str()),
            repo_name: identity.map(|identity| identity.name.as_str()),
        }
    }

    fn is_under(&self, root: &str) -> bool {
        let Some(directory) = self.directory else {
            return false;
        };
        let root = root.strip_suffix(['/', '\\']).unwrap_or(root);
        directory == root
            || directory
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/') || rest.starts_with('\\'))
    }
}

/// Commands report an inferred scope so a narrowed result set is never silent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeResolution {
    pub(crate) scope: ProjectScope,
    pub(crate) inferred: bool,
}

impl ScopeResolution {
    /// Announces an inferred non-global scope on stderr, keeping stdout free
    /// for the requested JSON/JSONL payload.
    pub(crate) fn announce(self) -> ProjectScope {
        if let (true, Some(value)) = (self.inferred, self.scope.value()) {
            eprintln!("scope: {value} (pass --project all for every project)");
        }
        self.scope
    }
}

/// The part of selector precedence that needs no database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SelectorForm {
    Global,
    Directory,
    Repository(RepoIdentity),
    Slug(String),
    /// Not a path, URL, or slug — the store resolves it against indexed repos.
    IndexedName,
}

/// Callers must still probe indexed directories before honouring `Slug` /
/// `IndexedName`, because a session directory can be recorded as a relative
/// path such as `owner/repo`.
pub(crate) fn classify_selector(value: &str) -> SelectorForm {
    let value = value.trim();
    if GLOBAL_KEYWORDS.iter().any(|keyword| value.eq_ignore_ascii_case(keyword)) {
        return SelectorForm::Global;
    }
    if looks_like_path(value) || Path::new(value).is_dir() {
        return SelectorForm::Directory;
    }
    if let Some(identity) = normalize_remote_url(value) {
        return SelectorForm::Repository(identity);
    }
    match normalize_slug(value) {
        Some(slug) => SelectorForm::Slug(slug),
        None => SelectorForm::IndexedName,
    }
}

pub(crate) fn auto_scope_for_dir(dir: &Path) -> ProjectScope {
    let Some(dir) = dir.to_str() else {
        return ProjectScope::Global;
    };
    let Some(toplevel) = git_toplevel(dir) else {
        return ProjectScope::Global;
    };
    match origin_identity(&toplevel) {
        Some(identity) => ProjectScope::Repository {
            filter: RepoFilter::Remote(identity.remote),
            local_root: Some(toplevel),
        },
        None => ProjectScope::Directory(toplevel),
    }
}

pub(crate) fn auto_scope() -> ProjectScope {
    match std::env::current_dir() {
        Ok(dir) => auto_scope_for_dir(&dir),
        Err(_) => ProjectScope::Global,
    }
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with('.') || value.starts_with('~')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn temp_dir(prefix: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git_init(root: &Path, origin: Option<&str>) {
        Command::new("git").arg("init").current_dir(root).output().unwrap();
        if let Some(origin) = origin {
            Command::new("git")
                .args(["remote", "add", "origin", origin])
                .current_dir(root)
                .output()
                .unwrap();
        }
    }

    #[test]
    fn global_keywords_win_over_every_other_form() {
        assert_eq!(classify_selector("all"), SelectorForm::Global);
        assert_eq!(classify_selector("GLOBAL"), SelectorForm::Global);
    }

    #[test]
    fn path_shape_stays_a_directory_boundary() {
        for value in ["/abs/path", "./rel", "../up", "~/home"] {
            assert_eq!(classify_selector(value), SelectorForm::Directory, "{value}");
        }
    }

    #[test]
    fn remote_urls_and_slugs_are_repository_forms() {
        assert_eq!(
            classify_selector("git@github.com:samzong/Recall.git"),
            SelectorForm::Repository(RepoIdentity {
                remote: "github.com/samzong/Recall".to_string(),
                slug: "samzong/Recall".to_string(),
                name: "Recall".to_string(),
            })
        );
        assert_eq!(
            classify_selector("samzong/Recall"),
            SelectorForm::Slug("samzong/Recall".to_string())
        );
        assert_eq!(
            classify_selector("group/subgroup/app"),
            SelectorForm::Slug("group/subgroup/app".to_string())
        );
        assert_eq!(classify_selector("Recall"), SelectorForm::IndexedName);
    }

    #[test]
    fn auto_scope_uses_repo_identity_from_nested_directory() {
        let root = temp_dir("recall-scope-repo");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        git_init(&root, Some("git@github.com:samzong/Recall.git"));

        let scope = auto_scope_for_dir(&nested);

        let root_name = root.file_name().unwrap().to_str().unwrap().to_string();
        match scope {
            ProjectScope::Repository { filter, local_root } => {
                assert_eq!(filter, RepoFilter::Remote("github.com/samzong/Recall".to_string()));
                assert!(local_root.is_some_and(|path| path.ends_with(&root_name)));
            }
            other => panic!("expected repository scope, got {other:?}"),
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn auto_scope_degrades_to_directory_without_resolvable_origin() {
        let root = temp_dir("recall-scope-noorigin");
        git_init(&root, None);

        let scope = auto_scope_for_dir(&root);

        match scope {
            ProjectScope::Directory(directory) => {
                assert!(directory.ends_with(root.file_name().unwrap().to_str().unwrap()));
            }
            other => panic!("expected directory scope, got {other:?}"),
        }

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn auto_scope_is_global_outside_git() {
        let root = temp_dir("recall-scope-nongit");

        assert_eq!(auto_scope_for_dir(&root), ProjectScope::Global);

        std::fs::remove_dir_all(root).unwrap();
    }
}
