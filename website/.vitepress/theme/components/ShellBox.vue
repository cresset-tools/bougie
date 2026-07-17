<script setup>
import { computed, ref } from 'vue'

const props = defineProps({
  // Single-line command (used directly on the landing).
  cmd: { type: String, default: '' },
  // Base64-encoded (possibly multi-line) command, used by the
  // markdown-it fence transform so arbitrary shell text — quotes,
  // pipes, braces, `{{ }}` — survives without escaping.
  raw: { type: String, default: '' },
  prompt: { type: String, default: '$' },
  // 'default' (paper box, accent prompt) or 'accent' (accent box).
  variant: { type: String, default: 'default' },
})

function decodeBase64Utf8(b64) {
  const bin = atob(b64)
  const bytes = Uint8Array.from(bin, (c) => c.charCodeAt(0))
  return new TextDecoder().decode(bytes)
}

const text = computed(() =>
  (props.raw ? decodeBase64Utf8(props.raw) : props.cmd).replace(/\n+$/, ''),
)

// Split each line into a code part and a trailing shell comment so the
// comment can be dimmed. A comment starts at line-start `#` or a `#`
// preceded by whitespace (so `foo#bar` mid-token isn't split).
const lines = computed(() =>
  text.value.split('\n').map((line) => {
    let at = -1
    if (line.startsWith('#')) at = 0
    else {
      const m = line.match(/\s#/)
      if (m) at = m.index + 1
    }
    return at < 0
      ? { code: line, comment: '' }
      : { code: line.slice(0, at), comment: line.slice(at) }
  }),
)

const multiline = computed(() => lines.value.length > 1)

const label = ref('copy')
function copy() {
  navigator.clipboard?.writeText(text.value).then(() => {
    label.value = 'copied'
    setTimeout(() => (label.value = 'copy'), 1500)
  })
}
</script>

<template>
  <div
    class="shell-box"
    :class="[`shell-box--${variant}`, { 'shell-box--multiline': multiline }]"
  >
    <span v-if="!multiline" class="shell-box__prompt">{{ prompt }}</span>
    <code class="shell-box__cmd"><span v-for="(ln, i) in lines" :key="i" class="shell-box__line"><span>{{ ln.code }}</span><span v-if="ln.comment" class="shell-box__comment">{{ ln.comment }}</span></span></code>
    <button
      class="shell-box__copy"
      type="button"
      aria-label="Copy command"
      @click="copy"
    >
      {{ label }}
    </button>
  </div>
</template>

<style scoped>
/* Terminal/shell box matching the bougie.tools landing page. Colors
   come from the landing's semantic tokens when present (so it tracks
   the landing's light/dark palette), else the global theme tokens. */
.shell-box {
  --box-ink: var(--ink, var(--vp-c-text-1));
  --box-accent: var(--accent, var(--vp-c-brand-1));
  --box-bg: var(--bg, var(--vp-c-bg));
  --box-pop: var(--pop, var(--bougie-pop, #caff00));

  display: flex;
  align-items: stretch;
  max-width: 700px;
  border: 2.5px solid var(--box-ink);
  font-family: var(--vp-font-family-mono);
}

.shell-box--multiline {
  align-items: flex-start;
}

.shell-box__prompt {
  display: flex;
  align-items: center;
  padding: 0 16px;
  background: var(--box-accent);
  color: #fff;
  font: 700 18px/1 var(--vp-font-family-mono);
}

.shell-box__cmd {
  flex: 1;
  min-width: 0;
  display: flex;
  align-items: center;
  padding: 15px 16px;
  /* Single line: tight, so the box height matches the landing install
     box to the pixel. Multi-line loosens below for readability. */
  font: 500 14px/1 var(--vp-font-family-mono);
  color: var(--box-ink);
  background: transparent;
  overflow-x: auto;
}

.shell-box--multiline .shell-box__cmd {
  display: block;
  line-height: 1.7;
}

.shell-box__line {
  display: block;
  white-space: pre;
}

.shell-box__comment {
  color: var(--box-ink);
  opacity: 0.5;
}

.shell-box__copy {
  flex-shrink: 0;
  border: 0;
  border-left: 2.5px solid var(--box-ink);
  background: transparent;
  color: var(--box-ink);
  padding: 0 14px;
  font: 700 11px/1 var(--vp-font-family-mono);
  letter-spacing: 0.08em;
  text-transform: uppercase;
  cursor: pointer;
}

.shell-box--multiline .shell-box__copy {
  align-self: stretch;
  padding-top: 15px;
}

.shell-box__copy:hover {
  background: var(--box-ink);
  color: var(--box-bg);
}

/* Accent variant: the landing's "command-box" — accent field, lime
   prompt, white ink. */
.shell-box--accent {
  border-color: #fff;
  background: var(--box-accent);
}

.shell-box--accent .shell-box__prompt {
  background: var(--box-pop);
  /* Fixed dark: the lime prompt is always a light field. */
  color: #0b0b0a;
}

.shell-box--accent .shell-box__cmd {
  color: #fff;
}

.shell-box--accent .shell-box__copy {
  border-left-color: #fff;
  color: #fff;
}

.shell-box--accent .shell-box__copy:hover {
  background: #fff;
  color: var(--box-accent);
}

@media (max-width: 760px) {
  .shell-box {
    flex-direction: column;
  }

  .shell-box__prompt {
    padding: 8px 16px;
  }

  .shell-box__copy {
    border-left: 0;
    border-top: 2.5px solid var(--box-ink);
    padding: 10px 16px;
  }

  .shell-box--accent .shell-box__copy {
    border-top-color: #fff;
  }
}
</style>
