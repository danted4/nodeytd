//! Release-only helper: wrap `ytd-gui` in `ytd.app` and zip it.
//! Not an updater asset. Usage: macos-app <gui-binary> <version> <out.zip>

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use ytd::icon_raster;

const ICON_PX: u32 = 1024;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let gui = args
        .next()
        .context("usage: macos-app <gui-binary> <version> <out.zip>")?;
    let version = args
        .next()
        .context("usage: macos-app <gui-binary> <version> <out.zip>")?;
    let out_zip = args
        .next()
        .context("usage: macos-app <gui-binary> <version> <out.zip>")?;
    if args.next().is_some() {
        bail!("usage: macos-app <gui-binary> <version> <out.zip>");
    }
    pack(Path::new(&gui), &version, Path::new(&out_zip))
}

fn pack(gui: &Path, version: &str, out_zip: &Path) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("macos-app only runs on macOS (needs iconutil, codesign, zip)");
    }
    let gui = gui
        .canonicalize()
        .with_context(|| format!("missing GUI binary {}", gui.display()))?;
    let version = parse_version(version)?;
    let out_zip = abs_out(out_zip)?;

    let work = std::env::temp_dir().join(format!("ytd-app-{}", std::process::id()));
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work).context("temp dir")?;
    let result = pack_in(&work, &gui, &version, &out_zip);
    let _ = fs::remove_dir_all(&work);
    result
}

fn pack_in(work: &Path, gui: &Path, version: &str, out_zip: &Path) -> Result<()> {
    let rgba = icon_raster::raster(ICON_PX);
    let png_path = work.join("icon.png");
    write_png(&png_path, ICON_PX, &rgba)?;

    let iconset = work.join("ytd.iconset");
    fs::create_dir_all(&iconset)?;
    for (name, size) in [
        ("icon_16x16.png", 16u32),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    ] {
        let scaled = if size == ICON_PX {
            rgba.clone()
        } else {
            resize_nearest(&rgba, ICON_PX, size)
        };
        write_png(&iconset.join(name), size, &scaled)?;
    }
    let icns = work.join("AppIcon.icns");
    run(Command::new("iconutil")
        .args(["-c", "icns", "-o"])
        .arg(&icns)
        .arg(&iconset))?;

    let app = work.join("ytd.app");
    let macos = app.join("Contents/MacOS");
    let resources = app.join("Contents/Resources");
    fs::create_dir_all(&macos)?;
    fs::create_dir_all(&resources)?;
    let exe = macos.join("ytd-gui");
    fs::copy(gui, &exe).context("copy ytd-gui")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(&exe)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&exe, permissions)?;
    }
    fs::copy(&icns, resources.join("AppIcon.icns"))?;
    fs::write(app.join("Contents/PkgInfo"), b"APPL????")?;
    fs::write(
        app.join("Contents/Info.plist"),
        ytd::update::app_info_plist(version),
    )?;

    let _ = Command::new("xattr").args(["-cr"]).arg(&app).status();
    run(Command::new("codesign")
        .args(["--force", "--deep", "-s", "-"])
        .arg(&app))?;
    let _ = Command::new("xattr").args(["-cr"]).arg(&app).status();

    if let Some(parent) = out_zip.parent() {
        fs::create_dir_all(parent)?;
    }
    let _ = fs::remove_file(out_zip);
    run(Command::new("zip")
        .current_dir(work)
        .env("COPYFILE_DISABLE", "1")
        .args(["-r", "-X"])
        .arg(out_zip)
        .arg("ytd.app"))?;
    eprintln!("wrote {}", out_zip.display());
    Ok(())
}

fn parse_version(raw: &str) -> Result<String> {
    let version = raw.strip_prefix('v').unwrap_or(raw);
    if version.is_empty() || !version.chars().all(|ch| ch.is_ascii_digit() || ch == '.') {
        bail!("version must look like X.Y.Z, got: {raw}");
    }
    Ok(version.to_string())
}

fn abs_out(path: &Path) -> Result<PathBuf> {
    if let Some(parent) = path.parent() {
        if parent.as_os_str().is_empty() {
            return Ok(std::env::current_dir()?.join(path));
        }
        let parent = if parent.exists() {
            parent.canonicalize()?
        } else {
            fs::create_dir_all(parent)?;
            parent.canonicalize()?
        };
        Ok(parent.join(path.file_name().context("zip name")?))
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn run(cmd: &mut Command) -> Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn {:?}", cmd.get_program()))?;
    if !status.success() {
        bail!("{:?} failed with {status}", cmd.get_program());
    }
    Ok(())
}

fn resize_nearest(src: &[u8], src_size: u32, dst_size: u32) -> Vec<u8> {
    let mut out = vec![0_u8; (dst_size as usize) * (dst_size as usize) * 4];
    for y in 0..dst_size {
        for x in 0..dst_size {
            let sx = x * src_size / dst_size;
            let sy = y * src_size / dst_size;
            let si = ((sy * src_size + sx) * 4) as usize;
            let di = ((y * dst_size + x) * 4) as usize;
            out[di..di + 4].copy_from_slice(&src[si..si + 4]);
        }
    }
    out
}

fn write_png(path: &Path, size: u32, rgba: &[u8]) -> Result<()> {
    let expected = (size as usize) * (size as usize) * 4;
    if rgba.len() != expected {
        bail!("rgba length {} != {expected}", rgba.len());
    }
    let mut raw = Vec::with_capacity(((size as usize) * 4 + 1) * size as usize);
    for y in 0..size as usize {
        raw.push(0);
        let start = y * size as usize * 4;
        raw.extend_from_slice(&rgba[start..start + size as usize * 4]);
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&chunk(b"IHDR", &ihdr(size)));
    png.extend_from_slice(&chunk(b"IDAT", &zlib_store(&raw)));
    png.extend_from_slice(&chunk(b"IEND", &[]));
    fs::write(path, png).with_context(|| format!("write {}", path.display()))
}

fn ihdr(size: u32) -> Vec<u8> {
    let mut data = Vec::with_capacity(13);
    data.extend_from_slice(&size.to_be_bytes());
    data.extend_from_slice(&size.to_be_bytes());
    data.extend_from_slice(&[8, 6, 0, 0, 0]);
    data
}

fn chunk(tag: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(tag);
    out.extend_from_slice(data);
    let mut crc = crc32_update(0xffff_ffff, tag);
    crc = crc32_update(crc, data) ^ 0xffff_ffff;
    out.extend_from_slice(&crc.to_be_bytes());
    out
}

fn zlib_store(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0;
    while i < data.len() {
        let end = (i + 65535).min(data.len());
        let chunk = &data[i..end];
        let last = end == data.len();
        let n = chunk.len() as u16;
        out.push(if last { 1 } else { 0 });
        out.extend_from_slice(&n.to_le_bytes());
        out.extend_from_slice(&(!n).to_le_bytes());
        out.extend_from_slice(chunk);
        i = end;
    }
    if data.is_empty() {
        out.push(1);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(!0u16).to_le_bytes());
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn crc32_update(mut crc: u32, data: &[u8]) -> u32 {
    for &b in data {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn adler32(data: &[u8]) -> u32 {
    let mut a = 1u32;
    let mut b = 0u32;
    for &byte in data {
        a = (a + u32::from(byte)) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_v_prefix_and_rejects_junk() {
        assert_eq!(parse_version("v3.2.2").unwrap(), "3.2.2");
        assert_eq!(parse_version("3.2.2").unwrap(), "3.2.2");
        assert!(parse_version("").is_err());
        assert!(parse_version("3.2.2-rc1").is_err());
    }

    #[test]
    fn png_starts_with_signature() {
        let dir = std::env::temp_dir().join(format!("ytd-png-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.png");
        write_png(&path, 1, &[1, 2, 3, 255]).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        assert!(bytes.len() > 33, "IHDR + IDAT");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn nearest_resize_keeps_corners() {
        let src = vec![10, 0, 0, 255, 0, 20, 0, 255, 0, 0, 30, 255, 40, 0, 0, 255];
        let out = resize_nearest(&src, 2, 1);
        assert_eq!(&out[..4], &[10, 0, 0, 255]);
    }
}
