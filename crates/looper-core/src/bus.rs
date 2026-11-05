//! Buses virtuais e matriz de saída.
//!
//! Buses internos são estéreo. Cada bus pode alimentar um ou mais canais
//! físicos da interface, com ganho próprio. A matriz é pré-calculada no control
//! thread; o callback só percorre listas fixas.

use crate::gain::db_to_gain;
use crate::time::MAX_BUSES;

/// Qual lado de um bus estéreo alimenta um canal físico.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelTap {
    /// Downmix `(L + R) / 2`. Padrão: um bus estéreo indo para uma saída mono.
    #[default]
    Mono,
    Left,
    Right,
}

/// Um destino físico de um bus.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputTarget {
    pub device_channel: u16,
    pub gain_db: f32,
    pub tap: ChannelTap,
}

impl OutputTarget {
    pub fn new(device_channel: u16) -> Self {
        Self {
            device_channel,
            gain_db: 0.0,
            tap: ChannelTap::Mono,
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn with_tap(mut self, tap: ChannelTap) -> Self {
        self.tap = tap;
        self
    }
}

/// Definição de um bus.
#[derive(Clone, Debug, PartialEq)]
pub struct BusSpec {
    pub name: String,
    pub gain_db: f32,
    pub muted: bool,
    pub outputs: Vec<OutputTarget>,
}

impl BusSpec {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            gain_db: 0.0,
            muted: false,
            outputs: Vec::new(),
        }
    }

    pub fn with_gain_db(mut self, gain_db: f32) -> Self {
        self.gain_db = gain_db;
        self
    }

    pub fn with_output(mut self, device_channel: u16) -> Self {
        self.outputs.push(OutputTarget::new(device_channel));
        self
    }

    pub fn with_output_gain(mut self, device_channel: u16, gain_db: f32) -> Self {
        self.outputs
            .push(OutputTarget::new(device_channel).with_gain_db(gain_db));
        self
    }

    pub fn with_output_tap(mut self, device_channel: u16, tap: ChannelTap) -> Self {
        self.outputs
            .push(OutputTarget::new(device_channel).with_tap(tap));
        self
    }
}

/// Rota já resolvida: bus -> canal físico, com ganho linear.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Route {
    pub bus: u8,
    pub gain: f32,
    pub tap: ChannelTap,
}

/// Matriz bus -> canal físico, por canal de saída.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputMatrix {
    device_channels: usize,
    routes: Vec<Vec<Route>>,
}

impl OutputMatrix {
    /// Valida e pré-calcula as rotas. Canais físicos inexistentes são ignorados.
    pub fn build(buses: &[BusSpec], device_channels: u16) -> Self {
        let n = device_channels as usize;
        let mut routes = vec![Vec::new(); n];
        for (bus_index, bus) in buses.iter().enumerate().take(MAX_BUSES) {
            for target in &bus.outputs {
                let ch = target.device_channel as usize;
                if ch >= n {
                    continue;
                }
                let gain = db_to_gain(target.gain_db);
                if gain > 0.0 {
                    routes[ch].push(Route {
                        bus: bus_index as u8,
                        gain,
                        tap: target.tap,
                    });
                }
            }
        }
        Self {
            device_channels: n,
            routes,
        }
    }

    /// Mapa 1:1 (bus 0 -> saída 0, bus 1 -> saída 1, ...). Usado como padrão.
    pub fn identity(device_channels: u16) -> Self {
        let n = device_channels as usize;
        let mut routes = vec![Vec::new(); n];
        for (ch, list) in routes.iter_mut().enumerate() {
            list.push(Route {
                bus: ch.min(MAX_BUSES - 1) as u8,
                gain: 1.0,
                tap: ChannelTap::Mono,
            });
        }
        Self {
            device_channels: n,
            routes,
        }
    }

    #[inline]
    pub fn device_channels(&self) -> usize {
        self.device_channels
    }

    #[inline]
    pub fn routes_for(&self, device_channel: usize) -> &[Route] {
        self.routes
            .get(device_channel)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn is_routed(&self, device_channel: usize) -> bool {
        !self.routes_for(device_channel).is_empty()
    }

    pub fn bus_is_routed(&self, bus: usize) -> bool {
        self.routes
            .iter()
            .any(|list| list.iter().any(|r| r.bus as usize == bus))
    }

    /// Quantos canais físicos recebem sinal de algum bus.
    pub fn active_channels(&self) -> usize {
        self.routes.iter().filter(|l| !l.is_empty()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_scenario_a_to_left_b_to_right() {
        let buses = vec![
            BusSpec::new("A").with_output(0),
            BusSpec::new("B").with_output(1),
        ];
        let m = OutputMatrix::build(&buses, 2);
        assert_eq!(m.routes_for(0).len(), 1);
        assert_eq!(m.routes_for(0)[0].bus, 0);
        assert_eq!(m.routes_for(1).len(), 1);
        assert_eq!(m.routes_for(1)[0].bus, 1);
        assert_eq!(m.active_channels(), 2);
    }

    #[test]
    fn bus_can_feed_several_outputs_with_gain() {
        let buses = vec![
            BusSpec::new("A").with_output_gain(0, 0.0),
            BusSpec::new("A-copy").with_output_gain(1, -6.0),
        ];
        let m = OutputMatrix::build(&buses, 4);
        assert_eq!(m.routes_for(0)[0].gain, 1.0);
        assert!((m.routes_for(1)[0].gain - 0.501_187).abs() < 1e-4);
        assert!(!m.is_routed(2));
        assert!(m.bus_is_routed(1));
    }

    #[test]
    fn invalid_device_channels_are_ignored() {
        let buses = vec![BusSpec::new("A").with_output(7)];
        let m = OutputMatrix::build(&buses, 2);
        assert_eq!(m.active_channels(), 0);
    }

    #[test]
    fn identity_maps_bus_to_same_channel() {
        let m = OutputMatrix::identity(4);
        assert_eq!(m.routes_for(2)[0].bus, 2);
        assert_eq!(m.active_channels(), 4);
    }

    #[test]
    fn muted_output_gain_is_not_routed() {
        let buses = vec![BusSpec::new("A").with_output_gain(0, -96.0)];
        let m = OutputMatrix::build(&buses, 1);
        assert!(!m.is_routed(0));
    }
}
