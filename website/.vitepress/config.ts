import { defineConfig } from 'vitepress'
import { genFeed } from './genFeed'

export default defineConfig({
  title: 'bougie',
  description:
    'PHP toolchain management, the luxury way. A Composer-compatible package manager, PHP version manager, dev services and web server in one fast binary.',
  cleanUrls: true,
  lastUpdated: true,
  sitemap: { hostname: 'https://bougie.tools' },

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

    nav: [
      // Changelog lives under /docs/, so exclude it from the Docs match
      // (otherwise both light up on the changelog page).
      { text: 'Docs', link: '/docs/', activeMatch: '^/docs/(?!changelog)' },
      { text: 'Blog', link: '/blog/', activeMatch: '^/blog/' },
      { text: 'Changelog', link: '/docs/changelog', activeMatch: '/docs/changelog' },
    ],

    // Diátaxis-shaped sidebar: Getting started, then Tutorials (learn),
    // Guides (do), Reference (look up), Concepts (understand).
    sidebar: {
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
    },

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
