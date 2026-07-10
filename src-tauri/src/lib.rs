use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Multipart, State, Query},
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{Method, StatusCode, header},
    response::{Html, Response},
    routing::{get, post},
    Json, Router,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};
use tokio::process::Child;

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
}

// ─── Compteur journalier ──────────────────────────────────────────

#[derive(Clone, Debug)]
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

// ─── État partagé ─────────────────────────────────────────────────

struct GlobalState {
    pin:            Arc<Mutex<String>>,
    pin_attempts:   Arc<Mutex<PinAttempts>>,
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

    fn register_success(&mut self) {
        self.fail_count = 0;
        self.locked_until = None;
    }
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
    pin_attempts:  Arc<Mutex<PinAttempts>>,
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

fn generate_device_id() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let hostname = hostname::get()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    let username = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "unknown".to_string());

    // Volume serial / machine-id
    let machine_extra = {
        #[cfg(target_os = "windows")]
        {
            std::fs::read_to_string("C:\\Windows\\System32\\drivers\\etc\\hosts")
                .map(|c| c.len().to_string())
                .unwrap_or_else(|_| "win".to_string())
        }
        #[cfg(not(target_os = "windows"))]
        {
            std::fs::read_to_string("/etc/machine-id")
                .unwrap_or_else(|_| "unix".to_string())
        }
    };

    let raw = format!("TB-{}:{}:{}", hostname, username, machine_extra);
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    let h = hasher.finish();
    format!("TBDEV-{:016X}", h)
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
            *global.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()) = PinAttempts::default();
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

    // Charge la licence si elle existe
    if let Ok(license) = load_license_data(&app).await {
        *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = license.plan;
    }

    let pin = generate_pin();
    *global.pin.lock().unwrap_or_else(|e| e.into_inner()) = pin.clone();

    let state = AppState {
        pin:           Arc::clone(&global.pin),
        pin_attempts:  Arc::clone(&global.pin_attempts),
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

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(Any)
        .allow_headers(Any);

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
        // (protège la mémoire du process ; la limite réelle par plan est
        // appliquée dans handle_upload une fois le champ lu).
        .layer(DefaultBodyLimit::max(20 * 1024 * 1024 * 1024))
        .layer(cors);

    tokio::spawn(async move {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                println!("🚀 Serveur démarré sur {}", addr);
                axum::serve(listener, router).await.unwrap();
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
    *global.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()) = PinAttempts::default();
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

async fn load_license_data(app: &AppHandle) -> Result<LicenseData, String> {
    let path = get_license_path(app)?;
    if !path.exists() { return Err("Pas de licence".to_string()); }
    let content = tokio::fs::read_to_string(&path).await
        .map_err(|e: std::io::Error| e.to_string())?;
    serde_json::from_str(&content).map_err(|e| e.to_string())
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
        return Err("Format de clé invalide".to_string());
    }

    let plan_type = match plan.as_str() {
        "monthly" => PlanType::Monthly,
        "annual"  => PlanType::Annual,
        "team"    => PlanType::Team,
        _         => return Err("Plan inconnu".to_string()),
    };

    let device_id = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();

    // ── Vérification côté serveur — OBLIGATOIRE, fail-closed ──
    // Pointe vers le Worker Cloudflare qui vérifie la signature HMAC.
    // Remplace par ton URL réelle de Worker une fois déployé.
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;

    let res = client
        .post("https://transferbridge-license.abouacero1998.workers.dev/")
        .json(&serde_json::json!({
            "key":       key,
            "plan":      plan,
            "device_id": device_id,
        }))
        .send().await;

    // Fail-closed : si le serveur ne confirme pas explicitement le succès,
    // on REFUSE l'activation. Plus de bypass possible si le serveur est down.
    match res {
        Ok(resp) => {
            let status = resp.status();
            if !status.is_success() {
                // Tente de récupérer le message d'erreur du serveur
                let err_msg = resp.json::<serde_json::Value>().await
                    .ok()
                    .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(|s| s.to_string()))
                    .unwrap_or_else(|| "Clé invalide ou déjà utilisée sur un autre appareil".to_string());
                return Err(err_msg);
            }
            // Vérifie explicitement le champ "success" dans la réponse
            let body: serde_json::Value = resp.json().await
                .map_err(|_| "Réponse du serveur de licence invalide".to_string())?;
            if body.get("success").and_then(|v| v.as_bool()) != Some(true) {
                return Err("Vérification de licence échouée".to_string());
            }
        }
        Err(_) => {
            // Le serveur est inaccessible (pas d'internet, Worker down, etc.)
            // → on REFUSE par sécurité, contrairement à avant.
            return Err(
                "Impossible de vérifier la licence — vérifie ta connexion internet et réessaie."
                    .to_string()
            );
        }
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    // Expiration : mensuel = 30j, annuel = 365j, team = aucune
    let expires_at = match plan_type {
        PlanType::Monthly => Some(now + 30 * 24 * 3600),
        PlanType::Annual  => Some(now + 365 * 24 * 3600),
        PlanType::Team    => None,
        PlanType::Free    => None,
    };

    let license = LicenseData {
        key: key.clone(),
        plan: plan_type.clone(),
        device_id,
        expires_at,
        activated_at: now,
    };

    let json = serde_json::to_string_pretty(&license).map_err(|e| e.to_string())?;
    let path = get_license_path(&app)?;
    tokio::fs::write(&path, json.as_bytes()).await
        .map_err(|e: std::io::Error| e.to_string())?;

    *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = plan_type;

    let _ = app.emit("plan-changed", license.plan.label());
    println!("⚡ Plan activé : {}", plan);
    Ok(())
}

#[tauri::command]
async fn check_license(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<serde_json::Value, String> {
    match load_license_data(&app).await {
        Err(_) => Ok(serde_json::json!({ "plan": "free", "valid": true })),
        Ok(license) => {
            // Vérifie l'expiration
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();

            if let Some(exp) = license.expires_at {
                if now > exp {
                    // Licence expirée → retour au plan gratuit
                    *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = PlanType::Free;
                    let _ = tokio::fs::remove_file(get_license_path(&app)?).await;
                    return Ok(serde_json::json!({
                        "plan": "free",
                        "valid": false,
                        "expired": true
                    }));
                }
            }

            // Vérifie le device ID
            let current_device = global.device_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if license.device_id != current_device
                && license.plan != PlanType::Team {
                return Ok(serde_json::json!({
                    "plan": "free",
                    "valid": false,
                    "wrong_device": true
                }));
            }

            *global.plan.lock().unwrap_or_else(|e| e.into_inner()) = license.plan.clone();

            Ok(serde_json::json!({
                "plan":         license.plan,
                "plan_label":   license.plan.label(),
                "valid":        true,
                "expires_at":   license.expires_at,
                "key":          license.key,
            }))
        }
    }
}

#[tauri::command]
async fn deactivate_license(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<(), String> {
    let path = get_license_path(&app)?;
    if path.exists() {
        tokio::fs::remove_file(&path).await
            .map_err(|e: std::io::Error| e.to_string())?;
    }
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
    // Envoie les infos du plan au téléphone dès la connexion
    let plan_info = {
        let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
        serde_json::json!({
            "type":           "plan-info",
            "bidirectional":  plan.allows_bidirectional(),
            "plan":           plan.label(),
        })
    };
    let _ = socket.send(Message::Text(plan_info.to_string().into())).await;

    while let Some(Ok(msg)) = {
        use futures_util::StreamExt;
        socket.next().await
    } {
        match msg {
            Message::Text(text) => {
                if let Ok(data) = serde_json::from_str::<serde_json::Value>(&text) {
                    if data["type"] == "progress" {
                        let filename = data["filename"].as_str().unwrap_or("").to_string();
                        let percent  = data["percent"].as_f64().unwrap_or(0.0);
                        let _ = state.app_handle.emit("upload-progress",
                            serde_json::json!({ "filename": filename, "percent": percent }));
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
}

// ─── Routes HTTP ──────────────────────────────────────────────────

async fn serve_mobile_ui() -> Html<String> {
    Html(MOBILE_UI.to_string())
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
        "bidirectional": plan.allows_bidirectional(),
        "uploads_left":  counter.remaining(plan.max_uploads_per_day()),
        "uploads_limit": plan.max_uploads_per_day(),
    })))
}

async fn verify_pin(
    State(state): State<AppState>,
    Json(body):   Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    // Anti brute-force : refuse toute tentative pendant le verrouillage,
    // sans même comparer le PIN (évite de "gaspiller" une fenêtre de timing).
    if let Some(retry_after) = state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()).seconds_locked() {
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
        let retry_after = {
            let mut attempts = state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner());
            attempts.register_failure();
            attempts.seconds_locked()
        };
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

    state.pin_attempts.lock().unwrap_or_else(|e| e.into_inner()).register_success();

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
    let mut token = String::new();
    let mut files_data: Vec<(String, Vec<u8>)> = vec![];
    let max_size = *state.max_file_size.lock().unwrap_or_else(|e| e.into_inner());

    while let Some(field) = multipart.next_field().await
        .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "token" {
            token = field.text().await
                .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?;
        } else {
            let filename = field.file_name().unwrap_or("fichier").to_string();
            let data = field.bytes().await
                .map_err(|e: axum::extract::multipart::MultipartError| (StatusCode::BAD_REQUEST, e.to_string()))?;

            // Vérif taille selon plan
            let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let plan_max = plan.max_file_size_bytes();
            let effective_max = if plan_max == 0 { max_size } else { plan_max.min(max_size) };

            if effective_max > 0 && data.len() as u64 > effective_max {
                let msg = format!("❌ '{}' dépasse la limite ({:.0}MB)", filename, effective_max as f64 / 1_048_576.0);
                let _ = state.app_handle.emit("upload-error", serde_json::json!({
                    "filename": filename, "error": "too_large", "message": msg.clone()
                }));
                return Err((StatusCode::PAYLOAD_TOO_LARGE, msg));
            }

            files_data.push((filename, data.to_vec()));
        }
    }

    if !is_valid_session(&state.sessions, &token) {
        let _ = state.app_handle.emit("upload-error", serde_json::json!({
            "error": "session_expired", "message": "Session expirée — reconnecte-toi"
        }));
        return Err((StatusCode::UNAUTHORIZED, serde_json::json!({ "error": "session_expired" }).to_string()));
    }

    // Vérif limite journalière
    {
        let plan = state.plan.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut counter = state.daily_counter.lock().unwrap_or_else(|e| e.into_inner());

        if !counter.can_upload(plan.max_uploads_per_day()) {
            let _ = state.app_handle.emit("upload-error", serde_json::json!({
                "error":   "daily_limit",
                "message": "Limite journalière atteinte (10/jour). Passez à Pro pour un accès illimité."
            }));
            return Err((StatusCode::TOO_MANY_REQUESTS, serde_json::json!({
                "error": "daily_limit",
                "message": "Limite de 10 envois/jour atteinte. Passez à Pro !"
            }).to_string()));
        }

        // Incrémente le compteur pour chaque fichier
        for _ in &files_data {
            counter.increment();
        }
    }

    for (filename, data) in files_data {
        let file_size: usize = data.len();
        let save_path = {
            let dir = state.save_dir.lock().unwrap_or_else(|e| e.into_inner());
            get_unique_path(dir.join(&filename))
        };
        tokio::fs::write(&save_path, data.as_slice()).await
            .map_err(|e: std::io::Error| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

        println!("✅ Reçu : {} ({} octets)", filename, file_size);
        let _ = state.app_handle.emit("file-received", serde_json::json!({
            "name": filename, "size": file_size, "path": save_path.to_string_lossy()
        }));
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
    let encoded_name = file_info.name.replace(' ', "%20");

    let response = axum::response::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{}\"", encoded_name))
        .header(header::CONTENT_LENGTH, file_info.size)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
        .body(body)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    println!("📲 Téléchargement : {}", file_info.name);
    Ok(response)
}

// ─── Utilitaires ──────────────────────────────────────────────────

fn generate_pin() -> String {
    use rand::Rng;
    let n: u32 = rand::thread_rng().gen_range(0..10_000);
    format!("{:04}", n)
}

fn get_unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() { return path; }
    let stem   = path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let ext    = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let parent = path.parent().unwrap_or(std::path::Path::new("."));
    let mut i  = 1;
    loop {
        let p = parent.join(format!("{}_{}{}", stem, i, ext));
        if !p.exists() { return p; }
        i += 1;
    }
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
    let stars = match payload.rating {
        5 => "⭐⭐⭐⭐⭐", 4 => "⭐⭐⭐⭐", 3 => "⭐⭐⭐", 2 => "⭐⭐", _ => "⭐",
    };
    let category_emoji = match payload.category.as_str() {
        "bug" => "🐛 Bug", "feature" => "💡 Idée",
        "performance" => "⚡ Performance", "ux" => "🎨 UX/Design", _ => "💬 Général",
    };
    let email_str = payload.email
        .filter(|e| !e.is_empty())
        .map(|e| format!("`{}`", e))
        .unwrap_or_else(|| "*Anonyme*".to_string());

    let discord_msg = serde_json::json!({
        "embeds": [{
            "title": format!("{} Nouveau feedback TransferBridge", stars),
            "color": match payload.rating { 5=>0x22C55E, 4=>0x3B82F6, 3=>0xF59E0B, 2=>0xF97316, _=>0xEF4444 },
            "fields": [
                { "name": "⭐ Note",       "value": format!("{}/5 {}", payload.rating, stars), "inline": true },
                { "name": "🏷️ Catégorie", "value": category_emoji, "inline": true },
                { "name": "💻 OS",         "value": &payload.os, "inline": true },
                { "name": "📦 Version",    "value": &payload.app_version, "inline": true },
                { "name": "📧 Email",      "value": email_str, "inline": true },
                { "name": "💬 Message",    "value": &payload.message, "inline": false },
            ],
            "footer": { "text": "TransferBridge Feedback System" },
            "timestamp": chrono::Utc::now().to_rfc3339()
        }]
    });

    let webhook_url = "https://discord.com/api/webhooks/1489240590427099266/0tWrGqPXORR-WtVPLsPiVcx1t6t_Nni0pPjK9kRFeLqsP9vdt5XWvFSeADnSVxc56ele";
    let client = reqwest::Client::new();
    let res = client.post(webhook_url)
        .header("Content-Type", "application/json")
        .body(discord_msg.to_string())
        .send().await
        .map_err(|e| format!("Erreur réseau : {}", e))?;

    if res.status().is_success() {
        Ok("✅ Feedback envoyé ! Merci 🙏".to_string())
    } else {
        Err(format!("Erreur Discord : {}", res.status()))
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

/// Télécharge cloudflared depuis GitHub si absent
async fn ensure_cloudflared(app: &AppHandle) -> Result<PathBuf, String> {
    let path = get_cloudflared_path(app)?;

    if path.exists() {
        println!("☁️  cloudflared déjà présent : {:?}", path);
        return Ok(path);
    }

    println!("☁️  Téléchargement de cloudflared...");

    // URL de la dernière version stable
    #[cfg(target_os = "windows")]
    let url = "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-windows-amd64.exe";
    #[cfg(target_os = "macos")]
    let url = "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-darwin-amd64";
    #[cfg(target_os = "linux")]
    let url = "https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-linux-amd64";

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .map_err(|e| e.to_string())?;

    let response = client.get(url)
        .send().await
        .map_err(|e| format!("Erreur téléchargement cloudflared : {}", e))?;

    if !response.status().is_success() {
        return Err(format!("Téléchargement échoué : HTTP {}", response.status()));
    }

    let bytes = response.bytes().await
        .map_err(|e| e.to_string())?;

    tokio::fs::write(&path, &bytes).await
        .map_err(|e| e.to_string())?;

    // Rendre exécutable sur Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).map_err(|e| e.to_string())?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).map_err(|e| e.to_string())?;
    }

    println!("✅ cloudflared téléchargé : {:?}", path);
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
            pin_attempts:   Arc::new(Mutex::new(PinAttempts::default())),
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