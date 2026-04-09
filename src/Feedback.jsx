import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";

const CATEGORIES = [
  { key: "bug",         label: "🐛 Bug",         desc: "Quelque chose ne fonctionne pas" },
  { key: "feature",     label: "💡 Idée",         desc: "Suggérer une fonctionnalité" },
  { key: "performance", label: "⚡ Performance",   desc: "Lenteur, crash, mémoire" },
  { key: "ux",          label: "🎨 UX/Design",     desc: "Interface, ergonomie" },
  { key: "general",     label: "💬 Général",       desc: "Autre chose" },
];

export default function Feedback({ onClose, t }) {
  const [step, setStep]         = useState(1); // 1=rating, 2=form, 3=done
  const [rating, setRating]     = useState(0);
  const [hovered, setHovered]   = useState(0);
  const [category, setCategory] = useState("");
  const [message, setMessage]   = useState("");
  const [email, setEmail]       = useState("");
  const [loading, setLoading]   = useState(false);
  const [error, setError]       = useState(null);

  async function submit() {
    if (!message.trim()) { setError("Merci d'écrire un message."); return; }
    setLoading(true); setError(null);
    try {
      await invoke("send_feedback", {
        payload: {
          rating,
          category: category || "general",
          message: message.trim(),
          email: email.trim() || null,
          app_version: "1.0.0",
          os: navigator.platform || "Windows",
        }
      });
      setStep(3);
    } catch (e) {
      setError("Erreur d'envoi. Vérifie ta connexion internet.");
    } finally {
      setLoading(false);
    }
  }

  return (
    <div style={s.overlay} onClick={e => e.target === e.currentTarget && onClose()}>
      <div style={s.modal}>

        {/* Header */}
        <div style={s.header}>
          <div>
            <h2 style={s.title}>💬 Ton avis nous aide</h2>
            <p style={s.subtitle}>Anonyme · 2 minutes max</p>
          </div>
          <button onClick={onClose} style={s.closeBtn}>✕</button>
        </div>

        {/* Step 1 — Note */}
        {step === 1 && (
          <div style={s.body}>
            <p style={s.question}>Comment tu trouves TransferBridge ?</p>
            <div style={s.stars}>
              {[1,2,3,4,5].map(n => (
                <button
                  key={n}
                  onMouseEnter={() => setHovered(n)}
                  onMouseLeave={() => setHovered(0)}
                  onClick={() => { setRating(n); setStep(2); }}
                  style={{
                    ...s.star,
                    color: n <= (hovered || rating) ? "#FBBF24" : "#334155",
                    transform: n <= (hovered || rating) ? "scale(1.2)" : "scale(1)",
                  }}
                >
                  ★
                </button>
              ))}
            </div>
            <div style={s.ratingLabels}>
              <span>Décevant</span>
              <span>Excellent !</span>
            </div>
            <p style={{ fontSize: 12, color: "#475569", textAlign: "center", marginTop: 16 }}>
              Clique sur une étoile pour continuer
            </p>
          </div>
        )}

        {/* Step 2 — Formulaire */}
        {step === 2 && (
          <div style={s.body}>
            {/* Étoiles recap */}
            <div style={{ display: "flex", justifyContent: "center", gap: 4, marginBottom: 24 }}>
              {[1,2,3,4,5].map(n => (
                <button
                  key={n}
                  onClick={() => setRating(n)}
                  style={{
                    ...s.starSmall,
                    color: n <= rating ? "#FBBF24" : "#334155",
                  }}
                >★</button>
              ))}
            </div>

            {/* Catégorie */}
            <p style={s.label}>Quel type de feedback ?</p>
            <div style={s.categories}>
              {CATEGORIES.map(c => (
                <button
                  key={c.key}
                  onClick={() => setCategory(c.key)}
                  style={{
                    ...s.catBtn,
                    background: category === c.key ? "rgba(59,130,246,0.15)" : "#0f172a",
                    border: `1px solid ${category === c.key ? "#3b82f6" : "#334155"}`,
                    color: category === c.key ? "#60a5fa" : "#94a3b8",
                  }}
                >
                  <span style={{ fontSize: 15 }}>{c.label.split(" ")[0]}</span>
                  <span style={{ fontSize: 12 }}>{c.label.split(" ").slice(1).join(" ")}</span>
                </button>
              ))}
            </div>

            {/* Message */}
            <p style={s.label}>Ton message <span style={{ color: "#ef4444" }}>*</span></p>
            <textarea
              value={message}
              onChange={e => setMessage(e.target.value)}
              placeholder="Décris ton expérience, ce qui fonctionne, ce qui manque..."
              maxLength={1000}
              style={s.textarea}
            />
            <div style={{ display: "flex", justifyContent: "flex-end" }}>
              <span style={{ fontSize: 11, color: "#475569" }}>{message.length}/1000</span>
            </div>

            {/* Email optionnel */}
            <p style={s.label}>Email (optionnel — pour qu'on te réponde)</p>
            <input
              type="email"
              value={email}
              onChange={e => setEmail(e.target.value)}
              placeholder="ton@email.com"
              style={s.input}
            />

            {error && <div style={s.error}>{error}</div>}

            <div style={{ display: "flex", gap: 10, marginTop: 20 }}>
              <button onClick={() => setStep(1)} style={s.backBtn}>← Retour</button>
              <button
                onClick={submit}
                disabled={loading || !message.trim()}
                style={{
                  ...s.submitBtn,
                  background: loading || !message.trim() ? "#334155" : "#3b82f6",
                  cursor: loading || !message.trim() ? "not-allowed" : "pointer",
                }}
              >
                {loading ? "⏳ Envoi..." : "🚀 Envoyer le feedback"}
              </button>
            </div>
          </div>
        )}

        {/* Step 3 — Confirmation */}
        {step === 3 && (
          <div style={{ ...s.body, textAlign: "center", padding: "40px 24px" }}>
            <div style={{ fontSize: 64, marginBottom: 20 }}>🙏</div>
            <h3 style={{ fontFamily: "inherit", fontSize: 20, fontWeight: 700, marginBottom: 12 }}>
              Merci pour ton feedback !
            </h3>
            <p style={{ color: "#94a3b8", fontSize: 14, marginBottom: 32, lineHeight: 1.6 }}>
              Ton avis nous aide directement à améliorer TransferBridge.<br/>
              On lit chaque message personnellement.
            </p>
            <div style={s.stars}>
              {[1,2,3,4,5].map(n => (
                <span key={n} style={{ fontSize: 28, color: n <= rating ? "#FBBF24" : "#334155" }}>★</span>
              ))}
            </div>
            <button onClick={onClose} style={{ ...s.submitBtn, marginTop: 32, cursor: "pointer" }}>
              Fermer
            </button>
          </div>
        )}
      </div>
    </div>
  );
}

const s = {
  overlay: {
    position: "fixed", inset: 0,
    background: "rgba(0,0,0,0.7)",
    backdropFilter: "blur(4px)",
    display: "flex", alignItems: "center", justifyContent: "center",
    zIndex: 1000, padding: 20,
  },
  modal: {
    background: "#1e293b",
    border: "1px solid #334155",
    borderRadius: 20,
    width: "100%", maxWidth: 480,
    boxShadow: "0 40px 80px rgba(0,0,0,0.5)",
    overflow: "hidden",
  },
  header: {
    display: "flex", justifyContent: "space-between", alignItems: "flex-start",
    padding: "20px 24px 0",
  },
  title: { fontSize: 18, fontWeight: 700, margin: 0 },
  subtitle: { fontSize: 12, color: "#64748b", marginTop: 2 },
  closeBtn: {
    background: "transparent", border: "none", color: "#64748b",
    fontSize: 16, cursor: "pointer", padding: 4,
  },
  body: { padding: "20px 24px 24px" },
  question: {
    fontSize: 16, fontWeight: 500, textAlign: "center",
    marginBottom: 28, color: "#f1f5f9",
  },
  stars: { display: "flex", justifyContent: "center", gap: 8, marginBottom: 8 },
  star: {
    background: "none", border: "none", cursor: "pointer",
    fontSize: 44, transition: "all 0.15s", lineHeight: 1,
  },
  starSmall: {
    background: "none", border: "none", cursor: "pointer",
    fontSize: 24, transition: "color 0.15s", lineHeight: 1,
  },
  ratingLabels: {
    display: "flex", justifyContent: "space-between",
    fontSize: 12, color: "#475569",
    padding: "0 8px",
  },
  label: {
    fontSize: 13, fontWeight: 500, color: "#94a3b8",
    marginBottom: 8, marginTop: 16,
  },
  categories: {
    display: "grid", gridTemplateColumns: "1fr 1fr",
    gap: 8, marginBottom: 4,
  },
  catBtn: {
    padding: "10px 12px", borderRadius: 10,
    cursor: "pointer", textAlign: "left",
    display: "flex", flexDirection: "column", gap: 2,
    transition: "all 0.15s",
  },
  textarea: {
    width: "100%", minHeight: 100,
    background: "#0f172a", border: "1px solid #334155",
    borderRadius: 10, color: "#f1f5f9",
    fontSize: 13, padding: "10px 12px",
    resize: "vertical", outline: "none",
    fontFamily: "inherit", lineHeight: 1.6,
  },
  input: {
    width: "100%", padding: "10px 12px",
    background: "#0f172a", border: "1px solid #334155",
    borderRadius: 10, color: "#f1f5f9",
    fontSize: 13, outline: "none",
    fontFamily: "inherit",
  },
  error: {
    marginTop: 10, padding: "8px 12px",
    background: "#450a0a", border: "1px solid #7f1d1d",
    borderRadius: 8, color: "#fca5a5", fontSize: 13,
  },
  backBtn: {
    padding: "10px 16px", background: "transparent",
    color: "#64748b", border: "1px solid #334155",
    borderRadius: 10, cursor: "pointer", fontSize: 14,
  },
  submitBtn: {
    flex: 1, padding: "12px",
    color: "white", border: "none",
    borderRadius: 10, fontSize: 14, fontWeight: 600,
  },
};