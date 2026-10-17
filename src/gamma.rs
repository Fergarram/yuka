//
// Gamma (Data Oriented zone)
//
// The CRTC's gamma ramp: three curves, one per channel, that the display controller passes
// every pixel through on its way to the panel. Night light, dimming past the backlight's
// floor, contrast and gamma are all one curve per channel, so they cost nothing to draw and
// reach everything on screen, the hardware cursor plane included, which a shader over our
// frame would miss.
//
// The scripts choose the curves (yk.gamma, lua/screen.lua); this module only resamples them
// to the size the CRTC takes and hands them over. The ramp found at start is kept and put
// back on exit, and ours is set again after a VT switch, since the console takes the CRTC
// back while we are away.
//
// All the libdrm FFI is contained here.
//

use crate::cursor;

//
// Constants
//

// The brightest a ramp can make the screen, at least, as a fraction of full. A script that
// asks for black gets this instead, so a wrong curve can dim the screen but never hide the
// error banner that says what went wrong.
const MIN_PEAK: f32 = 0.1;

// The most points a script may send per channel. CRTCs take 256 to 4096.
pub const MAX_POINTS: usize = 4096;

//
// libdrm FFI
//

extern "C" {
    fn drmModeCrtcGetGamma(fd: i32, crtc_id: u32, size: u32, r: *mut u16, g: *mut u16, b: *mut u16) -> i32;
    fn drmModeCrtcSetGamma(fd: i32, crtc_id: u32, size: u32, r: *mut u16, g: *mut u16, b: *mut u16) -> i32;
}

//
// Types
//

pub struct Gamma {
    fd: i32,
    crtc: u32,
    // Entries per channel the CRTC takes.
    size: usize,
    // Red, green and blue, size entries each, one after the other: what was there at start,
    // and what is set now.
    saved: Vec<u16>,
    ramp: Vec<u16>,
}

//
// Functions
//

// None nested, or when the CRTC has no gamma ramp to set.
pub fn init() -> Option<Gamma> {
    let fd = cursor::find_drm_fd()?;
    let crtc = cursor::find_active_crtc(fd)?;
    let size = cursor::gamma_size(fd, crtc);
    if size < 2 {
        println!("yuka: crtc {crtc} has no gamma ramp");
        return None;
    }
    let mut saved = vec![0u16; size * 3];
    if !get(fd, crtc, size, &mut saved) {
        // Nothing to put back, so put back a straight line.
        identity(&mut saved, size);
    }
    let ramp = saved.clone();
    println!("yuka: gamma ramp on crtc {crtc}, {size} entries");
    Some(Gamma { fd, crtc, size, saved, ramp })
}

// Set the ramp from the scripts' curves: each 2 to MAX_POINTS outputs from 0 to 1, evenly
// spaced over the inputs, resampled to the CRTC's size. Empty curves put back the ramp
// found at start.
pub fn set(g: &mut Gamma, r: &[f32], gr: &[f32], b: &[f32]) {
    if r.is_empty() {
        g.ramp.copy_from_slice(&g.saved);
        apply(g);
        return;
    }
    let peak = r.iter().chain(gr).chain(b).fold(0.0f32, |m, &v| m.max(v)).min(1.0);
    let lift = if peak < MIN_PEAK { MIN_PEAK / peak.max(1e-6) } else { 1.0 };
    let n = g.size;
    resample(r, lift, &mut g.ramp[0..n]);
    resample(gr, lift, &mut g.ramp[n..2 * n]);
    resample(b, lift, &mut g.ramp[2 * n..3 * n]);
    apply(g);
}

// Ours again, after the console has had the CRTC.
pub fn reapply(g: &mut Gamma) {
    apply(g);
}

// What was there before we started, on the way out.
pub fn restore(g: &mut Gamma) {
    g.ramp.copy_from_slice(&g.saved);
    apply(g);
}

fn resample(curve: &[f32], lift: f32, out: &mut [u16]) {
    let last = (curve.len() - 1) as f32;
    let n = out.len();
    for i in 0..n {
        let t = i as f32 / (n - 1) as f32 * last;
        let k = (t as usize).min(curve.len() - 2);
        let frac = t - k as f32;
        let v = curve[k] + (curve[k + 1] - curve[k]) * frac;
        let v = if v.is_finite() { (v * lift).clamp(0.0, 1.0) } else { 0.0 };
        out[i] = (v * 65535.0 + 0.5) as u16;
    }
}

fn identity(ramp: &mut [u16], size: usize) {
    for c in 0..3 {
        for i in 0..size {
            ramp[c * size + i] = (i * 65535 / (size - 1)) as u16;
        }
    }
}

fn get(fd: i32, crtc: u32, size: usize, out: &mut [u16]) -> bool {
    let (r, rest) = out.split_at_mut(size);
    let (gr, b) = rest.split_at_mut(size);
    let rc = unsafe {
        drmModeCrtcGetGamma(fd, crtc, size as u32, r.as_mut_ptr(), gr.as_mut_ptr(), b.as_mut_ptr())
    };
    rc == 0
}

fn apply(g: &mut Gamma) {
    let n = g.size;
    let (r, rest) = g.ramp.split_at_mut(n);
    let (gr, b) = rest.split_at_mut(n);
    let rc = unsafe {
        drmModeCrtcSetGamma(g.fd, g.crtc, n as u32, r.as_mut_ptr(), gr.as_mut_ptr(), b.as_mut_ptr())
    };
    if rc != 0 {
        eprintln!("yuka: drmModeCrtcSetGamma failed ({rc})");
    }
}
