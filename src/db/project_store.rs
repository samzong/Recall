use anyhow::{Result, bail};
use rusqlite::OptionalExtension;

use super::store::{ProjectDirectory, SessionPath, Store};
use crate::db::search::{RepoFilter, TimeRange};
use crate::repo_identity::{RepoIdentity, identity_from_slug, normalize_remote_url};

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
        if let Some(identity) = identity_from_slug(value) {
            return Ok(RepoFilter::Slug(identity.slug));
        }

        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT repo_slug
             FROM sessions
             WHERE repo_name = ?1 AND repo_slug IS NOT NULL AND repo_slug != ''
             ORDER BY repo_slug ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![value], |row| row.get::<_, String>(0))?;
        let candidates = rows.collect::<Result<Vec<_>, _>>()?;
        if candidates.len() > 1 {
            bail!("repo name '{value}' is ambiguous: {}", candidates.join(", "));
        }
        Ok(candidates
            .first()
            .cloned()
            .map(RepoFilter::Slug)
            .unwrap_or_else(|| RepoFilter::Name(value.to_string())))
    }

    pub(crate) fn resolve_project_repo_filters(
        &self,
        project_filter: Option<&str>,
        repo_filter: Option<&str>,
    ) -> Result<(Option<String>, Option<RepoFilter>)> {
        let mut directory = None;
        let mut repo = None;

        if let Some(project) = non_empty(project_filter) {
            if self.project_filter_is_directory(project)? {
                directory = Some(project.to_string());
            } else {
                repo = Some(self.resolve_repo_filter(project)?);
            }
        }

        if let Some(value) = non_empty(repo_filter) {
            if repo.is_some() {
                bail!("--project repo identity cannot be combined with --repo");
            }
            repo = Some(self.resolve_repo_filter(value)?);
        }

        Ok((directory, repo))
    }

    fn project_filter_is_directory(&self, value: &str) -> Result<bool> {
        if project_filter_looks_like_path(value) || std::path::Path::new(value).is_dir() {
            return Ok(true);
        }
        let found = self
            .conn
            .query_row(
                "SELECT 1
                 FROM sessions
                 WHERE directory = ?1 OR directory LIKE ?2
                 LIMIT 1",
                rusqlite::params![value, directory_child_pattern(value)],
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
    directory: Option<&str>,
    repo: Option<&RepoFilter>,
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

    if let Some(dir) = directory {
        sql.push_str(&format!(
            " AND (s.directory = ?{} OR s.directory LIKE ?{})",
            *param_idx,
            *param_idx + 1
        ));
        params.push(Box::new(dir.to_string()));
        params.push(Box::new(directory_child_pattern(dir)));
        *param_idx += 2;
    }

    if let Some(repo) = repo {
        let (column, value) = repo.column_and_value();
        sql.push_str(&format!(" AND s.{column} = ?{}", *param_idx));
        params.push(Box::new(value.to_string()));
        *param_idx += 1;
    }
}

fn directory_child_pattern(dir: &str) -> String {
    if dir.ends_with('/') { format!("{dir}%") } else { format!("{dir}/%") }
}

fn project_filter_looks_like_path(value: &str) -> bool {
    value.starts_with('/') || value.starts_with('.') || value.starts_with('~')
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}
