import NetworkApp from './NetworkApp.svelte';
import { mount } from 'svelte';
import { initThemePrePaint } from './lib/theme-service';

initThemePrePaint();

const app = mount(NetworkApp, {
  target: document.getElementById('network-app')!,
});

export default app;
