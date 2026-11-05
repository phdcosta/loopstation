//! Política de tempo mestre/serva para o Ableton Link.
//!
//! O Link não tem "mestre": qualquer peer pode propor um BPM. O que existe é um
//! valor compartilhado. A política deste app é:
//!
//! * quando o usuário muda o BPM localmente, publicamos no Link e ignoramos
//!   ecos por [`LOCAL_COMMIT_WINDOW_MS`];
//! * mudanças contínuas (arrastar um fader) são publicadas no máximo a cada
//!   [`PUBLISH_THROTTLE_MS`];
//! * fora da janela de commit, o BPM da rede é seguido.
//!
//! Tudo aqui é uma função do tempo em milissegundos recebido pelo chamador.
//! Isso mantém a política testável sem rede e sem relógio real; o wrapper C++
//! do Link entra depois, alimentando `now_ms` a partir de um `Instant`.

use std::fmt;

use looper_core::{clamp_bpm, DEFAULT_BPM};

/// Janela em que ecos da própria mudança são ignorados.
pub const LOCAL_COMMIT_WINDOW_MS: u64 = 300;
/// Intervalo mínimo entre publicações de mudanças contínuas.
pub const PUBLISH_THROTTLE_MS: u64 = 50;
/// Tolerância para considerar dois BPMs iguais.
pub const TEMPO_EPSILON: f64 = 1e-4;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TempoAuthority {
    /// O usuário acabou de propor um tempo; ecos são ignorados até `until_ms`.
    LocalCommit { until_ms: u64 },
    /// O tempo veio da rede: seguimos.
    Follow,
}

impl TempoAuthority {
    pub fn is_local_commit(self) -> bool {
        matches!(self, TempoAuthority::LocalCommit { .. })
    }
}

impl fmt::Display for TempoAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TempoAuthority::LocalCommit { .. } => write!(f, "local"),
            TempoAuthority::Follow => write!(f, "follow"),
        }
    }
}

/// O que fazer depois de uma mudança local de BPM.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LocalTempoAction {
    /// Publicar este valor no Link agora.
    Publish(f64),
    /// Mudança aceita, mas dentro do throttle: publicar no próximo `poll`.
    Throttled,
    /// BPM igual ao vigente: nada a fazer.
    Ignored,
}

/// O que fazer com um BPM recebido da rede.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RemoteTempoAction {
    /// Aplicar ao transporte (e agendar a mudança musical).
    Apply(f64),
    /// Eco da própria mudança dentro da janela de commit: ignorar.
    Echo,
    /// Igual ao BPM vigente: nada a fazer.
    Unchanged,
}

/// Telemetria para a UI.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LinkStatus {
    pub connected: bool,
    pub peers: u32,
    pub authority: TempoAuthority,
    pub bpm: f64,
}

impl Default for LinkStatus {
    fn default() -> Self {
        Self {
            connected: false,
            peers: 0,
            authority: TempoAuthority::Follow,
            bpm: DEFAULT_BPM,
        }
    }
}

/// Estado da política de tempo.
#[derive(Clone, Debug)]
pub struct LinkPolicy {
    authority: TempoAuthority,
    /// Último BPM efetivamente aplicado ao transporte.
    bpm: f64,
    /// Último BPM proposto localmente (para reconhecer o eco).
    local_bpm: f64,
    last_publish_ms: Option<u64>,
    pending_publish: Option<f64>,
    peers: u32,
    connected: bool,
}

impl Default for LinkPolicy {
    fn default() -> Self {
        Self::new(DEFAULT_BPM)
    }
}

impl LinkPolicy {
    pub fn new(initial_bpm: f64) -> Self {
        let bpm = clamp_bpm(initial_bpm);
        Self {
            authority: TempoAuthority::Follow,
            bpm,
            local_bpm: bpm,
            last_publish_ms: None,
            pending_publish: None,
            peers: 0,
            connected: false,
        }
    }

    #[inline]
    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    #[inline]
    pub fn authority(&self) -> TempoAuthority {
        self.authority
    }

    #[inline]
    pub fn connected(&self) -> bool {
        self.connected
    }

    #[inline]
    pub fn peers(&self) -> u32 {
        self.peers
    }

    pub fn status(&self) -> LinkStatus {
        LinkStatus {
            connected: self.connected,
            peers: self.peers,
            authority: self.authority,
            bpm: self.bpm,
        }
    }

    /// Atualiza a telemetria de conexão. Erros de rede nunca param o áudio.
    pub fn set_session(&mut self, connected: bool, peers: u32) {
        self.connected = connected;
        self.peers = if connected { peers } else { 0 };
    }

    /// Registra uma mudança local de BPM (UI ou MIDI).
    ///
    /// Devolve a ação imediata. Quando a resposta é [`LocalTempoAction::Throttled`],
    /// o valor fica pendente e sai em [`LinkPolicy::poll_publish`] assim que o
    /// throttle expirar.
    pub fn on_local_tempo(&mut self, now_ms: u64, bpm: f64) -> LocalTempoAction {
        let bpm = clamp_bpm(bpm);
        if (bpm - self.bpm).abs() < TEMPO_EPSILON {
            return LocalTempoAction::Ignored;
        }

        self.bpm = bpm;
        self.local_bpm = bpm;
        self.authority = TempoAuthority::LocalCommit {
            until_ms: now_ms.saturating_add(LOCAL_COMMIT_WINDOW_MS),
        };

        match self.last_publish_ms {
            Some(last) if now_ms.saturating_sub(last) < PUBLISH_THROTTLE_MS => {
                self.pending_publish = Some(bpm);
                LocalTempoAction::Throttled
            }
            _ => {
                self.last_publish_ms = Some(now_ms);
                self.pending_publish = None;
                LocalTempoAction::Publish(bpm)
            }
        }
    }

    /// Devolve um BPM pendente se o throttle já permitir publicá-lo.
    pub fn poll_publish(&mut self, now_ms: u64) -> Option<f64> {
        let pending = self.pending_publish?;
        match self.last_publish_ms {
            Some(last) if now_ms.saturating_sub(last) < PUBLISH_THROTTLE_MS => None,
            _ => {
                self.pending_publish = None;
                self.last_publish_ms = Some(now_ms);
                Some(pending)
            }
        }
    }

    /// Processa um BPM recebido da rede.
    pub fn on_remote_tempo(&mut self, now_ms: u64, bpm: f64) -> RemoteTempoAction {
        let bpm = clamp_bpm(bpm);

        if let TempoAuthority::LocalCommit { until_ms } = self.authority {
            if now_ms <= until_ms {
                // Eco da própria mudança (ou outro peer enquanto arrastamos):
                // a janela curta evita ping-pong.
                return RemoteTempoAction::Echo;
            }
            self.authority = TempoAuthority::Follow;
        }

        if (bpm - self.bpm).abs() < TEMPO_EPSILON {
            return RemoteTempoAction::Unchanged;
        }

        self.bpm = bpm;
        self.authority = TempoAuthority::Follow;
        RemoteTempoAction::Apply(bpm)
    }

    /// Força o BPM (ex.: carregar um projeto) sem publicar nem criar commit.
    pub fn force_bpm(&mut self, bpm: f64) {
        self.bpm = clamp_bpm(bpm);
        self.local_bpm = self.bpm;
        self.pending_publish = None;
    }

    /// True quando o Link está conectado e há outros peers.
    pub fn has_peers(&self) -> bool {
        self.connected && self.peers > 0
    }

    /// BPM local que originou o commit atual (útil para diagnóstico).
    pub fn local_bpm(&self) -> f64 {
        self.local_bpm
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_change_publishes_immediately() {
        let mut p = LinkPolicy::new(120.0);
        assert_eq!(
            p.on_local_tempo(1_000, 128.0),
            LocalTempoAction::Publish(128.0)
        );
        assert_eq!(p.bpm(), 128.0);
        assert!(p.authority().is_local_commit());
    }

    #[test]
    fn same_bpm_is_ignored() {
        let mut p = LinkPolicy::new(120.0);
        assert_eq!(p.on_local_tempo(0, 120.0), LocalTempoAction::Ignored);
        assert_eq!(p.on_local_tempo(0, 120.00001), LocalTempoAction::Ignored);
    }

    #[test]
    fn continuous_changes_are_throttled_and_flushed() {
        let mut p = LinkPolicy::new(120.0);
        assert_eq!(p.on_local_tempo(0, 121.0), LocalTempoAction::Publish(121.0));
        assert_eq!(p.on_local_tempo(10, 122.0), LocalTempoAction::Throttled);
        assert_eq!(p.on_local_tempo(20, 123.0), LocalTempoAction::Throttled);
        // Ainda dentro dos 50 ms.
        assert_eq!(p.poll_publish(49), None);
        // O valor mais recente sai quando o throttle expira.
        assert_eq!(p.poll_publish(50), Some(123.0));
        assert_eq!(p.poll_publish(51), None);
    }

    #[test]
    fn local_echo_is_ignored_inside_the_window() {
        let mut p = LinkPolicy::new(120.0);
        assert_eq!(p.on_local_tempo(0, 130.0), LocalTempoAction::Publish(130.0));
        assert_eq!(p.on_remote_tempo(100, 130.0), RemoteTempoAction::Echo);
        assert_eq!(p.on_remote_tempo(300, 130.0), RemoteTempoAction::Echo);
        // Fora da janela, o mesmo valor já é "unchanged".
        assert_eq!(p.on_remote_tempo(301, 130.0), RemoteTempoAction::Unchanged);
    }

    #[test]
    fn remote_change_after_window_is_followed() {
        let mut p = LinkPolicy::new(120.0);
        p.on_local_tempo(0, 130.0);
        assert_eq!(
            p.on_remote_tempo(1_000, 90.0),
            RemoteTempoAction::Apply(90.0)
        );
        assert_eq!(p.bpm(), 90.0);
        assert_eq!(p.authority(), TempoAuthority::Follow);
    }

    #[test]
    fn remote_bpm_is_clamped_and_non_finite_is_ignored() {
        let mut p = LinkPolicy::new(120.0);
        assert_eq!(
            p.on_remote_tempo(0, 10_000.0),
            RemoteTempoAction::Apply(looper_core::MAX_BPM)
        );
        // NaN vira DEFAULT_BPM via clamp_bpm; aqui já estamos em 300, então muda.
        assert_eq!(
            p.on_remote_tempo(1, f64::NAN),
            RemoteTempoAction::Apply(DEFAULT_BPM)
        );
    }

    #[test]
    fn session_telemetry_reflects_disconnect() {
        let mut p = LinkPolicy::new(120.0);
        p.set_session(true, 3);
        assert!(p.connected());
        assert_eq!(p.peers(), 3);
        assert!(p.has_peers());
        p.set_session(false, 3);
        assert!(!p.connected());
        assert_eq!(p.peers(), 0);
        assert!(!p.has_peers());
    }

    #[test]
    fn force_bpm_does_not_create_a_commit() {
        let mut p = LinkPolicy::new(120.0);
        p.force_bpm(100.0);
        assert_eq!(p.bpm(), 100.0);
        assert_eq!(p.authority(), TempoAuthority::Follow);
        assert_eq!(p.on_remote_tempo(0, 101.0), RemoteTempoAction::Apply(101.0));
    }
}
