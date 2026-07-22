import { defineConfig } from 'vitepress'
import { existsSync, readFileSync, readdirSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import path from 'node:path'
import { genFeed } from './genFeed'

// --- Draft pages ----------------------------------------------------
// A page with `draft: true` in its frontmatter is hidden from the
// production build — no route, no sidebar entry, no blog listing, no
// feed item — but still renders under `vitepress dev` so it can be
// previewed and written. Flip everything to draft, publish when ready.
const srcDir = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..')
const HIDE_DRAFTS = process.env.NODE_ENV === 'production'

function walkMarkdown(dir: string): string[] {
  if (!existsSync(dir)) return []
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = path.join(dir, e.name)
    if (e.isDirectory()) return walkMarkdown(p)
    return e.isFile() && p.endsWith('.md') ? [p] : []
  })
}

function isDraft(absPath: string): boolean {
  const m = readFileSync(absPath, 'utf-8').match(/^---\r?\n([\s\S]*?)\r?\n---/)
  return m ? /^draft:\s*(true|yes)\s*$/m.test(m[1]) : false
}

// File path (relative to srcDir) → clean route, e.g.
// docs/guides/foo.md → /docs/guides/foo, docs/guides/index.md → /docs/guides
function routeOf(rel: string): string {
  const noExt = rel.replace(/\.md$/, '').replace(/\\/g, '/')
  return ('/' + noExt.replace(/(^|\/)index$/, '$1')).replace(/\/$/, '') || '/'
}

const norm = (link: string) =>
  ('/' + link).replace(/\/{2,}/g, '/').replace(/\.html$/, '').replace(/\/$/, '') ||
  '/'

const draftFiles: string[] = []
const draftRoutes = new Set<string>()
if (HIDE_DRAFTS) {
  for (const abs of [
    ...walkMarkdown(path.join(srcDir, 'docs')),
    ...walkMarkdown(path.join(srcDir, 'blog')),
  ]) {
    if (isDraft(abs)) {
      const rel = path.relative(srcDir, abs)
      draftFiles.push(rel)
      draftRoutes.add(routeOf(rel))
    }
  }
}

// Remove draft entries (and any group/dropdown left empty) from nav or
// sidebar item lists.
function pruneItems(items: any[]): any[] {
  return items
    .map((item) => {
      if (item.items) {
        const kids = pruneItems(item.items)
        return kids.length ? { ...item, items: kids } : null
      }
      return item.link && draftRoutes.has(norm(item.link)) ? null : item
    })
    .filter(Boolean)
}

function pruneSidebar(sidebar: Record<string, any[]>) {
  return Object.fromEntries(
    Object.entries(sidebar).map(([k, v]) => [k, pruneItems(v)]),
  )
}

export default defineConfig({
  // Draft pages: excluded from the production build; links to them are
  // ignored so a still-linked draft doesn't fail the build.
  srcExclude: draftFiles,
  ignoreDeadLinks: HIDE_DRAFTS
    ? [(link: string) => draftRoutes.has(norm(link))]
    : false,

  title: 'bougie',
  description:
    'PHP toolchain management, the luxury way. A Composer-compatible package manager, PHP version manager, dev services and web server in one fast binary.',
  cleanUrls: true,
  lastUpdated: true,
  sitemap: { hostname: 'https://bougie.tools' },

  // The landing's hand-built top-bar/footer link to /docs/, which is a
  // draft-controlled route the theme nav can't reach. Expose whether the
  // docs landing is published so those links can hide themselves.
  vite: {
    define: {
      __DOCS_PUBLISHED__: JSON.stringify(!draftRoutes.has('/docs')),
    },
  },

  // The landing page (index.md) replicates the hand-built bougie.tools
  // page, which uses custom elements (site-wrap, top-bar, …) — tell Vue
  // they're plain elements, not components to resolve.
  vue: {
    template: {
      compilerOptions: {
        isCustomElement: (tag) => tag.includes('-'),
      },
    },
  },

  markdown: {
    config(md) {
      // Render *single-line* shell code fences (```sh / ```bash / …) as
      // the branded <ShellBox>. Multi-line blocks fall through to the
      // default fence so they keep VitePress's syntax highlighting and
      // aligned comments. The command is the component's children,
      // HTML-escaped (the block output is inserted as raw HTML, so it
      // isn't re-parsed as markdown).
      const SHELL = new Set(['sh', 'bash', 'shell', 'zsh', 'console'])
      const fallback = md.renderer.rules.fence
      md.renderer.rules.fence = (tokens, idx, options, env, self) => {
        const token = tokens[idx]
        const lang = (token.info || '').trim().split(/[\s{]/)[0].toLowerCase()
        const content = token.content.replace(/\n+$/, '')
        if (SHELL.has(lang) && !content.includes('\n')) {
          return `<ShellBox>${md.utils.escapeHtml(content)}</ShellBox>\n`
        }
        return fallback(tokens, idx, options, env, self)
      }
    },
  },

  head: [
    ['link', { rel: 'icon', href: '/favicon.svg', type: 'image/svg+xml' }],
    // Brand fonts, same as the bougie.tools landing page.
    ['link', { rel: 'preconnect', href: 'https://fonts.googleapis.com' }],
    [
      'link',
      { rel: 'preconnect', href: 'https://fonts.gstatic.com', crossorigin: '' },
    ],
    [
      'link',
      {
        rel: 'stylesheet',
        href: 'https://fonts.googleapis.com/css2?family=Archivo:wght@400;500;600;700;800;900&family=JetBrains+Mono:wght@400;500;700&display=swap',
      },
    ],
    [
      'link',
      {
        rel: 'alternate',
        type: 'application/rss+xml',
        title: 'bougie blog',
        href: 'https://bougie.tools/blog/feed.xml',
      },
    ],
    // Privacy-friendly analytics (Plausible), on every page.
    [
      'script',
      {
        async: '',
        src: 'https://analytics.yele.dev/js/pa-mTdm9cUNbJ3ZpeG1xYxMT.js',
      },
    ],
    [
      'script',
      {},
      'window.plausible = window.plausible || function () { (plausible.q = plausible.q || []).push(arguments) }, plausible.init = plausible.init || function (i) { plausible.o = i || {} }; plausible.init()',
    ],
  ],

  themeConfig: {
    siteTitle: 'bougie.tools',

    // pruneItems() drops nav entries whose page is a draft (prod only),
    // e.g. "Docs" disappears while the docs landing is still a draft.
    nav: pruneItems([
      // Changelog lives under /docs/, so exclude it from the Docs match
      // (otherwise both light up on the changelog page).
      { text: 'Docs', link: '/docs/', activeMatch: '^/docs/(?!changelog)' },
      { text: 'Blog', link: '/blog/', activeMatch: '^/blog/' },
      { text: 'Changelog', link: '/docs/changelog', activeMatch: '/docs/changelog' },
    ]),

    // Diátaxis-shaped sidebar: Getting started, then Tutorials (learn),
    // Guides (do), Reference (look up), Concepts (understand).
    // pruneSidebar() drops any entry whose page is a draft (prod only).
    sidebar: pruneSidebar({
      '/docs/': [
        {
          text: 'Getting started',
          items: [
            { text: 'What is bougie?', link: '/docs/' },
            { text: 'Installation', link: '/docs/installation' },
          ],
        },
        {
          text: 'Tutorials',
          items: [
            { text: 'Overview', link: '/docs/tutorials/' },
            { text: 'Your first project', link: '/docs/tutorials/first-project' },
            { text: 'A Mage-OS store', link: '/docs/tutorials/mageos-store' },
            { text: 'Single-file scripts', link: '/docs/tutorials/single-file-script' },
          ],
        },
        {
          text: 'Guides',
          items: [
            { text: 'Overview', link: '/docs/guides/' },
            { text: 'Migrate from Composer', link: '/docs/guides/migrate-from-composer' },
            { text: 'Manage dependencies', link: '/docs/guides/manage-dependencies' },
            { text: 'PHP versions', link: '/docs/guides/php-versions' },
            { text: 'Extensions', link: '/docs/guides/extensions' },
            { text: 'Services', link: '/docs/guides/services' },
            { text: 'Dev server', link: '/docs/guides/dev-server' },
            { text: 'Global tools', link: '/docs/guides/global-tools' },
            { text: 'Patches', link: '/docs/guides/patches' },
            { text: 'Recipes', link: '/docs/guides/recipes' },
            { text: 'Format PHP', link: '/docs/guides/format' },
            { text: 'Diagnose a failure', link: '/docs/guides/diagnose' },
            { text: 'Private registry', link: '/docs/guides/private-registry' },
            { text: 'Share a store', link: '/docs/guides/share-a-store' },
          ],
        },
        {
          text: 'Reference',
          items: [
            { text: 'Overview', link: '/docs/reference/' },
            { text: 'CLI', link: '/docs/reference/cli' },
            { text: 'Configuration', link: '/docs/reference/configuration' },
            { text: 'Environment variables', link: '/docs/reference/environment' },
            { text: 'Service catalog', link: '/docs/reference/services' },
            { text: 'Platform support', link: '/docs/reference/platforms' },
            { text: 'File layout', link: '/docs/reference/layout' },
            { text: 'Changelog', link: '/docs/changelog' },
          ],
        },
        {
          text: 'Concepts',
          items: [
            { text: 'Overview', link: '/docs/concepts/' },
            { text: 'The uv-for-PHP model', link: '/docs/concepts/uv-for-php' },
            { text: 'Native services, not Docker', link: '/docs/concepts/native-services' },
            { text: 'The tenant model', link: '/docs/concepts/tenant-model' },
            { text: 'How the resolver works', link: '/docs/concepts/resolver' },
            { text: 'No plugins, opt-in scripts', link: '/docs/concepts/no-plugins' },
            { text: 'Managed vs. system PHP', link: '/docs/concepts/managed-vs-system-php' },
            { text: 'Security & supply chain', link: '/docs/concepts/security' },
            { text: 'Telemetry & privacy', link: '/docs/concepts/telemetry' },
          ],
        },
      ],
    }),

    socialLinks: [
      { icon: 'github', link: 'https://github.com/cresset-tools/bougie' },
    ],

    search: { provider: 'local' },

    editLink: {
      pattern:
        'https://github.com/cresset-tools/bougie/edit/main/website/:path',
      text: 'Edit this page on GitHub',
    },

    footer: {
      message: 'Free software, licensed under the EUPL-1.2.',
      copyright: '© Cresset',
    },
  },

  buildEnd: genFeed,
})
