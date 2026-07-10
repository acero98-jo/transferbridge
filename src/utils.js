export function formatSize(bytes) {
  if (!bytes) return "0 o";
  if (bytes < 1024) return bytes + " o";
  if (bytes < 1048576) return (bytes / 1024).toFixed(1) + " Ko";
  return (bytes / 1048576).toFixed(1) + " Mo";
}

export function getFileIcon(name) {
  if (!name) return "📎";
  const ext = name.split(".").pop().toLowerCase();
  if (["jpg","jpeg","png","gif","webp","heic"].includes(ext)) return "🖼️";
  if (["mp4","mov","avi","mkv"].includes(ext)) return "🎬";
  if (ext === "pdf") return "📄";
  if (["zip","rar","7z"].includes(ext)) return "🗜️";
  if (["mp3","wav","aac"].includes(ext)) return "🎵";
  if (["doc","docx"].includes(ext)) return "📝";
  if (["xls","xlsx"].includes(ext)) return "📊";
  return "📎";
}

export function getFileType(name) {
  if (!name) return "other";
  const ext = name.split(".").pop().toLowerCase();
  if (["jpg","jpeg","png","gif","webp","heic"].includes(ext)) return "image";
  if (["mp4","mov","avi","mkv"].includes(ext)) return "video";
  if (ext === "pdf") return "pdf";
  if (["mp3","wav","aac"].includes(ext)) return "audio";
  return "other";
}

const BADGE_COLORS = {
  jpg:  { bg: "#14532d", color: "#4ade80" }, jpeg: { bg: "#14532d", color: "#4ade80" },
  png:  { bg: "#14532d", color: "#4ade80" }, gif:  { bg: "#14532d", color: "#4ade80" },
  webp: { bg: "#14532d", color: "#4ade80" }, heic: { bg: "#14532d", color: "#4ade80" },
  mp4:  { bg: "#134e4a", color: "#2dd4bf" },
  mov:  { bg: "#3b0764", color: "#c084fc" }, avi: { bg: "#3b0764", color: "#c084fc" }, mkv: { bg: "#3b0764", color: "#c084fc" },
  pdf:  { bg: "#450a0a", color: "#f87171" },
  zip:  { bg: "#1e3a8a", color: "#60a5fa" }, rar: { bg: "#1e3a8a", color: "#60a5fa" }, "7z": { bg: "#1e3a8a", color: "#60a5fa" },
  mp3:  { bg: "#78350f", color: "#fbbf24" }, wav: { bg: "#78350f", color: "#fbbf24" }, aac: { bg: "#78350f", color: "#fbbf24" },
  doc:  { bg: "#312e81", color: "#a5b4fc" }, docx: { bg: "#312e81", color: "#a5b4fc" },
  xls:  { bg: "#052e16", color: "#86efac" }, xlsx: { bg: "#052e16", color: "#86efac" },
};

export function getFileBadge(name) {
  if (!name) return { label: "FILE", bg: "#334155", color: "#cbd5e1" };
  const ext = name.split(".").pop().toLowerCase();
  const colors = BADGE_COLORS[ext] || { bg: "#334155", color: "#cbd5e1" };
  return { label: ext.slice(0, 4).toUpperCase(), ...colors };
}
