use std::collections::{HashMap, HashSet};

use super::*;
use crate::session::related::RelatedSessionGraph;
use crate::share::{BundledSessionInput, MAX_BUNDLE_SESSIONS};

#[allow(clippy::too_many_arguments)]
pub(super) fn cmd_session_share(
    ids: Vec<String>,
    source: Option<&str>,
    source_id: Option<&str>,
    include_related: bool,
    exclude_ids: Vec<String>,
    dry_run: bool,
    output: Option<&Path>,
    open: bool,
    copy_url: bool,
    tldr_file: Option<&Path>,
    format: SessionActionFormat,
) -> Result<()> {
    let store = Store::open()?;
    let config = AppConfig::load()?;
    let sources = adapters::source_labels();
    let inputs = resolve_share_bundle_inputs(
        &store,
        &sources,
        ids,
        source,
        source_id,
        include_related,
        exclude_ids,
    )?;
    let primary = inputs[0].session.clone();
    let tldr_markdown = tldr_file.and_then(read_tldr_file);
    let render_options = crate::share::ShareRenderOptions { tldr_markdown };

    let (preview, url) = if dry_run {
        let preview = if let Some(out_path) = output {
            crate::share::dry_run_bundle_to_output(&config, &inputs, &render_options, out_path)?
        } else {
            crate::share::preview_bundle_with_options(&config, &inputs, &render_options)?
        };
        let url = preview.url.clone();
        (preview, url)
    } else {
        anyhow::ensure!(output.is_none(), "--output requires --dry-run");
        let preview = crate::share::preview_bundle_with_options(&config, &inputs, &render_options)?;
        eprintln!("Sharing session {}...", primary.id);
        let result = crate::share::publish_bundle_with_options(&config, &inputs, &render_options)?;
        (preview, result.url)
    };

    if copy_url {
        anyhow::ensure!(
            !url.is_empty(),
            "no share URL available; configure sharing with `recall share init`"
        );
        crate::utils::copy_to_clipboard(&url)?;
    }
    if open {
        anyhow::ensure!(
            !url.is_empty(),
            "no share URL available; configure sharing with `recall share init`"
        );
        crate::utils::open_url_in_default_browser(&url)?;
    }

    let json_payload = share_result_json(&primary, &inputs, &preview, &url, dry_run)?;
    match format {
        SessionActionFormat::Text => {
            if dry_run {
                println!("Dry run OK");
            }
            if let Some(ref diff) = preview.bundle_diff {
                eprintln!(
                    "Bundle diff: +{} added, -{} removed",
                    diff.added.len(),
                    diff.removed.len()
                );
            }
            if url.is_empty() {
                println!("{}", preview.file_path.display());
            } else {
                println!("{url}");
            }
        }
        SessionActionFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&json_payload)?);
        }
    }
    Ok(())
}

fn share_result_json(
    primary: &Session,
    inputs: &[BundledSessionInput],
    preview: &crate::share::SharePreview,
    url: &str,
    dry_run: bool,
) -> Result<serde_json::Value> {
    let sessions_json: Vec<serde_json::Value> =
        inputs.iter().map(|item| session_ref_json(&item.session)).collect();
    let mut payload = serde_json::json!({
        "session": session_ref_json(primary),
        "sessions": sessions_json,
        "share": {
            "provider": preview.provider,
            "project_name": preview.project_name,
            "project_domain": preview.project_domain,
            "publish_dir": preview.publish_dir,
            "file_path": preview.file_path,
            "share_id": preview.share_id,
            "url": url,
            "html_bytes": preview.html_bytes
        },
        "dry_run": dry_run
    });
    if let Some(ref diff) = preview.bundle_diff {
        payload["bundle_diff"] = serde_json::to_value(diff)?;
    }
    Ok(payload)
}

fn resolve_share_bundle_inputs(
    store: &Store,
    sources: &[(String, String)],
    ids: Vec<String>,
    source: Option<&str>,
    source_id: Option<&str>,
    include_related: bool,
    exclude_ids: Vec<String>,
) -> Result<Vec<BundledSessionInput>> {
    if !ids.is_empty() && (source.is_some() || source_id.is_some()) {
        anyhow::bail!("use either --id or --source with --source-id, not both");
    }
    if source.is_some() != source_id.is_some() {
        anyhow::bail!("--source and --source-id must be provided together");
    }
    if ids.is_empty() && source.is_none() {
        anyhow::bail!("missing session selector: pass --id or --source with --source-id");
    }

    let mut seeds: Vec<Session> = Vec::new();
    let mut seed_ids: HashSet<String> = HashSet::new();
    if !ids.is_empty() {
        for id in ids {
            let session = resolve_session_ref(
                store,
                sources,
                &SessionSelector { id: Some(id), source: None, source_id: None },
            )?;
            if seed_ids.insert(session.id.clone()) {
                seeds.push(session);
            }
        }
    } else {
        let session = resolve_session_ref(
            store,
            sources,
            &SessionSelector {
                id: None,
                source: source.map(str::to_string),
                source_id: source_id.map(str::to_string),
            },
        )?;
        seeds.push(session);
    }

    let exclude_set: HashSet<String> = exclude_ids.into_iter().collect();
    let graph = RelatedSessionGraph::for_session(store, &seeds[0])?;

    struct SelectedMeta {
        session: Session,
        parent_id: Option<String>,
        depth: usize,
        relation: Option<crate::session::related::RelatedRelation>,
        via: Option<String>,
        role_label: Option<String>,
        display_title: String,
        spawn_anchor: Option<u32>,
    }

    let mut selected: Vec<SelectedMeta> = Vec::new();
    let mut selected_ids: HashSet<String> = HashSet::new();

    if include_related {
        if exclude_set.contains(&graph.root.id) {
            anyhow::bail!("cannot exclude the primary session: {}", graph.root.id);
        }
        selected_ids.insert(graph.root.id.clone());
        let root_title = crate::session::related::format_related_title(&graph.root, None, None);
        selected.push(SelectedMeta {
            session: graph.root,
            parent_id: None,
            depth: 0,
            relation: None,
            via: None,
            role_label: None,
            display_title: root_title,
            spawn_anchor: None,
        });
        for item in graph.related {
            if item.session.message_count == 0
                || exclude_set.contains(&item.session.id)
                || !selected_ids.insert(item.session.id.clone())
            {
                continue;
            }
            selected.push(SelectedMeta {
                session: item.session,
                parent_id: Some(item.parent_id),
                depth: item.depth,
                relation: Some(item.relation),
                via: item.via,
                role_label: item.role_label,
                display_title: item.display_title,
                spawn_anchor: item.spawn_anchor,
            });
        }
        for seed in seeds {
            if exclude_set.contains(&seed.id) || !selected_ids.insert(seed.id.clone()) {
                continue;
            }
            let title = crate::session::related::format_related_title(&seed, None, None);
            selected.push(SelectedMeta {
                session: seed,
                parent_id: None,
                depth: 1,
                relation: None,
                via: None,
                role_label: None,
                display_title: title,
                spawn_anchor: None,
            });
        }
    } else {
        let primary = seeds.remove(0);
        if exclude_set.contains(&primary.id) {
            anyhow::bail!("cannot exclude the primary session: {}", primary.id);
        }
        selected_ids.insert(primary.id.clone());
        let primary_title = crate::session::related::format_related_title(&primary, None, None);
        selected.push(SelectedMeta {
            session: primary,
            parent_id: None,
            depth: 0,
            relation: None,
            via: None,
            role_label: None,
            display_title: primary_title,
            spawn_anchor: None,
        });

        let mut related_map: HashMap<String, crate::session::related::RelatedSession> =
            graph.related.into_iter().map(|r| (r.session.id.clone(), r)).collect();

        for seed in seeds {
            if exclude_set.contains(&seed.id) || !selected_ids.insert(seed.id.clone()) {
                continue;
            }
            if let Some(rel) = related_map.remove(&seed.id) {
                selected.push(SelectedMeta {
                    session: rel.session,
                    parent_id: Some(rel.parent_id),
                    depth: rel.depth,
                    relation: Some(rel.relation),
                    via: rel.via,
                    role_label: rel.role_label,
                    display_title: rel.display_title,
                    spawn_anchor: rel.spawn_anchor,
                });
            } else {
                let title = crate::session::related::format_related_title(&seed, None, None);
                selected.push(SelectedMeta {
                    session: seed,
                    parent_id: None,
                    depth: 1,
                    relation: None,
                    via: None,
                    role_label: None,
                    display_title: title,
                    spawn_anchor: None,
                });
            }
        }
    }

    if selected.len() > MAX_BUNDLE_SESSIONS {
        anyhow::bail!(
            "session bundle has {} sessions, which exceeds the {MAX_BUNDLE_SESSIONS} session limit",
            selected.len()
        );
    }

    let mut inputs = Vec::with_capacity(selected.len());
    for item in selected {
        let messages = store.get_messages(&item.session.id)?;
        let events = store.list_session_events_for_session(&item.session.id)?;
        let usage_events = store.list_usage_events_for_session(&item.session.id)?;
        let display_meta =
            crate::share::meta::collect_session_display_meta(&item.session, &usage_events);
        inputs.push(BundledSessionInput {
            session: item.session,
            messages,
            events,
            display_meta,
            parent_id: item.parent_id,
            depth: item.depth,
            relation: item.relation,
            via: item.via,
            role_label: item.role_label,
            display_title: item.display_title,
            spawn_anchor: item.spawn_anchor,
        });
    }
    Ok(inputs)
}

pub(super) fn cmd_session_command(
    selector: &SessionSelector,
    print_command: bool,
    format: SessionActionFormat,
    action: SessionAction,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    if session.is_import || !store.has_native_binding(&session.id)? {
        anyhow::bail!("imported session is not resumable on this machine");
    }
    let command = session_action::command_for(action, &session.source, &session.source_id)
        .ok_or_else(|| {
            anyhow::anyhow!("{} is not supported for {}", action.label(), session.source)
        })?;

    if print_command {
        match format {
            SessionActionFormat::Text => println!("{}", command.display()),
            SessionActionFormat::Json => {
                let display = command.display();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "session": session_ref_json(&session),
                        "command": {
                            "program": command.program,
                            "args": command.args,
                            "display": display
                        }
                    }))?
                );
            }
        }
        return Ok(());
    }

    session_action::run(&command, session.directory.as_deref())
}

pub(super) fn cmd_session_handoff(
    selector: &SessionSelector,
    target: &str,
    print_prompt: bool,
) -> Result<()> {
    let store = Store::open()?;
    let sources = adapters::source_labels();
    let session = resolve_session_ref(&store, &sources, selector)?;
    let target = handoff::target_for(target)?;
    let messages = store.get_messages(&session.id)?;
    let prompt = handoff::build_prompt(&session, &messages);

    if print_prompt {
        print!("{prompt}");
        return Ok(());
    }

    let command = handoff::command_for_target(&target, prompt)?;
    handoff::require_installed(&command)?;
    session_action::run(&command, handoff_working_directory(session.directory.as_deref()))
}

pub(super) fn handoff_working_directory(directory: Option<&str>) -> Option<&str> {
    directory.filter(|dir| std::path::Path::new(*dir).is_dir())
}

pub(super) fn read_tldr_file(path: &Path) -> Option<String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let trimmed = content.trim();
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        }
        Err(err) => {
            eprintln!("Warning: skipping TL;DR file {}: {err}", path.display());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::store::SessionTopologyWrite;
    use crate::types::{ParentLink, ParentRelation, Role, ThreadRole};

    #[test]
    fn share_selector_validation_rejects_invalid_combinations() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let sources = adapters::source_labels();

        assert!(
            resolve_share_bundle_inputs(&store, &sources, vec![], None, None, false, vec![])
                .is_err()
        );
        assert!(
            resolve_share_bundle_inputs(
                &store,
                &sources,
                vec![],
                Some("codex"),
                None,
                false,
                vec![]
            )
            .is_err()
        );
        assert!(
            resolve_share_bundle_inputs(
                &store,
                &sources,
                vec![],
                None,
                Some("src-1"),
                false,
                vec![]
            )
            .is_err()
        );
        assert!(
            resolve_share_bundle_inputs(
                &store,
                &sources,
                vec!["id-1".into()],
                Some("codex"),
                Some("src-1"),
                false,
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn include_related_climbs_to_root_skips_empty_and_applies_exclude_id() {
        schema::register_sqlite_vec();
        let store = Store::open_in_memory().unwrap();
        let sources = adapters::source_labels();

        let root = Session {
            id: "root-id".into(),
            source: "codex".into(),
            source_id: "root-src".into(),
            title: "Root".into(),
            message_count: 1,
            started_at: 100,
            ..crate::types::test_support::session("root-id")
        };
        let child = Session {
            id: "child-id".into(),
            source: "codex".into(),
            source_id: "child-src".into(),
            title: "Child".into(),
            message_count: 1,
            started_at: 200,
            ..crate::types::test_support::session("child-id")
        };
        let excluded = Session {
            id: "excluded-id".into(),
            source: "codex".into(),
            source_id: "excluded-src".into(),
            title: "Excluded".into(),
            message_count: 1,
            started_at: 250,
            ..crate::types::test_support::session("excluded-id")
        };
        let empty = Session {
            id: "empty-id".into(),
            source: "codex".into(),
            source_id: "empty-src".into(),
            title: "Empty".into(),
            message_count: 0,
            started_at: 300,
            ..crate::types::test_support::session("empty-id")
        };

        store
            .persist_session_with_usage_and_events_with_topology(
                &root,
                &[crate::types::test_support::message("root-id", Role::User, "hi", 0)],
                &[],
                None,
                &[],
                None,
                &SessionTopologyWrite {
                    thread_role: Some(ThreadRole::Primary),
                    parents: &[],
                    parser_version: Some(1),
                },
            )
            .unwrap();
        for (sess, msg) in [(&child, Some("c")), (&excluded, Some("x")), (&empty, None)] {
            let msgs: Vec<_> = msg
                .into_iter()
                .map(|m| crate::types::test_support::message(&sess.id, Role::User, m, 0))
                .collect();
            store
                .persist_session_with_usage_and_events_with_topology(
                    sess,
                    &msgs,
                    &[],
                    None,
                    &[],
                    None,
                    &SessionTopologyWrite {
                        thread_role: Some(ThreadRole::Subagent),
                        parents: &[ParentLink {
                            relation: ParentRelation::Spawn,
                            source: "codex".into(),
                            source_id: "root-src".into(),
                        }],
                        parser_version: Some(1),
                    },
                )
                .unwrap();
        }

        let inputs = resolve_share_bundle_inputs(
            &store,
            &sources,
            vec!["child-id".into()],
            None,
            None,
            true,
            vec!["excluded-id".into()],
        )
        .unwrap();
        let ids: Vec<&str> = inputs.iter().map(|i| i.session.id.as_str()).collect();
        assert_eq!(ids, vec!["root-id", "child-id"]);

        assert!(
            resolve_share_bundle_inputs(
                &store,
                &sources,
                vec!["child-id".into()],
                None,
                None,
                true,
                vec!["root-id".into()],
            )
            .is_err()
        );
    }
}
