// Generates /blog/feed.xml from the blog posts at build time.
import path from 'node:path'
import { writeFileSync } from 'node:fs'
import { Feed } from 'feed'
import { createContentLoader, type SiteConfig } from 'vitepress'

const hostname = 'https://bougie.tools'

export async function genFeed(config: SiteConfig) {
  const feed = new Feed({
    title: 'bougie blog',
    description: 'News and engineering notes from the bougie project',
    id: hostname,
    link: hostname,
    language: 'en',
    favicon: `${hostname}/favicon.ico`,
    copyright: '© Cresset',
  })

  const posts = await createContentLoader('blog/*.md', {
    excerpt: true,
    render: true,
  }).load()

  posts.sort(
    (a, b) => +new Date(b.frontmatter.date) - +new Date(a.frontmatter.date),
  )

  for (const { url, excerpt, frontmatter, html } of posts) {
    // Skip the listing page itself and any draft.
    if (frontmatter.date === undefined || frontmatter.draft) continue
    feed.addItem({
      title: frontmatter.title,
      id: `${hostname}${url}`,
      link: `${hostname}${url}`,
      description: frontmatter.description ?? excerpt,
      content: html,
      date: new Date(frontmatter.date),
    })
  }

  writeFileSync(path.join(config.outDir, 'blog', 'feed.xml'), feed.rss2())
}
