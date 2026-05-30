use std::fs;
use std::fs::File;
use std::io::{BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use image::AnimationDecoder;
use image::codecs::gif::{GifDecoder, GifEncoder, Repeat};
use image::{DynamicImage, ImageFormat};
use log::{error, info};

use crate::common::{
    EngineError, FileKind, MAX_BINARY_INPUT_BYTES, MAX_GIF_FRAMES, detect_kind, ensure_file_size_within,
};

const WEBP_QUALITY: f32 = 80.0;
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
    } else {
        info!("decode raster source image with image::open: {}", src.display());
        image::open(src).with_context(|| format!("failed to decode image: {}", src.display()))
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
