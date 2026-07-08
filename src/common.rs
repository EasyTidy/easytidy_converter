use std::ffi::c_char;
use std::fs;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};

use anyhow::{Context, Result, anyhow, bail};

#[derive(thiserror::Error, Debug)]
pub(crate) enum EngineError {
    #[error("invalid argument: {0}")]
    InvalidArg(&'static str),
    #[error("io boundary exceeded: {0}")]
    IoLimit(&'static str),
    #[error("decode boundary exceeded: {0}")]
    DecodeLimit(&'static str),
}

impl EngineError {
    pub(crate) fn code(&self) -> i32 {
        match self {
            Self::InvalidArg(_) => ConvertErrorCode::InvalidArg as i32,
            Self::IoLimit(_) => ConvertErrorCode::IoError as i32,
            Self::DecodeLimit(_) => ConvertErrorCode::DecodeError as i32,
        }
    }
}

pub(crate) const MAX_C_PATH_BYTES: usize = 4096;
pub(crate) const MAX_TEXT_INPUT_BYTES: u64 = 16 * 1024 * 1024;
pub(crate) const MAX_BINARY_INPUT_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const MAX_DOCX_XML_BYTES: u64 = 32 * 1024 * 1024;
pub(crate) const MAX_PDF_PAGES: usize = 300;
pub(crate) const MAX_TYPST_PAGES: usize = 300;
pub(crate) const MAX_GIF_FRAMES: usize = 300;

const HEAVY_TASK_PERMITS: usize = 2;
static HEAVY_TASK_LIMITER: OnceLock<HeavyTaskLimiter> = OnceLock::new();

struct HeavyTaskLimiter {
    state: Mutex<usize>,
    cv: Condvar,
}

pub(crate) struct HeavyTaskPermit<'a> {
    limiter: &'a HeavyTaskLimiter,
}

impl HeavyTaskLimiter {
    fn new(limit: usize) -> Self {
        Self {
            state: Mutex::new(limit),
            cv: Condvar::new(),
        }
    }

    fn acquire(&self) -> Result<HeavyTaskPermit<'_>> {
        let mut remaining = self
            .state
            .lock()
            .map_err(|_| anyhow!("heavy task limiter mutex poisoned"))?;
        while *remaining == 0 {
            remaining = self
                .cv
                .wait(remaining)
                .map_err(|_| anyhow!("heavy task limiter condvar poisoned"))?;
        }
        *remaining -= 1;
        Ok(HeavyTaskPermit { limiter: self })
    }
}

impl Drop for HeavyTaskPermit<'_> {
    fn drop(&mut self) {
        if let Ok(mut remaining) = self.limiter.state.lock() {
            *remaining += 1;
            self.limiter.cv.notify_one();
        }
    }
}

pub(crate) fn acquire_heavy_task_permit() -> Result<HeavyTaskPermit<'static>> {
    HEAVY_TASK_LIMITER
        .get_or_init(|| HeavyTaskLimiter::new(HEAVY_TASK_PERMITS))
        .acquire()
}

#[repr(i32)]
#[derive(Debug, Clone, Copy)]
pub enum ConvertErrorCode {
    Success = 0,
    InvalidArg = 1,
    UnsupportedPath = 2,
    IoError = 3,
    DecodeError = 4,
    EncodeError = 5,
    Panic = 100,
    Internal = 255,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Jpg,
    Png,
    Webp,
    Bmp,
    Gif,
    Heic,
    Md,
    Pdf,
    Docx,
    Xlsx,
    Txt,
    Html,
    Csv,
}

pub(crate) unsafe fn c_path_from_ptr(ptr: *const c_char) -> Result<PathBuf> {
    if ptr.is_null() {
        return Err(EngineError::InvalidArg("null C string pointer").into());
    }

    let mut len = 0usize;
    while len < MAX_C_PATH_BYTES {
        let b = unsafe { *ptr.add(len) };
        if b == 0 {
            break;
        }
        len += 1;
    }

    if len == MAX_C_PATH_BYTES {
        return Err(EngineError::InvalidArg("unterminated C string").into());
    }

    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };
    let s = std::str::from_utf8(bytes)
        .context("C string is not valid UTF-8")?
        .trim();

    if s.is_empty() {
        return Err(EngineError::InvalidArg("path is empty").into());
    }

    Ok(PathBuf::from(s))
}

pub(crate) fn detect_kind(path: &Path) -> Result<FileKind> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("missing extension"))?
        .to_ascii_lowercase();

    let kind = match ext.as_str() {
        "jpg" | "jpeg" => FileKind::Jpg,
        "png" => FileKind::Png,
        "webp" => FileKind::Webp,
        "bmp" => FileKind::Bmp,
        "gif" => FileKind::Gif,
        "heic" | "heif" => FileKind::Heic,
        "md" | "markdown" => FileKind::Md,
        "pdf" => FileKind::Pdf,
        "docx" => FileKind::Docx,
        "xlsx" | "xls" | "ods" => FileKind::Xlsx,
        "txt" => FileKind::Txt,
        "html" | "htm" => FileKind::Html,
        "csv" => FileKind::Csv,
        _ => bail!("extension not supported: {ext}"),
    };

    Ok(kind)
}

pub(crate) fn is_image_kind(kind: FileKind) -> bool {
    matches!(
        kind,
        FileKind::Jpg | FileKind::Png | FileKind::Webp | FileKind::Bmp | FileKind::Gif | FileKind::Heic
    )
}

pub(crate) fn ensure_file_size_within(path: &Path, max_bytes: u64) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("failed to stat input: {}", path.display()))?;
    if !meta.is_file() {
        bail!("input is not a regular file: {}", path.display());
    }
    if meta.len() > max_bytes {
        return Err(EngineError::IoLimit("input file too large").into());
    }
    Ok(())
}

pub(crate) fn read_text_file_limited(path: &Path, max_bytes: u64) -> Result<String> {
    ensure_file_size_within(path, max_bytes)?;
    let file = File::open(path).with_context(|| format!("failed to open: {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut out = String::new();
    reader
        .read_to_string(&mut out)
        .with_context(|| format!("failed to read text file: {}", path.display()))?;
    Ok(out)
}

pub(crate) fn ensure_parent_dir(path: &Path) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if !dir.exists() {
        fs::create_dir_all(dir)
            .with_context(|| format!("failed to create output directory: {}", dir.display()))?;
    }
    Ok(())
}

pub(crate) fn map_error_to_code(err: &anyhow::Error) -> i32 {
    for cause in err.chain() {
        if let Some(typed) = cause.downcast_ref::<EngineError>() {
            return typed.code();
        }
    }

    let msg = err.to_string().to_ascii_lowercase();

    if msg.contains("null c string") || msg.contains("empty") || msg.contains("utf-8") {
        return ConvertErrorCode::InvalidArg as i32;
    }
    if msg.contains("unsupported") || msg.contains("not wired yet") || msg.contains("extension") {
        return ConvertErrorCode::UnsupportedPath as i32;
    }
    if msg.contains("read") || msg.contains("write") || msg.contains("create") || msg.contains("open") {
        return ConvertErrorCode::IoError as i32;
    }
    if msg.contains("decode") {
        return ConvertErrorCode::DecodeError as i32;
    }
    if msg.contains("encode") || msg.contains("pack") {
        return ConvertErrorCode::EncodeError as i32;
    }

    ConvertErrorCode::Internal as i32
}
