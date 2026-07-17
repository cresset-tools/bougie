import { defineConfig } from 'vitepress'
import { genFeed } from './genFeed'

export default defineConfig({
  title: 'bougie',
  description:
    'PHP toolchain management, the luxury way. A Composer-compatible package manager, PHP version manager, dev services and web server in one fast binary.',
  cleanUrls: true,
  lastUpdated: true,
  sitemap: { hostname: 'https://bougie.tools' },

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
    nav: [
      { text: 'Docs', link: '/docs/', activeMatch: '^/docs/' },
      { text: 'Blog', link: '/blog/', activeMatch: '^/blog/' },
      { text: 'Changelog', link: '/docs/changelog' },
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
