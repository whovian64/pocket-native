//! Host pacing only: guest clocks, frame work and game code stay unchanged.
use std::{collections::HashSet, time::Duration};

use crate::input::{FN, L3, Y};

#[derive(Default)]
pub(crate) struct Playback {
    pub fast_forward: bool,
    chord: bool,
    consumed_y: bool,
}

impl Playback {
    /// Toggle once per chord, in either press order. Keep Y out of the
    /// game until released, even if the modifier is released first.
    pub fn input(&mut self, keys: &HashSet<u16>) -> bool {
        let chord = keys.contains(&Y) && (keys.contains(&FN) || keys.contains(&L3));
        let changed = chord && !self.chord;
        if changed {
            self.fast_forward = !self.fast_forward;
        }
        self.chord = chord;
        self.consumed_y = keys.contains(&Y) && (self.consumed_y || chord);
        changed
    }

    pub fn filter_game_keys(&self, keys: &mut HashSet<u16>) {
        if self.consumed_y {
            keys.remove(&Y);
        }
    }

    pub fn speed(&self) -> u32 {
        if self.fast_forward { 2 } else { 1 }
    }

    pub fn interval(&self, panel_period: Duration) -> Duration {
        panel_period / self.speed()
    }

    /// Count guest frames between presentations, keeping the selected
    /// presentation cap in wall time even at twice the guest frame rate.
    pub fn presentation_stride(&self, cap: f64) -> u64 {
        (60.0 * self.speed() as f64 / cap).ceil().max(1.0) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::A;

    #[test]
    fn both_modifiers_toggle_once_in_either_press_order_and_consume_y() {
        for modifier in [FN, L3] {
            for first in [modifier, Y] {
                let mut playback = Playback::default();
                assert!(!playback.input(&HashSet::from([first])));
                let chord = HashSet::from([modifier, Y]);
                assert!(playback.input(&chord));
                assert!(playback.fast_forward);
                assert!(!playback.input(&chord));
                let mut released_modifier = HashSet::from([Y, A]);
                assert!(!playback.input(&released_modifier));
                playback.filter_game_keys(&mut released_modifier);
                assert_eq!(released_modifier, HashSet::from([A]));
                playback.input(&HashSet::new());
                assert!(playback.input(&chord));
                assert!(!playback.fast_forward);
                playback.input(&HashSet::new());
                let mut ordinary_y = HashSet::from([Y]);
                playback.input(&ordinary_y);
                playback.filter_game_keys(&mut ordinary_y);
                assert_eq!(ordinary_y, HashSet::from([Y]));
            }
        }
    }

    #[test]
    fn twice_the_guest_frames_with_the_same_presentation_budget() {
        let panel_period = Duration::from_secs_f64(1.0 / 60.0);
        let mut playback = Playback::default();
        for (fast, frames) in [(false, 60), (true, 120), (false, 60)] {
            playback.fast_forward = fast;
            let elapsed = playback.interval(panel_period) * frames;
            assert!(elapsed.abs_diff(Duration::from_secs(1)) < Duration::from_micros(1));
            for cap in [20, 30, 60] {
                assert_eq!(frames as u64 / playback.presentation_stride(cap as f64), cap);
            }
        }
    }
}
