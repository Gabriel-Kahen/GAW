//! ITU-R BS.1770 Annex 2 four-times true-peak interpolation.

/// Group delay, in input-rate frames, of the order-48 polyphase interpolator.
pub(crate) const TRUE_PEAK_GROUP_DELAY: usize = 6;

// The four 12-tap phases published in ITU-R BS.1770-5 Annex 2. Floating-point
// processing does not need the Annex's initial 12.04 dB fixed-point headroom.
const PHASES: [[f32; 12]; 4] = [
    [
        0.001_708_984_4,
        0.010_986_328,
        -0.019_653_32,
        0.033_203_125,
        -0.059_448_242,
        0.137_329_1,
        0.972_167_97,
        -0.102_294_92,
        0.047_607_422,
        -0.026_611_328,
        0.014_892_578,
        -0.008_300_781,
    ],
    [
        -0.029_174_805,
        0.029_296_875,
        -0.051_757_813,
        0.089_111_33,
        -0.166_503_9,
        0.465_087_9,
        0.779_785_16,
        -0.200_317_38,
        0.101_562_5,
        -0.058_227_54,
        0.033_081_055,
        -0.018_920_898,
    ],
    [
        -0.018_920_898,
        0.033_081_055,
        -0.058_227_54,
        0.101_562_5,
        -0.200_317_38,
        0.779_785_16,
        0.465_087_9,
        -0.166_503_9,
        0.089_111_33,
        -0.051_757_813,
        0.029_296_875,
        -0.029_174_805,
    ],
    [
        -0.008_300_781,
        0.014_892_578,
        -0.026_611_328,
        0.047_607_422,
        -0.102_294_92,
        0.972_167_97,
        0.137_329_1,
        -0.059_448_242,
        0.033_203_125,
        -0.019_653_32,
        0.010_986_328,
        0.001_708_984_4,
    ],
];

#[derive(Clone, Debug, Default)]
pub(crate) struct TruePeakDetector {
    history: [[f32; 12]; 2],
}

impl TruePeakDetector {
    #[inline]
    pub(crate) fn process(&mut self, input: [f32; 2], channels: usize) -> f32 {
        let mut peak = 0.0_f32;
        for (channel, sample) in input.iter().copied().take(channels).enumerate() {
            self.history[channel].rotate_right(1);
            self.history[channel][0] = sample;
            peak = peak.max(sample.abs());
            for phase in PHASES {
                let mut interpolated = 0.0;
                for (tap, coefficient) in phase.into_iter().enumerate() {
                    interpolated += self.history[channel][tap] * coefficient;
                }
                peak = peak.max(interpolated.abs());
            }
        }
        peak
    }

    pub(crate) fn reset(&mut self) {
        self.history = [[0.0; 12]; 2];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detector_finds_an_intersample_peak() {
        let mut detector = TruePeakDetector::default();
        let mut peak = 0.0_f32;
        // A near-Nyquist sinusoid with its continuous-time extrema between samples.
        for frame in 0..256 {
            let sample = (core::f32::consts::TAU * 0.24 * (frame as f32 + 0.5)).sin();
            peak = peak.max(detector.process([sample, 0.0], 1));
        }
        assert!(peak > 1.0, "expected intersample over, got {peak}");
    }

    #[test]
    fn quarter_rate_reference_vector_reads_minus_six_dbtp() {
        let mut detector = TruePeakDetector::default();
        let mut peak = 0.0_f32;
        for frame in 0..256 {
            let sample = 0.5
                * (core::f32::consts::TAU * 0.25 * frame as f32 + core::f32::consts::FRAC_PI_4)
                    .sin();
            peak = peak.max(detector.process([sample, 0.0], 1));
        }
        let dbtp = 20.0 * peak.log10();
        assert!(
            (-6.4..=-5.8).contains(&dbtp),
            "reference measured {dbtp} dBTP"
        );
    }
}
