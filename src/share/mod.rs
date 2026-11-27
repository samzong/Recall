mod assets;
mod inventory;
pub(crate) mod meta;
mod publish;
pub(crate) mod render;

pub(crate) use inventory::{ShareFormat, run_list, run_unpublish};
pub(crate) use publish::{
    BundleDiff, SharePreview, create_bundle_preview, create_session_preview, default_project_name,
    default_publish_dir, dry_run_bundle_to_output, expand_path, init_cloudflare_pages,
    open_preview_file, preflight_cloudflare_pages, preview_bundle_with_options,
    publish_bundle_with_options,
};
pub(crate) use render::{BundledSessionInput, MAX_BUNDLE_SESSIONS, ShareRenderOptions};

#[cfg(test)]
fn test_session(source_id: &str) -> crate::types::Session {
    crate::types::Session {
        id: "local-id".into(),
        title: "Fix <bug>".into(),
        directory: Some("/tmp/project".into()),
        message_count: 1,
        ..crate::types::test_support::session(source_id)
    }
}
