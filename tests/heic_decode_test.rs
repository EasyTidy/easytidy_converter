use easytidy_converter::easytidy_convert_file;
use std::ffi::CString;
use std::path::Path;

#[test]
fn test_heic_to_png_decodes_without_store_extension() {
    let src = "test_samples/sample.heic";
    let tgt = "test_output/heic_out.png";
    // test_samples/ 未纳入版本库（沿用现有约定，样本仅本地存放）。缺样本时跳过，避免 CI 硬失败。
    if !Path::new(src).exists() {
        eprintln!("跳过：缺少 HEIC 样本 {src}（本地放一个 .heic 即可运行本用例）");
        return;
    }
    std::fs::create_dir_all("test_output").unwrap();
    let _ = std::fs::remove_file(tgt);

    let c_src = CString::new(src).unwrap();
    let c_tgt = CString::new(tgt).unwrap();
    let code = unsafe { easytidy_convert_file(c_src.as_ptr(), c_tgt.as_ptr()) };
    assert_eq!(code, 0, "HEIC 解码返回错误码 {code}");

    let meta = std::fs::metadata(tgt).expect("输出 PNG 不存在");
    assert!(meta.len() > 1024, "输出 PNG 过小: {} bytes", meta.len());

    // 校验确实是有效 PNG 且能读出尺寸
    let img = image::open(tgt).expect("输出 PNG 无法解码");
    assert!(img.width() > 0 && img.height() > 0);
    println!("HEIC decoded OK -> PNG {}x{}", img.width(), img.height());
}
