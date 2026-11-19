//! Transient physical-button feedback. It never changes assignments.
use std::time::{Duration, Instant};

const AFTERGLOW: Duration = Duration::from_millis(850);

#[derive(Default)]
pub struct ButtonActivity {
    held: Vec<usize>,
    released: Vec<(usize, Instant)>,
    pub last: Option<usize>,
}

impl ButtonActivity {
    pub fn update(&mut self, mut pressed: Vec<usize>, count: usize, now: Instant) {
        pressed.retain(|i| *i < count);
        pressed.sort_unstable();
        pressed.dedup();
        for i in &pressed {
            if !self.held.contains(i) {
                self.last = Some(*i);
            }
        }
        self.released.retain(|(i, at)| {
            !pressed.contains(i) && now.saturating_duration_since(*at) < AFTERGLOW
        });
        for i in &self.held {
            if !pressed.contains(i) {
                self.released.push((*i, now));
            }
        }
        self.held = pressed;
    }

    pub fn held(&self, input: usize) -> bool {
        self.held.contains(&input)
    }

    pub fn glow(&self, input: usize, now: Instant) -> f32 {
        if self.held(input) {
            return 1.;
        }
        self.released
            .iter()
            .filter(|(i, _)| *i == input)
            .map(|(_, at)| {
                (1. - now.saturating_duration_since(*at).as_secs_f32() / AFTERGLOW.as_secs_f32())
                    .clamp(0., 1.)
            })
            .fold(0., f32::max)
    }

    pub fn fading(&self, now: Instant) -> bool {
        self.released
            .iter()
            .any(|(_, at)| now.saturating_duration_since(*at) < AFTERGLOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_tap_survives_a_frame_and_held_button_does_not_time_out() {
        let now = Instant::now();
        let mut activity = ButtonActivity::default();
        activity.update(vec![3], 11, now);
        assert_eq!(activity.glow(3, now + Duration::from_secs(10)), 1.);
        let release = now + Duration::from_secs(10);
        activity.update(vec![], 11, release);
        assert!(!activity.held(3));
        assert!(activity.glow(3, release + Duration::from_millis(16)) > 0.9);
        assert_eq!(activity.glow(3, release + AFTERGLOW), 0.);
        assert!(!activity.fading(release + AFTERGLOW));
        assert_eq!(activity.last, Some(3));
    }

    #[test]
    fn simultaneous_presses_duplicates_and_invalid_indices() {
        let now = Instant::now();
        let mut activity = ButtonActivity::default();
        activity.update(vec![3, 3, 900], 11, now);
        assert_eq!(activity.last, Some(3));
        activity.update(vec![3, 4], 11, now);
        assert_eq!(activity.last, Some(4));
        activity.update(vec![4], 11, now);
        assert!(activity.held(4));
        assert!(!activity.held(3));
        assert_eq!(activity.glow(3, now), 1.);
        activity.update(vec![3, 4], 11, now + AFTERGLOW);
        assert_eq!(activity.last, Some(3));
        assert!(!activity.fading(now + AFTERGLOW));
    }
}
