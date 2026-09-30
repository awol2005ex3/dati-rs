//! Static hosting for the embedded frontend: the Vue SPA shell and the VitePress help center.
//!
//! `build.rs` puts the output of `pnpm run build:all` in `frontend/dist`, and `rust-embed` bakes
//! that folder into the executable, so one binary serves both the API and the UI. The path rules
//! below port Spring's `SpaWebConfig` from the Java backend: a help-center page resolves as
//! `<path>.html` or `<path>/index.html`, any other unknown path gets `index.html` unless it belongs
//! to the API or looks like a file.

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use crate::api;
use crate::state::AppState;

#[derive(RustEmbed)]
#[folder = "../../frontend/dist"]
#[allow_missing]
struct Assets;

/// Route roots owned by the server: an unknown path under one must 404 instead of answering the
/// SPA shell, which would turn a typo into a page that silently renders without data.
const API_ROOTS: [&str; 1] = ["v1"];

const INDEX: &str = "index.html";

/// The whole HTTP surface: the JSON API keeps priority, the embedded frontend answers the rest.
pub fn app_router(state: AppState) -> Router {
    api::router(state).fallback(serve)
}

/// Which embedded file a request path resolves to, in the order the Java resolvers tried them.
#[derive(Debug, PartialEq, Eq)]
enum Candidate {
    /// `/docs` must bounce to `/docs/`, because VitePress serves the site from that base.
    Redirect,
    /// Asset keys to try in order; none of them existing means the request 404s.
    Try(Vec<String>),
}

fn candidates(path: &str) -> Candidate {
    if path == "/docs" {
        return Candidate::Redirect;
    }
    if let Some(rest) = path.strip_prefix("/docs/") {
        let base = if rest.is_empty() {
            "docs".to_owned()
        } else {
            format!("docs/{}", rest.trim_end_matches('/'))
        };
        // Unlike axum, Spring's path pattern drops the trailing slash before the resolver sees it,
        // so the directory case is detected from the raw request path. `/docs/` itself is a
        // directory too, even though nothing is left of it.
        let directory = rest.is_empty() || rest.ends_with('/');
        let leaf = if directory {
            format!("{base}/index.html")
        } else {
            format!("{base}.html")
        };
        return Candidate::Try(vec![base, leaf]);
    }
    let relative = path.trim_start_matches('/');
    let mut keys = vec![relative.to_owned()];
    if !is_api_root(relative) && !relative.contains('.') {
        keys.push(INDEX.to_owned());
    }
    Candidate::Try(keys)
}

fn is_api_root(path: &str) -> bool {
    API_ROOTS.contains(&path.split('/').next().unwrap_or_default())
}

async fn serve(uri: Uri, headers: HeaderMap) -> Response {
    match candidates(uri.path()) {
        Candidate::Redirect => (StatusCode::FOUND, [(header::LOCATION, "/docs/")]).into_response(),
        Candidate::Try(keys) => keys
            .iter()
            .find_map(|key| file_response(key, &headers))
            .unwrap_or_else(|| StatusCode::NOT_FOUND.into_response()),
    }
}

fn file_response(key: &str, headers: &HeaderMap) -> Option<Response> {
    let file = Assets::get(key)?;
    let hash = file.metadata.sha256_hash();
    let etag = format!(
        "\"{}\"",
        hash.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| value.as_bytes() == etag.as_bytes())
    {
        return Some((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }
    Some(
        (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, file.metadata.mimetype().to_owned()),
                (header::ETAG, etag),
            ],
            Body::from(file.data.into_owned()),
        )
            .into_response(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn try_keys(path: &str) -> Vec<String> {
        match candidates(path) {
            Candidate::Redirect => panic!("{path} is not a redirect"),
            Candidate::Try(keys) => keys,
        }
    }

    #[test]
    fn docs_pages_resolve_their_clean_urls() {
        assert_eq!(candidates("/docs"), Candidate::Redirect);
        // The site root has no path segment left to append ".html" to.
        assert_eq!(try_keys("/docs/"), ["docs", "docs/index.html"]);
        assert_eq!(try_keys("/docs/faq"), ["docs/faq", "docs/faq.html"]);
        assert_eq!(try_keys("/docs/en/"), ["docs/en", "docs/en/index.html"]);
        // An asset is tried as itself first, so the clean-URL rule never reaches it.
        assert_eq!(
            try_keys("/docs/hashmap.json"),
            ["docs/hashmap.json", "docs/hashmap.json.html"]
        );
    }

    #[test]
    fn spa_paths_fall_back_to_the_shell() {
        assert_eq!(try_keys("/"), ["", "index.html"]);
        assert_eq!(try_keys("/datasource"), ["datasource", "index.html"]);
        // A deep link the router owns, not the file server.
        assert_eq!(
            try_keys("/mcp-services/42/tools"),
            ["mcp-services/42/tools", "index.html"]
        );
    }

    #[test]
    fn api_and_file_paths_never_get_the_shell() {
        assert_eq!(try_keys("/v1/users"), ["v1/users"]);
        assert_eq!(
            try_keys("/v1/datasources/7/sync"),
            ["v1/datasources/7/sync"]
        );
        assert_eq!(try_keys("/assets/app-3f2a1c.js"), ["assets/app-3f2a1c.js"]);
        assert_eq!(try_keys("/favicon.ico"), ["favicon.ico"]);
    }

    #[test]
    fn only_the_first_segment_decides_whether_a_path_is_api() {
        // The service code of an MCP endpoint is not a route root, so it stays SPA territory.
        assert_eq!(try_keys("/svc-42/status"), ["svc-42/status", "index.html"]);
        assert_eq!(try_keys("/v1x/things"), ["v1x/things", "index.html"]);
    }
}
