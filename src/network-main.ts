import NetworkApp from './NetworkApp.svelte';
import { mount } from 'svelte';

const app = mount(NetworkApp, {
  target: document.getElementById('network-app')!,
});

export default app;
