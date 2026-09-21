// Route POST /feedback du Worker Cloudflare `transferbridge-license`.
// Le webhook Discord est un secret du Worker : `wrangler secret put DISCORD_WEBHOOK`.
// Le limiteur de débit réutilise le KV `LICENSES` (clés préfixées `rl:`).
// Dans le `fetch` du Worker : if (url.pathname === "/feedback") return handleFeedback(request, env);

const STARS = ["⭐", "⭐⭐", "⭐⭐⭐", "⭐⭐⭐⭐", "⭐⭐⭐⭐⭐"];
const COLORS = { 5: 0x22c55e, 4: 0x3b82f6, 3: 0xf59e0b, 2: 0xf97316, 1: 0xef4444 };
const CATEGORIES = {
  bug: "🐛 Bug", feature: "💡 Idée", performance: "⚡ Performance", ux: "🎨 UX/Design",
};
const MAX_BODY = 8192;
const MAX_PER_HOUR = 5;

const clip = (v, n) => String(v ?? "").slice(0, n);

export async function handleFeedback(request, env) {
  if (request.method !== "POST") return new Response("Method Not Allowed", { status: 405 });
  if (!env.DISCORD_WEBHOOK) return new Response("Not configured", { status: 503 });

  const text = await request.text();
  if (text.length > MAX_BODY) return new Response("Payload Too Large", { status: 413 });

  let p;
  try { p = JSON.parse(text); } catch { return new Response("Bad Request", { status: 400 }); }

  const rating = Number(p?.rating);
  const message = clip(p?.message, 1500).trim();
  if (!Number.isInteger(rating) || rating < 1 || rating > 5 || !message) {
    return new Response("Bad Request", { status: 400 });
  }

  // Limite de débit par IP : 5 feedbacks / heure (le webhook n'est pas public,
  // mais cette route l'est).
  const ip = request.headers.get("CF-Connecting-IP") || "unknown";
  const rlKey = `rl:fb:${ip}:${Math.floor(Date.now() / 3600000)}`;
  const used = parseInt((await env.LICENSES.get(rlKey)) || "0", 10);
  if (used >= MAX_PER_HOUR) return new Response("Too Many Requests", { status: 429 });
  await env.LICENSES.put(rlKey, String(used + 1), { expirationTtl: 3700 });

  const email = clip(p.email, 200).trim().replace(/`/g, "");
  const embed = {
    title: `${STARS[rating - 1]} Nouveau feedback TransferBridge`,
    description: message,
    color: COLORS[rating],
    fields: [
      { name: "⭐ Note", value: `${rating}/5`, inline: true },
      { name: "🏷️ Catégorie", value: CATEGORIES[p.category] || "💬 Général", inline: true },
      { name: "💻 OS", value: clip(p.os, 60) || "?", inline: true },
      { name: "📦 Version", value: clip(p.app_version, 30) || "?", inline: true },
      { name: "📧 Email", value: email ? `\`${email}\`` : "*Anonyme*", inline: true },
    ],
    footer: { text: "TransferBridge Feedback System" },
    timestamp: new Date().toISOString(),
  };

  const res = await fetch(env.DISCORD_WEBHOOK, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    // allowed_mentions vide : un message ne peut pas déclencher @everyone
    body: JSON.stringify({ embeds: [embed], allowed_mentions: { parse: [] } }),
  });
  return new Response(null, { status: res.ok ? 204 : 502 });
}
