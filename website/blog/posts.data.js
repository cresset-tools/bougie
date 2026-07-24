// Build-time loader that collects all blog posts for the index page.
import { createContentLoader } from 'vitepress'

// Drafts are listed in dev (so you can preview them) but hidden in the
// production build, matching srcExclude in config.ts.
const HIDE_DRAFTS = process.env.NODE_ENV === 'production'

export default createContentLoader('blog/*.md', {
  transform(raw) {
    return raw
      .filter(({ frontmatter }) => frontmatter.date !== undefined)
      .filter(({ frontmatter }) => !(HIDE_DRAFTS && frontmatter.draft))
      .map(({ url, frontmatter }) => ({
        title: frontmatter.title,
        description: frontmatter.description,
        draft: !!frontmatter.draft,
        url,
        date: formatDate(frontmatter.date),
      }))
      .sort((a, b) => b.date.time - a.date.time)
  },
})

function formatDate(raw) {
  const date = new Date(raw)
  return {
    time: +date,
    string: date.toLocaleDateString('en-US', {
      year: 'numeric',
      month: 'long',
      day: 'numeric',
      timeZone: 'UTC',
    }),
  }
}
