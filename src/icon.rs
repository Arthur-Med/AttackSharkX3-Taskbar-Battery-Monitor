//! Crisp Direct2D + DirectWrite renderer for the Windows taskbar battery widget.

use std::mem::size_of;
use std::ptr::null_mut;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_PREMULTIPLIED, D2D1_COLOR_F, D2D1_FIGURE_BEGIN_FILLED, D2D1_FIGURE_END_CLOSED,
    D2D1_PIXEL_FORMAT, D2D_RECT_F,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1DCRenderTarget, ID2D1Factory, ID2D1StrokeStyle,
    D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_CAP_STYLE_ROUND, D2D1_DASH_STYLE_SOLID,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_LINE_JOIN_ROUND, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_SOFTWARE,
    D2D1_RENDER_TARGET_USAGE_NONE, D2D1_ROUNDED_RECT, D2D1_STROKE_STYLE_PROPERTIES,
    D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteFontCollection, IDWriteTextFormat,
    IDWriteTextLayout, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TEXT_METRICS,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::HDC as WinGdiHdc;
use windows_numerics::Vector2;

use windows_sys::Win32::Foundation::{HWND, POINT, SIZE};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDC, ReleaseDC, SelectObject,
    AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPV5HEADER, BI_BITFIELDS, BLENDFUNCTION,
    DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{UpdateLayeredWindow, ULW_ALPHA};

use crate::protocol::BatteryState;

struct DibSurface {
    mem_dc: HDC,
    hbm: HBITMAP,
    prev_obj: HGDIOBJ,
}

impl DibSurface {
    fn new(width: i32, height: i32) -> Option<Self> {
        if width <= 0 || height <= 0 {
            return None;
        }
        let screen_dc = unsafe { GetDC(null_mut()) };
        let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
        unsafe { ReleaseDC(null_mut(), screen_dc) };
        if mem_dc.is_null() {
            return None;
        }

        let mut bmi = unsafe { std::mem::zeroed::<BITMAPV5HEADER>() };
        bmi.bV5Size = size_of::<BITMAPV5HEADER>() as u32;
        bmi.bV5Width = width;
        bmi.bV5Height = -height;
        bmi.bV5Planes = 1;
        bmi.bV5BitCount = 32;
        bmi.bV5Compression = BI_BITFIELDS;
        bmi.bV5RedMask = 0x00FF_0000;
        bmi.bV5GreenMask = 0x0000_FF00;
        bmi.bV5BlueMask = 0x0000_00FF;
        bmi.bV5AlphaMask = 0xFF00_0000;

        let mut bits_ptr: *mut core::ffi::c_void = null_mut();
        let hbm = unsafe {
            CreateDIBSection(
                mem_dc,
                (&bmi as *const BITMAPV5HEADER).cast::<BITMAPINFO>(),
                DIB_RGB_COLORS,
                &mut bits_ptr,
                null_mut(),
                0,
            )
        };
        if hbm.is_null() || bits_ptr.is_null() {
            if !hbm.is_null() {
                unsafe { DeleteObject(hbm) };
            }
            unsafe { DeleteDC(mem_dc) };
            return None;
        }

        let prev_obj = unsafe { SelectObject(mem_dc, hbm) };
        Some(Self {
            mem_dc,
            hbm,
            prev_obj,
        })
    }
}

impl Drop for DibSurface {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem_dc, self.prev_obj);
            DeleteObject(self.hbm);
            DeleteDC(self.mem_dc);
        }
    }
}

fn to_wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct TaskbarRenderer {
    d2d_factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    dc_target: ID2D1DCRenderTarget,
    stroke_style: ID2D1StrokeStyle,
    font_family_w: Vec<u16>,
    locale_w: Vec<u16>,
}

impl TaskbarRenderer {
    pub fn new() -> Option<Self> {
        let d2d_factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }.ok()?;
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;

        // Keep render target at 96 DPI in software mode to avoid GPU/DXGI driver overhead
        // while keeping DirectWrite and pixel-snapped vector geometry crisp.
        let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let dc_target = unsafe { d2d_factory.CreateDCRenderTarget(&rt_props) }.ok()?;
        unsafe {
            dc_target.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
            dc_target.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        }

        let stroke_props = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            dashCap: D2D1_CAP_STYLE_ROUND,
            lineJoin: D2D1_LINE_JOIN_ROUND,
            miterLimit: 10.0,
            dashStyle: D2D1_DASH_STYLE_SOLID,
            dashOffset: 0.0,
        };
        let stroke_style = unsafe { d2d_factory.CreateStrokeStyle(&stroke_props, None) }.ok()?;

        let font_family_w = pick_system_font(&dwrite_factory);
        let locale_w = to_wide_null("");

        Some(Self {
            d2d_factory,
            dwrite_factory,
            dc_target,
            stroke_style,
            font_family_w,
            locale_w,
        })
    }

    fn create_text_layout(
        &self,
        text: &str,
        font_size_px: f32,
    ) -> Option<(IDWriteTextLayout, DWRITE_TEXT_METRICS)> {
        let format: IDWriteTextFormat = unsafe {
            self.dwrite_factory.CreateTextFormat(
                PCWSTR(self.font_family_w.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                font_size_px,
                PCWSTR(self.locale_w.as_ptr()),
            )
        }
        .ok()?;
        unsafe {
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR);
        }

        let text_u16: Vec<u16> = text.encode_utf16().collect();
        let layout = unsafe {
            self.dwrite_factory
                .CreateTextLayout(&text_u16, &format, 240.0, 100.0)
        }
        .ok()?;
        let mut metrics = DWRITE_TEXT_METRICS::default();
        unsafe { layout.GetMetrics(&mut metrics) }.ok()?;
        Some((layout, metrics))
    }

    pub fn measure_widget_size(&self, state: &BatteryState, bar_h: i32, dpi: u32) -> (i32, i32) {
        let layout = LayoutMetrics::compute(self, state, bar_h, dpi);
        (layout.width, layout.height)
    }

    pub fn render_widget(
        &self,
        hwnd: HWND,
        state: &BatteryState,
        pos_x: i32,
        pos_y: i32,
        bar_h: i32,
        dpi: u32,
        hovered: bool,
        light_taskbar: bool,
    ) -> Option<(i32, i32)> {
        let metrics = LayoutMetrics::compute(self, state, bar_h, dpi);
        let surface = DibSurface::new(metrics.width, metrics.height)?;

        let bind_rect = RECT {
            left: 0,
            top: 0,
            right: metrics.width,
            bottom: metrics.height,
        };
        unsafe {
            self.dc_target
                .BindDC(WinGdiHdc(surface.mem_dc), &bind_rect)
                .ok()?;
            self.dc_target.BeginDraw();
            self.dc_target.Clear(Some(&D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            }));
        }

        let draw_result = self.draw_contents(&metrics, state, hovered, light_taskbar);
        let end_res = unsafe { self.dc_target.EndDraw(None, None) };
        if draw_result.is_err() || end_res.is_err() {
            return None;
        }

        let screen_dc = unsafe { GetDC(null_mut()) };
        let pt_dst = POINT { x: pos_x, y: pos_y };
        let win_size = SIZE {
            cx: metrics.width,
            cy: metrics.height,
        };
        let pt_src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };

        unsafe {
            UpdateLayeredWindow(
                hwnd,
                screen_dc,
                &pt_dst,
                &win_size,
                surface.mem_dc,
                &pt_src,
                0,
                &blend,
                ULW_ALPHA,
            );
            ReleaseDC(null_mut(), screen_dc);
        }

        Some((metrics.width, metrics.height))
    }

    fn draw_contents(
        &self,
        m: &LayoutMetrics,
        state: &BatteryState,
        hovered: bool,
        light_taskbar: bool,
    ) -> windows::core::Result<()> {
        let bg_color = if hovered {
            if light_taskbar {
                D2D1_COLOR_F {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 0.08,
                }
            } else {
                D2D1_COLOR_F {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 0.09,
                }
            }
        } else {
            // 1/255 alpha keeps the whole widget clickable/hoverable while staying invisible.
            D2D1_COLOR_F {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 1.0 / 255.0,
            }
        };

        let bg_brush = unsafe { self.dc_target.CreateSolidColorBrush(&bg_color, None) }?;
        let bg_rr = D2D1_ROUNDED_RECT {
            rect: D_RECT(0.0, 0.0, m.width as f32, m.height as f32),
            radiusX: m.hover_radius,
            radiusY: m.hover_radius,
        };
        unsafe {
            self.dc_target.FillRoundedRectangle(&bg_rr, &bg_brush);
        }

        let (fg_color, bolt_color) = state_colors(state, light_taskbar);
        let fg_brush = unsafe { self.dc_target.CreateSolidColorBrush(&fg_color, None) }?;

        // Pixel-aligned mouse outline + vertical scroll-wheel stroke.
        let mouse_rr = D2D1_ROUNDED_RECT {
            rect: D_RECT(m.mouse_left, m.mouse_top, m.mouse_right, m.mouse_bottom),
            radiusX: m.mouse_radius,
            radiusY: m.mouse_radius,
        };
        unsafe {
            self.dc_target.DrawRoundedRectangle(
                &mouse_rr,
                &fg_brush,
                m.stroke_w,
                &self.stroke_style,
            );
            self.dc_target.DrawLine(
                Vector2 {
                    X: m.mouse_cx,
                    Y: m.mouse_top,
                },
                Vector2 {
                    X: m.mouse_cx,
                    Y: m.wheel_bottom,
                },
                &fg_brush,
                m.stroke_w,
                &self.stroke_style,
            );
        }

        if let Some((ref layout, _)) = m.text_layout {
            unsafe {
                self.dc_target.DrawTextLayout(
                    Vector2 {
                        X: m.text_x,
                        Y: m.text_y,
                    },
                    layout,
                    &fg_brush,
                    D2D1_DRAW_TEXT_OPTIONS_NONE,
                );
            }
        } else {
            let bolt_brush = unsafe { self.dc_target.CreateSolidColorBrush(&bolt_color, None) }?;
            self.draw_lightning_bolt(&bolt_brush, m.text_x, m.bolt_y, m.bolt_w, m.bolt_h)?;
        }

        Ok(())
    }

    fn draw_lightning_bolt(
        &self,
        brush: &windows::Win32::Graphics::Direct2D::ID2D1SolidColorBrush,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    ) -> windows::core::Result<()> {
        let geom = unsafe { self.d2d_factory.CreatePathGeometry() }?;
        let sink = unsafe { geom.Open() }?;
        let pts = [
            Vector2 {
                X: x + 0.62 * w,
                Y: y + 0.04 * h,
            },
            Vector2 {
                X: x + 0.14 * w,
                Y: y + 0.54 * h,
            },
            Vector2 {
                X: x + 0.47 * w,
                Y: y + 0.54 * h,
            },
            Vector2 {
                X: x + 0.35 * w,
                Y: y + 0.96 * h,
            },
            Vector2 {
                X: x + 0.86 * w,
                Y: y + 0.44 * h,
            },
            Vector2 {
                X: x + 0.53 * w,
                Y: y + 0.44 * h,
            },
        ];
        unsafe {
            sink.BeginFigure(pts[0], D2D1_FIGURE_BEGIN_FILLED);
            sink.AddLines(&pts[1..]);
            sink.EndFigure(D2D1_FIGURE_END_CLOSED);
            sink.Close()?;
            self.dc_target.FillGeometry(&geom, brush, None);
        }
        Ok(())
    }
}

#[allow(non_snake_case)]
const fn D_RECT(left: f32, top: f32, right: f32, bottom: f32) -> D2D_RECT_F {
    D2D_RECT_F {
        left,
        top,
        right,
        bottom,
    }
}

struct LayoutMetrics {
    width: i32,
    height: i32,
    hover_radius: f32,
    stroke_w: f32,
    mouse_left: f32,
    mouse_top: f32,
    mouse_right: f32,
    mouse_bottom: f32,
    mouse_radius: f32,
    mouse_cx: f32,
    wheel_bottom: f32,
    text_x: f32,
    text_y: f32,
    text_layout: Option<(IDWriteTextLayout, DWRITE_TEXT_METRICS)>,
    bolt_y: f32,
    bolt_w: f32,
    bolt_h: f32,
}

impl LayoutMetrics {
    fn compute(renderer: &TaskbarRenderer, state: &BatteryState, bar_h: i32, dpi: u32) -> Self {
        let scale = (dpi as f32 / 96.0).max(1.0);
        let height = (bar_h - (8.0 * scale).round() as i32).max((24.0 * scale).round() as i32);

        // Snap stroke to integer physical pixels and align edges to exact pixel centers.
        let stroke_w = (1.4 * scale).round().max(1.0);
        let half = if (stroke_w as i32) % 2 == 1 { 0.5 } else { 0.0 };

        // Even inner width guarantees left, right, and center wheel line all land on exact pixel centers.
        let inner_w = ((10.0 * scale) / 2.0).round().max(4.0) * 2.0;
        let inner_h = (15.0 * scale).round().max(12.0);
        let pad_x = (8.0 * scale).round();
        let gap = (6.0 * scale).round();

        let mouse_left = pad_x + half;
        let mouse_top = ((height as f32 - inner_h) * 0.5).round() + half;
        let mouse_right = mouse_left + inner_w;
        let mouse_bottom = mouse_top + inner_h;
        let mouse_cx = mouse_left + inner_w * 0.5;
        let wheel_bottom = (mouse_top + inner_h * 0.36).round() + half;
        let mouse_radius = inner_w * 0.48;

        let text_x = (mouse_right + stroke_w * 0.5 + gap).round();
        let font_size = (12.5 * scale).round().max(11.0);

        let text_layout = widget_label(state).and_then(|label| renderer.create_text_layout(&label, font_size));

        let bolt_w = (9.0 * scale).round();
        let bolt_h = (14.0 * scale).round();
        let bolt_y = ((height as f32 - bolt_h) * 0.5).round();

        let (right_w, text_y) = match &text_layout {
            Some((_, tm)) => (tm.width.ceil(), ((height as f32 - tm.height) * 0.5).round()),
            None => (bolt_w, 0.0),
        };

        let width = (text_x + right_w + pad_x).ceil() as i32;

        Self {
            width: width.max((40.0 * scale).round() as i32),
            height,
            hover_radius: (5.0 * scale).round(),
            stroke_w,
            mouse_left,
            mouse_top,
            mouse_right,
            mouse_bottom,
            mouse_radius,
            mouse_cx,
            wheel_bottom,
            text_x,
            text_y,
            text_layout,
            bolt_y,
            bolt_w,
            bolt_h,
        }
    }
}

fn pick_system_font(dwrite_factory: &IDWriteFactory) -> Vec<u16> {
    let mut collection: Option<IDWriteFontCollection> = None;
    let var_text = to_wide_null("Segoe UI Variable Text");
    if unsafe { dwrite_factory.GetSystemFontCollection(&mut collection, false) }.is_ok() {
        if let Some(col) = collection {
            let mut idx = 0u32;
            let mut exists = BOOL(0);
            if unsafe { col.FindFamilyName(PCWSTR(var_text.as_ptr()), &mut idx, &mut exists) }
                .is_ok()
                && exists.as_bool()
            {
                return var_text;
            }
        }
    }
    to_wide_null("Segoe UI")
}

fn state_colors(state: &BatteryState, light_taskbar: bool) -> (D2D1_COLOR_F, D2D1_COLOR_F) {
    let bolt = if light_taskbar {
        D2D1_COLOR_F {
            r: 0.85,
            g: 0.47,
            b: 0.02,
            a: 1.0,
        }
    } else {
        D2D1_COLOR_F {
            r: 0.98,
            g: 0.80,
            b: 0.08,
            a: 1.0,
        }
    };

    let fg = if light_taskbar {
        match state {
            BatteryState::Wireless { percent, .. } if *percent <= 20 => D2D1_COLOR_F {
                r: 0.86,
                g: 0.15,
                b: 0.15,
                a: 1.0,
            },
            BatteryState::Wireless { .. } | BatteryState::WiredCharging => D2D1_COLOR_F {
                r: 0.09,
                g: 0.09,
                b: 0.11,
                a: 0.94,
            },
            BatteryState::Sleeping { .. } => D2D1_COLOR_F {
                r: 0.09,
                g: 0.09,
                b: 0.11,
                a: 0.58,
            },
            BatteryState::Disconnected => D2D1_COLOR_F {
                r: 0.09,
                g: 0.09,
                b: 0.11,
                a: 0.48,
            },
        }
    } else {
        match state {
            BatteryState::Wireless { percent, .. } if *percent <= 20 => D2D1_COLOR_F {
                r: 0.99,
                g: 0.65,
                b: 0.65,
                a: 1.0,
            },
            BatteryState::Wireless { .. } | BatteryState::WiredCharging => D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            BatteryState::Sleeping { .. } => D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.62,
            },
            BatteryState::Disconnected => D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 0.50,
            },
        }
    };

    (fg, bolt)
}

fn widget_label(state: &BatteryState) -> Option<String> {
    match state {
        BatteryState::Wireless { percent, .. } => Some(format!("{percent}%")),
        BatteryState::WiredCharging => None,
        BatteryState::Sleeping {
            last_percent: Some(p),
            ..
        } => Some(format!("{p}%")),
        BatteryState::Sleeping {
            last_percent: None, ..
        }
        | BatteryState::Disconnected => Some("--".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measures_all_states_with_directwrite() {
        let renderer = TaskbarRenderer::new().expect("Direct2D/DirectWrite init should succeed");
        let states = [
            BatteryState::Wireless {
                percent: 100,
                last_packet: [0x03, 0x10, 0x40, 0x01, 0x0A],
            },
            BatteryState::Wireless {
                percent: 20,
                last_packet: [0x03, 0x10, 0x40, 0x01, 0x02],
            },
            BatteryState::WiredCharging,
            BatteryState::Sleeping {
                last_percent: Some(80),
                last_packet: Some([0x03, 0x10, 0x40, 0x01, 0x08]),
            },
            BatteryState::Disconnected,
        ];

        for dpi in [96u32, 120, 144, 192] {
            let bar_h = (48.0 * dpi as f32 / 96.0).round() as i32;
            for state in &states {
                let (w, h) = renderer.measure_widget_size(state, bar_h, dpi);
                assert!(w > 35 && h >= 24);
                let surface = DibSurface::new(w, h).expect("DIB creation should succeed");
                let bind_rect = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                unsafe {
                    renderer
                        .dc_target
                        .BindDC(WinGdiHdc(surface.mem_dc), &bind_rect)
                        .expect("BindDC should succeed");
                    renderer.dc_target.BeginDraw();
                    let m = LayoutMetrics::compute(&renderer, state, bar_h, dpi);
                    renderer
                        .draw_contents(&m, state, false, false)
                        .expect("draw_contents should succeed");
                    renderer
                        .dc_target
                        .EndDraw(None, None)
                        .expect("EndDraw should succeed");
                }
            }
        }
    }
}
