import App from './App.svelte';
import { mount } from 'svelte';
import { initThemePrePaint } from './lib/theme-service';

initThemePrePaint();

const app = mount(App, {
  target: document.getElementById('app')!,
});

export default app;
