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
}
.blog-index h2 {
  border-top: none;
  margin: 0;
  padding: 0;
  font-size: 1.25rem;
}
.blog-index .date {
  margin: 0.25rem 0 0.5rem;
  font-size: 0.875rem;
  color: var(--vp-c-text-2);
}
.blog-index p {
  margin: 0;
}
</style>
