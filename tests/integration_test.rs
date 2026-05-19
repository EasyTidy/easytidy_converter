use easytidy_converter::easytidy_convert_file; // 导入你的导出函数
use std::ffi::CString;
use std::path::Path;

#[test]
fn test_all_conversions() {
    let samples = vec![
        ("test_samples/sample_chinese.md", "test_output/out_md.pdf"),
        ("test_samples/sample.pdf", "test_output/out_pdf.webp"),
        ("test_samples/complex_data.xlsx", "test_output/out_excel.md"),
    ];

    for (src, tgt) in samples {
        let src_path = Path::new(src);
        let tgt_path = Path::new(tgt);

        if let Some(parent) = tgt_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }

        let c_src = CString::new(src_path.to_str().unwrap()).unwrap();
        let c_tgt = CString::new(tgt_path.to_str().unwrap()).unwrap();

        // 1. 调用内核转换
        let result_code = unsafe {
            easytidy_convert_file(c_src.as_ptr(), c_tgt.as_ptr())
        };

        // 2. 通用断言：不管什么格式，返回值必须是 0 (Success)
        assert_eq!(result_code, 0, "内核转换返回了错误码！源文件: {}, 错误码: {}", src, result_code);

        // 3. 针对不同格式，进行差异化物理文件断言
        if src.ends_with(".pdf") {
            // PDF 转图片：断言多页输出的第一页物理文件名
            let page_1_path = tgt_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(format!(
                    "{}_page_1.{}",
                    tgt_path.file_stem().and_then(|s| s.to_str()).unwrap_or("out_pdf"),
                    tgt_path.extension().and_then(|s| s.to_str()).unwrap_or("webp")
                ));
            assert!(page_1_path.exists(), "PDF 转图片失败，第一页未生成！");
            assert!(std::fs::metadata(page_1_path).unwrap().len() > 0, "PDF 转换后的第一页大小为 0");
            println!("✅ PDF 转图片测试成功");
        } else {
            // 常规单文件输出（MD 转 PDF / Excel 转 MD）
            assert!(tgt_path.exists(), "目标文件未生成: {}", tgt);
            assert!(std::fs::metadata(tgt_path).unwrap().len() > 0, "生成的文件大小为 0: {}", tgt);
            println!("✅ {} 转换测试成功", src);
        }
    }
}