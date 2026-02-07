use crate::config::Config;
use rand::Rng;

/// Manages timing of periodic tasks with randomized jitter.
pub struct Scheduler {
    tick_count: u64,
    autopilot_interval: u64,
    rebalancer_interval: u64,
    judge_interval: u64,
    trigger_probability: f64,
    force_all: bool,
}

impl Scheduler {
    /// Create a normal scheduler with randomized intervals.
    pub fn new(config: &Config) -> Self {
        // Ticks are 10-minute intervals by default.
        // Autopilot runs ~every hour (6 ticks), rebalancer ~every 2 hours (12 ticks),
        // judge ~every 6 hours (36 ticks).
        Self {
            tick_count: 0,
            autopilot_interval: 6,
            rebalancer_interval: 12,
            judge_interval: 36,
            trigger_probability: config.rebalancer.trigger_probability,
            force_all: false,
        }
    }

    /// Create a scheduler that forces all modules to run (for run-once mode).
    pub fn new_force_all(config: &Config) -> Self {
        let mut s = Self::new(config);
        s.force_all = true;
        s
    }

    pub fn tick(&mut self) {
        self.tick_count += 1;
    }

    /// Should the autopilot module run this tick?
    pub fn should_run_autopilot(&self) -> bool {
        if self.force_all {
            return true;
        }
        self.tick_count % self.autopilot_interval == 0
    }

    /// Should the rebalancer module run this tick?
    /// Uses probabilistic triggering like CLBoss's EarningsRebalancer.
    pub fn should_run_rebalancer(&self) -> bool {
        if self.force_all {
            return true;
        }
        if self.tick_count % self.rebalancer_interval != 0 {
            return false;
        }
        // Probabilistic trigger (CLBoss uses 50% chance per hourly timer)
        let mut rng = rand::thread_rng();
        rng.gen::<f64>() < self.trigger_probability
    }

    /// Should the judge module run this tick?
    pub fn should_run_judge(&self) -> bool {
        if self.force_all {
            return true;
        }
        self.tick_count % self.judge_interval == 0
    }

    pub fn tick_count(&self) -> u64 {
        self.tick_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_config() -> Config {
        Config::test_default(std::path::PathBuf::from("/dev/null"))
    }

    #[test]
    fn test_tick_increments() {
        let config = test_config();
        let mut sched = Scheduler::new(&config);
        assert_eq!(sched.tick_count(), 0);
        sched.tick();
        assert_eq!(sched.tick_count(), 1);
        sched.tick();
        assert_eq!(sched.tick_count(), 2);
    }

    #[test]
    fn test_autopilot_runs_at_correct_interval() {
        let config = test_config();
        let mut sched = Scheduler::new(&config);
        // At tick 0, should run (0 % 6 == 0)
        assert!(sched.should_run_autopilot());
        // Ticks 1-5 should not run
        for _ in 0..5 {
            sched.tick();
            assert!(!sched.should_run_autopilot(), "tick {}", sched.tick_count());
        }
        // Tick 6 should run
        sched.tick();
        assert_eq!(sched.tick_count(), 6);
        assert!(sched.should_run_autopilot());
    }

    #[test]
    fn test_judge_runs_at_correct_interval() {
        let config = test_config();
        let mut sched = Scheduler::new(&config);
        // Tick 0: run
        assert!(sched.should_run_judge());
        // Skip to tick 35: shouldn't run
        for _ in 0..35 {
            sched.tick();
        }
        assert!(!sched.should_run_judge());
        // Tick 36: should run
        sched.tick();
        assert_eq!(sched.tick_count(), 36);
        assert!(sched.should_run_judge());
    }

    #[test]
    fn test_force_all_always_runs() {
        let config = test_config();
        let mut sched = Scheduler::new_force_all(&config);
        // Force mode should always return true
        assert!(sched.should_run_autopilot());
        assert!(sched.should_run_rebalancer());
        assert!(sched.should_run_judge());

        sched.tick();
        assert!(sched.should_run_autopilot());
        assert!(sched.should_run_rebalancer());
        assert!(sched.should_run_judge());
    }

    #[test]
    fn test_rebalancer_interval_gating() {
        let config = test_config();
        let mut sched = Scheduler::new(&config);
        // At tick 1, rebalancer should never run (1 % 12 != 0)
        sched.tick();
        // Even if probability were 1.0, interval gate says no
        assert!(!sched.should_run_rebalancer());
    }
}
