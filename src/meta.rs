//! Metadatos de torrents y enlaces magnet.
use crate::bencode::{self, Value};
use sha1::{Digest, Sha1};

pub struct FileEntry {
    pub path: Vec<String>,
    pub length: u64,
    pub offset: u64,
}

pub struct Meta {
    pub info_raw: Vec<u8>,
    pub info_hash: [u8; 20],
    pub name: String,
    pub piece_length: u64,
    pub hashes: Vec<[u8; 20]>,
    pub files: Vec<FileEntry>,
    pub multi: bool,
    pub private: bool,
    pub length: u64,
}

fn clean(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) || (c as u32) < 32 { '_' } else { c })
        .collect();
    let s = s.trim_matches(|c| c == ' ' || c == '.').to_string();
    if s.is_empty() {
        "_".into()
    } else {
        s
    }
}

impl Meta {
    pub fn from_info(info_raw: Vec<u8>) -> Result<Meta, String> {
        let info = bencode::decode(&info_raw)?;
        let info_hash: [u8; 20] = Sha1::digest(&info_raw).into();
        let name = clean(&info.get("name").and_then(|v| v.as_str()).ok_or("falta 'name'")?);
        let piece_length = info.get("piece length").and_then(|v| v.as_int()).ok_or("falta 'piece length'")? as u64;
        if piece_length == 0 {
            return Err("piece length inválido".into());
        }
        let pieces = info.get("pieces").and_then(|v| v.as_bytes()).ok_or("falta 'pieces'")?;
        if pieces.len() % 20 != 0 {
            return Err("'pieces' inválido".into());
        }
        let hashes: Vec<[u8; 20]> = pieces.chunks(20).map(|c| c.try_into().unwrap()).collect();
        let mut files = Vec::new();
        let mut off = 0u64;
        let multi = info.get("files").is_some();
        if let Some(list) = info.get("files").and_then(|v| v.as_list()) {
            for f in list {
                let length = f.get("length").and_then(|v| v.as_int()).ok_or("archivo sin length")? as u64;
                let path: Vec<String> = f
                    .get("path")
                    .and_then(|v| v.as_list())
                    .ok_or("archivo sin path")?
                    .iter()
                    .map(|p| clean(&p.as_str().unwrap_or_default()))
                    .collect();
                if path.is_empty() {
                    return Err("ruta vacía".into());
                }
                files.push(FileEntry { path, length, offset: off });
                off += length;
            }
        } else {
            let length = info.get("length").and_then(|v| v.as_int()).ok_or("falta 'length'")? as u64;
            files.push(FileEntry { path: vec![name.clone()], length, offset: 0 });
            off = length;
        }
        if hashes.len() as u64 != (off + piece_length - 1) / piece_length {
            return Err("número de piezas inconsistente".into());
        }
        let private = info.get("private").and_then(|v| v.as_int()) == Some(1);
        Ok(Meta { info_raw, info_hash, name, piece_length, hashes, files, multi, private, length: off })
    }

    pub fn piece_size(&self, idx: usize) -> u64 {
        if idx + 1 == self.hashes.len() {
            self.length - idx as u64 * self.piece_length
        } else {
            self.piece_length
        }
    }
}

/// Devuelve el torrent y la lista de trackers.
pub fn parse_torrent(data: &[u8]) -> Result<(Meta, Vec<String>), String> {
    let meta = Meta::from_info(bencode::info_span(data)?.to_vec())?;
    let top = bencode::decode(data)?;
    let mut trackers = Vec::new();
    if let Some(Value::List(tiers)) = top.get("announce-list") {
        for tier in tiers {
            for t in tier.as_list().unwrap_or(&[]) {
                trackers.extend(t.as_str());
            }
        }
    }
    trackers.extend(top.get("announce").and_then(|v| v.as_str()));
    trackers.dedup();
    let mut seen = std::collections::HashSet::new();
    trackers.retain(|t| seen.insert(t.clone()));
    Ok((meta, trackers))
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() && s.is_char_boundary(i + 1) && s.is_char_boundary(i + 3) => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn base32(s: &str) -> Option<Vec<u8>> {
    let mut bits = 0u64;
    let mut n = 0;
    let mut out = Vec::new();
    for c in s.to_ascii_uppercase().bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | v as u64;
        n += 5;
        if n >= 8 {
            out.push((bits >> (n - 8)) as u8);
            n -= 8;
        }
    }
    Some(out)
}

/// (info_hash, nombre, trackers)
pub fn parse_magnet(uri: &str) -> Result<([u8; 20], String, Vec<String>), String> {
    let q = uri.strip_prefix("magnet:?").ok_or("no es un enlace magnet")?;
    let (mut hash, mut name, mut trackers) = (None, String::new(), Vec::new());
    for kv in q.split('&') {
        let Some((k, v)) = kv.split_once('=') else { continue };
        let v = pct_decode(v);
        match k {
            "xt" => {
                if let Some(h) = v.strip_prefix("urn:btih:") {
                    let raw = if h.len() == 40 { hex::decode(h).ok() } else { base32(h) };
                    hash = raw.and_then(|r| <[u8; 20]>::try_from(r).ok());
                }
            }
            "dn" => name = v,
            "tr" => trackers.push(v),
            _ => {}
        }
    }
    Ok((hash.ok_or("magnet sin info-hash válido")?, name, trackers))
}
