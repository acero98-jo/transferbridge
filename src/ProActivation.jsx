import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open as openUrl } from "@tauri-apps/plugin-shell";
import { fmt } from "./i18n/index.js";

const PLAN_STYLES = {
  monthly: {
    icon: "⚡", price: "19.99€",
    color: "#3b82f6", colorBg: "rgba(59,130,246,0.12)", colorBorder: "rgba(59,130,246,0.3)",
    checkout: "https://transferbridge.site/checkout.html?plan=monthly",
  },
  annual: {
    icon: "🚀", price: "99.99€",
    color: "#8b5cf6", colorBg: "rgba(139,92,246,0.12)", colorBorder: "rgba(139,92,246,0.3)",
    checkout: "https://transferbridge.site/checkout.html?plan=annual",
  },
  team: {
    icon: "🏢",
    color: "#22c55e", colorBg: "rgba(34,197,94,0.12)", colorBorder: "rgba(34,197,94,0.3)",
    checkout: "https://transferbridge.site/checkout.html?plan=team",
  },
};

function buildPlans(t) {
  return [
    { id: "monthly", ...PLAN_STYLES.monthly, name: t.planMonthly, period: t.perMonth, badge: null, features: t.planMonthlyFeatures },
    { id: "annual", ...PLAN_STYLES.annual, name: t.planAnnual, period: t.perYear, badge: t.badgeAnnual, features: t.planAnnualFeatures },
    { id: "team", ...PLAN_STYLES.team, name: t.planTeam, price: t.onQuote, period: "", badge: t.badgeTeam, features: t.planTeamFeatures },
  ];
}

export default function ProActivation({ t, onActivated, onClose }) {
  const PLANS = buildPlans(t);
  const [step, setStep] = useState("plans"); // plans | activate
  const [selectedPlan, setSelectedPlan] = useState(null);
  const [key, setKey] = useState("");
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState(null);
  const [success, setSuccess] = useState(false);

  async function goToCheckout(plan) {
    try {
      await openUrl(plan.checkout);
    } catch {
      window.open(plan.checkout, "_blank");
    }
    // Après l'achat, l'utilisateur reviendra activer sa clé
    setSelectedPlan(plan);
    setStep("activate");
  }

  async function activate() {
    const cleaned = key.trim().toUpperCase();
    if (!cleaned.startsWith("TB-") || cleaned.split("-").length !== 5) {
      setError(t.proErrFormat);
      return;
    }
    if (!selectedPlan) {
      setError(t.proErrNoPlan);
      return;
    }
    setLoading(true);
    setError(null);
    try {
      await invoke("activate_license", {
        key: cleaned,
        plan: selectedPlan.id,
      });
      setSuccess(true);
      setTimeout(() => {
        onActivated();
        onClose();
      }, 2500);
    } catch (e) {
      // Le backend renvoie un code stable (ex. "device_limit", "not_found")
      const code = String(e?.message ?? e).replace(/^Error:\s*/, "").trim();
      setError(t.licErr[code] || t.licErr.generic);
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
            <h2 style={s.title}>
              {step === "plans" ? t.proTitle : fmt(t.proActivateTitle, { plan: selectedPlan?.name })}
            </h2>
            <p style={s.sub}>
              {step === "plans"
                ? t.proChoose
                : t.proEnterKey
              }
            </p>
          </div>
          <button onClick={onClose} style={s.closeBtn}>✕</button>
        </div>

        {/* ── Écran succès ── */}
        {success && (
          <div style={s.successScreen}>
            <div style={{ fontSize: 64, marginBottom: 16 }}>🎉</div>
            <h3 style={s.successTitle}>{fmt(t.proActivated, { plan: selectedPlan?.name })}</h3>
            <p style={s.successSub}>
              {t.proUnlocked}
            </p>
            <div style={{ fontSize: 40, marginTop: 16 }}>✨</div>
          </div>
        )}

        {/* ── Étape 1 : Choix du plan ── */}
        {!success && step === "plans" && (
          <div style={s.body}>

            {/* Comparaison rapide gratuit vs pro */}
            <div style={s.freeCompare}>
              <div style={s.freeCompareLabel}>{t.proCurrentFree}</div>
              <div style={s.freeCompareFeatures}>
                <span style={s.featureBad}>{t.proFreeLimit1}</span>
                <span style={s.featureBad}>{t.proFreeLimit2}</span>
                <span style={s.featureBad}>{t.proFreeLimit3}</span>
                <span style={s.featureBad}>{t.proFreeLimit4}</span>
              </div>
            </div>

            {/* Plans */}
            <div style={s.plansGrid}>
              {PLANS.map(plan => (
                <div
                  key={plan.id}
                  style={{
                    ...s.planCard,
                    borderColor: selectedPlan?.id === plan.id ? plan.color : "rgba(255,255,255,0.07)",
                    background: selectedPlan?.id === plan.id ? plan.colorBg : "#0f172a",
                  }}
                  onClick={() => setSelectedPlan(plan)}
                >
                  {plan.badge && (
                    <div style={{ ...s.planBadge, background: plan.colorBg, color: plan.color, borderColor: plan.colorBorder }}>
                      {plan.badge}
                    </div>
                  )}
                  <div style={s.planIcon}>{plan.icon}</div>
                  <div style={s.planName}>{plan.name}</div>
                  <div style={{ display: "flex", alignItems: "baseline", gap: 2, justifyContent: "center", margin: "8px 0" }}>
                    <span style={{ ...s.planPrice, color: plan.color }}>{plan.price}</span>
                    <span style={s.planPeriod}>{plan.period}</span>
                  </div>

                  <div style={s.planFeatures}>
                    {plan.features.map((f, i) => (
                      <div key={i} style={s.planFeatureItem}>
                        <span style={{ color: plan.color, fontSize: 10 }}>✓</span>
                        <span>{f}</span>
                      </div>
                    ))}
                  </div>

                  {plan.id === "team" ? (
                    <button
                      onClick={e => { e.stopPropagation(); goToCheckout(plan); }}
                      style={{ ...s.planBtn, background: plan.color }}
                    >
                      {t.proContact}
                    </button>
                  ) : (
                    <button
                      onClick={e => { e.stopPropagation(); goToCheckout(plan); }}
                      style={{ ...s.planBtn, background: plan.color }}
                    >
                      {fmt(t.proBuy, { price: plan.price })}
                    </button>
                  )}
                </div>
              ))}
            </div>

            {/* Déjà une clé ? */}
            <div style={s.alreadyHaveKey}>
              <span style={{ color: "#64748b", fontSize: 12 }}>{t.proHaveKey}</span>
              <button
                onClick={() => { if (!selectedPlan) setSelectedPlan(PLANS[0]); setStep("activate"); }}
                style={s.alreadyKeyBtn}
              >
                {t.proActivateKey}
              </button>
            </div>
          </div>
        )}

        {/* ── Étape 2 : Activation de la clé ── */}
        {!success && step === "activate" && (
          <div style={s.body}>

            {/* Plan sélectionné */}
            {selectedPlan && (
              <div style={{
                ...s.selectedPlanBar,
                background: selectedPlan.colorBg,
                borderColor: selectedPlan.colorBorder,
              }}>
                <span style={{ fontSize: 18 }}>{selectedPlan.icon}</span>
                <div>
                  <div style={{ fontSize: 13, fontWeight: 700, color: "#f1f5f9" }}>{selectedPlan.name}</div>
                  <div style={{ fontSize: 11, color: "#64748b" }}>
                    {selectedPlan.price}{selectedPlan.period} — {t.proBoundDevice}
                  </div>
                </div>
                <button onClick={() => setStep("plans")} style={s.changePlanBtn}>{t.proChange}</button>
              </div>
            )}

            {/* Champ clé */}
            <div style={{ marginBottom: 6, marginTop: 16 }}>
              <label style={s.keyLabel}>{t.proKeyLabel}</label>
            </div>
            <input
              type="text"
              value={key}
              onChange={e => { setKey(e.target.value.toUpperCase()); setError(null); }}
              placeholder="TB-XXXX-XXXX-XXXX-XXXX"
              style={s.keyInput}
              spellCheck={false}
              autoFocus
            />

            {error && <div style={s.errorBox}>{error}</div>}

            <button
              onClick={activate}
              disabled={loading || !key.trim()}
              style={{
                ...s.activateBtn,
                background: loading || !key.trim() ? "#334155" : (selectedPlan?.color || "#3b82f6"),
                cursor: loading || !key.trim() ? "not-allowed" : "pointer",
              }}
            >
              {loading ? t.proVerifying : t.proActivateBtn}
            </button>

            <div style={s.activateInfo}>
              {t.proBoundInfo1}<br/>
              {t.proBoundInfo2}
            </div>

            {/* Pas encore de clé */}
            <div style={s.alreadyHaveKey}>
              <span style={{ color: "#64748b", fontSize: 12 }}>{t.proNoKey}</span>
              <button onClick={() => setStep("plans")} style={s.alreadyKeyBtn}>
                {t.proSeePlans}
              </button>
            </div>
          </div>
        )}

      </div>
    </div>
  );
}

const s = {
  overlay: {
    position: "fixed", inset: 0,
    background: "rgba(0,0,0,0.8)",
    backdropFilter: "blur(6px)",
    display: "flex", alignItems: "center", justifyContent: "center",
    zIndex: 1000, padding: "clamp(12px, 3vw, 20px)",
    overflowY: "auto",
  },
  modal: {
    background: "#1e293b",
    border: "1px solid #334155",
    borderRadius: 20,
    width: "100%",
    maxWidth: 820,
    maxHeight: "90vh",
    overflowY: "auto",
    boxShadow: "0 40px 80px rgba(0,0,0,0.6)",
  },
  header: {
    display: "flex", justifyContent: "space-between",
    alignItems: "flex-start", padding: "20px 24px 0",
    position: "sticky", top: 0, background: "#1e293b", zIndex: 1,
  },
  title: { fontSize: "clamp(16px, 2vw, 20px)", fontWeight: 800, margin: 0, fontFamily: "inherit" },
  sub: { fontSize: 12, color: "#64748b", marginTop: 4 },
  closeBtn: {
    background: "transparent", border: "none", color: "#64748b",
    fontSize: 18, cursor: "pointer", padding: 4, flexShrink: 0,
  },
  body: { padding: "16px 20px 24px" },

  // Succès
  successScreen: { textAlign: "center", padding: "40px 24px" },
  successTitle: { fontSize: 22, fontWeight: 800, margin: "0 0 8px", fontFamily: "inherit" },
  successSub: { color: "#94a3b8", fontSize: 14, lineHeight: 1.6 },

  // Comparaison gratuit
  freeCompare: {
    background: "#0f172a", borderRadius: 10, padding: "10px 14px",
    marginBottom: 16,
  },
  freeCompareLabel: { fontSize: 11, color: "#475569", marginBottom: 8, textTransform: "uppercase", letterSpacing: "0.05em" },
  freeCompareFeatures: { display: "flex", flexWrap: "wrap", gap: 8 },
  featureBad: { fontSize: 12, color: "#64748b" },

  // Grille des plans
  plansGrid: {
    display: "grid",
    gridTemplateColumns: "repeat(auto-fit, minmax(min(100%, 220px), 1fr))",
    gap: 12,
    marginBottom: 16,
  },
  planCard: {
    border: "1px solid",
    borderRadius: 14, padding: "16px 14px",
    cursor: "pointer", transition: "all 0.2s",
    display: "flex", flexDirection: "column", gap: 0,
    position: "relative",
  },
  planBadge: {
    position: "absolute", top: -10, left: "50%",
    transform: "translateX(-50%)",
    padding: "3px 10px", borderRadius: 20,
    fontSize: 10, fontWeight: 700, border: "1px solid",
    whiteSpace: "nowrap",
  },
  planIcon: { fontSize: 28, textAlign: "center", marginBottom: 4, marginTop: 6 },
  planName: { fontSize: 14, fontWeight: 700, textAlign: "center", color: "#f1f5f9" },
  planPrice: { fontSize: 22, fontWeight: 800 },
  planPeriod: { fontSize: 12, color: "#64748b" },
  planFeatures: { display: "flex", flexDirection: "column", gap: 5, margin: "10px 0 14px" },
  planFeatureItem: {
    display: "flex", alignItems: "flex-start", gap: 6,
    fontSize: 11, color: "#94a3b8", lineHeight: 1.4,
  },
  planBtn: {
    width: "100%", padding: "10px",
    color: "white", border: "none", borderRadius: 10,
    fontSize: 13, fontWeight: 700, cursor: "pointer",
    marginTop: "auto",
  },

  // Déjà une clé
  alreadyHaveKey: {
    display: "flex", alignItems: "center", justifyContent: "center",
    gap: 10, marginTop: 12, flexWrap: "wrap",
  },
  alreadyKeyBtn: {
    background: "transparent", border: "none",
    color: "#3b82f6", fontSize: 12, fontWeight: 600, cursor: "pointer",
  },

  // Activation
  selectedPlanBar: {
    display: "flex", alignItems: "center", gap: 12,
    padding: "12px 14px", borderRadius: 12, border: "1px solid",
    flexWrap: "wrap",
  },
  changePlanBtn: {
    marginLeft: "auto", padding: "4px 10px",
    background: "transparent", color: "#64748b",
    border: "1px solid #334155", borderRadius: 6,
    fontSize: 11, cursor: "pointer",
  },
  keyLabel: { fontSize: 12, fontWeight: 500, color: "#94a3b8" },
  keyInput: {
    width: "100%", padding: "12px 14px",
    background: "#0f172a", border: "1px solid #334155",
    borderRadius: 10, color: "#f1f5f9",
    fontSize: "clamp(13px, 1.8vw, 15px)",
    fontFamily: "monospace", letterSpacing: "0.08em", outline: "none",
  },
  errorBox: {
    marginTop: 8, padding: "8px 12px",
    background: "#450a0a", border: "1px solid #7f1d1d",
    borderRadius: 8, color: "#fca5a5", fontSize: 12, lineHeight: 1.5,
  },
  activateBtn: {
    width: "100%", marginTop: 14, padding: 14,
    color: "white", border: "none", borderRadius: 10,
    fontSize: 15, fontWeight: 700,
    transition: "opacity 0.2s",
  },
  activateInfo: {
    marginTop: 10, padding: "8px 12px",
    background: "rgba(59,130,246,0.06)",
    border: "1px solid rgba(59,130,246,0.12)",
    borderRadius: 8, fontSize: 11, color: "#64748b",
    lineHeight: 1.6, textAlign: "center",
  },
};