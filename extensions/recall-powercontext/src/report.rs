use std::collections::BTreeMap;
use std::fmt::{self, Display, Write};

use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Counts {
    pub imported: u64,
    pub skipped: u64,
    pub conflicts: u64,
    pub failed: u64,
}

impl Counts {
    pub fn add(&mut self, kind: CountKind) {
        match kind {
            CountKind::Imported => self.imported += 1,
            CountKind::Skipped => self.skipped += 1,
            CountKind::Conflict => self.conflicts += 1,
            CountKind::Failed => self.failed += 1,
        }
    }

    fn total(self) -> u64 {
        self.imported + self.skipped + self.conflicts + self.failed
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CountKind {
    Imported,
    Skipped,
    Conflict,
    Failed,
}

impl CountKind {
    fn label(self) -> &'static str {
        match self {
            Self::Imported => "Imported",
            Self::Skipped => "Skipped",
            Self::Conflict => "Conflicts",
            Self::Failed => "Failed",
        }
    }

    fn value(self, counts: Counts) -> u64 {
        match self {
            Self::Imported => counts.imported,
            Self::Skipped => counts.skipped,
            Self::Conflict => counts.conflicts,
            Self::Failed => counts.failed,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BackfillReport {
    pub scope_id: String,
    pub server_url: String,
    pub dry_run: bool,
    pub time: String,
    pub roles: String,
    pub totals: Counts,
    pub by_adapter: Vec<AdapterCounts>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterCounts {
    pub adapter: String,
    #[serde(flatten)]
    pub counts: Counts,
}

impl Display for BackfillReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let total = self.totals.total();
        let noun = if total == 1 { "source" } else { "sources" };
        if self.dry_run {
            writeln!(f, "Dry run: {total} {noun} selected")?;
        } else if total == 0 {
            writeln!(f, "Backfill complete: no matching sources")?;
        } else if self.totals.failed > 0 {
            writeln!(f, "Backfill finished: {total} {noun} processed")?;
        } else if total == self.totals.imported {
            writeln!(f, "Backfill complete: {total} {noun} imported")?;
        } else {
            writeln!(f, "Backfill complete: {total} {noun} processed")?;
        }
        writeln!(f, "Scope: {}", self.scope_id.strip_prefix("git:").unwrap_or(&self.scope_id))?;

        if self.by_adapter.is_empty() {
            return Ok(());
        }

        let mut columns = vec![CountKind::Imported];
        for kind in [CountKind::Skipped, CountKind::Conflict, CountKind::Failed] {
            if kind.value(self.totals) > 0 {
                columns.push(kind);
            }
        }
        let adapter_width = self
            .by_adapter
            .iter()
            .map(|row| row.adapter.chars().count())
            .max()
            .unwrap_or(0)
            .max("Adapter".len())
            + 2;

        f.write_char('\n')?;
        write!(f, "{:<adapter_width$}", "Adapter")?;
        for kind in &columns {
            write!(f, "{:>11}", kind.label())?;
        }
        f.write_char('\n')?;
        for row in &self.by_adapter {
            write!(f, "{:<adapter_width$}", row.adapter)?;
            for kind in &columns {
                write!(f, "{:>11}", kind.value(row.counts))?;
            }
            f.write_char('\n')?;
        }
        write!(f, "{:<adapter_width$}", "Total")?;
        for kind in columns {
            write!(f, "{:>11}", kind.value(self.totals))?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct CountSink {
    totals: Counts,
    by_adapter: BTreeMap<String, Counts>,
}

impl CountSink {
    pub fn add(&mut self, adapter: &str, kind: CountKind) {
        self.totals.add(kind);
        self.by_adapter.entry(adapter.to_string()).or_default().add(kind);
    }

    pub fn finish(
        self,
        scope_id: String,
        server_url: String,
        dry_run: bool,
        roles: String,
        time: String,
    ) -> BackfillReport {
        let by_adapter = self
            .by_adapter
            .into_iter()
            .map(|(adapter, counts)| AdapterCounts { adapter, counts })
            .collect();
        BackfillReport {
            scope_id,
            server_url,
            dry_run,
            time,
            roles,
            totals: self.totals,
            by_adapter,
        }
    }
}
