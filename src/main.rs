#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

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
mod updates;
mod web;

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::{WebContext, WebViewBuilder};

/// Puerto local para detectar otra instancia y enviarle argumentos.
const IPC_PORT: u16 = 47831;

enum UserEvent {
    Show,
    Quit,
}

fn logo_rgba() -> Option<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(include_bytes!("../ui/logo.png")).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((img.into_raw(), w, h))
}

fn add_arg(session: &session::Session, arg: &str) {
    let r = if arg.starts_with("magnet:") {
        session.add_magnet(arg, None)
    } else {
        std::fs::read(arg).map_err(|e| e.to_string()).and_then(|d| session.add_file(&d, None))
    };
    if let Err(e) = r {
        eprintln!("{arg}: {e}");
    }
}

/// Registra (o quita) .torrent y magnet: para el usuario actual.
#[cfg(target_os = "linux")]
fn set_associations(register: bool) {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = std::path::PathBuf::from(home).join(".local/share/applications");
    let file = dir.join("huillin.desktop");
    let run = |cmd: &str, args: &[&str]| {
        let _ = std::process::Command::new(cmd).args(args).output();
    };
    if register {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        let _ = std::fs::create_dir_all(&dir);
        let entry = format!(
            "[Desktop Entry]\nType=Application\nName=Huillín\nComment=Cliente BitTorrent\nExec=\"{exe}\" %U\nTerminal=false\nCategories=Network;FileTransfer;P2P;\nMimeType=application/x-bittorrent;x-scheme-handler/magnet;\n"
        );
        if std::fs::write(&file, entry).is_ok() {
            run("update-desktop-database", &[&dir.display().to_string()]);
            run("xdg-mime", &["default", "huillin.desktop", "application/x-bittorrent"]);
            run("xdg-mime", &["default", "huillin.desktop", "x-scheme-handler/magnet"]);
        }
    } else {
        let _ = std::fs::remove_file(&file);
        run("update-desktop-database", &[&dir.display().to_string()]);
    }
}

#[cfg(target_os = "macos")]
fn set_associations(_register: bool) {
    eprintln!("En macOS la asociación se configura desde Finder (Obtener información → Abrir con).");
}

#[cfg(windows)]
fn set_associations(register: bool) {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    let reg = |args: &[&str]| {
        let _ = std::process::Command::new("reg").args(args).output();
    };
    if !register {
        for k in [r"HKCU\Software\Classes\.torrent", r"HKCU\Software\Classes\Huillin.torrent", r"HKCU\Software\Classes\magnet"] {
            reg(&["delete", k, "/f"]);
        }
        return;
    }
    let cmd = format!("\"{exe}\" \"%1\"");
    let icon = format!("\"{exe}\",0");
    let c = r"HKCU\Software\Classes";
    reg(&["add", &format!(r"{c}\.torrent"), "/ve", "/d", "Huillin.torrent", "/f"]);
    reg(&["add", &format!(r"{c}\Huillin.torrent"), "/ve", "/d", "Archivo torrent", "/f"]);
    reg(&["add", &format!(r"{c}\Huillin.torrent\DefaultIcon"), "/ve", "/d", &icon, "/f"]);
    reg(&["add", &format!(r"{c}\Huillin.torrent\shell\open\command"), "/ve", "/d", &cmd, "/f"]);
    reg(&["add", &format!(r"{c}\magnet"), "/ve", "/d", "URL:Magnet", "/f"]);
    reg(&["add", &format!(r"{c}\magnet"), "/v", "URL Protocol", "/d", "", "/f"]);
    reg(&["add", &format!(r"{c}\magnet\DefaultIcon"), "/ve", "/d", &icon, "/f"]);
    reg(&["add", &format!(r"{c}\magnet\shell\open\command"), "/ve", "/d", &cmd, "/f"]);
}

fn open_url(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

/// Datos de WebView2 fuera del ejecutable (en macOS/Linux se usa la ubicación por defecto).
fn webview_data_dir() -> Option<std::path::PathBuf> {
    if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA").map(|p| std::path::PathBuf::from(p).join("Huillin").join("webview"))
    } else {
        None
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--register" || a == "--unregister") {
        set_associations(args.iter().any(|a| a == "--register"));
        return;
    }
    let web_mode = args.iter().any(|a| a == "--web") || std::env::var("HUILLIN_WEB").is_ok();
    args.retain(|a| !a.starts_with("--"));

    // Instancia única: si ya hay una, le pasamos los argumentos y salimos.
    let ipc = match TcpListener::bind(("127.0.0.1", IPC_PORT)) {
        Ok(l) => l,
        Err(_) => {
            if let Ok(mut s) = TcpStream::connect(("127.0.0.1", IPC_PORT)) {
                for a in &args {
                    let _ = writeln!(s, "ADD {a}");
                }
                let _ = writeln!(s, "SHOW");
                return;
            }
            eprintln!("Puerto {IPC_PORT} ocupado por otro programa");
            return;
        }
    };

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let _rt_guard = rt.enter();
    let handle = rt.handle().clone();
    let session = rt.block_on(session::Session::new());
    for a in &args {
        add_arg(&session, a);
    }

    let app = web::router(session.clone());
    let listener = rt.block_on(async {
        for port in 8088..8120u16 {
            if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                return Some(l);
            }
        }
        None
    });
    let Some(listener) = listener else {
        eprintln!("No hay puertos libres para el panel web");
        return;
    };
    let url = format!("http://127.0.0.1:{}/", listener.local_addr().unwrap().port());
    println!("Huillín en {url}  (puerto BitTorrent {})", session.port());

    // Modo web (comportamiento anterior): sin ventana, abre el navegador.
    if web_mode {
        println!("Ctrl+C para salir");
        if std::env::var("HUILLIN_NO_BROWSER").is_err() && std::env::var("RTORRENT_NO_BROWSER").is_err() {
            open_url(&url);
        }
        let s_ipc = session.clone();
        let h = handle.clone();
        std::thread::spawn(move || ipc_loop(ipc, s_ipc, None, h));
        let s2 = session.clone();
        rt.block_on(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    s2.save();
                })
                .await;
        });
        return;
    }

    // Modo app: ventana nativa (WebView2) con la misma UI.
    rt.spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let s_ipc = session.clone();
    let p_ipc = proxy.clone();
    let h = handle.clone();
    std::thread::spawn(move || ipc_loop(ipc, s_ipc, Some(p_ipc), h));

    let logo = logo_rgba();
    let window = WindowBuilder::new()
        .with_title("Huillín")
        .with_inner_size(LogicalSize::new(1100.0, 720.0))
        .with_min_inner_size(LogicalSize::new(640.0, 480.0))
        .with_window_icon(logo.as_ref().and_then(|(d, w, h)| Icon::from_rgba(d.clone(), *w, *h).ok()))
        .build(&event_loop)
        .expect("ventana");

    let mut ctx = WebContext::new(webview_data_dir());
    let builder = WebViewBuilder::new_with_web_context(&mut ctx).with_url(&url);
    #[cfg(not(target_os = "linux"))]
    let _webview = builder.build(&window).expect("webview (¿WebView2 instalado?)");
    // En Linux se usa GTK (funciona en X11 y Wayland).
    #[cfg(target_os = "linux")]
    let _webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("gtk vbox")).expect("webview (¿WebKitGTK instalado?)")
    };

    // Bandeja del sistema
    let menu = Menu::new();
    let item_show = MenuItem::new("Mostrar Huillín", true, None);
    let item_quit = MenuItem::new("Salir", true, None);
    let _ = menu.append_items(&[&item_show, &item_quit]);
    let (id_show, id_quit) = (item_show.id().clone(), item_quit.id().clone());
    let mut tb = TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Huillín").with_menu_on_left_click(false);
    if let Some((d, w, h)) = &logo {
        if let Ok(i) = tray_icon::Icon::from_rgba(d.clone(), *w, *h) {
            tb = tb.with_icon(i);
        }
    }
    let mut tb = Some(tb);
    let mut tray = None;
    let p = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == id_show {
            let _ = p.send_event(UserEvent::Show);
        } else if e.id == id_quit {
            let _ = p.send_event(UserEvent::Quit);
        }
    }));
    let p = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            let _ = p.send_event(UserEvent::Show);
        }
    }));

    event_loop.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            // La bandeja se crea con el bucle ya en marcha (requisito de macOS).
            Event::NewEvents(StartCause::Init) => {
                tray = tb.take().and_then(|b| match b.build() {
                    Ok(icon) => Some(icon),
                    Err(error) => {
                        eprintln!("No se pudo crear el icono de la bandeja: {error}");
                        window.set_title("Huillín — bandeja no disponible");
                        None
                    }
                });
            }
            // Cerrar la ventana la oculta en la bandeja; la descarga sigue.
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                session.save();
                // Mantener el icono vivo y no ocultar una ventana sin forma de recuperarla.
                if tray.is_some() {
                    window.set_visible(false);
                } else {
                    window.set_minimized(true);
                }
            }
            Event::UserEvent(UserEvent::Show) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus();
            }
            Event::UserEvent(UserEvent::Quit) => {
                session.save();
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

/// Atiende a otras instancias: líneas `ADD <torrent|magnet>` y `SHOW`.
fn ipc_loop(listener: TcpListener, session: std::sync::Arc<session::Session>, proxy: Option<EventLoopProxy<UserEvent>>, rt: tokio::runtime::Handle) {
    let _g = rt.enter();
    for conn in listener.incoming().flatten() {
        for line in BufReader::new(conn).lines().map_while(Result::ok) {
            if let Some(a) = line.strip_prefix("ADD ") {
                add_arg(&session, a);
            } else if line == "SHOW" {
                if let Some(p) = &proxy {
                    let _ = p.send_event(UserEvent::Show);
                }
            }
        }
    }
}
