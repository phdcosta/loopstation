//! Identificadores estáveis do modelo.

use crate::time::{MAX_BUSES, MAX_CHANNELS};

/// Canal de loop. Índice sempre em `0..MAX_CHANNELS`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelId(pub u8);

impl ChannelId {
    pub const fn new(index: u8) -> Option<Self> {
        if (index as usize) < MAX_CHANNELS {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Bus virtual interno. Índice sempre em `0..MAX_BUSES`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BusId(pub u8);

impl BusId {
    pub const fn new(index: u8) -> Option<Self> {
        if (index as usize) < MAX_BUSES {
            Some(Self(index))
        } else {
            None
        }
    }

    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

/// Camada de áudio (base ou overdub).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u32);

impl LayerId {
    pub const fn index(self) -> usize {
        self.0 as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_out_of_range() {
        assert!(ChannelId::new(0).is_some());
        assert!(ChannelId::new((MAX_CHANNELS - 1) as u8).is_some());
        assert!(ChannelId::new(MAX_CHANNELS as u8).is_none());
        assert!(BusId::new((MAX_BUSES - 1) as u8).is_some());
        assert!(BusId::new(MAX_BUSES as u8).is_none());
    }

    #[test]
    fn index_roundtrip() {
        let ch = ChannelId::new(5).unwrap();
        assert_eq!(ChannelId::new(ch.index() as u8), Some(ch));
    }
}
