use std::ffi::c_char;
use std::sync::{Mutex, Once, OnceLock};

use anyhow::{Context, Result, anyhow, bail};
use pdfium_render::prelude::Pdfium;

mod common;
mod modules;

pub use common::{ConvertErrorCode, FileKind};

use common::{
    c_path_from_ptr, detect_kind, is_image_kind, map_error_to_code,
};
use modules::image_engine::convert_image;
use modules::office_engine::{
    convert_docx_to_markdown, convert_docx_to_plain_text, convert_excel_to_markdown,
    convert_md_to_docx,
};
use modules::pdf_engine::{convert_pdf_to_image, convert_pdf_to_markdown};
use modules::workflow_nodes::{convert_html_to_pdf, convert_html_to_md, convert_excel_to_csv};
use modules::typst_engine::{convert_md_to_image, convert_md_to_pdf};

static LOGGER_INIT: Once = Once::new();
pub(crate) static PDFIUM_INSTANCE: OnceLock<Pdfium> = OnceLock::new();
/// 串行化 pdfium 的初始化。
///
/// `Pdfium::new()` 内部会对 pdfium-render 自己的进程级全局 `BINDINGS`（OnceCell）做
/// `assert!(BINDINGS.get().is_none())` 和 `assert!(BINDINGS.set(..).is_ok())`，并调用
/// `FPDF_InitLibrary()`——整个进程只能成功执行一次。`get_pdfium_instance` 里仅靠
/// `PDFIUM_INSTANCE.get()` 做检查存在 TOCTOU 竞态：两个转换线程（heavy-task 限流允许 2 个并发）
/// 可能同时看到 `None`，于是各自调用 `Pdfium::new()`，导致第二次 `BINDINGS.set` 失败而 panic，
/// 并因 `FPDF_InitLibrary` 被重复调用而破坏 pdfium 原生全局状态（随后 0xC0000005 访问冲突）。
/// 用这个 Mutex 把“检查 + 初始化”整体串行化，确保 `Pdfium::new()` 全进程只跑一次。
static PDFIUM_INIT_LOCK: Mutex<()> = Mutex::new(());

/// 串行化所有 pdfium 渲染操作。
///
/// pdfium-render 0.9 的 `thread_safe` feature 只是给 `Pdfium` 加了 `unsafe impl Send + Sync`
/// （见 pdfium.rs 末尾），并不会对底层 FFI 调用做任何加锁；而 Google pdfium 原生库本身
/// 不是线程安全的。本项目的重任务限流允许 2 个并发（common.rs 的 HEAVY_TASK_PERMITS=2），
/// 因此可能有两个 PDF→图片任务同时调用 pdfium，触发未定义行为（0xC0000005 访问冲突）。
/// `convert_pdf_to_image` 全程持有此锁，把“打开文档 + 渲染各页”整体串行化。
pub(crate) static PDFIUM_RENDER_LOCK: Mutex<()> = Mutex::new(());


#[unsafe(no_mangle)]
pub unsafe extern "C" fn easytidy_convert_file(src_ptr: *const c_char, tgt_ptr: *const c_char) -> i32 {
    let guard = std::panic::catch_unwind(|| unsafe {
        let src = c_path_from_ptr(src_ptr).context("invalid src path pointer")?;
        let tgt = c_path_from_ptr(tgt_ptr).context("invalid tgt path pointer")?;
        convert_file(&src, &tgt)
    });

    match guard {
        Ok(Ok(())) => ConvertErrorCode::Success as i32,
        Ok(Err(err)) => {
            let code = map_error_to_code(&err);
            eprintln!("[EasyTidyConverter] convert failed (code={code}): {err:?}");
            code
        }
        Err(_) => {
            eprintln!("[EasyTidyConverter] panic captured inside easytidy_convert_file");
            ConvertErrorCode::Panic as i32
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn easytidy_init_logger() -> i32 {
    let guard = std::panic::catch_unwind(|| {
        LOGGER_INIT.call_once(|| {
            let mut builder = env_logger::Builder::from_env(
                env_logger::Env::default().default_filter_or("info"),
            );
            builder.format_timestamp_secs();

            match builder.try_init() {
                Ok(()) => {
                    eprintln!("[EasyTidyConverter] logger initialized (env_logger)");
                }
                Err(err) => {
                    eprintln!("[EasyTidyConverter] logger already initialized or unavailable: {err}");
                }
            }
        });

        ConvertErrorCode::Success as i32
    });

    match guard {
        Ok(code) => code,
        Err(_) => ConvertErrorCode::Panic as i32,
    }
}

fn convert_file(src: &std::path::Path, tgt: &std::path::Path) -> Result<()> {
    let src_kind = detect_kind(src).with_context(|| format!("unsupported src: {}", src.display()))?;
    let tgt_kind = detect_kind(tgt).with_context(|| format!("unsupported tgt: {}", tgt.display()))?;

    match (src_kind, tgt_kind) {
        (a, b) if is_image_kind(a) && is_image_kind(b) => convert_image(src, tgt, b),
        (FileKind::Md, FileKind::Docx) => convert_md_to_docx(src, tgt),
        (FileKind::Md, FileKind::Pdf) => convert_md_to_pdf(src, tgt),
        (FileKind::Md, img) if is_image_kind(img) => convert_md_to_image(src, tgt, img),
        (FileKind::Xlsx, FileKind::Md) => convert_excel_to_markdown(src, tgt),
        (FileKind::Docx, FileKind::Md) => convert_docx_to_markdown(src, tgt),
        (FileKind::Docx, FileKind::Txt) => convert_docx_to_plain_text(src, tgt),
        (FileKind::Pdf, img) if is_image_kind(img) => convert_pdf_to_image(src, tgt, img),
        (FileKind::Pdf, FileKind::Md) => convert_pdf_to_markdown(src, tgt),
        // Node: HTML -> PDF
        (FileKind::Html, FileKind::Pdf) => convert_html_to_pdf(src, tgt),
        // Node: HTML -> Markdown
        (FileKind::Html, FileKind::Md) => convert_html_to_md(src, tgt),
        // Node: XLSX -> CSV
        (FileKind::Xlsx, FileKind::Csv) => convert_excel_to_csv(src, tgt),
        _ => bail!("unsupported conversion route: {src_kind:?} -> {tgt_kind:?}"),
    }
}

pub(crate) fn get_pdfium_instance() -> Result<&'static Pdfium> {
    // 快路径：已初始化则直接返回，无需加锁。
    if let Some(instance) = PDFIUM_INSTANCE.get() {
        return Ok(instance);
    }

    // 慢路径：加锁串行化初始化，避免多线程同时调用 Pdfium::new() 触发
    // pdfium-render 全局 BINDINGS 的二次 set panic 以及 FPDF_InitLibrary 重复初始化。
    // 锁保护的数据是 ()，即便某次持锁期间 panic 导致中毒也可安全恢复，
    // 否则被 FFI 守护捕获的一次 panic 会让后续所有转换永久失败。
    let _guard = PDFIUM_INIT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // 持锁后再次检查：可能在等待锁期间已被其它线程完成初始化。
    if let Some(instance) = PDFIUM_INSTANCE.get() {
        return Ok(instance);
    }

    let bindings = Pdfium::bind_to_library(Pdfium::pdfium_platform_library_name_at_path("./"))
        .or_else(|_| Pdfium::bind_to_system_library())
        .map_err(|e| {
            anyhow!(
                "pdfium library not found ({e}); \
                 place pdfium.dll / libpdfium.so alongside the converter DLL"
            )
        })?;

    // 此处仍持有 PDFIUM_INIT_LOCK，保证全进程只有一个线程能执行 Pdfium::new()。
    let _ = PDFIUM_INSTANCE.set(Pdfium::new(bindings));

    PDFIUM_INSTANCE
        .get()
        .context("failed to initialize global pdfium instance")
}
