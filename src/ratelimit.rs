//! Limitador de ancho de banda (cubo de fichas) compartido por toda la sesión.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct Limiter {
    rate: AtomicU64, // bytes por segundo; 0 = sin límite
    state: Mutex<(f64, Instant)>,
}

impl Limiter {
    pub fn new() -> Self {
        Limiter { rate: AtomicU64::new(0), state: Mutex::new((0.0, Instant::now())) }
    }

    pub fn set(&self, bytes_per_sec: u64) {
        self.rate.store(bytes_per_sec, Ordering::Relaxed);
    }

    pub fn get(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Espera hasta poder transferir `n` bytes respetando el límite.
    pub async fn acquire(&self, n: usize) {
        let rate = self.get();
        if rate == 0 {
            return;
        }
        let wait = {
            let mut s = self.state.lock().unwrap();
            let now = Instant::now();
            let refill = now.duration_since(s.1).as_secs_f64() * rate as f64;
            s.0 = (s.0 + refill).min(rate as f64); // ráfaga máxima: 1 segundo
            s.1 = now;
            s.0 -= n as f64; // puede quedar en deuda; cada llamada espera la suya
            if s.0 >= 0.0 { 0.0 } else { -s.0 / rate as f64 }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}
