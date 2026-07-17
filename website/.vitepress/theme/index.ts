// theme-without-fonts: the brand fonts (Archivo + JetBrains Mono) are
// loaded via <head> links in config.ts, so skip the bundled Inter.
import DefaultTheme from 'vitepress/theme-without-fonts'
import './custom.css'

export default DefaultTheme
