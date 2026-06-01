import { useEffect, useState, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";
import QRCode from "qrcode";
import { languageNames, detectLanguage, getT } from "./i18n/index.js";
import Feedback from "./Feedback.jsx";
import Updater from "./Updater.jsx";
import SendToPhone from "./SendToPhone.jsx";
import ProActivation from "./ProActivation.jsx";

export default function App() {
  const [serverUrl, setServerUrl] = useState(null);
  const [qrCode, setQrCode] = useState(null);
  const [files, setFiles] = useState([]);
  const [status, setStatus] = useState("idle");
  const [saveDir, setSaveDir] = useState("");
  const [pin, setPin] = useState("····");
  const [showPin, setShowPin] = useState(false);
  const [lastDevice, setLastDevice] = useState(null);
  const [filter, setFilter] = useState("all");
  const [search, setSearch] = useState("");
  const [maxSizeMb, setMaxSizeMb] = useState(500);
  const [sessionError, setSessionError] = useState(null);
  const [showSettings, setShowSettings] = useState(false);
  const [lang, setLang] = useState(() => localStorage.getItem("tb_lang") || detectLanguage());
  const filesRef = useRef([]);
  const [showFeedback, setShowFeedback] = useState(false);
  const [isPro, setIsPro] = useState(false);
  const [showProModal, setShowProModal] = useState(false);

  const t = getT(lang);

  useEffect(() => { filesRef.current = files; }, [files]);

  useEffect(() => {
    // Initialise l'app de façon async
    async function init() {
      await initApp();

      // Vérifie la licence Pro
      try {
        const proStatus = await invoke("check_license");
        setIsPro(proStatus);
      } catch (e) { console.error(e); }
    }
    init();

    const unlistenFile = listen("file-received", (e) => {
      const newFile = {
        ...e.payload,
        time: new Date().toLocaleTimeString(),
        date: new Date().toLocaleDateString(),
        timestamp: Date.now(),
      };
      setFiles(prev => {
        const updated = [newFile, ...prev];
        invoke("save_history", { history: updated }).catch(console.error);
        return updated;
      });
      notifyFileReceived(e.payload.name, e.payload.size);
    });

    const unlistenPin = listen("pin-generated", (e) => setPin(e.payload));
    const unlistenDevice = listen("device-connected", (e) => setLastDevice(e.payload.time));

    const unlistenProgress = listen("upload-progress", (e) => {
      setFiles(prev => prev.map(f =>
        f.name === e.payload.filename ? { ...f, progress: e.payload.percent } : f
      ));
    });

    const unlistenError = listen("upload-error", (e) => {
      const payload = e.payload;
      if (payload.error === "session_expired") {
        setSessionError(t.sessionExpired);
      } else if (payload.error === "too_large") {
        setSessionError(payload.message);
      }
      setTimeout(() => setSessionError(null), 5000);
    });

    const unlistenPro = listen("pro-activated", () => {
      setIsPro(true);
    });

    return () => {
      unlistenFile.then(f => f());
      unlistenPin.then(f => f());
      unlistenDevice.then(f => f());
      unlistenProgress.then(f => f());
      unlistenError.then(f => f());
      unlistenPro.then(f => f());
    };
  }, []);

  function changeLanguage(newLang) {
    setLang(newLang);
    localStorage.setItem("tb_lang", newLang);
  }

  async function initApp() {
    setStatus("starting");
    try {
      await setupNotifications();

      // 1. Charge l'historique
      const history = await invoke("load_history");
      if (Array.isArray(history) && history.length > 0) setFiles(history);

      // 2. Charge le dossier
      const dir = await invoke("get_save_dir");
      setSaveDir(dir);

      // 3. Démarre le serveur EN PREMIER
      const url = await invoke("start_server");
      setServerUrl(url);
      setStatus("running");

      const qr = await QRCode.toDataURL(url, {
        width: 200, margin: 2,
        color: { dark: "#1e293b", light: "#f8fafc" }
      });
      setQrCode(qr);

      // 4. Charge la config APRÈS le serveur
      const config = await invoke("get_config");
      setMaxSizeMb(config.max_file_size_mb);
      await invoke("set_max_file_size", { sizeMb: config.max_file_size_mb });

    } catch (e) {
      console.error(e);
      setStatus("idle");
    }
  }

  async function setupNotifications() {
    try {
      let granted = await isPermissionGranted();
      if (!granted) {
        const permission = await requestPermission();
        granted = permission === "granted";
      }
      return granted;
    } catch { return false; }
  }

  async function notifyFileReceived(filename, size) {
    try {
      const granted = await isPermissionGranted();
      if (granted) {
        sendNotification({
          title: "📁 TransferBridge",
          body: `✅ ${filename} (${formatSize(size)})`,
        });
      }
    } catch (e) { console.error(e); }
  }

  async function chooseSaveDir() {
    try {
      const selected = await open({
        directory: true, multiple: false,
        title: "Choisir le dossier de destination"
      });
      if (selected) {
        setSaveDir(selected);
        await invoke("set_save_dir", { path: selected });
      }
    } catch (e) { console.error(e); }
  }

  async function regeneratePin() {
    try {
      const newPin = await invoke("regenerate_pin");
      setPin(newPin);
      setShowPin(true);
      setLastDevice(null);
      setTimeout(() => setShowPin(false), 5000);
    } catch (e) { console.error(e); }
  }

  async function updateMaxSize(mb) {
    const val = parseInt(mb);
    if (isNaN(val) || val < 1) return;
    setMaxSizeMb(val);
    await invoke("set_max_file_size", { sizeMb: val });
    await invoke("save_config", {
      config: { max_file_size_mb: val, allowed_extensions: [] }
    });
  }

  function clearHistory() {
    setFiles([]);
    invoke("save_history", { history: [] }).catch(console.error);
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
    if (["jpg","jpeg","png","gif","webp","heic"].includes(ext)) return "🖼️";
    if (["mp4","mov","avi","mkv"].includes(ext)) return "🎬";
    if (ext === "pdf") return "📄";
    if (["zip","rar","7z"].includes(ext)) return "🗜️";
    if (["mp3","wav","aac"].includes(ext)) return "🎵";
    if (["doc","docx"].includes(ext)) return "📝";
    if (["xls","xlsx"].includes(ext)) return "📊";
    return "📎";
  }

  function getFileType(name) {
    if (!name) return "other";
    const ext = name.split(".").pop().toLowerCase();
    if (["jpg","jpeg","png","gif","webp","heic"].includes(ext)) return "image";
    if (["mp4","mov","avi","mkv"].includes(ext)) return "video";
    if (ext === "pdf") return "pdf";
    if (["mp3","wav","aac"].includes(ext)) return "audio";
    return "other";
  }

  const totalSize = files.reduce((acc, f) => acc + (f.size || 0), 0);

  const filteredFiles = files.filter(f => {
    const matchFilter = filter === "all" || getFileType(f.name) === filter;
    const matchSearch = !search || f.name?.toLowerCase().includes(search.toLowerCase());
    return matchFilter && matchSearch;
  });

  const stats = {
    images: files.filter(f => getFileType(f.name) === "image").length,
    videos: files.filter(f => getFileType(f.name) === "video").length,
  };

  return (
    <div style={s.container}>

      {/* Header */}
      <div style={s.header}>
        <span style={{ fontSize: 32 }}>📁</span>
        <div style={{ flex: 1 }}>
          <h1 style={s.title}>TransferBridge</h1>
          <p style={s.subtitle}>{t.appSubtitle}</p>
        </div>
        <div style={{
          ...s.badge,
          background: status === "running" ? "#14532d" : "#1e293b",
          color: status === "running" ? "#86efac" : "#94a3b8"
        }}>
          {status === "running" ? t.statusActive : status === "starting" ? t.statusStarting : t.statusInactive}
        </div>

        {/* Badge Pro */}
        {isPro ? (
          <div style={s.proBadge}>⚡ Pro</div>
        ) : (
          <button onClick={() => setShowProModal(true)} style={s.proBtn}>
            ⚡ Passer Pro
          </button>
        )}
      </div>

      {/* Dossier destination */}
      <div style={s.dirBar}>
        <span style={{ color: "#64748b", fontSize: 13 }}>{t.destination}</span>
        <span style={s.dirPath}>{saveDir || "..."}</span>
        <button onClick={chooseSaveDir} style={s.dirBtn}>{t.changeFolder}</button>
      </div>

      {/* Erreur session */}
      {sessionError && (
        <div style={s.errorBanner}>{sessionError}</div>
      )}

      {/* Boutons feedback + paramètres — UNE SEULE FOIS */}
      <div style={{ display: "flex", justifyContent: "flex-end", gap: 8, marginBottom: 12 }}>
        <button onClick={() => setShowFeedback(true)} style={s.feedbackBtn}>
          💬 Feedback
        </button>
        <button onClick={() => setShowSettings(!showSettings)} style={s.settingsBtn}>
          {t.settings}
        </button>
      </div>

      {/* Panneau paramètres */}
      {showSettings && (
        <div style={s.settingsPanel}>
          <h3 style={s.settingsTitle}>{t.settingsTitle}</h3>

          {/* Limite taille */}
          <div style={s.settingRow}>
            <div>
              <div style={s.settingLabel}>{t.limitLabel}</div>
              <div style={s.settingDesc}>{t.limitDesc}</div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <input
                type="number" min="1" max="10000"
                value={maxSizeMb}
                onChange={e => updateMaxSize(e.target.value)}
                style={s.sizeInput}
              />
              <span style={{ fontSize: 13, color: "#94a3b8" }}>MB</span>
            </div>
          </div>

          {/* Préréglages */}
          <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap" }}>
            {[100, 500, 1000, 2000].map(size => (
              <button
                key={size}
                onClick={() => updateMaxSize(size)}
                style={{
                  ...s.presetBtn,
                  background: maxSizeMb === size ? "#3b82f6" : "#0f172a",
                  color: maxSizeMb === size ? "white" : "#64748b",
                }}
              >
                {size >= 1000 ? `${size/1000}GB` : `${size}MB`}
              </button>
            ))}
          </div>

          <div style={s.settingInfo}>{t.settingsInfo}</div>

          {/* Sélecteur de langue */}
          <div style={{ ...s.settingRow, borderBottom: "none", marginTop: 8 }}>
            <div style={s.settingLabel}>{t.language}</div>
            <div style={{ display: "flex", gap: 8 }}>
              {Object.entries(languageNames).map(([code, name]) => (
                <button
                  key={code}
                  onClick={() => changeLanguage(code)}
                  style={{
                    ...s.presetBtn,
                    background: lang === code ? "#3b82f6" : "#0f172a",
                    color: lang === code ? "white" : "#64748b",
                    fontSize: 13,
                  }}
                >
                  {name}
                </button>
              ))}
            </div>
          </div>
        </div>
      )}

      <div style={s.grid}>

        {/* Panneau gauche : QR + PIN + SendToPhone */}
        <div style={s.card}>
          <h2 style={s.cardTitle}>{t.scanTitle}</h2>
          {qrCode ? (
            <>
              <img src={qrCode} alt="QR Code" style={s.qr} />
              <p style={s.urlText}>{serverUrl}</p>
            </>
          ) : (
            <div style={s.empty}>⏳ {t.statusStarting}</div>
          )}

          {/* PIN */}
          <div style={s.pinBlock}>
            <div style={s.pinHeader}>
              <span style={{ fontSize: 13, color: "#94a3b8" }}>{t.pinCode}</span>
              {lastDevice && (
                <span style={{ fontSize: 11, color: "#22c55e" }}>{t.connectedAt} {lastDevice}</span>
              )}
            </div>
            <div style={s.pinDisplay}>
              {showPin
                ? pin.split("").map((d, i) => <div key={i} style={s.pinDigit}>{d}</div>)
                : [0,1,2,3].map(i => <div key={i} style={s.pinDigit}>●</div>)
              }
              <button onClick={() => setShowPin(!showPin)} style={s.pinToggle}>
                {showPin ? "🙈" : "👁️"}
              </button>
            </div>
            <p style={{ fontSize: 11, color: "#475569", textAlign: "center", marginTop: 6 }}>
              {t.pinHint}
            </p>
            <button onClick={regeneratePin} style={s.regenBtn}>
              {t.regeneratePin}
            </button>
          </div>

          {/* Envoyer vers le téléphone */}
          {status === "running" && <SendToPhone t={t} />}

          {/* Stats */}
          {files.length > 0 && (
            <div style={s.statsGrid}>
              <div style={s.statItem}>
                <span style={s.statNum}>{files.length}</span>
                <span style={s.statLabel}>{t.statsTotal}</span>
              </div>
              <div style={s.statItem}>
                <span style={s.statNum}>{stats.images}</span>
                <span style={s.statLabel}>{t.statsPhotos}</span>
              </div>
              <div style={s.statItem}>
                <span style={s.statNum}>{stats.videos}</span>
                <span style={s.statLabel}>{t.statsVideos}</span>
              </div>
              <div style={s.statItem}>
                <span style={s.statNum}>{formatSize(totalSize)}</span>
                <span style={s.statLabel}>{t.statsVolume}</span>
              </div>
            </div>
          )}
        </div>

        {/* Panneau droit : historique */}
        <div style={s.card}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 12 }}>
            <h2 style={{ ...s.cardTitle, marginBottom: 0 }}>
              {t.historyTitle}
              {files.length > 0 && <span style={s.count}>{files.length}</span>}
            </h2>
            {files.length > 0 && (
              <button onClick={clearHistory} style={s.clearBtn}>{t.clearHistory}</button>
            )}
          </div>

          {files.length > 0 && (
            <>
              <input
                type="text"
                placeholder={t.searchPlaceholder}
                value={search}
                onChange={e => setSearch(e.target.value)}
                style={s.searchInput}
              />
              <div style={s.filters}>
                {[
                  { key: "all",   label: t.filterAll },
                  { key: "image", label: "🖼️" },
                  { key: "video", label: "🎬" },
                  { key: "pdf",   label: "📄" },
                  { key: "audio", label: "🎵" },
                  { key: "other", label: "📎" },
                ].map(f => (
                  <button
                    key={f.key}
                    onClick={() => setFilter(f.key)}
                    style={{
                      ...s.filterBtn,
                      background: filter === f.key ? "#3b82f6" : "#0f172a",
                      color: filter === f.key ? "white" : "#64748b",
                    }}
                  >
                    {f.label}
                  </button>
                ))}
              </div>
            </>
          )}

          {filteredFiles.length === 0 ? (
            <div style={s.empty}>
              {files.length === 0 ? (
                <>
                  <div style={{ fontSize: 40, marginBottom: 12 }}>📭</div>
                  <p>{t.emptyHistory}</p>
                  <p style={{ fontSize: 12, marginTop: 4, color: "#475569" }}>{t.emptyHistoryHint}</p>
                </>
              ) : (
                <>
                  <div style={{ fontSize: 32, marginBottom: 8 }}>🔍</div>
                  <p>{t.noResults}</p>
                </>
              )}
            </div>
          ) : (
            <div style={s.fileList}>
              {filteredFiles.map((file, i) => (
                <div key={i} style={s.fileItem}>
                  <span style={{ fontSize: 24 }}>{getFileIcon(file.name)}</span>
                  <div style={{ flex: 1, minWidth: 0 }}>
                    <div style={s.fileName}>{file.name}</div>
                    <div style={s.fileMeta}>
                      {formatSize(file.size)}
                      {file.date && <span> · {file.date}</span>}
                      {file.time && <span> · {file.time}</span>}
                    </div>
                    {file.progress !== undefined && file.progress < 100 && (
                      <div style={s.progressWrap}>
                        <div style={s.progressBar}>
                          <div style={{
                            ...s.progressFill,
                            width: `${file.progress}%`,
                            background: "linear-gradient(90deg, #3b82f6, #06b6d4)"
                          }} />
                        </div>
                        <span style={s.progressLabel}>{file.progress}%</span>
                      </div>
                    )}
                  </div>
                  <span style={{ fontSize: 16 }}>✅</span>
                </div>
              ))}
            </div>
          )}
        </div>

      </div>

      {/* Modals */}
      {showFeedback && (
        <Feedback onClose={() => setShowFeedback(false)} t={t} />
      )}
      {showProModal && (
        <ProActivation
          onActivated={() => setIsPro(true)}
          onClose={() => setShowProModal(false)}
        />
      )}
      <Updater />

    </div>
  );
}

const s = {
  container: {
    background: "#0f172a", minHeight: "100vh", color: "#f1f5f9",
    fontFamily: "-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
    padding: "20px",
  },
  header: {
    display: "flex", alignItems: "center", gap: 14, marginBottom: 12,
    padding: "14px 18px", background: "#1e293b", borderRadius: 14,
    flexWrap: "wrap",
  },
  title: { fontSize: 18, fontWeight: 700, margin: 0 },
  subtitle: { fontSize: 12, color: "#94a3b8", margin: 0 },
  badge: { padding: "6px 12px", borderRadius: 20, fontSize: 13, fontWeight: 600 },
  proBadge: {
    padding: "6px 12px", background: "rgba(251,191,36,0.15)",
    border: "1px solid rgba(251,191,36,0.3)", borderRadius: 20,
    fontSize: 13, fontWeight: 600, color: "#FBBF24",
  },
  proBtn: {
    padding: "6px 14px",
    background: "linear-gradient(135deg, #3b82f6, #06b6d4)",
    color: "white", border: "none", borderRadius: 20,
    fontSize: 13, fontWeight: 600, cursor: "pointer",
  },
  dirBar: {
    display: "flex", alignItems: "center", gap: 10, marginBottom: 16,
    padding: "10px 16px", background: "#1e293b", borderRadius: 10,
  },
  dirPath: {
    flex: 1, fontSize: 12, color: "#94a3b8", fontFamily: "monospace",
    overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
  },
  dirBtn: {
    padding: "5px 12px", background: "#334155", color: "#94a3b8",
    border: "none", borderRadius: 8, fontSize: 12, cursor: "pointer",
  },
  errorBanner: {
    background: "#450a0a", color: "#fca5a5", padding: "10px 16px",
    borderRadius: 10, fontSize: 13, marginBottom: 12, border: "1px solid #7f1d1d",
  },
  feedbackBtn: {
    padding: "6px 14px", background: "rgba(59,130,246,0.1)",
    color: "#60a5fa", border: "1px solid rgba(59,130,246,0.25)",
    borderRadius: 8, fontSize: 13, cursor: "pointer",
  },
  settingsBtn: {
    padding: "6px 14px", background: "#1e293b", color: "#94a3b8",
    border: "1px solid #334155", borderRadius: 8, fontSize: 13, cursor: "pointer",
  },
  settingsPanel: {
    background: "#1e293b", borderRadius: 14, padding: 20, marginBottom: 16,
    border: "1px solid #334155",
  },
  settingsTitle: { fontSize: 15, fontWeight: 600, marginBottom: 16, color: "#f1f5f9" },
  settingRow: {
    display: "flex", justifyContent: "space-between", alignItems: "center",
    padding: "12px 0", borderBottom: "1px solid #334155",
  },
  settingLabel: { fontSize: 14, fontWeight: 500, marginBottom: 2 },
  settingDesc: { fontSize: 12, color: "#64748b" },
  sizeInput: {
    width: 80, padding: "6px 10px", background: "#0f172a",
    border: "1px solid #334155", borderRadius: 8,
    color: "#f1f5f9", fontSize: 14, textAlign: "center", outline: "none",
  },
  presetBtn: { padding: "5px 12px", border: "none", borderRadius: 8, fontSize: 12, cursor: "pointer" },
  settingInfo: { marginTop: 12, fontSize: 11, color: "#475569", textAlign: "center" },
  grid: { display: "grid", gridTemplateColumns: "1fr 1fr", gap: 16 },
  card: { background: "#1e293b", borderRadius: 14, padding: 20 },
  cardTitle: { fontSize: 15, fontWeight: 600, marginBottom: 16, display: "flex", alignItems: "center", gap: 8 },
  count: { background: "#3b82f6", color: "white", borderRadius: 12, padding: "2px 8px", fontSize: 12 },
  qr: { display: "block", margin: "0 auto 8px", borderRadius: 10, border: "4px solid #f8fafc" },
  urlText: { textAlign: "center", fontSize: 11, color: "#3b82f6", fontFamily: "monospace", marginBottom: 12, wordBreak: "break-all" },
  pinBlock: { background: "#0f172a", borderRadius: 12, padding: "14px 16px", marginTop: 4 },
  pinHeader: { display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 10 },
  pinDisplay: { display: "flex", justifyContent: "center", alignItems: "center", gap: 10 },
  pinDigit: {
    width: 44, height: 52, background: "#1e293b", border: "2px solid #3b82f6",
    borderRadius: 10, display: "flex", alignItems: "center", justifyContent: "center",
    fontSize: 22, fontWeight: 700, color: "#f1f5f9",
  },
  pinToggle: { background: "transparent", border: "none", cursor: "pointer", fontSize: 18, marginLeft: 8 },
  regenBtn: {
    width: "100%", marginTop: 10, padding: "8px 12px",
    background: "transparent", color: "#3b82f6",
    border: "1px solid #3b82f6", borderRadius: 8,
    fontSize: 12, cursor: "pointer",
  },
  statsGrid: { display: "grid", gridTemplateColumns: "1fr 1fr 1fr 1fr", gap: 8, marginTop: 16 },
  statItem: {
    background: "#0f172a", borderRadius: 10, padding: "10px 8px",
    display: "flex", flexDirection: "column", alignItems: "center", gap: 2,
  },
  statNum: { fontSize: 16, fontWeight: 700, color: "#3b82f6" },
  statLabel: { fontSize: 10, color: "#64748b" },
  searchInput: {
    width: "100%", padding: "8px 12px", background: "#0f172a",
    border: "1px solid #334155", borderRadius: 8, color: "#f1f5f9",
    fontSize: 13, marginBottom: 10, outline: "none",
  },
  filters: { display: "flex", gap: 6, marginBottom: 12, flexWrap: "wrap" },
  filterBtn: { padding: "4px 10px", borderRadius: 20, border: "none", fontSize: 12, cursor: "pointer" },
  empty: { textAlign: "center", color: "#64748b", padding: "32px 0" },
  fileList: { display: "flex", flexDirection: "column", gap: 8, maxHeight: 320, overflowY: "auto" },
  fileItem: { display: "flex", alignItems: "center", gap: 10, background: "#0f172a", borderRadius: 10, padding: "10px 12px" },
  fileName: { fontSize: 13, fontWeight: 500, whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis" },
  fileMeta: { fontSize: 11, color: "#64748b" },
  clearBtn: { padding: "4px 10px", background: "transparent", color: "#475569", border: "1px solid #334155", borderRadius: 8, fontSize: 12, cursor: "pointer" },
  progressWrap: { marginTop: 4 },
  progressBar: { width: "100%", height: 4, background: "#334155", borderRadius: 2, overflow: "hidden" },
  progressFill: { height: "100%", borderRadius: 2, transition: "width 0.15s ease" },
  progressLabel: { fontSize: 11, color: "#64748b" },
};