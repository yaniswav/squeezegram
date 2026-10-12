import "i18next"
import type en from "./locales/en.json"

// Type-checks translation keys against the English catalogue.
declare module "i18next" {
  interface CustomTypeOptions {
    defaultNS: "translation"
    resources: { translation: typeof en }
  }
}
