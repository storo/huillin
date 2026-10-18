//! API HTTP local + panel web.
use crate::session::Session;
use axum::{
    body::Bytes,
    extract::{Path, Query, Request, State},
    http::{header, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

type Res = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn bad(msg: impl ToString) -> (StatusCode, Json<Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({ "error": msg.to_string() })))
}

/// Protege la API local frente a páginas web ajenas (CSRF y DNS rebinding):
/// solo se acepta Host local y, en peticiones que modifican, una cabecera propia
/// que el navegador no permite enviar entre orígenes sin CORS.
async fn guard(req: Request, next: Next) -> Response {
    let host_ok = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map_or(false, |h| h.starts_with("127.0.0.1:") || h.starts_with("localhost:"));
    let method_ok = req.method() == Method::GET || req.headers().contains_key("x-rt");
    if !host_ok || !method_ok {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(req).await
}

pub fn router(s: Arc<Session>) -> Router {
    Router::new()
        .route("/", get(|| async { Html(include_str!("../ui/index.html")) }))
        .route("/logo.png", get(|| png(include_bytes!("../ui/logo.png"))))
        .route("/favicon.png", get(|| png(include_bytes!("../ui/favicon.png"))))
        .route("/api/torrents", get(list))
        .route("/api/magnet", post(magnet))
        .route("/api/add", post(add_file))
        .route("/api/settings", post(settings))
        .route("/api/updates/check", post(check_updates))
        .route("/api/updates/open", post(open_updates))
        .route("/api/t/{id}/files", get(files_get).post(files_set))
        .route("/api/t/{id}/{action}", post(action))
        .layer(axum::extract::DefaultBodyLimit::max(32 << 20))
        .layer(middleware::from_fn(guard))
        .with_state(s)
}

async fn png(bytes: &'static [u8]) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "public, max-age=86400")], bytes)
}

async fn list(State(s): State<Arc<Session>>) -> Json<Value> {
    let torrents = s.list();
    let down: f64 = torrents.iter().filter_map(|t| t["down"].as_f64()).sum();
    let up: f64 = torrents.iter().filter_map(|t| t["up"].as_f64()).sum();
    let clip = s.clip_event.lock().unwrap().clone();
    Json(json!({
        "version": crate::updates::CURRENT_VERSION,
        "torrents": torrents, "down": down, "up": up, "port": s.port(),
        "default_dir": s.default_dir.lock().unwrap().to_string_lossy(),
        "down_kbs": s.ctx.down.get() / 1024, "up_kbs": s.ctx.up.get() / 1024,
        "encryption": s.ctx.encryption.load(std::sync::atomic::Ordering::Relaxed),
        "clipboard": s.clipboard.load(std::sync::atomic::Ordering::Relaxed),
        "clip": { "n": clip.0, "name": clip.1 },
    }))
}

async fn check_updates() -> Res {
    crate::updates::check().await
        .map(|info| Json(json!(info)))
        .map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))))
}

async fn open_updates() -> Res {
    crate::updates::open_download_page()
        .map(|_| Json(json!({ "ok": true })))
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": "No se pudo abrir el navegador." }))))
}

#[derive(Deserialize)]
struct MagnetReq {
    uri: String,
    dir: Option<String>,
}

async fn magnet(State(s): State<Arc<Session>>, Json(r): Json<MagnetReq>) -> Res {
    s.add_magnet(&r.uri, r.dir).map(|id| Json(json!({ "id": id }))).map_err(bad)
}

#[derive(Deserialize)]
struct DirQ {
    dir: Option<String>,
}

async fn add_file(State(s): State<Arc<Session>>, Query(q): Query<DirQ>, body: Bytes) -> Res {
    s.add_file(&body, q.dir).map(|id| Json(json!({ "id": id }))).map_err(bad)
}

#[derive(Deserialize)]
struct SettingsReq {
    dir: Option<String>,
    down_kbs: Option<u64>,
    up_kbs: Option<u64>,
    encryption: Option<u8>,
    clipboard: Option<bool>,
}

async fn settings(State(s): State<Arc<Session>>, Json(r): Json<SettingsReq>) -> Res {
    if let Some(dir) = r.dir {
        if dir.trim().is_empty() {
            return Err(bad("carpeta vacía"));
        }
        *s.default_dir.lock().unwrap() = dir.trim().into();
    }
    if matches!(r.encryption, Some(e) if e > 2) {
        return Err(bad("modo de cifrado inválido"));
    }
    if let Some(c) = r.clipboard {
        s.clipboard.store(c, std::sync::atomic::Ordering::Relaxed);
    }
    s.set_limits(r.down_kbs, r.up_kbs, r.encryption);
    Ok(Json(json!({ "ok": true })))
}

async fn files_get(State(s): State<Arc<Session>>, Path(id): Path<String>) -> Res {
    let t = s.find(&id).ok_or_else(|| bad("torrent no encontrado"))?;
    Ok(Json(json!({ "files": t.files() })))
}

#[derive(Deserialize)]
struct FilesReq {
    skip: Vec<usize>,
}

async fn files_set(State(s): State<Arc<Session>>, Path(id): Path<String>, Json(r): Json<FilesReq>) -> Res {
    if s.set_skipped(&id, r.skip.into_iter().collect()) {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(bad("torrent no encontrado"))
    }
}

#[derive(Deserialize)]
struct ActionQ {
    delete: Option<bool>,
}

async fn action(State(s): State<Arc<Session>>, Path((id, action)): Path<(String, String)>, Query(q): Query<ActionQ>) -> Res {
    let ok = match action.as_str() {
        "pause" => s.pause(&id),
        "resume" => s.resume(&id),
        "remove" => s.remove(&id, q.delete.unwrap_or(false)),
        "open" => s.open_folder(&id),
        _ => return Err(bad("acción desconocida")),
    };
    if ok {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(bad("torrent no encontrado"))
    }
}
