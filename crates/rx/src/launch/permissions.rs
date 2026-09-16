use std::ffi::OsString;

use super::{EnvLookup, LaunchPlan, opencode_flag_index};
use crate::args::{self, Harness, LaunchRequest};

pub(crate) fn yolo_enabled(env: &EnvLookup) -> bool {
    env.get("RX_NO_YOLO").is_none()
}

pub(super) fn apply_yolo(request: &LaunchRequest, plan: &mut LaunchPlan, env: &EnvLookup) {
    if !yolo_enabled(env) {
        return;
    }
    let passthrough = args::before_double_dash(&request.passthrough);
    let (flags, at): (&[&str], usize) = match request.harness {
        Harness::Claude => {
            if args::has_flags(
                passthrough,
                &["--dangerously-skip-permissions", "--permission-mode"],
            ) {
                return;
            }
            (&["--dangerously-skip-permissions"], 0)
        }
        Harness::Codex => {
            if args::has_flags(passthrough, &["--sandbox", "--ask-for-approval"])
                || codex_user_sets_sandbox(passthrough)
            {
                return;
            }
            (
                &["--sandbox", "danger-full-access", "--ask-for-approval", "never"],
                plan.args.len().saturating_sub(request.passthrough.len()),
            )
        }
        Harness::OpenCode => {
            if args::has_flags(passthrough, &["--auto"]) {
                return;
            }
            let Some(at) = opencode_flag_index(&plan.args) else { return };
            (&["--auto"], at)
        }
        Harness::Kimi => {
            if args::has_flags(
                passthrough,
                &["--auto", "--yolo", "-y", "--yes", "--auto-approve", "--prompt", "-p"],
            ) {
                return;
            }
            (&["--auto"], 0)
        }
        Harness::Pi | Harness::Dsh => return,
    };
    plan.args.splice(at..at, flags.iter().map(OsString::from));
    let note = format!(
        "[rx] yolo: max permissions for {} via {} (RX_NO_YOLO=1 disables)",
        request.harness.as_str(),
        flags.join(" ")
    );
    plan.stderr_note = Some(match plan.stderr_note.take() {
        Some(existing) => format!("{existing}\n{note}"),
        None => note,
    });
}

fn codex_user_sets_sandbox(passthrough: &[OsString]) -> bool {
    let mut args = passthrough.iter();
    while let Some(arg) = args.next() {
        if (arg == "-c" || arg == "--config")
            && args.next().and_then(|value| value.to_str()).is_some_and(|value| {
                value.starts_with("sandbox_mode=") || value.starts_with("approval_policy=")
            })
        {
            return true;
        }
    }
    false
}
