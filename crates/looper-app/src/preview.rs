//! Rasterizador headless da UI — só para testes.
//!
//! Roda o `egui` sem janela, tesselar os shapes e pinta os triângulos num
//! buffer RGBA (fonte: a própria atlas de fontes do egui). Serve para inspecionar
//! o layout em PNG durante o desenvolvimento: o resultado é exatamente o que a
//! janela desenharia, sem depender de GPU nem de captura de tela.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use eframe::egui::{
    self,
    epaint::{ClippedPrimitive, ImageData, Primitive, TextureId},
    Color32, Pos2, Rect, Vec2,
};

use crate::LooperApp;

struct Texture {
    width: usize,
    height: usize,
    /// RGBA em 0..1 (a atlas de fonte vem como branco + alpha de cobertura).
    rgba: Vec<[f32; 4]>,
}

/// Renderiza um frame da UI e devolve RGBA8 (tamanho `w*h*4`).
pub fn render(app: &mut LooperApp, w: usize, h: usize) -> Vec<u8> {
    let ctx = egui::Context::default();
    crate::apply_theme(&ctx);

    let mut textures: HashMap<TextureId, Texture> = HashMap::new();
    let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(w as f32, h as f32));

    // Duas passadas: a primeira assenta fontes/layout, a segunda é a estável.
    let mut output = None;
    for pass in 0..2 {
        let raw = egui::RawInput {
            screen_rect: Some(screen),
            time: Some(pass as f64 * 0.033),
            focused: true,
            ..Default::default()
        };
        let out = ctx.run(raw, |ctx| app.draw(ctx));
        upload_textures(&mut textures, &out.textures_delta);
        output = Some(out);
    }
    let output = output.expect("ao menos uma passada");

    let mut canvas = Canvas::new(w, h);
    canvas.clear(Color32::from_rgb(8, 9, 12));

    let primitives: Vec<ClippedPrimitive> = ctx.tessellate(output.shapes, 1.0);
    for clipped in &primitives {
        let Primitive::Mesh(mesh) = &clipped.primitive else {
            continue;
        };
        let Some(tex) = textures.get(&mesh.texture_id) else {
            continue;
        };
        let clip = intersect(clipped.clip_rect, screen);
        for tri in mesh.indices.chunks_exact(3) {
            let (a, b, c) = (
                &mesh.vertices[tri[0] as usize],
                &mesh.vertices[tri[1] as usize],
                &mesh.vertices[tri[2] as usize],
            );
            canvas.triangle(a, b, c, tex, clip);
        }
    }

    canvas.into_rgba()
}

fn intersect(a: Rect, b: Rect) -> Rect {
    Rect::from_min_max(
        Pos2::new(a.min.x.max(b.min.x), a.min.y.max(b.min.y)),
        Pos2::new(a.max.x.min(b.max.x), a.max.y.min(b.max.y)),
    )
}

fn upload_textures(textures: &mut HashMap<TextureId, Texture>, delta: &egui::TexturesDelta) {
    for (id, image_delta) in &delta.set {
        let ImageData::Color(color) = &image_delta.image;
        let (tw, th) = (color.size[0], color.size[1]);
        let rgba: Vec<[f32; 4]> = color
            .pixels
            .iter()
            .map(|c| {
                [
                    c.r() as f32 / 255.0,
                    c.g() as f32 / 255.0,
                    c.b() as f32 / 255.0,
                    c.a() as f32 / 255.0,
                ]
            })
            .collect();

        // Sub-regiões (atlas que cresce) são aplicadas por cima da textura atual.
        if let (Some(pos), Some(existing)) = (image_delta.pos, textures.get_mut(id)) {
            let (px, py) = (pos[0], pos[1]);
            for y in 0..th {
                for x in 0..tw {
                    let (dx, dy) = (px + x, py + y);
                    if dx >= existing.width || dy >= existing.height {
                        continue;
                    }
                    let src = y * tw + x;
                    let dst = dy * existing.width + dx;
                    if let (Some(s), Some(d)) = (rgba.get(src), existing.rgba.get_mut(dst)) {
                        *d = *s;
                    }
                }
            }
            continue;
        }
        textures.insert(
            *id,
            Texture {
                width: tw,
                height: th,
                rgba,
            },
        );
    }
}

struct Canvas {
    width: usize,
    height: usize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            rgba: vec![0; width * height * 4],
        }
    }

    fn clear(&mut self, color: Color32) {
        for px in self.rgba.chunks_exact_mut(4) {
            px[0] = color.r();
            px[1] = color.g();
            px[2] = color.b();
            px[3] = 255;
        }
    }

    fn blend(&mut self, x: usize, y: usize, src: [f32; 4]) {
        if x >= self.width || y >= self.height {
            return;
        }
        let a = src[3].clamp(0.0, 1.0);
        if a <= 0.0 {
            return;
        }
        let i = (y * self.width + x) * 4;
        for (c, channel) in src.iter().enumerate().take(3) {
            let dst = self.rgba[i + c] as f32 / 255.0;
            let out = channel + dst * (1.0 - a);
            self.rgba[i + c] = (out.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
        self.rgba[i + 3] = 255;
    }

    fn triangle(
        &mut self,
        a: &egui::epaint::Vertex,
        b: &egui::epaint::Vertex,
        c: &egui::epaint::Vertex,
        tex: &Texture,
        clip: Rect,
    ) {
        let min_x = a.pos.x.min(b.pos.x).min(c.pos.x).max(clip.min.x);
        let max_x = a.pos.x.max(b.pos.x).max(c.pos.x).min(clip.max.x);
        let min_y = a.pos.y.min(b.pos.y).min(c.pos.y).max(clip.min.y);
        let max_y = a.pos.y.max(b.pos.y).max(c.pos.y).min(clip.max.y);
        if max_x <= min_x || max_y <= min_y {
            return;
        }

        let area =
            (b.pos.x - a.pos.x) * (c.pos.y - a.pos.y) - (b.pos.y - a.pos.y) * (c.pos.x - a.pos.x);
        if area.abs() < 1e-9 {
            return;
        }

        // Cores premultiplicadas em sRGB (como o egui usa).
        let ca = [
            a.color.r() as f32 / 255.0,
            a.color.g() as f32 / 255.0,
            a.color.b() as f32 / 255.0,
            a.color.a() as f32 / 255.0,
        ];
        let cb = [
            b.color.r() as f32 / 255.0,
            b.color.g() as f32 / 255.0,
            b.color.b() as f32 / 255.0,
            b.color.a() as f32 / 255.0,
        ];
        let cc = [
            c.color.r() as f32 / 255.0,
            c.color.g() as f32 / 255.0,
            c.color.b() as f32 / 255.0,
            c.color.a() as f32 / 255.0,
        ];

        let x0 = min_x.floor().max(0.0) as usize;
        let x1 = (max_x.ceil() as usize).min(self.width);
        let y0 = min_y.floor().max(0.0) as usize;
        let y1 = (max_y.ceil() as usize).min(self.height);

        for y in y0..y1 {
            for x in x0..x1 {
                let p = Pos2::new(x as f32 + 0.5, y as f32 + 0.5);
                let (w0, w1, w2) = barycentric(p, a.pos, b.pos, c.pos, area);
                if w0 < -0.0005 || w1 < -0.0005 || w2 < -0.0005 {
                    continue;
                }
                let mut color = [0.0f32; 4];
                for k in 0..4 {
                    color[k] = ca[k] * w0 + cb[k] * w1 + cc[k] * w2;
                }
                let u = a.uv.x * w0 + b.uv.x * w1 + c.uv.x * w2;
                let v = a.uv.y * w0 + b.uv.y * w1 + c.uv.y * w2;
                if let Some(texel) = sample(tex, u, v) {
                    for k in 0..4 {
                        color[k] *= texel[k];
                    }
                }
                self.blend(x, y, color);
            }
        }
    }

    fn into_rgba(self) -> Vec<u8> {
        self.rgba
    }
}

#[inline]
fn barycentric(p: Pos2, a: Pos2, b: Pos2, c: Pos2, area: f32) -> (f32, f32, f32) {
    let w0 = ((b.x - p.x) * (c.y - p.y) - (b.y - p.y) * (c.x - p.x)) / area;
    let w1 = ((c.x - p.x) * (a.y - p.y) - (c.y - p.y) * (a.x - p.x)) / area;
    (w0, w1, 1.0 - w0 - w1)
}

fn sample(tex: &Texture, u: f32, v: f32) -> Option<[f32; 4]> {
    let (x, y) = texel_coords(tex, u, v);
    tex.rgba.get(y * tex.width + x).copied()
}

#[inline]
fn texel_coords(tex: &Texture, u: f32, v: f32) -> (usize, usize) {
    let x = ((u.clamp(0.0, 1.0) * tex.width as f32) as usize).min(tex.width.saturating_sub(1));
    let y = ((v.clamp(0.0, 1.0) * tex.height as f32) as usize).min(tex.height.saturating_sub(1));
    (x, y)
}

// ------------------------------------------------------------------- PNG

/// Escreve um PNG RGBA8 com deflate "stored" (sem dependências).
pub fn write_png(path: &Path, width: usize, height: usize, rgba: &[u8]) -> std::io::Result<()> {
    let mut raw = Vec::with_capacity((width * 4 + 1) * height);
    for y in 0..height {
        raw.push(0u8);
        raw.extend_from_slice(&rgba[y * width * 4..(y + 1) * width * 4]);
    }
    let z = zlib_store(&raw);

    let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &ihdr);
    png_chunk(&mut png, b"IDAT", &z);
    png_chunk(&mut png, b"IEND", &[]);

    let mut file = std::fs::File::create(path)?;
    file.write_all(&png)
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc = Crc32::new();
    crc.update(kind);
    crc.update(data);
    out.extend_from_slice(&crc.finish().to_be_bytes());
}

fn zlib_store(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut pos = 0usize;
    if raw.is_empty() {
        out.extend_from_slice(&[0x01, 0x00, 0x00, 0xff, 0xff]);
    }
    while pos < raw.len() {
        let chunk = (raw.len() - pos).min(65_535);
        let last = pos + chunk == raw.len();
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&(chunk as u16).to_le_bytes());
        out.extend_from_slice(&(!(chunk as u16)).to_le_bytes());
        out.extend_from_slice(&raw[pos..pos + chunk]);
        pos += chunk;
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in data {
        a = (a + byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

struct Crc32 {
    table: [u32; 256],
    value: u32,
}

impl Crc32 {
    fn new() -> Self {
        let mut table = [0u32; 256];
        for (i, slot) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xedb8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *slot = c;
        }
        Self {
            table,
            value: 0xffff_ffff,
        }
    }

    fn update(&mut self, data: &[u8]) {
        for &byte in data {
            let idx = ((self.value ^ byte as u32) & 0xff) as usize;
            self.value = self.table[idx] ^ (self.value >> 8);
        }
    }

    fn finish(&self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

// ------------------------------------------------------------------- testes

#[cfg(test)]
mod tests {
    use super::*;
    use looper_core::{ChannelState, ChannelView, EngineSnapshot, MAX_CHANNELS};

    /// Envelope sintético com cara de música (ataques por compasso).
    fn demo_envelope(seed: f32) -> [f32; looper_core::ENVELOPE_BINS] {
        let mut bins = [0.0f32; looper_core::ENVELOPE_BINS];
        let count = bins.len() as f32;
        for (i, bin) in bins.iter_mut().enumerate() {
            let t = i as f32 / count;
            let beat = (t * 32.0).fract();
            let attack = (1.0 - beat * 4.0).max(0.0);
            let shape = 0.25
                + 0.55
                    * (t * std::f32::consts::TAU * (1.0 + seed) + seed)
                        .sin()
                        .abs();
            *bin = (attack * 0.8 + shape * 0.5).min(1.0);
        }
        bins
    }

    fn snapshot_demo() -> EngineSnapshot {
        let mut snap = EngineSnapshot::default();
        snap.transport.playing = true;
        snap.transport.bpm = 128.0;
        snap.transport.beat = 9.25;
        snap.transport.bar = 3;
        snap.transport.beat_in_bar = 1.25;
        snap.transport.beats_per_bar = 4;

        let states = [
            ChannelState::Playing,
            ChannelState::Playing,
            ChannelState::Recording,
            ChannelState::Overdubbing,
            ChannelState::Stopped,
            ChannelState::Armed,
            ChannelState::Empty,
            ChannelState::Empty,
        ];
        for (i, state) in states.iter().enumerate() {
            let has_audio = !matches!(state, ChannelState::Empty | ChannelState::Armed);
            let period = match i {
                0 => 8.0,
                1 => 16.0,
                2 => 4.0,
                3 => 8.0,
                4 => 32.0,
                _ => 0.0,
            };
            snap.channels[i] = ChannelView {
                state: *state,
                has_audio,
                phase: 0.18 + i as f32 * 0.09,
                period_beats: period,
                bars_remaining: if i == 1 { 0.6 } else { 3.4 },
                gain_db: -3.0 + i as f32,
                peak_l: if has_audio { 0.55 } else { 0.0 },
                peak_r: if has_audio { 0.38 } else { 0.0 },
                bus_mask: if i == 1 { 0b11 } else { 1u16 << (i % 2) },
                muted: i == 4,
                soloed: false,
                direction: if i == 3 {
                    looper_core::Direction::Reverse
                } else {
                    looper_core::Direction::Forward
                },
                speed: if i == 0 { 2.0 } else { 1.0 },
                crop_start_beats: if i == 1 { 8.0 } else { 0.0 },
                crop_len_beats: period,
                total_beats: if period > 0.0 { 32.0 } else { 0.0 },
                source_bpm: 128.0,
                layer_count: if has_audio { (i % 3 + 1) as u8 } else { 0 },
                rec_in_beats: if *state == ChannelState::Armed {
                    2.5
                } else {
                    0.0
                },
                rec_bars: if *state == ChannelState::Recording {
                    2.0
                } else {
                    0.0
                },
                rec_progress: if *state == ChannelState::Recording {
                    0.55
                } else {
                    0.0
                },
                rec_elapsed_bars: if *state == ChannelState::Recording {
                    2.2
                } else {
                    0.0
                },
                record_bars: if i == 2 { 4 } else { 0 },
                take_gain_db: if i == 4 { 36.0 } else { 0.0 },
                crossfade_ms: 5.0,
                seam_glue_ms: 30.0,
            };
        }
        snap.buses[0].gain_db = -2.0;
        snap.buses[0].peak_l = 0.7;
        snap.buses[0].peak_r = 0.6;
        snap.buses[1].gain_db = -6.0;
        snap.buses[1].peak_l = 0.4;
        snap.buses[1].peak_r = 0.5;
        snap
    }

    fn app_with(snapshot: EngineSnapshot) -> LooperApp {
        let mut app = LooperApp::preview_instance();
        app.snapshot = snapshot;
        for i in 0..MAX_CHANNELS {
            if app.snapshot.channels[i].has_audio {
                app.envelopes[i] = demo_envelope(i as f32 * 0.35);
            }
        }
        app.selected = 1;
        app.status = "Prévia headless da UI — 8 canais, 3 carregados.".into();
        app
    }

    fn render_png(name: &str, app: &mut LooperApp, w: usize, h: usize) -> Vec<u8> {
        let rgba = render(app, w, h);
        let dir = Path::new("target/preview");
        std::fs::create_dir_all(dir).unwrap();
        write_png(&dir.join(format!("{name}.png")), w, h, &rgba).unwrap();
        rgba
    }

    #[test]
    fn render_preview_snapshots() {
        let mut app = app_with(snapshot_demo());
        let full = render_png("ui-1500x940", &mut app, 1500, 940);
        assert_eq!(full.len(), 1500 * 940 * 4);

        // Janela mínima: é onde o layout costuma estourar.
        let mut small = app_with(snapshot_demo());
        let min = render_png("ui-1180x760", &mut small, 1180, 760);
        assert_eq!(min.len(), 1180 * 760 * 4);

        let mut empty = LooperApp::preview_instance();
        empty.status = "Prévia com canais vazios.".into();
        render_png("ui-vazio", &mut empty, 1500, 940);
    }

    #[test]
    fn rendered_pixels_are_not_a_flat_image() {
        let mut app = app_with(snapshot_demo());
        let rgba = render(&mut app, 1500, 940);
        let mut distinct = std::collections::HashSet::new();
        for px in rgba.chunks_exact(4).step_by(37) {
            distinct.insert((px[0], px[1], px[2]));
        }
        assert!(
            distinct.len() > 40,
            "a imagem parece vazia ({} cores)",
            distinct.len()
        );
    }
}
