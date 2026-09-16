use super::*;
use std::path::Path;

fn os(argv: &[&str]) -> Vec<OsString> {
    argv.iter().map(OsString::from).collect()
}

#[cfg(windows)]
#[test]
#[ignore = "requires native Kimi Code and Python"]
fn native_kimi_lease_survives_launcher_exit() -> Result<()> {
    if let Some(home) = std::env::var_os("RX_TEST_KIMI_HOME") {
        let paths = Paths::in_dir(PathBuf::from(home).join(".recall"));
        let request = args::LaunchRequest {
            harness: args::Harness::Kimi,
            provider: Some(std::env::var("RX_TEST_KIMI_PROVIDER")?),
            passthrough: os(&[
                "--model",
                "model-a",
                if std::env::var_os("RX_TEST_KIMI_ACP").is_some() { "acp" } else { "--version" },
            ]),
        };
        let plan = crate::launch::plan(&request, &paths, &EnvLookup::real())?;
        crate::launch::exec(&plan)
    } else {
        let status = std::process::Command::new("python")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/probe-kimi-windows-leases.py"))
            .arg("--launcher")
            .arg(std::env::current_exe()?)
            .status()?;
        assert!(status.success(), "native Kimi lease probe failed: {status}");
        Ok(())
    }
}
