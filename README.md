# Huillín

Cliente BitTorrent ligero escrito en Rust. Se ejecuta como app de escritorio (ventana nativa con WebView2) y también puede usarse como panel web local.

## Características

- Archivos `.torrent` y enlaces magnet
- DHT y trackers
- Cifrado de protocolo (MSE)
- Límite de velocidad
- Detección de magnets copiados al portapapeles (Windows)
- Ventana nativa de escritorio (WebView2) con icono propio y sin consola
- Bandeja del sistema: cerrar la ventana la oculta y las descargas siguen; clic en el icono la muestra, menú "Salir" cierra de verdad
- Instancia única: abrir un `.torrent` o magnet con la app ya abierta lo añade a esa instancia
- Asociación de `.torrent` y `magnet:` (por usuario): `huillin --register` / `huillin --unregister`
- Modo web opcional: `huillin --web` (o `HUILLIN_WEB=1`) abre el panel en `http://127.0.0.1:8088` (o el siguiente puerto libre hasta 8119)

## Requisitos

- [Rust](https://www.rust-lang.org/tools/install) (edición 2021)
- Windows 10/11 con WebView2 Runtime (incluido en Windows 11)

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
