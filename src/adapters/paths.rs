use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;

pub(crate) fn resolve_home_dir(
    relative: &str,
    missing_message: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let dir = home.join(relative);
    if !dir.exists() {
        tracing::debug!("{missing_message}");
        return Ok(None);
    }
    Ok(Some(dir))
}

/// Confine a discovered/derived path to its source `root`.
///
/// Both `root` and `candidate` are canonicalized (following symlinks) before the
/// containment check, so a source root that is itself a symlink still matches its
/// own descendants. Returns the canonical candidate iff it lives inside the
/// canonical root; otherwise an error. Canonicalization requires the path to
/// exist, so a non-existent candidate yields an error rather than a panic.
pub(crate) fn confined_path(root: &Path, candidate: &Path) -> anyhow::Result<PathBuf> {
    let canonical_root = fs::canonicalize(root)
        .with_context(|| format!("cannot canonicalize source root {}", root.display()))?;
    let canonical_candidate = fs::canonicalize(candidate)
        .with_context(|| format!("cannot canonicalize {}", candidate.display()))?;
    if canonical_candidate.starts_with(&canonical_root) {
        Ok(canonical_candidate)
    } else {
        anyhow::bail!("{} escapes source root {}", candidate.display(), root.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_root(label: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("recall-paths-{}-{}", label, uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn confined_path_accepts_file_inside_root() {
        let root = temp_root("inside");
        let file = root.join("a.jsonl");
        fs::write(&file, b"x").unwrap();

        let resolved = confined_path(&root, &file).unwrap();
        assert!(resolved.starts_with(fs::canonicalize(&root).unwrap()));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn confined_path_rejects_nonexistent_candidate() {
        let root = temp_root("missing");
        let ghost = root.join("nope.jsonl");

        assert!(confined_path(&root, &ghost).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn confined_path_rejects_symlink_escaping_root() {
        use std::os::unix::fs::symlink;

        let base = temp_root("escape");
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();

        // Secret OUTSIDE the confinement root (simulated /etc/passwd).
        let secret = base.join("passwd");
        fs::write(&secret, b"root:x:0:0").unwrap();

        // Symlink INSIDE the root pointing at the outside secret.
        let link = root.join("link.jsonl");
        symlink(&secret, &link).unwrap();

        let err = confined_path(&root, &link).unwrap_err();
        assert!(err.to_string().contains("escapes source root"));

        let _ = fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn confined_path_allows_candidate_via_symlinked_root() {
        // Proves we canonicalize the ROOT too, not only the candidate.
        use std::os::unix::fs::symlink;

        let base = temp_root("symlinked-root");
        let real = base.join("real");
        fs::create_dir_all(&real).unwrap();
        let file = real.join("a.jsonl");
        fs::write(&file, b"x").unwrap();

        // `root_link` is itself a symlink to the real dir; candidate addressed through it.
        let root_link = base.join("root-link");
        symlink(&real, &root_link).unwrap();
        let candidate = root_link.join("a.jsonl");

        assert!(confined_path(&root_link, &candidate).is_ok());

        let _ = fs::remove_dir_all(&base);
    }
}
