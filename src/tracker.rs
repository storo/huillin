//! Anuncios a trackers HTTP y UDP (BEP 15).
use crate::bencode::{self, Value};
use crate::network::Route;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

#[derive(Clone, Copy, PartialEq)]
pub enum Event {
    None,
    Started,
}

impl Event {
    fn code(self) -> u32 {
        match self {
            Event::None => 0,
            Event::Started => 2,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Event::None => "",
            Event::Started => "started",
        }
    }
}

pub struct Stats<'a> {
    pub info_hash: &'a [u8; 20],
    pub peer_id: &'a [u8; 20],
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

/// Formato compacto: 6 bytes por peer IPv4 o 18 bytes por peer IPv6.
fn compact(b: &[u8], v6: bool) -> Vec<SocketAddr> {
    if v6 {
        b.chunks_exact(18)
            .map(|c| {
                let ip: [u8; 16] = c[..16].try_into().unwrap();
                SocketAddr::from((Ipv6Addr::from(ip), u16::from_be_bytes([c[16], c[17]])))
            })
            .collect()
    } else {
        b.chunks_exact(6)
            .map(|c| SocketAddr::from((Ipv4Addr::new(c[0], c[1], c[2], c[3]), u16::from_be_bytes([c[4], c[5]]))))
            .collect()
    }
}

fn pct(b: &[u8]) -> String {
    b.iter()
        .map(|&c| {
            if c.is_ascii_alphanumeric() || b"-._~".contains(&c) {
                (c as char).to_string()
            } else {
                format!("%{:02X}", c)
            }
        })
        .collect()
}

/// Devuelve (intervalo en segundos, peers).
pub async fn announce(url: &str, s: &Stats<'_>, route: &Route) -> Result<(u32, Vec<SocketAddr>), String> {
    if let Some(rest) = url.strip_prefix("udp://") {
        udp(rest, s, route).await
    } else if let Some(rest) = url.strip_prefix("http://") {
        http(rest, s, route).await
    } else {
        Err(format!("tracker no soportado: {url}"))
    }
}

async fn udp(rest: &str, s: &Stats<'_>, route: &Route) -> Result<(u32, Vec<SocketAddr>), String> {
    let hostport = rest.split('/').next().unwrap_or(rest);
    let addr = route.resolve(hostport)
        .await
        .map_err(|e| e.to_string())?
        .into_iter().min_by_key(|a| a.is_ipv6()) // prefiere IPv4 si el host tiene ambas
        .ok_or("host sin direcciones")?;
    let v6 = addr.is_ipv6();
    let sock = route.udp(v6).map_err(|e| e.to_string())?;
    sock.connect(addr).await.map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 8192];

    let tid: u32 = rand::random();
    let mut req = Vec::new();
    req.extend(0x41727101980u64.to_be_bytes());
    req.extend(0u32.to_be_bytes());
    req.extend(tid.to_be_bytes());
    sock.send(&req).await.map_err(|e| e.to_string())?;
    let n = timeout(Duration::from_secs(8), sock.recv(&mut buf))
        .await
        .map_err(|_| "timeout")?
        .map_err(|e| e.to_string())?;
    if n < 16 || buf[0..4] != [0, 0, 0, 0] || buf[4..8] != tid.to_be_bytes() {
        return Err("respuesta connect inválida".into());
    }
    let conn = u64::from_be_bytes(buf[8..16].try_into().unwrap());

    let tid: u32 = rand::random();
    let mut req = Vec::new();
    req.extend(conn.to_be_bytes());
    req.extend(1u32.to_be_bytes());
    req.extend(tid.to_be_bytes());
    req.extend(s.info_hash);
    req.extend(s.peer_id);
    req.extend(s.downloaded.to_be_bytes());
    req.extend(s.left.to_be_bytes());
    req.extend(s.uploaded.to_be_bytes());
    req.extend(s.event.code().to_be_bytes());
    req.extend(0u32.to_be_bytes());
    req.extend(rand::random::<u32>().to_be_bytes());
    req.extend((-1i32).to_be_bytes());
    req.extend(s.port.to_be_bytes());
    sock.send(&req).await.map_err(|e| e.to_string())?;
    let n = timeout(Duration::from_secs(8), sock.recv(&mut buf))
        .await
        .map_err(|_| "timeout")?
        .map_err(|e| e.to_string())?;
    if n < 20 || buf[0..4] != [0, 0, 0, 1] || buf[4..8] != tid.to_be_bytes() {
        return Err("respuesta announce inválida".into());
    }
    let interval = u32::from_be_bytes(buf[8..12].try_into().unwrap());
    Ok((interval, compact(&buf[20..n], v6)))
}

async fn http(rest: &str, s: &Stats<'_>, route: &Route) -> Result<(u32, Vec<SocketAddr>), String> {
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let sep = if path.contains('?') { '&' } else { '?' };
    let mut query = format!(
        "info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=80",
        pct(s.info_hash),
        pct(s.peer_id),
        s.port,
        s.uploaded,
        s.downloaded,
        s.left
    );
    if s.event != Event::None {
        query.push_str(&format!("&event={}", s.event.name()));
    }
    let target = if hostport.starts_with('[') && hostport.ends_with(']') || !hostport.contains(':') { format!("{hostport}:80") } else { hostport.to_string() };
    let body = timeout(Duration::from_secs(20), async {
        let mut connected = None;
        for addr in route.resolve(&target).await.map_err(|e| e.to_string())? {
            if let Ok(stream) = route.connect(addr).await { connected = Some(stream); break; }
        }
        let mut c = connected.ok_or("No se puede conectar al tracker mediante la interfaz seleccionada")?;
        let req = format!(
            "GET {path}{sep}{query} HTTP/1.0\r\nHost: {hostport}\r\nUser-Agent: Huillin/0.1\r\nConnection: close\r\n\r\n"
        );
        c.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut resp = Vec::new();
        c.take(2 << 20).read_to_end(&mut resp).await.map_err(|e| e.to_string())?;
        Ok::<_, String>(resp)
    })
    .await
    .map_err(|_| "timeout")??;
    let split = body.windows(4).position(|w| w == b"\r\n\r\n").ok_or("respuesta HTTP inválida")?;
    let head = String::from_utf8_lossy(&body[..split]);
    if !head.lines().next().unwrap_or("").contains(" 200") {
        return Err(format!("HTTP: {}", head.lines().next().unwrap_or("")));
    }
    let v = bencode::decode(&body[split + 4..])?;
    if let Some(f) = v.get("failure reason") {
        return Err(f.as_str().unwrap_or_default());
    }
    let interval = v.get("interval").and_then(|i| i.as_int()).unwrap_or(1800) as u32;
    let mut peers6 = match v.get("peers6") {
        Some(Value::Bytes(b)) => compact(b, true),
        _ => Vec::new(),
    };
    let mut peers = match v.get("peers") {
        Some(Value::Bytes(b)) => compact(b, false),
        Some(Value::List(l)) => l
            .iter()
            .filter_map(|p| format!("{}:{}", p.get("ip")?.as_str()?, p.get("port")?.as_int()?).parse().ok())
            .collect(),
        _ => Vec::new(),
    };
    peers.append(&mut peers6);
    Ok((interval, peers))
}
