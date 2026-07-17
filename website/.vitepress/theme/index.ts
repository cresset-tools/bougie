// theme-without-fonts: the brand fonts (Archivo + JetBrains Mono) are
// loaded via <head> links in config.ts, so skip the bundled Inter.
import DefaultTheme from 'vitepress/theme-without-fonts'
import ShellBox from './components/ShellBox.vue'
import './custom.css'

export default {
  extends: DefaultTheme,
  enhanceApp({ app }) {
    // Reusable terminal/shell box (landing + docs).
    app.component('ShellBox', ShellBox)
  },
}
