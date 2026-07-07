use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoIdentity {
    pub(crate) remote: String,
    pub(crate) slug: String,
    pub(crate) name: String,
}

#[derive(Default)]
pub(crate) struct RepoIdentityCache {
    by_directory: HashMap<String, Option<RepoIdentity>>,
    by_toplevel: HashMap<String, Option<RepoIdentity>>,
}

impl RepoIdentityCache {
    pub(crate) fn resolve(&mut self, directory: Option<&str>) -> Option<RepoIdentity> {
        let directory = directory?.trim();
        if directory.is_empty() {
            return None;
        }
        if let Some(identity) = self.by_directory.get(directory) {
            return identity.clone();
        }

        let toplevel = git_output(["-C", directory, "rev-parse", "--show-toplevel"]);
        let Some(toplevel) = toplevel else {
            self.by_directory.insert(directory.to_string(), None);
            return None;
        };

        if let Some(identity) = self.by_toplevel.get(&toplevel) {
            let identity = identity.clone();
            self.by_directory.insert(directory.to_string(), identity.clone());
            return identity;
        }

        let identity = git_output(["-C", toplevel.as_str(), "remote", "get-url", "origin"])
            .and_then(|url| normalize_remote_url(&url));
        self.by_toplevel.insert(toplevel, identity.clone());
        self.by_directory.insert(directory.to_string(), identity.clone());
        identity
    }
}

pub(crate) fn normalize_remote_url(url: &str) -> Option<RepoIdentity> {
    let trimmed = url.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let path = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .or_else(|| trimmed.strip_prefix("ssh://git@github.com/"))
        .or_else(|| trimmed.strip_prefix("github.com/"))?;
    identity_from_slug(path)
}

pub(crate) fn identity_from_slug(slug: &str) -> Option<RepoIdentity> {
    let slug = slug.trim().trim_matches('/');
    let slug = slug.strip_suffix(".git").unwrap_or(slug);
    let mut parts = slug.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    if owner.is_empty() || name.is_empty() || parts.next().is_some() {
        return None;
    }
    let slug = format!("{owner}/{name}");
    Some(RepoIdentity { remote: format!("github.com/{slug}"), slug, name: name.to_string() })
}

fn git_output<const N: usize>(args: [&str; N]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() { None } else { Some(value.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn normalizes_github_remote_urls() {
        for url in [
            "https://github.com/samzong/Recall.git",
            "git@github.com:samzong/Recall.git",
            "ssh://git@github.com/samzong/Recall.git",
        ] {
            let identity = normalize_remote_url(url).unwrap();
            assert_eq!(identity.remote, "github.com/samzong/Recall");
            assert_eq!(identity.slug, "samzong/Recall");
            assert_eq!(identity.name, "Recall");
        }
    }

    #[test]
    fn resolves_identity_from_git_directory() {
        let root = std::env::temp_dir().join(format!("recall-repo-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Command::new("git").args(["init"]).current_dir(&root).output().unwrap();
        Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:samzong/Recall.git"])
            .current_dir(&root)
            .output()
            .unwrap();

        let mut cache = RepoIdentityCache::default();
        let identity = cache.resolve(root.to_str()).unwrap();
        assert_eq!(identity.remote, "github.com/samzong/Recall");
        assert_eq!(identity.slug, "samzong/Recall");
        assert_eq!(identity.name, "Recall");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn non_git_directory_has_no_identity() {
        let root = std::env::temp_dir().join(format!("recall-non-git-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let mut cache = RepoIdentityCache::default();
        assert_eq!(cache.resolve(root.to_str()), None);
        fs::remove_dir_all(root).unwrap();
    }
}
