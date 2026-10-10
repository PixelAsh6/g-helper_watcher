//! Build-time: paint the tray G glyph into a multi-size .ico and embed it
//! as the executable icon (mirrors `g_icon_rgba` in src/main.rs).

const SIZES: [u32; 3] = [16, 32, 48];
const GLYPH: [&str; 7] = [
    ".XXX.", "X...X", "X....", "X.XXX", "X...X", "X...X", ".XXX.",
];
// BGRA (BMP order): slate-900 background, green glyph.
const BG: [u8; 4] = [42, 23, 15, 255];
const FG: [u8; 4] = [94, 197, 34, 255];

/// G glyph, BGRA top-down. Cell = size/8 so every downscale by 2 stays crisp.
fn paint(size: u32) -> Vec<u8> {
    let cell = size / 8;
    let ox = (size - 5 * cell) / 2;
    let oy = (size - 7 * cell) / 2;
    let fw = ox + 5 * cell;
    let fh = oy + 7 * cell;
    let mut px = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let mut c = BG;
            if x >= ox && x < fw && y >= oy && y < fh {
                let gx = (x - ox) / cell;
                let gy = (y - oy) / cell;
                if GLYPH[gy as usize].as_bytes()[gx as usize] == b'X' {
                    c = FG;
                }
            }
            px[i..i + 4].copy_from_slice(&c);
        }
    }
    px
}

fn main() {
    // Build stamp: daemon writes it to the config so stale dashboard
    // children (older binaries) recognize themselves and exit.
    println!(
        "cargo:rustc-env=BUILD_ID={}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    );
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("gwatcher.ico");
    let mut blobs: Vec<Vec<u8>> = Vec::new();
    for &s in &SIZES {
        let xor = paint(s);
        let and_stride = s.div_ceil(32) * 4;
        let and_len = and_stride * s;
        let mut blob: Vec<u8> = Vec::new();
        blob.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER size
        blob.extend_from_slice(&(s as i32).to_le_bytes()); // width
        blob.extend_from_slice(&((s as i32) * 2).to_le_bytes()); // height (xor + and)
        blob.extend_from_slice(&1u16.to_le_bytes()); // planes
        blob.extend_from_slice(&32u16.to_le_bytes()); // bpp
        blob.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
        blob.extend_from_slice(&(s * s * 4).to_le_bytes());
        blob.extend_from_slice(&0i32.to_le_bytes()); // xppm
        blob.extend_from_slice(&0i32.to_le_bytes()); // yppm
        blob.extend_from_slice(&0u32.to_le_bytes());
        blob.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..s).rev() {
            // XOR mask: 32-bit BGRA, bottom-up rows
            for x in 0..s {
                let i = ((y * s + x) * 4) as usize;
                blob.extend_from_slice(&xor[i..i + 4]);
            }
        }
        blob.extend(std::iter::repeat_n(0u8, and_len as usize)); // opaque AND mask
        blobs.push(blob);
    }
    let mut ico = vec![0u8, 0, 1, 0];
    ico.extend_from_slice(&(blobs.len() as u16).to_le_bytes());
    let mut offset = (6 + blobs.len() * 16) as u32;
    for (i, blob) in blobs.iter().enumerate() {
        let s = SIZES[i];
        ico.push(s as u8);
        ico.push(s as u8);
        ico.push(0);
        ico.push(0);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(blob.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += blob.len() as u32;
    }
    for blob in &blobs {
        ico.extend_from_slice(blob);
    }
    std::fs::write(&out, &ico).unwrap();
    // Per-monitor-V2 DPI awareness: crisp native rendering on every monitor
    // instead of Windows bitmap-scaling (blurry + wrong size) the window.
    let manifest = out.with_file_name("gwatcher.manifest");
    std::fs::write(
        &manifest,
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">permonitorv2,permonitor</dpiAwareness>
    </windowsSettings>
  </application>
</assembly>
"#,
    )
    .unwrap();
    let mut res = winres::WindowsResource::new();
    res.set_icon(out.to_string_lossy().as_ref());
    res.set_manifest_file(manifest.to_string_lossy().as_ref());
    res.compile().unwrap();
}
