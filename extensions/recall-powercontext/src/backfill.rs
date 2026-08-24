use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::export::{RecallClient, normalize_time_selector, visit_stdin_sessions};
use crate::report::{BackfillReport, CountKind, CountSink};
use crate::scope::resolve_scope;
use crate::server::{CaptureResult, PowerContextClient, validate_server_url};
use crate::session::{CaptureItem, ExportRecord, RoleSet, session_captures};

pub struct BackfillOptions {
    pub cwd: PathBuf,
    pub server_url: String,
    pub token: Option<String>,
    pub time: Option<String>,
    pub roles: RoleSet,
    pub stdin: bool,
    pub dry_run: bool,
}

pub fn run(options: BackfillOptions) -> Result<BackfillReport> {
    let scope = resolve_scope(&options.cwd)?;
    let time = normalize_time_selector(options.time.as_deref())?;
    if options.stdin && time.is_some() {
        bail!("--time cannot be combined with --stdin; filter with recall export --time");
    }
    let server_url = validate_server_url(&options.server_url)?;
    let client = if options.dry_run {
        None
    } else {
        Some(PowerContextClient::new(server_url.clone(), options.token.clone()))
    };
    let watermark = match &client {
        Some(client) => client.journal_watermark(&scope.id)?,
        None => 0,
    };
    let mut counts = CountSink::default();
    {
        let mut visit = |record: ExportRecord| {
            apply_record(&record, &options, &scope.id, client.as_ref(), watermark, &mut counts)
        };
        if options.stdin {
            visit_stdin_sessions(&mut visit)?;
        } else {
            RecallClient::from_env().visit_export_sessions(
                &options.cwd,
                &scope.project,
                time.as_deref(),
                &mut visit,
            )?;
        }
    }
    Ok(counts.finish(
        scope.id,
        server_url,
        options.dry_run,
        options.roles.as_report_value(),
        time.unwrap_or_else(|| "all".to_string()),
    ))
}

fn apply_record(
    record: &ExportRecord,
    options: &BackfillOptions,
    scope_id: &str,
    client: Option<&PowerContextClient>,
    watermark: u64,
    counts: &mut CountSink,
) -> Result<()> {
    let captures = session_captures(record, options.roles)
        .with_context(|| format!("powercontext session {} failed", record.session.id))?;
    let adapter = record.session.source.trim();
    for item in captures {
        apply_item(adapter, item, options, scope_id, client, watermark, counts)?;
    }
    Ok(())
}

fn apply_item(
    adapter: &str,
    item: CaptureItem,
    options: &BackfillOptions,
    scope_id: &str,
    client: Option<&PowerContextClient>,
    watermark: u64,
    counts: &mut CountSink,
) -> Result<()> {
    let request = match item {
        CaptureItem::Failed(error) => {
            eprintln!("powercontext capture failed: {error}");
            counts.add(adapter, CountKind::Failed);
            return Ok(());
        }
        CaptureItem::Request(request) => request,
    };
    if options.dry_run {
        counts.add(&request.adapter, CountKind::Imported);
        return Ok(());
    }
    let client = client.expect("live backfill always builds a client");
    match client.capture(scope_id, &request)? {
        CaptureResult::Accepted { position } => {
            let kind = if position > watermark { CountKind::Imported } else { CountKind::Skipped };
            counts.add(&request.adapter, kind);
        }
        CaptureResult::Conflict => counts.add(&request.adapter, CountKind::Conflict),
        CaptureResult::Failed => counts.add(&request.adapter, CountKind::Failed),
    }
    Ok(())
}
