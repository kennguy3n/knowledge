// Applies the persisted theme before first paint to avoid a flash of the
// wrong theme. Runs as a blocking inline script (no React needed); the
// Settings page writes `knowledge.ui.theme` to localStorage.
export function ThemeScript() {
  const js = `(function(){try{var t=localStorage.getItem('knowledge.ui.theme');if(t==='light'||t==='dark'){document.documentElement.setAttribute('data-theme',t);}else{document.documentElement.setAttribute('data-theme',window.matchMedia&&window.matchMedia('(prefers-color-scheme: light)').matches?'light':'dark');}}catch(e){}})();`;
  return <script dangerouslySetInnerHTML={{ __html: js }} />;
}
