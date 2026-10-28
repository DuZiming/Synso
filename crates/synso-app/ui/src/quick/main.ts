// Quick window entry (second Vite page — see vite.config.ts's
// rollupOptions.input and tauri.conf.json's "quick" window). Same design
// tokens as the main window via app.css, plus the frameless page shell.
import "../app.css";
import "./quick.css";
import { mount } from "svelte";
import QuickWindow from "./QuickWindow.svelte";

const app = mount(QuickWindow, {
  target: document.getElementById("app")!,
});

export default app;
