use std::io::{IsTerminal, Write};

use anyhow::Result;

use crate::db::store::Store;

pub(crate) fn run_reset(yes: bool) -> Result<()> {
    if !yes {
        if !std::io::stdin().is_terminal() {
            anyhow::bail!("reset requires --yes when not attached to a terminal");
        }
        print!("Delete ALL indexed Recall data? This cannot be undone [y/N]: ");
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y") {
            println!("Aborted.");
            return Ok(());
        }
    }
    Store::open()?.reset_all_data()?;
    println!("All indexed data cleared.");
    Ok(())
}

pub(crate) fn run_vacuum() -> Result<()> {
    let path = Store::db_path()?;
    let before = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Store::open()?.vacuum()?;
    let after = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!(
        "Vacuumed {} -> {} ({} reclaimed)",
        crate::utils::humanize_bytes(before),
        crate::utils::humanize_bytes(after),
        crate::utils::humanize_bytes(before.saturating_sub(after)),
    );
    Ok(())
}
