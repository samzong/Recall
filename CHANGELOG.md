# Changelog

## [0.4.0](https://github.com/samzong/Recall/compare/v0.3.0...v0.4.0) (2026-08-11)


### ⚠ BREAKING CHANGES

* **scope:** unify project scope across CLI, TUI, and sync ([#118](https://github.com/samzong/Recall/issues/118))

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
