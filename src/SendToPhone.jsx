import { useState, useEffect } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { listen } from "@tauri-apps/api/event";

export default function SendToPhone({ t }) {
  const [pendingFiles, setPendingFiles] = useState([]);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    loadPendingFiles();
    // Écoute quand le téléphone télécharge un fichier
    const unlisten = listen("file-downloaded", (e) => {
      console.log("Téléchargé :", e.payload);
    });
    return () => { unlisten.then(f => f()); };
  }, []);

  async function loadPendingFiles() {
    try {
      const files = await invoke("get_pending_files");
      setPendingFiles(files);
    } catch (e) { console.error(e); }
  }

  async function pickFile() {
    const info = await invoke("get_plan_info");
    if (!info.bidirectional) return;
    setLoading(true);
    try {
      const selected = await open({
        multiple: true,
        title: "Choisir les fichiers à envoyer au téléphone",
      });
      if (!selected) return;

      const files = Array.isArray(selected) ? selected : [selected];
      for (const path of files) {
        const result = await invoke("queue_file_for_send", { path });
        setPendingFiles(prev => [...prev, result]);
      }
    } catch (e) {
      console.error(e);
    } finally {
      setLoading(false);
    }
  }

  async function cancelFile(fileId) {
    try {
      await invoke("cancel_pending_file", { fileId });
      setPendingFiles(prev => prev.filter(f => f.id !== fileId));
    } catch (e) { console.error(e); }
  }

  function formatSize(bytes) {
    if (!bytes) return "0 o";
    if (bytes < 1024) return bytes + " o";
    if (bytes < 1048576) return (bytes / 1024).toFixed(1) + " Ko";
    return (bytes / 1048576).toFixed(1) + " Mo";
  }

  function getFileIcon(name) {
    if (!name) return "📎";
    const ext = name.split(".").pop().toLowerCase();
    if (["jpg","jpeg","png","gif","webp"].includes(ext)) return "🖼️";
    if (["mp4","mov","avi","mkv"].includes(ext)) return "🎬";
    if (ext === "pdf") return "📄";
    if (["zip","rar","7z"].includes(ext)) return "🗜️";
    if (["mp3","wav","aac"].includes(ext)) return "🎵";
    if (["doc","docx"].includes(ext)) return "📝";
    return "📎";
  }

  return (
    <div style={s.container}>
      <div style={s.header}>
        <div>
          <h3 style={s.title}>📤 Envoyer vers le téléphone</h3>
          <p style={s.sub}>
            Sélectionne des fichiers — ils apparaîtront sur l'interface mobile
          </p>
        </div>
        <button onClick={pickFile} disabled={loading} style={s.addBtn}>
          {loading ? "⏳..." : "+ Ajouter"}
        </button>
      </div>

      {pendingFiles.length === 0 ? (
        <div style={s.empty}>
          <div style={{ fontSize: 36, marginBottom: 10 }}>📂</div>
          <p>Aucun fichier en attente</p>
          <p style={{ fontSize: 12, color: "#475569", marginTop: 4 }}>
            Les fichiers expirent après 10 minutes
          </p>
          <button onClick={pickFile} style={s.emptyBtn}>
            + Choisir un fichier
          </button>
        </div>
      ) : (
        <div style={s.fileList}>
          {pendingFiles.map((file) => (
            <div key={file.id} style={s.fileItem}>
              <span style={{ fontSize: 22 }}>{getFileIcon(file.name)}</span>
              <div style={{ flex: 1, minWidth: 0 }}>
                <div style={s.fileName}>{file.name}</div>
                <div style={s.fileMeta}>{formatSize(file.size)}</div>
              </div>
              <span style={s.waitBadge}>⏳ En attente</span>
              <button
                onClick={() => cancelFile(file.id)}
                style={s.cancelBtn}
                title="Annuler"
              >
                ✕
              </button>
            </div>
          ))}
        </div>
      )}

      {pendingFiles.length > 0 && (
        <p style={s.hint}>
          📱 Le téléphone voit ces fichiers dans l'onglet "Télécharger"
        </p>
      )}
    </div>
  );
}

const s = {
  container: {
    background: "#0f172a", borderRadius: 12,
    padding: "16px", marginTop: 16,
  },
  header: {
    display: "flex", justifyContent: "space-between",
    alignItems: "flex-start", marginBottom: 14,
    gap: 10,
  },
  title: { fontSize: 14, fontWeight: 700, margin: 0, color: "#f1f5f9" },
  sub: { fontSize: 12, color: "#64748b", marginTop: 2 },
  addBtn: {
    padding: "7px 14px", background: "#3b82f6", color: "white",
    border: "none", borderRadius: 8, fontSize: 12,
    fontWeight: 600, cursor: "pointer", flexShrink: 0,
  },
  empty: {
    textAlign: "center", color: "#64748b",
    padding: "24px 0",
  },
  emptyBtn: {
    marginTop: 12, padding: "8px 16px",
    background: "rgba(59,130,246,0.1)", color: "#3b82f6",
    border: "1px solid rgba(59,130,246,0.25)", borderRadius: 8,
    fontSize: 13, cursor: "pointer",
  },
  fileList: { display: "flex", flexDirection: "column", gap: 8 },
  fileItem: {
    display: "flex", alignItems: "center", gap: 10,
    background: "#1e293b", borderRadius: 10, padding: "10px 12px",
  },
  fileName: {
    fontSize: 13, fontWeight: 500,
    whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis",
  },
  fileMeta: { fontSize: 11, color: "#64748b" },
  waitBadge: {
    fontSize: 11, color: "#f59e0b",
    background: "rgba(245,158,11,0.1)",
    padding: "3px 8px", borderRadius: 6,
    whiteSpace: "nowrap",
  },
  cancelBtn: {
    background: "transparent", border: "none",
    color: "#475569", cursor: "pointer",
    fontSize: 14, padding: 4,
    borderRadius: 4,
  },
  hint: {
    fontSize: 11, color: "#475569",
    textAlign: "center", marginTop: 10,
  },
};