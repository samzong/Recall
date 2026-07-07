use std::io::Write as _;
use std::path::PathBuf;

use anyhow::Result;

use crate::config::AppConfig;

pub(crate) fn run(project_name: Option<String>, publish_dir: Option<PathBuf>) -> Result<()> {
    let mut config = AppConfig::load_or_default();
    let existing = config.share.clone();
    if let Some(ref share) = existing {
        println!("Share already initialized");
        println!("  Provider     {}", share.provider);
        println!("  Project      {}", share.project_name);
        println!("  Publish dir  {}", share.publish_dir);
        println!("  URL base     https://{}", share.project_domain);
        if !prompt_yes_no_default_yes("Reinitialize?")? {
            return Ok(());
        }
    }

    println!("Checking Wrangler and Cloudflare Pages...");
    crate::share::preflight_cloudflare_pages()?;

    let default_project = existing
        .as_ref()
        .map(|share| share.project_name.clone())
        .unwrap_or_else(crate::share::default_project_name);
    let project_name = match project_name {
        Some(name) => name,
        None => prompt_with_default("Cloudflare Pages project", &default_project)?,
    };

    let default_dir =
        existing.as_ref().map(|share| share.publish_dir.clone()).unwrap_or_else(|| {
            crate::share::default_publish_dir()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_else(|_| "share-pages".to_string())
        });
    let publish_dir = match publish_dir {
        Some(path) => crate::share::expand_path(&path.to_string_lossy()),
        None => {
            let input = prompt_with_default("Local share directory", &default_dir)?;
            crate::share::expand_path(&input)
        }
    };

    println!("Configuring Cloudflare Pages share target...");
    crate::share::init_cloudflare_pages(&mut config, project_name.clone(), publish_dir.clone())?;
    println!("Share initialized");
    println!("  Project      {project_name}");
    println!("  Publish dir  {}", publish_dir.display());
    if let Some(share) = config.share.as_ref() {
        println!("  URL base     https://{}", share.project_domain);
    }
    Ok(())
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();
    if trimmed.is_empty() { Ok(default.to_string()) } else { Ok(trimmed.to_string()) }
}

fn prompt_yes_no_default_yes(label: &str) -> Result<bool> {
    print!("{label} [Y/n]: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(trimmed.is_empty() || trimmed == "y" || trimmed == "yes")
}
