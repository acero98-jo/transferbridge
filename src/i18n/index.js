import fr from './fr.js';
import en from './en.js';
import es from './es.js';

export const languages = { fr, en, es };

export const languageNames = {
  fr: "🇫🇷 Français",
  en: "🇬🇧 English",
  es: "🇪🇸 Español",
};

// Détecte la langue du système
export function detectLanguage() {
  const sys = navigator.language?.toLowerCase() || "fr";
  if (sys.startsWith("es")) return "es";
  if (sys.startsWith("en")) return "en";
  return "fr";
}

export function getT(lang) {
  return languages[lang] || languages.fr;
}

// Remplace les {variables} d'une chaîne : fmt("{a} sur {b}", { a: 1, b: 2 })
export function fmt(template, vars = {}) {
  return String(template).replace(/\{(\w+)\}/g, (m, k) => (k in vars ? vars[k] : m));
}