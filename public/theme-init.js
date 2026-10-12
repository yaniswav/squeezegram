// Runs synchronously from <head>, before the first paint, so the window never flashes
// the wrong theme or text direction. Storage keys and the RTL list must stay in sync with
// src/lib/theme.ts and src/i18n/languages.ts.
;(function () {
  var root = document.documentElement
  var theme = null
  var language = null
  try {
    theme = localStorage.getItem("squeezegram.theme")
    language = localStorage.getItem("squeezegram.language")
  } catch {
    // Storage can be unavailable; fall back to system theme and default language.
  }

  var dark =
    theme === "dark" ||
    (theme !== "light" &&
      window.matchMedia("(prefers-color-scheme: dark)").matches)
  root.classList.toggle("dark", dark)
  root.style.colorScheme = dark ? "dark" : "light"

  if (language) {
    root.lang = language
    root.dir = language === "ar" || language === "xx-rtl" ? "rtl" : "ltr"
  }
})()
