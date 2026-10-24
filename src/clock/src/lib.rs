use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use simccxt_core::Config;

/// Time control modes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimeMode {
    /// Real-time (wall clock) mode
    RealTime,
    /// Simulated time with controlled stepping
    Simulated,
}

/// Clock state - controls time for the simulation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clock {
    /// Current time in the simulation
    current_time: DateTime<Utc>,
    /// Mode of operation
    mode: TimeMode,
    /// Whether the clock is paused
    paused: bool,
    /// Step size for simulated time
    step_ms: u64,
    /// Maximum time boundary
    end_time: Option<DateTime<Utc>>,
}

impl Clock {
    /// Create a new clock with the given configuration
    pub fn new(config: &Config) -> Self {
        Self {
            current_time: config.time_config.initial_time,
            mode: TimeMode::Simulated,
            paused: config.time_config.paused,
            step_ms: config.time_config.step_ms,
            end_time: None,
        }
    }

    /// Create a new real-time clock
    pub fn real_time() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            current_time: Utc::now(),
            #[cfg(target_arch = "wasm32")]
            current_time: DateTime::<Utc>::from_timestamp_millis(0)
                .expect("the Unix epoch is a valid timestamp"),
            mode: TimeMode::RealTime,
            paused: false,
            step_ms: 0,
            end_time: None,
        }
    }

    /// Create a new simulated clock starting at the given time
    pub fn simulated(start: DateTime<Utc>, step_ms: u64) -> Self {
        Self {
            current_time: start,
            mode: TimeMode::Simulated,
            paused: true,
            step_ms,
            end_time: None,
        }
    }

    /// Create a new simulated clock with end time
    pub fn simulated_with_end(start: DateTime<Utc>, end: DateTime<Utc>, step_ms: u64) -> Self {
        Self {
            current_time: start,
            mode: TimeMode::Simulated,
            paused: true,
            step_ms,
            end_time: Some(end),
        }
    }

    /// Get the current time
    pub fn now(&self) -> DateTime<Utc> {
        match self.mode {
            #[cfg(target_arch = "wasm32")]
            TimeMode::RealTime => self.current_time,
            #[cfg(not(target_arch = "wasm32"))]
            TimeMode::RealTime => {
                if self.paused {
                    self.current_time
                } else {
                    Utc::now()
                }
            }
            TimeMode::Simulated => self.current_time,
        }
    }

    /// Get the current time as Unix timestamp in milliseconds
    pub fn now_ms(&self) -> i64 {
        self.now().timestamp_millis()
    }

    /// Advance time by one step
    pub fn step(&mut self) -> bool {
        if self.paused {
            return false;
        }
        self.advance(Duration::milliseconds(self.step_ms as i64))
    }

    /// Advance time by n steps
    pub fn step_n(&mut self, n: u64) -> u64 {
        if self.paused {
            return 0;
        }
        let mut steps_taken = 0;
        for _ in 0..n {
            if self.advance(Duration::milliseconds(self.step_ms as i64)) {
                steps_taken += 1;
            } else {
                break;
            }
        }
        steps_taken
    }

    /// Advance to a specific timestamp
    pub fn seek(&mut self, timestamp: DateTime<Utc>) -> bool {
        if self.mode != TimeMode::Simulated {
            return false;
        }
        if timestamp <= self.current_time {
            return false;
        }
        self.current_time = timestamp;
        true
    }

    /// Reset the clock to its initial time
    pub fn reset(&mut self, initial_time: DateTime<Utc>) {
        self.current_time = initial_time;
        self.paused = true;
    }

    /// Pause the clock
    pub fn pause(&mut self) {
        self.paused = true;
    }

    /// Resume the clock
    pub fn resume(&mut self) {
        if self.mode == TimeMode::Simulated {
            self.paused = false;
        }
    }

    /// Set the step size in milliseconds
    pub fn set_step_ms(&mut self, ms: u64) {
        self.step_ms = ms;
    }

    /// Set the end time
    pub fn set_end_time(&mut self, end: DateTime<Utc>) {
        self.end_time = Some(end);
    }

    /// Run the clock to the end or until paused
    pub fn run(&mut self) -> bool {
        if self.mode != TimeMode::Simulated {
            return false;
        }
        self.paused = false;
        true
    }

    /// Internal helper to advance to a duration
    fn advance(&mut self, duration: Duration) -> bool {
        let new_time = self.current_time + duration;
        if let Some(end) = self.end_time {
            if new_time > end {
                self.current_time = end;
                self.paused = true;
                return false;
            }
        }
        self.current_time = new_time;
        true
    }

    /// Check if the clock has reached its end time
    pub fn is_done(&self) -> bool {
        if let Some(end) = self.end_time {
            self.now() >= end
        } else {
            false
        }
    }

    /// Get the time mode
    pub fn mode(&self) -> TimeMode {
        self.mode
    }

    /// Get the step size
    pub fn step_ms(&self) -> u64 {
        self.step_ms
    }

    /// Check if paused
    pub fn is_paused(&self) -> bool {
        self.paused
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            #[cfg(not(target_arch = "wasm32"))]
            current_time: Utc::now(),
            #[cfg(target_arch = "wasm32")]
            current_time: DateTime::<Utc>::from_timestamp_millis(0)
                .expect("the Unix epoch is a valid timestamp"),
            mode: TimeMode::Simulated,
            paused: true,
            step_ms: 1000,
            end_time: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn test_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap()
    }

    #[test]
    fn test_simulated_clock_initial_state() {
        let clock = Clock::simulated(test_time(), 1000);
        assert_eq!(clock.now(), test_time());
        assert!(clock.is_paused());
        assert_eq!(clock.step_ms(), 1000);
    }

    #[test]
    fn test_step_advances_time() {
        let mut clock = Clock::simulated(test_time(), 1000);
        clock.resume();
        assert!(!clock.is_paused());
        assert!(clock.step());
        assert_eq!(clock.now(), test_time() + Duration::milliseconds(1000));
    }

    #[test]
    fn test_step_when_paused_does_nothing() {
        let mut clock = Clock::simulated(test_time(), 1000);
        assert!(!clock.step());
        assert_eq!(clock.now(), test_time());
    }

    #[test]
    fn test_step_n_advances_multiple() {
        let mut clock = Clock::simulated(test_time(), 1000);
        clock.resume();
        let steps = clock.step_n(5);
        assert_eq!(steps, 5);
        assert_eq!(clock.now(), test_time() + Duration::milliseconds(5000));
    }

    #[test]
    fn test_step_n_stops_at_end() {
        let end = test_time() + Duration::milliseconds(3000);
        let mut clock = Clock::simulated_with_end(test_time(), end, 1000);
        clock.resume();
        let steps = clock.step_n(10);
        assert_eq!(steps, 3);
        assert_eq!(clock.now(), end);
        assert!(clock.is_done());
        assert!(clock.is_paused());
    }

    #[test]
    fn test_seek() {
        let mut clock = Clock::simulated(test_time(), 1000);
        let target = test_time() + Duration::hours(2);
        assert!(clock.seek(target));
        assert_eq!(clock.now(), target);
    }

    #[test]
    fn test_seek_backwards_fails() {
        let mut clock = Clock::simulated(test_time(), 1000);
        clock.resume();
        clock.step();
        let past = test_time();
        assert!(!clock.seek(past));
    }

    #[test]
    fn test_pause_resume() {
        let mut clock = Clock::simulated(test_time(), 1000);
        clock.resume();
        clock.step();
        clock.pause();
        assert!(clock.is_paused());
        let t = clock.now();
        clock.step();
        assert_eq!(clock.now(), t);
        clock.resume();
        clock.step();
        assert_ne!(clock.now(), t);
    }

    #[test]
    fn test_reset() {
        let mut clock = Clock::simulated(test_time(), 1000);
        clock.resume();
        clock.step_n(10);
        clock.reset(test_time());
        assert_eq!(clock.now(), test_time());
        assert!(clock.is_paused());
    }

    #[test]
    fn test_run() {
        let mut clock = Clock::simulated(test_time(), 1000);
        assert!(clock.run());
        assert!(!clock.is_paused());
    }

    #[test]
    fn test_now_ms() {
        let clock = Clock::simulated(test_time(), 1000);
        assert_eq!(clock.now_ms(), test_time().timestamp_millis());
    }

    #[test]
    fn test_default_clock() {
        let clock = Clock::default();
        assert_eq!(clock.mode(), TimeMode::Simulated);
        assert!(clock.is_paused());
    }
}
