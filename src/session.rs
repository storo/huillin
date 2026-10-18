//! Sesión: lista de torrents, listeners de conexiones entrantes (IPv4/IPv6) y persistencia.
use crate::meta::{self, Meta};
use crate::mse;
use crate::network::{Config as NetworkConfig, Network};
use crate::ratelimit::Limiter;
use crate::torrent::{Ctx, Incoming, State, Torrent, HEADER};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio::time::{sleep, timeout};

pub struct Session {
    pub ctx: Arc<Ctx>,
    pub torrents: Mutex<Vec<Arc<Torrent>>>,
    pub default_dir: Mutex<PathBuf>,
    /// Añadir automáticamente los magnets que se copien al portapapeles.
    pub clipboard: AtomicBool,
    /// (contador, nombre) del último magnet añadido desde el portapapeles.
    pub clip_event: Mutex<(u64, String)>,
    data_dir: PathBuf,
    save_lock: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
struct SavedTorrent {
    hash: String,
    name: String,
    save_dir: String,
    paused: bool,
    trackers: Vec<String>,
    #[serde(default)]
    skipped: Vec<usize>,
}

#[derive(Serialize, Deserialize)]
struct SavedState {
    #[serde(default)]
    network: NetworkConfig,
    default_dir: Option<String>,
    #[serde(default)]
    down_kbs: u64,
    #[serde(default)]
    up_kbs: u64,
    #[serde(default = "default_encryption")]
    encryption: u8,
    #[serde(default = "default_true")]
    clipboard: bool,
    torrents: Vec<SavedTorrent>,
}

fn default_encryption() -> u8 {
    1
}

fn default_true() -> bool {
    true
}

fn home_dir() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var(var).map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

/// Carpeta de estado (en Windows se mantiene `%APPDATA%\RTorrent`).
fn app_data_dir() -> PathBuf {
    if cfg!(windows) {
        std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(".")).join("RTorrent")
    } else if cfg!(target_os = "macos") {
        home_dir().join("Library/Application Support/Huillin")
    } else {
        std::env::var("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".local/share"))
            .join("huillin")
    }
}

impl Default for SavedState {
    fn default() -> Self {
        SavedState { network: NetworkConfig::default(), default_dir: None, down_kbs: 0, up_kbs: 0, encryption: 1, clipboard: true, torrents: Vec::new() }
    }
}

fn load_state(path: &std::path::Path) -> std::io::Result<SavedState> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(SavedState::default()),
        Err(error) => Err(error),
    }
}

impl Session {
    pub async fn new() -> Arc<Session> {
        let data_dir = app_data_dir();
        let _ = std::fs::create_dir_all(data_dir.join("torrents"));
        let mut default_dir = home_dir().join("Downloads");

        // Never discard a saved network restriction on a read/parse failure.
        let saved = load_state(&data_dir.join("state.json"))
            .expect("No se pudo leer state.json; se detiene el arranque para conservar la restricción de red");
        if let Some(d) = &saved.default_dir {
            default_dir = PathBuf::from(d);
        }
        let ctx = Arc::new(Ctx {
            peer_id: {
                let mut id: [u8; 20] = rand::random();
                id[..8].copy_from_slice(b"-RT0100-");
                id
            },
            port: AtomicU16::new(0),
            network: Network::new(saved.network),
            down: Limiter::new(),
            up: Limiter::new(),
            encryption: AtomicU8::new(saved.encryption.min(2)),
        });
        ctx.down.set(saved.down_kbs * 1024);
        ctx.up.set(saved.up_kbs * 1024);
        let s = Arc::new(Session {
            ctx,
            torrents: Mutex::new(Vec::new()),
            default_dir: Mutex::new(default_dir),
            clipboard: AtomicBool::new(saved.clipboard),
            clip_event: Mutex::new((0, String::new())),
            data_dir,
            save_lock: Mutex::new(()),
        });

        for st in saved.torrents {
            let Ok(raw) = hex::decode(&st.hash) else { continue };
            let Ok(info_hash) = <[u8; 20]>::try_from(raw) else { continue };
            let meta = std::fs::read(s.torrent_path(&st.hash)).ok().and_then(|b| meta::parse_torrent(&b).ok()).map(|m| m.0);
            let skipped: HashSet<usize> = st.skipped.into_iter().collect();
            let t = Torrent::new(s.ctx.clone(), info_hash, meta, st.name, st.trackers, PathBuf::from(st.save_dir), st.paused, skipped);
            if !st.paused {
                t.start();
            }
            s.torrents.lock().unwrap().push(t);
        }

        tokio::spawn(s.ctx.network.clone().monitor());
        tokio::spawn(s.clone().network_loop());

        let s2 = s.clone();
        tokio::spawn(async move {
            loop {
                sleep(Duration::from_secs(15)).await;
                s2.save();
            }
        });

        // Vigila el portapapeles; el contenido que ya había al arrancar se ignora.
        let s2 = s.clone();
        tokio::spawn(async move {
            let mut last = crate::clipboard::sequence();
            loop {
                sleep(Duration::from_millis(1000)).await;
                let seq = crate::clipboard::sequence();
                if seq == last {
                    continue;
                }
                last = seq;
                if !s2.clipboard.load(Ordering::Relaxed) {
                    continue;
                }
                let Some(text) = tokio::task::spawn_blocking(crate::clipboard::text).await.ok().flatten() else { continue };
                let text = text.trim();
                if text.len() < 8192 && text.get(..8).map_or(false, |p| p.eq_ignore_ascii_case("magnet:?")) {
                    if let Ok(id) = s2.add_magnet(text, None) {
                        let name = s2.find(&id).map(|t| t.inner.lock().unwrap().name.clone()).unwrap_or_default();
                        let mut ev = s2.clip_event.lock().unwrap();
                        *ev = (ev.0 + 1, name);
                    }
                }
            }
        });
        s
    }

    async fn network_loop(self: Arc<Self>) {
        let mut routes = self.ctx.network.subscribe();
        loop {
            let route = routes.borrow_and_update().clone();
            let mut listeners = Vec::new();
            let mut port = 0;
            if route.ready() {
                for p in 6881..=6999u16 {
                    for v6 in [false, true] {
                        if let Ok(listener) = route.listen(v6, p) {
                            listeners.push(listener);
                        }
                    }
                    if !listeners.is_empty() { port = p; break; }
                }
            }
            self.ctx.port.store(port, Ordering::Relaxed);
            let mut tasks = JoinSet::new();
            for listener in listeners {
                let session = self.clone();
                tasks.spawn(async move {
                    let mut incoming = JoinSet::new();
                    loop {
                        tokio::select! {
                            result = listener.accept() => {
                                if let Ok((stream, addr)) = result {
                                    let session = session.clone();
                                    incoming.spawn(async move { session.handle_incoming(stream, addr).await });
                                } else { sleep(Duration::from_millis(50)).await; }
                            }
                            Some(_) = incoming.join_next(), if !incoming.is_empty() => {}
                        }
                    }
                });
            }
            if port == 0 && route.ready() {
                tokio::select! { _ = routes.changed() => {}, _ = sleep(Duration::from_secs(2)) => {} }
            } else {
                let _ = routes.changed().await;
            }
            tasks.shutdown().await;
            self.ctx.port.store(0, Ordering::Relaxed);
        }
    }

    /// Distingue conexiones en claro de las cifradas (MSE) y las entrega al torrent correspondiente.
    async fn handle_incoming(&self, mut stream: TcpStream, addr: SocketAddr) {
        let mode = self.ctx.encryption.load(Ordering::Relaxed);
        let mut head = [0u8; 20];
        let plain = loop {
            match timeout(Duration::from_secs(10), stream.peek(&mut head)).await {
                Ok(Ok(0)) | Ok(Err(_)) | Err(_) => return,
                Ok(Ok(_)) if head[0] != HEADER[0] => break false,
                Ok(Ok(n)) if n >= 20 => break head == HEADER,
                Ok(Ok(_)) => sleep(Duration::from_millis(50)).await,
            }
        };

        let mut hs = [0u8; 68];
        let mut crypt = None;
        if plain {
            if mode == 2 {
                return;
            }
            if timeout(Duration::from_secs(10), stream.read_exact(&mut hs)).await.map_or(true, |r| r.is_err()) {
                return;
            }
        } else {
            if mode == 0 {
                return;
            }
            let hashes: Vec<[u8; 20]> = self.torrents.lock().unwrap().iter().map(|t| t.info_hash).collect();
            let Ok(Ok((mut c, _, ia))) = timeout(Duration::from_secs(20), mse::respond(&mut stream, &hashes)).await else {
                return;
            };
            if ia.len() > 68 {
                return;
            }
            hs[..ia.len()].copy_from_slice(&ia);
            if ia.len() < 68 {
                let rest = &mut hs[ia.len()..];
                if timeout(Duration::from_secs(10), stream.read_exact(rest)).await.map_or(true, |r| r.is_err()) {
                    return;
                }
                c.dec.apply(rest);
            }
            crypt = Some(c);
        }
        if hs[..20] != HEADER {
            return;
        }
        let ih: [u8; 20] = hs[28..48].try_into().unwrap();
        let t = self.torrents.lock().unwrap().iter().find(|t| t.info_hash == ih).cloned();
        if let Some(t) = t {
            t.incoming(Incoming { stream, addr, reserved: hs[20..28].try_into().unwrap(), crypt });
        }
    }

    fn torrent_path(&self, hex: &str) -> PathBuf {
        self.data_dir.join("torrents").join(format!("{hex}.torrent"))
    }

    pub fn find(&self, id: &str) -> Option<Arc<Torrent>> {
        self.torrents.lock().unwrap().iter().find(|t| t.id() == id).cloned()
    }

    pub fn save(&self) {
        let _ = self.save_checked();
    }

    fn save_checked(&self) -> std::io::Result<()> {
        let _save = self.save_lock.lock().unwrap();
        let list = self.torrents.lock().unwrap().clone();
        let mut torrents = Vec::new();
        for t in list {
            let id = t.id();
            let path = self.torrent_path(&id);
            if !path.exists() {
                if let Some(data) = t.torrent_file() {
                    let _ = std::fs::write(&path, data);
                }
            }
            let g = t.inner.lock().unwrap();
            let mut skipped: Vec<usize> = g.skipped.iter().copied().collect();
            skipped.sort_unstable();
            torrents.push(SavedTorrent {
                hash: id,
                name: g.name.clone(),
                save_dir: g.save_dir.to_string_lossy().into_owned(),
                paused: g.paused,
                trackers: g.trackers.clone(),
                skipped,
            });
        }
        let st = SavedState {
            network: self.ctx.network.current().config.clone(),
            default_dir: Some(self.default_dir.lock().unwrap().to_string_lossy().into_owned()),
            down_kbs: self.ctx.down.get() / 1024,
            up_kbs: self.ctx.up.get() / 1024,
            encryption: self.ctx.encryption.load(Ordering::Relaxed),
            clipboard: self.clipboard.load(Ordering::Relaxed),
            torrents,
        };
        let json = serde_json::to_vec_pretty(&st)?;
        let tmp = self.data_dir.join("state.json.tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, self.data_dir.join("state.json"))
    }

    fn resolve_dir(&self, dir: Option<String>) -> PathBuf {
        match dir.filter(|d| !d.trim().is_empty()) {
            Some(d) => PathBuf::from(d.trim()),
            None => self.default_dir.lock().unwrap().clone(),
        }
    }

    fn insert(&self, t: Arc<Torrent>) {
        t.start();
        self.torrents.lock().unwrap().push(t);
        self.save();
    }

    pub fn add_magnet(&self, uri: &str, dir: Option<String>) -> Result<String, String> {
        let (ih, name, trackers) = meta::parse_magnet(uri.trim())?;
        if self.torrents.lock().unwrap().iter().any(|t| t.info_hash == ih) {
            return Err("el torrent ya está en la lista".into());
        }
        let name = if name.is_empty() { hex::encode(ih) } else { name };
        let t = Torrent::new(self.ctx.clone(), ih, None, name, trackers, self.resolve_dir(dir), false, HashSet::new());
        let id = t.id();
        self.insert(t);
        Ok(id)
    }

    pub fn add_file(&self, data: &[u8], dir: Option<String>) -> Result<String, String> {
        let (m, trackers): (Meta, _) = meta::parse_torrent(data)?;
        if self.torrents.lock().unwrap().iter().any(|t| t.info_hash == m.info_hash) {
            return Err("el torrent ya está en la lista".into());
        }
        let ih = m.info_hash;
        let name = m.name.clone();
        let t = Torrent::new(self.ctx.clone(), ih, Some(m), name, trackers, self.resolve_dir(dir), false, HashSet::new());
        let id = t.id();
        let _ = std::fs::write(self.torrent_path(&id), data);
        self.insert(t);
        Ok(id)
    }

    pub fn pause(&self, id: &str) -> bool {
        let Some(t) = self.find(id) else { return false };
        t.pause();
        self.save();
        true
    }

    pub fn resume(&self, id: &str) -> bool {
        let Some(t) = self.find(id) else { return false };
        // Un torrent con error vuelve a verificarse al reanudar.
        {
            let mut g = t.inner.lock().unwrap();
            if matches!(g.state, State::Error(_)) {
                g.state = if g.meta.is_some() { State::Checking } else { State::Metadata };
                g.checked = false;
            }
        }
        t.start();
        self.save();
        true
    }

    pub fn remove(&self, id: &str, delete_files: bool) -> bool {
        let Some(t) = self.find(id) else { return false };
        t.pause();
        self.torrents.lock().unwrap().retain(|x| x.id() != id);
        if delete_files {
            t.delete_files();
        }
        let _ = std::fs::remove_file(self.torrent_path(id));
        self.save();
        true
    }

    pub fn set_skipped(&self, id: &str, skip: HashSet<usize>) -> bool {
        let Some(t) = self.find(id) else { return false };
        t.set_skipped(skip);
        self.save();
        true
    }

    pub fn set_limits(&self, down_kbs: Option<u64>, up_kbs: Option<u64>, encryption: Option<u8>) -> std::io::Result<()> {
        if let Some(v) = down_kbs {
            self.ctx.down.set(v.min(1 << 30) * 1024);
        }
        if let Some(v) = up_kbs {
            self.ctx.up.set(v.min(1 << 30) * 1024);
        }
        if let Some(v) = encryption {
            self.ctx.encryption.store(v.min(2), Ordering::Relaxed);
        }
        self.save_checked()
    }

    pub fn list(&self) -> Vec<serde_json::Value> {
        self.torrents.lock().unwrap().iter().map(|t| t.snapshot()).collect()
    }

    pub fn open_folder(&self, id: &str) -> bool {
        let Some(t) = self.find(id) else { return false };
        let snap = t.snapshot();
        let target = snap["path"]
            .as_str()
            .map(PathBuf::from)
            .filter(|p| p.exists())
            .unwrap_or_else(|| PathBuf::from(snap["save_dir"].as_str().unwrap_or(".")));
        #[cfg(windows)]
        {
            let mut cmd = std::process::Command::new("explorer");
            if target.is_file() {
                cmd.arg(format!("/select,{}", target.display()));
            } else {
                cmd.arg(&target);
            }
            cmd.spawn().is_ok()
        }
        #[cfg(target_os = "macos")]
        {
            let mut cmd = std::process::Command::new("open");
            if target.is_file() {
                cmd.arg("-R");
            }
            cmd.arg(&target).spawn().is_ok()
        }
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let dir = if target.is_file() { target.parent().map(PathBuf::from).unwrap_or(target) } else { target };
            std::process::Command::new("xdg-open").arg(dir).spawn().is_ok()
        }
    }

    pub fn port(&self) -> u16 {
        self.ctx.port.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod network_state_tests {
    use super::*;

    #[test]
    fn old_state_loads_without_network_settings() {
        let saved: SavedState = serde_json::from_str(r#"{"default_dir":null,"torrents":[]}"#).unwrap();
        assert!(saved.network.interface.is_empty());
    }

    #[test]
    fn corrupt_state_is_an_error_instead_of_unrestricted_defaults() {
        let path = std::env::temp_dir().join(format!("huillin-network-state-{}.json", hex::encode(rand::random::<[u8; 16]>())));
        assert!(load_state(&path).unwrap().network.interface.is_empty());
        std::fs::write(&path, br#"{"network":{"interface":"vpn","dns":"broken"},"torrents":[]}"#).unwrap();
        assert!(load_state(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
