<script setup>
// Reuse the theme's own appearance switch so it inherits the square
// #app .VPSwitch styling and the sun/moon icons. Used in the custom
// (layout: false) landing header, which otherwise has no toggle.
import VPSwitchAppearance from 'vitepress/dist/client/theme-default/components/VPSwitchAppearance.vue'
</script>

<template>
  <VPSwitchAppearance />
</template>
