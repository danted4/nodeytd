//! Embeds the app icon into the Windows executables.

fn main() {
    println!("cargo:rerun-if-changed=src/icon_raster.rs");
    #[cfg(windows)]
    embed_windows_icon();
}

#[cfg(windows)]
#[path = "src/icon_raster.rs"]
#[allow(dead_code)]
mod icon_raster;

#[cfg(windows)]
fn embed_windows_icon() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let ico_path = out.join("ytd.ico");
    write_ico(&ico_path, &[16, 24, 32, 48, 64, 256]);
    let mut res = winresource::WindowsResource::new();
    res.set_icon(ico_path.to_str().expect("utf-8 icon path"));
    res.compile().expect("embed ytd.ico");
}

/// An .ico of 32-bit BMP (DIB) images, one per size.
#[cfg(windows)]
fn write_ico(path: &std::path::Path, sizes: &[u32]) {
    let images: Vec<(u32, Vec<u8>)> = sizes
        .iter()
        .map(|&size| (size, rgba_to_dib(size, &icon_raster::raster(size))))
        .collect();
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, dib) in &images {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(dib.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += dib.len() as u32;
    }
    for (_, dib) in images {
        out.extend_from_slice(&dib);
    }
    std::fs::write(path, out).expect("write ytd.ico");
}

#[cfg(windows)]
fn rgba_to_dib(size: u32, rgba: &[u8]) -> Vec<u8> {
    let row_and = size.div_ceil(32) * 4;
    let xor_size = size * size * 4;
    let and_size = row_and * size;
    let mut dib = Vec::with_capacity((40 + xor_size + and_size) as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(size as i32).to_le_bytes());
    dib.extend_from_slice(&(size as i32 * 2).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&0u32.to_le_bytes());
    dib.extend_from_slice(&(xor_size + and_size).to_le_bytes());
    dib.extend_from_slice(&[0u8; 16]);
    for y in (0..size).rev() {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            dib.extend_from_slice(&[rgba[i + 2], rgba[i + 1], rgba[i], rgba[i + 3]]);
        }
    }
    dib.resize(dib.len() + and_size as usize, 0);
    dib
}
