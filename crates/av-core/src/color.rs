/// A colour in hue (degrees, 0..360), saturation (0..1), value (0..1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

impl Hsv {
    pub const fn new(h: f32, s: f32, v: f32) -> Self {
        Self { h, s, v }
    }

    /// Advances the hue by `degrees`, wrapping into 0..360.
    pub fn rotate(&mut self, degrees: f32) {
        self.h = (self.h + degrees).rem_euclid(360.0);
    }

    /// Converts to sRGB-encoded components in 0..1.
    pub fn to_rgb(self) -> [f32; 3] {
        let h = if self.h >= 360.0 { 0.0 } else { self.h } / 60.0;
        let i = h as i32;
        let f = h - i as f32;
        let v = self.v;
        let p = v * (1.0 - self.s);
        let q = v * (1.0 - self.s * f);
        let t = v * (1.0 - self.s * (1.0 - f));
        match i {
            0 => [v, t, p],
            1 => [q, v, p],
            2 => [p, v, t],
            3 => [p, q, v],
            4 => [t, p, v],
            _ => [v, p, q],
        }
    }

    /// Converts from sRGB-encoded components in 0..1.
    pub fn from_rgb([r, g, b]: [f32; 3]) -> Self {
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let delta = max - min;
        if delta < 1e-5 || max <= 0.0 {
            return Self::new(0.0, 0.0, max);
        }
        let h = if r >= max {
            (g - b) / delta
        } else if g >= max {
            2.0 + (b - r) / delta
        } else {
            4.0 + (r - g) / delta
        };
        Self::new((h * 60.0).rem_euclid(360.0), delta / max, max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: [f32; 3], b: [f32; 3]) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-4)
    }

    #[test]
    fn primaries() {
        assert!(close(Hsv::new(0.0, 1.0, 1.0).to_rgb(), [1.0, 0.0, 0.0]));
        assert!(close(Hsv::new(120.0, 1.0, 1.0).to_rgb(), [0.0, 1.0, 0.0]));
        assert!(close(Hsv::new(240.0, 1.0, 1.0).to_rgb(), [0.0, 0.0, 1.0]));
        assert!(close(Hsv::new(0.0, 0.0, 0.5).to_rgb(), [0.5, 0.5, 0.5]));
    }

    #[test]
    fn round_trip() {
        for h in (0..360).step_by(15) {
            let hsv = Hsv::new(h as f32, 0.7, 0.8);
            let back = Hsv::from_rgb(hsv.to_rgb());
            assert!((back.h - hsv.h).abs() < 1e-2, "{hsv:?} -> {back:?}");
            assert!((back.s - hsv.s).abs() < 1e-4);
            assert!((back.v - hsv.v).abs() < 1e-4);
        }
    }

    #[test]
    fn rotate_wraps() {
        let mut c = Hsv::new(350.0, 1.0, 1.0);
        c.rotate(20.0);
        assert!((c.h - 10.0).abs() < 1e-4);
        c.rotate(-20.0);
        assert!((c.h - 350.0).abs() < 1e-4);
    }
}
