# src/tui/

ratatui application. Module roles:

- `runner.rs` — terminal lifecycle and the event loop.
- `app.rs` — the `App` state struct; all state transitions happen here or in
  `event.rs`. This is the state machine; define transitions before editing.
- `event.rs` — key/input handling that mutates `App`.
- `search_state.rs`, `share_state.rs`, `usage_state.rs`, `viewing_state.rs` —
  per-concern state, kept out of `app.rs` when self-contained.
- `search_worker.rs` — background search thread connected by mpsc channels.
- `layout.rs`, `text_layout.rs` — geometry and text wrapping.
- `ui/` — rendering only. Draw functions read state; they must not mutate it.

## Search worker protocol

Searches run off the UI thread in two phases (`Text`, then `Hybrid` when the
semantic index is ready). Every `SearchRequest` carries an id; `App` keeps
`active_search_id` and drops any response whose id or query no longer matches
(`app.rs`). Preserve this discipline when adding async work: tag requests,
compare on receipt, never block the event loop on a channel.

## Rules

- New popups, panes, or key handling follow the existing split: state in a
  `*_state.rs` module or `App`, input in `event.rs`, drawing in `ui/`.
- Long-running work goes through a worker thread and messages, never inline in
  the event loop.
