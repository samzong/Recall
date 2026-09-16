use std::path::PathBuf;

use crate::config::Paths;
use crate::launch::EnvLookup;

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Residue {
    Absent,
    Removed,
    Unowned(PathBuf),
    Modified(PathBuf),
    Blocked(String),
    Failed(String),
}

impl Residue {
    fn cleared(&self) -> bool {
        matches!(self, Residue::Absent | Residue::Removed)
    }
}

struct Surface {
    harness: &'static str,
    holds_credential: bool,
    residue: Residue,
}

pub(crate) struct Report {
    surfaces: Vec<Surface>,
}

impl Report {
    pub(crate) fn credential_retained(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.holds_credential && !surface.residue.cleared())
    }

    pub(crate) fn removed(&self) -> bool {
        self.surfaces.iter().any(|surface| surface.residue == Residue::Removed)
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        self.surfaces.iter().filter_map(Surface::note).collect()
    }
}

impl Surface {
    fn note(&self) -> Option<String> {
        let harness = self.harness;
        match &self.residue {
            Residue::Absent | Residue::Removed => None,
            Residue::Unowned(path) => Some(format!(
                "{harness}: {} holds entries from an rx version that kept no ownership record; remove them by hand",
                path.display()
            )),
            Residue::Modified(path) => Some(format!(
                "{harness}: entries in {} no longer match what rx wrote and were kept; remove them by hand",
                path.display()
            )),
            Residue::Blocked(reason) => Some(format!("{harness}: {reason}")),
            Residue::Failed(error) => Some(format!("{harness}: cleanup failed: {error}")),
        }
    }
}

pub(crate) fn purge(provider_id: &str, paths: &Paths, env: &EnvLookup) -> Report {
    let surfaces = vec![
        surface("kimi", true, crate::kimi::purge(provider_id, paths, env)),
        surface("claude", false, crate::claude_catalog::purge(provider_id, env)),
        surface("pi", false, crate::pi::purge(provider_id, env)),
        surface("dsh", false, crate::dsh::purge(provider_id, paths)),
        surface("catalogs", false, crate::catalog::purge(provider_id, paths)),
    ];
    Report { surfaces }
}

fn surface(
    harness: &'static str,
    holds_credential: bool,
    result: anyhow::Result<Residue>,
) -> Surface {
    let residue = result.unwrap_or_else(|error| Residue::Failed(format!("{error:#}")));
    Surface { harness, holds_credential, residue }
}
