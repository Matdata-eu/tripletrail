//! Server branding (#262): the name, favicon, logo and accent colour an operator gives the web UI
//! and the other HTML pages (`--site-name`, `--site-favicon`, `--site-logo`, `--site-logo-dark`,
//! `--site-color`).
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
        })
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
            },
            "version": env!("CARGO_PKG_VERSION"),
        })
    }
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
    fn names_are_one_short_line() {
        assert!(Branding::new(Some("a\nb"), None, None, None, None).is_err());
        assert!(Branding::new(Some(&"x".repeat(81)), None, None, None, None).is_err());
        let b = Branding::new(Some(" Rail KG "), None, None, None, None).unwrap();
        assert_eq!(b.name(), "Rail KG");
    }
}
