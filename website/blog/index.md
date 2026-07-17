---
title: Blog
sidebar: false
aside: false
outline: false
---

<script setup>
import { data as posts } from './posts.data.js'
</script>

# Blog

News and engineering notes from the bougie project.
Subscribe via <a href="/blog/feed.xml" target="_blank" rel="noreferrer">RSS</a>.

<ul class="blog-index">
  <li v-for="post of posts" :key="post.url">
    <h2><a :href="post.url">{{ post.title }}</a></h2>
    <p class="date">{{ post.date.string }}</p>
    <p v-if="post.description">{{ post.description }}</p>
  </li>
</ul>

<style scoped>
.blog-index {
  list-style: none;
  padding: 0;
}
.blog-index li {
  margin: 2rem 0;
  border-top: 2.5px solid var(--vp-c-text-1);
  padding-top: 1.25rem;
}
.blog-index h2 {
  border-top: none;
  margin: 0;
  padding: 0;
  font-size: 1.375rem;
  font-weight: 800;
  letter-spacing: -0.02em;
}
.blog-index .date {
  margin: 0.5rem 0 0.5rem;
  font: 500 11px/1.4 var(--vp-font-family-mono);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  color: var(--vp-c-text-2);
}
.blog-index p {
  margin: 0;
}
</style>
