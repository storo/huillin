//! Consulta de versiones estables; la instalación sigue siendo manual.
use semver::Version;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DOWNLOAD_URL: &str = "https://github.com/storo/huillin/releases/latest";
const RELEASE_API: &str = "https://api.github.com/repos/storo/huillin/releases/latest";
// Comparte resultados entre ventanas y limita consultas repetidas, incluso con errores.
static CACHE: Mutex<Option<(Instant, Result<UpdateInfo, String>)>> = Mutex::const_new(None);

pub fn open_download_page() -> std::io::Result<()> {
    #[cfg(windows)]
    let program = "explorer.exe";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    std::process::Command::new(program)
        .arg(DOWNLOAD_URL)
        .spawn()
        .map(|_| ())
}

#[derive(Clone, Serialize, Debug)]
pub struct UpdateInfo {
    pub current_version: &'static str,
    pub latest_version: String,
    pub available: bool,
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

fn release_info(release: Release) -> Result<UpdateInfo, String> {
    let latest = Version::parse(release.tag_name.strip_prefix('v').unwrap_or(&release.tag_name))
        .map_err(|_| "GitHub devolvió una versión no reconocida.".to_string())?;
    if release.draft || release.prerelease || !latest.pre.is_empty() {
        return Err("GitHub no devolvió una versión estable publicada.".into());
    }
    let current = Version::parse(CURRENT_VERSION).expect("versión de Cargo válida");
    Ok(UpdateInfo {
        current_version: CURRENT_VERSION,
        available: latest.cmp_precedence(&current).is_gt(),
        latest_version: latest.to_string(),
    })
}

async fn fetch_release(client: &reqwest::Client, url: &str) -> Result<UpdateInfo, String> {
    let response = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|_| "No se pudo consultar GitHub. Revisa tu conexión e inténtalo de nuevo.".to_string())?;
    match response.status() {
        reqwest::StatusCode::NOT_FOUND => return Err("Todavía no hay una versión estable publicada en GitHub.".into()),
        reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::TOO_MANY_REQUESTS => {
            return Err("GitHub limitó las consultas. Inténtalo más tarde.".into());
        }
        status if !status.is_success() => {
            return Err(format!("No se pudo consultar GitHub (HTTP {}).", status.as_u16()))
        }
        _ => {}
    }
    let release = response
        .json::<Release>()
        .await
        .map_err(|_| "No se pudo interpretar la respuesta de GitHub.".to_string())?;
    release_info(release)
}

pub async fn check() -> Result<UpdateInfo, String> {
    let mut cache = CACHE.lock().await;
    if let Some((time, result)) = &*cache {
        if time.elapsed() < Duration::from_secs(60) {
            return result.clone();
        }
    }
    let result = match reqwest::Client::builder()
        .user_agent(concat!("Huillin/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => fetch_release(&client, RELEASE_API).await,
        Err(_) => Err("No se pudo iniciar la consulta de actualizaciones.".into()),
    };
    *cache = Some((Instant::now(), result.clone()));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
        }
    }

    #[test]
    fn compares_numeric_versions_and_ignores_build_metadata() {
        assert!(release_info(release("v0.10.0")).unwrap().available);
        assert!(!release_info(release(CURRENT_VERSION)).unwrap().available);
        assert!(!release_info(release("v0.0.1")).unwrap().available);
        assert!(
            !release_info(release(&format!("{CURRENT_VERSION}+build.2")))
                .unwrap()
                .available
        );
    }

    #[test]
    fn rejects_invalid_and_unpublished_versions() {
        assert!(release_info(release("latest")).is_err());
        assert!(release_info(release("v9.0.0-beta.1")).is_err());
        assert!(release_info(Release {
            draft: true,
            ..release("v9.0.0")
        })
        .is_err());
        assert!(release_info(Release {
            prerelease: true,
            ..release("v9.0.0")
        })
        .is_err());
    }

    #[tokio::test]
    async fn reports_connection_failure() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        drop(listener);
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        assert!(fetch_release(&client, &url).await.unwrap_err().contains("conexión"));
    }

    #[tokio::test]
    async fn handles_github_responses() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for (status, body, expected) in [
            (
                200,
                r#"{"tag_name":"v9.0.0","draft":false,"prerelease":false}"#,
                "available",
            ),
            (200, "invalid JSON", "interpretar"),
            (404, "{}", "Todavía"),
            (403, "{}", "limitó"),
            (429, "{}", "limitó"),
            (500, "{}", "HTTP 500"),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 4096];
                stream.read(&mut request).await.unwrap();
                let response = format!("HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                stream.write_all(response.as_bytes()).await.unwrap();
            });
            let client = reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(2))
                .build()
                .unwrap();
            let result = fetch_release(&client, &url).await;
            if expected == "available" {
                assert!(result.unwrap().available);
            } else {
                assert!(result.unwrap_err().contains(expected));
            }
            server.await.unwrap();
        }
    }
}
