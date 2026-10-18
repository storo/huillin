//! Cliente DHT mínimo (BEP 5, con IPv6 según BEP 32): solo búsquedas `get_peers`; no responde consultas.
//! Se usa una instancia por familia de direcciones (cada socket habla solo IPv4 o solo IPv6).
use crate::bencode::{self, Value};
use crate::network::{AbortOnDrop, Route};
use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::UdpSocket;
use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio::task::JoinSet;

const BOOTSTRAP: [&str; 4] =
    ["router.bittorrent.com:6881", "router.utorrent.com:6881", "dht.transmissionbt.com:6881", "dht.libtorrent.org:25401"];

pub struct Dht {
    sock: UdpSocket,
    v6: bool,
    id: [u8; 20],
    counter: AtomicU16,
    pending: Mutex<HashMap<[u8; 2], oneshot::Sender<Value>>>,
    route: Arc<Route>,
}

fn parse_peer(b: &[u8]) -> Option<SocketAddr> {
    match b.len() {
        6 => Some(SocketAddr::from((Ipv4Addr::new(b[0], b[1], b[2], b[3]), u16::from_be_bytes([b[4], b[5]])))),
        18 => {
            let ip: [u8; 16] = b[..16].try_into().ok()?;
            Some(SocketAddr::from((Ipv6Addr::from(ip), u16::from_be_bytes([b[16], b[17]]))))
        }
        _ => None,
    }
}

impl Dht {
    pub async fn start(v6: bool, route: Arc<Route>) -> std::io::Result<(Arc<Dht>, AbortOnDrop)> {
        let sock = route.udp(v6)?;
        let d = Arc::new(Dht { sock, v6, id: rand::random(), counter: AtomicU16::new(0), pending: Mutex::new(HashMap::new()), route });
        let rx = d.clone();
        let receiver = tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                match rx.sock.recv_from(&mut buf).await {
                    Ok((n, _)) => {
                        let Ok(msg) = bencode::decode(&buf[..n]) else { continue };
                        let Some(t) = msg.get("t").and_then(|t| t.as_bytes()) else { continue };
                        let Ok(t) = <[u8; 2]>::try_from(t) else { continue };
                        if msg.get("y").and_then(|y| y.as_bytes()) != Some(b"r") {
                            continue;
                        }
                        if let (Some(tx), Some(r)) = (rx.pending.lock().unwrap().remove(&t), msg.get("r")) {
                            let _ = tx.send(r.clone());
                        }
                    }
                    // En Windows un ICMP "puerto inaccesible" aparece como error de recepción.
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        });
        Ok((d, AbortOnDrop(receiver)))
    }

    async fn query(&self, addr: SocketAddr, info_hash: &[u8; 20]) -> Option<Value> {
        let tid = self.counter.fetch_add(1, Ordering::Relaxed).to_be_bytes();
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(tid, tx);
        let msg = bencode::dict(vec![
            ("t", Value::Bytes(tid.to_vec())),
            ("y", bencode::bytes("q")),
            ("q", bencode::bytes("get_peers")),
            (
                "a",
                bencode::dict(vec![("id", Value::Bytes(self.id.to_vec())), ("info_hash", Value::Bytes(info_hash.to_vec()))]),
            ),
        ]);
        let _ = self.sock.send_to(&bencode::encode(&msg), addr).await;
        let r = tokio::time::timeout(Duration::from_secs(3), rx).await.ok().and_then(|r| r.ok());
        self.pending.lock().unwrap().remove(&tid);
        r
    }

    /// Búsqueda iterativa: envía por `out` los peers que va encontrando.
    pub async fn get_peers(self: &Arc<Self>, info_hash: [u8; 20], out: UnboundedSender<SocketAddr>, duration: Duration) {
        let mut nodes: HashMap<SocketAddr, [u8; 20]> = HashMap::new();
        for host in BOOTSTRAP {
            if let Ok(addrs) = self.route.resolve(host).await {
                for a in addrs.into_iter().filter(|a| a.is_ipv6() == self.v6) {
                    nodes.insert(a, [0xff; 20]);
                }
            }
        }
        let (nodes_key, node_len) = if self.v6 { ("nodes6", 38) } else { ("nodes", 26) };
        let mut queried: HashSet<SocketAddr> = HashSet::new();
        let end = Instant::now() + duration;
        while Instant::now() < end && queried.len() < 300 {
            let mut todo: Vec<(SocketAddr, [u8; 20])> =
                nodes.iter().filter(|(a, _)| !queried.contains(*a)).map(|(a, d)| (*a, *d)).collect();
            todo.sort_by_key(|x| x.1);
            todo.truncate(8);
            if todo.is_empty() {
                break;
            }
            let mut set = JoinSet::new();
            for (addr, _) in todo {
                queried.insert(addr);
                let d = self.clone();
                set.spawn(async move { d.query(addr, &info_hash).await });
            }
            while let Some(res) = set.join_next().await {
                let Ok(Some(r)) = res else { continue };
                if let Some(vals) = r.get("values").and_then(|v| v.as_list()) {
                    for addr in vals.iter().filter_map(|v| v.as_bytes()).filter_map(parse_peer) {
                        let _ = out.send(addr);
                    }
                }
                if let Some(n) = r.get(nodes_key).and_then(|n| n.as_bytes()) {
                    for c in n.chunks_exact(node_len) {
                        let Some(addr) = parse_peer(&c[20..]) else { continue };
                        if addr.port() == 0 {
                            continue;
                        }
                        let mut dist = [0u8; 20];
                        for i in 0..20 {
                            dist[i] = c[i] ^ info_hash[i];
                        }
                        nodes.entry(addr).or_insert(dist);
                    }
                }
            }
        }
    }
}
