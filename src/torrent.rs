//! Un torrent: estado compartido, protocolo de peers (BEP 3, BEP 9/10), selección de piezas y choking.
use crate::bencode::{self, Value};
use crate::dht::Dht;
use crate::meta::Meta;
use crate::mse::{self, Crypt, Rc4};
use crate::network::{AbortOnDrop, Network, Route};
use crate::ratelimit::Limiter;
use crate::storage::Storage;
use crate::tracker::{self, Event, Stats};
use serde_json::json;
use sha1::{Digest, Sha1};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{interval, sleep, timeout};

pub const BLOCK: u32 = 16384;
pub const HEADER: [u8; 20] = *b"\x13BitTorrent protocol";
const PIPELINE: usize = 16;
const MAX_ACTIVE: usize = 60;

pub struct Ctx {
    pub peer_id: [u8; 20],
    pub port: AtomicU16,
    pub network: Arc<Network>,
    pub down: Limiter,
    pub up: Limiter,
    /// 0 = sin cifrado, 1 = preferir cifrado (con alternativa en claro), 2 = exigir cifrado
    pub encryption: AtomicU8,
}

#[derive(Clone, PartialEq, Debug)]
pub enum State {
    Metadata,
    Checking,
    Downloading,
    Seeding,
    Error(String),
}

pub struct Incoming {
    pub stream: TcpStream,
    pub addr: SocketAddr,
    pub reserved: [u8; 8],
    pub crypt: Option<Crypt>,
}

struct PieceBuf {
    data: Vec<u8>,
    state: Vec<u8>, // 0 libre, 1 pedido, 2 recibido
    got: usize,
}

impl PieceBuf {
    fn new(size: u32) -> Self {
        let nb = ((size + BLOCK - 1) / BLOCK) as usize;
        PieceBuf { data: vec![0; size as usize], state: vec![0; nb], got: 0 }
    }
    fn block_len(&self, b: usize) -> u32 {
        (self.data.len() as u32 - b as u32 * BLOCK).min(BLOCK)
    }
}

struct PeerInfo {
    tx: UnboundedSender<Vec<u8>>,
    pieces: Vec<bool>,
    count: usize, // piezas que tiene el peer
    raw_bf: Option<Vec<u8>>,
    early_have: Vec<usize>,
    choked: bool,
    am_interested: bool,
    peer_interested: bool,
    am_choking: bool,
    pending: HashSet<(usize, u32)>,
    down_total: u64,
    last_down: u64,
    rate: f64,
    ut_metadata: u8,
}

struct MetaAsm {
    size: usize,
    pieces: Vec<Option<Vec<u8>>>,
}

pub struct Inner {
    pub meta: Option<Arc<Meta>>,
    storage: Option<Arc<Storage>>,
    pub name: String,
    pub state: State,
    pub paused: bool,
    pub save_dir: PathBuf,
    pub trackers: Vec<String>,
    /// Indices de archivos que el usuario no quiere descargar.
    pub skipped: HashSet<usize>,
    wanted: Vec<bool>, // por pieza: indica si toca algun archivo deseado
    wanted_missing: usize,
    have: Vec<bool>,
    have_count: usize,
    partial: HashMap<usize, PieceBuf>,
    verifying: HashSet<usize>,
    avail: Vec<u16>,
    peers: HashMap<u64, PeerInfo>,
    next_pid: u64,
    active: HashSet<SocketAddr>,
    meta_asm: Option<MetaAsm>,
    pub checked: bool,
    down_rate: f64,
    up_rate: f64,
}

pub fn frame(id: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + payload.len());
    v.extend((payload.len() as u32 + 1).to_be_bytes());
    v.push(id);
    v.extend(payload);
    v
}

fn ext_frame(ext_id: u8, d: &Value, extra: &[u8]) -> Vec<u8> {
    let mut p = vec![ext_id];
    p.extend(bencode::encode(d));
    p.extend(extra);
    frame(20, &p)
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b[..4].try_into().unwrap())
}

impl Inner {
    fn set_meta(&mut self, meta: Arc<Meta>) {
        let n = meta.hashes.len();
        self.have = vec![false; n];
        self.have_count = 0;
        self.avail = vec![0; n];
        self.name = meta.name.clone();
        self.storage = Some(Arc::new(Storage::new(meta.clone(), &self.save_dir)));
        self.meta = Some(meta);
        self.meta_asm = None;
        self.recompute_wanted();
        let pids: Vec<u64> = self.peers.keys().copied().collect();
        for pid in pids {
            let p = self.peers.get_mut(&pid).unwrap();
            p.pieces = vec![false; n];
            p.count = 0;
            let early = std::mem::take(&mut p.early_have);
            let raw = p.raw_bf.take();
            for i in early.into_iter().filter(|&i| i < n) {
                if !p.pieces[i] {
                    p.pieces[i] = true;
                    p.count += 1;
                    self.avail[i] += 1;
                }
            }
            if let Some(raw) = raw {
                self.apply_raw(pid, &raw);
            }
        }
    }

    /// Recalcula que piezas hacen falta segun los archivos elegidos.
    fn recompute_wanted(&mut self) {
        let Some(m) = self.meta.clone() else { return };
        let n = m.hashes.len();
        let mut w = vec![false; n];
        for (i, f) in m.files.iter().enumerate() {
            if f.length > 0 && !self.skipped.contains(&i) {
                let a = (f.offset / m.piece_length) as usize;
                let b = ((f.offset + f.length - 1) / m.piece_length) as usize;
                w[a..=b].iter_mut().for_each(|x| *x = true);
            }
        }
        self.wanted_missing = (0..n).filter(|&i| w[i] && !self.have[i]).count();
        self.partial.retain(|&i, _| w[i]);
        self.wanted = w;
    }

    /// Alterna entre descargando y completado segun las piezas que faltan.
    fn update_state(&mut self) {
        if !matches!(self.state, State::Downloading | State::Seeding) {
            return;
        }
        let done = self.wanted_missing == 0;
        if done && self.state == State::Downloading {
            self.state = State::Seeding;
            self.partial.clear();
            for p in self.peers.values_mut() {
                p.pending.clear();
                if p.am_interested {
                    p.am_interested = false;
                    let _ = p.tx.send(frame(3, &[]));
                }
            }
        } else if !done && self.state == State::Seeding {
            self.state = State::Downloading;
            self.refresh_all();
        }
    }

    fn have_bytes(&self) -> u64 {
        let Some(m) = &self.meta else { return 0 };
        if self.have.is_empty() {
            return 0;
        }
        let last = self.have.len() - 1;
        let mut b = self.have_count as u64 * m.piece_length;
        if self.have[last] {
            b -= m.piece_length - m.piece_size(last);
        }
        b
    }

    fn apply_raw(&mut self, pid: u64, raw: &[u8]) {
        let n = self.have.len();
        let Some(p) = self.peers.get_mut(&pid) else { return };
        for i in 0..n {
            let bit = raw.get(i >> 3).map_or(false, |b| b & (0x80 >> (i & 7)) != 0);
            if bit && !p.pieces[i] {
                p.pieces[i] = true;
                p.count += 1;
                self.avail[i] += 1;
            }
        }
    }

    fn release_pending(&mut self, pid: u64) {
        let Some(p) = self.peers.get_mut(&pid) else { return };
        for (idx, off) in std::mem::take(&mut p.pending) {
            if let Some(buf) = self.partial.get_mut(&idx) {
                let b = (off / BLOCK) as usize;
                if buf.state[b] == 1 {
                    buf.state[b] = 0;
                }
            }
        }
    }

    fn update_interest(&mut self, pid: u64) {
        if self.state != State::Downloading {
            return;
        }
        let Some(p) = self.peers.get_mut(&pid) else { return };
        let want = (0..self.have.len()).any(|i| p.pieces[i] && !self.have[i] && self.wanted[i]);
        if want != p.am_interested {
            p.am_interested = want;
            let _ = p.tx.send(frame(if want { 2 } else { 3 }, &[]));
        }
    }

    fn next_request(&mut self, pid: u64) -> Option<(usize, u32, u32)> {
        let meta = self.meta.as_ref()?;
        let peer = self.peers.get(&pid)?;
        // 1. completar piezas ya empezadas
        for (&idx, buf) in self.partial.iter_mut() {
            if peer.pieces[idx] {
                if let Some(b) = buf.state.iter().position(|&s| s == 0) {
                    buf.state[b] = 1;
                    return Some((idx, b as u32 * BLOCK, buf.block_len(b)));
                }
            }
        }
        // 2. nueva pieza: la más rara que ofrezca este peer
        let n = self.have.len();
        let start = rand::random_range(0..n.max(1));
        let mut best: Option<(u16, usize)> = None;
        for k in 0..n {
            let i = (start + k) % n;
            if peer.pieces[i]
                && self.wanted[i]
                && !self.have[i]
                && !self.partial.contains_key(&i)
                && !self.verifying.contains(&i)
            {
                if best.map_or(true, |(a, _)| self.avail[i] < a) {
                    best = Some((self.avail[i], i));
                }
            }
        }
        if let Some((_, idx)) = best {
            let buf = PieceBuf::new(meta.piece_size(idx) as u32);
            let len = buf.block_len(0);
            let mut buf = buf;
            buf.state[0] = 1;
            self.partial.insert(idx, buf);
            return Some((idx, 0, len));
        }
        // 3. fin de la descarga: pedir bloques ya pedidos a otros peers
        for (&idx, buf) in self.partial.iter() {
            if peer.pieces[idx] {
                for b in 0..buf.state.len() {
                    if buf.state[b] == 1 && !peer.pending.contains(&(idx, b as u32 * BLOCK)) {
                        return Some((idx, b as u32 * BLOCK, buf.block_len(b)));
                    }
                }
            }
        }
        None
    }

    fn fill(&mut self, pid: u64) {
        if self.state != State::Downloading || self.paused {
            return;
        }
        loop {
            let ok = self.peers.get(&pid).map_or(false, |p| !p.choked && p.am_interested && p.pending.len() < PIPELINE);
            if !ok {
                break;
            }
            let Some((idx, off, len)) = self.next_request(pid) else { break };
            let p = self.peers.get_mut(&pid).unwrap();
            p.pending.insert((idx, off));
            let mut pl = Vec::with_capacity(12);
            pl.extend((idx as u32).to_be_bytes());
            pl.extend(off.to_be_bytes());
            pl.extend(len.to_be_bytes());
            let _ = p.tx.send(frame(6, &pl));
        }
    }

    fn refresh_all(&mut self) {
        let pids: Vec<u64> = self.peers.keys().copied().collect();
        for pid in pids {
            self.update_interest(pid);
            self.fill(pid);
        }
    }

    fn on_block(&mut self, pid: u64, idx: usize, begin: u32, data: &[u8]) -> Option<(usize, Vec<u8>)> {
        if let Some(p) = self.peers.get_mut(&pid) {
            p.pending.remove(&(idx, begin));
            p.down_total += data.len() as u64;
        }
        let buf = self.partial.get_mut(&idx)?;
        let b = (begin / BLOCK) as usize;
        if begin % BLOCK != 0 || b >= buf.state.len() || buf.state[b] == 2 || data.len() as u32 != buf.block_len(b) {
            return None;
        }
        let off = begin as usize;
        buf.data[off..off + data.len()].copy_from_slice(data);
        buf.state[b] = 2;
        buf.got += 1;
        if buf.got == buf.state.len() {
            let buf = self.partial.remove(&idx)?;
            self.verifying.insert(idx);
            return Some((idx, buf.data));
        }
        None
    }

    fn rechoke(&mut self) {
        let seeding = self.state == State::Seeding;
        let mut cand: Vec<(u64, f64)> = self
            .peers
            .iter()
            .filter(|(_, p)| p.peer_interested)
            .map(|(&id, p)| (id, if seeding { rand::random::<f64>() } else { p.rate }))
            .collect();
        cand.sort_by(|a, b| b.1.total_cmp(&a.1));
        let mut unchoke: HashSet<u64> = cand.iter().take(4).map(|c| c.0).collect();
        if cand.len() > 4 {
            unchoke.insert(cand[rand::random_range(4..cand.len())].0);
        }
        for (id, p) in self.peers.iter_mut() {
            let want = unchoke.contains(id);
            if want && p.am_choking {
                p.am_choking = false;
                let _ = p.tx.send(frame(1, &[]));
            } else if !want && !p.am_choking {
                p.am_choking = true;
                let _ = p.tx.send(frame(0, &[]));
            }
        }
    }

    fn broadcast(&self, f: &[u8]) {
        for p in self.peers.values() {
            let _ = p.tx.send(f.to_vec());
        }
    }
}

pub struct Torrent {
    pub info_hash: [u8; 20],
    ctx: Arc<Ctx>,
    pub inner: Mutex<Inner>,
    pub downloaded: AtomicU64,
    pub uploaded: AtomicU64,
    check_done: AtomicUsize,
    runner: Mutex<Option<JoinHandle<()>>>,
    incoming_tx: Mutex<Option<UnboundedSender<Incoming>>>,
}

struct PeerGuard {
    t: Arc<Torrent>,
    pid: u64,
}

impl Drop for PeerGuard {
    fn drop(&mut self) {
        let mut g = self.t.lock();
        g.release_pending(self.pid);
        if let Some(p) = g.peers.remove(&self.pid) {
            for (i, &h) in p.pieces.iter().enumerate() {
                if h && i < g.avail.len() {
                    g.avail[i] = g.avail[i].saturating_sub(1);
                }
            }
        }
    }
}

struct AddrGuard {
    t: Arc<Torrent>,
    addr: SocketAddr,
}

impl AddrGuard {
    fn claim(t: &Arc<Torrent>, addr: SocketAddr) -> Option<AddrGuard> {
        let mut g = t.lock();
        (g.active.len() < MAX_ACTIVE + 20 && g.active.insert(addr)).then(|| AddrGuard { t: t.clone(), addr })
    }
}

impl Drop for AddrGuard {
    fn drop(&mut self) {
        self.t.lock().active.remove(&self.addr);
    }
}

impl Torrent {
    pub fn new(
        ctx: Arc<Ctx>,
        info_hash: [u8; 20],
        meta: Option<Meta>,
        name: String,
        trackers: Vec<String>,
        save_dir: PathBuf,
        paused: bool,
        skipped: HashSet<usize>,
    ) -> Arc<Torrent> {
        let mut inner = Inner {
            meta: None,
            storage: None,
            name,
            state: State::Metadata,
            paused,
            save_dir,
            trackers,
            skipped,
            wanted: Vec::new(),
            wanted_missing: 0,
            have: Vec::new(),
            have_count: 0,
            partial: HashMap::new(),
            verifying: HashSet::new(),
            avail: Vec::new(),
            peers: HashMap::new(),
            next_pid: 1,
            active: HashSet::new(),
            meta_asm: None,
            checked: false,
            down_rate: 0.0,
            up_rate: 0.0,
        };
        if let Some(m) = meta {
            inner.set_meta(Arc::new(m));
        }
        Arc::new(Torrent {
            info_hash,
            ctx,
            inner: Mutex::new(inner),
            downloaded: AtomicU64::new(0),
            uploaded: AtomicU64::new(0),
            check_done: AtomicUsize::new(0),
            runner: Mutex::new(None),
            incoming_tx: Mutex::new(None),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    pub fn id(&self) -> String {
        hex::encode(self.info_hash)
    }

    pub fn start(self: &Arc<Self>) {
        let mut r = self.runner.lock().unwrap();
        if r.is_some() {
            return;
        }
        self.lock().paused = false;
        *r = Some(tokio::spawn(self.clone().run_supervised()));
    }

    pub fn pause(&self) {
        if let Some(h) = self.runner.lock().unwrap().take() {
            h.abort();
        }
        *self.incoming_tx.lock().unwrap() = None;
        let mut g = self.lock();
        g.paused = true;
        g.peers.clear();
        g.active.clear();
        g.partial.clear();
        g.verifying.clear();
        g.avail.iter_mut().for_each(|a| *a = 0);
        if g.state == State::Checking {
            g.checked = false;
        }
        g.down_rate = 0.0;
        g.up_rate = 0.0;
    }

    /// Lista de archivos con su estado de selección y progreso.
    pub fn files(&self) -> Vec<serde_json::Value> {
        let g = self.lock();
        let Some(m) = &g.meta else { return Vec::new() };
        m.files
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let progress = if f.length == 0 || g.have.is_empty() {
                    if f.length == 0 { 1.0 } else { 0.0 }
                } else {
                    let a = (f.offset / m.piece_length) as usize;
                    let b = ((f.offset + f.length - 1) / m.piece_length) as usize;
                    g.have[a..=b].iter().filter(|&&h| h).count() as f64 / (b - a + 1) as f64
                };
                json!({ "index": i, "path": f.path.join("/"), "size": f.length,
                        "wanted": !g.skipped.contains(&i), "progress": progress })
            })
            .collect()
    }

    /// Define qué archivos no se descargan.
    pub fn set_skipped(&self, skip: HashSet<usize>) {
        let mut g = self.lock();
        let Some(nfiles) = g.meta.as_ref().map(|m| m.files.len()) else { return };
        g.skipped = skip.into_iter().filter(|&i| i < nfiles).collect();
        g.recompute_wanted();
        g.update_state();
        if g.state == State::Downloading {
            g.refresh_all();
        }
    }

    pub fn delete_files(&self) {
        let storage = self.lock().storage.clone();
        if let Some(s) = storage {
            s.delete_files();
        }
    }

    pub fn incoming(&self, inc: Incoming) {
        if let Some(tx) = self.incoming_tx.lock().unwrap().as_ref() {
            let _ = tx.send(inc);
        }
    }

    /// Contenido de un .torrent mínimo (solo `info`) si ya se conocen los metadatos.
    pub fn torrent_file(&self) -> Option<Vec<u8>> {
        let g = self.lock();
        let m = g.meta.as_ref()?;
        let mut v = b"d4:info".to_vec();
        v.extend(&m.info_raw);
        v.push(b'e');
        Some(v)
    }

    fn handshake(&self) -> Vec<u8> {
        let mut h = Vec::with_capacity(68);
        h.extend(HEADER);
        let mut reserved = [0u8; 8];
        reserved[5] |= 0x10; // protocolo de extensiones
        h.extend(reserved);
        h.extend(self.info_hash);
        h.extend(self.ctx.peer_id);
        h
    }

    pub fn snapshot(&self) -> serde_json::Value {
        let g = self.lock();
        let size = g.meta.as_ref().map_or(0, |m| m.length);
        let n = g.have.len();
        // El progreso se mide sobre los archivos elegidos.
        let (mut wanted_size, mut wanted_done) = (0u64, 0u64);
        if let Some(m) = &g.meta {
            for i in 0..n.min(g.wanted.len()) {
                if g.wanted[i] {
                    let sz = m.piece_size(i);
                    wanted_size += sz;
                    if g.have[i] {
                        wanted_done += sz;
                    }
                }
            }
        }
        let progress = match (&g.state, g.meta.is_some()) {
            (State::Checking, _) if n > 0 => self.check_done.load(Ordering::Relaxed) as f64 / n as f64,
            (_, false) => 0.0,
            _ if wanted_size == 0 => 1.0,
            _ => wanted_done as f64 / wanted_size as f64,
        };
        let (state, error) = if g.paused {
            ("paused", String::new())
        } else {
            match &g.state {
                State::Metadata => ("metadata", String::new()),
                State::Checking => ("checking", String::new()),
                State::Downloading => ("downloading", String::new()),
                State::Seeding => ("seeding", String::new()),
                State::Error(e) => ("error", e.clone()),
            }
        };
        let seeds = if n > 0 { g.peers.values().filter(|p| p.count == n).count() } else { 0 };
        let left = wanted_size.saturating_sub(wanted_done);
        let eta = if state == "downloading" && g.down_rate > 1024.0 { (left as f64 / g.down_rate) as i64 } else { -1 };
        json!({
            "id": self.id(), "name": g.name, "size": size, "wanted_size": wanted_size, "progress": progress,
            "state": state, "error": error,
            "down": g.down_rate, "up": g.up_rate, "peers": g.peers.len(), "seeds": seeds, "eta": eta,
            "downloaded": wanted_done, "uploaded": self.uploaded.load(Ordering::Relaxed),
            "save_dir": g.save_dir.to_string_lossy(),
            "path": g.meta.as_ref().map(|m| {
                let mut p = g.save_dir.clone();
                p.push(&m.name);
                p.to_string_lossy().into_owned()
            }),
        })
    }

    // ---------- bucle principal ----------

    async fn run_supervised(self: Arc<Self>) {
        let mut routes = self.ctx.network.subscribe();
        loop {
            let route = routes.borrow_and_update().clone();
            if route.ready() {
                self.clone().run(route, &mut routes).await;
            } else if routes.changed().await.is_err() {
                return;
            }
        }
    }

    async fn run(self: Arc<Self>, route: Arc<Route>, routes: &mut tokio::sync::watch::Receiver<Arc<Route>>) {
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel::<SocketAddr>();
        let (inc_tx, mut inc_rx) = mpsc::unbounded_channel::<Incoming>();
        *self.incoming_tx.lock().unwrap() = Some(inc_tx);
        let mut tasks: JoinSet<()> = JoinSet::new();
        let trackers = self.lock().trackers.clone();
        for url in trackers {
            tasks.spawn(self.clone().tracker_loop(url, peer_tx.clone(), route.clone()));
        }
        tasks.spawn(self.clone().dht_loop(peer_tx.clone(), route.clone()));

        let mut tick = interval(Duration::from_secs(1));
        let mut cand: VecDeque<SocketAddr> = VecDeque::new();
        let (mut last_d, mut last_u) = (self.downloaded.load(Ordering::Relaxed), self.uploaded.load(Ordering::Relaxed));
        let mut n = 0u64;
        loop {
            tokio::select! {
                biased;
                _ = routes.changed() => {
                    *self.incoming_tx.lock().unwrap() = None;
                    tasks.shutdown().await;
                    let mut g = self.lock();
                    g.peers.clear();
                    g.active.clear();
                    g.partial.clear();
                    g.verifying.clear();
                    g.avail.fill(0);
                    g.down_rate = 0.0;
                    g.up_rate = 0.0;
                    if g.state == State::Checking { g.checked = false; }
                    return;
                }
                _ = tick.tick() => {
                    n += 1;
                    let (d, u) = (self.downloaded.load(Ordering::Relaxed), self.uploaded.load(Ordering::Relaxed));
                    let mut start_check = false;
                    let mut to_connect = Vec::new();
                    {
                        let mut g = self.lock();
                        g.down_rate = g.down_rate * 0.6 + (d - last_d) as f64 * 0.4;
                        g.up_rate = g.up_rate * 0.6 + (u - last_u) as f64 * 0.4;
                        for p in g.peers.values_mut() {
                            p.rate = p.rate * 0.5 + (p.down_total - p.last_down) as f64 * 0.5;
                            p.last_down = p.down_total;
                        }
                        if g.meta.is_none() {
                            g.state = State::Metadata;
                        } else if !g.checked {
                            g.checked = true;
                            g.state = State::Checking;
                            start_check = true;
                        }
                        let mut room = MAX_ACTIVE.saturating_sub(g.active.len()).min(10);
                        while room > 0 {
                            let Some(a) = cand.pop_front() else { break };
                            if !g.active.contains(&a) {
                                to_connect.push(a);
                                room -= 1;
                            }
                        }
                        if n % 10 == 0 {
                            g.rechoke();
                        }
                        if n % 5 == 0 {
                            g.refresh_all();
                        }
                        if n % 50 == 0 {
                            g.broadcast(&[0, 0, 0, 0]);
                        }
                    }
                    last_d = d;
                    last_u = u;
                    if start_check {
                        tasks.spawn(self.clone().do_check());
                    }
                    for a in to_connect {
                        tasks.spawn(self.clone().peer_connect(a, route.clone()));
                    }
                }
                Some(a) = peer_rx.recv() => {
                    if cand.len() < 2000 && !cand.contains(&a) {
                        cand.push_back(a);
                    }
                }
                Some(inc) = inc_rx.recv() => {
                    tasks.spawn(self.clone().peer_incoming(inc));
                }
                Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            }
        }
    }

    async fn do_check(self: Arc<Self>) {
        let storage = self.lock().storage.clone();
        let Some(storage) = storage else { return };
        let t = self.clone();
        let have = tokio::task::spawn_blocking(move || storage.check(&t.check_done)).await;
        let mut g = self.lock();
        match have {
            Ok(have) => {
                g.have_count = have.iter().filter(|&&h| h).count();
                let haves: Vec<usize> = have.iter().enumerate().filter(|(_, &h)| h).map(|(i, _)| i).collect();
                g.have = have;
                g.recompute_wanted();
                g.state = State::Downloading;
                g.update_state();
                for i in haves.into_iter().take(50_000) {
                    g.broadcast(&frame(4, &(i as u32).to_be_bytes()));
                }
                g.refresh_all();
            }
            Err(e) => g.state = State::Error(format!("error al verificar: {e}")),
        }
    }

    async fn finish_piece(self: &Arc<Self>, idx: usize, data: Vec<u8>) {
        let (meta, storage) = {
            let g = self.lock();
            (g.meta.clone().unwrap(), g.storage.clone().unwrap())
        };
        let res = tokio::task::spawn_blocking(move || {
            if Sha1::digest(&data).as_slice() != meta.hashes[idx] {
                return Ok(false);
            }
            storage.write(idx as u64 * meta.piece_length, &data).map(|_| true)
        })
        .await;
        let mut g = self.lock();
        g.verifying.remove(&idx);
        match res {
            Ok(Ok(true)) => {
                g.have[idx] = true;
                g.have_count += 1;
                g.broadcast(&frame(4, &(idx as u32).to_be_bytes()));
                if g.wanted[idx] {
                    g.wanted_missing -= 1;
                }
                g.update_state();
            }
            Ok(Ok(false)) => {} // hash incorrecto: la pieza se volverá a pedir
            Ok(Err(e)) => g.state = State::Error(format!("error de disco: {e}")),
            Err(e) => g.state = State::Error(e.to_string()),
        }
    }

    // ---------- descubrimiento de peers ----------

    async fn tracker_loop(self: Arc<Self>, url: String, out: UnboundedSender<SocketAddr>, route: Arc<Route>) {
        let mut event = Event::Started;
        loop {
            let (left, up, down) = {
                let g = self.lock();
                let left = g.meta.as_ref().map_or(1, |m| m.length.saturating_sub(g.have_bytes()));
                (left, self.uploaded.load(Ordering::Relaxed), self.downloaded.load(Ordering::Relaxed))
            };
            let stats = Stats {
                info_hash: &self.info_hash,
                peer_id: &self.ctx.peer_id,
                port: self.ctx.port.load(Ordering::Relaxed),
                uploaded: up,
                downloaded: down,
                left,
                event,
            };
            match tracker::announce(&url, &stats, &route).await {
                Ok((interval, peers)) => {
                    peers.into_iter().for_each(|p| {
                        let _ = out.send(p);
                    });
                    event = Event::None;
                    sleep(Duration::from_secs(interval.clamp(60, 1800) as u64)).await;
                }
                Err(_) => sleep(Duration::from_secs(120)).await,
            }
        }
    }

    async fn dht_loop(self: Arc<Self>, out: UnboundedSender<SocketAddr>, route: Arc<Route>) {
        if self.lock().meta.as_ref().is_some_and(|m| m.private) {
            return;
        }
        let mut dhts = Vec::new();
        let mut receivers = Vec::new();
        for v6 in [false, true] {
            if let Ok((dht, receiver)) = Dht::start(v6, route.clone()).await {
                dhts.push(dht);
                receivers.push(receiver);
            }
        }
        if dhts.is_empty() {
            return;
        }
        loop {
            if self.lock().meta.as_ref().is_some_and(|m| m.private) {
                return;
            }
            let mut set = JoinSet::new();
            for dht in &dhts {
                let dht = dht.clone();
                let (ih, out) = (self.info_hash, out.clone());
                set.spawn(async move { dht.get_peers(ih, out, Duration::from_secs(30)).await });
            }
            while set.join_next().await.is_some() {}
            let have_peers = self.lock().peers.len() >= 10;
            sleep(Duration::from_secs(if have_peers { 600 } else { 90 })).await;
        }
    }

    // ---------- conexiones ----------

    async fn peer_connect(self: Arc<Self>, addr: SocketAddr, route: Arc<Route>) {
        let Some(_ag) = AddrGuard::claim(&self, addr) else { return };
        let mode = self.ctx.encryption.load(Ordering::Relaxed);
        let mut res = None;
        if mode != 0 {
            res = self.dial(addr, true, &route).await;
        }
        if res.is_none() && mode != 2 {
            res = self.dial(addr, false, &route).await;
        }
        if let Some((s, reserved, crypt)) = res {
            self.run_peer(s, reserved, crypt).await;
        }
    }

    /// Conecta y completa el handshake, con o sin cifrado de protocolo.
    async fn dial(&self, addr: SocketAddr, encrypted: bool, route: &Route) -> Option<(TcpStream, [u8; 8], Option<Crypt>)> {
        let mut s = timeout(Duration::from_secs(8), route.connect(addr)).await.ok()?.ok()?;
        let mut crypt = if encrypted {
            Some(timeout(Duration::from_secs(15), mse::initiate(&mut s, &self.info_hash)).await.ok()?.ok()?)
        } else {
            None
        };
        let mut hs = self.handshake();
        if let Some(c) = &mut crypt {
            c.enc.apply(&mut hs);
        }
        s.write_all(&hs).await.ok()?;
        let mut r = [0u8; 68];
        timeout(Duration::from_secs(10), s.read_exact(&mut r)).await.ok()?.ok()?;
        if let Some(c) = &mut crypt {
            c.dec.apply(&mut r);
        }
        if r[..20] != HEADER || r[28..48] != self.info_hash || r[48..68] == self.ctx.peer_id {
            return None;
        }
        Some((s, r[20..28].try_into().unwrap(), crypt))
    }

    async fn peer_incoming(self: Arc<Self>, inc: Incoming) {
        let Some(_ag) = AddrGuard::claim(&self, inc.addr) else { return };
        let (mut s, mut crypt) = (inc.stream, inc.crypt);
        let mut hs = self.handshake();
        if let Some(c) = &mut crypt {
            c.enc.apply(&mut hs);
        }
        if s.write_all(&hs).await.is_err() {
            return;
        }
        self.run_peer(s, inc.reserved, crypt).await;
    }

    async fn run_peer(self: &Arc<Self>, stream: TcpStream, reserved: [u8; 8], crypt: Option<Crypt>) {
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let (mut enc, mut dec): (Option<Rc4>, Option<Rc4>) = match crypt {
            Some(c) => (Some(c.enc), Some(c.dec)),
            None => (None, None),
        };
        // Un único escritor: el orden de los bytes importa para el flujo RC4.
        let _writer = AbortOnDrop(tokio::spawn(async move {
            while let Some(mut f) = rx.recv().await {
                if let Some(e) = &mut enc {
                    e.apply(&mut f);
                }
                if wr.write_all(&f).await.is_err() {
                    break;
                }
            }
        }));
        let supports_ext = reserved[5] & 0x10 != 0;
        let pid = {
            let mut g = self.lock();
            if g.peers.len() >= MAX_ACTIVE + 20 {
                return;
            }
            let pid = g.next_pid;
            g.next_pid += 1;
            let n = g.have.len();
            g.peers.insert(
                pid,
                PeerInfo {
                    tx: tx.clone(),
                    pieces: vec![false; n],
                    count: 0,
                    raw_bf: None,
                    early_have: Vec::new(),
                    choked: true,
                    am_interested: false,
                    peer_interested: false,
                    am_choking: true,
                    pending: HashSet::new(),
                    down_total: 0,
                    last_down: 0,
                    rate: 0.0,
                    ut_metadata: 0,
                },
            );
            if supports_ext {
                let mut d = vec![
                    ("m", bencode::dict(vec![("ut_metadata", Value::Int(1))])),
                    ("v", bencode::bytes("Huillin 0.1")),
                    ("reqq", Value::Int(250)),
                ];
                if let Some(m) = &g.meta {
                    d.push(("metadata_size", Value::Int(m.info_raw.len() as i64)));
                }
                let _ = tx.send(ext_frame(0, &bencode::dict(d), &[]));
            }
            if g.have_count > 0 {
                let mut bf = vec![0u8; (n + 7) / 8];
                for (i, &h) in g.have.iter().enumerate() {
                    if h {
                        bf[i >> 3] |= 0x80 >> (i & 7);
                    }
                }
                let _ = tx.send(frame(5, &bf));
            }
            pid
        };
        let _guard = PeerGuard { t: self.clone(), pid };
        drop(tx);

        loop {
            let mut lb = [0u8; 4];
            match timeout(Duration::from_secs(150), rd.read_exact(&mut lb)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            if let Some(d) = &mut dec {
                d.apply(&mut lb);
            }
            let len = u32::from_be_bytes(lb) as usize;
            if len == 0 {
                continue;
            }
            if len > (1 << 21) {
                return;
            }
            let mut buf = vec![0u8; len];
            match timeout(Duration::from_secs(60), rd.read_exact(&mut buf)).await {
                Ok(Ok(_)) => {}
                _ => return,
            }
            if let Some(d) = &mut dec {
                d.apply(&mut buf);
            }
            if buf[0] == 7 {
                self.ctx.down.acquire(len).await; // límite de bajada
            }
            if self.handle_msg(pid, &buf).await.is_none() {
                return;
            }
        }
    }

    async fn handle_msg(self: &Arc<Self>, pid: u64, m: &[u8]) -> Option<()> {
        let p = &m[1..];
        match m[0] {
            0 => {
                let mut g = self.lock();
                g.release_pending(pid);
                g.peers.get_mut(&pid)?.choked = true;
            }
            1 => {
                let mut g = self.lock();
                g.peers.get_mut(&pid)?.choked = false;
                g.fill(pid);
            }
            2 => self.lock().peers.get_mut(&pid)?.peer_interested = true,
            3 => self.lock().peers.get_mut(&pid)?.peer_interested = false,
            4 => {
                if p.len() != 4 {
                    return None;
                }
                let idx = be32(p) as usize;
                let mut guard = self.lock();
                let g = &mut *guard;
                let n = g.have.len();
                if g.meta.is_none() {
                    let peer = g.peers.get_mut(&pid)?;
                    if peer.early_have.len() < 100_000 {
                        peer.early_have.push(idx);
                    }
                    return Some(());
                }
                if idx >= n {
                    return None;
                }
                let peer = g.peers.get_mut(&pid)?;
                if !peer.pieces[idx] {
                    peer.pieces[idx] = true;
                    peer.count += 1;
                    g.avail[idx] += 1;
                }
                g.update_interest(pid);
                g.fill(pid);
            }
            5 => {
                let mut g = self.lock();
                if g.meta.is_none() {
                    g.peers.get_mut(&pid)?.raw_bf = Some(p.to_vec());
                } else {
                    g.apply_raw(pid, p);
                    g.update_interest(pid);
                    g.fill(pid);
                }
            }
            6 => {
                if p.len() != 12 {
                    return None;
                }
                let (idx, begin, len) = (be32(p) as usize, be32(&p[4..]) as u64, be32(&p[8..]) as usize);
                let (meta, storage) = {
                    let g = self.lock();
                    let allowed = g.peers.get(&pid)?.am_choking == false && idx < g.have.len() && g.have[idx];
                    if !allowed {
                        return Some(());
                    }
                    (g.meta.clone()?, g.storage.clone()?)
                };
                if len > 32768 || begin + len as u64 > meta.piece_size(idx) {
                    return None;
                }
                let data = tokio::task::block_in_place(|| storage.read(idx as u64 * meta.piece_length + begin, len));
                if let Some(data) = data {
                    self.ctx.up.acquire(len).await; // límite de subida
                    let mut pl = Vec::with_capacity(8 + len);
                    pl.extend(&p[..8]);
                    pl.extend(data);
                    let g = self.lock();
                    if let Some(peer) = g.peers.get(&pid) {
                        let _ = peer.tx.send(frame(7, &pl));
                        self.uploaded.fetch_add(len as u64, Ordering::Relaxed);
                    }
                }
            }
            7 => {
                if p.len() < 9 {
                    return None;
                }
                let (idx, begin, data) = (be32(p) as usize, be32(&p[4..]), &p[8..]);
                self.downloaded.fetch_add(data.len() as u64, Ordering::Relaxed);
                let done = {
                    let mut g = self.lock();
                    let d = g.on_block(pid, idx, begin, data);
                    g.fill(pid);
                    d
                };
                if let Some((idx, data)) = done {
                    self.finish_piece(idx, data).await;
                }
            }
            20 => return self.handle_ext(pid, p),
            _ => {}
        }
        Some(())
    }

    /// Mensajes de extensión: handshake y ut_metadata (BEP 9/10).
    fn handle_ext(self: &Arc<Self>, pid: u64, p: &[u8]) -> Option<()> {
        let (&ext_id, body) = p.split_first()?;
        let mut g = self.lock();
        if ext_id == 0 {
            let d = bencode::decode(body).ok()?;
            let m = d.get("m").and_then(|m| m.get("ut_metadata")).and_then(|v| v.as_int()).unwrap_or(0);
            let size = d.get("metadata_size").and_then(|v| v.as_int()).unwrap_or(0) as usize;
            g.peers.get_mut(&pid)?.ut_metadata = m.clamp(0, 255) as u8;
            if g.meta.is_none() && m > 0 && size > 0 && size <= (16 << 20) {
                if g.meta_asm.is_none() {
                    g.meta_asm = Some(MetaAsm { size, pieces: vec![None; (size + 16383) / 16384] });
                }
                let asm = g.meta_asm.as_ref()?;
                let peer = g.peers.get(&pid)?;
                for (i, piece) in asm.pieces.iter().enumerate() {
                    if piece.is_none() {
                        let req = bencode::dict(vec![("msg_type", Value::Int(0)), ("piece", Value::Int(i as i64))]);
                        let _ = peer.tx.send(ext_frame(peer.ut_metadata, &req, &[]));
                    }
                }
            }
            return Some(());
        }
        if ext_id != 1 {
            return Some(());
        }
        let (d, end) = bencode::decode_prefix(body).ok()?;
        let msg_type = d.get("msg_type")?.as_int()?;
        let piece = d.get("piece")?.as_int()? as usize;
        match msg_type {
            0 => {
                let their = g.peers.get(&pid)?.ut_metadata;
                let reply = match &g.meta {
                    Some(m) if piece * 16384 < m.info_raw.len() => {
                        let raw = &m.info_raw;
                        let chunk = &raw[piece * 16384..(piece * 16384 + 16384).min(raw.len())];
                        let hd = bencode::dict(vec![
                            ("msg_type", Value::Int(1)),
                            ("piece", Value::Int(piece as i64)),
                            ("total_size", Value::Int(raw.len() as i64)),
                        ]);
                        ext_frame(their, &hd, chunk)
                    }
                    _ => ext_frame(their, &bencode::dict(vec![("msg_type", Value::Int(2)), ("piece", Value::Int(piece as i64))]), &[]),
                };
                if their != 0 {
                    let _ = g.peers.get(&pid)?.tx.send(reply);
                }
            }
            1 => {
                let data = &body[end..];
                let asm = g.meta_asm.as_mut()?;
                let expect = 16384.min(asm.size.saturating_sub(piece * 16384));
                if piece >= asm.pieces.len() || data.len() != expect {
                    return Some(());
                }
                asm.pieces[piece] = Some(data.to_vec());
                if asm.pieces.iter().all(|x| x.is_some()) {
                    let raw: Vec<u8> = asm.pieces.iter().flatten().flatten().copied().collect();
                    g.meta_asm = None;
                    if Sha1::digest(&raw).as_slice() == self.info_hash {
                        if let Ok(meta) = Meta::from_info(raw) {
                            g.set_meta(Arc::new(meta));
                        }
                    }
                }
            }
            _ => {}
        }
        Some(())
    }
}

#[cfg(test)]
mod network_tests {
    use super::*;
    use crate::network::Config;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn changing_interface_closes_existing_peers_and_keeps_pause_state() {
        let network = Network::new(Config::default());
        let ctx = Arc::new(Ctx {
            network: network.clone(), peer_id: [7; 20], port: AtomicU16::new(0),
            down: Limiter::new(), up: Limiter::new(), encryption: AtomicU8::new(0),
        });
        // Private empty metadata keeps this test entirely on loopback (no DHT/DNS).
        let meta = Meta {
            info_raw: Vec::new(), info_hash: [8; 20], name: "network-test".into(),
            piece_length: 16384, hashes: Vec::new(), files: Vec::new(),
            multi: false, private: true, length: 0,
        };
        let torrent = Torrent::new(ctx, [8; 20], Some(meta), "network-test".into(), Vec::new(),
            PathBuf::from("."), true, HashSet::new());
        torrent.lock().checked = true;
        torrent.start();
        timeout(Duration::from_secs(2), async {
            while torrent.incoming_tx.lock().unwrap().is_none() { tokio::task::yield_now().await; }
        }).await.unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).await.unwrap();
        let (mut remote, addr) = listener.accept().await.unwrap();
        // Incoming plaintext handshake. The peer writer sends its own handshake first.
        torrent.incoming_tx.lock().unwrap().as_ref().unwrap().send(Incoming {
            stream: client, addr, reserved: [0; 8], crypt: None,
        }).unwrap();
        let mut handshake = [0; 68];
        timeout(Duration::from_secs(2), remote.read_exact(&mut handshake)).await.unwrap().unwrap();
        assert_eq!(&handshake[..20], &HEADER);

        network.configure(Config { interface: "huillin-test-missing-interface".into(), ..Config::default() }).unwrap();
        timeout(Duration::from_secs(2), async {
            while torrent.incoming_tx.lock().unwrap().is_some() { tokio::task::yield_now().await; }
        }).await.unwrap();
        let mut rest = Vec::new();
        timeout(Duration::from_secs(2), remote.read_to_end(&mut rest)).await.unwrap().unwrap();
        assert!(!torrent.lock().paused);
        network.configure(Config::default()).unwrap();
        timeout(Duration::from_secs(2), async {
            while torrent.incoming_tx.lock().unwrap().is_none() { tokio::task::yield_now().await; }
        }).await.unwrap();
        torrent.pause();
        network.configure(Config { interface: "huillin-test-missing-interface".into(), ..Config::default() }).unwrap();
        network.configure(Config::default()).unwrap();
        tokio::task::yield_now().await;
        assert!(torrent.lock().paused);
        assert!(torrent.runner.lock().unwrap().is_none());
    }
}
