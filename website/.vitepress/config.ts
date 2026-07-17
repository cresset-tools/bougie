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
      // Render shell code fences as the branded <ShellBox>. Plain
      // ```sh / ```bash / … only convert when single-line, so multi-line
      // blocks keep VitePress's syntax highlighting and aligned comments.
      // ```shellbox is an explicit opt-in that always renders as a
      // ShellBox, including multi-line sessions. The command is passed
      // base64-encoded so arbitrary shell text survives without escaping
      // or clashing with Vue's `{{ }}` interpolation.
      const SHELL = new Set(['sh', 'bash', 'shell', 'zsh', 'console'])
      const fallback = md.renderer.rules.fence
      md.renderer.rules.fence = (tokens, idx, options, env, self) => {
        const token = tokens[idx]
        const lang = (token.info || '').trim().split(/[\s{]/)[0].toLowerCase()
        const content = token.content.replace(/\n+$/, '')
        const isBox =
          lang === 'shellbox' || (SHELL.has(lang) && !content.includes('\n'))
        if (isBox) {
          const b64 = Buffer.from(content, 'utf-8').toString('base64')
          return `<ShellBox raw="${b64}" />\n`
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

    sidebar: {
      '/docs/': [
        {
          text: 'Getting started',
          items: [
            { text: 'What is bougie?', link: '/docs/' },
            { text: 'Installation', link: '/docs/installation' },
            { text: 'Quickstart', link: '/docs/quickstart' },
          ],
        },
        {
          text: 'Reference',
          items: [{ text: 'Changelog', link: '/docs/changelog' }],
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
