use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Multipart, State,
    },
    http::{Method, StatusCode},
    response::{Html, Response},
    routing::{get, post},
    Json, Router,
};
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tower_http::cors::{Any, CorsLayer};

// ─── État partagé ─────────────────────────────────────────────────
struct GlobalState {
    pin:           Arc<Mutex<String>>,
    sessions:      Arc<Mutex<Vec<Session>>>,
    save_dir:      Arc<Mutex<PathBuf>>,
    started:       Mutex<bool>,
    max_file_size: Arc<Mutex<u64>>,
}

#[derive(Clone)]
struct AppState {
    pin:           Arc<Mutex<String>>,
    sessions:      Arc<Mutex<Vec<Session>>>,
    save_dir:      Arc<Mutex<PathBuf>>,
    app_handle:    AppHandle,
    max_file_size: Arc<Mutex<u64>>,
}

#[derive(Clone)]
struct Session {
    token:      String,
    expires_at: Instant,
}

// ─── Commandes Tauri ──────────────────────────────────────────────

#[tauri::command]
async fn start_server(
    app:    AppHandle,
    global: tauri::State<'_, GlobalState>,
) -> Result<String, String> {
    {
        let already = *global.started.lock().unwrap();
        if already {
            let pin = generate_pin();
            *global.pin.lock().unwrap() = pin.clone();
            global.sessions.lock().unwrap().clear();
            let _ = app.emit("pin-generated", pin);
            let ip = get_local_ip();
            return Ok(format!("http://{}:3030", ip));
        }
    }

    let save_dir = app.path().download_dir()
        .unwrap_or_else(|_| PathBuf::from("."));
    *global.save_dir.lock().unwrap() = save_dir;

    let pin = generate_pin();
    *global.pin.lock().unwrap() = pin.clone();

    let state = AppState {
        pin:           Arc::clone(&global.pin),
        sessions:      Arc::clone(&global.sessions),
        save_dir:      Arc::clone(&global.save_dir),
        app_handle:    app.clone(),
        max_file_size: Arc::clone(&global.max_file_size),
    };

    let ip = get_local_ip();
    let port = 3030u16;
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    let cors = CorsLayer::new()
        .allow_methods([Method::GET, Method::POST])
        .allow_origin(Any)
        .allow_headers(Any);

    let router = Router::new()
        .route("/",           get(serve_mobile_ui))
        .route("/ping",       get(|| async { "pong" }))
        .route("/verify-pin", post(verify_pin))
        .route("/upload",     post(handle_upload))
        .route("/ws",         get(ws_handler))
        .with_state(state)
        .layer(cors);

    *global.started.lock().unwrap() = true;

    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::bind(addr).await
            .expect("Impossible de démarrer le serveur");
        axum::serve(listener, router).await.unwrap();
    });

    let _ = app.emit("pin-generated", pin.clone());
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
    println!("🔐 Nouveau PIN : {}", new_pin);
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

// ─── WebSocket ────────────────────────────────────────────────────

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(mut socket: WebSocket, state: AppState) {
    let _ = socket.send(Message::Text(
        serde_json::json!({ "type": "connected" }).to_string().into()
    )).await;
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

    Ok(Json(serde_json::json!({ "success": true, "token": token })))
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
        .map_err(|e: axum::extract::multipart::MultipartError| {
            (StatusCode::BAD_REQUEST, e.to_string())
        })?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "token" {
            token = field.text().await
                .map_err(|e: axum::extract::multipart::MultipartError| {
                    (StatusCode::BAD_REQUEST, e.to_string())
                })?;
        } else {
            let filename = field.file_name().unwrap_or("fichier").to_string();
            let data = field.bytes().await
                .map_err(|e: axum::extract::multipart::MultipartError| {
                    (StatusCode::BAD_REQUEST, e.to_string())
                })?;

            if max_size > 0 && data.len() as u64 > max_size {
                let msg = format!(
                    "❌ '{}' dépasse la limite ({:.0}MB)",
                    filename, max_size as f64 / 1_048_576.0
                );
                let _ = state.app_handle.emit("upload-error", serde_json::json!({
                    "filename": filename,
                    "error": "too_large",
                    "message": msg.clone()
                }));
                return Err((StatusCode::PAYLOAD_TOO_LARGE, msg));
            }

            files_data.push((filename, data.to_vec()));
        }
    }

    if !is_valid_session(&state.sessions, &token) {
        let _ = state.app_handle.emit("upload-error", serde_json::json!({
            "error": "session_expired",
            "message": "Session expirée — reconnecte-toi"
        }));
        return Err((StatusCode::UNAUTHORIZED,
            serde_json::json!({ "error": "session_expired" }).to_string()));
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
            "name":  filename,
            "size":  file_size,
            "path":  save_path.to_string_lossy()
        }));
    }

    Ok("✅ Fichiers reçus".to_string())
}

// ─── Utilitaires ──────────────────────────────────────────────────

fn generate_pin() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH).unwrap()
        .subsec_nanos();
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
        5 => "⭐⭐⭐⭐⭐", 4 => "⭐⭐⭐⭐",
        3 => "⭐⭐⭐",     2 => "⭐⭐", _ => "⭐",
    };
    let category_emoji = match payload.category.as_str() {
        "bug"         => "🐛 Bug",
        "feature"     => "💡 Idée",
        "performance" => "⚡ Performance",
        "ux"          => "🎨 UX/Design",
        _             => "💬 Général",
    };
    let email_str = payload.email
        .filter(|e| !e.is_empty())
        .map(|e| format!("`{}`", e))
        .unwrap_or_else(|| "*Anonyme*".to_string());

    let discord_msg = serde_json::json!({
        "embeds": [{
            "title": format!("{} Nouveau feedback TransferBridge", stars),
            "color": match payload.rating {
                5 => 0x22C55E, 4 => 0x3B82F6,
                3 => 0xF59E0B, 2 => 0xF97316, _ => 0xEF4444
            },
            "fields": [
                { "name": "⭐ Note",       "value": format!("{}/5 {}", payload.rating, stars), "inline": true },
                { "name": "🏷️ Catégorie", "value": category_emoji,    "inline": true },
                { "name": "💻 OS",         "value": &payload.os,       "inline": true },
                { "name": "📦 Version",    "value": &payload.app_version, "inline": true },
                { "name": "📧 Email",      "value": email_str,         "inline": true },
                { "name": "💬 Message",    "value": &payload.message,  "inline": false },
            ],
            "footer":    { "text": "TransferBridge Feedback System" },
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
        update.download_and_install(
            |_chunk, _total| {},
            || {},
        ).await.map_err(|e| e.to_string())?;
    }
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
    #pin-screen { width: 100%; max-width: 340px; }
    .pin-label { font-size: 15px; color: #94a3b8; text-align: center; margin-bottom: 20px; }
    .pin-inputs { display: flex; justify-content: center; gap: 12px; margin-bottom: 24px; }
    .pin-digit {
      width: 60px; height: 68px; background: #1e293b;
      border: 2px solid #334155; border-radius: 12px;
      font-size: 28px; font-weight: 700; color: #f1f5f9;
      text-align: center; outline: none; transition: border-color 0.2s;
      -webkit-appearance: none; appearance: none; caret-color: transparent;
    }
    .pin-digit:focus { border-color: #3b82f6; }
    .pin-digit.filled { border-color: #3b82f6; background: #1e3a5f; }
    .pin-digit.error { border-color: #ef4444; animation: shake 0.3s; }
    @keyframes shake { 0%,100%{transform:translateX(0)} 25%{transform:translateX(-6px)} 75%{transform:translateX(6px)} }
    .pin-btn {
      width: 100%; padding: 18px; background: #334155;
      color: white; border: none; border-radius: 14px;
      font-size: 17px; font-weight: 600; cursor: pointer;
      transition: background 0.2s; -webkit-appearance: none;
    }
    .pin-btn.active { background: #3b82f6; }
    .pin-btn:active { opacity: 0.85; }
    .pin-error { margin-top: 14px; padding: 12px; background: #450a0a; border-radius: 10px; color: #fca5a5; font-size: 13px; text-align: center; display: none; }
    #upload-screen { width: 100%; max-width: 400px; display: none; }
    .connected-badge { background: #14532d; color: #86efac; padding: 10px 16px; border-radius: 20px; font-size: 13px; font-weight: 600; text-align: center; margin-bottom: 20px; }
    .drop-zone { width: 100%; border: 2px dashed #334155; border-radius: 16px; padding: 40px 24px; text-align: center; cursor: pointer; transition: all 0.2s; background: #1e293b; }
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
    .file-size { font-size: 11px; color: #64748b; margin-bottom: 4px; }
    .progress-bar { width: 100%; height: 6px; background: #334155; border-radius: 3px; overflow: hidden; }
    .progress-fill { height: 100%; border-radius: 3px; transition: width 0.15s ease; width: 0%; background: linear-gradient(90deg, #3b82f6, #06b6d4); }
    .progress-fill.done { background: #22c55e; }
    .progress-fill.error { background: #ef4444; }
    .progress-label { font-size: 11px; color: #64748b; margin-top: 3px; display: flex; justify-content: space-between; }
    .file-status { font-size: 18px; }
    .send-btn { width: 100%; margin-top: 14px; padding: 16px; background: #3b82f6; color: white; border: none; border-radius: 14px; font-size: 16px; font-weight: 600; cursor: pointer; display: none; -webkit-appearance: none; }
    .send-btn:disabled { background: #334155; cursor: not-allowed; }
    .result { margin-top: 12px; padding: 12px; border-radius: 10px; font-size: 13px; text-align: center; display: none; }
    .result.ok { background: #14532d; color: #86efac; }
    .result.err { background: #450a0a; color: #fca5a5; }
  </style>
</head>
<body>
  <div class="logo">📁</div>
  <h1>TransferBridge</h1>
  <p class="subtitle">Transfert sécurisé vers le PC</p>

  <div id="pin-screen">
    <p class="pin-label">🔐 Saisis le code PIN affiché sur le PC</p>
    <div class="pin-inputs" id="pinInputs">
      <input class="pin-digit" id="p0" type="tel" maxlength="1" inputmode="numeric" autocomplete="one-time-code">
      <input class="pin-digit" id="p1" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
      <input class="pin-digit" id="p2" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
      <input class="pin-digit" id="p3" type="tel" maxlength="1" inputmode="numeric" autocomplete="off">
    </div>
    <button class="pin-btn" id="pinBtn" onclick="submitPin()">Valider</button>
    <div class="pin-error" id="pinError">❌ PIN incorrect. Demande un nouveau PIN sur le PC.</div>
  </div>

  <div id="upload-screen">
    <div class="connected-badge">✅ Connecté — session sécurisée</div>
    <div class="drop-zone" id="dropZone">
      <input type="file" id="fileInput" multiple accept="*/*">
      <div class="drop-icon">📂</div>
      <div class="drop-text"><span>Appuie ici</span> pour sélectionner<br>photos, vidéos, PDF, tout type</div>
    </div>
    <div class="file-list" id="fileList"></div>
    <button class="send-btn" id="sendBtn">🚀 Envoyer sur le PC</button>
    <div class="result" id="result"></div>
  </div>

  <script>
    let sessionToken = null, selectedFiles = [], ws = null;

    function connectWS() {
      ws = new WebSocket('ws://' + location.host + '/ws');
      ws.onclose = () => setTimeout(connectWS, 2000);
    }
    connectWS();

    function sendProgress(filename, percent) {
      if (ws && ws.readyState === WebSocket.OPEN)
        ws.send(JSON.stringify({ type: 'progress', filename, percent }));
    }

    const digits = [0,1,2,3].map(i => document.getElementById('p'+i));
    const pinBtn = document.getElementById('pinBtn');
    const pinError = document.getElementById('pinError');

    function refreshPinBtn() {
      const ok = digits.every(d => d.value.replace(/\D/g,'').length === 1);
      pinBtn.className = ok ? 'pin-btn active' : 'pin-btn';
    }

    digits.forEach((el, i) => {
      function handleChange() {
        const v = el.value.replace(/\D/g,'').slice(-1);
        el.value = v;
        if (v) {
          el.classList.add('filled');
          if (i < 3) digits[i+1].focus();
        } else {
          el.classList.remove('filled');
        }
        refreshPinBtn();
      }
      el.addEventListener('input',  handleChange);
      el.addEventListener('keyup',  handleChange);
      el.addEventListener('change', handleChange);

      el.addEventListener('keydown', e => {
        if (e.key === 'Backspace' && !el.value && i > 0) {
          digits[i-1].value = '';
          digits[i-1].classList.remove('filled');
          digits[i-1].focus();
          refreshPinBtn();
        }
        if (!/[\d]/.test(e.key) && !['Backspace','Tab','ArrowLeft','ArrowRight'].includes(e.key)) {
          e.preventDefault();
        }
      });

      el.addEventListener('focus', () => setTimeout(() => el.select(), 50));
      el.addEventListener('click', () => {
        const first = digits.find(d => !d.value);
        if (first) first.focus();
      });
    });

    document.getElementById('pin-screen').addEventListener('touchstart', e => {
      if (e.target.classList.contains('pin-digit') || e.target.id === 'pinBtn') return;
      const first = digits.find(d => !d.value) || digits[3];
      first.focus();
    }, { passive: true });

    async function submitPin() {
      const pin = digits.map(d => d.value.replace(/\D/g,'')).join('');
      if (pin.length !== 4) return;
      pinBtn.disabled = true;
      pinBtn.textContent = '⏳ Vérification...';
      pinError.style.display = 'none';
      try {
        const res = await fetch('/verify-pin', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ pin })
        });
        const data = await res.json();
        if (data.success) {
          sessionToken = data.token;
          document.getElementById('pin-screen').style.display = 'none';
          document.getElementById('upload-screen').style.display = 'block';
        } else throw new Error();
      } catch {
        digits.forEach(d => { d.classList.add('error'); d.value = ''; d.classList.remove('filled'); });
        setTimeout(() => digits.forEach(d => d.classList.remove('error')), 500);
        pinError.style.display = 'block';
        pinBtn.disabled = false;
        pinBtn.textContent = 'Valider';
        pinBtn.className = 'pin-btn';
        digits[0].focus();
      }
    }

    const dropZone = document.getElementById('dropZone');
    const fileInput = document.getElementById('fileInput');
    const fileList  = document.getElementById('fileList');
    const sendBtn   = document.getElementById('sendBtn');
    const result    = document.getElementById('result');

    dropZone.addEventListener('click', () => fileInput.click());
    fileInput.addEventListener('change', e => addFiles(e.target.files));
    dropZone.addEventListener('dragover', e => { e.preventDefault(); dropZone.classList.add('drag-over'); });
    dropZone.addEventListener('dragleave', () => dropZone.classList.remove('drag-over'));
    dropZone.addEventListener('drop', e => { e.preventDefault(); dropZone.classList.remove('drag-over'); addFiles(e.dataTransfer.files); });

    function getIcon(n) {
      const e = n.split('.').pop().toLowerCase();
      if (['jpg','jpeg','png','gif','webp','heic'].includes(e)) return '🖼️';
      if (['mp4','mov','avi','mkv'].includes(e)) return '🎬';
      if (e === 'pdf') return '📄';
      if (['zip','rar','7z'].includes(e)) return '🗜️';
      if (['mp3','wav','aac'].includes(e)) return '🎵';
      return '📎';
    }

    function fmtSize(b) {
      if (b < 1024) return b + ' o';
      if (b < 1048576) return (b/1024).toFixed(1) + ' Ko';
      return (b/1048576).toFixed(1) + ' Mo';
    }

    function addFiles(files) {
      for (const f of files) selectedFiles.push(f);
      render();
      sendBtn.style.display = selectedFiles.length ? 'block' : 'none';
      result.style.display = 'none';
    }

    function render() {
      fileList.innerHTML = selectedFiles.map((f,i) => `
        <div class="file-item">
          <div class="file-icon">${getIcon(f.name)}</div>
          <div class="file-info">
            <div class="file-name">${f.name}</div>
            <div class="file-size">${fmtSize(f.size)}</div>
            <div class="progress-bar"><div class="progress-fill" id="pf-${i}"></div></div>
            <div class="progress-label"><span id="pl-${i}">En attente</span><span id="pp-${i}">0%</span></div>
          </div>
          <div class="file-status" id="st-${i}">⏳</div>
        </div>`).join('');
    }

    function uploadFile(file, index) {
      return new Promise((resolve, reject) => {
        const fd = new FormData();
        fd.append('token', sessionToken);
        fd.append('file', file, file.name);
        const xhr = new XMLHttpRequest();
        xhr.upload.addEventListener('progress', e => {
          if (e.lengthComputable) {
            const pct = Math.round((e.loaded / e.total) * 100);
            const fill  = document.getElementById('pf-' + index);
            const label = document.getElementById('pl-' + index);
            const pctEl = document.getElementById('pp-' + index);
            if (fill)  fill.style.width = pct + '%';
            if (label) label.textContent = pct < 100 ? 'Envoi...' : 'Finalisation...';
            if (pctEl) pctEl.textContent = pct + '%';
            sendProgress(file.name, pct);
          }
        });
        xhr.addEventListener('load', () => {
          if (xhr.status >= 200 && xhr.status < 300) {
            const fill   = document.getElementById('pf-' + index);
            const label  = document.getElementById('pl-' + index);
            const pctEl  = document.getElementById('pp-' + index);
            const status = document.getElementById('st-' + index);
            if (fill)   { fill.style.width = '100%'; fill.classList.add('done'); }
            if (label)  label.textContent = 'Envoyé !';
            if (pctEl)  pctEl.textContent = '100%';
            if (status) status.textContent = '✅';
            resolve();
          } else if (xhr.status === 413) {
            const fill   = document.getElementById('pf-' + index);
            const label  = document.getElementById('pl-' + index);
            const status = document.getElementById('st-' + index);
            if (fill)   fill.classList.add('error');
            if (label)  label.textContent = '❌ Fichier trop lourd';
            if (status) status.textContent = '❌';
            reject(new Error('Trop lourd'));
          } else if (xhr.status === 401) {
            document.getElementById('upload-screen').style.display = 'none';
            document.getElementById('pin-screen').style.display = 'block';
            pinError.textContent = '⚠️ Session expirée. Saisis le nouveau PIN.';
            pinError.style.display = 'block';
            reject(new Error('Session expirée'));
          } else {
            reject(new Error('Erreur serveur'));
          }
        });
        xhr.addEventListener('error', () => {
          const fill   = document.getElementById('pf-' + index);
          const status = document.getElementById('st-' + index);
          if (fill)   fill.classList.add('error');
          if (status) status.textContent = '❌';
          reject(new Error('Erreur réseau'));
        });
        xhr.open('POST', '/upload');
        xhr.send(fd);
      });
    }

    sendBtn.addEventListener('click', async () => {
      sendBtn.disabled = true;
      sendBtn.textContent = '⏳ Envoi en cours...';
      let allOk = true;
      for (let i = 0; i < selectedFiles.length; i++) {
        try { await uploadFile(selectedFiles[i], i); }
        catch { allOk = false; }
      }
      result.style.display = 'block';
      if (allOk) {
        result.className = 'result ok';
        result.textContent = '✅ Tous les fichiers ont été envoyés !';
        selectedFiles = [];
        setTimeout(() => {
          render();
          sendBtn.style.display = 'none';
          sendBtn.disabled = false;
          sendBtn.textContent = '🚀 Envoyer sur le PC';
        }, 2000);
      } else {
        result.className = 'result err';
        result.textContent = "❌ Certains fichiers ont échoué.";
        sendBtn.disabled = false;
        sendBtn.textContent = '🔄 Réessayer';
      }
    });
  </script>
</body>
</html>"#;

// ─── Point d'entrée ───────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(GlobalState {
            pin:           Arc::new(Mutex::new(String::new())),
            sessions:      Arc::new(Mutex::new(vec![])),
            save_dir:      Arc::new(Mutex::new(PathBuf::from("."))),
            started:       Mutex::new(false),
            max_file_size: Arc::new(Mutex::new(500 * 1024 * 1024)),
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
            install_update
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}