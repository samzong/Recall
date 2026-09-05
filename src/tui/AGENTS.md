# TUI

- Keep application state in `App` or a focused `*_state.rs` module, input
  handling in `event.rs`, terminal lifecycle in `runner.rs`, and drawing in
  `ui/`. Draw functions read state without mutating it.
- Define affected state transitions before editing. Reuse the existing
  state/input/render split for new panes, popups, and keys.
- Long-running work belongs in workers with message-based results; never
  block the event loop waiting for work or a channel response.
- Search responses must match both `active_search_id` and the current query
  before updating results (`App::apply_search_response`). Preserve request
  identity checks when adding asynchronous work so stale results cannot
  replace current state.
