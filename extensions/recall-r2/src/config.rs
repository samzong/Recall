use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::protocol::{Failure, MESSAGE_LIMIT, Result, validate_key};

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Target {
    pub endpoint: String,
    pub bucket: String,
    pub prefix: String,
    pub credential_profile: String,
}

#[derive(Parser)]
#[command(name = "recall-r2 --recall-remote-configure")]
pub(crate) struct Configure {
    #[arg(long)]
    endpoint: Option<String>,
    #[arg(long)]
    bucket: Option<String>,
    #[arg(long)]
    prefix: Option<String>,
    #[arg(long)]
    credential_profile: Option<String>,
}

pub(crate) fn path() -> Result<PathBuf> {
    dirs::config_dir().map(|dir| dir.join("recall").join("r2.json")).ok_or_else(Failure::io)
}

impl Target {
    pub fn load(path: &Path) -> Result<Self> {
        if fs::metadata(path).is_ok_and(|metadata| !metadata.is_file()) {
            return Err(Failure::invalid("R2 configuration must be a regular file"));
        }
        let file = fs::File::open(path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                Failure::new("not_configured", "R2 is not configured; run recall remote connect")
            } else {
                Failure::io()
            }
        })?;
        let mut bytes = Vec::new();
        file.take(MESSAGE_LIMIT + 1).read_to_end(&mut bytes).map_err(|_| Failure::io())?;
        if bytes.len() as u64 > MESSAGE_LIMIT {
            return Err(Failure::invalid("R2 configuration is too large"));
        }
        let target: Self = serde_json::from_slice(&bytes).map_err(|_| {
            Failure::invalid("R2 configuration is invalid; run recall remote connect")
        })?;
        target.validate()?;
        Ok(target)
    }

    fn validate(&self) -> Result<()> {
        let endpoint = Url::parse(&self.endpoint).map_err(|_| {
            Failure::invalid("use the R2 S3 endpoint from the Cloudflare dashboard")
        })?;
        let labels: Vec<_> = endpoint.host_str().unwrap_or_default().split('.').collect();
        let account = labels.first().copied().unwrap_or_default();
        let suffix = &labels[1.min(labels.len())..];
        let r2_host = suffix == ["r2", "cloudflarestorage", "com"]
            || matches!(suffix, ["eu" | "us" | "fedramp", "r2", "cloudflarestorage", "com"]);
        if endpoint.scheme() != "https"
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.port().is_some()
            || endpoint.path() != "/"
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !r2_host
            || account.len() != 32
            || !account.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Failure::invalid(
                "use an HTTPS R2 S3 account endpoint without a path or credentials",
            ));
        }
        if !(3..=63).contains(&self.bucket.len())
            || !self
                .bucket
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            || self.bucket.starts_with('-')
            || self.bucket.ends_with('-')
        {
            return Err(Failure::invalid("invalid R2 bucket name"));
        }
        validate_key(&self.prefix, true)?;
        if self.prefix.len() >= 1024 {
            return Err(Failure::invalid("R2 directory leaves no space for an object key"));
        }
        if self.credential_profile.is_empty()
            || self.credential_profile.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "credential profile must be a nonempty local profile name",
            ));
        }
        Ok(())
    }

    pub fn full_key(&self, relative: &str, prefix: bool) -> Result<String> {
        validate_key(relative, prefix)?;
        let key = format!("{}{relative}", self.prefix);
        if key.len() > 1024 {
            return Err(Failure::invalid("R2 object key exceeds 1024 bytes"));
        }
        Ok(key)
    }

    fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        let parent = path.parent().ok_or_else(Failure::io)?;
        fs::create_dir_all(parent).map_err(|_| Failure::io())?;
        let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| Failure::io())?;
        serde_json::to_writer_pretty(&mut file, self).map_err(|_| Failure::io())?;
        file.flush().map_err(|_| Failure::io())?;
        file.as_file().sync_all().map_err(|_| Failure::io())?;
        file.persist(path).map_err(|_| Failure::io())?;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| Failure::io())?;
        Ok(())
    }
}

impl Configure {
    pub fn run(self) -> Result<()> {
        let path = path()?;
        let interactive = io::stdin().is_terminal() && io::stderr().is_terminal();
        let complete = self.endpoint.is_some()
            && self.bucket.is_some()
            && self.prefix.is_some()
            && self.credential_profile.is_some();
        if !interactive && !complete {
            return Err(Failure::invalid(
                "non-interactive configuration requires --endpoint, --bucket, --prefix and --credential-profile",
            ));
        }
        let existing = match Target::load(&path) {
            Ok(target) => Some(target),
            Err(error) if error.code == "not_configured" => None,
            Err(error) => {
                eprintln!("{}; replacing the configuration requires complete input", error.message);
                None
            }
        };
        let endpoint = field(
            self.endpoint,
            "R2 S3 endpoint",
            existing.as_ref().map(|c| c.endpoint.as_str()),
            interactive,
        )?;
        let bucket = field(
            self.bucket,
            "Bucket",
            existing.as_ref().map(|c| c.bucket.as_str()),
            interactive,
        )?;
        let prefix = field(
            self.prefix,
            "Directory",
            Some(existing.as_ref().map_or("recall/", |c| c.prefix.as_str())),
            interactive,
        )?;
        let credential_profile = field(
            self.credential_profile,
            "Local credential profile",
            Some(existing.as_ref().map_or("recall-r2", |c| c.credential_profile.as_str())),
            interactive,
        )?;
        let prefix =
            if prefix.is_empty() || prefix.ends_with('/') { prefix } else { format!("{prefix}/") };
        let target = Target { endpoint, bucket, prefix, credential_profile };
        target.validate()?;
        if interactive {
            eprintln!(
                "\nR2 endpoint: {}\nBucket: {}\nDirectory: {}\nCredential profile: {}",
                target.endpoint, target.bucket, target.prefix, target.credential_profile
            );
            eprint!("Save this R2 destination? [y/N] ");
            io::stderr().flush().map_err(|_| Failure::io())?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer).map_err(|_| Failure::io())?;
            if !answer.trim().eq_ignore_ascii_case("y")
                && !answer.trim().eq_ignore_ascii_case("yes")
            {
                return Err(Failure::invalid("R2 configuration cancelled"));
            }
        }
        target.save(&path)?;
        eprintln!("R2 destination saved. Access has not yet been verified.");
        Ok(())
    }
}

fn field(
    value: Option<String>,
    label: &str,
    default: Option<&str>,
    interactive: bool,
) -> Result<String> {
    if let Some(value) = value {
        return Ok(value);
    }
    if !interactive {
        return Err(Failure::invalid("missing R2 configuration parameter"));
    }
    if let Some(default) = default {
        eprint!("{label} [{default}]: ");
    } else {
        eprint!("{label}: ");
    }
    io::stderr().flush().map_err(|_| Failure::io())?;
    let mut value = String::new();
    if io::stdin().read_line(&mut value).map_err(|_| Failure::io())? == 0 {
        return Err(Failure::invalid("R2 configuration cancelled"));
    }
    let value = value.trim();
    Ok(if value.is_empty() { default.unwrap_or_default().to_owned() } else { value.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            endpoint: "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com".into(),
            bucket: "recall-test".into(),
            prefix: "foo/".into(),
            credential_profile: "recall-r2".into(),
        }
    }

    #[test]
    fn credentials_can_only_be_sent_to_an_r2_account_endpoint() {
        let mut target = target();
        for endpoint in [
            "https://example.com",
            "https://r2.cloudflarestorage.com.evil.example",
            "http://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com",
            "https://secret@0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com",
            "https://0123456789abcdef0123456789abcdef.r2.cloudflarestorage.com/bucket",
        ] {
            target.endpoint = endpoint.into();
            assert!(target.validate().is_err());
        }
        for jurisdiction in ["", "eu.", "us.", "fedramp."] {
            target.endpoint = format!(
                "https://0123456789abcdef0123456789abcdef.{jurisdiction}r2.cloudflarestorage.com"
            );
            assert!(target.validate().is_ok());
        }
    }

    #[test]
    fn target_roundtrips_and_invalid_replacement_preserves_previous_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r2.json");
        let mut target = target();
        target.save(&path).unwrap();
        assert_eq!(Target::load(&path).unwrap().bucket, "recall-test");
        target.endpoint = "https://example.com".into();
        assert!(target.save(&path).is_err());
        assert_eq!(Target::load(&path).unwrap().bucket, "recall-test");
    }

    #[test]
    fn complete_key_limit_includes_target_prefix() {
        let target = target();
        assert_eq!(target.full_key("a/b", false).unwrap(), "foo/a/b");
        assert!(target.full_key(&"a".repeat(1021), false).is_err());
        assert_eq!(target.full_key("", true).unwrap(), "foo/");
    }
}
