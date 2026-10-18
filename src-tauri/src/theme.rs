//! Theme tokens: defaults, validation and WCAG contrast checks (contract §6).

use serde_json::{Map, Value, json};

pub const COLOR_KEYS: &[&str] = &[
    "bg",
    "surface",
    "surface-2",
    "text",
    "text-muted",
    "accent",
    "accent-text",
    "border",
    "success",
    "warning",
    "danger",
];
pub const OTHER_KEYS: &[&str] = &["radius", "font", "font-mono", "density"];

/// Pairs (foreground, background) that must reach a 4.5:1 contrast ratio.
const CONTRAST_PAIRS: &[(&str, &str)] = &[("text", "bg"), ("text", "surface"), ("accent-text", "accent")];
pub const MIN_CONTRAST: f64 = 4.5;

/// Light, neutral defaults that read as a blank canvas.
pub fn defaults() -> Map<String, Value> {
    let v = json!({
        "bg": "#f7f7f5",
        "surface": "#ffffff",
        "surface-2": "#efefec",
        "text": "#1d1d1f",
        "text-muted": "#6b6b70",
        "accent": "#3e63dd",
        "accent-text": "#ffffff",
        "border": "#e2e2de",
        "success": "#2b8a3e",
        "warning": "#b7791f",
        "danger": "#d0312d",
        "radius": 10,
        "font": "-apple-system, BlinkMacSystemFont, \"Inter\", \"Segoe UI\", system-ui, sans-serif",
        "font-mono": "\"SF Mono\", ui-monospace, Menlo, Consolas, monospace",
        "density": "normal"
    });
    match v {
        Value::Object(m) => m,
        _ => unreachable!(),
    }
}

/// Defaults merged with overrides.
pub fn resolve(overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut out = defaults();
    for (k, v) in overrides {
        out.insert(k.clone(), v.clone());
    }
    out
}

/// Merge `tokens` into `current` overrides and validate the result.
/// A `null` value resets that token to its default.
/// Returns the new overrides map or a list of human-readable problems.
pub fn merge_and_validate(
    current: &Map<String, Value>,
    tokens: &Map<String, Value>,
) -> Result<Map<String, Value>, Vec<String>> {
    let mut problems = Vec::new();
    let mut next = current.clone();
    for (key, value) in tokens {
        if !COLOR_KEYS.contains(&key.as_str()) && !OTHER_KEYS.contains(&key.as_str()) {
            problems.push(format!(
                "unknown token \"{key}\" (allowed: {}, {})",
                COLOR_KEYS.join(", "),
                OTHER_KEYS.join(", ")
            ));
            continue;
        }
        if value.is_null() {
            next.remove(key);
            continue;
        }
        match validate_value(key, value) {
            Ok(v) => {
                next.insert(key.clone(), v);
            }
            Err(e) => problems.push(format!("{key}: {e}")),
        }
    }
    if !problems.is_empty() {
        return Err(problems);
    }
    let resolved = resolve(&next);
    problems.extend(contrast_problems(&resolved));
    if problems.is_empty() { Ok(next) } else { Err(problems) }
}

fn validate_value(key: &str, value: &Value) -> Result<Value, String> {
    if COLOR_KEYS.contains(&key) {
        let s = value
            .as_str()
            .ok_or_else(|| format!("expected a CSS color string, got {value}"))?
            .trim();
        parse_color(s)?;
        return Ok(Value::String(s.to_string()));
    }
    match key {
        "radius" => {
            let n = value
                .as_f64()
                .ok_or_else(|| format!("expected a number of px (e.g. 8), got {value}"))?;
            if !(0.0..=64.0).contains(&n) {
                return Err(format!("radius must be between 0 and 64 px, got {n}"));
            }
            Ok(value.clone())
        }
        "font" | "font-mono" => {
            let s = value
                .as_str()
                .ok_or_else(|| format!("expected a font-family string, got {value}"))?
                .trim();
            if s.is_empty() || s.len() > 300 {
                return Err("font-family must be 1..300 characters".into());
            }
            if s.chars().any(|c| matches!(c, ';' | '{' | '}' | '<' | '>' | '\\')) {
                return Err("font-family must not contain ; { } < > or \\".into());
            }
            Ok(Value::String(s.to_string()))
        }
        "density" => match value.as_str() {
            Some("compact" | "normal" | "comfy") => Ok(value.clone()),
            _ => Err(format!("expected \"compact\", \"normal\" or \"comfy\", got {value}")),
        },
        _ => Err("unknown token".into()),
    }
}

/// Parsed color. `rgb` is Some when contrast can be computed (hex, rgb(), hsl()).
pub struct Color {
    pub rgb: Option<[f64; 3]>,
}

pub fn parse_color(s: &str) -> Result<Color, String> {
    let lower = s.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix('#') {
        let digits: Vec<u8> = hex
            .chars()
            .map(|c| c.to_digit(16).map(|d| d as u8))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("\"{s}\" is not valid hex"))?;
        let rgb = match digits.len() {
            3 => [digits[0] * 17, digits[1] * 17, digits[2] * 17],
            6 => [
                digits[0] * 16 + digits[1],
                digits[2] * 16 + digits[3],
                digits[4] * 16 + digits[5],
            ],
            _ => return Err(format!("\"{s}\": use #rgb or #rrggbb")),
        };
        return Ok(Color { rgb: Some(rgb.map(|c| c as f64 / 255.0)) });
    }
    let (name, args) = split_fn(&lower).ok_or_else(|| {
        format!("\"{s}\" is not a supported color; use #rgb, #rrggbb, rgb(), hsl() or oklch()")
    })?;
    let parts = fn_args(args);
    match name {
        "rgb" | "rgba" => {
            if parts.len() < 3 || parts.len() > 4 {
                return Err(format!("\"{s}\": rgb() needs 3 components (+ optional alpha)"));
            }
            let mut rgb = [0.0; 3];
            for i in 0..3 {
                let p = parts[i];
                let v = if let Some(pc) = p.strip_suffix('%') {
                    num(pc, s)? / 100.0
                } else {
                    num(p, s)? / 255.0
                };
                if !(0.0..=1.0).contains(&v) {
                    return Err(format!("\"{s}\": rgb component out of range"));
                }
                rgb[i] = v;
            }
            if parts.len() == 4 {
                alpha(parts[3], s)?;
            }
            Ok(Color { rgb: Some(rgb) })
        }
        "hsl" | "hsla" => {
            if parts.len() < 3 || parts.len() > 4 {
                return Err(format!("\"{s}\": hsl() needs 3 components (+ optional alpha)"));
            }
            let h = num(parts[0].trim_end_matches("deg"), s)?;
            let sat = num(parts[1].trim_end_matches('%'), s)? / 100.0;
            let l = num(parts[2].trim_end_matches('%'), s)? / 100.0;
            if !(0.0..=1.0).contains(&sat) || !(0.0..=1.0).contains(&l) {
                return Err(format!("\"{s}\": saturation/lightness must be 0%..100%"));
            }
            if parts.len() == 4 {
                alpha(parts[3], s)?;
            }
            Ok(Color { rgb: Some(hsl_to_rgb(h, sat, l)) })
        }
        "oklch" => {
            if parts.len() < 3 || parts.len() > 4 {
                return Err(format!("\"{s}\": oklch() needs L C H (+ optional alpha)"));
            }
            let l = parts[0];
            if let Some(pc) = l.strip_suffix('%') {
                num(pc, s)?;
            } else {
                num(l, s)?;
            }
            num(parts[1].trim_end_matches('%'), s)?;
            num(parts[2].trim_end_matches("deg"), s)?;
            if parts.len() == 4 {
                alpha(parts[3], s)?;
            }
            Ok(Color { rgb: None })
        }
        _ => Err(format!(
            "\"{s}\" is not a supported color; use #rgb, #rrggbb, rgb(), hsl() or oklch()"
        )),
    }
}

fn split_fn(s: &str) -> Option<(&str, &str)> {
    let open = s.find('(')?;
    let inner = s[open + 1..].strip_suffix(')')?;
    Some((s[..open].trim(), inner))
}

fn fn_args(args: &str) -> Vec<&str> {
    args.split(|c: char| c == ',' || c == '/' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .collect()
}

fn num(p: &str, s: &str) -> Result<f64, String> {
    p.trim()
        .parse::<f64>()
        .ok()
        .filter(|n| n.is_finite())
        .ok_or_else(|| format!("\"{s}\": \"{p}\" is not a number"))
}

fn alpha(p: &str, s: &str) -> Result<(), String> {
    let v = if let Some(pc) = p.strip_suffix('%') { num(pc, s)? / 100.0 } else { num(p, s)? };
    if (0.0..=1.0).contains(&v) { Ok(()) } else { Err(format!("\"{s}\": alpha must be 0..1")) }
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [f64; 3] {
    let h = ((h % 360.0) + 360.0) % 360.0 / 360.0;
    if s == 0.0 {
        return [l, l, l];
    }
    let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
    let p = 2.0 * l - q;
    let f = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    [f(h + 1.0 / 3.0), f(h), f(h - 1.0 / 3.0)]
}

fn luminance(rgb: [f64; 3]) -> f64 {
    let lin = |c: f64| {
        if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(rgb[0]) + 0.7152 * lin(rgb[1]) + 0.0722 * lin(rgb[2])
}

pub fn contrast_ratio(a: [f64; 3], b: [f64; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// Contrast problems in a resolved theme.
pub fn contrast_problems(resolved: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    for (fg, bg) in CONTRAST_PAIRS {
        let (Some(fs), Some(bs)) = (
            resolved.get(*fg).and_then(Value::as_str),
            resolved.get(*bg).and_then(Value::as_str),
        ) else {
            continue;
        };
        let (Ok(fc), Ok(bc)) = (parse_color(fs), parse_color(bs)) else { continue };
        if let (Some(f), Some(b)) = (fc.rgb, bc.rgb) {
            let ratio = contrast_ratio(f, b);
            if ratio < MIN_CONTRAST {
                out.push(format!(
                    "contrast of {fg} ({fs}) on {bg} ({bs}) is {ratio:.2}:1; it must be at least {MIN_CONTRAST}:1. Make {fg} darker or {bg} lighter (or the reverse for a dark theme)."
                ));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_pass() {
        assert!(contrast_problems(&defaults()).is_empty());
    }

    #[test]
    fn rejects_low_contrast() {
        let mut t = Map::new();
        t.insert("text".into(), json!("#cccccc"));
        let err = merge_and_validate(&Map::new(), &t).unwrap_err();
        assert!(err[0].contains("contrast"), "{err:?}");
    }

    #[test]
    fn parses_formats() {
        for c in ["#fff", "#a1b2c3", "rgb(10, 20, 30)", "rgb(10 20 30 / 50%)", "hsl(210 40% 50%)", "oklch(0.7 0.1 200)"] {
            parse_color(c).unwrap();
        }
        assert!(parse_color("red").is_err());
        assert!(parse_color("#12345").is_err());
    }
}
