//! Lectura/escritura de piezas sobre los archivos del torrent.
use crate::meta::Meta;
use sha1::{Digest, Sha1};
use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(f, buf, off)
}
#[cfg(windows)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(f, buf, off)
}
#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(f, buf, off)
}
#[cfg(unix)]
fn write_at(f: &File, buf: &[u8], off: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(f, buf, off)
}

pub struct Storage {
    meta: Arc<Meta>,
    root: PathBuf,
    handles: Mutex<Vec<Option<Arc<File>>>>,
}

impl Storage {
    pub fn new(meta: Arc<Meta>, root: &Path) -> Storage {
        let n = meta.files.len();
        Storage { meta, root: root.to_path_buf(), handles: Mutex::new(vec![None; n]) }
    }

    pub fn path(&self, i: usize) -> PathBuf {
        let mut p = self.root.clone();
        if self.meta.multi {
            p.push(&self.meta.name);
        }
        for c in &self.meta.files[i].path {
            p.push(c);
        }
        p
    }

    fn file(&self, i: usize, create: bool) -> io::Result<Arc<File>> {
        let mut h = self.handles.lock().unwrap();
        if let Some(f) = &h[i] {
            return Ok(f.clone());
        }
        let path = self.path(i);
        let f = if create {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?
        } else {
            OpenOptions::new().read(true).open(&path)?
        };
        let f = Arc::new(f);
        if create {
            h[i] = Some(f.clone());
        }
        Ok(f)
    }

    /// (índice de archivo, offset en el archivo, longitud) que cubren [pos, pos+len)
    fn spans(&self, mut pos: u64, mut len: u64) -> Vec<(usize, u64, usize)> {
        let files = &self.meta.files;
        let mut i = files.partition_point(|f| f.offset + f.length <= pos);
        let mut out = Vec::new();
        while len > 0 && i < files.len() {
            let f = &files[i];
            let off = pos - f.offset;
            let n = (f.length - off).min(len);
            if n > 0 {
                out.push((i, off, n as usize));
            }
            pos += n;
            len -= n;
            i += 1;
        }
        out
    }

    pub fn write(&self, pos: u64, data: &[u8]) -> io::Result<()> {
        let mut done = 0;
        for (i, off, n) in self.spans(pos, data.len() as u64) {
            let f = self.file(i, true)?;
            let mut chunk = &data[done..done + n];
            let mut o = off;
            while !chunk.is_empty() {
                let w = write_at(&f, chunk, o)?;
                chunk = &chunk[w..];
                o += w as u64;
            }
            done += n;
        }
        Ok(())
    }

    /// None si falta algún archivo o está incompleto.
    pub fn read(&self, pos: u64, len: usize) -> Option<Vec<u8>> {
        let mut out = vec![0u8; len];
        let mut done = 0;
        for (i, off, n) in self.spans(pos, len as u64) {
            let f = self.file(i, false).ok()?;
            let mut got = 0;
            while got < n {
                let r = read_at(&f, &mut out[done + got..done + n], off + got as u64).ok()?;
                if r == 0 {
                    return None;
                }
                got += r;
            }
            done += n;
        }
        (done == len).then_some(out)
    }

    pub fn any_exists(&self) -> bool {
        (0..self.meta.files.len()).any(|i| self.path(i).exists())
    }

    /// Verifica todas las piezas contra sus hashes (bloqueante).
    pub fn check(&self, progress: &AtomicUsize) -> Vec<bool> {
        let n = self.meta.hashes.len();
        progress.store(0, Ordering::Relaxed);
        if !self.any_exists() {
            return vec![false; n];
        }
        (0..n)
            .map(|i| {
                let ok = self
                    .read(i as u64 * self.meta.piece_length, self.meta.piece_size(i) as usize)
                    .map(|d| Sha1::digest(&d).as_slice() == self.meta.hashes[i])
                    .unwrap_or(false);
                progress.store(i + 1, Ordering::Relaxed);
                ok
            })
            .collect()
    }

    pub fn delete_files(&self) {
        self.handles.lock().unwrap().iter_mut().for_each(|h| *h = None);
        for i in 0..self.meta.files.len() {
            let p = self.path(i);
            let _ = std::fs::remove_file(&p);
            let mut dir = p.parent().map(|d| d.to_path_buf());
            while let Some(d) = dir {
                if d == self.root || std::fs::remove_dir(&d).is_err() {
                    break;
                }
                dir = d.parent().map(|d| d.to_path_buf());
            }
        }
    }
}
