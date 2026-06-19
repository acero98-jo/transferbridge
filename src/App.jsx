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
  const [showFeedback, setShowFeedback] = useState(false);
  const [showProModal, setShowProModal] = useState(false);

  // Tunnel Cloudflare (Pro)
  const [tunnelStatus, setTunnelStatus] = useState("inactive"); // inactive | starting | active | error
  const [tunnelUrl, setTunnelUrl] = useState(null);
  const [tunnelQrCode, setTunnelQrCode] = useState(null);
  const [showRemoteAccess, setShowRemoteAccess] = useState(false);

  // Plan & compteur
  const [planInfo, setPlanInfo] = useState({
    plan: "free",
    plan_label: "Gratuit",
    uploads_today: 0,
    uploads_limit: 10,
    uploads_left: 10,
    max_file_mb: 500,
    bidirectional: false,
    max_devices: 1,
    device_id: "",
  });

  const filesRef = useRef([]);
  const t = getT(lang);

  useEffect(() => { filesRef.current = files; }, [files]);

  useEffect(() => {
    async function init() {
      await initApp();
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
        setSessionError("Session expirée — reconnecte le téléphone");
      } else if (payload.error === "too_large") {
        setSessionError(payload.message);
      } else if (payload.error === "daily_limit") {
        setSessionError("Limite de 10 envois/jour atteinte. Passez à Pro !");
      }
      setTimeout(() => setSessionError(null), 6000);
    });

    // Mise à jour du compteur après chaque envoi
    const unlistenCounter = listen("counter-updated", (e) => {
      setPlanInfo(prev => ({
        ...prev,
        uploads_today: e.payload.uploads_today,
        uploads_left:  e.payload.uploads_left,
        uploads_limit: e.payload.uploads_limit,
      }));
    });

    const unlistenPlan = listen("plan-changed", () => {
      // Recharge les infos du plan
      invoke("get_plan_info").then(info => setPlanInfo(info)).catch(console.error);
    });

    // ── Tunnel Cloudflare (Pro) ──
    const unlistenTunnelStarting = listen("tunnel-starting", () => {
      setTunnelStatus("starting");
    });

    const unlistenTunnelReady = listen("tunnel-ready", async (e) => {
      const url = e.payload.url;
      setTunnelUrl(url);
      setTunnelStatus("active");
      try {
        const qr = await QRCode.toDataURL(url, {
          width: 200, margin: 2,
          color: { dark: "#1e293b", light: "#f8fafc" }
        });
        setTunnelQrCode(qr);
      } catch (err) { console.error(err); }
    });

    const unlistenTunnelError = listen("tunnel-error", (e) => {
      console.error("Tunnel error:", e.payload);
      setTunnelStatus("error");
    });

    const unlistenTunnelStopped = listen("tunnel-stopped", () => {
      setTunnelStatus("inactive");
      setTunnelUrl(null);
      setTunnelQrCode(null);
    });

    return () => {
      unlistenFile.then(f => f());
      unlistenPin.then(f => f());
      unlistenDevice.then(f => f());
      unlistenProgress.then(f => f());
      unlistenError.then(f => f());
      unlistenCounter.then(f => f());
      unlistenPlan.then(f => f());
      unlistenTunnelStarting.then(f => f());
      unlistenTunnelReady.then(f => f());
      unlistenTunnelError.then(f => f());
      unlistenTunnelStopped.then(f => f());
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

      const history = await invoke("load_history");
      if (Array.isArray(history) && history.length > 0) setFiles(history);

      const dir = await invoke("get_save_dir");
      setSaveDir(dir);

      // Démarre le serveur
      const url = await invoke("start_server");
      setServerUrl(url);
      setStatus("running");

      const qr = await QRCode.toDataURL(url, {
        width: 200, margin: 2,
        color: { dark: "#1e293b", light: "#f8fafc" }
      });
      setQrCode(qr);

      // Vérifie la licence et charge les infos du plan
      await invoke("check_license");
      const info = await invoke("get_plan_info");
      setPlanInfo(info);

      // Charge la config APRÈS
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
      const selected = await open({ directory: true, multiple: false, title: "Choisir le dossier" });
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
    await invoke("save_config", { config: { max_file_size_mb: val, allowed_extensions: [] } });
  }

  async function deactivateLicense() {
    try {
      await invoke("deactivate_license");
      const info = await invoke("get_plan_info");
      setPlanInfo(info);
    } catch (e) { console.error(e); }
  }

  async function restartTunnel() {
    try {
      setTunnelStatus("starting");
      await invoke("restart_tunnel");
    } catch (e) {
      console.error(e);
      setTunnelStatus("error");
    }
  }

  async function stopTunnel() {
    try {
      await invoke("stop_tunnel");
      setTunnelStatus("inactive");
      setTunnelUrl(null);
      setTunnelQrCode(null);
    } catch (e) { console.error(e); }
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

  const isFree = planInfo.plan === "free";
  const isLimitReached = isFree && planInfo.uploads_left !== null && planInfo.uploads_left <= 0;
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

  // Couleur du badge selon le plan
  const planColors = {
    free:    { bg: "#1e293b", color: "#94a3b8" },
    monthly: { bg: "rgba(59,130,246,0.15)", color: "#60a5fa", border: "rgba(59,130,246,0.3)" },
    annual:  { bg: "rgba(139,92,246,0.15)", color: "#a78bfa", border: "rgba(139,92,246,0.3)" },
    team:    { bg: "rgba(34,197,94,0.15)", color: "#4ade80", border: "rgba(34,197,94,0.3)" },
  };
  const planColor = planColors[planInfo.plan] || planColors.free;

  return (
    <div style={s.container}>

      {/* ── Header ── */}
      <div style={s.header}>
        <span style={{ fontSize: 28, flexShrink: 0 }}>📁</span>
        <div style={{ flex: 1, minWidth: 0 }}>
          <h1 style={s.title}>TransferBridge</h1>
          <p style={s.subtitle}>{t.appSubtitle}</p>
        </div>

        {/* Badge statut serveur */}
        <div style={{
          ...s.badge,
          background: status === "running" ? "#14532d" : "#1e293b",
          color: status === "running" ? "#86efac" : "#94a3b8",
        }}>
          {status === "running" ? "🟢" : status === "starting" ? "⏳" : "⭕"}
          <span style={{ display: "none" }}> {status === "running" ? t.statusActive : t.statusStarting}</span>
        </div>

        {/* Badge plan */}
        {isFree ? (
          <button onClick={() => setShowProModal(true)} style={s.proBtn}>
            ⚡ Passer Pro
          </button>
        ) : (
          <div style={{
            ...s.planBadge,
            background: planColor.bg,
            color: planColor.color,
            border: `1px solid ${planColor.border || "transparent"}`,
          }}>
            ⚡ {planInfo.plan_label}
          </div>
        )}
      </div>

      {/* ── Bandeau plan gratuit + compteur ── */}
      {isFree && (
        <div style={s.freeBanner}>
          <div style={s.freeBannerLeft}>
            <span style={{ fontSize: 13, color: "#94a3b8" }}>Plan Gratuit</span>
            <span style={{ fontSize: 12, color: isLimitReached ? "#ef4444" : "#64748b", marginLeft: 8 }}>
              {isLimitReached
                ? "⛔ Limite atteinte — revient demain ou passe à Pro"
                : `${planInfo.uploads_left ?? 10}/${planInfo.uploads_limit ?? 10} envois restants aujourd'hui`
              }
            </span>
          </div>
          <div style={s.counterWrap}>
            <div style={s.counterBar}>
              <div style={{
                ...s.counterFill,
                width: `${Math.min(100, ((planInfo.uploads_today || 0) / (planInfo.uploads_limit || 10)) * 100)}%`,
                background: isLimitReached ? "#ef4444"
                  : (planInfo.uploads_today || 0) >= 7 ? "#f59e0b"
                  : "#3b82f6",
              }} />
            </div>
            <button onClick={() => setShowProModal(true)} style={s.upgradeMiniBtn}>
              ⚡ Upgrade
            </button>
          </div>
        </div>
      )}

      {/* ── Alerte session / limite ── */}
      {sessionError && (
        <div style={{
          ...s.errorBanner,
          background: sessionError.includes("Limite") ? "#451a03" : "#450a0a",
          borderColor: sessionError.includes("Limite") ? "#92400e" : "#7f1d1d",
          color: sessionError.includes("Limite") ? "#fed7aa" : "#fca5a5",
        }}>
          ⚠️ {sessionError}
          {sessionError.includes("Limite") && (
            <button onClick={() => setShowProModal(true)} style={s.errorProBtn}>
              ⚡ Passer Pro
            </button>
          )}
        </div>
      )}

      {/* ── Dossier destination ── */}
      <div style={s.dirBar}>
        <span style={{ color: "#64748b", fontSize: 13, flexShrink: 0 }}>📂</span>
        <span style={s.dirPath}>{saveDir || "..."}</span>
        <button onClick={chooseSaveDir} style={s.dirBtn}>{t.changeFolder}</button>
      </div>

      {/* ── Boutons actions ── */}
      <div style={s.actionRow}>
        <button onClick={() => setShowFeedback(true)} style={s.feedbackBtn}>
          💬 Feedback
        </button>
        <button onClick={() => setShowSettings(!showSettings)} style={s.settingsBtn}>
          ⚙️ {t.settings}
        </button>
        {!isFree && (
          <button onClick={deactivateLicense} style={s.deactivateBtn} title="Désactiver sur cet appareil">
            🔓 Déconnecter
          </button>
        )}
      </div>

      {/* ── Panneau paramètres ── */}
      {showSettings && (
        <div style={s.settingsPanel}>
          <h3 style={s.settingsTitle}>{t.settingsTitle}</h3>

          <div style={s.settingRow}>
            <div>
              <div style={s.settingLabel}>{t.limitLabel}</div>
              <div style={s.settingDesc}>
                {isFree ? "Gratuit : max 500MB/fichier" : "Pro : illimité"}
              </div>
            </div>
            <div style={{ display: "flex", alignItems: "center", gap: 8 }}>
              <input
                type="number" min="1" max="10000"
                value={maxSizeMb}
                onChange={e => updateMaxSize(e.target.value)}
                disabled={!isFree}
                style={{ ...s.sizeInput, opacity: !isFree ? 0.5 : 1 }}
              />
              <span style={{ fontSize: 13, color: "#94a3b8" }}>MB</span>
            </div>
          </div>

          {isFree && (
            <div style={{ display: "flex", gap: 8, marginTop: 10, flexWrap: "wrap" }}>
              {[100, 250, 500].map(size => (
                <button key={size} onClick={() => updateMaxSize(size)} style={{
                  ...s.presetBtn,
                  background: maxSizeMb === size ? "#3b82f6" : "#0f172a",
                  color: maxSizeMb === size ? "white" : "#64748b",
                }}>
                  {size}MB
                </button>
              ))}
            </div>
          )}

          {/* Infos appareil */}
          {planInfo.device_id && (
            <div style={{ marginTop: 12, padding: "8px 12px", background: "#0f172a", borderRadius: 8 }}>
              <div style={{ fontSize: 11, color: "#475569" }}>ID appareil</div>
              <div style={{ fontSize: 11, fontFamily: "monospace", color: "#64748b", marginTop: 2 }}>
                {planInfo.device_id}
              </div>
            </div>
          )}

          <div style={s.settingInfo}>{t.settingsInfo}</div>

          {/* Langue */}
          <div style={{ ...s.settingRow, borderBottom: "none", marginTop: 8 }}>
            <div style={s.settingLabel}>{t.language}</div>
            <div style={{ display: "flex", gap: 8, flexWrap: "wrap" }}>
              {Object.entries(languageNames).map(([code, name]) => (
                <button key={code} onClick={() => changeLanguage(code)} style={{
                  ...s.presetBtn,
                  background: lang === code ? "#3b82f6" : "#0f172a",
                  color: lang === code ? "white" : "#64748b",
                  fontSize: 13,
                }}>
                  {name}
                </button>
              ))}
            </div>
          </div>
        </div>
      )}

      {/* ── Bandeau Tunnel Cloudflare (Pro seulement) ── */}
      {!isFree && (
        <div style={s.tunnelBanner}>
          <div style={s.tunnelLeft}>
            <span style={{
              ...s.tunnelDot,
              background: tunnelStatus === "active" ? "#22c55e"
                : tunnelStatus === "starting" ? "#f59e0b"
                : tunnelStatus === "error" ? "#ef4444"
                : "#475569",
              boxShadow: tunnelStatus === "active" ? "0 0 8px #22c55e" : "none",
            }} />
            <div>
              <div style={s.tunnelTitle}>
                🌐 Mode Relay cloud
                {tunnelStatus === "active" && <span style={s.tunnelLiveTag}>EN LIGNE</span>}
              </div>
              <div style={s.tunnelSub}>
                {tunnelStatus === "inactive" && "Inactif — démarre automatiquement"}
                {tunnelStatus === "starting" && "⏳ Établissement du tunnel sécurisé..."}
                {tunnelStatus === "active" && "Accessible depuis n'importe quel réseau (4G, autre Wi-Fi...)"}
                {tunnelStatus === "error" && "❌ Erreur — clique sur Relancer"}
              </div>
            </div>
          </div>
          <div style={s.tunnelActions}>
            {tunnelStatus === "active" && (
              <button onClick={() => setShowRemoteAccess(true)} style={s.tunnelViewBtn}>
                📡 Voir le QR distant
              </button>
            )}
            {(tunnelStatus === "error" || tunnelStatus === "inactive") && (
              <button onClick={restartTunnel} style={s.tunnelRestartBtn}>
                🔄 {tunnelStatus === "error" ? "Relancer" : "Activer"}
              </button>
            )}
            {tunnelStatus === "active" && (
              <button onClick={stopTunnel} style={s.tunnelStopBtn}>
                ⏹️
              </button>
            )}
          </div>
        </div>
      )}

      {/* Teaser Relay cloud pour gratuit */}
      {isFree && status === "running" && (
        <div style={s.tunnelTeaser} onClick={() => setShowProModal(true)}>
          <span style={{ fontSize: 16 }}>🌐</span>
          <span style={{ fontSize: 12, color: "#60a5fa", flex: 1 }}>
            Mode Relay cloud — Accède à ton PC depuis n'importe où, même hors Wi-Fi
          </span>
          <span style={s.tunnelTeaserLock}>⚡ Pro</span>
        </div>
      )}

      {/* ── Grille principale ── */}
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
            <button onClick={regeneratePin} style={s.regenBtn}>{t.regeneratePin}</button>
          </div>

          {/* SendToPhone — Pro seulement */}
          {status === "running" && !isFree && <SendToPhone t={t} />}

          {/* Teaser SendToPhone pour gratuit */}
          {isFree && status === "running" && (
            <div style={s.proFeatureTeaser} onClick={() => setShowProModal(true)}>
              <div style={s.teaserIcon}>📤</div>
              <div>
                <div style={s.teaserTitle}>Envoyer vers le téléphone</div>
                <div style={s.teaserSub}>Disponible avec le plan Pro</div>
              </div>
              <div style={s.teaserLock}>🔒</div>
            </div>
          )}

          {/* Stats */}
          {files.length > 0 && (
            <div style={s.statsGrid}>
              {[
                { num: files.length, label: t.statsTotal },
                { num: stats.images, label: t.statsPhotos },
                { num: stats.videos, label: t.statsVideos },
                { num: formatSize(totalSize), label: t.statsVolume },
              ].map((item, i) => (
                <div key={i} style={s.statItem}>
                  <span style={s.statNum}>{item.num}</span>
                  <span style={s.statLabel}>{item.label}</span>
                </div>
              ))}
            </div>
          )}
        </div>

        {/* Panneau droit : historique */}
        <div style={s.card}>
          <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center", marginBottom: 12 }}>
            <h2 style={{ ...s.cardTitle, marginBottom: 0 }}>
              {t.historyTitle}
              {files.length > 0 && <span style={s.count}>{files.length}</span>}
              {isFree && files.length > 0 && (
                <span style={s.historyLimitBadge}>7 jours</span>
              )}
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
                  <button key={f.key} onClick={() => setFilter(f.key)} style={{
                    ...s.filterBtn,
                    background: filter === f.key ? "#3b82f6" : "#0f172a",
                    color: filter === f.key ? "white" : "#64748b",
                  }}>
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
                  <span style={{ fontSize: 22, flexShrink: 0 }}>{getFileIcon(file.name)}</span>
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
                  <span style={{ fontSize: 14, flexShrink: 0 }}>✅</span>
                </div>
              ))}
            </div>
          )}

          {/* Upsell Pro dans l'historique pour gratuit */}
          {isFree && files.length > 0 && (
            <div style={s.historyUpsell} onClick={() => setShowProModal(true)}>
              <span style={{ fontSize: 14 }}>⚡</span>
              <span style={{ fontSize: 12, color: "#60a5fa" }}>
                Pro : historique illimité, pas de limite de temps
              </span>
              <span style={{ fontSize: 12, color: "#3b82f6", fontWeight: 600 }}>→</span>
            </div>
          )}
        </div>

      </div>

      {/* ── Modals ── */}
      {showFeedback && (
        <Feedback onClose={() => setShowFeedback(false)} t={t} />
      )}
      {showProModal && (
        <ProActivation
          onActivated={async () => {
            await invoke("check_license");
            const info = await invoke("get_plan_info");
            setPlanInfo(info);
          }}
          onClose={() => setShowProModal(false)}
        />
      )}
      {showRemoteAccess && (
        <div style={s.overlay} onClick={e => e.target === e.currentTarget && setShowRemoteAccess(false)}>
          <div style={s.remoteModal}>
            <div style={s.remoteHeader}>
              <div>
                <h2 style={s.remoteTitle}>🌐 Accès distant</h2>
                <p style={s.remoteSub}>Scanne ce QR depuis n'importe quel réseau (4G, autre Wi-Fi...)</p>
              </div>
              <button onClick={() => setShowRemoteAccess(false)} style={s.remoteCloseBtn}>✕</button>
            </div>
            <div style={s.remoteBody}>
              {tunnelQrCode ? (
                <>
                  <img src={tunnelQrCode} alt="QR distant" style={s.remoteQr} />
                  <p style={s.remoteUrl}>{tunnelUrl}</p>
                  <div style={s.remoteWarning}>
                    ⚠️ Cette URL transite par Cloudflare. Le PIN reste requis pour la sécurité.
                  </div>
                </>
              ) : (
                <div style={{ textAlign: "center", color: "#64748b", padding: 40 }}>
                  ⏳ Génération du QR code...
                </div>
              )}
            </div>
          </div>
        </div>
      )}
      <Updater />

    </div>
  );
}

// ─── Styles responsive ────────────────────────────────────────────
const s = {
  container: {
    background: "#0f172a",
    minHeight: "100vh",
    color: "#f1f5f9",
    fontFamily: "-apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif",
    padding: "clamp(10px, 2vw, 20px)",
    maxWidth: "1400px",
    margin: "0 auto",
  },
  header: {
    display: "flex",
    alignItems: "center",
    gap: "clamp(8px, 1.5vw, 14px)",
    marginBottom: 12,
    padding: "12px clamp(12px, 2vw, 18px)",
    background: "#1e293b",
    borderRadius: 14,
    flexWrap: "wrap",
  },
  title: { fontSize: "clamp(14px, 2vw, 18px)", fontWeight: 700, margin: 0 },
  subtitle: { fontSize: "clamp(10px, 1.2vw, 12px)", color: "#94a3b8", margin: 0 },
  badge: {
    padding: "5px 10px", borderRadius: 20, fontSize: 13, fontWeight: 600, flexShrink: 0,
  },
  planBadge: {
    padding: "5px 12px", borderRadius: 20, fontSize: 13, fontWeight: 600, flexShrink: 0,
  },
  proBtn: {
    padding: "6px 14px",
    background: "linear-gradient(135deg, #3b82f6, #06b6d4)",
    color: "white", border: "none", borderRadius: 20,
    fontSize: 13, fontWeight: 600, cursor: "pointer", flexShrink: 0,
    whiteSpace: "nowrap",
  },

  // Bandeau plan gratuit
  freeBanner: {
    display: "flex", alignItems: "center", justifyContent: "space-between",
    flexWrap: "wrap", gap: 8,
    background: "#1e293b", borderRadius: 10,
    padding: "8px 14px", marginBottom: 12,
  },
  freeBannerLeft: { display: "flex", alignItems: "center", flexWrap: "wrap", gap: 4 },
  counterWrap: { display: "flex", alignItems: "center", gap: 8 },
  counterBar: {
    width: 80, height: 6, background: "#334155", borderRadius: 3, overflow: "hidden",
  },
  counterFill: { height: "100%", borderRadius: 3, transition: "width 0.3s, background 0.3s" },
  upgradeMiniBtn: {
    padding: "4px 10px", background: "linear-gradient(135deg, #3b82f6, #06b6d4)",
    color: "white", border: "none", borderRadius: 8,
    fontSize: 11, fontWeight: 600, cursor: "pointer", whiteSpace: "nowrap",
  },

  dirBar: {
    display: "flex", alignItems: "center", gap: 10, marginBottom: 12,
    padding: "9px 14px", background: "#1e293b", borderRadius: 10,
    minWidth: 0,
  },
  dirPath: {
    flex: 1, fontSize: "clamp(10px, 1.2vw, 12px)", color: "#94a3b8",
    fontFamily: "monospace", overflow: "hidden", textOverflow: "ellipsis", whiteSpace: "nowrap",
  },
  dirBtn: {
    padding: "4px 10px", background: "#334155", color: "#94a3b8",
    border: "none", borderRadius: 8, fontSize: 12, cursor: "pointer", flexShrink: 0,
  },

  errorBanner: {
    padding: "10px 14px", borderRadius: 10, fontSize: 13,
    marginBottom: 12, border: "1px solid",
    display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap",
  },
  errorProBtn: {
    marginLeft: "auto", padding: "4px 10px",
    background: "#3b82f6", color: "white",
    border: "none", borderRadius: 6, fontSize: 12,
    fontWeight: 600, cursor: "pointer",
  },

  actionRow: {
    display: "flex", justifyContent: "flex-end", gap: 8,
    marginBottom: 12, flexWrap: "wrap",
  },
  feedbackBtn: {
    padding: "6px 12px", background: "rgba(59,130,246,0.1)",
    color: "#60a5fa", border: "1px solid rgba(59,130,246,0.25)",
    borderRadius: 8, fontSize: 12, cursor: "pointer",
  },
  settingsBtn: {
    padding: "6px 12px", background: "#1e293b", color: "#94a3b8",
    border: "1px solid #334155", borderRadius: 8, fontSize: 12, cursor: "pointer",
  },
  deactivateBtn: {
    padding: "6px 12px", background: "transparent", color: "#475569",
    border: "1px solid #334155", borderRadius: 8, fontSize: 12, cursor: "pointer",
  },

  settingsPanel: {
    background: "#1e293b", borderRadius: 14, padding: "16px 20px",
    marginBottom: 16, border: "1px solid #334155",
  },
  settingsTitle: { fontSize: 14, fontWeight: 600, marginBottom: 14, color: "#f1f5f9" },
  settingRow: {
    display: "flex", justifyContent: "space-between", alignItems: "center",
    padding: "10px 0", borderBottom: "1px solid #334155", flexWrap: "wrap", gap: 8,
  },
  settingLabel: { fontSize: 13, fontWeight: 500, marginBottom: 2 },
  settingDesc: { fontSize: 11, color: "#64748b" },
  sizeInput: {
    width: 70, padding: "5px 8px", background: "#0f172a",
    border: "1px solid #334155", borderRadius: 8,
    color: "#f1f5f9", fontSize: 13, textAlign: "center", outline: "none",
  },
  presetBtn: { padding: "4px 10px", border: "none", borderRadius: 8, fontSize: 12, cursor: "pointer" },
  settingInfo: { marginTop: 10, fontSize: 11, color: "#475569", textAlign: "center" },

  // Grille responsive
  grid: {
    display: "grid",
    gridTemplateColumns: "repeat(auto-fit, minmax(min(100%, 340px), 1fr))",
    gap: "clamp(10px, 2vw, 16px)",
  },
  card: {
    background: "#1e293b", borderRadius: 14,
    padding: "clamp(14px, 2vw, 20px)",
    minWidth: 0,
  },
  cardTitle: {
    fontSize: "clamp(13px, 1.5vw, 15px)", fontWeight: 600,
    marginBottom: 14, display: "flex", alignItems: "center", gap: 8, flexWrap: "wrap",
  },
  count: {
    background: "#3b82f6", color: "white", borderRadius: 12,
    padding: "2px 7px", fontSize: 11,
  },
  historyLimitBadge: {
    background: "#334155", color: "#64748b", borderRadius: 8,
    padding: "2px 7px", fontSize: 10,
  },
  qr: {
    display: "block", margin: "0 auto 8px",
    borderRadius: 10, border: "4px solid #f8fafc",
    maxWidth: 180, width: "100%",
  },
  urlText: {
    textAlign: "center", fontSize: 10, color: "#3b82f6",
    fontFamily: "monospace", marginBottom: 12, wordBreak: "break-all",
  },
  pinBlock: {
    background: "#0f172a", borderRadius: 12,
    padding: "12px 14px", marginTop: 4,
  },
  pinHeader: {
    display: "flex", justifyContent: "space-between",
    alignItems: "center", marginBottom: 10, flexWrap: "wrap", gap: 4,
  },
  pinDisplay: { display: "flex", justifyContent: "center", alignItems: "center", gap: 8 },
  pinDigit: {
    width: 40, height: 48, background: "#1e293b", border: "2px solid #3b82f6",
    borderRadius: 10, display: "flex", alignItems: "center", justifyContent: "center",
    fontSize: 20, fontWeight: 700, color: "#f1f5f9",
  },
  pinToggle: {
    background: "transparent", border: "none", cursor: "pointer",
    fontSize: 16, marginLeft: 6,
  },
  regenBtn: {
    width: "100%", marginTop: 8, padding: "7px 10px",
    background: "transparent", color: "#3b82f6",
    border: "1px solid #3b82f6", borderRadius: 8,
    fontSize: 11, cursor: "pointer",
  },

  // Teaser Pro
  proFeatureTeaser: {
    display: "flex", alignItems: "center", gap: 10,
    background: "rgba(59,130,246,0.05)", border: "1px dashed rgba(59,130,246,0.2)",
    borderRadius: 10, padding: "10px 12px", marginTop: 10, cursor: "pointer",
    transition: "all 0.2s",
  },
  teaserIcon: { fontSize: 22, flexShrink: 0 },
  teaserTitle: { fontSize: 13, fontWeight: 600, color: "#60a5fa" },
  teaserSub: { fontSize: 11, color: "#475569", marginTop: 1 },
  teaserLock: { fontSize: 16, marginLeft: "auto", flexShrink: 0 },

  statsGrid: {
    display: "grid", gridTemplateColumns: "repeat(4, 1fr)",
    gap: 6, marginTop: 14,
  },
  statItem: {
    background: "#0f172a", borderRadius: 10, padding: "8px 4px",
    display: "flex", flexDirection: "column", alignItems: "center", gap: 2,
  },
  statNum: { fontSize: "clamp(13px, 1.5vw, 16px)", fontWeight: 700, color: "#3b82f6" },
  statLabel: { fontSize: 9, color: "#64748b", textAlign: "center" },

  searchInput: {
    width: "100%", padding: "7px 11px", background: "#0f172a",
    border: "1px solid #334155", borderRadius: 8, color: "#f1f5f9",
    fontSize: 12, marginBottom: 8, outline: "none",
  },
  filters: { display: "flex", gap: 5, marginBottom: 10, flexWrap: "wrap" },
  filterBtn: {
    padding: "3px 9px", borderRadius: 20, border: "none",
    fontSize: 11, cursor: "pointer",
  },
  empty: { textAlign: "center", color: "#64748b", padding: "28px 0" },
  fileList: {
    display: "flex", flexDirection: "column", gap: 7,
    maxHeight: "clamp(200px, 40vh, 350px)", overflowY: "auto",
  },
  fileItem: {
    display: "flex", alignItems: "center", gap: 9,
    background: "#0f172a", borderRadius: 10, padding: "9px 11px",
  },
  fileName: {
    fontSize: "clamp(11px, 1.3vw, 13px)", fontWeight: 500,
    whiteSpace: "nowrap", overflow: "hidden", textOverflow: "ellipsis",
  },
  fileMeta: { fontSize: 10, color: "#64748b" },
  clearBtn: {
    padding: "3px 9px", background: "transparent", color: "#475569",
    border: "1px solid #334155", borderRadius: 8, fontSize: 11, cursor: "pointer",
  },
  progressWrap: { marginTop: 3 },
  progressBar: { width: "100%", height: 3, background: "#334155", borderRadius: 2, overflow: "hidden" },
  progressFill: { height: "100%", borderRadius: 2, transition: "width 0.15s ease" },
  progressLabel: { fontSize: 10, color: "#64748b" },

  historyUpsell: {
    display: "flex", alignItems: "center", gap: 8,
    marginTop: 10, padding: "8px 12px",
    background: "rgba(59,130,246,0.06)", border: "1px solid rgba(59,130,246,0.15)",
    borderRadius: 8, cursor: "pointer",
  },

  // ── Tunnel Cloudflare ──
  tunnelBanner: {
    display: "flex", alignItems: "center", justifyContent: "space-between",
    flexWrap: "wrap", gap: 10,
    background: "linear-gradient(135deg, rgba(34,197,94,0.06), rgba(6,182,212,0.04))",
    border: "1px solid rgba(34,197,94,0.15)",
    borderRadius: 10, padding: "10px 14px", marginBottom: 12,
  },
  tunnelLeft: { display: "flex", alignItems: "center", gap: 10, flex: 1, minWidth: 0 },
  tunnelDot: { width: 8, height: 8, borderRadius: "50%", flexShrink: 0, transition: "all 0.3s" },
  tunnelTitle: { fontSize: 13, fontWeight: 600, color: "#f1f5f9", display: "flex", alignItems: "center", gap: 8 },
  tunnelLiveTag: {
    fontSize: 9, fontWeight: 700, color: "#22c55e",
    background: "rgba(34,197,94,0.15)", padding: "2px 6px", borderRadius: 4,
  },
  tunnelSub: { fontSize: 11, color: "#64748b", marginTop: 2 },
  tunnelActions: { display: "flex", gap: 6, flexShrink: 0 },
  tunnelViewBtn: {
    padding: "6px 12px", background: "#22c55e", color: "white",
    border: "none", borderRadius: 8, fontSize: 12, fontWeight: 600, cursor: "pointer",
  },
  tunnelRestartBtn: {
    padding: "6px 12px", background: "rgba(59,130,246,0.15)", color: "#60a5fa",
    border: "1px solid rgba(59,130,246,0.3)", borderRadius: 8, fontSize: 12, fontWeight: 600, cursor: "pointer",
  },
  tunnelStopBtn: {
    padding: "6px 10px", background: "transparent", color: "#64748b",
    border: "1px solid #334155", borderRadius: 8, fontSize: 12, cursor: "pointer",
  },
  tunnelTeaser: {
    display: "flex", alignItems: "center", gap: 10,
    background: "rgba(59,130,246,0.05)", border: "1px dashed rgba(59,130,246,0.2)",
    borderRadius: 10, padding: "10px 14px", marginBottom: 12, cursor: "pointer",
  },
  tunnelTeaserLock: {
    fontSize: 11, fontWeight: 700, color: "#3b82f6",
    background: "rgba(59,130,246,0.12)", padding: "3px 8px", borderRadius: 6, flexShrink: 0,
  },

  // ── Modal accès distant ──
  overlay: {
    position: "fixed", inset: 0,
    background: "rgba(0,0,0,0.75)", backdropFilter: "blur(6px)",
    display: "flex", alignItems: "center", justifyContent: "center",
    zIndex: 1000, padding: 20,
  },
  remoteModal: {
    background: "#1e293b", border: "1px solid #334155",
    borderRadius: 20, width: "100%", maxWidth: 420,
    boxShadow: "0 40px 80px rgba(0,0,0,0.6)",
  },
  remoteHeader: {
    display: "flex", justifyContent: "space-between", alignItems: "flex-start",
    padding: "20px 22px 0",
  },
  remoteTitle: { fontSize: 17, fontWeight: 700, margin: 0 },
  remoteSub: { fontSize: 12, color: "#64748b", marginTop: 4, lineHeight: 1.5 },
  remoteCloseBtn: {
    background: "transparent", border: "none", color: "#64748b",
    fontSize: 16, cursor: "pointer", flexShrink: 0,
  },
  remoteBody: { padding: "18px 22px 24px", textAlign: "center" },
  remoteQr: {
    display: "block", margin: "0 auto 12px",
    borderRadius: 10, border: "4px solid #f8fafc", maxWidth: 200, width: "100%",
  },
  remoteUrl: {
    fontSize: 11, color: "#22c55e", fontFamily: "monospace",
    wordBreak: "break-all", marginBottom: 14,
  },
  remoteWarning: {
    fontSize: 11, color: "#94a3b8", background: "#0f172a",
    borderRadius: 8, padding: "10px 12px", lineHeight: 1.5, textAlign: "left",
  },
};