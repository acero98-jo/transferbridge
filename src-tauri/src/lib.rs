use axum::{
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Path, Multipart, State, Query},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, header},
    response::{Html, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine as _};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::io::ReaderStream;
use tokio::process::Child;
use tokio::sync::broadcast;

const WORKER_URL: &str = "https://transferbridge-license.abouacero1998.workers.dev";

// ─── Plans ────────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PlanType {
    Free,
    Monthly,
    Annual,
    Team,
}

impl PlanType {
    pub fn max_file_size_bytes(&self) -> u64 {
        match self {
            PlanType::Free => 500 * 1024 * 1024,  // 500 MB
            _ => 0, // illimité
        }
    }

    pub fn max_uploads_per_day(&self) -> Option<u32> {
        match self {
            PlanType::Free => Some(10),
            _ => None, // illimité
        }
    }

    pub fn allows_bidirectional(&self) -> bool {
        match self {
            PlanType::Free => false,
            _ => true,
        }
    }

    pub fn max_devices(&self) -> u32 {
        match self {
            PlanType::Free => 1,
            PlanType::Monthly => 1,
            PlanType::Annual => 3,
            PlanType::Team => 999,
        }
    }

    /// Identifiant stable (indépendant de la langue) : "free", "monthly", "annual", "team"
    pub fn id(&self) -> &'static str {
        match self {
            PlanType::Free => "free",
            PlanType::Monthly => "monthly",
            PlanType::Annual => "annual",
            PlanType::Team => "team",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            PlanType::Free => "Gratuit",
            PlanType::Monthly => "Pro Mensuel",
            PlanType::Annual => "Pro Annuel",
            PlanType::Team => "Team",
        }
    }
}

// ─── Licence ──────────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LicenseData {
    pub key:        String,
    pub plan:       PlanType,
    pub device_id:  String,
    pub expires_at: Option<u64>, // timestamp unix, None = pas d'expiration
    pub activated_at: u64,
    /// Jeton signé par le Worker : seule source de vérité pour plan/appareil/expiration
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub last_checked: u64,
}

// ─── Compteur journalier ──────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DailyCounter {
    pub count:    u32,
    pub day:      String, // "2025-06-13" format YYYY-MM-DD
}

impl DailyCounter {
    pub fn new() -> Self {
        DailyCounter { count: 0, day: current_day() }
    }

    pub fn reset_if_new_day(&mut self) {
        let today = current_day();
        if self.day != today {
            self.count = 0;
            self.day = today;
        }
    }

    pub fn increment(&mut self) {
        self.reset_if_new_day();
        self.count += 1;
    }

    pub fn can_upload(&mut self, limit: Option<u32>) -> bool {
        self.reset_if_new_day();
        match limit {
            None => true,
            Some(max) => self.count < max,
        }
    }

    pub fn remaining(&mut self, limit: Option<u32>) -> Option<u32> {
        self.reset_if_new_day();
        limit.map(|max| max.saturating_sub(self.count))
    }
}

fn current_day() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn get_counter_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("counter.json"))
}

fn load_counter(app: &AppHandle) -> DailyCounter {
    get_counter_path(app).ok()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|c| serde_json::from_str::<DailyCounter>(&c).ok())
        .map(|mut c| { c.reset_if_new_day(); c })
        .unwrap_or_else(DailyCounter::new)
}

fn persist_counter(app: &AppHandle, counter: &DailyCounter) {
    if let (Ok(path), Ok(json)) = (get_counter_path(app), serde_json::to_string(counter)) {
        let _ = std::fs::write(path, json);
    }
}

// ─── État partagé ─────────────────────────────────────────────────

struct GlobalState {
    pin:            Arc<Mutex<String>>,
    pin_attempts:   Arc<Mutex<PinGuard>>,
    events:         broadcast::Sender<String>,
    sessions:       Arc<Mutex<Vec<Session>>>,
    save_dir:       Arc<Mutex<PathBuf>>,
    started:        Mutex<bool>,
    max_file_size:  Arc<Mutex<u64>>,
    pending_files:  Arc<Mutex<Vec<PendingFile>>>,
    plan:           Arc<Mutex<PlanType>>,
    daily_counter:  Arc<Mutex<DailyCounter>>,
    device_id:      Arc<Mutex<String>>,
    // ── Cloudflare Tunnel ──
    tunnel_url:     Arc<Mutex<Option<String>>>,
    tunnel_process: Arc<Mutex<Option<Child>>>,
    tunnel_active:  Arc<Mutex<bool>>,
}

// ─── Anti brute-force PIN ──────────────────────────────────────────
// Le PIN fait 4 chiffres (10 000 combinaisons) : sans verrouillage,
// un attaquant sur le même réseau local pourrait le deviner en quelques
// secondes. On verrouille les tentatives après plusieurs échecs, avec
// un backoff exponentiel plafonné.
#[derive(Default)]
struct PinAttempts {
    fail_count:   u32,
    locked_until: Option<Instant>,
}

impl PinAttempts {
    const MAX_ATTEMPTS: u32 = 5;
    const BASE_LOCKOUT_SECS: u64 = 30;
    const MAX_LOCKOUT_SECS: u64 = 300;

    fn seconds_locked(&self) -> Option<u64> {
        self.locked_until.and_then(|until| {
            let now = Instant::now();
            if until > now { Some((until - now).as_secs() + 1) } else { None }
        })
    }

    fn register_failure(&mut self) {
        self.fail_count += 1;
        if self.fail_count >= Self::MAX_ATTEMPTS {
            let extra = (self.fail_count - Self::MAX_ATTEMPTS).min(4);
            let secs = (Self::BASE_LOCKOUT_SECS * (1u64 << extra)).min(Self::MAX_LOCKOUT_SECS);
            self.locked_until = Some(Instant::now() + Duration::from_secs(secs));
        }
    }

}

// Le verrouillage est tenu par IP : un attaquant ne peut plus bloquer le
// vrai utilisateur en saturant un compteur global. En complément, trop
// d'échecs cumulés (toutes IP confondues) font tourner le PIN, ce qui
// plafonne le nombre d'essais utiles à MAX_TOTAL_FAILURES par PIN.
#[derive(Default)]
struct PinGuard {
    per_ip:         HashMap<IpAddr, PinAttempts>,
    total_failures: u32,
}

impl PinGuard {
    const MAX_TOTAL_FAILURES: u32 = 20;
    const MAX_TRACKED_IPS: usize = 1024;

    fn seconds_locked(&self, ip: IpAddr) -> Option<u64> {
        self.per_ip.get(&ip).and_then(|a| a.seconds_locked())
    }

    /// Enregistre un échec. Retourne true si le PIN doit être régénéré.
    fn register_failure(&mut self, ip: IpAddr) -> bool {
        if self.per_ip.len() >= Self::MAX_TRACKED_IPS && !self.per_ip.contains_key(&ip) {
            self.per_ip.clear();
        }
        self.per_ip.entry(ip).or_default().register_failure();
        self.total_failures += 1;
        if self.total_failures >= Self::MAX_TOTAL_FAILURES {
            *self = PinGuard::default();
            return true;
        }
        false
    }

    fn register_success(&mut self, ip: IpAddr) {
        self.per_ip.remove(&ip);
    }
}

/// IP réelle du client : derrière le tunnel Cloudflare, toutes les requêtes
/// arrivent depuis 127.0.0.1 et l'IP d'origine est dans CF-Connecting-IP
/// (l'en-tête n'est pris en compte que pour une connexion locale).
fn client_ip(addr: SocketAddr, headers: &HeaderMap) -> IpAddr {
    if addr.ip().is_loopback() {
        if let Some(ip) = headers.get("cf-connecting-ip")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.trim().parse::<IpAddr>().ok())
        {
            return ip;
        }
    }
    addr.ip()
}

#[derive(Clone, serde::Serialize)]
struct PendingFile {
    id:       String,
    name:     String,
    size:     u64,
    path:     String,
    added_at: u64,
}

#[derive(Clone)]
struct AppState {
    pin:           Arc<Mutex<String>>,
    pin_attempts:  Arc<Mutex<PinGuard>>,
    events:        broadcast::Sender<String>,
    sessions:      Arc<Mutex<Vec<Session>>>,
    save_dir:      Arc<Mutex<PathBuf>>,
    app_handle:    AppHandle,
    max_file_size: Arc<Mutex<u64>>,
    pending_files: Arc<Mutex<Vec<PendingFile>>>,
    plan:          Arc<Mutex<PlanType>>,
    daily_counter: Arc<Mutex<DailyCounter>>,
}

#[derive(Clone)]
struct Session {
    token:      String,
    expires_at: Instant,
}

// ─── Device Fingerprint ───────────────────────────────────────────

fn machine_guid() -> Option<String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let out = std::process::Command::new("reg")
            .args(["query", r"HKLM\SOFTWARE\Microsoft\Cryptography", "/v", "MachineGuid"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .find(|l| l.contains("MachineGuid"))
            .and_then(|l| l.split_whitespace().last())
            .map(|s| s.to_string())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::fs::read_to_string("/etc/machine-id").ok().map(|s| s.trim().to_string())
    }
}

fn generate_device_id() -> String {
    let hostname = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());

    let machine = machine_guid().unwrap_or_else(|| "unknown".to_string());

    let digest = Sha256::digest(format!("TB-{}:{}:{}", hostname, username, machine).as_bytes());
    let short: String = digest.iter().take(8).map(|b| format!("{:02X}", b)).collect();
    format!("TBDEV-{}", short)
}

// ─── Commandes Tauri ──────────────────────────────────────────────

#[tauri::command]
async fn start_server(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<String, String> {
    let ip = get_local_ip();

    {
        let mut started = global.started.lock().unwrap_or_else(|e| e.into_inner());
        if *started {
            let pin = generate_pin();
            *global.pin.lock().unwrap_or_else(|e| e.into_inner()) = pin.clone();
            global.sessions.lock().unwrap_or_else(|e| e.into_inner()).clear();
            *global.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()) = PinGuard::default();
            let _ = app.emit("pin-generated", pin);
            return Ok(format!("http://{}:3030", ip));
        }
        *started = true;
    }

    // Génère ou charge le device ID
    let device_id = {
        let id_path = get_device_id_path(&app)?;
        if id_path.exists() {
            std::fs::read_to_string(&id_path).unwrap_or_else(|_| generate_device_id())
        } else {
            let id = generate_device_id();
            let _ = std::fs::write(&id_path, &id);
            id
        }
    };
    *global.device_id.lock().unwrap_or_else(|e| e.into_inner()) = device_id;

    let save_dir = app.path().download_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    *global.save_dir.lock().unwrap_or_else(|e| e.into_inner()) = save_dir;

    // Charge la licence si elle existe : le plan n'est accordé que si le jeton
    // signé est valide pour cet appareil (sinon check_license tranchera).
    if let Ok(license) = load_license_data(&app).await {
        let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Ok(plan) = evaluate_license(&license, &device_id, unix_now()) {
            *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = plan;
        }
    }

    *global.daily_counter.lock().unwrap_or_else(|e| e.into_inner()) = load_counter(&app);

    let pin = generate_pin();
    *global.pin.lock().unwrap_or_else(|e| e.into_inner()) = pin.clone();

    let state = AppState {
        pin:           Arc::clone(&global.pin),
        pin_attempts:  Arc::clone(&global.pin_attempts),
        events:        global.events.clone(),
        sessions:      Arc::clone(&global.sessions),
        save_dir:      Arc::clone(&global.save_dir),
        app_handle:    app.clone(),
        max_file_size: Arc::clone(&global.max_file_size),
        pending_files: Arc::clone(&global.pending_files),
        plan:          Arc::clone(&global.plan),
        daily_counter: Arc::clone(&global.daily_counter),
    };

    let port = 3030u16;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    // Pas de CORS : la page mobile est servie par ce même serveur (même
    // origine). Sans CORS, un site tiers ouvert dans le navigateur de la
    // victime ne peut pas lire les réponses de l'API ni forcer le PIN.
    let router = Router::new()
        .route("/",              get(serve_mobile_ui))
        .route("/ping",          get(|| async { "pong" }))
        .route("/verify-pin",    post(verify_pin))
        .route("/upload",        post(handle_upload))
        .route("/ws",            get(ws_handler))
        .route("/files-to-send", get(list_pending_files))
        .route("/send/:file_id", get(download_file))
        .route("/plan-info",     get(get_plan_info_route))
        .with_state(state)
        // Plafond de sécurité contre les requêtes anormalement volumineuses
        // (la limite réelle par plan est appliquée pendant l'écriture en
        // flux dans handle_upload).
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024 * 1024));

    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                println!("🚀 Serveur démarré sur {}", addr);
                axum::serve(
                    listener,
                    router.into_make_service_with_connect_info::<SocketAddr>(),
                ).await.unwrap();
            }
            Err(e) => eprintln!("❌ Port déjà occupé : {}", e),
        }
    });

    let _ = app.emit("pin-generated", pin.clone());

    // ── Lance le tunnel Cloudflare si plan Pro ──
    let plan = global.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if plan.allows_bidirectional() {
        let app_clone   = app.clone();
        let tunnel_url  = Arc::clone(&global.tunnel_url);
        let tunnel_act  = Arc::clone(&global.tunnel_active);
        let tunnel_proc = Arc::clone(&global.tunnel_process);

        tokio::spawn(async move {
            // Notifie React que le tunnel démarre
            let _ = app_clone.emit("tunnel-starting", true);

            // Attend que le serveur HTTP soit bien démarré
            tokio::time::sleep(tokio::time::Duration::from_millis(800)).await;

            match ensure_cloudflared(&app_clone).await {
                Ok(path) => {
                    match launch_tunnel(path, app_clone.clone(), tunnel_url, tunnel_act).await {
                        Ok(child) => {
                            *tunnel_proc.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                            println!("☁️  Tunnel Cloudflare lancé");
                        }
                        Err(e) => {
                            eprintln!("❌ Erreur lancement tunnel : {}", e);
                            let _ = app_clone.emit("tunnel-error", e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("❌ Erreur cloudflared : {}", e);
                    let _ = app_clone.emit("tunnel-error", e);
                }
            }
        });
    }

    Ok(format!("http://{}:{}", ip, port))
}

#[tauri::command]
async fn regenerate_pin(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<String, String> {
    let new_pin = generate_pin();
    *global.pin.lock().unwrap_or_else(|e| e.into_inner()) = new_pin.clone();
    global.sessions.lock().unwrap_or_else(|e| e.into_inner()).clear();
    *global.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()) = PinGuard::default();
    let _ = app.emit("pin-generated", new_pin.clone());
    Ok(new_pin)
}

#[tauri::command]
fn get_save_dir(global: tauri::State<'_, GlobalState>) -> String {
    global.save_dir.lock().unwrap_or_else(|e| e.into_inner()).to_string_lossy().to_string()
}

#[tauri::command]
fn set_save_dir(
    global: tauri::State<'_, GlobalState>,
    path: String,
) -> Result<(), String> {
    *global.save_dir.lock().unwrap_or_else(|e| e.into_inner()) = PathBuf::from(&path);
    println!("📁 Dossier changé : {}", path);
    Ok(())
}

#[tauri::command]
async fn save_history(app: AppHandle, history: serde_json::Value) -> Result<(), String> {
    let path = get_history_path(&app)?;
    let json = serde_json::to_string_pretty(&history).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json.as_bytes()).await
        .map_err(|e: std::io::Error| e.to_string())?;
    Ok(())
}

#[tauri::command]
async fn load_history(app: AppHandle) -> Result<serde_json::Value, String> {
    let path = get_history_path(&app)?;
    if !path.exists() { return Ok(serde_json::json!([])); }
    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e: std::io::Error| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

fn get_history_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("history.json"))
}

// ─── Infos plan (depuis React) ────────────────────────────────────

#[derive(serde::Serialize)]
struct PlanInfo {
    plan:           PlanType,
    plan_label:     String,
    uploads_today:  u32,
    uploads_limit:  Option<u32>,
    uploads_left:   Option<u32>,
    max_file_mb:    Option<u64>,  // None = illimité
    bidirectional:  bool,
    max_devices:    u32,
    device_id:      String,
}

#[tauri::command]
fn get_plan_info(
    global: tauri::State<'_, GlobalState>,
) -> PlanInfo {
    let plan = global.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut counter = global.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
    counter.reset_if_new_day();
    let limit = plan.max_uploads_per_day();
    let left  = counter.remaining(limit);
    let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();

    PlanInfo {
        plan_label:    plan.label().to_string(),
        uploads_today: counter.count,
        uploads_limit: limit,
        uploads_left:  left,
        max_file_mb:   if plan.max_file_size_bytes() == 0 { None } else { Some(plan.max_file_size_bytes() / 1_048_576) },
        bidirectional: plan.allows_bidirectional(),
        max_devices:   plan.max_devices(),
        device_id,
        plan,
    }
}

// ─── Licence ──────────────────────────────────────────────────────

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn load_license_data(app: &AppHandle) -> Result<LicenseData, String> {
    let path = get_license_path(app)?;
    if !path.exists() { return Err("Pas de licence".to_string()); }
    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e: std::io::Error| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

async fn write_license(app: &AppHandle, license: &LicenseData) -> Result<(), String> {
    let json = serde_json::to_string_pretty(license).map_err(|e| e.to_string())?;
    let path = get_license_path(app)?;
    tokio::fs::write(&path, json.as_bytes()).await
        .map_err(|e: std::io::Error| e.to_string())
}

async fn remove_license(app: &AppHandle) {
    if let Ok(path) = get_license_path(app) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

// ── Licences signées ──────────────────────────────────────────────
// Le Worker signe (Ed25519) un jeton {v, key, plan, device_id, iat, exp}.
// L'app ne se fie qu'à ce jeton : le plan, l'appareil et l'expiration
// viennent de la partie signée, jamais des autres champs de license.json
// (que l'utilisateur peut modifier).

/// Clé publique Ed25519 (32 octets en base64) qui vérifie les jetons du Worker.
const LICENSE_PUBLIC_KEY_B64: &str = "WhWLZ6v6X0HA2b9qnwDcovLovBVhZ6lNIm9Kudx2BF8=";

const LICENSE_RECHECK_SECS: u64 = 24 * 3600;

#[derive(Debug, serde::Deserialize)]
struct LicenseToken {
    v:         u8,
    key:       String,
    plan:      PlanType,
    device_id: String,
    iat:       u64,
    exp:       Option<u64>,
}

fn verify_license_token_with(token: &str, vk: &VerifyingKey) -> Result<LicenseToken, String> {
    let (body_b64, sig_b64) = token.split_once('.').ok_or("jeton mal formé")?;
    let sig_bytes = URL_SAFE_NO_PAD.decode(sig_b64).map_err(|_| "signature illisible")?;
    let sig = Signature::from_slice(&sig_bytes).map_err(|_| "signature invalide")?;
    vk.verify(body_b64.as_bytes(), &sig).map_err(|_| "signature invalide")?;

    let body = URL_SAFE_NO_PAD.decode(body_b64).map_err(|_| "jeton illisible")?;
    let token: LicenseToken = serde_json::from_slice(&body).map_err(|_| "jeton invalide")?;
    if token.v != 1 || token.plan == PlanType::Free {
        return Err("jeton non supporté".to_string());
    }
    Ok(token)
}

fn license_verifying_key() -> Result<VerifyingKey, String> {
    let bytes = STANDARD.decode(LICENSE_PUBLIC_KEY_B64)
        .map_err(|_| "clé publique de licence non configurée".to_string())?;
    let arr: [u8; 32] = bytes.try_into()
        .map_err(|_| "clé publique de licence invalide".to_string())?;
    VerifyingKey::from_bytes(&arr).map_err(|_| "clé publique de licence invalide".to_string())
}

#[derive(Debug, PartialEq)]
enum LicenseIssue {
    NoToken,
    Invalid,
    WrongDevice,
    Expired,
}

fn evaluate_license_with(
    vk: &VerifyingKey,
    license: &LicenseData,
    device_id: &str,
    now: u64,
) -> Result<PlanType, LicenseIssue> {
    let token = license.token.as_deref().ok_or(LicenseIssue::NoToken)?;
    let payload = verify_license_token_with(token, vk).map_err(|_| LicenseIssue::Invalid)?;
    if payload.key != license.key {
        return Err(LicenseIssue::Invalid);
    }
    if payload.device_id != device_id {
        return Err(LicenseIssue::WrongDevice);
    }
    if let Some(exp) = payload.exp {
        if now > exp {
            return Err(LicenseIssue::Expired);
        }
    }
    Ok(payload.plan)
}

/// Plan effectif d'une licence locale, sans réseau (jeton, appareil, expiration).
fn evaluate_license(license: &LicenseData, device_id: &str, now: u64) -> Result<PlanType, LicenseIssue> {
    let vk = license_verifying_key().map_err(|_| LicenseIssue::Invalid)?;
    evaluate_license_with(&vk, license, device_id, now)
}

fn license_http_client(secs: u64) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(secs))
        .build()
        .map_err(|e| e.to_string())
}

/// Demande (ou renouvelle) l'activation auprès du Worker. Renvoie la licence
/// à enregistrer, ou un code d'erreur stable (voir `licErr` côté interface) :
/// "not_found", "revoked", "device_limit", "expired", "plan_mismatch",
/// "server_unreachable", "server_invalid", "generic"…
async fn request_activation(key: &str, plan_id: &str, device_id: &str) -> Result<LicenseData, String> {
    let client = license_http_client(15)?;

    let res = client
        .post(format!("{}/", WORKER_URL))
        .json(&serde_json::json!({ "key": key, "plan": plan_id, "device_id": device_id }))
        .send().await
        // Fail-closed : serveur injoignable → pas d'activation
        .map_err(|_| "server_unreachable".to_string())?;

    let status = res.status();
    let body: serde_json::Value = res.json().await.unwrap_or(serde_json::Value::Null);

    if !status.is_success() {
        return Err(body.get("code").and_then(|c| c.as_str()).unwrap_or("generic").to_string());
    }
    if body.get("success").and_then(|v| v.as_bool()) != Some(true) {
        return Err("generic".to_string());
    }

    let token = body.get("token").and_then(|t| t.as_str()).ok_or("server_invalid")?;
    let payload = verify_license_token_with(token, &license_verifying_key()?)
        .map_err(|_| "server_invalid".to_string())?;
    if payload.key != key || payload.device_id != device_id {
        return Err("server_invalid".to_string());
    }

    Ok(LicenseData {
        key:          key.to_string(),
        plan:         payload.plan,
        device_id:    device_id.to_string(),
        expires_at:   payload.exp,
        activated_at: payload.iat,
        token:        Some(token.to_string()),
        last_checked: unix_now(),
    })
}

enum RemoteCheck {
    Valid,
    Rejected(String),
    Unreachable,
}

/// Revalidation légère (révocation, expiration, appareil) sans rien modifier côté serveur.
async fn remote_check(key: &str, device_id: &str) -> RemoteCheck {
    let Ok(client) = license_http_client(8) else { return RemoteCheck::Unreachable };
    let Ok(res) = client
        .post(format!("{}/license/check", WORKER_URL))
        .json(&serde_json::json!({ "key": key, "device_id": device_id }))
        .send().await
    else { return RemoteCheck::Unreachable };

    let status = res.status();
    if status.is_success() {
        return RemoteCheck::Valid;
    }
    if status == reqwest::StatusCode::UNAUTHORIZED {
        let code = res.json::<serde_json::Value>().await.ok()
            .and_then(|b| b.get("code").and_then(|c| c.as_str()).map(|s| s.to_string()));
        if let Some(code) = code {
            if matches!(code.as_str(), "revoked" | "expired" | "not_found" | "bad_signature" | "device_not_registered") {
                return RemoteCheck::Rejected(code);
            }
        }
    }
    // 5xx, ancien Worker sans cette route, etc. : on ne pénalise pas l'utilisateur
    RemoteCheck::Unreachable
}

#[tauri::command]
async fn activate_license(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
    key:    String,
    plan:   String,
) -> Result<(), String> {
    let parts: Vec<&str> = key.split('-').collect();
    if parts.len() != 5 || parts[0] != "TB" {
        return Err("invalid_key_format".to_string());
    }
    let plan_id = match plan.as_str() {
        "monthly" | "annual" | "team" => plan.as_str(),
        _ => return Err("generic".to_string()),
    };

    let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let license = request_activation(&key, plan_id, &device_id).await?;

    write_license(&app, &license).await?;
    *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = license.plan.clone();

    let _ = app.emit("plan-changed", license.plan.label());
    println!("⚡ Plan activé : {}", license.plan.id());
    Ok(())
}

#[tauri::command]
async fn check_license(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<serde_json::Value, String> {
    let mut license = match load_license_data(&app).await {
        Err(_) => return Ok(serde_json::json!({ "plan": "free", "valid": true })),
        Ok(license) => license,
    };
    let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let set_plan = |p: PlanType| *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = p;

    // Licence créée avant les jetons signés : on en récupère un une seule fois.
    if license.token.is_none() {
        match request_activation(&license.key, license.plan.id(), &device_id).await {
            Ok(fresh) => {
                write_license(&app, &fresh).await?;
                license = fresh;
            }
            Err(code) => {
                set_plan(PlanType::Free);
                let offline = code == "server_unreachable";
                if !offline && code != "server_invalid" && code != "generic" {
                    remove_license(&app).await;
                }
                return Ok(serde_json::json!({
                    "plan": "free", "valid": false, "needs_online": offline, "code": code,
                }));
            }
        }
    }

    match evaluate_license(&license, &device_id, unix_now()) {
        Err(LicenseIssue::Expired) => {
            set_plan(PlanType::Free);
            remove_license(&app).await;
            Ok(serde_json::json!({ "plan": "free", "valid": false, "expired": true }))
        }
        Err(LicenseIssue::WrongDevice) => {
            set_plan(PlanType::Free);
            Ok(serde_json::json!({ "plan": "free", "valid": false, "wrong_device": true }))
        }
        Err(_) => {
            set_plan(PlanType::Free);
            Ok(serde_json::json!({ "plan": "free", "valid": false, "invalid": true }))
        }
        Ok(plan) => {
            // Revalidation quotidienne : détecte une licence révoquée ou expirée côté serveur
            let now = unix_now();
            if now.saturating_sub(license.last_checked) > LICENSE_RECHECK_SECS {
                match remote_check(&license.key, &device_id).await {
                    RemoteCheck::Valid => {
                        license.last_checked = now;
                        let _ = write_license(&app, &license).await;
                    }
                    RemoteCheck::Rejected(code) => {
                        set_plan(PlanType::Free);
                        remove_license(&app).await;
                        return Ok(serde_json::json!({
                            "plan": "free", "valid": false, "revoked": true, "code": code,
                        }));
                    }
                    RemoteCheck::Unreachable => {}
                }
            }

            set_plan(plan.clone());
            Ok(serde_json::json!({
                "plan":       plan,
                "plan_label": plan.label(),
                "valid":      true,
                "expires_at": license.expires_at,
                "key":        license.key,
            }))
        }
    }
}

#[tauri::command]
async fn deactivate_license(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<(), String> {
    // Libère l'appareil côté serveur pour pouvoir réutiliser la clé ailleurs
    // (best effort : sans internet, la désactivation locale a quand même lieu).
    if let Ok(license) = load_license_data(&app).await {
        let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Ok(client) = license_http_client(8) {
            let _ = client
                .post(format!("{}/license/deactivate", WORKER_URL))
                .json(&serde_json::json!({ "key": license.key, "device_id": device_id }))
                .send().await;
        }
    }

    remove_license(&app).await;
    *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = PlanType::Free;
    let _ = app.emit("plan-changed", "free");
    println!("🔓 Licence désactivée sur cet appareil");
    Ok(())
}

fn get_license_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("license.json"))
}

fn get_device_id_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("device.id"))
}

// ─── WebSocket ────────────────────────────────────────────────────

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    use futures_util::StreamExt;

    let mut events = state.events.subscribe();

    // Envoie les infos du plan au téléphone dès la connexion
    let plan_info = {
        let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
        serde_json::json!({
            "type":           "plan-info",
            "bidirectional":  plan.allows_bidirectional(),
            "plan":           plan.label(),
            "plan_id":        plan.id(),
        })
    };
    let _ = socket.send(Message::Text(plan_info.to_string().into())).await;

    loop {
        tokio::select! {
            incoming = socket.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                        if data["type"] == "progress" {
                            let filename = data["filename"].as_str().unwrap_or("").to_string();
                            let percent  = data["percent"].as_f64().unwrap_or(0.0).clamp(0.0, 100.0);
                            let _ = state.app_handle.emit("upload-progress",
                                serde_json::json!({ "filename": filename, "percent": percent }));
                        }
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
            Ok(event) = events.recv() => {
                if socket.send(Message::Text(event)).await.is_err() { break; }
            }
        }
    }
}

// ─── Routes HTTP ──────────────────────────────────────────────────

fn mobile_ui_html() -> String {
    MOBILE_UI.replace("__APP_VERSION__", env!("CARGO_PKG_VERSION"))
}

async fn serve_mobile_ui() -> Html<String> {
    Html(mobile_ui_html())
}

// Route HTTP pour infos plan (accessible depuis le téléphone)
async fn get_plan_info_route(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let token = params.get("token").cloned().unwrap_or_default();
    if !is_valid_session(&state.sessions, &token) {
        return Err((StatusCode::UNAUTHORIZED, "Session invalide".to_string()));
    }
    let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
    counter.reset_if_new_day();

    Ok(Json(serde_json::json!({
        "plan":          plan.label(),
        "plan_id":       plan.id(),
        "bidirectional": plan.allows_bidirectional(),
        "uploads_left":  counter.remaining(plan.max_uploads_per_day()),
        "uploads_limit": plan.max_uploads_per_day(),
    })))
}

async fn verify_pin(
    State(state): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers:      HeaderMap,
    Json(body):   Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let ip = client_ip(addr, &headers);

    // Anti brute-force : refuse toute tentative pendant le verrouillage,
    // sans même comparer le PIN (évite de "gaspiller" une fenêtre de timing).
    if let Some(retry_after) = state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()).seconds_locked(ip) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            serde_json::json!({
                "success": false,
                "error": "locked",
                "message": "Trop de tentatives incorrectes. Réessaie plus tard.",
                "retry_after": retry_after,
            }).to_string(),
        ));
    }

    let input   = body["pin"].as_str().unwrap_or("").to_string();
    let correct = state.pin.lock().unwrap_or_else(|e| e.into_inner()).clone();

    if input != correct {
        let (retry_after, rotate) = {
            let mut attempts = state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner());
            let rotate = attempts.register_failure(ip);
            (attempts.seconds_locked(ip), rotate)
        };
        if rotate {
            // Trop d'échecs cumulés : le PIN courant est considéré comme
            // attaqué, on le remplace (les sessions déjà ouvertes restent).
            let new_pin = generate_pin();
            *state.pin.lock().unwrap_or_else(|e| e.into_inner()) = new_pin.clone();
            let _ = state.app_handle.emit("pin-generated", new_pin);
        }
        let _ = state.app_handle.emit("pin-failed", &input);
        return Err((
            StatusCode::UNAUTHORIZED,
            serde_json::json!({
                "success": false,
                "error": "PIN incorrect",
                "locked": retry_after.is_some(),
                "retry_after": retry_after,
            }).to_string(),
        ));
    }

    state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()).register_success(ip);

    let token = uuid::Uuid::new_v4().to_string();
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).push(Session {
        token:      token.clone(),
        expires_at: Instant::now() + Duration::from_secs(600),
    });

    let _ = state.app_handle.emit("device-connected", serde_json::json!({
        "time": chrono::Local::now().format("%H:%M").to_string()
    }));

    // Envoie aussi les infos du plan avec le token
    let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
    counter.reset_if_new_day();

    Ok(Json(serde_json::json!({
        "success":       true,
        "token":         token,
        "plan":          plan.label(),
        "plan_id":       plan.id(),
        "bidirectional": plan.allows_bidirectional(),
        "uploads_left":  counter.remaining(plan.max_uploads_per_day()),
        "uploads_limit": plan.max_uploads_per_day(),
    })))
}

fn is_valid_session(sessions: &Arc<Mutex<Vec<Session>>>, token: &str) -> bool {
    let mut s = sessions.lock().unwrap_or_else(|e| e.into_inner());
    s.retain(|s| s.expires_at > Instant::now());
    s.iter().any(|s| s.token == token)
}

async fn handle_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<String, (StatusCode, String)> {
    use tokio::io::AsyncWriteExt;

    let bad = |e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string());
    let session_error = || {
        let _ = state.app_handle.emit("upload-error", serde_json::json!({
            "error": "session_expired", "message": "Session expirée — reconnecte-toi"
        }));
        (StatusCode::UNAUTHORIZED, serde_json::json!({ "error": "session_expired" }).to_string())
    };

    // Le champ "token" doit précéder les fichiers : la session est validée
    // avant d'écrire ou même de lire le moindre octet de fichier.
    let mut authorized = false;

    while let Some(mut field) = multipart.next_field().await.map_err(bad)? {
        let name = field.name().unwrap_or("").to_string();
        if name == "token" {
            let token = field.text().await.map_err(bad)?;
            authorized = is_valid_session(&state.sessions, &token);
            continue;
        }

        if !authorized {
            return Err(session_error());
        }

        let filename = sanitize_filename(field.file_name().unwrap_or("fichier"));

        // Limite journalière vérifiée fichier par fichier, avant l'écriture
        let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let limit_reached = {
            let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
            !counter.can_upload(plan.max_uploads_per_day())
        };
        if limit_reached {
            let _ = state.app_handle.emit("upload-error", serde_json::json!({
                "error":   "daily_limit",
                "message": "Limite journalière atteinte (10/jour). Passez à Pro pour un accès illimité."
            }));
            return Err((StatusCode::TOO_MANY_REQUESTS, serde_json::json!({
                "error": "daily_limit",
                "message": "Limite de 10 envois/jour atteinte. Passez à Pro !"
            }).to_string()));
        }

        // Plan gratuit : plafonné à 500 Mo (et au réglage utilisateur s'il est
        // plus bas). Plans payants : illimité.
        let plan_max = plan.max_file_size_bytes();
        let user_max = *state.max_file_size.lock().unwrap_or_else(|e| e.into_inner());
        let effective_max = match (plan_max, user_max) {
            (0, _) => 0,
            (p, 0) => p,
            (p, u) => p.min(u),
        };

        let dir = state.save_dir.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let (mut file, save_path) = create_unique_file(&dir, &filename).await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        // Écriture en flux : la mémoire utilisée reste constante quelle que
        // soit la taille du fichier.
        let mut written: u64 = 0;
        loop {
            let chunk = match field.chunk().await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(e) => {
                    discard_partial(file, &save_path).await;
                    return Err(bad(e));
                }
            };

            written += chunk.len() as u64;
            if effective_max > 0 && written > effective_max {
                discard_partial(file, &save_path).await;
                let msg = format!("❌ '{}' dépasse la limite ({:.0}MB)", filename, effective_max as f64 / 1_048_576.0);
                let _ = state.app_handle.emit("upload-error", serde_json::json!({
                    "filename": filename, "error": "too_large", "message": msg.clone(),
                    "limit_mb": effective_max / 1_048_576,
                }));
                return Err((StatusCode::PAYLOAD_TOO_LARGE, msg));
            }

            if let Err(e) = file.write_all(&chunk).await {
                discard_partial(file, &save_path).await;
                return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
            }
        }

        if let Err(e) = file.flush().await {
            discard_partial(file, &save_path).await;
            return Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()));
        }
        drop(file);

        {
            let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
            counter.increment();
            persist_counter(&state.app_handle, &counter);
        }

        println!("✅ Reçu : {} ({} octets)", filename, written);
        let _ = state.app_handle.emit("file-received", serde_json::json!({
            "name": filename, "size": written, "path": save_path.to_string_lossy()
        }));
    }

    if !authorized {
        return Err(session_error());
    }

    // Envoie le compteur mis à jour
    let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());
    let remaining = counter.remaining(plan.max_uploads_per_day());
    let _ = state.app_handle.emit("counter-updated", serde_json::json!({
        "uploads_today": counter.count,
        "uploads_left":  remaining,
        "uploads_limit": plan.max_uploads_per_day(),
    }));

    Ok("✅ Fichiers reçus".to_string())
}

async fn discard_partial(file: tokio::fs::File, path: &std::path::Path) {
    drop(file);
    let _ = tokio::fs::remove_file(path).await;
}

/// Ne garde que le nom de fichier : retire tout chemin (`..\..\x`, `/etc/x`),
/// les caractères interdits sous Windows et les noms réservés (CON, NUL…).
fn sanitize_filename(raw: &str) -> String {
    let base = raw.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        .take(200)
        .collect();
    let cleaned = cleaned.trim().trim_end_matches(|c| c == '.' || c == ' ').to_string();

    let stem = cleaned.split('.').next().unwrap_or("").trim().to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.len() == 4
            && stem.as_bytes()[3].is_ascii_digit());

    if cleaned.is_empty() || reserved {
        "fichier".to_string()
    } else {
        cleaned
    }
}

/// Crée le fichier sans jamais écraser un fichier existant : `create_new`
/// est atomique, donc deux envois simultanés du même nom ne se marchent pas dessus.
async fn create_unique_file(
    dir: &std::path::Path,
    filename: &str,
) -> std::io::Result<(tokio::fs::File, PathBuf)> {
    let p = std::path::Path::new(filename);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();

    let mut i = 0u32;
    loop {
        let name = if i == 0 { filename.to_string() } else { format!("{}_{}{}", stem, i, ext) };
        let candidate = dir.join(&name);
        match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&candidate).await {
            Ok(file) => return Ok((file, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => i += 1,
            Err(e) => return Err(e),
        }
    }
}

async fn list_pending_files(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let token = params.get("token").cloned().unwrap_or_default();
    if !is_valid_session(&state.sessions, &token) {
        return Err((StatusCode::UNAUTHORIZED, "Session invalide".to_string()));
    }

    // Bidirectionnel réservé aux plans payants
    let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !plan.allows_bidirectional() {
        return Ok(Json(serde_json::json!({ "files": [], "pro_required": true })));
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut files = state.pending_files.lock().unwrap_or_else(|e| e.into_inner());
    files.retain(|f| now - f.added_at < 600);

    let list: Vec<serde_json::Value> = files.iter().map(|f| {
        serde_json::json!({ "id": f.id, "name": f.name, "size": f.size })
    }).collect();

    Ok(Json(serde_json::json!({ "files": list, "pro_required": false })))
}

async fn download_file(
    State(state): State<AppState>,
    Path(file_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<axum::response::Response<Body>, (StatusCode, String)> {
    let token = params.get("token").cloned().unwrap_or_default();
    if !is_valid_session(&state.sessions, &token) {
        return Err((StatusCode::UNAUTHORIZED, "Session invalide".to_string()));
    }

    let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !plan.allows_bidirectional() {
        return Err((StatusCode::FORBIDDEN, "Fonctionnalité Pro requise".to_string()));
    }

    let file_info = {
        let files = state.pending_files.lock().unwrap_or_else(|e| e.into_inner());
        files.iter().find(|f| f.id == file_id).cloned()
    };

    let file_info = file_info.ok_or_else(|| {
        (StatusCode::NOT_FOUND, "Fichier introuvable ou expiré".to_string())
    })?;

    let file = tokio::fs::File::open(&file_info.path).await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, content_disposition(&file_info.name))
        .header(header::CONTENT_LENGTH, file_info.size)
        .body(body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    println!("📲 Téléchargement : {}", file_info.name);
    let _ = state.app_handle.emit("file-downloaded", serde_json::json!({
        "id": file_info.id, "name": file_info.name,
    }));
    Ok(response)
}

/// En-tête Content-Disposition sûr : nom ASCII de repli + nom UTF-8 encodé
/// (RFC 5987). Aucun guillemet ni saut de ligne du nom d'origine ne passe.
fn content_disposition(name: &str) -> String {
    let ascii: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') { c } else { '_' })
        .collect();
    let mut encoded = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_') {
            encoded.push(b as char);
        } else {
            encoded.push_str(&format!("%{:02X}", b));
        }
    }
    format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", ascii, encoded)
}

// ─── Utilitaires ──────────────────────────────────────────────────

fn generate_pin() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..10_000);
    format!("{:04}", n)
}

fn get_local_ip() -> String {
    use std::net::UdpSocket;
    let detected = (|| -> Option<String> {
        let s = UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("8.8.8.8:80").ok()?;
        Some(s.local_addr().ok()?.ip().to_string())
    })();

    detected.unwrap_or_else(|| {
        eprintln!("⚠️  Impossible de détecter l'IP locale (pas de réseau ?), repli sur 127.0.0.1");
        "127.0.0.1".to_string()
    })
}

// ─── Config ───────────────────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct AppConfig {
    max_file_size_mb:   u64,
    allowed_extensions: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig { max_file_size_mb: 500, allowed_extensions: vec![] }
    }
}

#[tauri::command]
async fn get_config(app: AppHandle) -> Result<AppConfig, String> {
    let path = get_config_path(&app)?;
    if !path.exists() { return Ok(AppConfig::default()); }
    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e: std::io::Error| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
}

#[tauri::command]
async fn save_config(app: AppHandle, config: AppConfig) -> Result<(), String> {
    let path = get_config_path(&app)?;
    let json = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
    tokio::fs::write(&path, json.as_bytes()).await
        .map_err(|e: std::io::Error| e.to_string())?;
    Ok(())
}

fn get_config_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir.join("config.json"))
}

#[tauri::command]
fn set_max_file_size(
    global: tauri::State<'_, GlobalState>,
    size_mb: u64,
) -> Result<(), String> {
    *global.max_file_size.lock().unwrap_or_else(|e| e.into_inner()) = size_mb * 1024 * 1024;
    println!("📏 Limite fichier : {}MB", size_mb);
    Ok(())
}

// ─── Feedback ─────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
struct FeedbackPayload {
    rating:      u8,
    category:    String,
    message:     String,
    email:       Option<String>,
    app_version: String,
    os:          String,
}

#[tauri::command]
async fn send_feedback(payload: FeedbackPayload) -> Result<String, String> {
    if !(1..=5).contains(&payload.rating) {
        return Err("Note invalide".to_string());
    }
    let message: String = payload.message.chars().take(1500).collect();
    if message.trim().is_empty() {
        return Err("Message vide".to_string());
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let res = client
        .post(format!("{}/feedback", WORKER_URL))
        .json(&serde_json::json!({
            "rating":      payload.rating,
            "category":    payload.category.chars().take(30).collect::<String>(),
            "message":     message,
            "email":       payload.email.map(|e| e.chars().take(200).collect::<String>()),
            "app_version": payload.app_version.chars().take(30).collect::<String>(),
            "os":          payload.os.chars().take(60).collect::<String>(),
        }))
        .send().await
        .map_err(|e| format!("Erreur réseau : {}", e))?;

    if res.status().is_success() {
        Ok("✅ Feedback envoyé ! Merci 🙏".to_string())
    } else {
        Err(format!("Erreur serveur : {}", res.status()))
    }
}

// ─── Updater ──────────────────────────────────────────────────────

#[tauri::command]
async fn check_update(app: AppHandle) -> Result<bool, String> {
    use tauri_plugin_updater::UpdaterExt;
    match app.updater().map_err(|e| e.to_string())?.check().await {
        Ok(Some(_)) => Ok(true),
        Ok(None)    => Ok(false),
        Err(e)      => Err(e.to_string()),
    }
}

#[tauri::command]
async fn install_update(app: AppHandle) -> Result<(), String> {
    use tauri_plugin_updater::UpdaterExt;
    let update = app.updater()
        .map_err(|e| e.to_string())?
        .check().await
        .map_err(|e| e.to_string())?;

    if let Some(update) = update {
        let _ = app.emit("update-download-progress", serde_json::json!({ "percent": 0 }));
        update.download_and_install(|_chunk, _total| {}, || {})
            .await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ─── Pending files (PC → Téléphone) — Pro seulement ──────────────

#[tauri::command]
async fn queue_file_for_send(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
    path:   String,
) -> Result<serde_json::Value, String> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let plan = global.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if !plan.allows_bidirectional() {
        return Err("Fonctionnalité réservée au plan Pro".to_string());
    }

    let file_path = PathBuf::from(&path);
    if !file_path.exists() { return Err("Fichier introuvable".to_string()); }

    let metadata = tokio::fs::metadata(&file_path).await.map_err(|e| e.to_string())?;
    let file_id  = uuid::Uuid::new_v4().to_string();
    let filename = file_path.file_name().unwrap_or_default().to_string_lossy().to_string();
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();

    let pending = PendingFile {
        id: file_id.clone(), name: filename.clone(),
        size: metadata.len(), path: path.clone(), added_at: timestamp,
    };

    global.pending_files.lock().unwrap_or_else(|e| e.into_inner()).push(pending.clone());
    println!("📤 Fichier en attente : {} ({})", filename, file_id);

    let _ = app.emit("file-queued", serde_json::json!({
        "id": file_id, "name": filename, "size": metadata.len(),
    }));
    // Prévient les téléphones connectés (sans révéler le nom du fichier)
    let _ = global.events.send(serde_json::json!({ "type": "file-queued" }).to_string());

    Ok(serde_json::json!({ "id": pending.id, "name": pending.name, "size": pending.size }))
}

#[tauri::command]
fn get_pending_files(global: tauri::State<'_, GlobalState>) -> Vec<PendingFile> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut files = global.pending_files.lock().unwrap_or_else(|e| e.into_inner());
    files.retain(|f| now - f.added_at < 600);
    files.clone()
}

#[tauri::command]
fn cancel_pending_file(
    global:  tauri::State<'_, GlobalState>,
    file_id: String,
) -> Result<(), String> {
    global.pending_files.lock().unwrap_or_else(|e| e.into_inner()).retain(|f| f.id != file_id);
    println!("❌ Fichier annulé : {}", file_id);
    Ok(())
}

// ─── Interface mobile HTML ────────────────────────────────────────
const MOBILE_UI: &str = include_str!("mobile_ui.html");

// ─── Cloudflare Tunnel ────────────────────────────────────────────

/// Chemin où cloudflared.exe sera stocké
fn get_cloudflared_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(target_os = "windows")]
    return Ok(dir.join("cloudflared.exe"));
    #[cfg(not(target_os = "windows"))]
    return Ok(dir.join("cloudflared"));
}

// Version épinglée + empreinte SHA-256 (publiée par GitHub pour chaque
// asset de la release). Pour mettre à jour : changer la version et les
// empreintes ensemble.
const CLOUDFLARED_VERSION: &str = "2026.9.1";

#[cfg(target_os = "windows")]
const CLOUDFLARED_ASSET: (&str, &str) = (
    "cloudflared-windows-amd64.exe",
    "2837888cc0f5d58f15b6dc478376de90b4d3ba5241c7947455d1e0a0df429712",
);
#[cfg(target_os = "linux")]
const CLOUDFLARED_ASSET: (&str, &str) = (
    "cloudflared-linux-amd64",
    "03f1f25d1cc93b9ad6c60569d44060bc4f17ed97075760ed8cfca4b12dcd68cc",
);
// macOS : cloudflared est publié en .tgz, non géré pour l'instant
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
const CLOUDFLARED_ASSET: (&str, &str) = ("", "");

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|b| format!("{:02x}", b)).collect()
}

/// Retourne le chemin de cloudflared en garantissant que le binaire
/// correspond exactement à la version épinglée (hash vérifié).
async fn ensure_cloudflared(app: &AppHandle) -> Result<PathBuf, String> {
    let (asset, expected_hash) = CLOUDFLARED_ASSET;
    if asset.is_empty() {
        return Err("Mode Relay cloud non disponible sur ce système".to_string());
    }

    let path = get_cloudflared_path(app)?;

    if let Ok(existing) = tokio::fs::read(&path).await {
        if sha256_hex(&existing) == expected_hash {
            println!("☁️  cloudflared déjà présent et vérifié : {:?}", path);
            return Ok(path);
        }
        println!("☁️  cloudflared présent mais version/empreinte différente, remplacement");
    }

    println!("☁️  Téléchargement de cloudflared {}...", CLOUDFLARED_VERSION);
    let url = format!(
        "https://github.com/cloudflare/cloudflared/releases/download/{}/{}",
        CLOUDFLARED_VERSION, asset
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client.get(&url)
        .send().await
        .map_err(|e| format!("Erreur téléchargement cloudflared : {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Téléchargement échoué : HTTP {}", response.status()));
    }

    let bytes = response.bytes().await
        .map_err(|e| e.to_string())?;

    if sha256_hex(&bytes) != expected_hash {
        return Err("Empreinte de cloudflared invalide : binaire rejeté".to_string());
    }

    // Écriture dans un fichier temporaire puis renommage : un téléchargement
    // interrompu ne laisse jamais un exécutable partiel à l'emplacement final.
    let tmp = path.with_extension("download");
    tokio::fs::write(&tmp, &bytes).await
        .map_err(|e| e.to_string())?;

    // Rendre exécutable sur Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms).map_err(|e| e.to_string())?;
    }

    tokio::fs::rename(&tmp, &path).await
        .map_err(|e| e.to_string())?;

    println!("✅ cloudflared téléchargé et vérifié : {:?}", path);
    Ok(path)
}

/// Lance le tunnel Cloudflare et récupère l'URL publique
async fn launch_tunnel(
    cloudflared_path: PathBuf,
    app: AppHandle,
    global_tunnel_url: Arc<Mutex<Option<String>>>,
    global_tunnel_active: Arc<Mutex<bool>>,
) -> Result<Child, String> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    let mut child = tokio::process::Command::new(&cloudflared_path)
        .args([
            "tunnel",
            "--url", "http://localhost:3030",
            "--no-autoupdate",
            "--loglevel", "info",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Impossible de lancer cloudflared : {}", e))?;

    // Lit stderr pour capturer l'URL (cloudflared écrit l'URL dans stderr)
    let stderr = child.stderr.take()
        .ok_or("Impossible de lire stderr de cloudflared")?;

    let app_clone = app.clone();
    let tunnel_url_clone = Arc::clone(&global_tunnel_url);
    let tunnel_active_clone = Arc::clone(&global_tunnel_active);

    tokio::spawn(async move {
        let reader = BufReader::new(stderr);
        let mut lines = reader.lines();

        while let Ok(Some(line)) = lines.next_line().await {
            println!("☁️  cloudflared: {}", line);

            // Cherche l'URL publique dans les logs
            // Format: "https://xxxx-xxxx-xxxx.trycloudflare.com"
            if let Some(url) = extract_tunnel_url(&line) {
                println!("🌐 Tunnel URL : {}", url);

                *tunnel_url_clone.lock().unwrap_or_else(|e| e.into_inner()) = Some(url.clone());
                *tunnel_active_clone.lock().unwrap_or_else(|e| e.into_inner()) = true;

                // Notifie React
                let _ = app_clone.emit("tunnel-ready", serde_json::json!({
                    "url": url,
                    "active": true,
                }));

                // Génère un nouveau QR code avec l'URL publique
                let _ = app_clone.emit("tunnel-url-changed", url);
            }

            // Détecte les erreurs
            if line.contains("failed") || line.contains("error") || line.contains("ERR") {
                let _ = app_clone.emit("tunnel-error", &line);
            }
        }

        // Le processus s'est arrêté
        *tunnel_active_clone.lock().unwrap_or_else(|e| e.into_inner()) = false;
        *tunnel_url_clone.lock().unwrap_or_else(|e| e.into_inner()) = None;
        let _ = app_clone.emit("tunnel-stopped", serde_json::json!({ "active": false }));
        println!("☁️  Tunnel Cloudflare arrêté");
    });

    Ok(child)
}

/// Extrait l'URL du tunnel depuis les logs de cloudflared
fn extract_tunnel_url(line: &str) -> Option<String> {
    // cloudflared écrit quelque chose comme :
    // "Your quick Tunnel has been created! Visit it at (it may take some time to be reachable):"
    // "https://example-tunnel.trycloudflare.com"
    // OU dans une seule ligne :
    // "| https://xxxx.trycloudflare.com |"

    if line.contains("trycloudflare.com") {
        // Cherche une URL https://
        let start = line.find("https://")?;
        let rest = &line[start..];
        let end = rest.find(|c: char| c.is_whitespace() || c == '|' || c == '"')
            .unwrap_or(rest.len());
        let url = rest[..end].trim().to_string();
        if url.contains("trycloudflare.com") {
            return Some(url);
        }
    }
    None
}

// ─── Commandes Tauri Tunnel ───────────────────────────────────────

#[tauri::command]
async fn get_tunnel_status(
    global: tauri::State<'_, GlobalState>,
) -> Result<serde_json::Value, String> {
    let active = *global.tunnel_active.lock().unwrap_or_else(|e| e.into_inner());
    let url    = global.tunnel_url.lock().unwrap_or_else(|e| e.into_inner()).clone();

    Ok(serde_json::json!({
        "active": active,
        "url":    url,
    }))
}

#[tauri::command]
async fn stop_tunnel(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<(), String> {
    // Extrait le Child du Mutex AVANT le .await (MutexGuard n'est pas Send)
    let child_opt = {
        let mut process = global.tunnel_process.lock().unwrap_or_else(|e| e.into_inner());
        process.take()
    };

    if let Some(mut child) = child_opt {
        let _ = child.kill().await;
        println!("☁️  Tunnel Cloudflare arrêté manuellement");
    }

    *global.tunnel_active.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *global.tunnel_url.lock().unwrap_or_else(|e| e.into_inner()) = None;
    let _ = app.emit("tunnel-stopped", serde_json::json!({ "active": false }));
    Ok(())
}

#[tauri::command]
async fn restart_tunnel(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<(), String> {
    // Extrait le Child du Mutex AVANT le .await (MutexGuard n'est pas Send)
    let child_opt = {
        let mut process = global.tunnel_process.lock().unwrap_or_else(|e| e.into_inner());
        process.take()
    };

    if let Some(mut child) = child_opt {
        let _ = child.kill().await;
    }

    *global.tunnel_active.lock().unwrap_or_else(|e| e.into_inner()) = false;
    *global.tunnel_url.lock().unwrap_or_else(|e| e.into_inner()) = None;

    // Relance — clone tout ce dont on a besoin avant le .await
    let plan = global.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if plan.allows_bidirectional() {
        let tunnel_url_arc    = Arc::clone(&global.tunnel_url);
        let tunnel_active_arc = Arc::clone(&global.tunnel_active);

        let cloudflared_path = ensure_cloudflared(&app).await?;
        let child = launch_tunnel(
            cloudflared_path,
            app.clone(),
            tunnel_url_arc,
            tunnel_active_arc,
        ).await?;

        *global.tunnel_process.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
        let _ = app.emit("tunnel-starting", true);
    }

    Ok(())
}

// ─── Point d'entrée ───────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(GlobalState {
            pin:            Arc::new(Mutex::new(String::new())),
            pin_attempts:   Arc::new(Mutex::new(PinGuard::default())),
            events:         broadcast::channel(16).0,
            sessions:       Arc::new(Mutex::new(vec![])),
            save_dir:       Arc::new(Mutex::new(PathBuf::from("."))),
            started:        Mutex::new(false),
            max_file_size:  Arc::new(Mutex::new(500 * 1024 * 1024)),
            pending_files:  Arc::new(Mutex::new(vec![])),
            plan:           Arc::new(Mutex::new(PlanType::Free)),
            daily_counter:  Arc::new(Mutex::new(DailyCounter::new())),
            device_id:      Arc::new(Mutex::new(String::new())),
            tunnel_url:     Arc::new(Mutex::new(None)),
            tunnel_process: Arc::new(Mutex::new(None)),
            tunnel_active:  Arc::new(Mutex::new(false)),
        })
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            start_server,
            get_save_dir,
            set_save_dir,
            save_history,
            load_history,
            regenerate_pin,
            get_config,
            save_config,
            set_max_file_size,
            send_feedback,
            check_update,
            install_update,
            queue_file_for_send,
            get_pending_files,
            cancel_pending_file,
            activate_license,
            check_license,
            deactivate_license,
            get_plan_info,
            get_tunnel_status,
            stop_tunnel,
            restart_tunnel,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths() {
        assert_eq!(sanitize_filename(r"..\..\Startup\evil.bat"), "evil.bat");
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename(r"C:\Windows\x.dll"), "x.dll");
        assert_eq!(sanitize_filename("photo 1.jpg"), "photo 1.jpg");
    }

    #[test]
    fn sanitize_rejects_reserved_and_empty() {
        assert_eq!(sanitize_filename(""), "fichier");
        assert_eq!(sanitize_filename(".."), "fichier");
        assert_eq!(sanitize_filename("dir/"), "fichier");
        assert_eq!(sanitize_filename("NUL.txt"), "fichier");
        assert_eq!(sanitize_filename("com1"), "fichier");
        assert_eq!(sanitize_filename("a<b>:c?.txt"), "abc.txt");
    }

    #[test]
    fn content_disposition_blocks_injection() {
        let h = content_disposition("a\"\r\nX-Evil: 1.txt");
        assert!(!h.contains('\r') && !h.contains('\n'));
        assert_eq!(h.matches('"').count(), 2);
        assert!(h.contains("filename*=UTF-8''"));
    }

    #[test]
    fn pin_lock_is_per_ip() {
        let a: IpAddr = "192.168.1.10".parse().unwrap();
        let b: IpAddr = "192.168.1.11".parse().unwrap();
        let mut g = PinGuard::default();
        for _ in 0..PinAttempts::MAX_ATTEMPTS {
            g.register_failure(a);
        }
        assert!(g.seconds_locked(a).is_some());
        assert!(g.seconds_locked(b).is_none());
    }

    #[test]
    fn pin_rotates_after_too_many_total_failures() {
        let mut g = PinGuard::default();
        let mut rotated = false;
        for i in 0..PinGuard::MAX_TOTAL_FAILURES {
            let ip: IpAddr = format!("10.0.0.{}", i + 1).parse().unwrap();
            rotated = g.register_failure(ip);
        }
        assert!(rotated);
        assert_eq!(g.total_failures, 0);
    }

    #[test]
    fn cf_connecting_ip_only_trusted_from_loopback() {
        let mut h = HeaderMap::new();
        h.insert("cf-connecting-ip", "203.0.113.7".parse().unwrap());
        let local: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let lan: SocketAddr = "192.168.1.50:5000".parse().unwrap();
        assert_eq!(client_ip(local, &h), "203.0.113.7".parse::<IpAddr>().unwrap());
        assert_eq!(client_ip(lan, &h), "192.168.1.50".parse::<IpAddr>().unwrap());
    }
}

#[cfg(test)]
mod license_tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair(seed: u8) -> (SigningKey, VerifyingKey) {
        let sk = SigningKey::from_bytes(&[seed; 32]);
        let vk = sk.verifying_key();
        (sk, vk)
    }

    fn make_token(sk: &SigningKey, payload: serde_json::Value) -> String {
        let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let sig = sk.sign(body.as_bytes());
        format!("{}.{}", body, URL_SAFE_NO_PAD.encode(sig.to_bytes()))
    }

    fn payload(plan: &str, device: &str, exp: Option<u64>) -> serde_json::Value {
        serde_json::json!({ "v": 1, "key": "TB-A-B-C-D", "plan": plan, "device_id": device, "iat": 1000, "exp": exp })
    }

    fn license(token: Option<String>) -> LicenseData {
        LicenseData {
            key: "TB-A-B-C-D".into(), plan: PlanType::Monthly, device_id: "D1".into(),
            expires_at: None, activated_at: 0, token, last_checked: 0,
        }
    }

    #[test]
    fn valid_token_grants_plan_from_signed_payload() {
        let (sk, vk) = keypair(7);
        // license.plan dit Monthly, le jeton signé dit Annual : le jeton fait foi
        let l = license(Some(make_token(&sk, payload("annual", "D1", Some(5000)))));
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Ok(PlanType::Annual));
    }

    #[test]
    fn editing_license_json_fields_changes_nothing() {
        let (sk, vk) = keypair(7);
        let mut l = license(Some(make_token(&sk, payload("monthly", "D1", Some(5000)))));
        l.plan = PlanType::Team;
        l.expires_at = None;
        l.device_id = "D1".into();
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Ok(PlanType::Monthly));
        // Expiration dépassée selon le jeton, même si license.json prétend le contraire
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 6000), Err(LicenseIssue::Expired));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let (sk, vk) = keypair(7);
        let token = make_token(&sk, payload("monthly", "D1", Some(5000)));
        let sig = token.split_once('.').unwrap().1;
        let forged_body = URL_SAFE_NO_PAD.encode(payload("team", "D1", None).to_string().as_bytes());
        let l = license(Some(format!("{}.{}", forged_body, sig)));
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Err(LicenseIssue::Invalid));
    }

    #[test]
    fn token_signed_with_another_key_is_rejected() {
        let (attacker, _) = keypair(9);
        let (_, vk) = keypair(7);
        let l = license(Some(make_token(&attacker, payload("team", "D1", None))));
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Err(LicenseIssue::Invalid));
    }

    #[test]
    fn token_is_bound_to_device() {
        let (sk, vk) = keypair(7);
        let l = license(Some(make_token(&sk, payload("monthly", "D1", Some(5000)))));
        assert_eq!(evaluate_license_with(&vk, &l, "AUTRE", 1000), Err(LicenseIssue::WrongDevice));
    }

    #[test]
    fn expiry_rules() {
        let (sk, vk) = keypair(7);
        let expiring = license(Some(make_token(&sk, payload("monthly", "D1", Some(5000)))));
        assert!(evaluate_license_with(&vk, &expiring, "D1", 4999).is_ok());
        assert_eq!(evaluate_license_with(&vk, &expiring, "D1", 5001), Err(LicenseIssue::Expired));
        let forever = license(Some(make_token(&sk, payload("team", "D1", None))));
        assert_eq!(evaluate_license_with(&vk, &forever, "D1", u64::MAX / 2), Ok(PlanType::Team));
    }

    #[test]
    fn missing_or_foreign_token_is_rejected() {
        let (sk, vk) = keypair(7);
        assert_eq!(evaluate_license_with(&vk, &license(None), "D1", 1000), Err(LicenseIssue::NoToken));
        // jeton d'une autre clé de licence
        let mut other = payload("monthly", "D1", None);
        other["key"] = serde_json::json!("TB-X-X-X-X");
        let l = license(Some(make_token(&sk, other)));
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Err(LicenseIssue::Invalid));
        // un jeton "free" n'a aucun sens
        let l = license(Some(make_token(&sk, payload("free", "D1", None))));
        assert_eq!(evaluate_license_with(&vk, &l, "D1", 1000), Err(LicenseIssue::Invalid));
    }

    #[test]
    fn malformed_tokens_do_not_panic() {
        let (_, vk) = keypair(7);
        for t in ["", ".", "abc", "a.b", "!!!.???", "e30.e30"] {
            assert!(verify_license_token_with(t, &vk).is_err());
        }
    }
}

#[cfg(test)]
mod interop_tests {
    use super::*;

    // Jeton réellement produit par le Worker dans le runtime Cloudflare (workerd),
    // avec une clé jetable : garantit que la convention de signature du Worker
    // (Ed25519 sur la chaîne base64url du corps) est bien celle vérifiée par l'app.
    const WORKER_PUBLIC_KEY_B64: &str = "CJ6WrBjzzuPkiAJiwe1BbVu9Ba/o5naHfwgcdJuG91A=";
    const WORKER_TOKEN: &str = "eyJ2IjoxLCJrZXkiOiJUQi05OTk5LTk5OTktOTk5OS05OTk5IiwicGxhbiI6ImFubnVhbCIsImRldmljZV9pZCI6IlRCREVWLUxPQ0FMIiwiaWF0IjoxNzkwMDMyMTQ2LCJleHAiOjE4MjE1NjgxNDZ9.s_wzFO6fmFqtD3Ucy4fIcQxMvkqMQ_FBbzZLP2VDRIeRYGlLD9HfgCkUj_9FNApM_EbjY9-iEPsTH1TBTQ5PDQ";

    #[test]
    fn app_verifies_a_token_signed_by_the_cloudflare_worker() {
        let bytes = STANDARD.decode(WORKER_PUBLIC_KEY_B64).unwrap();
        let vk = VerifyingKey::from_bytes(&bytes.try_into().unwrap()).unwrap();
        let t = verify_license_token_with(WORKER_TOKEN, &vk).expect("signature du Worker acceptée");
        assert_eq!(t.key, "TB-9999-9999-9999-9999");
        assert_eq!(t.plan, PlanType::Annual);
        assert_eq!(t.device_id, "TBDEV-LOCAL");
        assert_eq!(t.exp, Some(1821568146));
    }
}

#[cfg(test)]
mod key_config_tests {
    use super::*;

    #[test]
    fn embedded_license_public_key_is_a_valid_ed25519_key() {
        license_verifying_key().expect("LICENSE_PUBLIC_KEY_B64 doit être une clé Ed25519 valide");
    }
}

#[cfg(test)]
mod mobile_page_tests {
    use super::*;

    #[test]
    fn mobile_page_shows_the_real_app_version() {
        let html = mobile_ui_html();
        assert!(!html.contains("__APP_VERSION__"), "le marqueur de version doit être remplacé");
        assert!(html.contains(&format!("v{}", env!("CARGO_PKG_VERSION"))));
    }
}
