(() => {
  const button = document.querySelector('.theme-toggle');
  const themeColor = document.querySelector('meta[name="theme-color"]');
  const preference = window.matchMedia('(prefers-color-scheme: light)');
  let saved = null;

  try {
    const stored = localStorage.getItem('gitviews-theme');
    if (stored === 'light' || stored === 'dark') saved = stored;
  } catch (error) {
    // Theme switching still works for this visit if storage is unavailable.
  }

  function applyTheme(theme) {
    document.documentElement.dataset.theme = theme;
    const label = theme === 'dark' ? '切换到浅色模式' : '切换到深色模式';
    button.setAttribute('aria-label', label);
    button.title = label;
    themeColor.setAttribute('content', theme === 'dark' ? '#0a0f19' : '#f6f9fc');
  }

  applyTheme(saved || (preference.matches ? 'light' : 'dark'));
  button.hidden = false;
  button.addEventListener('click', () => {
    saved = document.documentElement.dataset.theme === 'dark' ? 'light' : 'dark';
    applyTheme(saved);
    try {
      localStorage.setItem('gitviews-theme', saved);
    } catch (error) {
      // Keep the selection for this page even if it cannot be persisted.
    }
  });
  preference.addEventListener('change', () => {
    if (!saved) applyTheme(preference.matches ? 'light' : 'dark');
  });
  window.addEventListener('storage', (event) => {
    if (event.key !== 'gitviews-theme') return;
    saved = event.newValue === 'light' || event.newValue === 'dark' ? event.newValue : null;
    applyTheme(saved || (preference.matches ? 'light' : 'dark'));
  });
})();
