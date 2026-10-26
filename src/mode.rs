//
// Display modes (Data Oriented zone)
//
// A mode the display never listed, built from its size and refresh rate. The kernel
// prunes the modes a display advertises down to what it believes the link can carry,
// and sometimes believes wrong (a capture dongle, an adapter between the port and the
// screen). YUKA_MODE=WxH[@Hz] builds one here and raylib drives it as is.
//
// The timings are VESA CVT with reduced blanking (CVT-RB, version 1): the shortest
// blanking a digital display accepts, so the lowest pixel clock for a size and rate.
// This is what the kernel's own video=WxHR@Hz option computes.
//
// YUKA_MODE=edid[:n] instead takes the display's own timings, the nth detailed timing
// in its EDID (0, the preferred one, by default), for a display that only locks onto
// exactly what it advertises.
//

use std::fs;

use crate::ray::DrmMode;

//
// Constants
//

// Horizontal blanking and sync, in pixels. The front porch is what is left of half the
// blanking once the sync is taken out.
const RB_H_BLANK: u32 = 160;
const RB_H_SYNC: u32 = 32;
// Shortest vertical blanking, in microseconds.
const RB_MIN_V_BLANK_US: f64 = 460.0;
// Vertical front porch and shortest back porch, in lines.
const RB_V_FPORCH: u32 = 3;
const RB_MIN_V_BPORCH: u32 = 6;
// The pixel clock is a whole number of these, in kHz.
const CLOCK_STEP_KHZ: u32 = 250;
// Horizontal sizes are whole cells of this many pixels.
const CELL_GRAN: u32 = 8;

// drmModeModeInfo flags and type.
const DRM_MODE_FLAG_PHSYNC: u32 = 1 << 0;
const DRM_MODE_FLAG_NHSYNC: u32 = 1 << 1;
const DRM_MODE_FLAG_PVSYNC: u32 = 1 << 2;
const DRM_MODE_FLAG_NVSYNC: u32 = 1 << 3;
const DRM_MODE_TYPE_USERDEF: u32 = 1 << 5;

// EDID layout: 128 byte blocks, the base block's detailed timings at these offsets, and
// a CTA extension block (tag 2) whose byte 2 says where its own detailed timings start.
const EDID_BLOCK: usize = 128;
const EDID_DTD_LEN: usize = 18;
const EDID_BASE_DTDS: [usize; 4] = [54, 72, 90, 108];
const CTA_TAG: u8 = 2;

//
// Functions
//

// "1920x1200" or "1920x1200@30": width, height and refresh rate, 60 Hz unless named.
pub fn parse(text: &str) -> Option<(u32, u32, u32)> {
    let (size, hz) = match text.split_once('@') {
        Some((size, hz)) => (size, hz.trim().parse().ok()?),
        None => (text, 60),
    };
    let (w, h) = size.split_once('x')?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    if w == 0 || h == 0 || hz == 0 || w > 8192 || h > 8192 || hz > 240 {
        return None;
    }
    Some((w, h, hz))
}

// The CVT-RB mode for this size and rate.
pub fn cvt_rb(width: u32, height: u32, hz: u32) -> DrmMode {
    let h_pixels = width / CELL_GRAN * CELL_GRAN;
    let v_lines = height;

    // The sync width says the aspect ratio, so a display can guess it from the timings.
    let v_sync = if h_pixels * 3 == v_lines * 4 {
        4
    } else if h_pixels * 9 == v_lines * 16 {
        5
    } else if h_pixels * 10 == v_lines * 16 {
        6
    } else if h_pixels * 4 == v_lines * 5 || h_pixels * 9 == v_lines * 15 {
        7
    } else {
        10
    };

    // Enough whole lines of blanking to last the shortest blanking time.
    let h_period_us = (1_000_000.0 / hz as f64 - RB_MIN_V_BLANK_US) / v_lines as f64;
    let vbi_lines = (RB_MIN_V_BLANK_US / h_period_us) as u32 + 1;
    let v_blank = vbi_lines.max(RB_V_FPORCH + v_sync + RB_MIN_V_BPORCH);

    let htotal = h_pixels + RB_H_BLANK;
    let vtotal = v_lines + v_blank;
    let clock_hz = hz as f64 * htotal as f64 * vtotal as f64;
    let clock = (clock_hz / 1000.0) as u32 / CLOCK_STEP_KHZ * CLOCK_STEP_KHZ;

    let hsync_start = h_pixels + RB_H_BLANK / 2 - RB_H_SYNC;
    let vsync_start = v_lines + RB_V_FPORCH;

    DrmMode {
        clock,
        hdisplay: h_pixels as u16,
        hsync_start: hsync_start as u16,
        hsync_end: (hsync_start + RB_H_SYNC) as u16,
        htotal: htotal as u16,
        hskew: 0,
        vdisplay: v_lines as u16,
        vsync_start: vsync_start as u16,
        vsync_end: (vsync_start + v_sync) as u16,
        vtotal: vtotal as u16,
        vscan: 0,
        vrefresh: hz,
        flags: DRM_MODE_FLAG_PHSYNC | DRM_MODE_FLAG_NVSYNC,
        kind: DRM_MODE_TYPE_USERDEF,
        name: mode_name(h_pixels, v_lines),
    }
}

// The nth detailed timing in the EDID of the connector with this kernel name, as a mode.
pub fn from_edid(connector: &str, index: usize) -> Option<DrmMode> {
    let edid = read_edid(connector)?;
    let mut found = 0;
    let mut blocks = 0;
    while (blocks + 1) * EDID_BLOCK <= edid.len() {
        let block = &edid[blocks * EDID_BLOCK..(blocks + 1) * EDID_BLOCK];
        let (start, end) = if blocks == 0 {
            (EDID_BASE_DTDS[0], EDID_BASE_DTDS[3] + EDID_DTD_LEN)
        } else if block[0] == CTA_TAG && block[2] >= 4 {
            (block[2] as usize, EDID_BLOCK - 1)
        } else {
            (0, 0)
        };
        let mut o = start;
        while o + EDID_DTD_LEN <= end {
            // A zero pixel clock marks a display descriptor (name, ranges), not a timing.
            let dtd = &block[o..o + EDID_DTD_LEN];
            if dtd[0] != 0 || dtd[1] != 0 {
                if found == index {
                    return Some(dtd_mode(dtd));
                }
                found += 1;
            } else if blocks > 0 {
                break;
            }
            o += EDID_DTD_LEN;
        }
        blocks += 1;
    }
    None
}

// /sys/class/drm/cardN-<connector>/edid, on whichever card has the connector.
fn read_edid(connector: &str) -> Option<Vec<u8>> {
    let suffix = format!("-{connector}");
    for entry in fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("card") && name.ends_with(&suffix) {
            let edid = fs::read(entry.path().join("edid")).ok()?;
            if edid.len() >= EDID_BLOCK {
                return Some(edid);
            }
        }
    }
    None
}

// One 18 byte detailed timing descriptor as a mode.
fn dtd_mode(b: &[u8]) -> DrmMode {
    let clock = (b[0] as u32 | (b[1] as u32) << 8) * 10;
    let h_active = b[2] as u32 | ((b[4] & 0xf0) as u32) << 4;
    let h_blank = b[3] as u32 | ((b[4] & 0x0f) as u32) << 8;
    let v_active = b[5] as u32 | ((b[7] & 0xf0) as u32) << 4;
    let v_blank = b[6] as u32 | ((b[7] & 0x0f) as u32) << 8;
    let h_fporch = b[8] as u32 | ((b[11] & 0xc0) as u32) << 2;
    let h_sync = b[9] as u32 | ((b[11] & 0x30) as u32) << 4;
    let v_fporch = (b[10] >> 4) as u32 | ((b[11] & 0x0c) as u32) << 2;
    let v_sync = (b[10] & 0x0f) as u32 | ((b[11] & 0x03) as u32) << 4;
    let htotal = h_active + h_blank;
    let vtotal = v_active + v_blank;
    let vrefresh = ((clock as f64 * 1000.0) / (htotal as f64 * vtotal as f64)).round() as u32;

    // Byte 17: bits 4 and 3 set is digital separate sync, whose bits 2 and 1 are the
    // vertical and horizontal polarities. Anything else gets the common negative pair.
    let flags = if b[17] & 0x18 == 0x18 {
        let h = if b[17] & 0x02 != 0 { DRM_MODE_FLAG_PHSYNC } else { DRM_MODE_FLAG_NHSYNC };
        let v = if b[17] & 0x04 != 0 { DRM_MODE_FLAG_PVSYNC } else { DRM_MODE_FLAG_NVSYNC };
        h | v
    } else {
        DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC
    };

    DrmMode {
        clock,
        hdisplay: h_active as u16,
        hsync_start: (h_active + h_fporch) as u16,
        hsync_end: (h_active + h_fporch + h_sync) as u16,
        htotal: htotal as u16,
        hskew: 0,
        vdisplay: v_active as u16,
        vsync_start: (v_active + v_fporch) as u16,
        vsync_end: (v_active + v_fporch + v_sync) as u16,
        vtotal: vtotal as u16,
        vscan: 0,
        vrefresh,
        flags,
        kind: DRM_MODE_TYPE_USERDEF,
        name: mode_name(h_active, v_active),
    }
}

// "WxH", nul padded, as drmModeModeInfo names its modes.
fn mode_name(w: u32, h: u32) -> [u8; 32] {
    let mut name = [0u8; 32];
    let text = format!("{w}x{h}");
    let n = text.len().min(name.len() - 1);
    name[..n].copy_from_slice(&text.as_bytes()[..n]);
    name
}
