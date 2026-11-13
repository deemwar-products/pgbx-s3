# pgbx docs site

Astro Starlight. Pages live in `src/content/docs/` (Markdown/MDX); the sidebar is in `astro.config.mjs`.

## Dev

```sh
cd site
npm ci
npm run dev        # http://localhost:4321/pgbx/
```

## Build

```sh
npm run build      # static site + Pagefind search index in dist/
npm run preview
```

`site` and `base` come from env:

| var | default |
|---|---|
| `PUBLIC_SITE_URL` | `https://example.github.io` |
| `PUBLIC_BASE_PATH` | `/pgbx` |

## Deploy (Cloudflare Pages)

Live at **https://pgbx.deemwar.com** (Cloudflare Pages project `pgbx`). `.github/workflows/pages.yml` (manual: *Run
workflow*) builds with `PUBLIC_SITE_URL=https://pgbx.deemwar.com PUBLIC_BASE_PATH=/` and deploys with wrangler, using
the repo secrets `CLOUDFLARE_API_TOKEN` and `CLOUDFLARE_ACCOUNT_ID`. By hand:

```sh
cd site && PUBLIC_SITE_URL=https://pgbx.deemwar.com PUBLIC_BASE_PATH=/ npm run build
npx wrangler pages deploy dist --project-name pgbx --branch main
```

The installers in `site/public` (`install.sh`, `install.ps1`, `install.cmd`) ship with the site, so a deploy also
publishes them.

Edit links are off until the repo exists: add `editLink: { baseUrl: 'https://github.com/<org>/pgbx/edit/main/site/' }`
to the Starlight config.

## Keep it true

Every function, setting and command here must match `src/lib.rs`, `cli/src/main.rs` and the README.
Change the code, change the page in the same PR.
