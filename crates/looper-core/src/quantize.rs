//! Quantização: resolução de comandos para a grade musical.
//!
//! Toda ação (armar, gravar, disparar, parar) é resolvida aqui, no control
//! thread, e viaja para o motor com um beat alvo. O motor então a aplica no
//! sample exato.

use crate::time::{BEAT_EPSILON, DEFAULT_BEATS_PER_BAR};

/// Grade de disparo/gravação.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quantize {
    /// Aplica imediatamente, sem esperar boundary.
    Off,
    /// Próximo beat.
    Beat,
    /// Próximo compasso.
    Bar,
    /// Próximo múltiplo de N compassos.
    Bars(u32),
    /// Próximo múltiplo do quantum configurado.
    Quantum,
}

/// Configuração da grade do transporte.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub beats_per_bar: u32,
    /// Quantum em beats (padrão: um compasso).
    pub quantum_beats: f64,
}

impl Grid {
    pub fn new(beats_per_bar: u32, quantum_beats: f64) -> Self {
        let beats_per_bar = beats_per_bar.max(1);
        let bar = beats_per_bar as f64;
        let quantum_beats = if quantum_beats.is_finite() {
            quantum_beats.clamp(0.0625, bar)
        } else {
            bar
        };
        Self {
            beats_per_bar,
            quantum_beats,
        }
    }

    #[inline]
    pub fn bar_beats(&self) -> f64 {
        self.beats_per_bar.max(1) as f64
    }
}

impl Default for Grid {
    fn default() -> Self {
        Self::new(DEFAULT_BEATS_PER_BAR, DEFAULT_BEATS_PER_BAR as f64)
    }
}

impl Quantize {
    /// Passo da grade em beats. `0.0` significa "sem grade".
    pub fn step_beats(self, grid: Grid) -> f64 {
        match self {
            Quantize::Off => 0.0,
            Quantize::Beat => 1.0,
            Quantize::Bar => grid.bar_beats(),
            Quantize::Bars(n) => grid.bar_beats() * n.max(1) as f64,
            Quantize::Quantum => grid.quantum_beats.max(0.0625),
        }
    }

    /// Próximo boundary estritamente à frente (ou o próprio, se já estiver nele).
    pub fn resolve_up(self, beat: f64, grid: Grid) -> f64 {
        let step = self.step_beats(grid);
        if step <= 0.0 {
            return beat.max(0.0);
        }
        ceil_to_step(beat, step)
    }

    /// Boundary anterior (usado para fechar tomadas e cortar ciclos).
    pub fn resolve_down(self, beat: f64, grid: Grid) -> f64 {
        let step = self.step_beats(grid);
        if step <= 0.0 {
            return beat.max(0.0);
        }
        floor_to_step(beat, step)
    }
}

fn ceil_to_step(x: f64, step: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let x = x.max(0.0);
    let k = (x / step - BEAT_EPSILON).ceil();
    (k * step).max(0.0)
}

fn floor_to_step(x: f64, step: f64) -> f64 {
    if !x.is_finite() {
        return 0.0;
    }
    let x = x.max(0.0);
    let k = (x / step + BEAT_EPSILON).floor();
    (k * step).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid() -> Grid {
        Grid::new(4, 4.0)
    }

    #[test]
    fn off_is_identity() {
        assert_eq!(Quantize::Off.resolve_up(3.3, grid()), 3.3);
        assert_eq!(Quantize::Off.resolve_down(3.3, grid()), 3.3);
    }

    #[test]
    fn beat_boundaries_are_stable() {
        let q = Quantize::Beat;
        assert_eq!(q.resolve_up(0.0, grid()), 0.0);
        assert_eq!(q.resolve_up(3.999_999, grid()), 4.0);
        assert_eq!(q.resolve_up(4.0, grid()), 4.0);
        assert_eq!(q.resolve_up(4.000_001, grid()), 5.0);
        assert_eq!(q.resolve_down(4.0, grid()), 4.0);
        assert_eq!(q.resolve_down(3.999, grid()), 3.0);
    }

    #[test]
    fn bar_and_bars() {
        let g = grid();
        assert_eq!(Quantize::Bar.resolve_up(1.0, g), 4.0);
        assert_eq!(Quantize::Bar.resolve_up(4.0, g), 4.0);
        assert_eq!(Quantize::Bar.resolve_up(4.5, g), 8.0);
        assert_eq!(Quantize::Bars(2).resolve_up(4.5, g), 8.0);
        assert_eq!(Quantize::Bars(2).resolve_up(8.0, g), 8.0);
        assert_eq!(Quantize::Bars(2).resolve_up(8.5, g), 16.0);
    }

    #[test]
    fn quantum_uses_configured_step() {
        let g = Grid::new(4, 0.5);
        assert_eq!(Quantize::Quantum.resolve_up(0.0, g), 0.0);
        assert_eq!(Quantize::Quantum.resolve_up(0.1, g), 0.5);
        assert_eq!(Quantize::Quantum.resolve_up(0.5, g), 0.5);
        assert_eq!(Quantize::Quantum.resolve_up(0.75, g), 1.0);
    }

    #[test]
    fn negative_positions_clamp_to_zero() {
        assert_eq!(Quantize::Bar.resolve_up(-3.0, grid()), 0.0);
        assert_eq!(Quantize::Beat.resolve_down(-0.5, grid()), 0.0);
    }

    #[test]
    fn grid_clamps_quantum() {
        let g = Grid::new(4, 100.0);
        assert_eq!(g.quantum_beats, 4.0);
        let g = Grid::new(4, 0.0);
        assert_eq!(g.quantum_beats, 0.0625);
    }
}
