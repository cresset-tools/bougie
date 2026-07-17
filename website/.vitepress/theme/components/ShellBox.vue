<script setup>
import { ref } from 'vue'

const props = defineProps({
  // The command to display and copy. May also be passed as the default slot.
  cmd: { type: String, default: '' },
  // Left prompt glyph.
  prompt: { type: String, default: '$' },
  // 'default' (paper box, accent prompt) or 'accent' (accent box, lime prompt).
  variant: { type: String, default: 'default' },
})

const root = ref(null)
const label = ref('copy')

function copy() {
  const text = (props.cmd || root.value?.querySelector('code')?.textContent || '').trim()
  navigator.clipboard?.writeText(text).then(() => {
    label.value = 'copied'
    setTimeout(() => (label.value = 'copy'), 1500)
  })
}
</script>

<template>
  <div ref="root" class="shell-box" :class="`shell-box--${variant}`">
    <span class="shell-box__prompt">{{ prompt }}</span>
    <code class="shell-box__cmd"><slot>{{ cmd }}</slot></code>
    <button class="shell-box__copy" type="button" aria-label="Copy command" @click="copy">
      {{ label }}
    </button>
  </div>
</template>

<style scoped>
/* Terminal/shell box matching the bougie.tools landing page. Colors
   come from the global brand tokens so it works on any page (light or
   dark) — the landing simply resolves them to the same values. */
.shell-box {
  /* Prefer the landing's semantic tokens (so it tracks the landing's
     light/dark palette), falling back to the global theme tokens when
     used on a docs page outside .landing. */
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
  display: flex;
  align-items: center;
  padding: 15px 16px;
  font: 500 14px/1 var(--vp-font-family-mono);
  color: var(--box-ink);
  background: transparent;
  overflow-x: auto;
}

.shell-box__copy {
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
