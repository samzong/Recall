use std::process::{Command, Stdio};

use anyhow::Result;

use crate::adapters::{self, ResumeCommand};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Resume,
    OpenApp,
}

impl SessionAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Resume => "resume",
            Self::OpenApp => "open",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Resume => "Resume",
            Self::OpenApp => "Open in app",
        }
    }
}

pub fn command_for(action: SessionAction, source: &str, source_id: &str) -> Option<ResumeCommand> {
    match action {
        SessionAction::Resume => adapters::resume_command_for(source, source_id),
        SessionAction::OpenApp => adapters::app_command_for(source, source_id),
    }
}

pub fn run(command: &ResumeCommand, directory: Option<&str>) -> Result<()> {
    let mut process = Command::new(&command.program);
    process
        .args(&command.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(directory) = directory {
        process.current_dir(directory);
    }
    let status = process.status()?;
    if !status.success() {
        anyhow::bail!("command exited with status {status}");
    }
    Ok(())
}
