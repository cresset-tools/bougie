---
layout: false
title: bougie - PHP toolchain management, the luxury way
titleTemplate: false
description: A Composer-compatible package manager, PHP toolchain manager, and dev server — in one fast Rust binary.
head:
  - - meta
    - property: og:title
      content: bougie — PHP toolchain management, the luxury way
  - - meta
    - property: og:description
      content: A Composer-compatible package manager, PHP toolchain manager, and dev server — in one fast Rust binary.
  - - meta
    - property: og:type
      content: website
  - - meta
    - property: og:url
      content: https://bougie.tools/
---

<script setup>
// __DOCS_PUBLISHED__ is a Vite define (config.ts): false while the docs
// landing (/docs/) is a draft, so the Docs links hide themselves rather
// than 404. They reappear automatically once docs are published.
const docsPublished = __DOCS_PUBLISHED__
</script>

<div class="landing">
<site-wrap>
<top-bar>
<a class="main-link" href="/">bougie.tools</a>
<nav>
<a v-if="docsPublished" href="/docs/">Docs</a>
<a href="/blog/">Blog</a>
<a href="/docs/changelog">Changelog</a>
</nav>
<layout-spacer></layout-spacer>
<AppearanceToggle />
<span><a href="https://github.com/cresset-tools/bougie">GitHub</a></span>
</top-bar>

<hero-banner>
<h1>bougie</h1>
<hero-tagline>PHP toolchain management, <mark>the luxury way.</mark></hero-tagline>
<hero-sub>
Start up your PHP projects with no hassle, and live the bougie lifestyle.
It does it all: install PHP, install your <code>vendor/</code> and then starts your services.
</hero-sub>
<div class="install-slot"><ShellBox>curl -LsSf https://bougie.tools/install.sh | sh</ShellBox></div>
<install-alt>Windows: <code>irm https://bougie.tools/install.ps1 | iex</code></install-alt>
</hero-banner>

<feature-grid>
<feature-cell>
<feature-num>01</feature-num>
<h3>PHP</h3>
<p>
Are you running multiple projects at the same time?
bougie allows you to install <key-token>multiple PHP versions</key-token> and use them simultaneously.
</p>
</feature-cell>
<feature-cell>
<feature-num>02</feature-num>
<h3>Extensions</h3>
<p>
bougie installs a basic PHP extension set by default, but add that niche extension you need with just a <key-token>bougie ext add protobuf</key-token>
</p>
</feature-cell>
<feature-cell>
<feature-num>03</feature-num>
<h3>Services</h3>
<p>
You may only need <code>php artisan serve</code>, but in case you need Elasticsearch, Rabbitmq, Redis and Mysql <key-token>bougie manages and runs your services</key-token>.
</p>
</feature-cell>
<feature-cell>
<feature-num>04</feature-num>
<h3>Drop-in compatible</h3>
<p>Reads your <key-token>composer.json</key-token> and <key-token>composer.lock</key-token> and
produces the same <key-token>vendor/</key-token>. Pick up more bougie features when you need it</p>
</feature-cell>
<feature-cell class="double">
<feature-num>05</feature-num>
<h3>All native</h3>
<p>
HTTP server, Elasticsearch, Rabbitmq, Redis, Mysql and of course: PHP-FPM all run with <key-token>native binaries</key-token>.
This gives you native disk speed on your Macbook, and no extra hassle with finding Docker containers.
</p>
<p>
Bougie fetches everything you need automatically from the bougie index.
Our binaries are built in GitHub actions, signed by the GitHub JWT.
When installing this signature makes sure that the binary was built from source you can verify.
</p>
</feature-cell>
</feature-grid>

<call-band>
<band-tag>Mage-OS and Magento</band-tag>
<h2>First class support for Magento</h2>
<p>Magento has the heaviest dev setup in PHP. bougie makes it so you don't need a long tutorial but only
<band-em>one command to a running store</band-em>. You'll have the correct PHP version, every required extension,
the full service stack, and the packages are done installing before your coffee is done.
</p>
<p>
When you have installed bougie, try this command to get a Mage-OS demo:
</p>
<div class="command-slot"><ShellBox variant="accent">bougie new bougie-store --starter mageos --start</ShellBox></div>
</call-band>

<footer>
<a href="https://github.com/cresset-tools/bougie">GitHub</a>
<a v-if="docsPublished" href="/docs/">Docs</a>
<layout-spacer></layout-spacer>
<span>a <a href="https://cresset.tools">cresset.tools</a> project</span>
</footer>
</site-wrap>
</div>

<style scoped>
.landing {
  --bg: #ffffff;
  --ink: #000000;
  --accent: #2f27ff;
  --accent-ink: #fff;
  --pop: #caff00;
  --muted: #4a4a44;
  --main-font: "Archivo", ui-sans-serif, system-ui, sans-serif;
  --mono-font: "JetBrains Mono", ui-monospace, SFMono-Regular, Menlo, monospace;

  min-height: 100vh;
  background: var(--bg);
  color: var(--ink);
  font: 16px/1.5 var(--main-font);
  -webkit-font-smoothing: antialiased;
}

/* Dark mode: invert the semantic tokens. Everything on the landing is
   painted from --bg/--ink/--accent/--muted, so the whole page follows.
   (The lime --pop stays; text on it is fixed-dark below.) */
.dark .landing {
  --bg: #1b1b1f;
  --ink: #e8e8e6;
  --accent: #6a62ff;
  --accent-ink: #ffffff;
  --muted: #a0a09a;
}

* {
  box-sizing: border-box
}

a {
  color: inherit;
  text-decoration: none
}

site-wrap {
  display: block;
  max-width: 940px;
  margin: 0 auto;
  padding: 0 36px
}

top-bar {
  display: flex;
  align-items: center;
  gap: 16px;
  margin-top: 34px;
  border-bottom: 3px solid var(--ink);
  padding-bottom: 12px;
  font: 500 12px/1 var(--mono-font);
  letter-spacing: .06em;
  text-transform: uppercase;
}

top-bar a.main-link {
  background: var(--ink);
  color: var(--bg);
  padding: 5px 10px
}

top-bar a.main-link:hover {
  background: var(--accent);
  color: var(--accent-ink)
}

top-bar nav {
  display: flex;
  gap: 18px
}

top-bar nav a,
top-bar span a {
  border-bottom: 2px solid transparent;
  padding-bottom: 2px
}

/* Render the GitHub link like the nav links (inline-block, tight
   line-height) so it shares their 16px box and baseline instead of
   an inline anchor's taller line box. */
top-bar span a {
  display: inline-block;
  line-height: 1
}

top-bar nav a:hover,
top-bar span a:hover {
  border-bottom-color: var(--accent);
  color: var(--accent)
}

layout-spacer {
  flex: 1
}

hero-banner {
  display: block;
  padding: 44px 0 0
}

h1 {
  margin: 0;
  font-weight: 900;
  font-size: 168px;
  line-height: .82;
  letter-spacing: -.05em
}

hero-square {
  display: inline-block;
  width: .4em;
  height: .4em;
  background: var(--accent);
  margin-left: .04em
}

hero-tagline {
  display: block;
  margin: 28px 0 0;
  max-width: 680px;
  font-weight: 700;
  font-size: 30px;
  line-height: 1.12;
  letter-spacing: -.02em
}

hero-tagline mark {
  background: var(--accent);
  color: var(--accent-ink);
  padding: 0 .12em
}

hero-sub {
  display: block;
  margin: 16px 0 0;
  max-width: 580px;
  font-size: 16.5px;
  color: var(--muted);
  font-weight: 500;
  text-wrap: pretty
}

/* Spacing for the <ShellBox> in the hero (the box itself is the shared
   component in .vitepress/theme/components/ShellBox.vue). */
.install-slot {
  margin: 32px 0 0;
}

install-alt {
  display: block;
  margin: 10px 2px 0;
  font: 500 12.5px/1.4 var(--mono-font);
  color: var(--muted)
}

install-alt code {
  color: var(--ink)
}

feature-grid {
  display: grid;
  grid-template-columns: 1fr 1fr;
  margin: 72px 0 0;
  gap: 2.5px;
  background: var(--ink);
  border: 2.5px solid var(--ink);
}

feature-cell {
  display: block;
  padding: 26px;
  background: var(--bg);
}

feature-cell.double {
  grid-column: span 2;
}

feature-num {
  display: block;
  font: 700 13px/1 var(--mono-font);
  color: var(--accent)
}

feature-cell h3 {
  margin: 10px 0 8px;
  font-weight: 800;
  font-size: 24px;
  letter-spacing: -.02em
}

feature-cell p {
  margin: 0;
  font-size: 14.5px;
  color: var(--muted);
  font-weight: 500;
  text-wrap: pretty
}

key-token {
  font: 600 .86em/1 var(--mono-font);
  color: #0b0b0a;
  background: var(--pop);
  padding: 1px 4px
}

call-band {
  display: block;
  margin: 24px 0 0;
  background: var(--accent);
  color: var(--accent-ink);
  padding: 34px 32px
}

band-tag {
  display: block;
  font: 700 12px/1 var(--mono-font);
  letter-spacing: .1em;
  text-transform: uppercase;
  opacity: .85
}

call-band h2 {
  margin: 12px 0 12px;
  font-weight: 900;
  font-size: 38px;
  line-height: 1;
  letter-spacing: -.02em
}

call-band p {
  margin: 0;
  max-width: 680px;
  font-size: 16px;
  font-weight: 500;
  text-wrap: pretty
}

band-em {
  background: var(--pop);
  color: #0b0b0a;
  padding: 0 .12em
}

.command-slot {
  margin: 20px 0 0;
}

footer {
  margin: 44px 0 56px;
  border-top: 3px solid var(--ink);
  padding-top: 16px;
  display: flex;
  gap: 18px;
  align-items: center;
  flex-wrap: wrap;
  font: 500 12px/1 var(--mono-font);
  text-transform: uppercase;
  letter-spacing: .06em
}

footer a {
  border-bottom: 2px solid transparent;
  padding-bottom: 2px
}

footer a:hover {
  border-bottom-color: var(--accent);
  color: var(--accent)
}

@media (max-width:760px) {
  site-wrap {
    padding: 0 22px
  }

  top-bar {
    flex-wrap: wrap;
    gap: 12px
  }

  h1 {
    font-size: 84px
  }

  hero-tagline {
    font-size: 24px
  }

  feature-grid {
    grid-template-columns: 1fr
  }

  feature-cell.double {
    grid-column: span 1
  }

  call-band h2 {
    font-size: 30px
  }

  footer {
    flex-direction: column;
    align-items: flex-start;
    gap: 10px
  }

  footer layout-spacer {
    display: none
  }
}
</style>
