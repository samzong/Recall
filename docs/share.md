# Share Spec

## Goal

Let a user publish one Recall session to a browser-viewable URL. The current supported provider is Cloudflare Pages.

## Flow

1. The user runs `recall share init` once.
2. Recall checks that `wrangler` exists, is logged in, and can access Cloudflare Pages.
3. Recall asks for a Pages project name and a local publish directory.
4. In the TUI session view, the user presses `s`.
5. Recall writes one static HTML file to `<publish_dir>/<session_uuid>.html`.
6. Recall deploys the publish directory with Wrangler.
7. The TUI shows `https://<project_name>.pages.dev/<session_uuid>` for the user to copy.

## Scope

- Supported provider: Cloudflare Pages on `pages.dev`.
- Published unit: one session, one static HTML page.
- Re-publishing the same session UUID overwrites the same route.
- Wrangler work during TUI publish is hidden unless it fails.

## Page

- Show readable user and assistant messages.
- Collapse tool calls and tool results by default.
- Do not show local filesystem paths.

## Privacy

- The published page is public to anyone with the URL.
- Recall sets no-index headers and robots rules, but this is not access control.
- Auth is not supported now; if needed later, it may use another Cloudflare tool such as Workers.
- The user is responsible for choosing sessions that are safe to share.

## Non-Goals

- No share picker command.
- No list, revoke, or update command.
- No authentication or private access control.
- No provider abstraction beyond what is needed for the current Cloudflare Pages path.
