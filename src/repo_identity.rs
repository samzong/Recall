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

        let Some(toplevel) = git_toplevel(directory) else {
            self.by_directory.insert(directory.to_string(), None);
            return None;
        };

        if let Some(identity) = self.by_toplevel.get(&toplevel) {
            let identity = identity.clone();
            self.by_directory.insert(directory.to_string(), identity.clone());
            return identity;
        }

        let identity = origin_identity(&toplevel);
        self.by_toplevel.insert(toplevel, identity.clone());
        self.by_directory.insert(directory.to_string(), identity.clone());
        identity
    }
}

pub(crate) fn git_toplevel(directory: &str) -> Option<String> {
    git_output(["-C", directory, "rev-parse", "--show-toplevel"])
}

pub(crate) fn origin_identity(toplevel: &str) -> Option<RepoIdentity> {
    git_output(["-C", toplevel, "remote", "get-url", "origin"])
        .and_then(|url| normalize_remote_url(&url))
}

/// Parses any Git remote form into a host-qualified identity:
/// `scheme://[user@]host[:port]/path`, scp-like `[user@]host:path`, and bare
/// `host/path`. GitHub inputs keep the exact `github.com/owner/repo` shape that
/// is already persisted in `sessions.repo_remote`, so widening host support
/// does not rewrite existing rows.
pub(crate) fn normalize_remote_url(url: &str) -> Option<RepoIdentity> {
    let (host, path) = split_host_and_path(url.trim())?;
    let slug = normalize_slug(path)?;
    let name = slug.rsplit('/').next()?.to_string();
    Some(RepoIdentity { remote: format!("{host}/{slug}"), slug, name })
}

/// Normalizes an `owner/repo` selector, keeping extra path segments so GitLab
/// subgroups survive. Returns `None` for anything that is not a bare slug.
pub(crate) fn normalize_slug(value: &str) -> Option<String> {
    let value = value.trim().trim_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    let segments: Vec<&str> = value.split('/').map(str::trim).collect();
    if segments.len() < 2 || segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    Some(segments.join("/"))
}

fn split_host_and_path(url: &str) -> Option<(String, &str)> {
    if url.is_empty() {
        return None;
    }

    let scheme_relative = ["https://", "http://", "ssh://", "git://", "git+ssh://"]
        .iter()
        .find_map(|scheme| url.strip_prefix(scheme));

    if let Some(rest) = scheme_relative {
        let (authority, path) = strip_userinfo(rest).split_once('/')?;
        return Some((normalize_host(authority)?, path));
    }

    let rest = strip_userinfo(url);
    // scp-like remotes put the path after a colon; a bare `host/owner/repo`
    // splits on the first slash instead.
    let (authority, path) = rest.split_once(':').or_else(|| rest.split_once('/'))?;
    Some((normalize_host(authority)?, path))
}

fn strip_userinfo(value: &str) -> &str {
    value.split_once('@').map(|(_, rest)| rest).unwrap_or(value)
}

fn normalize_host(authority: &str) -> Option<String> {
    let host = match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => authority,
    };
    let host = host.trim().trim_end_matches('.');
    if host.is_empty() || !host_looks_like_host(host) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

/// Guards the schemeless forms: without this, the slug `group/subgroup/repo`
/// would be read as host `group` plus path `subgroup/repo`.
fn host_looks_like_host(host: &str) -> bool {
    host.contains('.') || host.eq_ignore_ascii_case("localhost")
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
            "github.com/samzong/Recall",
        ] {
            let identity = normalize_remote_url(url).unwrap();
            assert_eq!(identity.remote, "github.com/samzong/Recall");
            assert_eq!(identity.slug, "samzong/Recall");
            assert_eq!(identity.name, "Recall");
        }
    }

    #[test]
    fn normalizes_non_github_remote_urls() {
        let cases = [
            ("git@gitlab.com:group/subgroup/app.git", "gitlab.com/group/subgroup/app"),
            ("ssh://git@git.internal.example:2222/team/app.git", "git.internal.example/team/app"),
            ("https://GitLab.com/group/app", "gitlab.com/group/app"),
            ("https://bitbucket.org/team/app.git", "bitbucket.org/team/app"),
        ];
        for (url, expected_remote) in cases {
            let identity = normalize_remote_url(url).unwrap();
            assert_eq!(identity.remote, expected_remote, "{url}");
            assert_eq!(identity.name, "app", "{url}");
        }
    }

    #[test]
    fn rejects_values_without_a_host() {
        for value in ["", "samzong", "samzong/Recall", "group/subgroup/app", "/abs/path"] {
            assert_eq!(normalize_remote_url(value), None, "{value}");
        }
    }

    #[test]
    fn normalizes_slug_selectors() {
        assert_eq!(normalize_slug("samzong/Recall.git").as_deref(), Some("samzong/Recall"));
        assert_eq!(normalize_slug("/group/subgroup/app/").as_deref(), Some("group/subgroup/app"));
        assert_eq!(normalize_slug("Recall"), None);
        assert_eq!(normalize_slug("samzong//Recall"), None);
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
