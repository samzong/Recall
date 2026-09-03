# Changelog


## [0.5.9](https://github.com/samzong/Recall/compare/v0.5.8...v0.5.9) (2026-09-03)


### Features

* **share:** add list and unpublish for published pages ([#210](https://github.com/samzong/Recall/issues/210))
* **recall:** add session continuation fallback ([#212](https://github.com/samzong/Recall/issues/212))
* **mcp:** expose session provenance ([#215](https://github.com/samzong/Recall/issues/215))
* **share:** improve session preview experience ([#219](https://github.com/samzong/Recall/issues/219))
* **mcp:** exclude the current session from discovery ([#224](https://github.com/samzong/Recall/issues/224))
* **events:** preserve structured tool event relationships ([#226](https://github.com/samzong/Recall/issues/226))
* **share:** render structured tool timelines ([#227](https://github.com/samzong/Recall/issues/227))
* **adapters:** add droid source adapter ([#232](https://github.com/samzong/Recall/issues/232))
* **adapters:** add Amp source adapter ([#231](https://github.com/samzong/Recall/issues/231))
* **adapters:** add OpenHands source adapter ([#229](https://github.com/samzong/Recall/issues/229))
* **mcp:** add cursor-agent MCP host ([#235](https://github.com/samzong/Recall/issues/235))


### Fixes

* **search:** preserve tokenizer migration compatibility ([#181](https://github.com/samzong/Recall/issues/181))
* **wrapped:** align metrics with indexed session data ([#214](https://github.com/samzong/Recall/issues/214))
* **kimi:** reject unstable composite snapshots ([#216](https://github.com/samzong/Recall/issues/216))
* **sync:** reconcile stale sessions safely ([#217](https://github.com/samzong/Recall/issues/217))
* **sync:** preserve session ids across refreshes ([#218](https://github.com/samzong/Recall/issues/218))
* **sync:** defer file-scan metadata writes ([#221](https://github.com/samzong/Recall/issues/221))
* **acceptance:** enforce search and release invariants ([#233](https://github.com/samzong/Recall/issues/233))
* **adapters:** recover sessions across storage formats ([#234](https://github.com/samzong/Recall/issues/234))


### Performance

* **sync:** stop event payload write amplification and show progress ([#211](https://github.com/samzong/Recall/issues/211))


### Refactors

* **sync:** restrict adapters to immutable sync context ([#222](https://github.com/samzong/Recall/issues/222))
* **rx:** simplify launcher internals ([#225](https://github.com/samzong/Recall/issues/225))

## Upgrade note

Before using `recall powercontext backfill` with a Recall release that exports
schema v6, run `recall ext upgrade powercontext` and verify that
`recall ext list` shows version 0.1.1 or later. `recall-powercontext` 0.1.0
accepts only schema v5.

## [0.5.8](https://github.com/samzong/Recall/compare/v0.5.7...v0.5.8) (2026-09-01)


### Features

* **mcp:** add file_history tool with per-file patch events ([#192](https://github.com/samzong/Recall/issues/192))
* **adapters:** index Cursor Agent CLI store.db sessions ([#195](https://github.com/samzong/Recall/issues/195))
* **adapters:** index Qwen Code project chat JSONL sessions ([#196](https://github.com/samzong/Recall/issues/196))
* **adapters:** index Kilo Code CLI kilo.db sessions ([#197](https://github.com/samzong/Recall/issues/197))
* **adapters:** index Crush project crush.db sessions ([#198](https://github.com/samzong/Recall/issues/198))
* **adapters:** index MiMo Code mimocode.db sessions ([#199](https://github.com/samzong/Recall/issues/199))
* **adapters:** index ZCode cli/db.sqlite sessions ([#200](https://github.com/samzong/Recall/issues/200))
* **adapters:** index OMP session JSONL ([#201](https://github.com/samzong/Recall/issues/201))
* **adapters:** scan Cline across editor hosts and add Roo ([#203](https://github.com/samzong/Recall/issues/203))
* **adapters:** index Kiro CLI v2/v3 session files ([#204](https://github.com/samzong/Recall/issues/204))
* **adapters:** index VS Code Chat sessions and CLI usage ([#205](https://github.com/samzong/Recall/issues/205))
* **adapters:** index Goose sessions.db ([#206](https://github.com/samzong/Recall/issues/206))
* **adapters:** index Cline CLI sessions on the cline source ([#207](https://github.com/samzong/Recall/issues/207))


### Fixes

* **rx:** support dsh reasoning effort controls ([#191](https://github.com/samzong/Recall/issues/191))


### Documentation

* **readme:** refresh project visuals ([#193](https://github.com/samzong/Recall/issues/193))
* **rx:** add hosting guide for embedders ([#194](https://github.com/samzong/Recall/issues/194))
* **adapters:** add ZCode to the support table ([#202](https://github.com/samzong/Recall/issues/202))
* **adapters:** add Goose to the architecture diagram ([#208](https://github.com/samzong/Recall/issues/208))


## [0.5.7](https://github.com/samzong/Recall/compare/v0.5.6...v0.5.7) (2026-08-28)


### Fixes

* **rx:** scope hosted harness env and harden dsh profile install ([#188](https://github.com/samzong/Recall/issues/188))


### Documentation

* **rx:** add launcher design contract and agent rules ([#189](https://github.com/samzong/Recall/issues/189))


## [0.5.6](https://github.com/samzong/Recall/compare/v0.5.5...v0.5.6) (2026-08-28)


### Features

* **rx:** add hosted gateway launch path ([#187](https://github.com/samzong/Recall/issues/187))


## [0.5.5](https://github.com/samzong/Recall/compare/v0.5.4...v0.5.5) (2026-08-28)


### Features

* **adapters:** open Copilot desktop sessions via ghapp ([#183](https://github.com/samzong/Recall/issues/183))
* **rx:** add Kimi Code launch path ([#185](https://github.com/samzong/Recall/issues/185))
* **rx:** launch harnesses from picker letter shortcuts ([#186](https://github.com/samzong/Recall/issues/186))


### Fixes

* **rx:** stabilize dsh installation ([#184](https://github.com/samzong/Recall/issues/184))


## [0.5.4](https://github.com/samzong/Recall/compare/v0.5.3...v0.5.4) (2026-08-27)


### Features

* **rx:** add max-permission launch defaults ([#175](https://github.com/samzong/Recall/issues/175))
* **rx:** add shell completions ([#177](https://github.com/samzong/Recall/issues/177))
* **mcp:** serve a read-only session index over stdio ([#178](https://github.com/samzong/Recall/issues/178))
* **recall:** add wrapped usage stats card ([#179](https://github.com/samzong/Recall/issues/179))
* **rx:** add 'none' provider to skip provider injection ([#180](https://github.com/samzong/Recall/issues/180))


### Fixes

* **rx:** include provider name on catalog fetch errors ([#169](https://github.com/samzong/Recall/issues/169))
* **rx:** exit successfully on update help
* **rx:** honor OpenCode long-form model flags
* **rx:** reject non-object Pi models.json roots ([#171](https://github.com/samzong/Recall/issues/171))
* **rx:** ignore option detectors after -- ([#172](https://github.com/samzong/Recall/issues/172))
* **rx:** persist update checks and serialize state writes ([#173](https://github.com/samzong/Recall/issues/173))
* **rx:** preserve non-UTF-8 arguments and executable paths ([#174](https://github.com/samzong/Recall/issues/174))
* **mcp:** preserve existing host registration ([#182](https://github.com/samzong/Recall/issues/182))


### Documentation

* **rx:** refresh architecture diagram ([#176](https://github.com/samzong/Recall/issues/176))


## [0.5.3](https://github.com/samzong/Recall/compare/v0.5.2...v0.5.3) (2026-08-24)


### Features

* **rx:** add provider model catalogs ([#165](https://github.com/samzong/Recall/issues/165))
* **rx:** add DeepSeek Harness support ([#166](https://github.com/samzong/Recall/issues/166))
* **search:** improve multi-term relevance ([#167](https://github.com/samzong/Recall/issues/167))
* **powercontext:** add recall backfill extension ([#168](https://github.com/samzong/Recall/issues/168))


## [0.5.2](https://github.com/samzong/Recall/compare/v0.5.1...v0.5.2) (2026-08-24)


### Features

* **rx:** add provider management ([#162](https://github.com/samzong/Recall/issues/162))


### Fixes

* **rx:** reconcile seeded catalog ownership ([#163](https://github.com/samzong/Recall/issues/163))
* **tui:** preserve contrast in light terminals ([#164](https://github.com/samzong/Recall/issues/164))


## [0.5.1](https://github.com/samzong/Recall/compare/v0.5.0...v0.5.1) (2026-08-23)


### Features

* **adapters:** add kimi-code source adapter ([#141](https://github.com/samzong/Recall/issues/141))
* **rx:** add rx gateway launcher for agent harnesses ([#146](https://github.com/samzong/Recall/issues/146))
* **rx:** install missing harnesses and seed Claude catalogs ([#147](https://github.com/samzong/Recall/issues/147))
* **rx:** allow named gateway profiles over shared drivers ([#148](https://github.com/samzong/Recall/issues/148))


### Fixes

* **sync:** skip command envelopes when deriving session titles ([#142](https://github.com/samzong/Recall/issues/142))
* **rx:** harden profile keys and release dry runs ([#149](https://github.com/samzong/Recall/issues/149))
* **rx:** align version identity and serialize catalog seeds ([#150](https://github.com/samzong/Recall/issues/150))


### Refactors

* **db:** derive scope message totals from session counts ([#144](https://github.com/samzong/Recall/issues/144))


### Documentation

* **trace:** add recall trace CLI design proposal ([#143](https://github.com/samzong/Recall/issues/143))


## [0.5.0](https://github.com/samzong/Recall/compare/v0.4.0...v0.5.0) (2026-08-14)


### Features

* **adapters:** add DeepSeek Harness support ([#129](https://github.com/samzong/Recall/issues/129))


### Fixes

* **deps:** close known RustSec vulnerabilities ([#130](https://github.com/samzong/Recall/issues/130))
* **release:** gate builds on validated tags ([#131](https://github.com/samzong/Recall/issues/131))
* **config:** fail closed on invalid settings ([#132](https://github.com/samzong/Recall/issues/132))
* **sync:** honor excluded transcript paths ([#133](https://github.com/samzong/Recall/issues/133))
* **export:** read JSONL from one snapshot ([#134](https://github.com/samzong/Recall/issues/134))
* **export:** replace JSONL targets atomically ([#135](https://github.com/samzong/Recall/issues/135))
* **share:** prevent deploying unmanaged directory contents ([#136](https://github.com/samzong/Recall/issues/136))
* **cli:** reject invalid time filters ([#137](https://github.com/samzong/Recall/issues/137))
* **search:** bound semantic pagination ([#138](https://github.com/samzong/Recall/issues/138))
* **info:** read indexed source stats ([#139](https://github.com/samzong/Recall/issues/139))
* **tui:** move usage refresh off event loop ([#140](https://github.com/samzong/Recall/issues/140))


### Documentation

* **changelog:** back-fill history and the 0.4.0 upgrade note ([#126](https://github.com/samzong/Recall/issues/126))


### revert

* **release:** put version choice back in the maintainer's hands ([#128](https://github.com/samzong/Recall/issues/128))

## [0.4.0](https://github.com/samzong/Recall/compare/v0.3.0...v0.4.0) (2026-08-11)


### ⚠ BREAKING CHANGES

* **scope:** unify project scope across CLI, TUI, and sync ([#118](https://github.com/samzong/Recall/issues/118))

Upgrading from 0.3.x: commands that take a session set — `recall search`,
`recall session list`, `recall export`, `recall sync` — no longer cover every
project by default. Inside a Git checkout they resolve to that project,
including its other worktrees; pass `--project all` for the previous global
behaviour. `--repo` is deprecated in favour of `--project`, which now also
accepts `owner/repo` and remote URLs. `protocol_version` is `2`: extensions and
scripts relying on the flagless global scope must be updated. The TUI setting
`default_current_repo_scope` is removed; the TUI follows the same automatic
scope as the CLI.

### Features

* **bench:** add CodSpeed performance benchmarks ([#117](https://github.com/samzong/Recall/issues/117)) ([29bbdde](https://github.com/samzong/Recall/commit/29bbdde0dee589c7a3dc0940ae478d75b4cd21ad))
* **reflect:** add broader reflection signals ([#108](https://github.com/samzong/Recall/issues/108)) ([de6db94](https://github.com/samzong/Recall/commit/de6db943b05e61d92648e024150a1731403d4e63))
* **reflect:** add scope kind to reports ([19b6474](https://github.com/samzong/Recall/commit/19b647400c94474271bc2f60603fe2f3f29dfd50))
* **reflect:** support personal scope resolution ([4a7a561](https://github.com/samzong/Recall/commit/4a7a561fa55e3f903afade3f80deff809d6e7b37))
* **scope:** unify project scope across CLI, TUI, and sync ([#118](https://github.com/samzong/Recall/issues/118)) ([fc764f9](https://github.com/samzong/Recall/commit/fc764f933a7ad0be293797c65b385cee04504e28))
* **session:** index portable session topology ([#115](https://github.com/samzong/Recall/issues/115)) ([1493329](https://github.com/samzong/Recall/commit/149332962bd197a3f472c2bc4071a2a9e1ef424a))
* **tui:** navigate subagents and show session lineage ([#116](https://github.com/samzong/Recall/issues/116)) ([e0111cd](https://github.com/samzong/Recall/commit/e0111cd47028c65e5606be07a638453d636b90ce))


### Fixes

* **bench:** measure steady state, and pin the measurement environment ([#119](https://github.com/samzong/Recall/issues/119)) ([8925d5b](https://github.com/samzong/Recall/commit/8925d5b84918504516c181d06d169324da0d5bd5))
* **clipboard:** fall back wl-copy -&gt; xclip -&gt; xsel on Linux ([#111](https://github.com/samzong/Recall/issues/111)) ([6b2f21f](https://github.com/samzong/Recall/commit/6b2f21ff544fc8a7a6ab8babe1d2b87482dca975))
* **reflect:** align personal scope polish ([6e6d33c](https://github.com/samzong/Recall/commit/6e6d33c3e6511ac85021b73091852e0eb3341e59))
* **release:** scope release-please to the root package and pre-1.0 bumps ([#125](https://github.com/samzong/Recall/issues/125)) ([42172b0](https://github.com/samzong/Recall/commit/42172b02dcd7149b61faef2161be34056acfa94d))
* **utils:** satisfy workspace clippy ([5ee941f](https://github.com/samzong/Recall/commit/5ee941f09e75854526a16c41f41bd1810edd77ff))


### Performance

* **cursor:** stop rebuilding global metadata once per session ([#121](https://github.com/samzong/Recall/issues/121)) ([396b036](https://github.com/samzong/Recall/commit/396b03636c0da8db8a4d270dc5b541c2bdedbac2))
* **sync:** account for what each adapter actually costs ([#120](https://github.com/samzong/Recall/issues/120)) ([0570dde](https://github.com/samzong/Recall/commit/0570ddeff7e175b7c513e8f09e97829cdce93a41))


### Refactors

* **adapters:** extract shared json and path helpers ([#103](https://github.com/samzong/Recall/issues/103)) ([f295faf](https://github.com/samzong/Recall/commit/f295fafcc2fcbf269e19f4dbd8818984794ad01e))
* **adapters:** extract shared session_state_is_current helper ([#105](https://github.com/samzong/Recall/issues/105)) ([8293fca](https://github.com/samzong/Recall/commit/8293fca2b85a28a532c6adfac33dc98bc9606c0c))
* **adapters:** extract shared usage helpers ([#104](https://github.com/samzong/Recall/issues/104)) ([35c8917](https://github.com/samzong/Recall/commit/35c89172b994d91176cf3cf95e9bca3c05bb5600))
* **sync:** extract SyncJob for session sync orchestration ([#106](https://github.com/samzong/Recall/issues/106)) ([6714f61](https://github.com/samzong/Recall/commit/6714f61c86f7268f1c65aa253ce3bbe1738c7dfd))
* **tui:** centralize UI colors in Theme tokens ([#114](https://github.com/samzong/Recall/issues/114)) ([22625bf](https://github.com/samzong/Recall/commit/22625bf8979a185d9913c1bfb20290ac466d492b))
* **tui:** consolidate picker state and cursor helpers ([#102](https://github.com/samzong/Recall/issues/102)) ([cdbe356](https://github.com/samzong/Recall/commit/cdbe3568776c89685e0e83b4890d78e9e0ef57b7))


### Documentation

* **agents:** add module-level guidance files ([#101](https://github.com/samzong/Recall/issues/101)) ([44debb4](https://github.com/samzong/Recall/commit/44debb41c73767ad18c6a0ea8928297adc6e9092))
* **reflect:** clarify phased scope roadmap ([9ed0f68](https://github.com/samzong/Recall/commit/9ed0f689a647bab7ce9c2b5666fea131dcadc036))
* **reflect:** convert design to KEP format ([ebd38f1](https://github.com/samzong/Recall/commit/ebd38f196e21eaf0af2988057125f5c47c851548))

## [0.3.0](https://github.com/samzong/Recall/compare/v0.2.10...v0.3.0) (2026-07-08)


### Features

* Seed viewing search from list query ([#80](https://github.com/samzong/Recall/issues/80))
* Add core protocol outputs ([#85](https://github.com/samzong/Recall/issues/85))
* Add cli list and external dispatch ([#89](https://github.com/samzong/Recall/issues/89))
* Add official extension release flow ([#92](https://github.com/samzong/Recall/issues/92))
* Manage official extensions ([#94](https://github.com/samzong/Recall/issues/94))
* Add jsonl include projection ([#97](https://github.com/samzong/Recall/issues/97))
* Add extension recall-reflect ([#83](https://github.com/samzong/Recall/issues/83))


### Fixes

* Skip semantic worker in debug builds ([#84](https://github.com/samzong/Recall/issues/84))
* Make session refresh atomic ([#88](https://github.com/samzong/Recall/issues/88))
* Keep extension releases out of latest ([#93](https://github.com/samzong/Recall/issues/93))
* Fill selected result row highlight ([#91](https://github.com/samzong/Recall/issues/91))
* Support page scrolling in session panes ([#90](https://github.com/samzong/Recall/issues/90))
* Capture scrollbar drag ([#96](https://github.com/samzong/Recall/issues/96))
* Set release repo context ([#99](https://github.com/samzong/Recall/issues/99))


### Refactors

* Slim pub surface and split large modules ([#81](https://github.com/samzong/Recall/issues/81))
* Simplify extension metadata ([#95](https://github.com/samzong/Recall/issues/95))


### Documentation

* Add agent guidance and align development docs ([#82](https://github.com/samzong/Recall/issues/82))


## [0.2.10](https://github.com/samzong/Recall/compare/v0.2.9...v0.2.10) (2026-07-06)


### Features

* Add scroll anchoring, mouse selection, and scrollbars ([#78](https://github.com/samzong/Recall/issues/78))


### Fixes

* Keep selected session visible ([#68](https://github.com/samzong/Recall/issues/68))
* Clarify shortcut hints for search and detail views ([#71](https://github.com/samzong/Recall/issues/71))


## [0.2.9](https://github.com/samzong/Recall/compare/v0.2.8...v0.2.9) (2026-07-06)


### Features

* Overhaul session page renderer with markdown and editorial layout ([#56](https://github.com/samzong/Recall/issues/56))
* Add repo identity scope for list, search, and export ([#59](https://github.com/samzong/Recall/issues/59))
* Add agent handoff flow ([#58](https://github.com/samzong/Recall/issues/58))
* Install bundled agent skill ([#60](https://github.com/samzong/Recall/issues/60))
* Default sessions to current repo scope ([#61](https://github.com/samzong/Recall/issues/61))


### Fixes

* Purge orphan grok usage events on migrate ([#64](https://github.com/samzong/Recall/issues/64))
* Exclude subagent sessions from indexing ([#63](https://github.com/samzong/Recall/issues/63))
* Parse token usage from session updates ([#65](https://github.com/samzong/Recall/issues/65))


### Documentation

* Add share link intent routing to recall skill ([#57](https://github.com/samzong/Recall/issues/57))
* Refine recall skill project lookup
* Add chinese readme with language switch ([#66](https://github.com/samzong/Recall/issues/66))


## [0.2.8](https://github.com/samzong/Recall/compare/v0.2.7...v0.2.8) (2026-06-16)


### Features

* Add CLI session workflows ([#51](https://github.com/samzong/Recall/issues/51))
* Add session preview rendering ([#53](https://github.com/samzong/Recall/issues/53))
* Add shell completions ([#54](https://github.com/samzong/Recall/issues/54))
* Move search to background worker with deferred filters ([#55](https://github.com/samzong/Recall/issues/55))


### Fixes

* Use wrangler project domain for share URLs ([#50](https://github.com/samzong/Recall/issues/50))


### Refactors

* Split session command modules ([#52](https://github.com/samzong/Recall/issues/52))


## [0.2.7](https://github.com/samzong/Recall/compare/v0.2.6...v0.2.7) (2026-06-11)


### Features

* Publish sessions to Cloudflare Pages ([#49](https://github.com/samzong/Recall/issues/49))


### Fixes

* Restore agent transcript project attribution ([#47](https://github.com/samzong/Recall/issues/47))
* Default to latest active sessions ([#48](https://github.com/samzong/Recall/issues/48))


## [0.2.6](https://github.com/samzong/Recall/compare/v0.2.5...v0.2.6) (2026-06-10)


### Features

* Show session detail summary ([#42](https://github.com/samzong/Recall/issues/42))
* Add JSONL import ([#43](https://github.com/samzong/Recall/issues/43))


### Fixes

* Improve export help ([#45](https://github.com/samzong/Recall/issues/45))


### Documentation

* Add recall project memory skill ([#41](https://github.com/samzong/Recall/issues/41))
* Update export usage ([#46](https://github.com/samzong/Recall/issues/46))


## [0.2.5](https://github.com/samzong/Recall/compare/v0.2.4...v0.2.5) (2026-06-05)


### Features

* Add Recall website ([#36](https://github.com/samzong/Recall/issues/36))
* Opt-out sessions from indexing via excluded_paths globs ([#38](https://github.com/samzong/Recall/issues/38))


### Fixes

* Bump openssl, rustls-webpki, rand for CVE remediation ([#35](https://github.com/samzong/Recall/issues/35))
* Durable cwd fallback + parse custom-title, summary, duration ([#37](https://github.com/samzong/Recall/issues/37))
* Dedupe adjacent duplicate assistant messages ([#40](https://github.com/samzong/Recall/issues/40))


### Documentation

* Expand repo description to reflect current feature scope


## [0.2.4](https://github.com/samzong/Recall/compare/v0.2.2...v0.2.4) (2026-05-30)


### Features

* Add project directory filter ([#27](https://github.com/samzong/Recall/issues/27))
* Add machine-readable session export ([#28](https://github.com/samzong/Recall/issues/28))
* Index session events ([#30](https://github.com/samzong/Recall/issues/30))
* Open Codex sessions in app ([#32](https://github.com/samzong/Recall/issues/32))


## [0.2.2](https://github.com/samzong/Recall/compare/v0.2.1...v0.2.2) (2026-05-25)


### Features

* Add homebrew tap update workflow
* Add Pi usage support ([#25](https://github.com/samzong/Recall/issues/25))


### Documentation

* Update README.md


## [0.2.1](https://github.com/samzong/Recall/compare/v0.2.0...v0.2.1) (2026-05-24)


### Features

* Add OpenCode token usage ([#22](https://github.com/samzong/Recall/issues/22))
* Add usage dashboard ([#23](https://github.com/samzong/Recall/issues/23))


### Fixes

* Improve usage CLI filtering and display ([#21](https://github.com/samzong/Recall/issues/21))


## [0.2.0](https://github.com/samzong/Recall/compare/v0.1.6...v0.2.0) (2026-05-22)


### Features

* Add efficient filter controls ([#17](https://github.com/samzong/Recall/issues/17))
* Add Cline adapter ([#18](https://github.com/samzong/Recall/issues/18))
* Unify filter controls ([#19](https://github.com/samzong/Recall/issues/19))


### Documentation

* Update support matrix for Cline


## [0.1.6](https://github.com/samzong/Recall/compare/v0.1.5...v0.1.6) (2026-05-20)


### Features

* Add search eval harness and tune hybrid retrieval ([#11](https://github.com/samzong/Recall/issues/11))
* Add Cursor IDE adapter ([#15](https://github.com/samzong/Recall/issues/15))
* Enable resume command using --resume flag
* Add Antigravity CLI source ([#16](https://github.com/samzong/Recall/issues/16))


### Documentation

* Add positioning intro and TUI capability rows
* Add architecture diagram to README


## [0.1.5](https://github.com/samzong/Recall/compare/v0.1.4...v0.1.5) (2026-04-13)


### Performance

* Make OpenCode sync incremental and restore safe scans ([#7](https://github.com/samzong/Recall/issues/7))
* Make sync incremental via shared file_scan helper ([#8](https://github.com/samzong/Recall/issues/8))
* Make sync incremental via shared file_scan helper ([#10](https://github.com/samzong/Recall/issues/10))


### Refactors

* Make metal default on macos via target-specific deps ([#6](https://github.com/samzong/Recall/issues/6))


## [0.1.4](https://github.com/samzong/Recall/compare/v0.1.3...v0.1.4) (2026-04-11)


### ⚠ BREAKING CHANGES

* Merge index into sync with --force flag


## [0.1.3](https://github.com/samzong/Recall/compare/v0.1.2...v0.1.3) (2026-04-11)


### Features

* Add Gemini CLI and Kiro CLI adapters ([#2](https://github.com/samzong/Recall/issues/2))
* Add GitHub Copilot CLI adapter ([#4](https://github.com/samzong/Recall/issues/4))


### Performance

* Enable metal on macOS, fix tokenizer/batch overhead ([#5](https://github.com/samzong/Recall/issues/5))


## [0.1.1](https://github.com/samzong/Recall/compare/v0.1.0...v0.1.1) (2026-04-09)


### Features

* Add persistent settings and semantic queue


### Refactors

* Replace in-process semantic worker with background subprocess


## [0.1.0](https://github.com/samzong/Recall/releases/tag/v0.1.0) (2026-04-09)
