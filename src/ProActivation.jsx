import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

export default function ProActivation({ onActivated, onClose }) {
  const [key, setKey] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState(null);
  const [success, setSuccess] = useState(false);

  async function activate() {
    const cleaned = key.trim().toUpperCase();
    if (!cleaned.startsWith("TB-") || cleaned.split("-").length !== 5) {
      setError("Format invalide. La clé doit commencer par TB-XXXX-XXXX-XXXX-XXXX");
      return;
    }
    setLoading(true);
    setError(null);
    try {
      await invoke("activate_license", { key: cleaned });
      setSuccess(true);
      setTimeout(() => { onActivated(); onClose(); }, 2000);
    } catch (e) {
      setError(e.toString().includes("invalide")
        ? "❌ Clé invalide ou déjà utilisée."
        : "❌ Erreur de vérification. Vérifie ta connexion.");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div style={s.overlay} onClick={e => e.target === e.currentTarget && onClose()}>
      <div style={s.modal}>
        <div style={s.header}>
          <div>
            <h2 style={s.title}>⚡ Activer TransferBridge Pro</h2>
            <p style={s.sub}>Entre ta clé de licence pour débloquer toutes les fonctionnalités</p>
          </div>
          <button onClick={onClose} style={s.closeBtn}>✕</button>
        </div>

        {!success ? (
          <div style={s.body}>
            {/* Features Pro */}
            <div style={s.featuresGrid}>
              {[
                { icon: "∞", label: "Taille illimitée" },
                { icon: "☁️", label: "Mode Relay cloud" },
                { icon: "🔒", label: "Chiffrement E2E" },
                { icon: "📱", label: "5 appareils" },
              ].map((f, i) => (
                <div key={i} style={s.featureItem}>
                  <span style={s.featureIcon}>{f.icon}</span>
                  <span style={s.featureLabel}>{f.label}</span>
                </div>
              ))}
            </div>

            {/* Input clé */}
            <p style={s.label}>🔑 Clé de licence</p>
            <input
              type="text"
              value={key}
              onChange={e => { setKey(e.target.value.toUpperCase()); setError(null); }}
              placeholder="TB-XXXX-XXXX-XXXX-XXXX"
              style={s.input}
              spellCheck={false}
            />

            {error && <div style={s.error}>{error}</div>}

            <button
              onClick={activate}
              disabled={loading || !key.trim()}
              style={{
                ...s.activateBtn,
                background: loading || !key.trim() ? "#334155" : "#3b82f6",
                cursor: loading || !key.trim() ? "not-allowed" : "pointer",
              }}
            >
              {loading ? "⏳ Vérification..." : "⚡ Activer Pro"}
            </button>

            <div style={s.divider}>
              <span>Pas encore de licence ?</span>
            </div>

            <button
              onClick={() => {
                const { shell } = window.__TAURI__;
                shell?.open("https://transferbridge.site/checkout");
              }}
              style={s.buyBtn}
            >
              💳 Acheter pour 19.99€ →
            </button>

            <p style={s.hint}>
              Disponible par Carte bancaire, PayPal ou Mobile Money (Afrique)
            </p>
          </div>
        ) : (
          <div style={{ textAlign: "center", padding: "40px 24px" }}>
            <div style={{ fontSize: 64, marginBottom: 16 }}>🎉</div>
            <h3 style={{ fontSize: 20, fontWeight: 700, marginBottom: 8 }}>
              TransferBridge Pro activé !
            </h3>
            <p style={{ color: "#94a3b8", fontSize: 14 }}>
              Toutes les fonctionnalités Pro sont maintenant disponibles.
            </p>
          </div>
        )}
      </div>
    </div>
  );
}

const s = {
  overlay: { position:"fixed", inset:0, background:"rgba(0,0,0,0.7)", backdropFilter:"blur(4px)", display:"flex", alignItems:"center", justifyContent:"center", zIndex:1000, padding:20 },
  modal: { background:"#1e293b", border:"1px solid #334155", borderRadius:20, width:"100%", maxWidth:460, boxShadow:"0 40px 80px rgba(0,0,0,0.5)", overflow:"hidden" },
  header: { display:"flex", justifyContent:"space-between", alignItems:"flex-start", padding:"20px 24px 0" },
  title: { fontSize:18, fontWeight:700, margin:0 },
  sub: { fontSize:12, color:"#64748b", marginTop:2 },
  closeBtn: { background:"transparent", border:"none", color:"#64748b", fontSize:16, cursor:"pointer" },
  body: { padding:"20px 24px 24px" },
  featuresGrid: { display:"grid", gridTemplateColumns:"1fr 1fr", gap:8, marginBottom:20 },
  featureItem: { background:"rgba(59,130,246,0.08)", border:"1px solid rgba(59,130,246,0.15)", borderRadius:10, padding:"10px 12px", display:"flex", alignItems:"center", gap:8 },
  featureIcon: { fontSize:18 },
  featureLabel: { fontSize:13, fontWeight:500, color:"#94a3b8" },
  label: { fontSize:13, fontWeight:500, color:"#94a3b8", marginBottom:8 },
  input: { width:"100%", padding:"12px 14px", background:"#0f172a", border:"1px solid #334155", borderRadius:10, color:"#f1f5f9", fontSize:15, fontFamily:"monospace", letterSpacing:"0.05em", outline:"none" },
  error: { marginTop:8, padding:"8px 12px", background:"#450a0a", border:"1px solid #7f1d1d", borderRadius:8, color:"#fca5a5", fontSize:13 },
  activateBtn: { width:"100%", marginTop:16, padding:14, color:"white", border:"none", borderRadius:10, fontSize:15, fontWeight:600 },
  divider: { textAlign:"center", color:"#475569", fontSize:12, margin:"16px 0", borderTop:"1px solid #334155", paddingTop:16 },
  buyBtn: { width:"100%", padding:13, background:"transparent", color:"#3b82f6", border:"1px solid #3b82f6", borderRadius:10, fontSize:14, fontWeight:600, cursor:"pointer" },
  hint: { textAlign:"center", fontSize:11, color:"#475569", marginTop:8 },
};