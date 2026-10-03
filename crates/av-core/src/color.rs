/// A colour in hue (degrees), saturation and lightness (0 to 1), as CSS uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsl {
    pub h: f32,
    pub s: f32,
    pub l: f32,
}

impl Hsl {
    pub const fn new(h: f32, s: f32, l: f32) -> Self {
        Self { h, s, l }
    }

    /// The same hue and saturation, lightness shifted by `dl` (clamped).
    pub fn lighten(self, dl: f32) -> Self {
        Self::new(self.h, self.s, (self.l + dl).clamp(0.0, 1.0))
    }

    /// Converts to sRGB-encoded components in 0..1.
    pub fn to_rgb(self) -> [f32; 3] {
        let h = self.h.rem_euclid(360.0) / 360.0;
        let (s, l) = (self.s.clamp(0.0, 1.0), self.l.clamp(0.0, 1.0));
        if s == 0.0 {
            return [l; 3];
        }
        let q = if l < 0.5 {
            l * (1.0 + s)
        } else {
            l + s - l * s
        };
        let p = 2.0 * l - q;
        let channel = |t: f32| {
            let t = t.rem_euclid(1.0);
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
        [channel(h + 1.0 / 3.0), channel(h), channel(h - 1.0 / 3.0)]
    }
}

/// Colour schemes across the spectrum, low frequencies at `t = 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Palette {
    /// Warm amber through red to magenta.
    #[default]
    Ember,
    /// Teal through blue to violet.
    Aurora,
    /// A blue-to-violet band whose hue slowly turns over time.
    Drift,
}

impl Palette {
    /// Hue drift for [`Palette::Drift`], in degrees per second.
    const DRIFT_SPEED: f32 = 10.0;

    /// Colour at position `t` (0 to 1) across the spectrum, `time` seconds in.
    pub fn color(self, t: f32, time: f32) -> Hsl {
        let t = t.clamp(0.0, 1.0);
        match self {
            Self::Ember => Hsl::new(38.0 - 70.0 * t, 0.92, 0.60),
            Self::Aurora => Hsl::new(168.0 + 112.0 * t, 0.78, 0.62),
            Self::Drift => Hsl::new(210.0 + Self::DRIFT_SPEED * time + 50.0 * t, 0.72, 0.62),
        }
    }
}

impl std::str::FromStr for Palette {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "ember" => Ok(Self::Ember),
            "aurora" => Ok(Self::Aurora),
            "drift" => Ok(Self::Drift),
            _ => Err(format!(
                "unknown palette {s:?} (try ember, aurora or drift)"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_matches_css() {
        // Reference values from CSS hsl().
        let cases = [
            (Hsl::new(0.0, 1.0, 0.5), [1.0, 0.0, 0.0]),
            (Hsl::new(120.0, 1.0, 0.25), [0.0, 0.5, 0.0]),
            (Hsl::new(240.0, 1.0, 0.75), [0.5, 0.5, 1.0]),
            (Hsl::new(38.0, 0.92, 0.6), [0.968, 0.698, 0.232]),
            (Hsl::new(-32.0, 0.92, 0.6), [0.968, 0.232, 0.625]),
        ];
        for (hsl, rgb) in cases {
            assert!(close(hsl.to_rgb(), rgb), "{hsl:?} -> {:?}", hsl.to_rgb());
        }
    }

    #[test]
    fn palettes_parse_and_span_the_spectrum() {
        assert_eq!("Ember".parse::<Palette>(), Ok(Palette::Ember));
        assert!("neon".parse::<Palette>().is_err());
        let lo = Palette::Ember.color(0.0, 0.0);
        let hi = Palette::Ember.color(1.0, 0.0);
        assert!((lo.h - 38.0).abs() < 1e-4 && (hi.h + 32.0).abs() < 1e-4);
        // Drift turns with time; the others don't.
        assert_ne!(
            Palette::Drift.color(0.5, 0.0),
            Palette::Drift.color(0.5, 3.0)
        );
        assert_eq!(
            Palette::Ember.color(0.5, 0.0),
            Palette::Ember.color(0.5, 3.0)
        );
    }

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 2e-3)
    }
}
