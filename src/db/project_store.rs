use anyhow::{Result, bail};
use rusqlite::OptionalExtension;

use super::store::{ProjectDirectory, SessionPath, Store};
use crate::db::search::{RepoFilter, TimeRange};
use crate::project_scope::{ProjectScope, ScopeResolution, SelectorForm, classify_selector};
use crate::repo_identity::{RepoIdentity, normalize_remote_url, normalize_slug};

impl Store {
    pub(crate) fn session_paths_for_source(&self, source: &str) -> Result<Vec<SessionPath>> {
        let mut stmt = self.conn.prepare(
            "SELECT source_id, directory, source_file_path, repo_remote, repo_slug, repo_name
             FROM sessions
             WHERE source = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![source], |row| {
            Ok(SessionPath {
                source_id: row.get(0)?,
                directory: row.get(1)?,
                source_file_path: row.get(2)?,
                repo_remote: row.get(3)?,
                repo_slug: row.get(4)?,
                repo_name: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn update_session_repo_identity(
        &self,
        source: &str,
        source_id: &str,
        identity: &RepoIdentity,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions
             SET repo_remote = ?1, repo_slug = ?2, repo_name = ?3
             WHERE source = ?4 AND source_id = ?5",
            rusqlite::params![
                identity.remote.as_str(),
                identity.slug.as_str(),
                identity.name.as_str(),
                source,
                source_id,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn resolve_repo_filter(&self, value: &str) -> Result<RepoFilter> {
        let value = value.trim();
        if value.is_empty() {
            bail!("repo filter cannot be empty");
        }

        if let Some(identity) = normalize_remote_url(value) {
            return Ok(RepoFilter::Remote(identity.remote));
        }
        if let Some(slug) = normalize_slug(value) {
            return Ok(RepoFilter::Slug(slug));
        }
        self.repo_filter_for_indexed_name(value)
    }

    /// Sessions imported from JSONL may carry `repo_name` without `repo_slug`,
    /// so a name indexed without any slug still resolves to a name filter; only
    /// a name matching nothing is an error, instead of silently matching
    /// nothing.
    fn repo_filter_for_indexed_name(&self, value: &str) -> Result<RepoFilter> {
        let mut stmt =
            self.conn.prepare("SELECT DISTINCT repo_slug FROM sessions WHERE repo_name = ?1")?;
        let rows =
            stmt.query_map(rusqlite::params![value], |row| row.get::<_, Option<String>>(0))?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        if rows.is_empty() {
            bail!("no indexed project matches '{value}'; pass a path, owner/repo, or a remote URL");
        }

        let mut slugs: Vec<String> =
            rows.into_iter().flatten().filter(|slug| !slug.is_empty()).collect();
        slugs.sort();
        if slugs.len() > 1 {
            bail!("repo name '{value}' is ambiguous: {}", slugs.join(", "));
        }
        Ok(slugs
            .into_iter()
            .next()
            .map(RepoFilter::Slug)
            .unwrap_or_else(|| RepoFilter::Name(value.to_string())))
    }

    /// Resolves what a user-facing command should operate on. Without an
    /// explicit selector the scope is derived from the current directory, so
    /// running inside a checkout means that project. `repo_filter` is the
    /// deprecated `--repo` flag, kept until extensions have migrated.
    pub(crate) fn resolve_scope(
        &self,
        project_filter: Option<&str>,
        repo_filter: Option<&str>,
    ) -> Result<ScopeResolution> {
        let project = non_empty(project_filter);
        let repo = non_empty(repo_filter);

        if let Some(repo) = repo {
            if project.is_some() {
                bail!("--repo cannot be combined with --project; pass the identity to --project");
            }
            eprintln!("warning: --repo is deprecated; pass the identity to --project instead");
            let scope = ProjectScope::Repository {
                filter: self.resolve_repo_filter(repo)?,
                local_root: None,
            };
            return Ok(ScopeResolution { scope, inferred: false });
        }

        match project {
            Some(project) => Ok(ScopeResolution {
                scope: self.resolve_project_selector(project)?,
                inferred: false,
            }),
            None => {
                Ok(ScopeResolution { scope: crate::project_scope::auto_scope(), inferred: true })
            }
        }
    }

    pub(crate) fn resolve_project_selector(&self, value: &str) -> Result<ProjectScope> {
        let form = classify_selector(value);
        let directory = ProjectScope::Directory(value.trim().to_string());

        // A slug or bare name can also be an indexed relative directory; the
        // directory boundary wins so those sessions stay reachable.
        if matches!(form, SelectorForm::Slug(_) | SelectorForm::IndexedName)
            && self.directory_is_indexed(value)?
        {
            return Ok(directory);
        }

        Ok(match form {
            SelectorForm::Global => ProjectScope::Global,
            SelectorForm::Directory => directory,
            SelectorForm::Repository(identity) => ProjectScope::Repository {
                filter: RepoFilter::Remote(identity.remote),
                local_root: None,
            },
            SelectorForm::Slug(slug) => {
                ProjectScope::Repository { filter: RepoFilter::Slug(slug), local_root: None }
            }
            SelectorForm::IndexedName => ProjectScope::Repository {
                filter: self.repo_filter_for_indexed_name(value.trim())?,
                local_root: None,
            },
        })
    }

    fn directory_is_indexed(&self, value: &str) -> Result<bool> {
        let value = value.trim();
        let found = self
            .conn
            .query_row(
                &format!(
                    "SELECT 1 FROM sessions WHERE directory = ?1 OR {} LIMIT 1",
                    directory_child_sql("directory", 2)
                ),
                rusqlite::params![directory_root(value), escaped_directory_root(value)],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    pub(crate) fn list_project_directories(&self) -> Result<Vec<ProjectDirectory>> {
        let mut stmt = self.conn.prepare(
            "SELECT directory, COUNT(*) AS sessions, MAX(COALESCE(updated_at, started_at)) AS last_seen
             FROM sessions
             WHERE directory IS NOT NULL AND directory != ''
             GROUP BY directory
             ORDER BY last_seen DESC, sessions DESC, directory ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(ProjectDirectory {
                directory: row.get(0)?,
                sessions: row.get::<_, i64>(1)? as u64,
                last_seen: row.get(2)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

pub(crate) fn apply_scope_filters(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    sources: Option<&[String]>,
    time_range: TimeRange,
    scope: &ProjectScope,
) {
    if let Some(sources) = sources
        && !sources.is_empty()
    {
        let placeholders: Vec<String> =
            (0..sources.len()).map(|offset| format!("?{}", *param_idx + offset)).collect();
        sql.push_str(&format!(" AND s.source IN ({})", placeholders.join(", ")));
        for source in sources {
            params.push(Box::new(source.clone()));
        }
        *param_idx += sources.len();
    }

    if let Some(min_ts) = time_range.millis_ago() {
        sql.push_str(&format!(" AND s.started_at >= ?{}", *param_idx));
        params.push(Box::new(min_ts));
        *param_idx += 1;
    }

    apply_project_scope(sql, params, param_idx, scope);
}

/// The only place a `ProjectScope` becomes SQL, so every entry point agrees on
/// what a scope means.
pub(crate) fn apply_project_scope(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    scope: &ProjectScope,
) {
    match scope {
        ProjectScope::Global => {}
        ProjectScope::Directory(directory) => {
            push_directory_predicate(sql, params, param_idx, directory);
        }
        ProjectScope::Repository { filter, local_root } => {
            let (column, value) = filter.column_and_value();
            let Some(local_root) = local_root else {
                sql.push_str(&format!(" AND s.{column} = ?{}", *param_idx));
                params.push(Box::new(value.to_string()));
                *param_idx += 1;
                return;
            };
            // Sessions indexed before repo identity backfill have no repo
            // columns, so an auto-derived scope also accepts the checkout it
            // came from; otherwise they would silently disappear.
            sql.push_str(&format!(
                " AND (s.{column} = ?{} OR s.directory = ?{} OR {})",
                *param_idx,
                *param_idx + 1,
                directory_child_sql("s.directory", *param_idx + 2)
            ));
            params.push(Box::new(value.to_string()));
            params.push(Box::new(directory_root(local_root).to_string()));
            params.push(Box::new(escaped_directory_root(local_root)));
            *param_idx += 3;
        }
    }
}

fn push_directory_predicate(
    sql: &mut String,
    params: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
    param_idx: &mut usize,
    directory: &str,
) {
    sql.push_str(&format!(
        " AND (s.directory = ?{} OR {})",
        *param_idx,
        directory_child_sql("s.directory", *param_idx + 1)
    ));
    params.push(Box::new(directory_root(directory).to_string()));
    params.push(Box::new(escaped_directory_root(directory)));
    *param_idx += 2;
}

/// A trailing separator must not change what a directory boundary means, so
/// both the SQL predicate and `ProjectScope::matches` compare against this form.
fn directory_root(dir: &str) -> &str {
    dir.strip_suffix(['/', '\\']).unwrap_or(dir)
}

/// One pattern per separator, so Windows paths are not silently excluded. The
/// parameter carries only the escaped root; the separator and wildcard are
/// appended in SQL.
fn directory_child_sql(column: &str, param_idx: usize) -> String {
    format!(
        "({column} LIKE ?{param_idx} || '/%' ESCAPE '\\' \
         OR {column} LIKE ?{param_idx} || '\\\\%' ESCAPE '\\')"
    )
}

/// `_` and `%` are LIKE wildcards and common in directory names, so an
/// unescaped root would pull in unrelated sibling directories that
/// `ProjectScope::matches` rejects.
fn escaped_directory_root(dir: &str) -> String {
    directory_root(dir).replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}
