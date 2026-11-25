use std::io::{self, Write};

use image::DynamicImage;
use ratatui::{Frame, layout::Rect};
use ratatui_image::{
    Image,
    protocol::{Protocol, halfblocks::Halfblocks, kitty::Kitty, sixel::Sixel},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Kitty,
    Sixel,
    Cells,
}

fn mode(term: Option<&str>, multiplexed: bool) -> Mode {
    match (term, multiplexed) {
        (Some("xterm-kitty"), false) => Mode::Kitty,
        (Some("foot" | "foot-extra" | "mlterm" | "xterm-sixel"), false) => Mode::Sixel,
        _ => Mode::Cells,
    }
}

pub(in super::super) struct Painter {
    mode: Mode,
    id: u32,
    cached: Option<(Rect, Protocol)>,
}

impl Painter {
    pub(in super::super) fn new() -> Self {
        Self {
            mode: mode(
                std::env::var("TERM").ok().as_deref(),
                std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some(),
            ),
            id: rand::random::<u32>() | 1,
            cached: None,
        }
    }

    pub(in super::super) fn draw(&mut self, image: &DynamicImage, area: Rect, frame: &mut Frame) {
        if area.is_empty() {
            return;
        }
        if self.cached.as_ref().is_none_or(|(last, _)| *last != area) {
            // OS window geometry only. Never send a graphics probe, read a terminal
            // response, use a passthrough wrapper, or ask for window/screen contents.
            let cell = crossterm::terminal::window_size().ok().and_then(|size| {
                let w = size.width.checked_div(size.columns)?;
                let h = size.height.checked_div(size.rows)?;
                (w > 0 && h > 0).then_some((w, h))
            });
            self.cached = Some((area, self.encode(image, area, cell)));
        }
        if let Some((_, protocol)) = &self.cached {
            frame.render_widget(Image::new(protocol), protocol.area());
        }
    }

    fn encode(&self, image: &DynamicImage, area: Rect, cell: Option<(u16, u16)>) -> Protocol {
        if self.mode != Mode::Cells
            && let Some((cw, ch)) = cell
        {
            let w = u32::from(area.width) * u32::from(cw);
            let h = u32::from(area.height) * u32::from(ch);
            // Never upsample. Protocol encoding cost is bounded by the stored thumbnail,
            // independent of the window dimensions.
            let pixels = image.thumbnail(w.min(image.width()), h.min(image.height()));
            let rect = Rect::new(
                area.x,
                area.y,
                pixels.width().div_ceil(cw.into()) as u16,
                pixels.height().div_ceil(ch.into()) as u16,
            );
            let encoded = match self.mode {
                Mode::Kitty => Kitty::new(pixels, rect, self.id, false).map(Protocol::Kitty),
                Mode::Sixel => Sixel::new(pixels, rect, false).map(Protocol::Sixel),
                Mode::Cells => unreachable!(),
            };
            if let Ok(protocol) = encoded {
                return protocol;
            }
        }
        // Unknown terminal, unknown pixel geometry, or unavailable encoder: emit only
        // ratatui cells. Halfblocks uses u16 for its grid product, so cap the cell grid.
        let w = u32::from(area.width.min(128)).min(image.width());
        let h = u32::from(area.height.min(64)) * 2;
        let pixels = image.thumbnail(w, h.min(image.height()));
        let rect = Rect::new(
            area.x,
            area.y,
            pixels.width() as u16,
            pixels.height().div_ceil(2) as u16,
        );
        Protocol::Halfblocks(Halfblocks::new(pixels, rect).expect("in-memory primitive halfblocks"))
    }

    pub(in super::super) fn clear(&mut self, mut out: impl Write) -> io::Result<bool> {
        let Some((_, protocol)) = self.cached.take() else {
            return Ok(false);
        };
        if matches!(protocol, Protocol::Kitty(_)) {
            // Upper-case I deletes this placement AND frees this image's terminal data.
            write!(out, "\x1b_Ga=d,d=I,i={},q=2\x1b\\", self.id)?;
        }
        // Erase sixels and colored cells as well, even on quit/error before the next draw.
        crossterm::execute!(
            out,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All)
        )?;
        Ok(true)
    }
}

impl Drop for Painter {
    fn drop(&mut self) {
        let _ = self.clear(io::stdout());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use ratatui::{buffer::Buffer, widgets::Widget};

    #[test]
    fn no_active_probes_or_mux_passthrough() {
        assert!(mode(Some("xterm-kitty"), false) == Mode::Kitty);
        assert!(mode(Some("xterm-sixel"), false) == Mode::Sixel);
        for term in [
            Some("screen"),
            Some("xterm-256color"),
            Some("xterm-kitty\x1b"),
            None,
        ] {
            assert!(mode(term, false) == Mode::Cells);
        }
        assert!(mode(Some("xterm-kitty"), true) == Mode::Cells);
    }

    #[test]
    fn kitty_encodes_only_pixels_and_releases_only_its_id() {
        let mut painter = Painter {
            mode: Mode::Kitty,
            id: 9,
            cached: None,
        };
        // Literal ESC, ], BEL as RGB: never inject those bytes into the control channel.
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(1, 1, Rgb([27, 93, 7])));
        let area = Rect::new(0, 0, 3, 3);
        let proto = painter.encode(&img, area, Some((8, 16)));
        let mut buf = Buffer::empty(area);
        Image::new(&proto).render(proto.area(), &mut buf);
        let symbol = buf[(0, 0)].symbol();
        assert!(symbol.contains("\x1b_Gq=2,i=9,a=T,U=1,f=32,t=d,s=1,v=1,m=0;G10H/w==\x1b\\"));
        assert!(!symbol.contains("\x1b]\x07"));
        painter.cached = Some((area, proto));
        let mut out = Vec::new();
        assert!(painter.clear(&mut out).unwrap());
        assert!(out.starts_with(b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\"));
        assert!(!painter.clear(&mut out).unwrap());
    }

    #[test]
    fn sixel_uses_a_raster_stream_and_uncertain_terminals_use_only_cells() {
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(24, 12, |x, _| {
            Rgb(if x < 12 { [255, 0, 0] } else { [0, 255, 0] })
        }));
        let area = Rect::new(0, 0, 12, 4);
        let painter = Painter {
            mode: Mode::Sixel,
            id: 9,
            cached: None,
        };
        let mut buf = Buffer::empty(area);
        let proto = painter.encode(&img, area, Some((8, 16)));
        Image::new(&proto).render(proto.area(), &mut buf);
        let data = buf[(0, 0)].symbol();
        assert!(data.starts_with("\x1bP") && data.ends_with("\x1b\\"));
        assert!(data.contains("24;12") && data.contains("#"));
        let proto = painter.encode(&img, area, None);
        assert!(matches!(proto, Protocol::Halfblocks(_)));
        Image::new(&proto).render(proto.area(), &mut buf);
        assert!(buf.content.iter().all(|c| !c.symbol().contains('\x1b')));
    }
}
