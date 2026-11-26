//! Server branding (#262): the name, favicon, logo and accent colour an operator gives the web UI
//! and the other HTML pages (`--site-name`, `--site-favicon`, `--site-logo`, `--site-logo-dark`,
//! `--site-color`), and the notices and links on them (`--banner`, `--site-link`,
//! `--support-url`, `--contact-email`).
//!
//! Values are plain text or checked `http(s)` URLs, never HTML. Image files are read once at
//! start-up and served from `/$/ui/brand/{favicon,logo,logo-dark}` with a strict content type;
//! an image given as a URL is a redirect from the same path, so every page refers to the
//! server's own path. `GET /$/ui/config.json` gives the same settings to scripts, without
//! credentials: it holds no data.

use serde_json::{Value, json};

/// The largest favicon or logo file the server reads.
pub(crate) const MAX_IMAGE: u64 = 256 * 1024;

/// The name the pages carry when no `--site-name` is given.
pub(crate) const DEFAULT_NAME: &str = "TripleTrail";

/// An image of the brand: read from a file, or found at a URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Image {
    File {
        content_type: &'static str,
        bytes: Vec<u8>,
    },
    Url(String),
}

/// The accent colour and the shades the pages derive from it, for light and dark themes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Color {
    pub(crate) hex: String,
    light: Shades,
    dark: Shades,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Shades {
    brand: String,
    fill: String,
    strong: String,
    soft: String,
}

/// What `--site-*` set; all of it optional, and nothing set leaves the pages as they were.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Branding {
    pub(crate) name: Option<String>,
    pub(crate) favicon: Option<Image>,
    pub(crate) logo: Option<Image>,
    pub(crate) logo_dark: Option<Image>,
    pub(crate) color: Option<Color>,
    pub(crate) banner: Option<Banner>,
    /// `--site-link`: (label, URL) for the pages' footer.
    pub(crate) links: Vec<(String, String)>,
    pub(crate) support_url: Option<String>,
    pub(crate) contact_email: Option<String>,
    /// What a new viewer of the web UI starts on (`--default-dataset`, `--ui-home`,
    /// `--ui-theme`, `--map-view`).
    pub(crate) defaults: Defaults,
}

/// What a viewer of the web UI starts on, until they choose otherwise.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Defaults {
    /// The dataset the pages open on, when the viewer may see it.
    pub(crate) dataset: Option<String>,
    /// The page `/` and `/$/ui/` open: one of [`HOMES`]; the Datasets page without it.
    pub(crate) home: Option<&'static str>,
    /// The first visit's theme: `light` or `dark`; `auto` (the system's) without it.
    pub(crate) theme: Option<&'static str>,
    /// The Map page's start view when the data's extent is unknown, as `ZOOM/LAT/LON`.
    pub(crate) map_view: Option<String>,
}

/// The pages `--ui-home` may name.
pub const HOMES: [&str; 5] = ["datasets", "query", "map", "search", "explore"];

/// One line of notice over every page of the web UI (`--banner`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Banner {
    pub(crate) text: String,
    pub(crate) warning: bool,
    pub(crate) link: Option<String>,
}

impl Branding {
    /// Reads and checks the options: a name of plain text, images as files (SVG, PNG, ICO,
    /// WebP, JPEG; at most 256 KB) or `http(s)` URLs, and a `#rrggbb` colour that white text
    /// reads on (WCAG AA, 4.5:1).
    pub fn new(
        name: Option<&str>,
        favicon: Option<&str>,
        logo: Option<&str>,
        logo_dark: Option<&str>,
        color: Option<&str>,
    ) -> Result<Branding, String> {
        let name = match name.map(str::trim) {
            None | Some("") => None,
            Some(n) if n.chars().count() > 80 => {
                return Err("--site-name takes at most 80 characters".to_owned());
            }
            Some(n) if n.chars().any(char::is_control) => {
                return Err("--site-name takes one line of text".to_owned());
            }
            Some(n) => Some(n.to_owned()),
        };
        Ok(Branding {
            name,
            favicon: favicon.map(|v| image("--site-favicon", v)).transpose()?,
            logo: logo.map(|v| image("--site-logo", v)).transpose()?,
            logo_dark: logo_dark
                .map(|v| image("--site-logo-dark", v))
                .transpose()?,
            color: color.map(parse_color).transpose()?,
            ..Branding::default()
        })
    }

    /// Adds the notices and links: a banner of one line of plain text (`info` or `warning`,
    /// with an optional link), footer links as `LABEL|URL`, the support link that the About
    /// dialog's feedback points to, and the contact e-mail. URLs are `http(s)`.
    pub fn with_notices(
        mut self,
        banner: Option<&str>,
        banner_level: Option<&str>,
        banner_link: Option<&str>,
        links: &[String],
        support_url: Option<&str>,
        contact_email: Option<&str>,
    ) -> Result<Branding, String> {
        let warning = match banner_level.map(str::trim) {
            None | Some("" | "info") => false,
            Some("warning") => true,
            Some(other) => {
                return Err(format!("--banner-level takes info or warning, not {other}"));
            }
        };
        let link = banner_link
            .map(|u| checked_url("--banner-link", u))
            .transpose()?;
        self.banner = match banner.map(str::trim) {
            None | Some("") => {
                if link.is_some() || warning {
                    return Err("--banner-level and --banner-link need --banner".to_owned());
                }
                None
            }
            Some(text) => Some(Banner {
                text: one_line("--banner", text, 200)?,
                warning,
                link,
            }),
        };
        self.links = links
            .iter()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let (label, url) = l.split_once('|').ok_or_else(|| {
                    format!("--site-link takes LABEL|URL, as Privacy|https://example.org/privacy, not {l}")
                })?;
                Ok((
                    one_line("--site-link", label.trim(), 40)?,
                    checked_url("--site-link", url)?,
                ))
            })
            .collect::<Result<_, String>>()?;
        self.support_url = support_url
            .map(|u| checked_url("--support-url", u))
            .transpose()?;
        self.contact_email = match contact_email.map(str::trim) {
            None | Some("") => None,
            Some(e) => {
                let ok = e.len() <= 254
                    && e.split('@').count() == 2
                    && e.split('@').all(|p| !p.is_empty())
                    && e.split('@').nth(1).is_some_and(|d| d.contains('.'))
                    && !e.chars().any(|c| {
                        c.is_whitespace()
                            || c.is_control()
                            || matches!(c, '"' | '<' | '>' | '\\' | ',' | ';' | '(' | ')')
                    });
                if !ok {
                    return Err(format!("--contact-email: {e} is not an e-mail address"));
                }
                Some(e.to_owned())
            }
        };
        Ok(self)
    }

    /// Adds what a new viewer starts on: the dataset the pages open on, the page `/$/ui/`
    /// opens (one of [`HOMES`]), the first visit's theme (`auto`, `light` or `dark`) and the
    /// Map page's start view as `ZOOM/LAT/LON`.
    pub fn with_defaults(
        mut self,
        dataset: Option<&str>,
        home: Option<&str>,
        theme: Option<&str>,
        map_view: Option<&str>,
    ) -> Result<Branding, String> {
        self.defaults.dataset = dataset
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| one_line("--default-dataset", d, 200))
            .transpose()?;
        self.defaults.home = match home.map(|h| h.trim().to_ascii_lowercase()) {
            None => None,
            Some(h) if h.is_empty() || h == "datasets" => None,
            Some(h) => Some(
                *HOMES
                    .iter()
                    .find(|p| **p == h)
                    .ok_or_else(|| format!("--ui-home takes {}, not {h}", HOMES.join(", ")))?,
            ),
        };
        self.defaults.theme = match theme.map(|t| t.trim().to_ascii_lowercase()).as_deref() {
            None | Some("" | "auto") => None,
            Some("light") => Some("light"),
            Some("dark") => Some("dark"),
            Some(other) => {
                return Err(format!("--ui-theme takes auto, light or dark, not {other}"));
            }
        };
        self.defaults.map_view = map_view
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(parse_map_view)
            .transpose()?;
        Ok(self)
    }

    /// The banner over the web UI's pages, which a viewer may dismiss for the browser session.
    /// Empty without `--banner`.
    pub(crate) fn banner_html(&self) -> String {
        let Some(b) = &self.banner else {
            return String::new();
        };
        let text = escape(&b.text);
        let text = match &b.link {
            Some(url) => format!(
                "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{text}</a>",
                escape(url)
            ),
            None => text,
        };
        format!(
            "<div class=\"site-banner {}\" id=\"site-banner\" role=\"status\" data-text=\"{}\">\
             <span>{text}</span><button type=\"button\" class=\"site-banner-close\" \
             aria-label=\"Dismiss\" title=\"Dismiss\">×</button></div>",
            if b.warning { "warning" } else { "info" },
            escape(&b.text),
        )
    }

    /// The footer of `--site-link`s, for the web UI and the server's other HTML pages. Empty
    /// without them.
    pub(crate) fn footer_html(&self) -> String {
        if self.links.is_empty() {
            return String::new();
        }
        let links: Vec<String> = self
            .links
            .iter()
            .map(|(label, url)| format!("<a href=\"{}\">{}</a>", escape(url), escape(label)))
            .collect();
        format!("<footer class=\"site-footer\">{}</footer>", links.join(" "))
    }

    /// The origins of the brand's images given as URLs, for the `img-src` of pages with a
    /// strict `Content-Security-Policy`: a favicon follows its redirect.
    pub(crate) fn image_origins(&self) -> String {
        let mut origins: Vec<String> = [&self.favicon, &self.logo, &self.logo_dark]
            .into_iter()
            .filter_map(|i| match i {
                Some(Image::Url(u)) => {
                    let (scheme, rest) = u.split_once("://")?;
                    let host = rest.split(['/', '?', '#']).next()?;
                    Some(format!(" {}://{host}", scheme.to_ascii_lowercase()))
                }
                _ => None,
            })
            .collect();
        origins.sort();
        origins.dedup();
        origins.concat()
    }

    /// The name the pages carry: `--site-name`, or TripleTrail.
    pub(crate) fn name(&self) -> &str {
        self.name.as_deref().unwrap_or(DEFAULT_NAME)
    }

    /// The image served at `/$/ui/brand/{which}`.
    pub(crate) fn image(&self, which: &str) -> Option<&Image> {
        match which {
            "favicon" => self.favicon.as_ref(),
            "logo" => self.logo.as_ref(),
            "logo-dark" => self.logo_dark.as_ref(),
            _ => None,
        }
    }

    /// Where the pages load the favicon from.
    pub(crate) fn favicon_href(&self) -> &'static str {
        if self.favicon.is_some() {
            "/$/ui/brand/favicon"
        } else {
            "/$/ui/icon.svg"
        }
    }

    /// The favicon `<link>` of the OGC API and describe pages: only with `--site-favicon`, so
    /// that they name no path of the web UI otherwise.
    pub(crate) fn favicon_link(&self) -> &'static str {
        if self.favicon.is_some() {
            "<link rel=\"icon\" href=\"/$/ui/brand/favicon\">"
        } else {
            ""
        }
    }

    /// The `<style>` the web UI's pages add after their own stylesheets: MatGUI's brand
    /// variables from `--site-color`. Empty without it.
    pub(crate) fn style(&self) -> String {
        let mut css = String::new();
        if let Some(c) = &self.color {
            let vars = |s: &Shades, on: &str| {
                format!(
                    "--yasgui-brand:{b};--yasgui-brand-fill:{f};--yasgui-brand-fill-strong:{st};\
                     --yasgui-brand-strong:{st};--yasgui-brand-soft:{so};--yasgui-on-brand:{on};\
                     --yasgui-focus:{st};--yasgui-input-focus:{st};",
                    b = s.brand,
                    f = s.fill,
                    st = s.strong,
                    so = s.soft,
                )
            };
            css.push_str(&format!(
                ":root{{{}}}[data-theme=dark]{{{}}}",
                vars(&c.light, "#ffffff"),
                vars(&c.dark, "#ffffff")
            ));
        }
        if css.is_empty() {
            css
        } else {
            format!("<style>{css}</style>")
        }
    }

    /// The `<style>` of the REST API and GraphQL pages, which keep their own look: links and
    /// buttons in `--site-color`. Empty without it.
    pub(crate) fn page_style(&self) -> String {
        match &self.color {
            None => String::new(),
            Some(c) => format!(
                "<style>:root{{--brand:{}}}@media (prefers-color-scheme: dark){{:root{{--brand:{}}}}}\
                 .api-docs-bar a{{color:{}}}</style>",
                c.light.brand, c.dark.brand, c.light.brand
            ),
        }
    }

    /// The top bar's logo: TripleTrail's mark, or `--site-logo` (and `--site-logo-dark` in
    /// the dark theme) at its own width.
    pub(crate) fn logo_html(&self) -> String {
        match (&self.logo, &self.logo_dark) {
            (None, _) => "<span class=\"logo\" aria-hidden=\"true\"></span>".to_owned(),
            (Some(_), None) => {
                "<img class=\"logo-img\" src=\"/$/ui/brand/logo\" alt=\"\">".to_owned()
            }
            (Some(_), Some(_)) => {
                "<img class=\"logo-img logo-light\" src=\"/$/ui/brand/logo\" alt=\"\">\
                 <img class=\"logo-img logo-dark\" src=\"/$/ui/brand/logo-dark\" alt=\"\">"
                    .to_owned()
            }
        }
    }

    /// `/$/ui/config.json`: the branding for scripts (the web UI, the tools app later).
    pub(crate) fn config_json(&self) -> Value {
        let path = |which: &str, image: &Option<Image>| {
            image.as_ref().map(|_| format!("/$/ui/brand/{which}"))
        };
        json!({
            "site": {
                "name": self.name(),
                "named": self.name.is_some(),
                "favicon": self.favicon_href(),
                "logo": path("logo", &self.logo),
                "logoDark": path("logo-dark", &self.logo_dark),
                "color": self.color.as_ref().map(|c| c.hex.clone()),
                "banner": self.banner.as_ref().map(|b| json!({
                    "text": b.text,
                    "level": if b.warning { "warning" } else { "info" },
                    "link": b.link,
                })),
                "links": self.links.iter().map(|(label, url)| json!({ "label": label, "url": url })).collect::<Vec<_>>(),
                "supportUrl": self.support_url,
                "contactEmail": self.contact_email,
                "defaults": {
                    "dataset": self.defaults.dataset,
                    "home": self.defaults.home.unwrap_or("datasets"),
                    "theme": self.defaults.theme.unwrap_or("auto"),
                    "mapView": self.defaults.map_view,
                },
            },
            "version": env!("CARGO_PKG_VERSION"),
        })
    }
}

/// Plain text for HTML: `&`, `<`, `>` and `"` escaped.
pub(crate) fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `ZOOM/LAT/LON` (as in a map link's `#zoom/lat/lon`): zoom 0 to 22, latitude -85 to 85,
/// longitude -180 to 180. Given back as written, without spaces.
fn parse_map_view(value: &str) -> Result<String, String> {
    let bad = || {
        format!(
            "--map-view takes ZOOM/LAT/LON, as 5/50.8/4.4 (zoom 0 to 22, latitude -85 to 85, \
             longitude -180 to 180), not {value}"
        )
    };
    let parts: Vec<&str> = value.split('/').map(str::trim).collect();
    let [zoom, lat, lon] = parts[..] else {
        return Err(bad());
    };
    let number = |s: &str, low: f64, high: f64| {
        s.parse::<f64>()
            .ok()
            .filter(|n| n.is_finite() && (low..=high).contains(n))
            .ok_or_else(bad)
    };
    number(zoom, 0.0, 22.0)?;
    number(lat, -85.0, 85.0)?;
    number(lon, -180.0, 180.0)?;
    Ok(format!("{zoom}/{lat}/{lon}"))
}

/// One line of plain text, at most `max` characters.
fn one_line(option: &str, text: &str, max: usize) -> Result<String, String> {
    if text.is_empty() {
        return Err(format!("{option} takes some text"));
    }
    if text.chars().count() > max {
        return Err(format!("{option} takes at most {max} characters"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("{option} takes one line of text"));
    }
    Ok(text.to_owned())
}

/// An `http(s)` URL that can go in an HTML attribute as it is.
fn checked_url(option: &str, value: &str) -> Result<String, String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://"))
        || value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\'))
        || value.split("://").nth(1).is_none_or(|rest| rest.is_empty())
    {
        return Err(format!("{option} takes an http(s) URL, not {value}"));
    }
    Ok(value.to_owned())
}

fn image(option: &str, value: &str) -> Result<Image, String> {
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        if value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '<' | '>' | '\\'))
            || value.split("://").nth(1).is_none_or(|rest| rest.is_empty())
        {
            return Err(format!("{option}: {value} is not a URL"));
        }
        return Ok(Image::Url(value.to_owned()));
    }
    if lower.contains("://") || lower.starts_with("data:") || lower.starts_with("javascript:") {
        return Err(format!(
            "{option} takes a file or an http(s) URL, not {value}"
        ));
    }
    let content_type = match lower.rsplit('.').next() {
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("webp") => "image/webp",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => {
            return Err(format!(
                "{option}: {value} is not an SVG, PNG, ICO, WebP or JPEG file"
            ));
        }
    };
    let size = std::fs::metadata(value)
        .map_err(|e| format!("{option}: cannot read {value}: {e}"))?
        .len();
    if size > MAX_IMAGE {
        return Err(format!(
            "{option}: {value} has {size} bytes, more than the {} KB an image may have",
            MAX_IMAGE / 1024
        ));
    }
    let bytes = std::fs::read(value).map_err(|e| format!("{option}: cannot read {value}: {e}"))?;
    Ok(Image::File {
        content_type,
        bytes,
    })
}

type Rgb = [f64; 3];

fn parse_color(value: &str) -> Result<Color, String> {
    let v = value.trim();
    let hex = v.strip_prefix('#').unwrap_or(v);
    let rgb = (hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| [0, 2, 4].map(|i| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0))))
        .ok_or_else(|| format!("--site-color takes a colour as #rrggbb, not {v}"))?;
    let ratio = contrast(rgb, [255.0; 3]);
    if ratio < 4.5 {
        return Err(format!(
            "--site-color #{hex}: white text on it has a contrast of {ratio:.2}:1, less than the \
             4.5:1 WCAG AA asks for; take a darker colour"
        ));
    }
    let white = [255.0; 3];
    // The dark theme's background, MatGUI's.
    let night = [11.0, 14.0, 19.0];
    Ok(Color {
        hex: format!("#{}", hex.to_ascii_lowercase()),
        light: Shades {
            brand: css(rgb),
            fill: css(rgb),
            strong: css(mix(rgb, [0.0; 3], 0.25)),
            soft: css(mix(rgb, white, 0.88)),
        },
        dark: Shades {
            brand: css(mix(rgb, white, 0.45)),
            fill: css(rgb),
            strong: css(mix(rgb, white, 0.6)),
            soft: css(mix(rgb, night, 0.8)),
        },
    })
}

/// `a` moved the fraction `t` of the way to `b`.
fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

fn css(c: Rgb) -> String {
    let [r, g, b] = c.map(|v| v.round().clamp(0.0, 255.0) as u8);
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// WCAG 2's contrast ratio of two colours.
fn contrast(a: Rgb, b: Rgb) -> f64 {
    let lum = |c: Rgb| {
        let [r, g, b] = c.map(|v| {
            let s = v / 255.0;
            if s <= 0.039_28 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (x, y) = (lum(a), lum(b));
    (x.max(y) + 0.05) / (x.min(y) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_set_changes_nothing() {
        let b = Branding::new(None, None, None, None, None).unwrap();
        assert_eq!(b.name(), "TripleTrail");
        assert_eq!(b.style(), "");
        assert_eq!(b.favicon_href(), "/$/ui/icon.svg");
        assert_eq!(b.config_json()["site"]["named"], false);
    }

    #[test]
    fn colours_white_text_cannot_read_on_are_refused() {
        // The default brand blue passes; a light yellow does not.
        let c = Branding::new(None, None, None, None, Some("#0563BB")).unwrap();
        assert_eq!(c.color.as_ref().unwrap().hex, "#0563bb");
        assert!(
            c.style().contains("--yasgui-brand:#0563bb"),
            "{}",
            c.style()
        );
        let e = Branding::new(None, None, None, None, Some("#f5d000")).unwrap_err();
        assert!(e.contains("contrast"), "{e}");
        for bad in ["blue", "#12345", "#zzzzzz", "rgb(0,0,0)"] {
            let e = Branding::new(None, None, None, None, Some(bad)).unwrap_err();
            assert!(e.contains("#rrggbb"), "{bad}: {e}");
        }
    }

    #[test]
    fn images_are_files_of_known_types_or_http_urls() {
        let dir = tripletrail_testdir::tempdir("branding-images");
        let svg = dir.join("logo.svg");
        std::fs::write(&svg, "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        let b = Branding::new(None, svg.to_str(), None, None, None).unwrap();
        assert!(matches!(
            b.favicon,
            Some(Image::File {
                content_type: "image/svg+xml",
                ..
            })
        ));
        assert_eq!(b.favicon_href(), "/$/ui/brand/favicon");
        let b =
            Branding::new(None, None, Some("https://example.org/logo.png"), None, None).unwrap();
        assert_eq!(
            b.logo,
            Some(Image::Url("https://example.org/logo.png".to_owned()))
        );
        let big = dir.join("big.png");
        std::fs::write(&big, vec![0u8; MAX_IMAGE as usize + 1]).unwrap();
        for (value, says) in [
            (big.to_str().unwrap(), "more than"),
            ("javascript:alert(1)", "file or an http(s) URL"),
            ("ftp://example.org/x.svg", "file or an http(s) URL"),
            ("https://example.org/a\"b.svg", "not a URL"),
            ("logo.gif", "not an SVG"),
            ("/no/such/logo.svg", "cannot read"),
        ] {
            let e = Branding::new(None, None, Some(value), None, None).unwrap_err();
            assert!(e.contains(says), "{value}: {e}");
        }
    }

    #[test]
    fn notices_are_plain_text_and_checked_urls() {
        let none = || Branding::new(None, None, None, None, None).unwrap();
        let b = none()
            .with_notices(
                Some("Demo <b>data</b> resets nightly"),
                Some("warning"),
                Some("https://example.org/status"),
                &[
                    "Privacy|https://example.org/privacy".to_owned(),
                    "Contact & help|https://example.org/c?a=1&b=2".to_owned(),
                ],
                Some("https://example.org/help"),
                Some("data@example.org"),
            )
            .unwrap();
        let banner = b.banner_html();
        assert!(banner.contains("class=\"site-banner warning\""), "{banner}");
        assert!(
            banner.contains(">Demo &lt;b&gt;data&lt;/b&gt; resets nightly</a>"),
            "{banner}"
        );
        assert!(banner.contains("href=\"https://example.org/status\""));
        let footer = b.footer_html();
        assert!(
            footer.contains("<a href=\"https://example.org/c?a=1&amp;b=2\">Contact &amp; help</a>"),
            "{footer}"
        );
        let config = b.config_json();
        assert_eq!(config["site"]["banner"]["level"], "warning");
        assert_eq!(config["site"]["links"][0]["label"], "Privacy");
        assert_eq!(config["site"]["supportUrl"], "https://example.org/help");
        assert_eq!(config["site"]["contactEmail"], "data@example.org");
        assert_eq!(none().banner_html(), "");
        assert_eq!(none().footer_html(), "");
        let refused = |banner: Option<&str>,
                       level: Option<&str>,
                       link: Option<&str>,
                       links: &[&str],
                       support: Option<&str>,
                       email: Option<&str>| {
            let links: Vec<String> = links.iter().map(|l| (*l).to_owned()).collect();
            none()
                .with_notices(banner, level, link, &links, support, email)
                .unwrap_err()
        };
        assert!(
            refused(Some("x"), Some("loud"), None, &[], None, None).contains("info or warning")
        );
        assert!(
            refused(
                Some("x"),
                None,
                Some("javascript:alert(1)"),
                &[],
                None,
                None
            )
            .contains("http(s) URL")
        );
        assert!(
            refused(None, None, Some("https://example.org/"), &[], None, None)
                .contains("need --banner")
        );
        assert!(refused(Some("a\nb"), None, None, &[], None, None).contains("one line"));
        assert!(
            refused(
                None,
                None,
                None,
                &["Privacy https://example.org/"],
                None,
                None
            )
            .contains("LABEL|URL")
        );
        assert!(
            refused(
                None,
                None,
                None,
                &["Privacy|ftp://example.org/"],
                None,
                None
            )
            .contains("http(s) URL")
        );
        assert!(refused(None, None, None, &[], Some("example.org"), None).contains("http(s) URL"));
        for bad in [
            "nobody",
            "a@b",
            "a b@example.org",
            "<a@example.org>",
            "a@b@example.org",
        ] {
            assert!(
                refused(None, None, None, &[], None, Some(bad)).contains("e-mail"),
                "{bad}"
            );
        }
    }

    #[test]
    fn image_urls_give_their_origins_for_img_src() {
        let b = Branding::new(
            None,
            Some("https://cdn.example.org/brand/favicon.png"),
            Some("https://cdn.example.org/brand/logo.svg"),
            Some("http://Other.example.org:8080/logo-dark.svg?v=2"),
            None,
        )
        .unwrap();
        assert_eq!(
            b.image_origins(),
            " http://Other.example.org:8080 https://cdn.example.org"
        );
        assert_eq!(Branding::default().image_origins(), "");
    }

    #[test]
    fn viewer_defaults_are_checked() {
        let none = || Branding::new(None, None, None, None, None).unwrap();
        let b = none()
            .with_defaults(
                Some(" rail "),
                Some("Map"),
                Some("dark"),
                Some("5 / 50.8 / 4.4"),
            )
            .unwrap();
        assert_eq!(b.defaults.dataset.as_deref(), Some("rail"));
        assert_eq!(b.defaults.home, Some("map"));
        assert_eq!(b.defaults.theme, Some("dark"));
        assert_eq!(b.defaults.map_view.as_deref(), Some("5/50.8/4.4"));
        let config = b.config_json();
        assert_eq!(config["site"]["defaults"]["home"], "map");
        assert_eq!(config["site"]["defaults"]["mapView"], "5/50.8/4.4");
        let plain = none()
            .with_defaults(None, Some("datasets"), Some("auto"), None)
            .unwrap();
        assert_eq!(plain.defaults, Defaults::default());
        assert_eq!(plain.config_json()["site"]["defaults"]["home"], "datasets");
        assert_eq!(plain.config_json()["site"]["defaults"]["theme"], "auto");
        let refused = |home: Option<&str>, theme: Option<&str>, view: Option<&str>| {
            none().with_defaults(None, home, theme, view).unwrap_err()
        };
        assert!(refused(Some("server"), None, None).contains("--ui-home takes datasets, query"));
        assert!(refused(None, Some("blue"), None).contains("auto, light or dark"));
        for view in [
            "5/50.8", "23/0/0", "5/90/0", "5/0/181", "a/b/c", "5/NaN/0", "5/0/0/0",
        ] {
            assert!(
                refused(None, None, Some(view)).contains("ZOOM/LAT/LON"),
                "{view}"
            );
        }
    }

    #[test]
    fn names_are_one_short_line() {
        assert!(Branding::new(Some("a\nb"), None, None, None, None).is_err());
        assert!(Branding::new(Some(&"x".repeat(81)), None, None, None, None).is_err());
        let b = Branding::new(Some(" Rail KG "), None, None, None, None).unwrap();
        assert_eq!(b.name(), "Rail KG");
    }
}
