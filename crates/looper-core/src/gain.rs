//! Ganho, pan e suavização.

/// Converte dB em ganho linear. Abaixo de -96 dB vira silêncio absoluto.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    if !db.is_finite() || db <= -96.0 {
        0.0
    } else {
        10.0f32.powf(db / 20.0)
    }
}

#[inline]
pub fn gain_to_db(gain: f32) -> f32 {
    if gain <= 0.0 || !gain.is_finite() {
        -96.0
    } else {
        20.0 * gain.log10()
    }
}

/// Pan equal-power. `-1.0` = esquerda, `0.0` = centro, `1.0` = direita.
#[inline]
pub fn pan_gains(pan: f32) -> (f32, f32) {
    let p = if pan.is_finite() {
        pan.clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let angle = (p + 1.0) * std::f32::consts::FRAC_PI_4;
    (angle.cos(), angle.sin())
}

/// Curva equal-power de crossfade: devolve `(fade_out, fade_in)`.
#[inline]
pub fn equal_power_crossfade(t: f32) -> (f32, f32) {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let angle = t * std::f32::consts::FRAC_PI_2;
    (angle.cos(), angle.sin())
}

/// Ganho suavizado por one-pole, sem alocação. Usado no callback para evitar
/// clique em mudança de volume/mute.
#[derive(Clone, Copy, Debug)]
pub struct SmoothedGain {
    current: f32,
    target: f32,
    coeff: f32,
}

impl SmoothedGain {
    pub fn new(value: f32, sample_rate: u32, time_ms: f32) -> Self {
        let mut s = Self {
            current: value,
            target: value,
            coeff: 1.0,
        };
        s.set_time(sample_rate, time_ms);
        s
    }

    pub fn set_time(&mut self, sample_rate: u32, time_ms: f32) {
        if sample_rate == 0 || time_ms <= 0.0 || !time_ms.is_finite() {
            self.coeff = 1.0;
        } else {
            let samples = sample_rate as f32 * time_ms / 1000.0;
            self.coeff = 1.0 - (-1.0 / samples.max(1.0)).exp();
        }
    }

    #[inline]
    pub fn set_target(&mut self, target: f32) {
        self.target = if target.is_finite() { target } else { 0.0 };
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target
    }

    #[inline]
    pub fn current(&self) -> f32 {
        self.current
    }

    #[inline]
    pub fn next_sample(&mut self) -> f32 {
        self.current += (self.target - self.current) * self.coeff;
        // Em f32, incrementos abaixo de ~1 ulp em 1.0 se perdem; por isso o
        // limiar de assentamento é 1e-5 (≈ -100 dB), inaudível.
        if (self.target - self.current).abs() < 1e-5 {
            self.current = self.target;
        }
        self.current
    }

    pub fn snap_to(&mut self, value: f32) {
        self.current = value;
        self.target = value;
    }

    pub fn is_settled(&self) -> bool {
        (self.current - self.target).abs() < 1e-5
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_roundtrip() {
        assert!((db_to_gain(0.0) - 1.0).abs() < 1e-6);
        assert!((db_to_gain(-6.0) - 0.501_187).abs() < 1e-4);
        assert_eq!(db_to_gain(-120.0), 0.0);
        assert!((gain_to_db(db_to_gain(-12.0)) + 12.0).abs() < 1e-4);
    }

    #[test]
    fn pan_center_is_equal_power() {
        let (l, r) = pan_gains(0.0);
        assert!((l - r).abs() < 1e-6);
        assert!((l * l + r * r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn pan_extremes() {
        let (l, r) = pan_gains(-1.0);
        assert!((l - 1.0).abs() < 1e-6);
        assert!(r.abs() < 1e-6);
        let (l, r) = pan_gains(1.0);
        assert!(l.abs() < 1e-6);
        assert!((r - 1.0).abs() < 1e-6);
    }

    #[test]
    fn crossfade_endpoints() {
        let (o, i) = equal_power_crossfade(0.0);
        assert!((o - 1.0).abs() < 1e-6 && i.abs() < 1e-6);
        let (o, i) = equal_power_crossfade(1.0);
        assert!(o.abs() < 1e-6 && (i - 1.0).abs() < 1e-6);
        let (o, i) = equal_power_crossfade(0.5);
        assert!((o - i).abs() < 1e-6);
    }

    #[test]
    fn smoothed_gain_converges_without_overshoot() {
        let mut g = SmoothedGain::new(0.0, 48_000, 5.0);
        g.set_target(1.0);
        let mut prev = 0.0;
        for _ in 0..48_000 {
            let v = g.next_sample();
            assert!(v >= prev - 1e-6);
            assert!(v <= 1.0 + 1e-6);
            prev = v;
        }
        assert!(
            g.is_settled(),
            "current={} target={}",
            g.current(),
            g.target()
        );
        assert!((g.current() - 1.0).abs() < 1e-5);
    }
}
