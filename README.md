# Huillín

Cliente BitTorrent ligero escrito en Rust, con panel web local.

## Características

- Archivos `.torrent` y enlaces magnet
- DHT y trackers
- Cifrado de protocolo (MSE)
- Límite de velocidad
- Detección de magnets copiados al portapapeles (Windows)
- Panel web en `http://127.0.0.1:8088` (o el siguiente puerto libre hasta 8119)

## Requisitos

- [Rust](https://www.rust-lang.org/tools/install) (edición 2021)
- Windows (el portapapeles y la apertura automática del navegador usan APIs de Windows)

## Compilar y ejecutar

```bash
cargo run --release
```

También puedes pasar torrents o magnets como argumentos:

```bash
cargo run --release -- archivo.torrent "magnet:?xt=urn:btih:..."
```

Para que no se abra el navegador automáticamente, define `HUILLIN_NO_BROWSER=1`.

## Licencia

[MIT](LICENSE)
