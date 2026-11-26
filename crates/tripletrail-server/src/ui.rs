//! The web UI, served at `/$/ui/`: a small page in the MatGUI style to manage datasets
//! (create, attach, detach, swap aliases, upload, back up) and to query them with MatGUI.
//!
//! MatGUI is embedded (gzipped) so the UI works without network access; `--matgui-url`
//! loads it from a CDN instead and leaves the embedded copy unused.

use crate::branding::{Branding, Image};
use crate::{HttpError, Reply, State, request_header};
use std::io::Read;
use tiny_http::Request;

const INDEX: &str = include_str!("../ui/index.html");
const APP_JS: &str = include_str!("../ui/app.js");
const API_DOCS: &str = include_str!("../ui/api-docs.html");
const API_DOCS_JS: &str = include_str!("../ui/api-docs.js");
const GRAPHQL: &str = include_str!("../ui/graphql.html");
const GRAPHQL_JS: &str = include_str!("../ui/graphql.js");
const APP_CSS: &str = include_str!("../ui/app.css");
const GUIDE_JS: &str = include_str!("../ui/guide.js");
const WORKBENCH_JS: &str = include_str!("../ui/workbench.js");
const ACCESS_JS: &str = include_str!("../ui/access.js");
const DESCRIBE_MAP_JS: &str = include_str!("../ui/describe-map.js");
const DESCRIBE_JS: &str = include_str!("../ui/describe.js");
const MAP_JS: &str = include_str!("../ui/map.js");
const MAP_TOOLS_JS: &str = include_str!("../ui/map-tools.js");
const MAP_SHARE_JS: &str = include_str!("../ui/map-share.js");
const SEARCH_JS: &str = include_str!("../ui/search.js");
const SHAPES_JS: &str = include_str!("../ui/shapes.js");
const EXPLORE_JS: &str = include_str!("../ui/explore.js");
const AI_JS: &str = include_str!("../ui/ai.js");
const BUILDER_JS: &str = include_str!("../ui/builder.js");
const IMPORT_JS: &str = include_str!("../ui/import.js");
const RECORDS_JS: &str = include_str!("../ui/records.js");
const RECORDS_EDIT_JS: &str = include_str!("../ui/records-edit.js");
const FORMS_JS: &str = include_str!("../ui/forms.js");
const ROW_RULES_JS: &str = include_str!("../ui/row-rules.js");
/// Release notes for the What's new dialog, newest first.
pub(crate) const WHATS_NEW: &str = include_str!("../ui/whats-new.json");
const ICON: &str = include_str!("../ui/icon.svg");

/// MatGUI 6.15.0's build files: (name, media type, bytes, gzipped).
const MATGUI: &[(&str, &str, &[u8], bool)] = &[
    (
        "yasgui.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/matgui/yasgui.min.js.gz"),
        true,
    ),
    (
        "yasgui.min.css",
        "text/css; charset=utf-8",
        include_bytes!("../ui/vendor/matgui/yasgui.min.css.gz"),
        true,
    ),
    (
        "yasgui-fa-solid-900-XY6737PV.woff2",
        "font/woff2",
        include_bytes!("../ui/vendor/matgui/yasgui-fa-solid-900-XY6737PV.woff2"),
        false,
    ),
    (
        "yasgui-layers-2x-TBM42ERR.png",
        "image/png",
        include_bytes!("../ui/vendor/matgui/yasgui-layers-2x-TBM42ERR.png"),
        false,
    ),
    (
        "yasgui-layers-55W3Q4RM.png",
        "image/png",
        include_bytes!("../ui/vendor/matgui/yasgui-layers-55W3Q4RM.png"),
        false,
    ),
    (
        "yasgui-marker-icon-2V3QKKVC.png",
        "image/png",
        include_bytes!("../ui/vendor/matgui/yasgui-marker-icon-2V3QKKVC.png"),
        false,
    ),
    (
        "yasgui-spritesheet-2x-KORMVNAU.png",
        "image/png",
        include_bytes!("../ui/vendor/matgui/yasgui-spritesheet-2x-KORMVNAU.png"),
        false,
    ),
    (
        "yasgui-spritesheet-YHL5CRRG.png",
        "image/png",
        include_bytes!("../ui/vendor/matgui/yasgui-spritesheet-YHL5CRRG.png"),
        false,
    ),
    (
        "LICENSE",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/matgui/LICENSE"),
        false,
    ),
];

/// The Workbench's editor and tables (#402), both MIT: CodeMirror 6 bundled by
/// `ui/vendor/codemirror/src/build.sh`, and Tabulator 6.3.1's build. Always embedded.
const WORKBENCH_VENDOR: &[(&str, &str, &[u8], bool)] = &[
    // MapLibre GL JS 6.16.0 (BSD-3-Clause), for the describe pages' map; its module loads
    // its worker from beside it.
    (
        "maplibre/maplibre-gl.mjs",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/maplibre/maplibre-gl.mjs.gz"),
        true,
    ),
    (
        "maplibre/maplibre-gl-worker.mjs",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/maplibre/maplibre-gl-worker.mjs.gz"),
        true,
    ),
    (
        "maplibre/maplibre-gl.css",
        "text/css; charset=utf-8",
        include_bytes!("../ui/vendor/maplibre/maplibre-gl.css.gz"),
        true,
    ),
    (
        "maplibre/LICENSE.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/maplibre/LICENSE.txt"),
        false,
    ),
    (
        "codemirror/codemirror.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/codemirror/codemirror.min.js.gz"),
        true,
    ),
    (
        "codemirror/LICENSES.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/codemirror/LICENSES.txt"),
        false,
    ),
    (
        "tabulator/tabulator.min.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/tabulator/tabulator.min.js.gz"),
        true,
    ),
    (
        "tabulator/tabulator.min.css",
        "text/css; charset=utf-8",
        include_bytes!("../ui/vendor/tabulator/tabulator.min.css.gz"),
        true,
    ),
    (
        "tabulator/LICENSE",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/tabulator/LICENSE"),
        false,
    ),
    // Swagger UI 5.34.0 (Apache-2.0, swagger-ui-dist from npm), for the REST API's docs
    // page (#143); THIRD-PARTY.txt holds the notices of what its bundle includes.
    (
        "swagger-ui/swagger-ui-bundle.js",
        "text/javascript; charset=utf-8",
        include_bytes!("../ui/vendor/swagger-ui/swagger-ui-bundle.js.gz"),
        true,
    ),
    (
        "swagger-ui/swagger-ui.css",
        "text/css; charset=utf-8",
        include_bytes!("../ui/vendor/swagger-ui/swagger-ui.css.gz"),
        true,
    ),
    (
        "swagger-ui/LICENSE",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/swagger-ui/LICENSE"),
        false,
    ),
    (
        "swagger-ui/NOTICE",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/swagger-ui/NOTICE"),
        false,
    ),
    (
        "swagger-ui/THIRD-PARTY.txt",
        "text/plain; charset=utf-8",
        include_bytes!("../ui/vendor/swagger-ui/THIRD-PARTY.txt"),
        false,
    ),
];

/// Where the page loads MatGUI from when no `--matgui-url` is given.
const EMBEDDED_BASE: &str = "/$/ui/vendor/matgui/";

fn page(content_type: &str, body: &str) -> Reply {
    let mut reply = Reply::bytes(200, content_type, body.as_bytes().to_vec());
    reply.headers.push(("Cache-Control", "no-cache".to_owned()));
    reply
}

pub(crate) fn serve(state: &State, request: &Request, path: &[&str]) -> Result<Reply, HttpError> {
    let brand = &state.options.branding;
    // The brand's images and settings are public, for the other HTML pages too (#262).
    match path {
        ["config.json"] => {
            let mut reply = crate::json_reply(200, &brand.config_json());
            reply.headers.push(("Cache-Control", "no-cache".to_owned()));
            return Ok(reply);
        }
        ["brand", "custom.css"] => {
            return match &brand.css {
                Some(css) => {
                    let mut reply = page("text/css; charset=utf-8", css);
                    reply.headers.push(("Cache-Control", "no-cache".to_owned()));
                    reply
                        .headers
                        .push(("X-Content-Type-Options", "nosniff".to_owned()));
                    Ok(reply)
                }
                None => Err(HttpError(404, "No --ui-css stylesheet".to_owned())),
            };
        }
        ["brand", which] => {
            return match brand.image(which) {
                Some(Image::File {
                    content_type,
                    bytes,
                }) => {
                    let mut reply = Reply::bytes(200, content_type, bytes.clone());
                    reply.headers.push(("Cache-Control", "no-cache".to_owned()));
                    reply
                        .headers
                        .push(("X-Content-Type-Options", "nosniff".to_owned()));
                    // An SVG opened on its own runs no script.
                    reply.headers.push((
                        "Content-Security-Policy",
                        "default-src 'none'; style-src 'unsafe-inline'; sandbox".to_owned(),
                    ));
                    Ok(reply)
                }
                Some(Image::Url(url)) => {
                    let mut reply = Reply::empty(302);
                    reply.headers.push(("Location", url.clone()));
                    Ok(reply)
                }
                None if *which == "favicon" => Ok(page("image/svg+xml", ICON)),
                None => Err(HttpError(404, format!("No brand image {which}"))),
            };
        }
        _ => {}
    }
    if !state.options.ui {
        return Err(HttpError(
            404,
            "The web UI is turned off (--no-ui)".to_owned(),
        ));
    }
    // Relative links need the trailing slash.
    if request
        .url()
        .split('?')
        .next()
        .is_some_and(|u| u.ends_with("/ui"))
    {
        let mut reply = Reply::empty(302);
        reply.headers.push(("Location", "/$/ui/".to_owned()));
        return Ok(reply);
    }
    match path {
        []
        | ["index.html"]
        | [
            "datasets" | "query" | "map" | "search" | "explore" | "tasks" | "server" | "workbench"
            | "access" | "shapes" | "import" | "records",
            ..,
        ] => {
            let base = state
                .options
                .matgui_url
                .as_deref()
                .map(|u| {
                    if u.ends_with('/') {
                        u.to_owned()
                    } else {
                        format!("{u}/")
                    }
                })
                .unwrap_or_else(|| EMBEDDED_BASE.to_owned());
            let topbar = match &brand.name {
                Some(name) => format!("<b>{}</b>", html_attribute(name)),
                None => "<b>TripleTrail</b> server".to_owned(),
            };
            // The operator's text goes in last, so that it is never read as a placeholder.
            let html = INDEX
                .replace("{{MATGUI}}", &html_attribute(&base))
                .replace("{{VERSION}}", env!("CARGO_PKG_VERSION"))
                .replace("{{FAVICON}}", brand.favicon_href())
                .replace("{{BRAND_STYLE}}", &brand.style())
                .replace("{{LOGO}}", &brand.logo_html())
                .replace("{{TOPBAR_NAME}}", &topbar)
                .replace("{{BANNER}}", &brand.banner_html())
                .replace("{{FOOTER}}", &brand.footer_html())
                .replace(
                    "{{SUPPORT}}",
                    &html_attribute(brand.support_url.as_deref().unwrap_or_default()),
                )
                .replace(
                    "{{CONTACT}}",
                    &html_attribute(brand.contact_email.as_deref().unwrap_or_default()),
                )
                .replace("{{CUSTOM_CSS}}", brand.css_link())
                .replace(
                    "{{MAP_VIEWS}}",
                    &html_attribute(&brand.map_views_json().to_string()),
                )
                .replace("{{UI_HOME}}", brand.defaults.home.unwrap_or("datasets"))
                .replace("{{UI_THEME}}", brand.defaults.theme.unwrap_or("auto"))
                .replace(
                    "{{MAP_VIEW}}",
                    brand.defaults.map_view.as_deref().unwrap_or_default(),
                )
                .replace(
                    "{{DEFAULT_DATASET}}",
                    &html_attribute(brand.defaults.dataset.as_deref().unwrap_or_default()),
                )
                .replace("{{SITE_NAME}}", &html_attribute(brand.name()));
            let mut reply = page("text/html; charset=utf-8", &html);
            reply.headers.push((
                "Content-Security-Policy",
                // The page itself loads nothing from elsewhere but MatGUI; MatGUI's map
                // fetches tiles, and queries may go to any endpoint a user picks.
                "frame-ancestors 'self'; base-uri 'self'; form-action 'self'".to_owned(),
            ));
            reply
                .headers
                .push(("X-Content-Type-Options", "nosniff".to_owned()));
            Ok(reply)
        }
        // The REST API's docs (#143): Swagger UI over `/{ds}/api/openapi.json`.
        ["api", ds] => {
            // The page names the dataset only; its definition is read under the caller's
            // access, so the page tells nothing about a dataset the caller may not read.
            let html = branded(API_DOCS, brand).replace("{{DATASET}}", &html_attribute(ds));
            let mut reply = page("text/html; charset=utf-8", &html);
            reply.headers.push((
                "Content-Security-Policy",
                format!(
                    "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:{}; \
                     frame-ancestors 'self'; base-uri 'self'; form-action 'self'",
                    brand.image_origins()
                ),
            ));
            reply
                .headers
                .push(("X-Content-Type-Options", "nosniff".to_owned()));
            Ok(reply)
        }
        // The GraphQL playground (#144) over `/{ds}/graphql`.
        ["graphql", ds] => {
            let html = branded(GRAPHQL, brand).replace("{{DATASET}}", &html_attribute(ds));
            let mut reply = page("text/html; charset=utf-8", &html);
            reply.headers.push((
                "Content-Security-Policy",
                format!(
                    "default-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:{}; \
                     frame-ancestors 'self'; base-uri 'self'; form-action 'self'",
                    brand.image_origins()
                ),
            ));
            reply
                .headers
                .push(("X-Content-Type-Options", "nosniff".to_owned()));
            Ok(reply)
        }
        ["graphql.js"] => Ok(page("text/javascript; charset=utf-8", GRAPHQL_JS)),
        ["api-docs.js"] => Ok(page("text/javascript; charset=utf-8", API_DOCS_JS)),
        ["app.js"] => Ok(page("text/javascript; charset=utf-8", APP_JS)),
        ["app.css"] => Ok(page("text/css; charset=utf-8", APP_CSS)),
        ["guide.js"] => Ok(page("text/javascript; charset=utf-8", GUIDE_JS)),
        ["workbench.js"] => Ok(page("text/javascript; charset=utf-8", WORKBENCH_JS)),
        ["access.js"] => Ok(page("text/javascript; charset=utf-8", ACCESS_JS)),
        ["map.js"] => Ok(page("text/javascript; charset=utf-8", MAP_JS)),
        ["map-tools.js"] => Ok(page("text/javascript; charset=utf-8", MAP_TOOLS_JS)),
        ["map-share.js"] => Ok(page("text/javascript; charset=utf-8", MAP_SHARE_JS)),
        ["search.js"] => Ok(page("text/javascript; charset=utf-8", SEARCH_JS)),
        ["shapes.js"] => Ok(page("text/javascript; charset=utf-8", SHAPES_JS)),
        ["explore.js"] => Ok(page("text/javascript; charset=utf-8", EXPLORE_JS)),
        ["ai.js"] => Ok(page("text/javascript; charset=utf-8", AI_JS)),
        // The `tts:` vocabulary, which the AI assistant (#146) gives its model.
        ["tts.ttl"] => Ok(page(
            "text/turtle; charset=utf-8",
            tripletrail::shapes::VOCABULARY,
        )),
        ["builder.js"] => Ok(page("text/javascript; charset=utf-8", BUILDER_JS)),
        ["import.js"] => Ok(page("text/javascript; charset=utf-8", IMPORT_JS)),
        ["records.js"] => Ok(page("text/javascript; charset=utf-8", RECORDS_JS)),
        ["records-edit.js"] => Ok(page("text/javascript; charset=utf-8", RECORDS_EDIT_JS)),
        ["forms.js"] => Ok(page("text/javascript; charset=utf-8", FORMS_JS)),
        ["row-rules.js"] => Ok(page("text/javascript; charset=utf-8", ROW_RULES_JS)),
        ["describe-map.js"] => Ok(page("text/javascript; charset=utf-8", DESCRIBE_MAP_JS)),
        ["describe.js"] => Ok(page("text/javascript; charset=utf-8", DESCRIBE_JS)),
        ["whats-new.json"] => Ok(page("application/json", WHATS_NEW)),
        ["icon.svg"] => Ok(page("image/svg+xml", ICON)),
        ["vendor", folder, name] => {
            let found = if *folder == "matgui" {
                MATGUI.iter().find(|(n, ..)| n == name)
            } else {
                let path = format!("{folder}/{name}");
                WORKBENCH_VENDOR.iter().find(|(n, ..)| *n == path)
            };
            let Some((_, content_type, bytes, gzipped)) = found else {
                return Err(HttpError(404, format!("No file {folder}/{name}")));
            };
            let accepts_gzip = request_header(request, "Accept-Encoding")
                .is_some_and(|v| v.split(',').any(|e| e.trim().starts_with("gzip")));
            let mut reply = if *gzipped && !accepts_gzip {
                let mut out = Vec::new();
                flate2::read::GzDecoder::new(*bytes)
                    .read_to_end(&mut out)
                    .map_err(|e| HttpError(500, e.to_string()))?;
                Reply::bytes(200, content_type, out)
            } else {
                let mut reply = Reply::bytes(200, content_type, bytes.to_vec());
                if *gzipped {
                    reply.headers.push(("Content-Encoding", "gzip".to_owned()));
                }
                reply
            };
            // The files never change for a given tripletrail version.
            reply
                .headers
                .push(("Cache-Control", "public, max-age=86400".to_owned()));
            reply.headers.push(("Vary", "Accept-Encoding".to_owned()));
            Ok(reply)
        }
        _ => Err(HttpError(404, "No such page".to_owned())),
    }
}

/// The REST API and GraphQL pages with the server's name, favicon and colour.
fn branded(template: &str, brand: &Branding) -> String {
    template
        .replace("{{FAVICON}}", brand.favicon_href())
        .replace("{{BRAND_STYLE}}", &brand.page_style())
        .replace("{{SITE_NAME}}", &html_attribute(brand.name()))
}

fn html_attribute(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    /// A merge left in a page or script would break the whole UI, which the HTTP tests (looking
    /// for strings) would not notice.
    #[test]
    fn no_merge_conflict_markers() {
        for (name, text) in [
            ("index.html", super::INDEX),
            ("app.js", super::APP_JS),
            ("app.css", super::APP_CSS),
            ("guide.js", super::GUIDE_JS),
            ("workbench.js", super::WORKBENCH_JS),
            ("access.js", super::ACCESS_JS),
            ("describe-map.js", super::DESCRIBE_MAP_JS),
            ("describe.js", super::DESCRIBE_JS),
            ("map.js", super::MAP_JS),
            ("map-tools.js", super::MAP_TOOLS_JS),
            ("map-share.js", super::MAP_SHARE_JS),
            ("search.js", super::SEARCH_JS),
            ("shapes.js", super::SHAPES_JS),
            ("explore.js", super::EXPLORE_JS),
            ("ai.js", super::AI_JS),
            ("builder.js", super::BUILDER_JS),
            ("import.js", super::IMPORT_JS),
            ("records.js", super::RECORDS_JS),
            ("records-edit.js", super::RECORDS_EDIT_JS),
            ("forms.js", super::FORMS_JS),
            ("row-rules.js", super::ROW_RULES_JS),
            ("whats-new.json", super::WHATS_NEW),
        ] {
            for marker in ["<<<<<<< ", "=======\n", ">>>>>>> "] {
                assert!(
                    !text.lines().any(|l| format!("{l}\n").starts_with(marker)),
                    "{name} has a merge conflict marker {marker:?}"
                );
            }
        }
    }
}
