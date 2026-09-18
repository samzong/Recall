use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};

use recall_publish::config::{Config, config_path};
use recall_publish::doctor;
use recall_publish::manifest::manifest_json;
use recall_publish::pipeline::{self, PrepareRequest, Selection};
use recall_publish::progress::Progress;
use recall_publish::protocol::RecallClient;
use recall_publish::scan::environment_dir;
use recall_publish::select::{TimeWindow, scope_name, validate_scope_name};
use recall_publish::upload;
use recall_publish::workspace::{Workspace, default_data_dir, publish_root};

#[derive(Parser)]
#[command(
    name = "recall-publish",
    about = "Prepare, review, and publish a redacted Recall session dataset",
    version
)]
struct Cli {
    #[arg(long = "recall-extension-manifest", hide = true)]
    print_manifest: bool,
    #[arg(long, global = true)]
    quiet: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    Doctor(DoctorArgs),
    Prepare(Box<PrepareArgs>),
    Approve(ScopeArgs),
    Upload(UploadArgs),
}

#[derive(Args)]
struct DoctorArgs {
    #[arg(long)]
    install: bool,
}

#[derive(Args)]
struct PrepareArgs {
    #[arg(long, default_value = "all")]
    project: String,
    #[arg(long = "source")]
    sources: Vec<String>,
    #[arg(long = "thread-role")]
    thread_roles: Vec<String>,
    #[arg(long)]
    since: Option<String>,
    #[arg(long)]
    until: Option<String>,
    #[arg(long, default_value = "UTC")]
    timezone: String,
    #[arg(long = "session")]
    sessions: Vec<String>,
    #[arg(long = "exclude-session")]
    excluded_sessions: Vec<String>,
    #[arg(long)]
    license: String,
    #[arg(long)]
    author: Option<String>,
    #[arg(long)]
    scope: Option<String>,
}

#[derive(Args)]
struct ScopeArgs {
    scope: String,
}

#[derive(Args)]
struct UploadArgs {
    scope: String,
    #[arg(long)]
    repo: String,
    #[arg(long)]
    private: bool,
    #[arg(long = "dry-run")]
    dry_run: bool,
}

fn emit(value: &Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn load_config() -> Result<Config> {
    let path = config_path()?;
    let config = Config::load(&path)?;
    config.validate()?;
    Ok(config)
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.print_manifest {
        println!("{}", serde_json::to_string(&manifest_json())?);
        return Ok(());
    }
    let Some(command) = cli.command else {
        bail!("no command given; run `recall publish --help`");
    };

    let client = RecallClient::from_env();
    let data_dir = default_data_dir()?;
    let root = publish_root(&data_dir);

    match command {
        Command::Doctor(args) => {
            if args.install {
                doctor::install(&environment_dir(&data_dir))?;
            }
            let report = doctor::report(&data_dir, &client)?;
            emit(&report)?;
            if report["ready"] != Value::Bool(true) {
                bail!("the publishing environment is not ready");
            }
        }
        Command::Prepare(args) => {
            let config = load_config()?;
            let author = args
                .author
                .clone()
                .or_else(|| config.publisher.author.clone())
                .context("no author; pass --author or set publisher.author in the config")?;
            let window =
                TimeWindow::parse(args.since.as_deref(), args.until.as_deref(), &args.timezone)?;
            let scope = match args.scope.clone() {
                Some(scope) => scope,
                None => scope_name(
                    Some(&author),
                    Some(&args.project),
                    &args.sources,
                    window.label().as_deref(),
                ),
            };
            validate_scope_name(&scope)?;
            let env_dir = environment_dir(&data_dir);
            doctor::materialize(&env_dir)?;
            let workspace = Workspace::new(&root, &scope);
            let progress = Progress::new(cli.quiet);
            let request = PrepareRequest {
                selection: Selection {
                    project: args.project,
                    sources: args.sources,
                    thread_roles: args.thread_roles,
                    window,
                    include_ids: args.sessions,
                    exclude_ids: args.excluded_sessions,
                },
                scope: scope.clone(),
                license: args.license,
                author,
                config: &config,
                env_dir: &env_dir,
                progress: &progress,
            };
            let preview = pipeline::prepare(&client, &workspace, &request)?;
            emit(&json!({
                "scope": scope,
                "workspace": workspace.root.display().to_string(),
                "preview": preview,
                "next": format!("review the workspace, then run `recall publish approve {scope}`"),
            }))?;
        }
        Command::Approve(args) => {
            validate_scope_name(&args.scope)?;
            let workspace = Workspace::new(&root, &args.scope);
            let approval = pipeline::approve(&workspace)?;
            emit(&serde_json::to_value(&approval)?)?;
        }
        Command::Upload(args) => {
            validate_scope_name(&args.scope)?;
            let workspace = Workspace::new(&root, &args.scope);
            let result = if args.dry_run {
                upload::dry_run(&workspace, &args.repo)?
            } else {
                upload::upload(&workspace, &environment_dir(&data_dir), &args.repo, args.private)?
            };
            emit(&result)?;
        }
    }
    Ok(())
}
