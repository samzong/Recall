use std::collections::HashSet;

use anyhow::Result;

use crate::db::store::{SessionPath, Store};
use crate::project_scope::{ProjectScope, SessionScopeFields};
use crate::repo_identity::RepoIdentityCache;

pub(super) fn delete_excluded_sessions_for_source(
    store: &Store,
    source_id: &str,
    matcher: &globset::GlobSet,
    scope: &ProjectScope,
    target_source_id: Option<&str>,
    deleted: &mut HashSet<String>,
) -> Result<u32> {
    let mut count = 0;
    for path in store.session_paths_for_source(source_id)? {
        if target_source_id.is_some_and(|target| target != path.source_id) {
            continue;
        }
        if !scope.matches(SessionScopeFields {
            directory: path.directory.as_deref(),
            repo_remote: path.repo_remote.as_deref(),
            repo_slug: path.repo_slug.as_deref(),
            repo_name: path.repo_name.as_deref(),
        }) {
            continue;
        }
        if paths_match_excluded(
            path.directory.as_deref(),
            path.source_file_path.as_deref(),
            matcher,
        ) {
            let source_id_to_delete = path.source_id;
            store.delete_session_data(source_id, &source_id_to_delete)?;
            if deleted.insert(source_id_to_delete) {
                count += 1;
            }
        }
    }
    Ok(count)
}

pub(super) fn path_matches_scope(
    scope: &ProjectScope,
    path: &SessionPath,
    cache: &mut RepoIdentityCache,
) -> Result<bool> {
    if matches!(scope, ProjectScope::Global) {
        return Ok(true);
    }
    let repo = if path.repo_remote.is_some() && path.repo_slug.is_some() && path.repo_name.is_some()
    {
        None
    } else {
        cache.resolve(path.directory.as_deref())?
    };
    Ok(scope.matches(SessionScopeFields {
        directory: path.directory.as_deref(),
        repo_remote: path
            .repo_remote
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.remote.as_str())),
        repo_slug: path
            .repo_slug
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.slug.as_str())),
        repo_name: path
            .repo_name
            .as_deref()
            .or_else(|| repo.as_ref().map(|repo| repo.name.as_str())),
    }))
}

pub(super) fn paths_match_excluded(
    directory: Option<&str>,
    source_file_path: Option<&str>,
    matcher: &globset::GlobSet,
) -> bool {
    directory.is_some_and(|path| matcher.is_match(path))
        || source_file_path.is_some_and(|path| path_or_ancestor_matches(path, matcher))
}

fn path_or_ancestor_matches(path: &str, matcher: &globset::GlobSet) -> bool {
    let path = std::path::Path::new(path);
    path.ancestors().any(|candidate| matcher.is_match(candidate))
}
