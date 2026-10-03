//! The peak balls that sit on top of the bars.
//!
//! While music plays, each ball rides its bar's peak marker. When playback
//! stops (pause, end, silence) the balls come loose and drift slowly in random,
//! gently wandering directions, bouncing softly off the edges. When sound comes
//! back they glide home to their bars.

use std::f32::consts::TAU;

/// One ball. `x` is the centre as a fraction of the width (0 to 1); `y` is the
/// height above the floor in bar-height units (1.0 = the tallest bar).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ball {
    pub x: f32,
    pub y: f32,
    heading: f32,
    speed: f32,
    mode: Mode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Attached,
    Drifting,
    Returning,
}

#[derive(Debug, Clone)]
pub struct Balls {
    balls: Vec<Ball>,
    rng: u64,
}

impl Balls {
    /// Drift speed range, in bar heights per second.
    const MIN_SPEED: f32 = 0.025;
    const MAX_SPEED: f32 = 0.07;
    /// How much the heading wanders, in radians per square-root second.
    const WANDER: f32 = 1.1;
    /// How fast a returning ball closes the gap (per 60 Hz frame).
    const RETURN_RATE: f32 = 0.12;
    /// Highest a drifting ball goes, in bar heights above the floor.
    const CEILING: f32 = 1.08;
    /// Lowest a drifting ball goes (just above the floor).
    const FLOOR: f32 = 0.02;

    pub fn new(count: usize, seed: u64) -> Self {
        let mut balls = Self {
            balls: Vec::with_capacity(count),
            rng: seed | 1,
        };
        balls.balls = (0..count)
            .map(|i| Ball {
                x: home_x(i, count),
                y: 0.0,
                heading: 0.0,
                speed: 0.0,
                mode: Mode::Attached,
            })
            .collect();
        balls
    }

    pub fn balls(&self) -> &[Ball] {
        &self.balls
    }

    /// Advances by `dt` seconds. `peaks` are the bars' peak heights (one per
    /// ball); `idle` sets the balls drifting; `aspect` is the drawing area's
    /// width over the bar height in pixels, so drift is equally fast in x and y.
    pub fn update(&mut self, peaks: &[f32], idle: bool, aspect: f32, dt: f32) {
        assert_eq!(peaks.len(), self.balls.len(), "one peak per ball");
        let n = self.balls.len();
        let aspect = aspect.max(1e-3);
        for (i, &peak) in peaks.iter().enumerate() {
            let home = home_x(i, n);
            let mut ball = self.balls[i];
            match (ball.mode, idle) {
                (Mode::Attached | Mode::Returning, true) => {
                    ball.mode = Mode::Drifting;
                    ball.heading = self.random() * TAU;
                    ball.speed =
                        Self::MIN_SPEED + self.random() * (Self::MAX_SPEED - Self::MIN_SPEED);
                }
                (Mode::Drifting, false) => ball.mode = Mode::Returning,
                _ => {}
            }
            match ball.mode {
                Mode::Attached => {
                    ball.x = home;
                    ball.y = peak;
                }
                Mode::Drifting => {
                    // A random walk on the heading keeps paths gently curving.
                    ball.heading += (self.random() - 0.5) * 2.0 * Self::WANDER * dt.sqrt();
                    ball.x += ball.heading.cos() * ball.speed * dt / aspect;
                    ball.y += ball.heading.sin() * ball.speed * dt;
                    // Soft bounces: mirror the heading at the walls.
                    if ball.x < 0.0 || ball.x > 1.0 {
                        ball.heading = std::f32::consts::PI - ball.heading;
                        ball.x = ball.x.clamp(0.0, 1.0);
                    }
                    if ball.y < Self::FLOOR || ball.y > Self::CEILING {
                        ball.heading = -ball.heading;
                        ball.y = ball.y.clamp(Self::FLOOR, Self::CEILING);
                    }
                }
                Mode::Returning => {
                    let t = 1.0 - (1.0 - Self::RETURN_RATE).powf(dt * 60.0);
                    ball.x += (home - ball.x) * t;
                    ball.y += (peak - ball.y) * t;
                    let dx = (home - ball.x) * aspect;
                    if dx.abs() < 0.005 && (peak - ball.y).abs() < 0.005 {
                        ball.mode = Mode::Attached;
                    }
                }
            }
            self.balls[i] = ball;
        }
    }

    /// Uniform in 0..1 (xorshift64*).
    fn random(&mut self) -> f32 {
        self.rng ^= self.rng >> 12;
        self.rng ^= self.rng << 25;
        self.rng ^= self.rng >> 27;
        let r = self.rng.wrapping_mul(0x2545_F491_4F6C_DD1D);
        (r >> 40) as f32 / (1u64 << 24) as f32
    }
}

/// Centre of bar `i` of `n`, as a fraction of the width.
fn home_x(i: usize, n: usize) -> f32 {
    (i as f32 + 0.5) / n as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    const DT: f32 = 1.0 / 60.0;
    const ASPECT: f32 = 2.0;

    #[test]
    fn attached_balls_ride_the_peaks() {
        let mut b = Balls::new(4, 1);
        b.update(&[0.1, 0.5, 0.9, 0.0], false, ASPECT, DT);
        let ys: Vec<f32> = b.balls().iter().map(|b| b.y).collect();
        assert_eq!(ys, [0.1, 0.5, 0.9, 0.0]);
        assert_eq!(b.balls()[1].x, 0.375);
    }

    #[test]
    fn idle_balls_drift_slowly_and_stay_inside() {
        let mut b = Balls::new(16, 7);
        let peaks = [0.6; 16];
        b.update(&peaks, false, ASPECT, DT);
        let start: Vec<Ball> = b.balls().to_vec();
        for _ in 0..(60 * 120) {
            b.update(&peaks, true, ASPECT, DT);
            for ball in b.balls() {
                assert!((0.0..=1.0).contains(&ball.x), "{ball:?}");
                assert!(
                    (Balls::FLOOR..=Balls::CEILING).contains(&ball.y),
                    "{ball:?}"
                );
            }
        }
        // Every ball has moved, in different directions.
        let moved: Vec<(f32, f32)> = b
            .balls()
            .iter()
            .zip(&start)
            .map(|(now, then)| ((now.x - then.x) * ASPECT, now.y - then.y))
            .collect();
        assert!(
            moved.iter().all(|(dx, dy)| dx.hypot(*dy) > 0.01),
            "{moved:?}"
        );
        let rightward = moved.iter().filter(|(dx, _)| *dx > 0.0).count();
        assert!(
            (2..=14).contains(&rightward),
            "directions look biased: {moved:?}"
        );
    }

    #[test]
    fn one_second_of_drift_is_gentle() {
        let mut b = Balls::new(8, 3);
        let peaks = [0.5; 8];
        b.update(&peaks, false, ASPECT, DT);
        for _ in 0..60 {
            b.update(&peaks, true, ASPECT, DT);
        }
        for (i, ball) in b.balls().iter().enumerate() {
            let dx = (ball.x - home_x(i, 8)) * ASPECT;
            let dist = dx.hypot(ball.y - 0.5);
            assert!(dist <= Balls::MAX_SPEED * 1.05, "ball {i} moved {dist}");
        }
    }

    #[test]
    fn balls_glide_home_when_sound_returns() {
        let mut b = Balls::new(8, 9);
        let peaks = [0.4; 8];
        for _ in 0..600 {
            b.update(&peaks, true, ASPECT, DT);
        }
        // A few frames after resuming they are on their way, not yet home.
        b.update(&peaks, false, ASPECT, DT);
        assert!(b.balls().iter().any(|ball| ball.mode == Mode::Returning));
        for _ in 0..120 {
            b.update(&peaks, false, ASPECT, DT);
        }
        for (i, ball) in b.balls().iter().enumerate() {
            assert_eq!(ball.mode, Mode::Attached, "ball {i}");
            assert_eq!((ball.x, ball.y), (home_x(i, 8), 0.4));
        }
    }
}
