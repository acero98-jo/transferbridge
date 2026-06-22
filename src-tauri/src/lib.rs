use axum::{
    body::Body,
    extract::{Path, Multipart, State, Query},
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
        let mut started = global.started.lock().unwrap();
        if *started {
            let pin = generate_pin();
            *global.pin.lock().unwrap() = pin.clone();
            global.sessions.lock().unwrap().clear();
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
    *global.device_id.lock().unwrap() = device_id;

    let save_dir = app.path().download_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    *global.save_dir.lock().unwrap() = save_dir;

    // Charge la licence si elle existe
    if let Ok(license) = load_license_data(&app).await {
        *global.plan.lock().unwrap() = license.plan;
    }

    let pin = generate_pin();
    *global.pin.lock().unwrap() = pin.clone();

    let state = AppState {
        pin:           Arc::clone(&global.pin),
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
    let plan = global.plan.lock().unwrap().clone();
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
                            *tunnel_proc.lock().unwrap() = Some(child);
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
    *global.pin.lock().unwrap() = new_pin.clone();
    global.sessions.lock().unwrap().clear();
    let _ = app.emit("pin-generated", new_pin.clone());
    Ok(new_pin)
}

#[tauri::command]
fn get_save_dir(global: tauri::State<'_, GlobalState>) -> String {
    global.save_dir.lock().unwrap().to_string_lossy().to_string()
}

#[tauri::command]
fn set_save_dir(
    global: tauri::State<'_, GlobalState>,
    path: String,
) -> Result<(), String> {
    *global.save_dir.lock().unwrap() = PathBuf::from(&path);
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
    let plan = global.plan.lock().unwrap().clone();
    let mut counter = global.daily_counter.lock().unwrap();
    counter.reset_if_new_day();
    let limit = plan.max_uploads_per_day();
    let left  = counter.remaining(limit);
    let device_id = global.device_id.lock().unwrap().clone();

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

    let device_id = global.device_id.lock().unwrap().clone();

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

    *global.plan.lock().unwrap() = plan_type;

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
                    *global.plan.lock().unwrap() = PlanType::Free;
                    let _ = tokio::fs::remove_file(get_license_path(&app)?).await;
                    return Ok(serde_json::json!({
                        "plan": "free",
                        "valid": false,
                        "expired": true
                    }));
                }
            }

            // Vérifie le device ID
            let current_device = global.device_id.lock().unwrap().clone();
            if license.device_id != current_device
                && license.plan != PlanType::Team {
                return Ok(serde_json::json!({
                    "plan": "free",
                    "valid": false,
                    "wrong_device": true
                }));
            }

            *global.plan.lock().unwrap() = license.plan.clone();

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
    *global.plan.lock().unwrap() = PlanType::Free;
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
        let plan = state.plan.lock().unwrap().clone();
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
    let plan = state.plan.lock().unwrap().clone();
    let mut counter = state.daily_counter.lock().unwrap();
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
    let input   = body["pin"].as_str().unwrap_or("").to_string();
    let correct = state.pin.lock().unwrap().clone();

    println!("🔐 Vérification PIN : saisie='{}' correct='{}'", input, correct);

    if input != correct {
        let _ = state.app_handle.emit("pin-failed", &input);
        return Err((
            StatusCode::UNAUTHORIZED,
            serde_json::json!({ "success": false, "error": "PIN incorrect" }).to_string(),
        ));
    }

    let token = uuid::Uuid::new_v4().to_string();
    state.sessions.lock().unwrap().push(Session {
        token:      token.clone(),
        expires_at: Instant::now() + Duration::from_secs(600),
    });

    let _ = state.app_handle.emit("device-connected", serde_json::json!({
        "time": chrono::Local::now().format("%H:%M").to_string()
    }));

    // Envoie aussi les infos du plan avec le token
    let plan = state.plan.lock().unwrap().clone();
    let mut counter = state.daily_counter.lock().unwrap();
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
    let mut s = sessions.lock().unwrap();
    s.retain(|s| s.expires_at > Instant::now());
    s.iter().any(|s| s.token == token)
}

async fn handle_upload(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<String, (StatusCode, String)> {
    let mut token = String::new();
    let mut files_data: Vec<(String, Vec<u8>)> = vec![];
    let max_size = *state.max_file_size.lock().unwrap();

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
            let plan = state.plan.lock().unwrap().clone();
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
        let plan = state.plan.lock().unwrap().clone();
        let mut counter = state.daily_counter.lock().unwrap();

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
            let dir = state.save_dir.lock().unwrap();
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
    let plan = state.plan.lock().unwrap().clone();
    let mut counter = state.daily_counter.lock().unwrap();
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
    let plan = state.plan.lock().unwrap().clone();
    if !plan.allows_bidirectional() {
        return Ok(Json(serde_json::json!({ "files": [], "pro_required": true })));
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let mut files = state.pending_files.lock().unwrap();
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

    let plan = state.plan.lock().unwrap().clone();
    if !plan.allows_bidirectional() {
        return Err((StatusCode::FORBIDDEN, "Fonctionnalité Pro requise".to_string()));
    }

    let file_info = {
        let files = state.pending_files.lock().unwrap();
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
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos();
    let tid = format!("{:?}", std::thread::current().id());
    let seed = nanos ^ tid.bytes().fold(0u32, |a, b| a.wrapping_add(b as u32));
    format!("{:04}", seed % 10000)
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
    let s = UdpSocket::bind("0.0.0.0:0").unwrap();
    s.connect("8.8.8.8:80").unwrap();
    s.local_addr().unwrap().ip().to_string()
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
    *global.max_file_size.lock().unwrap() = size_mb * 1024 * 1024;
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

    let webhook_url = "REMPLACE_PAR_TON_WEBHOOK_DISCORD";
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

    let plan = global.plan.lock().unwrap().clone();
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

    global.pending_files.lock().unwrap().push(pending.clone());
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
    let mut files = global.pending_files.lock().unwrap();
    files.retain(|f| now - f.added_at < 600);
    files.clone()
}

#[tauri::command]
fn cancel_pending_file(
    global:  tauri::State<'_, GlobalState>,
    file_id: String,
) -> Result<(), String> {
    global.pending_files.lock().unwrap().retain(|f| f.id != file_id);
    println!("❌ Fichier annulé : {}", file_id);
    Ok(())
}

// ─── Interface mobile HTML ────────────────────────────────────────
const MOBILE_UI: &str = r#"<!DOCTYPE html>
<html lang="fr">
<head>
  <meta charset="UTF-8">
  <meta name="viewport" content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=no">
  <title>TransferBridge</title>
  <style>
    * { box-sizing: border-box; margin: 0; padding: 0; -webkit-tap-highlight-color: transparent; }
    body { font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', sans-serif; background: #0f172a; color: #f1f5f9; min-height: 100vh; display: flex; flex-direction: column; align-items: center; justify-content: center; padding: 24px; }
    .logo { font-size: 48px; text-align: center; margin-bottom: 12px; }
    h1 { font-size: 24px; font-weight: 700; text-align: center; margin-bottom: 4px; }
    .subtitle { color: #94a3b8; font-size: 14px; text-align: center; margin-bottom: 32px; }

    /* PIN */
    #pin-screen { width: 100%; max-width: 340px; }
    .pin-label { font-size: 15px; color: #94a3b8; text-align: center; margin-bottom: 20px; }
    .pin-inputs { display: flex; justify-content: center; gap: 12px; margin-bottom: 24px; }
    .pin-digit { width: 60px; height: 68px; background: #1e293b; border: 2px solid #334155; border-radius: 12px; font-size: 28px; font-weight: 700; color: #f1f5f9; text-align: center; outline: none; transition: border-color 0.2s; caret-color: transparent; }
    .pin-digit:focus { border-color: #3b82f6; }
    .pin-digit.filled { border-color: #3b82f6; background: #1e3a5f; }
    .pin-digit.error { border-color: #ef4444; animation: shake 0.3s; }
    @keyframes shake { 0%,100%{transform:translateX(0)} 25%{transform:translateX(-6px)} 75%{transform:translateX(6px)} }
    .pin-btn { width: 100%; padding: 18px; background: #334155; color: white; border: none; border-radius: 14px; font-size: 17px; font-weight: 600; cursor: pointer; }
    .pin-btn.active { background: #3b82f6; }
    .pin-error { margin-top: 14px; padding: 12px; background: #450a0a; border-radius: 10px; color: #fca5a5; font-size: 13px; text-align: center; display: none; }

    /* Upload */
    #upload-screen { width: 100%; max-width: 400px; display: none; }
    .connected-badge { background: #14532d; color: #86efac; padding: 10px 16px; border-radius: 20px; font-size: 13px; font-weight: 600; text-align: center; margin-bottom: 12px; }

    /* Compteur journalier */
    .daily-counter { display: flex; align-items: center; justify-content: space-between; background: #1e293b; border-radius: 12px; padding: 10px 16px; margin-bottom: 12px; }
    .counter-text { font-size: 13px; color: #94a3b8; }
    .counter-bar-wrap { width: 80px; height: 6px; background: #334155; border-radius: 3px; overflow: hidden; }
    .counter-bar { height: 100%; border-radius: 3px; background: #3b82f6; transition: width 0.3s; }
    .counter-bar.warn { background: #f59e0b; }
    .counter-bar.full { background: #ef4444; }
    .counter-limit { font-size: 12px; font-weight: 700; color: #f1f5f9; }

    /* Bannière Pro */
    .pro-banner { background: linear-gradient(135deg, rgba(59,130,246,0.15), rgba(6,182,212,0.15)); border: 1px solid rgba(59,130,246,0.3); border-radius: 12px; padding: 12px 16px; margin-bottom: 12px; text-align: center; }
    .pro-banner-title { font-size: 14px; font-weight: 700; color: #60a5fa; margin-bottom: 4px; }
    .pro-banner-sub { font-size: 12px; color: #64748b; }
    .pro-banner-btn { display: inline-block; margin-top: 8px; padding: 8px 16px; background: #3b82f6; color: white; border-radius: 8px; font-size: 12px; font-weight: 600; text-decoration: none; }

    /* Zone upload */
    .drop-zone { width: 100%; border: 2px dashed #334155; border-radius: 16px; padding: 40px 24px; text-align: center; cursor: pointer; transition: all 0.2s; background: #1e293b; }
    .drop-zone.disabled { opacity: 0.5; cursor: not-allowed; }
    .drop-zone.drag-over { border-color: #3b82f6; background: #1e3a5f; }
    .drop-zone input { display: none; }
    .drop-icon { font-size: 40px; margin-bottom: 12px; }
    .drop-text { color: #94a3b8; font-size: 15px; }
    .drop-text span { color: #3b82f6; font-weight: 600; }
    .file-list { width: 100%; margin-top: 12px; }
    .file-item { background: #1e293b; border-radius: 10px; padding: 12px 14px; margin-bottom: 8px; display: flex; align-items: center; gap: 10px; }
    .file-icon { font-size: 22px; }
    .file-info { flex: 1; min-width: 0; }
    .file-name { font-size: 13px; font-weight: 500; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .file-size-t { font-size: 11px; color: #64748b; margin-bottom: 4px; }
    .progress-bar { width: 100%; height: 6px; background: #334155; border-radius: 3px; overflow: hidden; }
    .progress-fill { height: 100%; border-radius: 3px; transition: width 0.15s ease; width: 0%; background: linear-gradient(90deg, #3b82f6, #06b6d4); }
    .progress-fill.done { background: #22c55e; }
    .progress-fill.error { background: #ef4444; }
    .progress-label { font-size: 11px; color: #64748b; margin-top: 3px; display: flex; justify-content: space-between; }
    .file-status { font-size: 18px; }
    .send-btn { width: 100%; margin-top: 14px; padding: 16px; background: #3b82f6; color: white; border: none; border-radius: 14px; font-size: 16px; font-weight: 600; cursor: pointer; }
    .send-btn:disabled { background: #334155; cursor: not-allowed; }
    .result { margin-top: 12px; padding: 12px; border-radius: 10px; font-size: 13px; text-align: center; display: none; }
    .result.ok { background: #14532d; color: #86efac; }
    .result.err { background: #450a0a; color: #fca5a5; }
    .result.limit { background: #451a03; color: #fed7aa; border: 1px solid #92400e; }

    /* Onglets Pro */
    .tabs { display: flex; background: #1e293b; border-radius: 12px; padding: 4px; gap: 4px; margin-bottom: 12px; }
    .tab-btn { flex: 1; padding: 10px; border: none; border-radius: 8px; font-size: 14px; font-weight: 600; cursor: pointer; transition: all 0.2s; }
    .tab-btn.active { background: #3b82f6; color: white; }
    .tab-btn.inactive { background: transparent; color: #64748b; }
    .tab-btn.pro-locked { color: #475569; cursor: default; }
    .dl-item { background: #1e293b; border-radius: 12px; padding: 14px 16px; display: flex; align-items: center; gap: 12px; margin-bottom: 10px; }
    .dl-btn { padding: 10px 16px; background: #3b82f6; color: white; border-radius: 8px; font-size: 13px; font-weight: 600; text-decoration: none; white-space: nowrap; }
    .refresh-btn { width: 100%; margin-top: 16px; padding: 12px; background: transparent; color: #64748b; border: 1px solid #334155; border-radius: 12px; font-size: 14px; cursor: pointer; }
    .empty-state { text-align: center; padding: 40px 0; color: #64748b; }
  </style>
</head>
<body>
  <div class="logo">📁</div>
  <h1>TransferBridge</h1>
  <p class="subtitle">Transfert sécurisé · v1.2.0</p>

  <!-- PIN Screen -->
  <div id="pin-screen">
    <p class="pin-label">🔐 Saisis le code PIN affiché sur le PC</p>
    <div class="pin-inputs">
      <input class="pin-digit" id="p0" type="tel" maxlength="1" inputmode="numeric" autocomplete="one-time-code">
      <input class="pin-digit" id="p1" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
      <input class="pin-digit" id="p2" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
      <input class="pin-digit" id="p3" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
    </div>
    <button class="pin-btn" id="pinBtn" onclick="submitPin()">Valider</button>
    <div class="pin-error" id="pinError">❌ PIN incorrect. Demande un nouveau PIN sur le PC.</div>
  </div>

  <!-- Upload Screen -->
  <div id="upload-screen">
    <div class="connected-badge" id="connectedBadge">✅ Connecté</div>

    <!-- Compteur journalier (gratuit seulement) -->
    <div class="daily-counter" id="dailyCounter" style="display:none">
      <div class="counter-text">Envois aujourd'hui</div>
      <div class="counter-bar-wrap">
        <div class="counter-bar" id="counterBar"></div>
      </div>
      <div class="counter-limit"><span id="counterLeft">10</span>/<span id="counterMax">10</span></div>
    </div>

    <!-- Bannière Pro quand limite atteinte -->
    <div class="pro-banner" id="proBanner" style="display:none">
      <div class="pro-banner-title">🚀 Limite journalière atteinte</div>
      <div class="pro-banner-sub">Passez à Pro pour des envois illimités, taille illimitée et bien plus.</div>
      <a href="https://transferbridge.site/checkout.html" target="_blank" class="pro-banner-btn">⚡ Passer à Pro — 19.99€/mois</a>
    </div>

    <!-- Onglets (Pro seulement) -->
    <div class="tabs" id="tabs" style="display:none">
      <button class="tab-btn active" id="tab-receive" onclick="switchTab('receive')">📥 Recevoir</button>
      <button class="tab-btn inactive" id="tab-download" onclick="switchTab('download')">📤 Du PC</button>
    </div>

    <!-- Panel Recevoir -->
    <div id="panel-receive">
      <div class="drop-zone" id="dropZone">
        <input type="file" id="fileInput" multiple accept="*/*">
        <div class="drop-icon">📂</div>
        <div class="drop-text"><span>Appuie ici</span> pour sélectionner<br>photos, vidéos, PDF, tout type</div>
      </div>
      <div class="file-list" id="fileList"></div>
      <button class="send-btn" id="sendBtn" style="display:none">🚀 Envoyer sur le PC</button>
      <div class="result" id="result"></div>
    </div>

    <!-- Panel Télécharger (Pro) -->
    <div id="panel-download" style="display:none">
      <div id="dl-empty" class="empty-state">
        <div style="font-size:40px;margin-bottom:12px">📭</div>
        <p>Aucun fichier en attente</p>
        <p style="font-size:12px;margin-top:4px;color:#475569">Envoie des fichiers depuis l'app PC</p>
      </div>
      <div id="dl-list"></div>
      <button class="refresh-btn" onclick="refreshDownloads()">🔄 Actualiser</button>
    </div>
  </div>

  <script>
    let sessionToken = null, selectedFiles = [], ws = null;
    let planInfo = { plan: 'Gratuit', bidirectional: false, uploads_left: 10, uploads_limit: 10 };

    // ── WebSocket ──
    function connectWS() {
      ws = new WebSocket('ws://' + location.host + '/ws');
      ws.onmessage = e => {
        try {
          const data = JSON.parse(e.data);
          if (data.type === 'plan-info') {
            planInfo = { ...planInfo, ...data };
            updatePlanUI();
          }
          if (data.type === 'file-queued') {
            const tab = document.getElementById('tab-download');
            if (tab) tab.textContent = '📤 Du PC 🔴';
            const panel = document.getElementById('panel-download');
            if (panel && panel.style.display !== 'none') refreshDownloads();
          }
        } catch {}
      };
      ws.onclose = () => setTimeout(connectWS, 2000);
    }
    connectWS();

    function sendProgress(filename, percent) {
      if (ws && ws.readyState === WebSocket.OPEN)
        ws.send(JSON.stringify({ type: 'progress', filename, percent }));
    }

    function updatePlanUI() {
      const isPro = planInfo.plan !== 'Gratuit';
      const badge = document.getElementById('connectedBadge');
      if (badge) {
        badge.textContent = isPro
          ? '⚡ ' + planInfo.plan + ' — Connecté'
          : '✅ Connecté — Plan Gratuit';
      }

      // Onglets bidirectionnels : Pro seulement
      const tabs = document.getElementById('tabs');
      if (tabs) tabs.style.display = isPro ? 'flex' : 'none';

      // Compteur journalier : gratuit seulement
      const counter = document.getElementById('dailyCounter');
      const proBanner = document.getElementById('proBanner');
      if (!isPro && planInfo.uploads_limit) {
        const left = planInfo.uploads_left ?? 10;
        const max  = planInfo.uploads_limit ?? 10;
        const used = max - left;

        counter.style.display = 'flex';
        document.getElementById('counterLeft').textContent = left;
        document.getElementById('counterMax').textContent  = max;

        const bar = document.getElementById('counterBar');
        const pct = (used / max) * 100;
        bar.style.width = pct + '%';
        bar.className = 'counter-bar' + (pct >= 100 ? ' full' : pct >= 70 ? ' warn' : '');

        // Bloque si limite atteinte
        if (left <= 0) {
          proBanner.style.display = 'block';
          const dz = document.getElementById('dropZone');
          dz.classList.add('disabled');
          dz.onclick = null;
        } else {
          proBanner.style.display = 'none';
        }
      } else {
        counter.style.display  = 'none';
        proBanner.style.display = 'none';
      }
    }

    // ── PIN ──
    const digits  = [0,1,2,3].map(i => document.getElementById('p'+i));
    const pinBtn   = document.getElementById('pinBtn');
    const pinError = document.getElementById('pinError');

    function refreshPinBtn() {
      const ok = digits.every(d => d.value.replace(/\D/g,'').length === 1);
      pinBtn.className = ok ? 'pin-btn active' : 'pin-btn';
    }

    digits.forEach((el, i) => {
      function handleChange() {
        const v = el.value.replace(/\D/g,'').slice(-1);
        el.value = v;
        if (v) { el.classList.add('filled'); if (i < 3) digits[i+1].focus(); }
        else el.classList.remove('filled');
        refreshPinBtn();
      }
      ['input','keyup','change'].forEach(evt => el.addEventListener(evt, handleChange));
      el.addEventListener('keydown', e => {
        if (e.key === 'Backspace' && !el.value && i > 0) {
          digits[i-1].value = ''; digits[i-1].classList.remove('filled'); digits[i-1].focus(); refreshPinBtn();
        }
      });
      el.addEventListener('focus', () => setTimeout(() => el.select(), 50));
    });

    async function submitPin() {
      const pin = digits.map(d => d.value.replace(/\D/g,'')).join('');
      if (pin.length !== 4) return;
      pinBtn.disabled = true; pinBtn.textContent = '⏳ Vérification...';
      pinError.style.display = 'none';
      try {
        const res  = await fetch('/verify-pin', {
          method:'POST', headers:{'Content-Type':'application/json'}, body: JSON.stringify({pin})
        });
        const data = await res.json();
        if (data.success) {
          sessionToken = data.token;
          // Récupère les infos du plan depuis la réponse
          planInfo = {
            plan:          data.plan || 'Gratuit',
            bidirectional: data.bidirectional || false,
            uploads_left:  data.uploads_left,
            uploads_limit: data.uploads_limit,
          };
          document.getElementById('pin-screen').style.display  = 'none';
          document.getElementById('upload-screen').style.display = 'block';
          updatePlanUI();
        } else throw new Error();
      } catch {
        digits.forEach(d => { d.classList.add('error'); d.value = ''; d.classList.remove('filled'); });
        setTimeout(() => digits.forEach(d => d.classList.remove('error')), 500);
        pinError.style.display = 'block';
        pinBtn.disabled = false; pinBtn.textContent = 'Valider'; pinBtn.className = 'pin-btn';
        digits[0].focus();
      }
    }

    // ── Onglets ──
    function switchTab(tab) {
      const isReceive = tab === 'receive';
      document.getElementById('panel-receive').style.display  = isReceive ? 'block' : 'none';
      document.getElementById('panel-download').style.display = isReceive ? 'none'  : 'block';
      document.getElementById('tab-receive').className  = isReceive ? 'tab-btn active' : 'tab-btn inactive';
      document.getElementById('tab-download').className = isReceive ? 'tab-btn inactive' : 'tab-btn active';
      if (!isReceive) refreshDownloads();
    }

    // ── Downloads PC → Téléphone (Pro) ──
    async function refreshDownloads() {
      try {
        const res  = await fetch('/files-to-send?token=' + sessionToken);
        if (!res.ok) throw new Error();
        const data = await res.json();
        if (data.pro_required) {
          document.getElementById('dl-list').innerHTML = '';
          document.getElementById('dl-empty').innerHTML = `
            <div style="font-size:40px;margin-bottom:12px">⚡</div>
            <p style="font-weight:700;color:#f1f5f9">Fonctionnalité Pro</p>
            <p style="font-size:12px;margin-top:4px">Le transfert PC → Téléphone est réservé aux abonnés Pro.</p>
            <a href="https://transferbridge.site/checkout.html" target="_blank" style="display:inline-block;margin-top:12px;padding:8px 16px;background:#3b82f6;color:white;border-radius:8px;font-size:12px;font-weight:600;text-decoration:none">⚡ Passer à Pro</a>
          `;
          document.getElementById('dl-empty').style.display = 'block';
          return;
        }
        renderDownloads(data.files || []);
      } catch { renderDownloads([]); }
    }

    function renderDownloads(files) {
      const empty = document.getElementById('dl-empty');
      const list  = document.getElementById('dl-list');
      if (!files || !files.length) {
        empty.innerHTML = `<div style="font-size:40px;margin-bottom:12px">📭</div><p>Aucun fichier en attente</p><p style="font-size:12px;margin-top:4px;color:#475569">Envoie des fichiers depuis l'app PC</p>`;
        empty.style.display='block'; list.innerHTML=''; return;
      }
      empty.style.display = 'none';
      list.innerHTML = files.map(f => `
        <div class="dl-item">
          <span style="font-size:24px">${getIcon(f.name)}</span>
          <div style="flex:1;min-width:0">
            <div style="font-size:14px;font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis">${f.name}</div>
            <div style="font-size:11px;color:#64748b;margin-top:2px">${fmtSize(f.size)}</div>
          </div>
          <a href="/send/${f.id}?token=${sessionToken}" download="${f.name}" class="dl-btn">⬇️</a>
        </div>`).join('');
    }

    // ── Upload Téléphone → PC ──
    const dropZone = document.getElementById('dropZone');
    const fileInput = document.getElementById('fileInput');
    const fileList  = document.getElementById('fileList');
    const sendBtn   = document.getElementById('sendBtn');
    const result    = document.getElementById('result');

    dropZone.addEventListener('click', () => {
      if (dropZone.classList.contains('disabled')) return;
      fileInput.click();
    });
    fileInput.addEventListener('change', e => addFiles(e.target.files));
    dropZone.addEventListener('dragover', e => { e.preventDefault(); if(!dropZone.classList.contains('disabled')) dropZone.classList.add('drag-over'); });
    dropZone.addEventListener('dragleave', () => dropZone.classList.remove('drag-over'));
    dropZone.addEventListener('drop', e => {
      e.preventDefault(); dropZone.classList.remove('drag-over');
      if (!dropZone.classList.contains('disabled')) addFiles(e.dataTransfer.files);
    });

    function getIcon(n) {
      const e = n.split('.').pop().toLowerCase();
      if (['jpg','jpeg','png','gif','webp','heic'].includes(e)) return '🖼️';
      if (['mp4','mov','avi','mkv'].includes(e)) return '🎬';
      if (e==='pdf') return '📄'; if (['zip','rar','7z'].includes(e)) return '🗜️';
      if (['mp3','wav','aac'].includes(e)) return '🎵'; return '📎';
    }
    function fmtSize(b) {
      if (b<1024) return b+' o'; if (b<1048576) return (b/1024).toFixed(1)+' Ko';
      return (b/1048576).toFixed(1)+' Mo';
    }
    function addFiles(files) {
      // Vérifie la limite
      const left = planInfo.uploads_left;
      if (left !== null && left !== undefined && left <= 0) {
        document.getElementById('proBanner').style.display = 'block';
        return;
      }
      for (const f of files) selectedFiles.push(f);
      render(); sendBtn.style.display = selectedFiles.length ? 'block' : 'none'; result.style.display='none';
    }
    function render() {
      fileList.innerHTML = selectedFiles.map((f,i) => `
        <div class="file-item">
          <div class="file-icon">${getIcon(f.name)}</div>
          <div class="file-info">
            <div class="file-name">${f.name}</div>
            <div class="file-size-t">${fmtSize(f.size)}</div>
            <div class="progress-bar"><div class="progress-fill" id="pf-${i}"></div></div>
            <div class="progress-label"><span id="pl-${i}">En attente</span><span id="pp-${i}">0%</span></div>
          </div>
          <div class="file-status" id="st-${i}">⏳</div>
        </div>`).join('');
    }

    function uploadFile(file, index) {
      return new Promise((resolve, reject) => {
        const fd = new FormData(); fd.append('token', sessionToken); fd.append('file', file, file.name);
        const xhr = new XMLHttpRequest();
        xhr.upload.addEventListener('progress', e => {
          if (!e.lengthComputable) return;
          const pct = Math.round((e.loaded/e.total)*100);
          const fill=document.getElementById('pf-'+index), label=document.getElementById('pl-'+index), pctEl=document.getElementById('pp-'+index);
          if(fill) fill.style.width=pct+'%'; if(label) label.textContent=pct<100?'Envoi...':'Finalisation...'; if(pctEl) pctEl.textContent=pct+'%';
          sendProgress(file.name, pct);
        });
        xhr.addEventListener('load', () => {
          if (xhr.status>=200 && xhr.status<300) {
            const fill=document.getElementById('pf-'+index), label=document.getElementById('pl-'+index);
            const pctEl=document.getElementById('pp-'+index), status=document.getElementById('st-'+index);
            if(fill){fill.style.width='100%';fill.classList.add('done');} if(label) label.textContent='Envoyé !';
            if(pctEl) pctEl.textContent='100%'; if(status) status.textContent='✅';
            // Met à jour le compteur
            if (planInfo.uploads_left !== null && planInfo.uploads_left !== undefined) {
              planInfo.uploads_left = Math.max(0, planInfo.uploads_left - 1);
              updatePlanUI();
            }
            resolve();
          } else if (xhr.status===429) {
            // Limite journalière
            result.style.display='block';
            result.className='result limit';
            result.textContent='⏳ Limite de 10 envois/jour atteinte. Revenez demain ou passez à Pro !';
            if (planInfo.uploads_left !== null) planInfo.uploads_left = 0;
            updatePlanUI();
            reject(new Error('Limite atteinte'));
          } else if (xhr.status===413) {
            const fill=document.getElementById('pf-'+index), label=document.getElementById('pl-'+index), status=document.getElementById('st-'+index);
            if(fill) fill.classList.add('error'); if(label) label.textContent='❌ Trop lourd'; if(status) status.textContent='❌';
            reject(new Error('Trop lourd'));
          } else if (xhr.status===401) {
            document.getElementById('upload-screen').style.display='none';
            document.getElementById('pin-screen').style.display='block';
            pinError.textContent='⚠️ Session expirée. Saisis le nouveau PIN.'; pinError.style.display='block';
            reject(new Error('Session expirée'));
          } else reject(new Error('Erreur serveur'));
        });
        xhr.addEventListener('error', () => {
          const fill=document.getElementById('pf-'+index), status=document.getElementById('st-'+index);
          if(fill) fill.classList.add('error'); if(status) status.textContent='❌'; reject(new Error('Erreur réseau'));
        });
        xhr.open('POST', '/upload'); xhr.send(fd);
      });
    }

    sendBtn.addEventListener('click', async () => {
      sendBtn.disabled=true; sendBtn.textContent='⏳ Envoi en cours...';
      let allOk=true;
      for (let i=0; i<selectedFiles.length; i++) { try { await uploadFile(selectedFiles[i],i); } catch { allOk=false; } }
      result.style.display='block';
      if (allOk) {
        result.className='result ok'; result.textContent='✅ Tous les fichiers ont été envoyés !';
        selectedFiles=[];
        setTimeout(()=>{ render(); sendBtn.style.display='none'; sendBtn.disabled=false; sendBtn.textContent='🚀 Envoyer sur le PC'; }, 2000);
      } else if (result.className !== 'result limit') {
        result.className='result err'; result.textContent="❌ Certains fichiers ont échoué.";
        sendBtn.disabled=false; sendBtn.textContent='🔄 Réessayer';
      }
    });
  </script>
</body>
</html>"#;

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

                *tunnel_url_clone.lock().unwrap() = Some(url.clone());
                *tunnel_active_clone.lock().unwrap() = true;

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
        *tunnel_active_clone.lock().unwrap() = false;
        *tunnel_url_clone.lock().unwrap() = None;
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
    let active = *global.tunnel_active.lock().unwrap();
    let url    = global.tunnel_url.lock().unwrap().clone();

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
        let mut process = global.tunnel_process.lock().unwrap();
        process.take()
    };

    if let Some(mut child) = child_opt {
        let _ = child.kill().await;
        println!("☁️  Tunnel Cloudflare arrêté manuellement");
    }

    *global.tunnel_active.lock().unwrap() = false;
    *global.tunnel_url.lock().unwrap() = None;
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
        let mut process = global.tunnel_process.lock().unwrap();
        process.take()
    };

    if let Some(mut child) = child_opt {
        let _ = child.kill().await;
    }

    *global.tunnel_active.lock().unwrap() = false;
    *global.tunnel_url.lock().unwrap() = None;

    // Relance — clone tout ce dont on a besoin avant le .await
    let plan = global.plan.lock().unwrap().clone();
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

        *global.tunnel_process.lock().unwrap() = Some(child);
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