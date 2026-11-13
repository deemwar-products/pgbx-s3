// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

// Served at https://pgbx.deemwar.com (Cloudflare Pages project `pgbx`); local dev keeps /pgbx unless PUBLIC_BASE_PATH is set
const site = process.env.PUBLIC_SITE_URL || 'https://example.github.io';
const base = process.env.PUBLIC_BASE_PATH || '/pgbx';

export default defineConfig({
  site,
  base,
  trailingSlash: 'always',
  integrations: [
    starlight({
      title: 'pgbx',
      description: "Never lose your app's database. Automatic Postgres backups to your own S3 bucket.",
      components: {
        Head: './src/components/ProductHead.astro',
        Hero: './src/components/Hero.astro',
      },
      // editLink: add { baseUrl: 'https://github.com/<org>/pgbx/edit/main/site/' } once the repo exists
      sidebar: [
        { label: 'Getting started', items: [{ autogenerate: { directory: 'getting-started' } }] },
        { label: 'Concepts', items: [{ autogenerate: { directory: 'concepts' } }] },
        { label: 'Guides', items: [{ autogenerate: { directory: 'guides' } }] },
        { label: 'Reference', items: [{ autogenerate: { directory: 'reference' } }] },
        { label: 'For AI agents', items: [{ autogenerate: { directory: 'agents' } }] },
        { label: 'Testing & guarantees', slug: 'testing' },
        { label: 'Release notes', slug: 'releases' },
      ],
    }),
  ],
});
