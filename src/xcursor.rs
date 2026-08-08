//
// Xcursor themes (Data Oriented zone)
//
// Reads the cursor a desktop would use, out of the theme files every toolkit reads, so the pointer
// over empty canvas is the same pointer the windows show. We drew our own before, and a drawn
// arrow is always slightly the wrong arrow: it changes shape as you cross onto a window.
//
// The format is small and fixed, so this is a parser rather than a dependency. A file is a header,
// a table of contents, and a chunk per image, where an image is a size, a hotspot and a block of
// ARGB pixels. Animated cursors are several images at the same nominal size, distinguished only by
// a delay; the first is taken and the rest ignored, since nothing here animates a pointer.
//
// No unsafe: everything is a bounds checked read out of a Vec<u8>.
//

use std::fs;
use std::path::PathBuf;

//
// Constants
//

// "Xcur", little endian.
const MAGIC: u32 = 0x7275_6358;
// The chunk type that carries an image, as opposed to a comment.
const CHUNK_IMAGE: u32 = 0xfffd_0002;
// Bytes in a table of contents entry, and in an image chunk's own header.
const TOC_ENTRY: usize = 12;
const IMAGE_HEADER: usize = 36;
// What a cursor is asked for at, when nothing says otherwise. The size every toolkit defaults to.
const DEFAULT_SIZE: u32 = 24;
// Where themes live, when XCURSOR_PATH does not say. The home ones first, so a theme a person
// installed for themselves wins over the system's.
const SEARCH: [&str; 4] = [
    "~/.local/share/icons",
    "~/.icons",
    "/usr/share/icons",
    "/usr/local/share/icons",
];
// How far an Inherits chain is followed. Two is enough for every theme anyone ships: a theme that
// inherits Adwaita, which inherits nothing.
const MAX_INHERIT: usize = 4;

//
// Types
//

// One cursor image, ready for the plane: ARGB8888, premultiplied, which is what both the Xcursor
// format and the DRM cursor plane mean by those bytes.
pub struct Image {
    pub w: i32,
    pub h: i32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub pixels: Vec<u32>,
}

//
// Functions
//

// Find a cursor by name, at the closest size to what is asked for, from whichever theme the system
// is configured with. None when no theme is installed, which is a real state on a bare machine and
// the reason the caller keeps a drawn arrow to fall back to.
pub fn load(name: &str, want: u32) -> Option<Image> {
    let want = if want == 0 { size_from_env() } else { want };
    let mut theme = std::env::var("XCURSOR_THEME").unwrap_or_default();
    if theme.is_empty() {
        theme = system_theme().unwrap_or_else(|| "Adwaita".to_string());
    }

    // The theme, then whatever it inherits, then the last resort every theme is expected to have.
    let mut names = vec![theme];
    for _ in 0..MAX_INHERIT {
        let Some(next) = inherits(names.last()?) else { break };
        if names.contains(&next) {
            break;
        }
        names.push(next);
    }
    names.push("Adwaita".to_string());
    names.push("default".to_string());

    for theme in &names {
        for dir in dirs() {
            let path = dir.join(theme).join("cursors").join(name);
            let Ok(bytes) = fs::read(&path) else { continue };
            if let Some(image) = parse(&bytes, want) {
                println!(
                    "om_wm: cursor {name} {}x{} hotspot {},{} from {}",
                    image.w,
                    image.h,
                    image.hot_x,
                    image.hot_y,
                    path.display()
                );
                return Some(image);
            }
        }
    }
    None
}

fn size_from_env() -> u32 {
    std::env::var("XCURSOR_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_SIZE)
}

// Where to look, from XCURSOR_PATH if it is set and from the usual places if it is not. A leading
// ~ is expanded here rather than by a shell, since nothing here goes through one.
fn dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let expand = |s: &str| -> PathBuf {
        match s.strip_prefix("~/") {
            Some(rest) if !home.is_empty() => PathBuf::from(&home).join(rest),
            _ => PathBuf::from(s),
        }
    };
    match std::env::var("XCURSOR_PATH") {
        Ok(path) if !path.is_empty() => path.split(':').map(expand).collect(),
        _ => SEARCH.iter().map(|s| expand(s)).collect(),
    }
}

// The theme the system is set to, which is written as an inheritance from the pseudo-theme called
// "default" rather than stated anywhere directly.
fn system_theme() -> Option<String> {
    inherits("default")
}

// What a theme inherits, out of its index.theme. A flat key = value read, because that file is one
// and pulling in an ini parser to read a single line would be the tail wagging the dog.
fn inherits(theme: &str) -> Option<String> {
    for dir in dirs() {
        let path = dir.join(theme).join("index.theme");
        let Ok(text) = fs::read_to_string(&path) else { continue };
        for line in text.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("Inherits") else { continue };
            let Some(value) = rest.trim_start().strip_prefix('=') else { continue };
            // A list is allowed; the first name is the one that wins.
            let first = value.split(',').next()?.trim();
            if !first.is_empty() {
                return Some(first.to_string());
            }
        }
    }
    None
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let end = at.checked_add(4)?;
    let slice = bytes.get(at..end)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

// Pick the image whose nominal size is nearest the one asked for, and read it.
//
// Nearest rather than at least, because a theme ships a handful of sizes and the closest of them is
// what a toolkit picks too: asking for 24 from a theme with 16 and 32 should give the same answer
// everywhere, or the canvas pointer and the window pointers disagree by a size.
fn parse(bytes: &[u8], want: u32) -> Option<Image> {
    if u32_at(bytes, 0)? != MAGIC {
        return None;
    }
    let header = u32_at(bytes, 4)? as usize;
    let ntoc = u32_at(bytes, 12)? as usize;

    let mut best: Option<(u32, usize)> = None;
    for i in 0..ntoc {
        let at = header + i * TOC_ENTRY;
        if u32_at(bytes, at)? != CHUNK_IMAGE {
            continue;
        }
        let size = u32_at(bytes, at + 4)?;
        let pos = u32_at(bytes, at + 8)? as usize;
        // The image has to fit the plane whatever its nominal size claims, so a theme with only
        // enormous cursors is skipped rather than cropped to a corner of itself.
        let (w, h) = (u32_at(bytes, pos + 16)?, u32_at(bytes, pos + 20)?);
        if w == 0 || h == 0 || w > 64 || h > 64 {
            continue;
        }
        let closer = match best {
            None => true,
            Some((have, _)) => size.abs_diff(want) < have.abs_diff(want),
        };
        if closer {
            best = Some((size, pos));
        }
    }

    let (_, pos) = best?;
    let w = u32_at(bytes, pos + 16)? as i32;
    let h = u32_at(bytes, pos + 20)? as i32;
    let hot_x = u32_at(bytes, pos + 24)? as i32;
    let hot_y = u32_at(bytes, pos + 28)? as i32;
    let count = (w as usize).checked_mul(h as usize)?;
    let mut pixels = Vec::with_capacity(count);
    for i in 0..count {
        pixels.push(u32_at(bytes, pos + IMAGE_HEADER + i * 4)?);
    }
    Some(Image { w, h, hot_x, hot_y, pixels })
}
