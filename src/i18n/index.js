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