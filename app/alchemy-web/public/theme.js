// Applies a stored Light or Dark override before first paint, so the page
// never flashes the wrong theme. A classic script from 'self', so the
// desktop shell's CSP allows it. Mirrors THEME_STORAGE_KEY and
// applyThemePreference in src/lib/theme.ts.
try {
  const theme = window.localStorage.getItem("alchemy.theme");
  if (theme === "light" || theme === "dark") {
    document.documentElement.dataset.theme = theme;
  }
} catch {
  // No storage: follow the system theme.
}
