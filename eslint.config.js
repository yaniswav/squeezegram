import js from "@eslint/js"
import { defineConfig, globalIgnores } from "eslint/config"
import reactHooks from "eslint-plugin-react-hooks"
import reactRefresh from "eslint-plugin-react-refresh"
import globals from "globals"
import tseslint from "typescript-eslint"

// Physical (left/right) utilities that have a logical (start/end) equivalent.
const PHYSICAL_CLASS =
  /(?:^|[\s:])-?(?:m[lr]|p[lr]|left|right|scroll-m[lr]|scroll-p[lr])-|(?:^|[\s:])(?:text-(?:left|right)|float-(?:left|right)|clear-(?:left|right))(?:\s|$)|(?:^|[\s:])(?:rounded|border)-(?:l|r|tl|tr|bl|br)(?:-|\s|$)|(?:^|[\s:])space-x-/

export default defineConfig([
  globalIgnores(["dist", "src-tauri/target", "src-tauri/gen"]),
  {
    files: ["src/**/*.{ts,tsx}"],
    extends: [
      js.configs.recommended,
      tseslint.configs.recommended,
      reactHooks.configs.flat.recommended,
      reactRefresh.configs.vite,
    ],
    languageOptions: { ecmaVersion: 2022, globals: globals.browser },
  },
  {
    // RTL must work, so use logical Tailwind utilities (ms-/me-/ps-/pe-,
    // start/end, text-start/text-end) instead of physical ones. Generated shadcn/ui files are
    // exempt: they carry their own rtl: variants.
    files: ["src/**/*.{ts,tsx}"],
    ignores: ["src/components/ui/**"],
    rules: {
      "no-restricted-syntax": [
        "error",
        {
          selector: `:matches(JSXAttribute[name.name='className'], CallExpression[callee.name='cn']) Literal[value=/${PHYSICAL_CLASS.source}/]`,
          message:
            "Physical Tailwind utility: use the logical one (ms-/me-/ps-/pe-, start-/end-, text-start/text-end, rounded-s/e, border-s/e).",
        },
      ],
    },
  },
  {
    // Generated shadcn/ui components export variants next to components by design.
    files: ["src/components/ui/**"],
    rules: { "react-refresh/only-export-components": "off" },
  },
  {
    files: ["public/**/*.js"],
    extends: [js.configs.recommended],
    languageOptions: { sourceType: "script", globals: globals.browser },
  },
  {
    files: ["*.config.{js,ts}"],
    extends: [js.configs.recommended, tseslint.configs.recommended],
    languageOptions: { globals: globals.node },
  },
])
