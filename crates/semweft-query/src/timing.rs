use std::time::Instant;

pub(crate) struct Timing {
    stage: &'static str,
    started: Option<Instant>,
}

impl Timing {
    pub(crate) fn new(stage: &'static str) -> Self {
        Self {
            stage,
            started: std::env::var_os("SEMWEFT_QUERY_TIMING").map(|_| Instant::now()),
        }
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            eprintln!(
                "semweft_timing stage={} elapsed_ms={:.3}",
                self.stage,
                started.elapsed().as_secs_f64() * 1000.0,
            );
        }
    }
}
