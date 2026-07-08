use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Once;

use anyhow::{Context, Result, anyhow, bail};
use image::AnimationDecoder;
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{DynamicImage, ImageFormat};
use libheif_rs::integration::image::register_all_decoding_hooks;
use log::{error, info, warn};

#[cfg(target_os = "windows")]
use std::iter;
#[cfg(target_os = "windows")]
use std::os::windows::ffi::OsStrExt;
#[cfg(target_os = "windows")]
use windows::core::{HRESULT, PCWSTR};
#[cfg(target_os = "windows")]
use windows::Win32::Foundation::GENERIC_READ;
#[cfg(target_os = "windows")]
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppRGBA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeCustom, WICDecodeMetadataCacheOnDemand,
};
#[cfg(target_os = "windows")]
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};

use crate::common::{
    EngineError, FileKind, MAX_BINARY_INPUT_BYTES, MAX_GIF_FRAMES, detect_kind, ensure_file_size_within,
};

const WEBP_QUALITY: f32 = 80.0;
static HEIF_HOOKS_INIT: Once = Once::new();

/// GIF 编码速度，取值范围 [1, 30]：数值越大越快、质量略降；数值越小越慢、质量越高。
/// image crate 默认（`GifEncoder::new`）用 1，对整页 PDF 光栅图会非常慢且耗内存。
/// 取 10 作为质量与性能的平衡点，足以保证文档类图片清晰可读。
const GIF_ENCODE_SPEED: i32 = 10;

pub(crate) fn convert_image(src: &Path, tgt: &Path, tgt_kind: FileKind) -> Result<()> {
    info!(
        "image conversion start: src={}, tgt={}, tgt_kind={:?}",
        src.display(),
        tgt.display(),
        tgt_kind
    );

    if tgt_kind == FileKind::Webp {
        let img = load_any_image(src)?;
        save_as_webp(&img, tgt)?;
    } else if tgt_kind == FileKind::Gif {
        convert_to_gif(src, tgt)?;
    } else {
        let img = load_any_image(src)?;
        let format = image_format_from_kind(tgt_kind)?;
        img.save_with_format(tgt, format)
            .with_context(|| format!("failed to write image: {}", tgt.display()))?;
    }

    info!("image conversion done: src={}, tgt={}", src.display(), tgt.display());
    Ok(())
}

fn load_any_image(src: &Path) -> Result<DynamicImage> {
    let src_kind = detect_kind(src)?;
    if src_kind == FileKind::Webp {
        info!("decode webp source image: {}", src.display());
        let bytes = fs::read(src).with_context(|| format!("failed to read: {}", src.display()))?;
        let decoder = webp::Decoder::new(&bytes);
        let decoded = decoder.decode().ok_or_else(|| {
            error!("failed to decode webp image: {}", src.display());
            anyhow!("failed to decode webp image")
        })?;
        let width = decoded.width();
        let height = decoded.height();
        let rgba = image::RgbaImage::from_raw(width, height, decoded.to_vec()).ok_or_else(|| {
            error!("invalid webp RGBA buffer: {}", src.display());
            anyhow!("invalid webp RGBA buffer")
        })?;
        Ok(DynamicImage::ImageRgba8(rgba))
    } else if src_kind == FileKind::Heic {
        load_heic_image(src)
    } else {
        info!("decode raster source image with image::open: {}", src.display());
        image::open(src).with_context(|| format!("failed to decode image: {}", src.display()))
    }
}

fn load_heic_with_libheif(src: &Path) -> Result<DynamicImage> {
    HEIF_HOOKS_INIT.call_once(register_all_decoding_hooks);
    info!("decode heic/heif source image with libheif hook: {}", src.display());
    image::open(src).with_context(|| format!("failed to decode image: {}", src.display()))
}

#[cfg(not(target_os = "windows"))]
fn load_heic_image(src: &Path) -> Result<DynamicImage> {
    load_heic_with_libheif(src)
}

#[cfg(target_os = "windows")]
struct ComScope {
    should_uninitialize: bool,
}

#[cfg(target_os = "windows")]
impl ComScope {
    fn enter() -> Result<Self> {
        unsafe {
            match CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
                Ok(()) => Ok(Self {
                    should_uninitialize: true,
                }),
                Err(err) if err.code() == HRESULT(0x80010106u32 as i32) => Ok(Self {
                    should_uninitialize: false,
                }),
                Err(err) => Err(anyhow!("failed to initialize COM for HEIC decode: {err}")),
            }
        }
    }
}

#[cfg(target_os = "windows")]
impl Drop for ComScope {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

#[cfg(target_os = "windows")]
fn load_heic_image(src: &Path) -> Result<DynamicImage> {
    let _com = ComScope::enter()?;
    info!("decode heic/heif source image with WIC first: {}", src.display());

    match load_heic_with_wic(src) {
        Ok(img) => Ok(img),
        Err(wic_err) => {
            warn!(
                "WIC HEIC decode failed for {}: {}; fallback to embedded libheif",
                src.display(),
                wic_err
            );
            load_heic_with_libheif(src).with_context(|| {
                format!(
                    "failed to decode HEIC with both WIC and libheif for {}",
                    src.display()
                )
            })
        }
    }
}

#[cfg(target_os = "windows")]
fn load_heic_with_wic(src: &Path) -> Result<DynamicImage> {
    unsafe {
        let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
            .context("failed to create WIC imaging factory")?;

        let wide_path: Vec<u16> = src.as_os_str().encode_wide().chain(iter::once(0)).collect();
        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wide_path.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .with_context(|| format!("failed to open HEIC/HEIF decoder for {}", src.display()))?;

        let frame = decoder
            .GetFrame(0)
            .with_context(|| format!("failed to read HEIC frame: {}", src.display()))?;

        let mut width = 0u32;
        let mut height = 0u32;
        frame
            .GetSize(&mut width, &mut height)
            .with_context(|| format!("failed to query HEIC size: {}", src.display()))?;

        let converter = factory
            .CreateFormatConverter()
            .with_context(|| format!("failed to create WIC format converter: {}", src.display()))?;
        converter
            .Initialize(
                &frame,
                &GUID_WICPixelFormat32bppRGBA,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .with_context(|| format!("failed to convert HEIC pixels to RGBA: {}", src.display()))?;

        let stride = width
            .checked_mul(4)
            .ok_or_else(|| anyhow!("HEIC image stride overflow: {}", src.display()))?;
        let buffer_len = stride
            .checked_mul(height)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| anyhow!("HEIC image buffer overflow: {}", src.display()))?;
        let mut rgba = vec![0u8; buffer_len];

        converter
            .CopyPixels(std::ptr::null(), stride, &mut rgba)
            .with_context(|| format!("failed to copy HEIC pixels: {}", src.display()))?;

        let img = image::RgbaImage::from_raw(width, height, rgba).ok_or_else(|| {
            anyhow!("invalid HEIC RGBA buffer dimensions: {}", src.display())
        })?;
        Ok(DynamicImage::ImageRgba8(img))
    }
}

pub(crate) fn save_as_webp(img: &DynamicImage, tgt: &Path) -> Result<()> {
    info!(
        "encode webp (lossy) with quality={}: {}",
        WEBP_QUALITY,
        tgt.display()
    );
    let encoder = webp::Encoder::from_image(img).map_err(|e| {
        error!("failed to build webp encoder for {}: {e}", tgt.display());
        anyhow!("failed to build webp encoder: {e}")
    })?;
    let webp_data = encoder.encode(WEBP_QUALITY);
    fs::write(tgt, &*webp_data).with_context(|| format!("failed to write: {}", tgt.display()))?;
    info!("webp output written: {}", tgt.display());
    Ok(())
}

/// 把单帧静态图编码成 GIF。
///
/// 直接用 `DynamicImage::save_with_format(_, ImageFormat::Gif)` 会以 speed=1（“不计代价追求质量”）
/// 对整张 PDF 光栅图（2480x3507 ≈ 870 万像素）跑 NeuQuant 量化，既慢又吃内存；并且底层 `gif::Encoder`
/// 在 Drop 时会 `write_trailer().unwrap()`，写盘失败就会 panic。这里改为：
/// 1. 用 `GifEncoder::new_with_speed` 取一个平衡的 speed，显著降低耗时与内存峰值；
/// 2. 编码器包裹 `BufWriter`：Drop 阶段写入的只是结尾标记（落到内存缓冲，不直接写盘，几乎不会失败），
///    随后由我们显式 `flush` 把缓冲刷到磁盘，并将真正的 IO 错误转成 `Result` 返回，而不是吞掉或 panic。
pub(crate) fn save_as_gif(img: &DynamicImage, tgt: &Path) -> Result<()> {
    info!("encode gif (single frame): {}", tgt.display());
    let file = File::create(tgt).with_context(|| format!("failed to create gif file: {}", tgt.display()))?;
    let mut writer = BufWriter::new(file);
    {
        let mut encoder = GifEncoder::new_with_speed(&mut writer, GIF_ENCODE_SPEED);
        encoder
            .set_repeat(Repeat::Infinite)
            .with_context(|| format!("failed to set gif repeat: {}", tgt.display()))?;
        encoder
            .encode_frame(image::Frame::new(img.to_rgba8()))
            .with_context(|| format!("failed to encode gif frame: {}", tgt.display()))?;
        // encoder 在此 drop，向 BufWriter 缓冲写入 GIF 结尾标记。
    }
    writer
        .flush()
        .with_context(|| format!("failed to flush gif: {}", tgt.display()))?;
    info!("gif output written: {}", tgt.display());
    Ok(())
}

fn convert_to_gif(src: &Path, tgt: &Path) -> Result<()> {
    let src_kind = detect_kind(src)?;

    if src_kind == FileKind::Gif {
        info!("gif target with gif source: preserve frames when available: {}", src.display());
        return convert_gif_to_gif_preserve_frames(src, tgt);
    }

    info!("gif target with static source, encoding single frame: {}", src.display());
    let img = load_any_image(src)?;
    save_as_gif(&img, tgt)
}

fn convert_gif_to_gif_preserve_frames(src: &Path, tgt: &Path) -> Result<()> {
    ensure_file_size_within(src, MAX_BINARY_INPUT_BYTES)?;
    let input = File::open(src).with_context(|| format!("failed to open gif: {}", src.display()))?;
    let decoder = GifDecoder::new(BufReader::new(input))
        .with_context(|| format!("failed to decode gif: {}", src.display()))?;

    let output = File::create(tgt).with_context(|| format!("failed to create gif file: {}", tgt.display()))?;
    let mut writer = BufWriter::new(output);
    let mut frame_count = 0usize;
    {
        let mut encoder = GifEncoder::new_with_speed(&mut writer, GIF_ENCODE_SPEED);
        encoder
            .set_repeat(Repeat::Infinite)
            .with_context(|| format!("failed to set gif repeat: {}", tgt.display()))?;

        for frame in decoder.into_frames() {
            if frame_count >= MAX_GIF_FRAMES {
                return Err(EngineError::DecodeLimit("gif frame count exceeds safety limit").into());
            }
            let frame = frame.with_context(|| format!("failed to decode gif frame: {}", src.display()))?;
            encoder
                .encode_frame(frame)
                .with_context(|| format!("failed to encode gif frame: {}", tgt.display()))?;
            frame_count += 1;
        }
    }
    writer
        .flush()
        .with_context(|| format!("failed to flush gif: {}", tgt.display()))?;

    if frame_count == 0 {
        return Err(EngineError::DecodeLimit("gif contains no frames").into());
    }

    info!(
        "gif frame preservation done: src={}, tgt={}, frames={}",
        src.display(),
        tgt.display(),
        frame_count
    );

    Ok(())
}

pub(crate) fn image_format_from_kind(kind: FileKind) -> Result<ImageFormat> {
    match kind {
        FileKind::Jpg => Ok(ImageFormat::Jpeg),
        FileKind::Png => Ok(ImageFormat::Png),
        FileKind::Bmp => Ok(ImageFormat::Bmp),
        FileKind::Gif => Ok(ImageFormat::Gif),
        _ => bail!("kind is not a non-webp image format: {kind:?}"),
    }
}
