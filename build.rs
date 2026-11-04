// Build script: stamps the file icon and version metadata into the executable.
//
// Windows reads an exe's Explorer icon from an `RT_ICON` resource, not from the
// runtime `with_icon()` call, so without this the shipped binary shows a blank
// generic sheet. The version block is what File Properties and Task Manager
// display instead of "fxviewer.exe".
//
// Deliberately optional: on a non-Windows host, or when `rc.exe` is missing, the
// build continues without resources instead of failing.

use std::path::PathBuf;
use std::process::Command;

#[allow(dead_code)]
#[path = "src/icon_art.rs"]
mod icon_art;

/// Sizes packed into the .ico. 16-64 as BMP (what the classic shell surfaces
/// expect), 128-256 as PNG (a 256px BMP alone would add 260 KB).
const SIZES: [usize; 7] = [16, 24, 32, 48, 64, 128, 256];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/icon_art.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR not set"));

    std::fs::write(out.join("fxviewer.ico"), build_ico()).expect("write icon");
    std::fs::write(out.join("fxviewer.rc"), RESOURCES).expect("write resource script");

    let rc = match find_rc() {
        Some(p) => p,
        None => {
            println!("cargo:warning=rc.exe not found; building without file icon");
            return;
        }
    };

    match Command::new(&rc)
        .current_dir(&out)
        .arg("/nologo")
        .arg("/fo")
        .arg("fxviewer.res")
        .arg("fxviewer.rc")
        .output()
    {
        Ok(o) if o.status.success() => {
            println!("cargo:rustc-link-arg={}", out.join("fxviewer.res").display());
        }
        Ok(o) => println!(
            "cargo:warning=rc.exe failed; building without file icon: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=could not run rc.exe: {e}"),
    }
}

/// Resource compiler shipped with the Windows SDK. Picks the newest version
/// installed; `FXVIEWER_RC` overrides for unusual setups.
fn find_rc() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FXVIEWER_RC") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
    }

    let mut best: Option<(Vec<u32>, PathBuf)> = None;
    for var in ["ProgramFiles(x86)", "ProgramFiles"] {
        let Ok(base) = std::env::var(var) else {
            continue;
        };
        let bin = PathBuf::from(base)
            .join("Windows Kits")
            .join("10")
            .join("bin");
        let Ok(entries) = std::fs::read_dir(&bin) else {
            continue;
        };

        for entry in entries.flatten() {
            let exe = entry.path().join("x64").join("rc.exe");
            if !exe.is_file() {
                continue;
            }
            let version: Vec<u32> = entry
                .file_name()
                .to_string_lossy()
                .split('.')
                .map(|p| p.parse().unwrap_or(0))
                .collect();
            if best.as_ref().is_none_or(|(v, _)| version > *v) {
                best = Some((version, exe));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn build_ico() -> Vec<u8> {
    let images: Vec<(usize, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| {
            let rgba = icon_art::render(size);
            let data = if size >= 128 {
                png(&rgba, size)
            } else {
                dib(&rgba, size)
            };
            (size, data)
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());

    let mut offset = 6 + 16 * images.len();
    for (size, data) in &images {
        let dim = if *size >= 256 { 0 } else { *size as u8 }; // 0 means 256
        out.push(dim);
        out.push(dim);
        out.push(0); // palette colours
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += data.len();
    }
    for (_, data) in &images {
        out.extend_from_slice(data);
    }
    out
}

/// Classic 32bpp icon bitmap: BITMAPINFOHEADER, bottom-up BGRA, then a 1bpp
/// AND mask (unused because the alpha channel carries transparency, but the
/// format requires it to be present).
fn dib(rgba: &[u8], size: usize) -> Vec<u8> {
    let mask_row = size.div_ceil(32) * 4;
    let mask = mask_row * size;
    let pixels = size * size * 4;

    let mut out = Vec::with_capacity(40 + pixels + mask);
    out.extend_from_slice(&40u32.to_le_bytes()); // header size
    out.extend_from_slice(&(size as i32).to_le_bytes()); // width
    out.extend_from_slice(&((size * 2) as i32).to_le_bytes()); // height incl. mask
    out.extend_from_slice(&1u16.to_le_bytes()); // planes
    out.extend_from_slice(&32u16.to_le_bytes()); // bpp
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&((pixels + mask) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 16]); // resolution + palette counts

    for y in (0..size).rev() {
        for x in 0..size {
            let i = (y * size + x) * 4;
            out.push(rgba[i + 2]);
            out.push(rgba[i + 1]);
            out.push(rgba[i]);
            out.push(rgba[i + 3]);
        }
    }
    out.resize(out.len() + mask, 0);
    out
}

fn png(rgba: &[u8], size: usize) -> Vec<u8> {
    let mut raw = Vec::with_capacity(size * (size * 4 + 1));
    for y in 0..size {
        raw.push(0); // filter: none
        raw.extend_from_slice(&rgba[y * size * 4..(y + 1) * size * 4]);
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 9);

    let mut out = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&(size as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, adaptive, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);

    let mut crc = 0xFFFF_FFFFu32;
    for &b in kind.iter().chain(data.iter()) {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                0xEDB8_8320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
        }
    }
    out.extend_from_slice(&(crc ^ 0xFFFF_FFFF).to_be_bytes());
}

const RESOURCES: &str = r#"1 ICON "fxviewer.ico"

1 VERSIONINFO
  FILEVERSION 0,1,0,0
  PRODUCTVERSION 0,1,0,0
  FILEFLAGSMASK 0x3fL
  FILEFLAGS 0x0L
  FILEOS 0x40004L
  FILETYPE 0x1L
  FILESUBTYPE 0x0L
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "CompanyName", "FXViewer"
      VALUE "FileDescription", "FXViewer - resource terminal"
      VALUE "FileVersion", "0.2.12.0"
      VALUE "InternalName", "FXViewer"
      VALUE "LegalCopyright", "MIT licensed"
      VALUE "OriginalFilename", "FXViewer.exe"
      VALUE "ProductName", "FXViewer"
      VALUE "ProductVersion", "0.2.12"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#;
