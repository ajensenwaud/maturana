//! Embedded static assets: the whole frontend ships inside the binary
//! (single-binary deploy, no Node toolchain at build or run time).

use axum::extract::Path as UrlPath;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use include_dir::{include_dir, Dir};

static ASSETS: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets");

pub async fn index() -> Response {
    file_response("index.html")
}

pub async fn login_page() -> Response {
    file_response("login.html")
}

pub async fn asset(UrlPath(path): UrlPath<String>) -> Response {
    file_response(&path)
}

fn file_response(path: &str) -> Response {
    // Dev ergonomics: in DEBUG builds, serve assets live from the on-disk crate
    // dir so frontend edits show on reload without recompiling the binary. The
    // release build (e.g. aidev) is unaffected — debug_assertions is off, so this
    // block compiles out and assets are always served from the embedded copy.
    #[cfg(debug_assertions)]
    if !path.contains("..") {
        let disk = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets")
            .join(path);
        if let Ok(bytes) = std::fs::read(&disk) {
            return (
                [
                    (header::CONTENT_TYPE, content_type(path)),
                    (header::CACHE_CONTROL, "no-store"),
                ],
                bytes,
            )
                .into_response();
        }
    }
    let Some(file) = ASSETS.get_file(path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    (
        [
            (header::CONTENT_TYPE, content_type(path)),
            // Single binary, no asset fingerprinting: always revalidate.
            (header::CACHE_CONTROL, "no-store"),
        ],
        file.contents(),
    )
        .into_response()
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, ext)| ext) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("woff2") => "font/woff2",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_shell_assets_exist() {
        for required in [
            "index.html",
            "login.html",
            "css/tokens.css",
            "css/app.css",
            "js/app.js",
            "js/ws.js",
        ] {
            assert!(
                ASSETS.get_file(required).is_some(),
                "missing embedded asset {required}"
            );
        }
    }

    #[test]
    fn content_types_map() {
        assert_eq!(content_type("css/tokens.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("js/ws.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("unknown.bin"), "application/octet-stream");
    }
}
