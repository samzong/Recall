use crate::error::{Failure, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;

#[derive(Deserialize, Serialize)]
pub struct Config {
    pub scheme: String,
    #[serde(flatten)]
    options: HashMap<String, String>,
}

pub fn path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("RECALL_OPENDAL_CONFIG") {
        if path.is_empty() {
            return Err(Failure::new("not_configured", "RECALL_OPENDAL_CONFIG is empty"));
        }
        return Ok(path.into());
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| Failure::new("not_configured", "cannot determine home directory"))?
        .join(".config/recall-opendal.json"))
}

fn parse(bytes: &[u8]) -> Result<Config> {
    serde_json::from_slice(bytes).map_err(|error| {
        Failure::new("not_configured", format!("invalid OpenDAL configuration: {error}"))
    })
}

pub fn load() -> Result<Config> {
    let path = path()?;
    let bytes = std::fs::read(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            Failure::new(
                "not_configured",
                format!("missing {}; run --recall-remote-configure", path.display()),
            )
        } else {
            Failure::io(error)
        }
    })?;
    parse(&bytes)
}

impl Config {
    pub fn operator(&self) -> Result<opendal::blocking::Operator> {
        let options = self.sanitized_options()?;
        let operator =
            opendal::Operator::via_iter(&self.scheme, options).map_err(Failure::backend)?;
        opendal::blocking::Operator::new(operator).map_err(Failure::backend)
    }

    fn sanitized_options(&self) -> Result<HashMap<String, String>> {
        let mut options = self.options.clone();
        if self.scheme == "fs" {
            // fs writes in place unless atomic_write_dir is set. Recall kills a
            // provider process that exceeds its deadline, which would leave a
            // truncated object at the final key and permanently poison the
            // remote (size mismatch on the next enumeration). OpenDAL resolves
            // atomic_write_dir against the process CWD, so inject an absolute
            // path under the backend root: same-filesystem rename, immune to
            // the caller's working directory, and never listed under v1/.
            let root = options
                .get("root")
                .cloned()
                .ok_or_else(|| Failure::new("not_configured", "fs backend requires a root"))?;
            let staging = std::path::Path::new(&root).join(".recall-opendal-tmp");
            options
                .entry("atomic_write_dir".to_string())
                .or_insert_with(|| staging.to_string_lossy().into_owned());
        } else if options.remove("atomic_write_dir").is_some() {
            return Err(Failure::new(
                "not_configured",
                "atomic_write_dir is only supported by the fs scheme",
            ));
        }
        Ok(options)
    }
}

pub fn configure(args: &[String]) -> Result<()> {
    let config = match args {
        [flag, json] if flag == "--config-json" => parse(json.as_bytes())?,
        [flag, file] if flag == "--config-file" => {
            parse(&std::fs::read(file).map_err(Failure::io)?)?
        }
        [] if path()?.exists() => load()?,
        [] if std::io::stdin().is_terminal() => {
            eprint!(
                "OpenDAL configuration JSON (e.g. {{\"scheme\":\"fs\",\"root\":\"/absolute/path\"}}): "
            );
            std::io::stderr().flush().map_err(Failure::io)?;
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).map_err(Failure::io)?;
            parse(line.as_bytes())?
        }
        [] => {
            return Err(Failure::new(
                "not_configured",
                "use --config-json or --config-file in non-interactive mode",
            ));
        }
        _ => {
            return Err(Failure::new(
                "invalid_request",
                "expected --config-json JSON or --config-file PATH",
            ));
        }
    };
    crate::storage::with_operator(|| crate::storage::verify(&config.operator()?))?;
    let path = path()?;
    let parent =
        path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent).map_err(Failure::io)?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(Failure::io)?;
    serde_json::to_writer_pretty(&mut file, &config)
        .map_err(|error| Failure::new("unavailable", error.to_string()))?;
    file.as_file_mut().sync_all().map_err(Failure::io)?;
    file.persist(&path).map_err(|error| Failure::io(error.error))?;
    eprintln!("saved {} (scheme={})", path.display(), config.scheme);
    Ok(())
}
