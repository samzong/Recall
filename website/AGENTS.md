# website/

Next.js + Fumadocs documentation site, statically exported to GitHub Pages.
Independent of the Rust workspace — use pnpm, never cargo or npm.

```bash
pnpm dev            # development server
pnpm build          # static export
pnpm lint           # eslint
pnpm types:check    # fumadocs-mdx + next typegen + tsc --noEmit
```

## Rules

- `public/extensions/catalog.json` is generated release state written by the
  extension release workflow. Never hand-edit version entries — it must
  contain real asset URLs and SHA-256 checksums.
- Docs content lives in `content/`; keep it aligned with actual CLI behavior.
