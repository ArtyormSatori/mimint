//! Golden-task canvas harness (P4.1): draws canvas v1 fullscreen
//! and logs clicks in the `FileOracle` line format.
//!
//! ```text
//! evalharness --log <path> [--font <ttf-path>] [--font-size <px>] [--dump <ppm-path>]
//! ```
//!
//! The canvas is rasterized by hand into an RGBA buffer (axis-
//! aligned rectangles, Bresenham lines, one 32-segment circle)
//! with text from a real font file via ab_glyph, so Vietnamese
//! labels render with correct diacritics. Hand rasterization
//! is deliberate: it is deterministic (no anti-aliasing
//! variance) and needs no graphics dependency. The buffer is
//! shown in an override-redirect X11 window at (0,0), 1024x768
//! — the window manager cannot move it, so the geometry is
//! deterministic. The log carries:
//!
//! ```text
//! CANVAS canvas-v1 font=<path> size=<px>
//! GEOM x=0 y=0 w=1024 h=768
//! READY
//! BUTTON root_x=.. root_y=.. win_x=.. win_y=.. detail=1 t=<ms>
//! ```
//!
//! Layout version `canvas-v1` and the font path + size are
//! recorded in the log AND in the golden-task measurement log:
//! a measurement without them is not reproducible. `--dump`
//! writes the drawn pixels as a PPM (the drawn image; the
//! grounder's input image is the screenshot, hashed separately
//! by the golden runner).
//!
//! Canvas v1 boxes (absolute pixels, from
//! `phase0/golden-tasks-seed.md`):
//!
//! - test-environment banner [0,0,1024,34]: full-width band
//!   reading `MÔI TRƯỜNG THỬ NGHIỆM · canvas-v1 · dữ liệu giả
//!   lập`. The band labels the canvas as a test fixture; it
//!   moves no box, so the layout version stays `canvas-v1`.
//! - `Mở` [100,60,180,100]; `Lưu` [220,60,300,100];
//!   `Đóng` [340,60,420,100]; `Lưu bản sao` [460,60,580,100]
//! - `Đăng nhập` [100,140,220,180]
//! - magnifier icon 32×32 [600,60,632,92]
//! - input box [160,200,500,240], label `Tên:` [100,200,150,240]
//! - checkbox [100,300,120,320], label `Nhớ tôi` [128,300,200,320]
//! - list rows ×32px from y=360; row 3 `Mục 3 — Báo cáo` [100,424,500,456]
//! - close glyph `×` 16×16 [1000,8,1016,24], drawn text with
//!   no function (decorative only); the caption `chỉ trang trí`
//!   sits immediately left of it, inside the banner band.
//!   Clicking the glyph logs a click like any other point —
//!   the canvas never acts on clicks itself.
//!
//! Linux-only: other targets exit with an error. The harness
//! runs until killed (the golden runner owns its lifetime).

#[cfg(target_os = "linux")]
mod linux_impl {
    use ab_glyph::{Font, FontRef, Glyph, Point, PxScale};
    use std::env;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::process::exit;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        create_gc, create_window, map_window, put_image, CreateGCAux, CreateWindowAux, EventMask,
        ImageFormat, WindowClass,
    };
    use x11rb::protocol::Event;

    pub const CANVAS_VERSION: &str = "canvas-v1";
    pub const WIDTH: u32 = 1024;
    pub const HEIGHT: u32 = 768;
    const DEFAULT_FONT: &str = "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf";
    const DEFAULT_FONT_SIZE: f32 = 16.0;

    struct Logger {
        file: std::fs::File,
    }

    impl Logger {
        fn log(&mut self, line: &str) {
            let _ = writeln!(self.file, "{line}");
            let _ = self.file.flush();
        }
    }

    /// An opaque RGBA canvas, row-major, no anti-aliasing.
    struct Canvas {
        data: Vec<u8>,
    }

    impl Canvas {
        fn new() -> Canvas {
            Canvas {
                data: vec![255u8; WIDTH as usize * HEIGHT as usize * 4],
            }
        }

        fn set(&mut self, x: i32, y: i32, color: (u8, u8, u8)) {
            if x < 0 || y < 0 || x as u32 >= WIDTH || y as u32 >= HEIGHT {
                return;
            }
            let idx = (y as usize * WIDTH as usize + x as usize) * 4;
            self.data[idx] = color.0;
            self.data[idx + 1] = color.1;
            self.data[idx + 2] = color.2;
            self.data[idx + 3] = 255;
        }

        fn blend(&mut self, x: i32, y: i32, color: (u8, u8, u8), coverage: f32) {
            if x < 0 || y < 0 || x as u32 >= WIDTH || y as u32 >= HEIGHT {
                return;
            }
            let v = coverage.clamp(0.0, 1.0);
            let idx = (y as usize * WIDTH as usize + x as usize) * 4;
            for (channel, src) in [color.0, color.1, color.2].iter().enumerate() {
                let dst = self.data[idx + channel] as f32;
                self.data[idx + channel] = (*src as f32 * v + dst * (1.0 - v)) as u8;
            }
            self.data[idx + 3] = 255;
        }

        fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: (u8, u8, u8)) {
            for py in y..y + h {
                for px in x..x + w {
                    self.set(px, py, color);
                }
            }
        }

        fn stroke_rect(&mut self, x: i32, y: i32, w: i32, h: i32, width: i32, color: (u8, u8, u8)) {
            for t in 0..width {
                self.fill_rect(x + t, y + t, w - 2 * t, 1, color);
                self.fill_rect(x + t, y + h - 1 - t, w - 2 * t, 1, color);
                self.fill_rect(x + t, y + t, 1, h - 2 * t, color);
                self.fill_rect(x + w - 1 - t, y + t, 1, h - 2 * t, color);
            }
        }

        /// Bresenham line, `width` pixels thick (square brush).
        fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, width: i32, color: (u8, u8, u8)) {
            let dx = (x1 - x0).abs();
            let dy = -(y1 - y0).abs();
            let sx = if x0 < x1 { 1 } else { -1 };
            let sy = if y0 < y1 { 1 } else { -1 };
            let mut err = dx + dy;
            let (mut x, mut y) = (x0, y0);
            loop {
                let half = width / 2;
                self.fill_rect(x - half, y - half, width, width, color);
                if x == x1 && y == y1 {
                    break;
                }
                let e2 = 2 * err;
                if e2 >= dy {
                    err += dy;
                    x += sx;
                }
                if e2 <= dx {
                    err += dx;
                    y += sy;
                }
            }
        }

        fn circle(&mut self, cx: i32, cy: i32, r: i32, width: i32, color: (u8, u8, u8)) {
            let mut prev: Option<(i32, i32)> = None;
            for i in 0..=32 {
                let angle = i as f32 / 32.0 * std::f32::consts::TAU;
                let px = cx + (r as f32 * angle.cos()) as i32;
                let py = cy + (r as f32 * angle.sin()) as i32;
                if let Some((ox, oy)) = prev {
                    self.line(ox, oy, px, py, width, color);
                }
                prev = Some((px, py));
            }
        }
    }

    /// Width of `text` at `size`, in pixels (advances + kerning).
    fn text_width(font: &FontRef, text: &str, size: f32) -> f32 {
        let upm = font.units_per_em().unwrap_or(2048.0);
        let factor = size / upm;
        let mut width = 0.0f32;
        let mut prev = None;
        for ch in text.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = prev {
                width += font.kern_unscaled(p, id) * factor;
            }
            width += font.h_advance_unscaled(id) * factor;
            prev = Some(id);
        }
        width
    }

    /// Draw `text` with its baseline at `baseline_y`, left edge
    /// at `x`, coverage-blended in `color`.
    fn draw_text(
        canvas: &mut Canvas,
        font: &FontRef,
        text: &str,
        x: f32,
        baseline_y: f32,
        size: f32,
        color: (u8, u8, u8),
    ) {
        let upm = font.units_per_em().unwrap_or(2048.0);
        let factor = size / upm;
        let scale = PxScale::from(size);
        let mut cursor = x;
        let mut prev = None;
        for ch in text.chars() {
            let id = font.glyph_id(ch);
            if let Some(p) = prev {
                cursor += font.kern_unscaled(p, id) * factor;
            }
            let glyph = Glyph {
                id,
                scale,
                position: Point {
                    x: cursor,
                    y: baseline_y,
                },
            };
            if let Some(outlined) = font.outline_glyph(glyph) {
                let bounds = outlined.px_bounds();
                let ox = bounds.min.x as i32;
                let oy = bounds.min.y as i32;
                outlined.draw(|dx, dy, coverage| {
                    canvas.blend(ox + dx as i32, oy + dy as i32, color, coverage);
                });
            }
            cursor += font.h_advance_unscaled(id) * factor;
            prev = Some(id);
        }
    }

    /// A box as (x, y, width, height) in canvas pixels.
    type BoxPx = (i32, i32, i32, i32);

    fn draw_text_centered(canvas: &mut Canvas, font: &FontRef, text: &str, box_: BoxPx, size: f32) {
        let (box_x, box_y, box_w, box_h) = box_;
        let width = text_width(font, text, size);
        let x = box_x as f32 + (box_w as f32 - width) / 2.0;
        let baseline = box_y as f32 + box_h as f32 / 2.0 + size * 0.35;
        draw_text(canvas, font, text, x, baseline, size, (0, 0, 0));
    }

    fn draw_button(canvas: &mut Canvas, font: &FontRef, label: &str, box_: BoxPx, size: f32) {
        let (x, y, w, h) = box_;
        canvas.fill_rect(x, y, w, h, (0xE8, 0xE8, 0xE8));
        canvas.stroke_rect(x, y, w, h, 1, (0x80, 0x80, 0x80));
        draw_text_centered(canvas, font, label, box_, size);
    }

    fn draw_canvas(font: &FontRef, size: f32) -> Canvas {
        let mut canvas = Canvas::new();

        // Test-environment banner (F4-19): y=0..34, full
        // width. It labels the canvas as a test fixture and
        // moves no box, so CANVAS_VERSION stays canvas-v1.
        canvas.fill_rect(0, 0, WIDTH as i32, 34, (0xFF, 0xF3, 0xCD));
        canvas.fill_rect(0, 33, WIDTH as i32, 1, (0xD9, 0xA4, 0x04));
        draw_text(
            &mut canvas,
            font,
            "MÔI TRƯỜNG THỬ NGHIỆM · canvas-v1 · dữ liệu giả lập",
            8.0,
            17.0 + size * 0.35,
            size,
            (0x33, 0x33, 0x33),
        );

        // Buttons row.
        draw_button(&mut canvas, font, "Mở", (100, 60, 80, 40), size);
        draw_button(&mut canvas, font, "Lưu", (220, 60, 80, 40), size);
        draw_button(&mut canvas, font, "Đóng", (340, 60, 80, 40), size);
        draw_button(&mut canvas, font, "Lưu bản sao", (460, 60, 120, 40), size);
        draw_button(&mut canvas, font, "Đăng nhập", (100, 140, 120, 40), size);

        // Magnifier icon [600,60,632,92]: circle + handle.
        canvas.circle(613, 73, 9, 2, (0, 0, 0));
        canvas.line(620, 80, 630, 90, 2, (0, 0, 0));

        // Input box + label.
        draw_text(
            &mut canvas,
            font,
            "Tên:",
            100.0,
            220.0 + size * 0.35,
            size,
            (0, 0, 0),
        );
        canvas.fill_rect(160, 200, 340, 40, (255, 255, 255));
        canvas.stroke_rect(160, 200, 340, 40, 1, (0x80, 0x80, 0x80));

        // Checkbox + label.
        canvas.fill_rect(100, 300, 20, 20, (255, 255, 255));
        canvas.stroke_rect(100, 300, 20, 20, 2, (0, 0, 0));
        draw_text(
            &mut canvas,
            font,
            "Nhớ tôi",
            128.0,
            310.0 + size * 0.35,
            size,
            (0, 0, 0),
        );

        // List: 5 rows × 32px from y=360.
        let rows = [
            "Mục 1 — Tổng quan",
            "Mục 2 — Chi tiết",
            "Mục 3 — Báo cáo",
            "Mục 4 — Cài đặt",
            "Mục 5 — Trợ giúp",
        ];
        for (i, label) in rows.iter().enumerate() {
            let y = 360 + i as i32 * 32;
            let fill = if i % 2 == 0 {
                (255, 255, 255)
            } else {
                (0xF0, 0xF0, 0xF0)
            };
            canvas.fill_rect(100, y, 400, 32, fill);
            canvas.stroke_rect(100, y, 400, 32, 1, (0xC0, 0xC0, 0xC0));
            draw_text(
                &mut canvas,
                font,
                label,
                110.0,
                y as f32 + 16.0 + size * 0.35,
                size,
                (0, 0, 0),
            );
        }

        // Close glyph × [1000,8,1016,24]: drawn text with no
        // function. The caption says so, right-aligned to end
        // just left of the glyph, inside the banner band.
        let caption = "chỉ trang trí";
        let caption_size = 12.0;
        let caption_x = 992.0 - text_width(font, caption, caption_size);
        draw_text(
            &mut canvas,
            font,
            caption,
            caption_x,
            17.0 + caption_size * 0.35,
            caption_size,
            (0x33, 0x33, 0x33),
        );
        draw_text(
            &mut canvas,
            font,
            "×",
            1000.0,
            16.0 + size * 0.35,
            size,
            (0, 0, 0),
        );

        canvas
    }

    /// RGBA canvas → BGRA bytes for a depth-24 ZPixmap PutImage.
    fn to_bgra(canvas: &Canvas) -> Vec<u8> {
        let mut out = Vec::with_capacity(canvas.data.len());
        for px in canvas.data.as_chunks::<4>().0 {
            out.push(px[2]);
            out.push(px[1]);
            out.push(px[0]);
            out.push(255);
        }
        out
    }

    fn write_ppm(path: &str, canvas: &Canvas) {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .unwrap_or_else(|err| {
                eprintln!("cannot write dump {path}: {err}");
                exit(2);
            });
        let _ = writeln!(file, "P6\n{WIDTH} {HEIGHT}\n255");
        for px in canvas.data.as_chunks::<4>().0 {
            let _ = file.write_all(&[px[0], px[1], px[2]]);
        }
    }

    pub fn run() {
        let args: Vec<String> = env::args().collect();
        let mut log_path = String::new();
        let mut font_path = DEFAULT_FONT.to_string();
        let mut font_size = DEFAULT_FONT_SIZE;
        let mut dump_path = String::new();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--log" => {
                    i += 1;
                    log_path = args.get(i).cloned().unwrap_or_default();
                }
                "--font" => {
                    i += 1;
                    font_path = args.get(i).cloned().unwrap_or_default();
                }
                "--font-size" => {
                    i += 1;
                    font_size = args
                        .get(i)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(DEFAULT_FONT_SIZE);
                }
                "--dump" => {
                    i += 1;
                    dump_path = args.get(i).cloned().unwrap_or_default();
                }
                other => {
                    eprintln!("unknown argument: {other}");
                    exit(2);
                }
            }
            i += 1;
        }
        if log_path.is_empty() {
            eprintln!(
                "usage: evalharness --log <path> [--font <ttf>] [--font-size <px>] [--dump <ppm>]"
            );
            exit(2);
        }
        let font_bytes = std::fs::read(&font_path).unwrap_or_else(|err| {
            eprintln!("cannot read font {font_path}: {err}");
            exit(2);
        });
        let font = FontRef::try_from_slice(&font_bytes).unwrap_or_else(|err| {
            eprintln!("cannot parse font {font_path}: {err:?}");
            exit(2);
        });
        let canvas = draw_canvas(&font, font_size);
        if !dump_path.is_empty() {
            write_ppm(&dump_path, &canvas);
        }
        let bgra = to_bgra(&canvas);

        let mut log = Logger {
            file: OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&log_path)
                .unwrap_or_else(|err| {
                    eprintln!("cannot open log {log_path}: {err}");
                    exit(2);
                }),
        };
        log.log(&format!(
            "CANVAS {CANVAS_VERSION} font={font_path} size={font_size}"
        ));

        let (conn, screen_num) = x11rb::connect(None).unwrap_or_else(|err| {
            eprintln!("cannot connect to X: {err}");
            exit(2);
        });
        let screen = &conn.setup().roots[screen_num];
        let win = conn.generate_id().unwrap();
        let gc = conn.generate_id().unwrap();
        create_window(
            &conn,
            screen.root_depth,
            win,
            screen.root,
            0,
            0,
            WIDTH as u16,
            HEIGHT as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .background_pixel(screen.white_pixel)
                .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS)
                .override_redirect(1),
        )
        .unwrap();
        create_gc(&conn, gc, win, &CreateGCAux::new()).unwrap();
        map_window(&conn, win).unwrap();
        conn.flush().unwrap();

        // Push the image once the window exists; re-push on Expose.
        let push = |conn: &x11rb::rust_connection::RustConnection| {
            put_image(
                conn,
                ImageFormat::Z_PIXMAP,
                win,
                gc,
                WIDTH as u16,
                HEIGHT as u16,
                0,
                0,
                0,
                screen.root_depth,
                &bgra,
            )
            .unwrap();
            conn.flush().unwrap();
        };
        push(&conn);
        log.log(&format!("GEOM x=0 y=0 w={WIDTH} h={HEIGHT}"));
        log.log("READY");

        while let Ok(event) = conn.wait_for_event() {
            match event {
                Event::Expose(_) => push(&conn),
                Event::ButtonPress(ev) => {
                    log.log(&format!(
                        "BUTTON root_x={} root_y={} win_x={} win_y={} detail={} t={}",
                        ev.root_x, ev.root_y, ev.event_x, ev.event_y, ev.detail, ev.time
                    ));
                }
                _ => {}
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn main() {
    linux_impl::run();
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("evalharness is Linux-only (X11 canvas)");
    std::process::exit(2);
}
