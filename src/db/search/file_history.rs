use super::{SearchEngine, SessionEventHit, qualified_session_columns};
use crate::db::store::{SESSION_COLUMNS, session_from_row};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryTarget {
    pub(crate) requested_path: String,
    pub(crate) repo_remote: Option<String>,
    pub(crate) repo_root: Option<String>,
    pub(crate) path: Option<String>,
    pub(crate) absolute_path: Option<String>,
}

impl SearchEngine<'_> {
    pub(crate) fn resolve_file_history_target(
        &self,
        project: &str,
        path: &str,
    ) -> anyhow::Result<FileHistoryTarget> {
        use crate::project_scope::{SelectorForm, classify_selector};
        use crate::repo_identity::{
            RepoIdentityCache, git_toplevel, normalize_remote_url, origin_identity,
        };
        let project = project.trim();
        anyhow::ensure!(!project.is_empty(), "target_project is required");
        anyhow::ensure!(
            !path.is_empty()
                && !path.starts_with('~')
                && path.len() <= 4096
                && !path.contains('\0'),
            "invalid target path"
        );
        let (remote, root) = match classify_selector(project) {
            SelectorForm::Global => {
                anyhow::bail!("target_project must identify one repository or directory")
            }
            SelectorForm::Directory => {
                let directory = std::fs::canonicalize(project).map_err(|_| {
                    anyhow::anyhow!(
                        "target_project directory is unavailable; pass an indexed remote identity"
                    )
                })?;
                let directory = directory
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target directory"))?;
                let root = git_toplevel(directory).unwrap_or_else(|| directory.to_string());
                let remote = origin_identity(&root).map(|identity| identity.remote);
                (remote, Some(root))
            }
            SelectorForm::Repository(identity) => (Some(identity.remote), None),
            form => {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_remote') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') IS NOT NULL")?;
                let mut matches = Vec::new();
                for remote in stmt.query_map([], |row| row.get::<_, String>(0))? {
                    let remote = remote?;
                    let Some(identity) = normalize_remote_url(&remote) else {
                        continue;
                    };
                    let matches_selector = match &form {
                        SelectorForm::Slug(slug) => identity.slug == *slug,
                        SelectorForm::IndexedName => identity.name == project,
                        _ => false,
                    };
                    if matches_selector {
                        matches.push(remote);
                    }
                }
                anyhow::ensure!(
                    matches.len() == 1,
                    "target_project has no unique indexed target identity; pass an absolute directory or remote URL"
                );
                (matches.pop(), None)
            }
        };
        let mut absolute_path = None;
        let relative = if std::path::Path::new(path).is_absolute() || root.is_some() {
            let resolved = RepoIdentityCache::default().resolve_file(path, root.as_deref());
            if let Some(file) = resolved.filter(|file| {
                remote.as_deref().is_some_and(|remote| file.repo_remote.as_deref() == Some(remote))
                    || root.as_deref().is_some_and(|root| {
                        file.repo_root.as_deref() == Some(root)
                            || (file.repo_root.is_none()
                                && std::path::Path::new(&file.absolute_path).starts_with(root))
                    })
            }) {
                absolute_path = Some(file.absolute_path.clone());
                file.repo_relative_path
                    .or_else(|| {
                        root.as_deref()
                            .and_then(|root| {
                                std::path::Path::new(&file.absolute_path).strip_prefix(root).ok()
                            })
                            .and_then(std::path::Path::to_str)
                            .map(str::to_string)
                    })
                    .ok_or_else(|| anyhow::anyhow!("target path is outside target_project"))?
            } else if std::path::Path::new(path).is_absolute() {
                let mut stmt = self.conn.prepare("SELECT DISTINCT json_extract(evidence_json, '$.target.repo_relative_path') FROM event_files WHERE json_extract(evidence_json, '$.target.repo_remote') = ?1 AND json_extract(evidence_json, '$.target.absolute_path') = ?2")?;
                let paths = stmt
                    .query_map(rusqlite::params![remote, path], |row| {
                        row.get::<_, Option<String>>(0)
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                if paths.len() != 1 || paths[0].is_none() {
                    return Ok(FileHistoryTarget {
                        requested_path: path.to_string(),
                        repo_remote: remote,
                        repo_root: root,
                        path: None,
                        absolute_path: Some(path.to_string()),
                    });
                }
                absolute_path = Some(path.to_string());
                paths
                    .into_iter()
                    .next()
                    .flatten()
                    .ok_or_else(|| anyhow::anyhow!("target path identity is unresolved"))?
            } else {
                anyhow::bail!("target path is outside target_project or cannot be resolved");
            }
        } else {
            path.to_string()
        };
        let mut normalized = std::path::PathBuf::new();
        for component in std::path::Path::new(&relative).components() {
            match component {
                std::path::Component::Normal(part) => normalized.push(part),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir if normalized.pop() => {}
                _ => anyhow::bail!("target path must stay inside target_project"),
            }
        }
        anyhow::ensure!(!normalized.as_os_str().is_empty(), "target path must identify a file");
        Ok(FileHistoryTarget {
            requested_path: path.to_string(),
            repo_remote: remote,
            repo_root: root,
            path: Some(
                normalized
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid target path"))?
                    .to_string(),
            ),
            absolute_path,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryQuery {
    pub(crate) target: FileHistoryTarget,
    pub(crate) sources: Option<Vec<String>>,
    pub(crate) kind: Option<String>,
    pub(crate) include_command_candidates: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryMatch {
    pub(crate) evidence: crate::types::FileEvidence,
    pub(crate) match_basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryEvidence {
    pub(crate) event_ref: String,
    pub(crate) source_session_id: String,
    pub(crate) actor: String,
    pub(crate) status: Option<String>,
    pub(crate) message_seq: Option<u32>,
    pub(crate) tool_call_id: Option<String>,
    pub(crate) source_event_id: Option<String>,
    pub(crate) parser_version: u32,
    pub(crate) visibility: Option<String>,
    pub(crate) is_import: bool,
    pub(crate) command_evidence_status: Option<String>,
    pub(crate) files: Vec<FileHistoryMatch>,
    pub(crate) file_associations: u64,
    pub(crate) matching_file_associations: u64,
    pub(crate) files_truncated: bool,
    pub(crate) target_truncated: bool,
}

#[derive(Debug)]
pub(crate) struct FileHistoryHit {
    pub(crate) hit: SessionEventHit,
    pub(crate) evidence: FileHistoryEvidence,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistorySourceCoverage {
    pub(crate) source: String,
    pub(crate) registered: bool,
    pub(crate) supports_event_backfill: bool,
    pub(crate) indexed_sessions: u64,
    pub(crate) imported_sessions: u64,
    pub(crate) sessions_without_parser_state: u64,
    pub(crate) observed_parser_versions: std::collections::BTreeMap<u32, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct FileHistoryCoverage {
    pub(crate) scope: String,
    pub(crate) native_source_scan: String,
    pub(crate) parser_currency: String,
    pub(crate) import_coverage: String,
    pub(crate) sources: Vec<FileHistorySourceCoverage>,
}

#[derive(Debug)]
pub(crate) struct FileHistoryPage {
    pub(crate) events: Vec<FileHistoryHit>,
    pub(crate) next_cursor: Option<String>,
    pub(crate) coverage: Option<FileHistoryCoverage>,
}

#[derive(Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct FileHistorySnapshot {
    index_id: String,
    events: u64,
    last_event_id: i64,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct FileHistoryCursor {
    version: u8,
    query: FileHistoryQuery,
    snapshot: FileHistorySnapshot,
    last_unknown_time: bool,
    last_timestamp: i64,
    last_event_id: i64,
}

fn file_history_predicate() -> &'static str {
    "((json_extract(f.evidence_json, '$.target.repo_remote') = ?1
            AND json_extract(f.evidence_json, '$.target.repo_relative_path') = ?3)
       OR (json_extract(f.evidence_json, '$.target.repo_remote') IS NULL
            AND json_extract(f.evidence_json, '$.target.repo_root') = ?2
            AND json_extract(f.evidence_json, '$.target.repo_relative_path') = ?3)
       OR (json_extract(f.evidence_json, '$.target.absolute_path') = ?4)
       OR (f.path = ?4 AND substr(f.path, 1, 1) = '/')
       OR (f.path = ?6 AND substr(?6, 1, 1) = '/'))
      AND (?5 OR json_extract(f.evidence_json, '$.kind') != 'command')"
}

fn file_history_parameters(query: &FileHistoryQuery) -> Vec<Box<dyn rusqlite::types::ToSql>> {
    vec![
        Box::new(query.target.repo_remote.clone()),
        Box::new(query.target.repo_root.clone()),
        Box::new(query.target.path.clone()),
        Box::new(query.target.absolute_path.clone()),
        Box::new(query.include_command_candidates),
        Box::new(query.target.requested_path.clone()),
    ]
}

impl SearchEngine<'_> {
    pub(crate) fn file_history_page(
        &self,
        query: &FileHistoryQuery,
        limit: usize,
        cursor: Option<&str>,
    ) -> anyhow::Result<FileHistoryPage> {
        anyhow::ensure!((1..=50).contains(&limit), "file history limit must be between 1 and 50");
        let tx = self.conn.unchecked_transaction()?;
        let mut parameters = file_history_parameters(query);
        let mut filter = format!(
            "e.id IN (SELECT f.event_id FROM event_files f WHERE {})",
            file_history_predicate()
        );
        if let Some(kind) = &query.kind {
            parameters.push(Box::new(kind.clone()));
            filter.push_str(&format!(" AND e.kind = ?{}", parameters.len()));
        }
        if let Some(sources) = &query.sources {
            if sources.is_empty() {
                filter.push_str(" AND 0");
            } else {
                let placeholders = sources
                    .iter()
                    .map(|source| {
                        parameters.push(Box::new(source.clone()));
                        format!("?{}", parameters.len())
                    })
                    .collect::<Vec<_>>();
                filter.push_str(&format!(" AND e.source IN ({})", placeholders.join(",")));
            }
        }
        let mut remaining_bytes = 64 * 1024 * 1024;
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            parameters.iter().map(|value| value.as_ref()).collect();
        let snapshot = tx.query_row(
            &format!("SELECT (SELECT index_id FROM file_history_state WHERE id = 1), COUNT(*), COALESCE(MAX(e.id), 0) FROM session_events e NOT INDEXED WHERE {filter}"),
            refs.as_slice(),
            |row| Ok(FileHistorySnapshot { index_id: row.get(0)?, events: row.get(1)?, last_event_id: row.get(2)? }),
        )?;
        let continuation = if let Some(cursor) = cursor {
            anyhow::ensure!(cursor.len() <= 16384, "invalid file history cursor");
            let cursor: FileHistoryCursor = serde_json::from_str(cursor)
                .map_err(|_| anyhow::anyhow!("invalid file history cursor"))?;
            anyhow::ensure!(
                cursor.version == 1 && cursor.query == *query && cursor.last_event_id > 0,
                "file history cursor does not match this query"
            );
            anyhow::ensure!(
                cursor.snapshot == snapshot,
                "file history cursor is stale; query the target again"
            );
            Some(cursor)
        } else {
            None
        };
        if let Some(cursor) = continuation {
            let index = parameters.len() + 1;
            parameters.push(Box::new(cursor.last_unknown_time));
            parameters.push(Box::new(cursor.last_timestamp));
            parameters.push(Box::new(cursor.last_event_id));
            filter.push_str(&format!(" AND ((e.timestamp IS NULL) > ?{index} OR ((e.timestamp IS NULL) = ?{index} AND (COALESCE(e.timestamp, 0) < ?{} OR (COALESCE(e.timestamp, 0) = ?{} AND e.id < ?{}))))", index + 1, index + 1, index + 2));
        }
        parameters.push(Box::new((limit + 1) as i64));
        let sql = format!(
            "SELECT {}, e.kind, e.name, substr(e.target, 1, 201), e.event_seq, substr(e.summary, 1, 201), e.timestamp, e.id, e.actor, e.status, e.message_seq, e.tool_call_id, e.source_event_id, e.parser_version, e.visibility, e.command_evidence_status, length(e.target) > 200, e.is_meta FROM session_events e NOT INDEXED JOIN sessions s ON s.id = e.session_id WHERE {filter} ORDER BY (e.timestamp IS NULL), COALESCE(e.timestamp, 0) DESC, e.id DESC LIMIT ?{}",
            qualified_session_columns(),
            parameters.len()
        );
        let refs: Vec<&dyn rusqlite::types::ToSql> =
            parameters.iter().map(|value| value.as_ref()).collect();
        let page_lengths = SESSION_COLUMNS
            .split(", ")
            .chain(["kind", "actor", "status", "tool_call_id", "source_event_id", "name"])
            .map(|field| format!("COALESCE(length(CAST({field} AS BLOB)), 0)"))
            .collect::<Vec<_>>()
            .join(" + ");
        let page_bytes: usize = tx.query_row(
            &format!("SELECT COALESCE(SUM({page_lengths}), 0) FROM ({sql})"),
            refs.as_slice(),
            |row| row.get(0),
        )?;
        anyhow::ensure!(page_bytes <= remaining_bytes, "evidence_budget_exceeded");
        remaining_bytes -= page_bytes;
        let mut stmt = tx.prepare(&sql)?;
        let mut rows = stmt
            .query_map(refs.as_slice(), |row| {
                let session = session_from_row(row, self.conn)?;
                Ok((
                    row.get::<_, i64>(23)?,
                    FileHistoryHit {
                        evidence: FileHistoryEvidence {
                            event_ref: String::new(),
                            source_session_id: session.source_id.clone(),
                            actor: row.get(24)?,
                            status: row.get(25)?,
                            message_seq: row.get(26)?,
                            tool_call_id: row.get(27)?,
                            source_event_id: row.get(28)?,
                            parser_version: row.get(29)?,
                            visibility: row.get(30)?,
                            command_evidence_status: row.get(31)?,
                            target_truncated: row.get::<_, Option<bool>>(32)?.unwrap_or(false),
                            is_import: session.is_import,
                            files: Vec::new(),
                            file_associations: 0,
                            matching_file_associations: 0,
                            files_truncated: false,
                        },
                        hit: SessionEventHit {
                            session,
                            kind: row.get(17)?,
                            name: row.get(18)?,
                            target: row.get(19)?,
                            event_seq: row.get(20)?,
                            summary: row.get(21)?,
                            timestamp: row.get(22)?,
                            visibility: row
                                .get::<_, Option<String>>(30)?
                                .as_deref()
                                .and_then(crate::types::EvidenceVisibility::parse),
                            is_meta: row.get(33)?,
                        },
                    },
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = if has_more {
            let (id, last) =
                rows.last().ok_or_else(|| anyhow::anyhow!("missing file history continuation"))?;
            Some(serde_json::to_string(&FileHistoryCursor {
                version: 1,
                query: query.clone(),
                snapshot,
                last_unknown_time: last.hit.timestamp.is_none(),
                last_timestamp: last.hit.timestamp.unwrap_or(0),
                last_event_id: *id,
            })?)
        } else {
            None
        };
        for (id, hit) in &mut rows {
            hit.evidence.event_ref =
                serde_json::to_string(&crate::db::event_store::event_reference(&tx, *id)?)?;
            hit.evidence.file_associations = tx.query_row(
                "SELECT COUNT(*) FROM event_files WHERE event_id = ?1",
                [*id],
                |row| row.get(0),
            )?;
            let mut parameters = file_history_parameters(query);
            parameters.push(Box::new(*id));
            let refs: Vec<&dyn rusqlite::types::ToSql> =
                parameters.iter().map(|value| value.as_ref()).collect();
            hit.evidence.matching_file_associations = tx.query_row(
                &format!(
                    "SELECT COUNT(*) FROM event_files f WHERE event_id = ?7 AND {}",
                    file_history_predicate()
                ),
                refs.as_slice(),
                |row| row.get(0),
            )?;
            let files_sql = format!(
                "SELECT evidence_json FROM event_files f WHERE event_id = ?7 AND {} ORDER BY position LIMIT 32",
                file_history_predicate()
            );
            let bytes: usize = tx.query_row(&format!("SELECT COALESCE(SUM(length(CAST(evidence_json AS BLOB))), 0) FROM ({files_sql})"), refs.as_slice(), |row| row.get(0))?;
            anyhow::ensure!(bytes <= remaining_bytes, "evidence_budget_exceeded");
            remaining_bytes -= bytes;
            let mut stmt = tx.prepare(&files_sql)?;
            for evidence in stmt.query_map(refs.as_slice(), |row| row.get::<_, String>(0))? {
                let evidence: crate::types::FileEvidence = serde_json::from_str(&evidence?)?;
                let basis = if evidence.target.as_ref().is_some_and(|target| {
                    query.target.path.is_some()
                        && target.repo_remote.is_some()
                        && target.repo_remote == query.target.repo_remote
                        && target.repo_relative_path.as_deref() == query.target.path.as_deref()
                }) {
                    "repository_identity"
                } else if evidence.target.as_ref().is_some_and(|target| {
                    query.target.path.is_some()
                        && target.repo_root.is_some()
                        && target.repo_root == query.target.repo_root
                        && target.repo_relative_path.as_deref() == query.target.path.as_deref()
                }) {
                    "repository_root"
                } else if evidence.target.as_ref().is_some_and(|target| {
                    Some(target.absolute_path.as_str()) == query.target.absolute_path.as_deref()
                }) {
                    "absolute_path"
                } else {
                    "native_absolute_path"
                };
                hit.evidence
                    .files
                    .push(FileHistoryMatch { evidence, match_basis: basis.to_string() });
            }
            hit.evidence.files_truncated =
                hit.evidence.matching_file_associations > hit.evidence.files.len() as u64;
        }
        let coverage = cursor
            .is_none()
            .then(|| SearchEngine::new(&tx).file_history_coverage(query.sources.as_deref()))
            .transpose()?;
        Ok(FileHistoryPage {
            events: rows.into_iter().map(|(_, hit)| hit).collect(),
            next_cursor,
            coverage,
        })
    }

    fn file_history_coverage(
        &self,
        sources: Option<&[String]>,
    ) -> anyhow::Result<FileHistoryCoverage> {
        let selected = |source: &str| {
            sources.is_none_or(|sources| sources.iter().any(|selected| selected == source))
        };
        let mut coverage = std::collections::BTreeMap::new();
        for adapter in crate::adapters::all_adapters() {
            if selected(adapter.id()) {
                coverage.insert(
                    adapter.id().to_string(),
                    FileHistorySourceCoverage {
                        source: adapter.id().to_string(),
                        registered: true,
                        supports_event_backfill: crate::adapters::source_supports_event_backfill(
                            adapter.id(),
                        ),
                        ..Default::default()
                    },
                );
            }
        }
        let mut stmt = self.conn.prepare("SELECT s.source, COUNT(*), SUM(s.is_import), SUM(p.session_id IS NULL) FROM sessions s LEFT JOIN event_session_state p ON p.session_id = s.id GROUP BY s.source")?;
        for row in stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u64>(1)?,
                row.get::<_, u64>(2)?,
                row.get::<_, u64>(3)?,
            ))
        })? {
            let (source, indexed_sessions, imported_sessions, sessions_without_parser_state) = row?;
            if !selected(&source) {
                continue;
            }
            let entry = coverage
                .entry(source.clone())
                .or_insert_with(|| FileHistorySourceCoverage { source, ..Default::default() });
            entry.indexed_sessions = indexed_sessions;
            entry.imported_sessions = imported_sessions;
            entry.sessions_without_parser_state = sessions_without_parser_state;
        }
        let mut stmt = self.conn.prepare("SELECT source, parser_version, COUNT(*) FROM event_session_state GROUP BY source, parser_version")?;
        for row in stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u32>(1)?, row.get::<_, u64>(2)?))
        })? {
            let (source, version, count) = row?;
            if let Some(entry) = coverage.get_mut(&source) {
                entry.observed_parser_versions.insert(version, count);
            }
        }
        Ok(FileHistoryCoverage {
            scope: "selected_sources_all_indexed_sessions".to_string(),
            native_source_scan: "not_performed".to_string(),
            parser_currency: "recorded_versions_only".to_string(),
            import_coverage: "unknown".to_string(),
            sources: coverage.into_values().collect(),
        })
    }
}
