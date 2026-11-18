//! Quiet test chime (same shape as the spike's tone.py: 523 Hz then 659 Hz, 0.15
//! peak, soft attack/decay), followed by silence.

use super::Source;

pub struct ToneSource {
    pos: u64,
    total: u64,
    rate: f64,
    amplitude: f32,
    split: bool,
}

impl ToneSource {
    pub fn new(seconds: f64, amplitude: f32) -> Self {
        Self { pos: 0, total: (seconds * 44100.0) as u64, rate: 44100.0, amplitude, split: false }
    }

    /// Put the first note on the left channel only and the second on the right only.
    pub fn split(mut self, on: bool) -> Self {
        self.split = on;
        self
    }

    fn sample(&self, i: u64) -> f32 {
        let secs = self.total as f64 / self.rate;
        let t = i as f64 / self.rate;
        let half = secs / 2.0;
        let f = if t < half { 523.25 } else { 659.25 };
        let env = (t * 20.0).min(1.0) * ((secs - t) * 4.0).min(1.0) * (-((t % half) * 1.2)).exp();
        (self.amplitude as f64 * env * (2.0 * std::f64::consts::PI * f * t).sin()) as f32
    }
}

impl Source for ToneSource {
    fn fill(&mut self, out: &mut [[f32; 2]]) {
        for o in out.iter_mut() {
            let s = if self.pos < self.total { self.sample(self.pos) } else { 0.0 };
            *o = if !self.split {
                [s, s]
            } else if self.pos < self.total / 2 {
                [s, 0.0]
            } else {
                [0.0, s]
            };
            self.pos += 1;
        }
    }

    fn finished(&self) -> bool {
        self.pos >= self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tone_is_quiet_and_ends() {
        let mut t = ToneSource::new(0.5, 0.15);
        let mut buf = vec![[0.0f32; 2]; 352];
        let mut peak = 0.0f32;
        while !t.finished() {
            t.fill(&mut buf);
            peak = buf.iter().fold(peak, |p, f| p.max(f[0].abs()));
        }
        assert!(peak > 0.05 && peak <= 0.15, "{peak}");
        t.fill(&mut buf);
        assert!(buf.iter().all(|f| f[0] == 0.0));
    }
}
