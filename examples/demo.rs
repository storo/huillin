//! Sesión aislada para grabar la interfaz sin mostrar las descargas personales.
//! cargo run --example demo
#[path = "../src/bencode.rs"]
mod bencode;
#[path = "../src/clipboard.rs"]
mod clipboard;
#[path = "../src/dht.rs"]
mod dht;
#[path = "../src/meta.rs"]
mod meta;
#[path = "../src/mse.rs"]
mod mse;
#[path = "../src/network.rs"]
mod network;
#[path = "../src/ratelimit.rs"]
mod ratelimit;
#[path = "../src/session.rs"]
mod session;
#[path = "../src/storage.rs"]
mod storage;
#[path = "../src/torrent.rs"]
mod torrent;
#[path = "../src/tracker.rs"]
mod tracker;
#[path = "../src/updates.rs"]
mod updates;
#[path = "../src/web.rs"]
mod web;

#[tokio::main]
async fn main() {
    let root = std::env::current_dir().unwrap().join("dist/demo");
    std::fs::create_dir_all(root.join("RTorrent")).unwrap();
    // Estas variables solo afectan a este proceso de ejemplo.
    std::env::set_var("APPDATA", &root);
    std::env::set_var("XDG_DATA_HOME", &root);
    if cfg!(target_os = "macos") {
        std::env::set_var("HOME", &root);
    }
    let session = session::Session::new().await;
    *session.default_dir.lock().unwrap() = root.join("Downloads");
    session
        .clipboard
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:8120")
        .await
        .unwrap();
    println!("Demo aislada: http://127.0.0.1:8120");
    axum::serve(listener, web::router(session)).await.unwrap();
}
