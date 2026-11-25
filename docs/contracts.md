# Contract checks

Extension versions and `min_recall` use SemVer precedence, including prerelease stages and numeric sequence numbers.
For example, `1.0.0-rc.1` cannot satisfy `min_recall: 1.0.0`.
Prerelease extension releases do not enter the stable catalog.

Run `make check` to build the workspace and exercise its public contracts.
The command runs `python3 scripts/check-contracts target/debug/recall --extensions` after the Rust tests.
This smoke check uses a temporary HOME and an rx-agent fixture for CLI JSON/JSONL, MCP stdio, and extension manifests.
To test a downloaded core binary, run `python3 scripts/check-contracts /path/to/recall` from the repository.
The script removes its temporary index and fixture after the run.
The v1.0.0-rc.1 preparation run passed on macOS arm64. CI runs the same check on Linux.
