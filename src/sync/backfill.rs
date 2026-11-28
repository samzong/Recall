use std::collections::HashSet;

use super::{
    AdapterRun, BackfillPlan, ExistingState, SyncJob, SyncStats, path_matches_scope,
    paths_match_excluded,
};
use crate::adapters;
use crate::db::store::{ParserStateMeta, SessionTopologyWrite};
use crate::sync_progress::format_elapsed;
use anyhow::Result;

impl SyncJob {
    pub(super) fn sync_event_backfill_adapter(
        &mut self,
        adapter: &dyn adapters::SourceAdapter,
    ) -> Result<()> {
        let source = adapter.id();
        if self
            .options
            .sources
            .as_ref()
            .is_some_and(|sources| !sources.iter().any(|id| id == source))
        {
            return Ok(());
        }
        if !self.config.is_source_enabled(source) {
            self.event_backfill.as_mut().unwrap().disabled.push(source.to_string());
            return Ok(());
        }
        if !adapters::source_supports_event_backfill(source) {
            self.event_backfill.as_mut().unwrap().unsupported_sources.push(source.to_string());
            return Ok(());
        }
        let context = self.load_adapter_sync_context(source)?;
        let mut paths = Vec::new();
        for path in context.session_paths() {
            if path_matches_scope(&self.options.scope, path, &mut self.repo_cache)?
                && !self.path_excluder.as_ref().is_some_and(|matcher| {
                    paths_match_excluded(
                        path.directory.as_deref(),
                        path.source_file_path.as_deref(),
                        matcher,
                    )
                })
            {
                paths.push(path.clone());
            }
        }
        let scan = self.scan_sessions(adapter, adapter.label(), &context)?;
        let Some(scan) = scan else {
            self.event_backfill.as_mut().unwrap().unavailable.push(source.to_string());
            self.event_backfill.as_mut().unwrap().unknown_original += paths.len() as u32;
            return Ok(());
        };
        let observed = scan
            .scan
            .sessions
            .iter()
            .map(|raw| raw.source_id.as_str())
            .chain(scan.scan.observations.iter().map(|observation| observation.source_id.as_str()))
            .collect::<HashSet<_>>();
        for path in &paths {
            if observed.contains(path.source_id.as_str()) {
                continue;
            }
            let missing_record = matches!(&scan.reconcile, Some(adapters::ReconcilePlan::CompleteLiveSet(live)) if !live.contains(&path.source_id));
            let missing_path = path
                .source_file_path
                .as_deref()
                .is_some_and(|path| matches!(std::path::Path::new(path).try_exists(), Ok(false)));
            if missing_record || missing_path {
                self.event_backfill.as_mut().unwrap().missing_original += 1;
            } else {
                self.event_backfill.as_mut().unwrap().unknown_original += 1;
            }
        }
        let report = self.event_backfill.as_mut().unwrap();
        report.scanned += scan.scan.stats.parsed;
        report.unstable += scan.scan.stats.unstable_sessions;
        if matches!(
            scan.reconcile,
            Some(
                adapters::ReconcilePlan::PartialInventory(_)
                    | adapters::ReconcilePlan::UnavailableInventory(_)
            )
        ) {
            report.unavailable.push(source.to_string());
        }
        let mut existing = self.prepare_existing_state(source, context)?;
        for raw in scan.scan.sessions {
            if let Err(error) =
                self.process_raw_session(source, raw, &mut existing, &mut HashSet::new())
            {
                self.event_backfill.as_mut().unwrap().failed_writes += 1;
                if self.options.emit {
                    eprintln!("Event backfill failed for {source}: {error}");
                }
            }
        }
        Ok(())
    }

    pub(super) fn apply_backfill(
        &mut self,
        source_id: &str,
        raw_source_id: &str,
        raw: &mut adapters::RawSession,
        plan: BackfillPlan,
        was_imported: bool,
        existing: &mut ExistingState,
    ) -> Result<()> {
        if (plan.usage && raw.usage_events.iter().any(|event| event.message_seq.is_some()))
            || (plan.events && raw.events.iter().any(|event| event.message_seq.is_some()))
        {
            let stored = existing
                .meta
                .get(raw_source_id)
                .map(|session| self.store.get_messages(&session.id))
                .transpose()?;
            let same_messages = stored.as_ref().is_some_and(|stored| {
                stored.len() == raw.messages.len()
                    && stored.iter().zip(&raw.messages).enumerate().all(
                        |(index, (stored, parsed))| {
                            stored.seq == index as u32
                                && stored.role == parsed.role
                                && stored.content == parsed.content
                                && stored.timestamp == parsed.timestamp
                        },
                    )
            });
            if !same_messages {
                for event in &mut raw.events {
                    event.message_seq = None;
                }
                for event in &mut raw.usage_events {
                    event.message_seq = None;
                }
            }
        }
        let mut reprocessed = false;
        if plan.usage
            && let Some(parser_version) = raw.usage_parser_version
            && self.store.persist_usage_events_for_existing_session_with_exclusions(
                source_id,
                raw_source_id,
                &raw.usage_events,
                &raw.excluded_codex_usage,
                parser_version,
                raw.updated_at,
            )?
        {
            existing.usage_meta.insert(
                raw_source_id.to_string(),
                ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
            );
            reprocessed = true;
        }
        if plan.events
            && let Some(parser_version) = raw.event_parser_version
            && self.store.persist_session_events_for_existing_session(
                source_id,
                raw_source_id,
                &raw.events,
                parser_version,
                raw.updated_at,
            )?
        {
            existing.event_meta.insert(
                raw_source_id.to_string(),
                ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
            );
            reprocessed = true;
        }
        if plan.metadata
            && let Some(parser_version) = raw.metadata_parser_version
        {
            let topology = SessionTopologyWrite {
                thread_role: raw.thread_role,
                parents: &raw.parent_links,
                parser_version: Some(parser_version),
            };
            if self.store.persist_topology_for_existing_session(
                source_id,
                raw_source_id,
                &topology,
            )? {
                existing.metadata_meta.insert(
                    raw_source_id.to_string(),
                    ParserStateMeta { parser_version, source_updated_at: raw.updated_at },
                );
                reprocessed = true;
            }
        }
        if self.event_backfill.is_none()
            && (raw.custom_title.is_some()
                || raw.summary.is_some()
                || raw.duration_minutes.is_some())
        {
            self.store.update_session_fields(
                source_id,
                raw_source_id,
                raw.custom_title.as_deref(),
                raw.summary.as_deref(),
                raw.duration_minutes,
                None,
            )?;
        }
        if was_imported {
            self.store.clear_import_marker(source_id, raw_source_id)?;
        }
        if reprocessed {
            self.stats.reprocessed_sessions += 1;
        }
        Ok(())
    }

    /// The evidence a scan-level optimisation has to move: how many candidates
    /// each adapter considered, how many it rejected without reading the
    /// transcript, and how many transcripts it actually parsed.
    pub(super) fn report_adapter_breakdown(&self) {
        let runs = &self.adapter_runs;
        if runs.is_empty() {
            return;
        }

        println!();
        println!(
            "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
            "Source", "candidates", "pre-parse rej", "parsed", "scoped", "touched", "ms"
        );
        for run in runs {
            println!(
                "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
                run.label,
                run.scan.candidates,
                run.scan.rejected_before_parse,
                run.scan.parsed,
                run.out_of_scope,
                run.touched,
                run.elapsed_ms
            );
        }
        let total = |f: fn(&AdapterRun) -> u32| runs.iter().map(f).sum::<u32>();
        println!(
            "{:<6} {:>10} {:>14} {:>8} {:>8} {:>9} {:>8}",
            "total",
            total(|run| run.scan.candidates),
            total(|run| run.scan.rejected_before_parse),
            total(|run| run.scan.parsed),
            total(|run| run.out_of_scope),
            total(|run| run.touched),
            runs.iter().map(|run| run.elapsed_ms).sum::<u128>()
        );
    }

    pub(super) fn report_progress(&self) -> Result<()> {
        if let Some(report) = &self.event_backfill {
            if self.options.emit {
                println!(
                    "Event backfill {}: scanned={}, new={}, updated={}, unchanged={}, excluded={}, out_of_scope={}, no_events={}, unsupported_sessions={}, missing_original={}, unknown_original={}, unstable={}, failed_writes={}, parse_failures=unknown",
                    if report.dry_run { "preview" } else { "finished" },
                    report.scanned,
                    self.stats.new_sessions,
                    self.stats.reprocessed_sessions,
                    self.stats.skipped,
                    self.stats.excluded_out,
                    self.stats.out_of_scope,
                    report.no_events,
                    report.unsupported_sessions,
                    report.missing_original,
                    report.unknown_original,
                    report.unstable,
                    report.failed_writes
                );
                for (label, sources) in [
                    ("disabled", &report.disabled),
                    ("unsupported", &report.unsupported_sources),
                    ("unavailable", &report.unavailable),
                ] {
                    if !sources.is_empty() {
                        println!("{label}: {}", sources.join(", "));
                    }
                }
            }
            anyhow::ensure!(
                report.failed_writes == 0,
                "event backfill could not persist {} sessions",
                report.failed_writes
            );
            return Ok(());
        }

        let SyncStats {
            new_sessions,
            updated_sessions,
            reprocessed_sessions,
            total_messages,
            skipped,
            filtered_out,
            excluded_out,
            out_of_scope,
        } = self.stats;
        let touched = self.stats.touched();

        if self.options.verbose {
            println!();
            if self.options.force {
                print!(
                    "Force sync: {new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} reprocessed, {total_messages} messages"
                );
            } else {
                print!(
                    "Sync: {new_sessions} new, {updated_sessions} updated, {skipped} unchanged, {total_messages} messages"
                );
            }
            for (count, label) in [
                (filtered_out, "outside configured time scope"),
                (out_of_scope, "outside project scope"),
                (excluded_out, "excluded by excluded_paths"),
            ] {
                if count > 0 {
                    print!(", {count} {label}");
                }
            }
            println!();
            self.report_adapter_breakdown();
            println!(
                "Settings: sources [{}], time scope [{}]",
                self.labels
                    .iter()
                    .filter(|(id, _)| self.config.is_source_enabled(id))
                    .map(|(_, label)| label.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.config.sync_window.label()
            );
            let progress = self.store.semantic_progress()?;
            if progress.total_sessions > 0 {
                println!(
                    "Semantic queue: {}/{} done, {} pending, {} failed",
                    progress.done_sessions,
                    progress.total_sessions,
                    progress.pending_sessions + progress.processing_sessions,
                    progress.failed_sessions
                );
            }
        } else if self.options.emit {
            let elapsed = format_elapsed(self.started.elapsed().as_millis());
            if self.options.force {
                println!("Reprocessed {touched} sessions, {total_messages} messages in {elapsed}");
            } else if touched == 0 {
                println!("Up to date ({elapsed}).");
            } else if reprocessed_sessions > 0 {
                println!(
                    "{new_sessions} new, {updated_sessions} updated, {reprocessed_sessions} backfilled, {total_messages} messages in {elapsed}"
                );
            } else {
                println!(
                    "{new_sessions} new, {updated_sessions} updated, {total_messages} messages in {elapsed}"
                );
            }
        }

        Ok(())
    }
}
