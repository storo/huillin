//! Codificación y decodificación bencode.
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Dict(d) => d.get(key.as_bytes()),
            _ => None,
        }
    }
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_str(&self) -> Option<String> {
        self.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned())
    }
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }
}

pub fn dict(items: Vec<(&str, Value)>) -> Value {
    Value::Dict(items.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect())
}

pub fn bytes(s: &str) -> Value {
    Value::Bytes(s.as_bytes().to_vec())
}

fn dec(d: &[u8], i: usize, depth: usize) -> Result<(Value, usize), String> {
    if depth > 64 {
        return Err("bencode demasiado anidado".into());
    }
    let c = *d.get(i).ok_or("bencode truncado")?;
    match c {
        b'i' => {
            let j = d[i..].iter().position(|&b| b == b'e').ok_or("entero sin cerrar")? + i;
            let n = std::str::from_utf8(&d[i + 1..j])
                .map_err(|e| e.to_string())?
                .parse::<i64>()
                .map_err(|e| e.to_string())?;
            Ok((Value::Int(n), j + 1))
        }
        b'l' => {
            let mut i = i + 1;
            let mut out = Vec::new();
            while *d.get(i).ok_or("lista sin cerrar")? != b'e' {
                let (v, ni) = dec(d, i, depth + 1)?;
                out.push(v);
                i = ni;
            }
            Ok((Value::List(out), i + 1))
        }
        b'd' => {
            let mut i = i + 1;
            let mut out = BTreeMap::new();
            while *d.get(i).ok_or("diccionario sin cerrar")? != b'e' {
                let (k, ni) = dec(d, i, depth + 1)?;
                let Value::Bytes(k) = k else { return Err("clave no es string".into()) };
                let (v, ni) = dec(d, ni, depth + 1)?;
                out.insert(k, v);
                i = ni;
            }
            Ok((Value::Dict(out), i + 1))
        }
        b'0'..=b'9' => {
            let j = d[i..].iter().position(|&b| b == b':').ok_or("string sin ':'")? + i;
            let n: usize = std::str::from_utf8(&d[i..j])
                .map_err(|e| e.to_string())?
                .parse()
                .map_err(|_| "longitud inválida")?;
            let end = j.checked_add(1 + n).filter(|&e| e <= d.len()).ok_or("string truncado")?;
            Ok((Value::Bytes(d[j + 1..end].to_vec()), end))
        }
        _ => Err("bencode inválido".into()),
    }
}

/// Decodifica el primer valor y devuelve también el índice donde termina.
pub fn decode_prefix(d: &[u8]) -> Result<(Value, usize), String> {
    dec(d, 0, 0)
}

pub fn decode(d: &[u8]) -> Result<Value, String> {
    decode_prefix(d).map(|x| x.0)
}

pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    enc(v, &mut out);
    out
}

fn enc(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Int(i) => out.extend(format!("i{}e", i).as_bytes()),
        Value::Bytes(b) => {
            out.extend(format!("{}:", b.len()).as_bytes());
            out.extend(b);
        }
        Value::List(l) => {
            out.push(b'l');
            l.iter().for_each(|x| enc(x, out));
            out.push(b'e');
        }
        Value::Dict(d) => {
            out.push(b'd');
            for (k, x) in d {
                out.extend(format!("{}:", k.len()).as_bytes());
                out.extend(k);
                enc(x, out);
            }
            out.push(b'e');
        }
    }
}

/// Bytes crudos del diccionario `info` de un .torrent (necesarios para el info-hash).
pub fn info_span(d: &[u8]) -> Result<&[u8], String> {
    if d.first() != Some(&b'd') {
        return Err("no es un archivo .torrent".into());
    }
    let mut i = 1;
    while d.get(i) != Some(&b'e') {
        let (k, ni) = dec(d, i, 1)?;
        let start = ni;
        let (_, ni) = dec(d, ni, 1)?;
        if matches!(&k, Value::Bytes(b) if b == b"info") {
            return Ok(&d[start..ni]);
        }
        i = ni;
    }
    Err("el .torrent no contiene 'info'".into())
}
