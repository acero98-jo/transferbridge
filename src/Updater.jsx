import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { relaunch } from "@tauri-apps/plugin-process";

export default function Updater() {
  const [updateAvailable, setUpdateAvailable] = useState(false);
  const [downloading, setDownloading]         = useState(false);
  const [progress, setProgress]               = useState(0);
  const [done, setDone]                       = useState(false);

  useEffect(() => {
    // Vérifie les mises à jour 3 secondes après le démarrage
    const timer = setTimeout(async () => {
      try {
        const available = await invoke("check_update");
        if (available) setUpdateAvailable(true);
      } catch (e) {
        console.log("Pas de mise à jour disponible");
      }
    }, 3000);

    const unlisten = listen("update-download-progress", (e) => {
      setProgress(e.payload.percent || 0);
    });

    return () => {
      clearTimeout(timer);
      unlisten.then(f => f());
    };
  }, []);

  async function handleUpdate() {
    setDownloading(true);
    try {
      await invoke("install_update");
      setDone(true);
    } catch (e) {
      console.error(e);
      setDownloading(false);
    }
  }

  async function handleRelaunch() {
    await relaunch();
  }

  if (!updateAvailable) return null;

  return (
    <div style={s.banner}>
      {!downloading && !done && (
        <>
          <div style={s.info}>
            <span style={s.icon}>🆕</span>
            <div>
              <div style={s.title}>Mise à jour disponible !</div>
              <div style={s.sub}>Une nouvelle version de TransferBridge est prête.</div>
            </div>
          </div>
          <div style={s.actions}>
            <button onClick={() => setUpdateAvailable(false)} style={s.skipBtn}>
              Plus tard
            </button>
            <button onClick={handleUpdate} style={s.updateBtn}>
              ⬇️ Mettre à jour
            </button>
          </div>
        </>
      )}

      {downloading && !done && (
        <div style={s.progress}>
          <div style={s.progressLabel}>
            <span>⏳ Téléchargement en cours...</span>
            <span>{progress}%</span>
          </div>
          <div style={s.progressBar}>
            <div style={{ ...s.progressFill, width: `${progress}%` }} />
          </div>
        </div>
      )}

      {done && (
        <div style={s.info}>
          <span style={s.icon}>✅</span>
          <div>
            <div style={s.title}>Mise à jour installée !</div>
            <div style={s.sub}>Relance l'app pour appliquer les changements.</div>
          </div>
          <button onClick={handleRelaunch} style={s.updateBtn}>
            🔄 Relancer
          </button>
        </div>
      )}
    </div>
  );
}

const s = {
  banner: {
    position: "fixed", bottom: 20, right: 20, zIndex: 500,
    background: "#1e293b", border: "1px solid #3b82f6",
    borderRadius: 14, padding: "16px 20px",
    boxShadow: "0 8px 32px rgba(0,0,0,0.4)",
    maxWidth: 380, width: "calc(100vw - 40px)",
  },
  info: { display: "flex", alignItems: "center", gap: 12 },
  icon: { fontSize: 28, flexShrink: 0 },
  title: { fontSize: 14, fontWeight: 700, color: "#f1f5f9" },
  sub:   { fontSize: 12, color: "#64748b", marginTop: 2 },
  actions: {
    display: "flex", gap: 8, marginTop: 14,
    justifyContent: "flex-end",
  },
  skipBtn: {
    padding: "8px 14px", background: "transparent",
    color: "#64748b", border: "1px solid #334155",
    borderRadius: 8, fontSize: 13, cursor: "pointer",
  },
  updateBtn: {
    padding: "8px 16px", background: "#3b82f6",
    color: "white", border: "none",
    borderRadius: 8, fontSize: 13,
    fontWeight: 600, cursor: "pointer",
  },
  progress: { width: "100%" },
  progressLabel: {
    display: "flex", justifyContent: "space-between",
    fontSize: 13, color: "#94a3b8", marginBottom: 8,
  },
  progressBar: {
    width: "100%", height: 6,
    background: "#334155", borderRadius: 3, overflow: "hidden",
  },
  progressFill: {
    height: "100%", borderRadius: 3,
    background: "linear-gradient(90deg, #3b82f6, #06b6d4)",
    transition: "width 0.3s ease",
  },
};