//! Cifrado de protocolo BitTorrent (Message Stream Encryption / Protocol Encryption).
//! Intercambio Diffie-Hellman de 768 bits y flujo RC4 sobre toda la conexión.
use num_bigint::BigUint;
use sha1::{Digest, Sha1};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

const P_HEX: &str = "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485B576625E7EC6F44C42E9A63A36210000000000090563";
const MAX_PAD: usize = 512;

#[derive(Clone)]
pub struct Rc4 {
    s: [u8; 256],
    i: u8,
    j: u8,
}

impl Rc4 {
    /// RC4 descartando los primeros 1024 bytes del keystream, como exige MSE.
    pub fn new(key: &[u8]) -> Rc4 {
        let mut s = [0u8; 256];
        for (i, v) in s.iter_mut().enumerate() {
            *v = i as u8;
        }
        let mut j = 0u8;
        for i in 0..256 {
            j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
            s.swap(i, j as usize);
        }
        let mut r = Rc4 { s, i: 0, j: 0 };
        r.apply(&mut [0u8; 1024]);
        r
    }

    pub fn apply(&mut self, buf: &mut [u8]) {
        for b in buf {
            self.i = self.i.wrapping_add(1);
            self.j = self.j.wrapping_add(self.s[self.i as usize]);
            self.s.swap(self.i as usize, self.j as usize);
            let k = self.s[self.s[self.i as usize].wrapping_add(self.s[self.j as usize]) as usize];
            *b ^= k;
        }
    }
}

/// Estados de cifrado de cada dirección de una conexión.
pub struct Crypt {
    pub enc: Rc4,
    pub dec: Rc4,
}

fn sha1(parts: &[&[u8]]) -> [u8; 20] {
    let mut h = Sha1::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

fn to96(x: &BigUint) -> [u8; 96] {
    let b = x.to_bytes_be();
    let mut out = [0u8; 96];
    out[96 - b.len()..].copy_from_slice(&b);
    out
}

fn keypair() -> (BigUint, [u8; 96]) {
    let p = BigUint::parse_bytes(P_HEX.as_bytes(), 16).unwrap();
    let x = BigUint::from_bytes_be(&rand::random::<[u8; 20]>());
    let y = BigUint::from(2u32).modpow(&x, &p);
    (x, to96(&y))
}

fn shared(x: &BigUint, their: &[u8; 96]) -> Result<[u8; 96], String> {
    let p = BigUint::parse_bytes(P_HEX.as_bytes(), 16).unwrap();
    let y = BigUint::from_bytes_be(their);
    if y <= BigUint::from(1u32) || y >= &p - 1u32 {
        return Err("clave pública inválida".into());
    }
    Ok(to96(&y.modpow(x, &p)))
}

fn random_pad() -> Vec<u8> {
    let n = rand::random_range(0..=MAX_PAD);
    (0..n).map(|_| rand::random()).collect()
}

async fn read_exact<S: AsyncRead + Unpin>(s: &mut S, buf: &mut [u8]) -> Result<(), String> {
    timeout(Duration::from_secs(10), s.read_exact(buf))
        .await
        .map_err(|_| "timeout".to_string())?
        .map_err(|e| e.to_string())
        .map(|_| ())
}

/// Lee byte a byte hasta encontrar `pattern` (máximo `max` bytes).
async fn scan<S: AsyncRead + Unpin>(s: &mut S, pattern: &[u8], max: usize) -> Result<(), String> {
    let mut window: Vec<u8> = Vec::with_capacity(pattern.len() + 1);
    for _ in 0..max {
        let mut b = [0u8; 1];
        read_exact(s, &mut b).await?;
        window.push(b[0]);
        if window.len() > pattern.len() {
            window.remove(0);
        }
        if window == pattern {
            return Ok(());
        }
    }
    Err("sincronización MSE no encontrada".into())
}

/// Lado que inicia la conexión (A).
pub async fn initiate<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S, ih: &[u8; 20]) -> Result<Crypt, String> {
    let (x, ya) = keypair();
    let mut out = ya.to_vec();
    out.extend(random_pad());
    s.write_all(&out).await.map_err(|e| e.to_string())?;
    let mut yb = [0u8; 96];
    read_exact(s, &mut yb).await?;
    let secret = shared(&x, &yb)?;

    let mut enc = Rc4::new(&sha1(&[b"keyA", &secret, ih]));
    let mut dec = Rc4::new(&sha1(&[b"keyB", &secret, ih]));
    let mut msg = sha1(&[b"req1", &secret]).to_vec();
    let (r2, r3) = (sha1(&[b"req2", ih]), sha1(&[b"req3", &secret]));
    msg.extend((0..20).map(|i| r2[i] ^ r3[i]));
    let mut part = vec![0u8; 8]; // VC
    part.extend(2u32.to_be_bytes()); // crypto_provide: RC4
    part.extend(0u16.to_be_bytes()); // len(PadC)
    part.extend(0u16.to_be_bytes()); // len(IA)
    enc.apply(&mut part);
    msg.extend(part);
    s.write_all(&msg).await.map_err(|e| e.to_string())?;

    let mut pat = [0u8; 8];
    dec.clone().apply(&mut pat);
    scan(s, &pat, MAX_PAD + 8).await?;
    dec.apply(&mut [0u8; 8]);
    let mut hdr = [0u8; 6];
    read_exact(s, &mut hdr).await?;
    dec.apply(&mut hdr);
    if u32::from_be_bytes(hdr[..4].try_into().unwrap()) & 2 == 0 {
        return Err("el peer no eligió RC4".into());
    }
    let pad = u16::from_be_bytes([hdr[4], hdr[5]]) as usize;
    if pad > MAX_PAD {
        return Err("relleno demasiado grande".into());
    }
    let mut padbuf = vec![0u8; pad];
    read_exact(s, &mut padbuf).await?;
    dec.apply(&mut padbuf);
    Ok(Crypt { enc, dec })
}

/// Lado que recibe la conexión (B). Devuelve el cifrado, el info-hash elegido y los datos iniciales (IA).
pub async fn respond<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    hashes: &[[u8; 20]],
) -> Result<(Crypt, [u8; 20], Vec<u8>), String> {
    let mut ya = [0u8; 96];
    read_exact(s, &mut ya).await?;
    let (y, yb) = keypair();
    let mut out = yb.to_vec();
    out.extend(random_pad());
    s.write_all(&out).await.map_err(|e| e.to_string())?;
    let secret = shared(&y, &ya)?;

    scan(s, &sha1(&[b"req1", &secret]), MAX_PAD + 20).await?;
    let mut x = [0u8; 20];
    read_exact(s, &mut x).await?;
    let r3 = sha1(&[b"req3", &secret]);
    let skey = *hashes
        .iter()
        .find(|ih| {
            let r2 = sha1(&[b"req2", *ih]);
            (0..20).all(|i| r2[i] ^ r3[i] == x[i])
        })
        .ok_or("torrent desconocido")?;

    let mut dec = Rc4::new(&sha1(&[b"keyA", &secret, &skey]));
    let mut enc = Rc4::new(&sha1(&[b"keyB", &secret, &skey]));
    let mut h = [0u8; 14];
    read_exact(s, &mut h).await?;
    dec.apply(&mut h);
    if h[..8] != [0u8; 8] {
        return Err("VC inválido".into());
    }
    if u32::from_be_bytes(h[8..12].try_into().unwrap()) & 2 == 0 {
        return Err("el peer no ofrece RC4".into());
    }
    let padc = u16::from_be_bytes([h[12], h[13]]) as usize;
    if padc > MAX_PAD {
        return Err("relleno demasiado grande".into());
    }
    let mut pad = vec![0u8; padc + 2];
    read_exact(s, &mut pad).await?;
    dec.apply(&mut pad);
    let ia_len = u16::from_be_bytes([pad[padc], pad[padc + 1]]) as usize;
    let mut ia = vec![0u8; ia_len];
    read_exact(s, &mut ia).await?;
    dec.apply(&mut ia);

    let mut reply = vec![0u8; 8];
    reply.extend(2u32.to_be_bytes());
    reply.extend(0u16.to_be_bytes());
    enc.apply(&mut reply);
    s.write_all(&reply).await.map_err(|e| e.to_string())?;
    Ok((Crypt { enc, dec }, skey, ia))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prime_is_768_bits() {
        let p = BigUint::parse_bytes(P_HEX.as_bytes(), 16).unwrap();
        assert_eq!(p.bits(), 768);
    }

    #[test]
    fn rc4_roundtrip() {
        let mut a = Rc4::new(b"clave");
        let mut b = Rc4::new(b"clave");
        let mut data = b"hola mundo".to_vec();
        a.apply(&mut data);
        assert_ne!(&data, b"hola mundo");
        b.apply(&mut data);
        assert_eq!(&data, b"hola mundo");
    }

    #[tokio::test]
    async fn handshake_between_both_sides() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        let ih = [7u8; 20];
        let hashes = vec![[1u8; 20], ih, [9u8; 20]];
        let (ra, rb) = tokio::join!(initiate(&mut a, &ih), respond(&mut b, &hashes));
        let mut ca = ra.unwrap();
        let (mut cb, skey, ia) = rb.unwrap();
        assert_eq!(skey, ih);
        assert!(ia.is_empty());
        // A -> B
        let mut m = b"BitTorrent protocol A->B".to_vec();
        ca.enc.apply(&mut m);
        a.write_all(&m).await.unwrap();
        let mut got = vec![0u8; m.len()];
        b.read_exact(&mut got).await.unwrap();
        cb.dec.apply(&mut got);
        assert_eq!(&got, b"BitTorrent protocol A->B");
        // B -> A
        let mut m = b"respuesta B->A".to_vec();
        cb.enc.apply(&mut m);
        b.write_all(&m).await.unwrap();
        let mut got = vec![0u8; m.len()];
        a.read_exact(&mut got).await.unwrap();
        ca.dec.apply(&mut got);
        assert_eq!(&got, b"respuesta B->A");
    }

    #[tokio::test]
    async fn rejects_unknown_torrent() {
        let (mut a, mut b) = tokio::io::duplex(1 << 16);
        tokio::spawn(async move {
            let _ = initiate(&mut a, &[7u8; 20]).await;
        });
        assert!(respond(&mut b, &[[1u8; 20]]).await.is_err());
    }
}
