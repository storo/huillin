mod bencode;
mod clipboard;
mod dht;
mod meta;
mod mse;
mod ratelimit;
mod session;
mod storage;
mod torrent;
mod tracker;
mod web;

#[tokio::main]
async fn main() {
    let session = session::Session::new().await;

    // Argumentos opcionales: archivos .torrent o enlaces magnet
    for arg in std::env::args().skip(1) {
        let r = if arg.starts_with("magnet:") {
            session.add_magnet(&arg, None)
        } else {
            std::fs::read(&arg).map_err(|e| e.to_string()).and_then(|d| session.add_file(&d, None))
        };
        if let Err(e) = r {
            eprintln!("{arg}: {e}");
        }
    }

    let app = web::router(session.clone());
    let mut listener = None;
    for port in 8088..8120u16 {
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            listener = Some(l);
            break;
        }
    }
    let Some(listener) = listener else {
        eprintln!("No hay puertos libres para el panel web");
        return;
    };
    let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    println!("Huillín en {url}  (puerto BitTorrent {})", session.port());
    println!("Ctrl+C para salir");
    if std::env::var("HUILLIN_NO_BROWSER").is_err() && std::env::var("RTORRENT_NO_BROWSER").is_err() {
        let _ = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
    }

    let s2 = session.clone();
    let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            s2.save();
        })
        .await;
}
