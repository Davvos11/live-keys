//! SFZ curves: 128-point tables mapping a normalized controller value to a
//! modulation amount. Indices 0–6 are built in; `<curve>` headers add others.

/// A curve, sampled at 128 points over the controller's range 0..=1.
#[derive(Debug, Clone, PartialEq)]
pub struct Curve([f32; 128]);

impl Curve {
    /// The built-in curves (as in sfizz): 0 linear, 1 bipolar −1..1, 2 inverted,
    /// 3 bipolar inverted, 4 x², 5 √x, 6 √(1−x).
    pub fn builtin(index: usize) -> Option<Curve> {
        let f: fn(f32) -> f32 = match index {
            0 => |x| x,
            1 => |x| 2.0 * x - 1.0,
            2 => |x| 1.0 - x,
            3 => |x| 1.0 - 2.0 * x,
            4 => |x| x * x,
            5 => |x| x.sqrt(),
            6 => |x| (1.0 - x).sqrt(),
            _ => return None,
        };
        Some(Curve(std::array::from_fn(|i| f(i as f32 / 127.0))))
    }

    /// A curve through the given points (`v000=…`), linear between them. A
    /// missing first or last point defaults to 0 or 1.
    pub fn from_points(points: &[(u8, f32)]) -> Curve {
        let mut points: Vec<(u8, f32)> = points.iter().copied().filter(|&(i, _)| i < 128).collect();
        points.sort_by_key(|&(i, _)| i);
        points.dedup_by_key(|p| p.0);
        if points.first().is_none_or(|&(i, _)| i != 0) {
            points.insert(0, (0, 0.0));
        }
        if points.last().is_none_or(|&(i, _)| i != 127) {
            points.push((127, 1.0));
        }
        let mut table = [0.0; 128];
        for pair in points.windows(2) {
            let ((i0, v0), (i1, v1)) = (pair[0], pair[1]);
            for i in i0..=i1 {
                let t = (i - i0) as f32 / (i1 - i0) as f32;
                table[i as usize] = v0 + (v1 - v0) * t;
            }
        }
        Curve(table)
    }

    /// The curve at `x` (0..=1, clamped), interpolated between points.
    pub fn eval(&self, x: f32) -> f32 {
        let pos = x.clamp(0.0, 1.0) * 127.0;
        let i = pos as usize;
        if i >= 127 {
            return self.0[127];
        }
        let t = pos - i as f32;
        self.0[i] + (self.0[i + 1] - self.0[i]) * t
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_interpolate_and_endpoints_default() {
        // Salamander's curve 7: a step from 0 to 1.
        let c = Curve::from_points(&[(0, 0.0), (1, 1.0), (127, 1.0)]);
        assert_eq!(c.eval(0.0), 0.0);
        assert_eq!(c.eval(1.0 / 127.0), 1.0);
        assert_eq!(c.eval(0.5), 1.0);
        let c = Curve::from_points(&[(63, 0.5)]);
        assert_eq!(c.eval(0.0), 0.0);
        assert!((c.eval(63.0 / 127.0) - 0.5).abs() < 1e-6);
        assert_eq!(c.eval(1.0), 1.0);
    }
}
