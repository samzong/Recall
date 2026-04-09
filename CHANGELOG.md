# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-04-09

### Added

- Hybrid search engine: FTS5 full-text + sqlite-vec vector KNN with RRF fusion
- Source adapters: Claude Code, OpenCode, Codex
- TUI with dual-panel layout, session preview, full-screen viewing, export
- CLI subcommands: `index`, `sync`, `search`
- Local embedding via fastembed (MultilingualE5Small)
- SQLite storage with FTS5 and sqlite-vec extensions
