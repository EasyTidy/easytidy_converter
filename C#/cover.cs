using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading;
using System.Threading.Tasks;

/// <summary>
/// 与 Rust 动态库对应的错误码。
/// 保持与 Rust 侧 ConvertErrorCode 一致，便于业务侧稳定处理。
/// </summary>
public enum EasyTidyResultCode
{
    Success = 0,
    InvalidArg = 1,
    UnsupportedPath = 2,
    IoError = 3,
    DecodeError = 4,
    EncodeError = 5,
    Panic = 100,
    Internal = 255,
}

public static class EasyTidyNative
{
    // DLL 名称按实际产物调整，比如 easytidy_converter.dll。
    // 如果 DLL 不在进程工作目录，可在程序启动时通过 PATH 或 NativeLibrary.Load 显式加载。
    [DllImport("easytidy_converter", EntryPoint = "easytidy_init_logger", CallingConvention = CallingConvention.Cdecl)]
    private static extern int easytidy_init_logger();

    [DllImport("easytidy_converter", EntryPoint = "easytidy_convert_file", CallingConvention = CallingConvention.Cdecl)]
    private static extern int easytidy_convert_file(IntPtr srcUtf8, IntPtr tgtUtf8);

    // 只初始化一次 Rust 侧日志，避免重复初始化带来的噪音。
    private static readonly Lazy<int> LoggerInitCode = new(() => easytidy_init_logger());

    /// <summary>
    /// 初始化 Rust 侧日志。
    /// 建议在应用启动阶段调用一次。
    /// </summary>
    public static EasyTidyResultCode InitLogger()
    {
        return (EasyTidyResultCode)LoggerInitCode.Value;
    }

    /// <summary>
    /// 执行一次转换（同步版本）。
    /// </summary>
    public static EasyTidyResultCode Convert(string srcPath, string tgtPath)
    {
        if (string.IsNullOrWhiteSpace(srcPath)) throw new ArgumentException("source path is empty", nameof(srcPath));
        if (string.IsNullOrWhiteSpace(tgtPath)) throw new ArgumentException("target path is empty", nameof(tgtPath));

        IntPtr srcPtr = IntPtr.Zero;
        IntPtr tgtPtr = IntPtr.Zero;
        try
        {
            srcPtr = StringToUtf8Ptr(srcPath);
            tgtPtr = StringToUtf8Ptr(tgtPath);

            int code = easytidy_convert_file(srcPtr, tgtPtr);
            return (EasyTidyResultCode)code;
        }
        finally
        {
            if (srcPtr != IntPtr.Zero) Marshal.FreeCoTaskMem(srcPtr);
            if (tgtPtr != IntPtr.Zero) Marshal.FreeCoTaskMem(tgtPtr);
        }
    }

    /// <summary>
    /// 执行一次转换（异步版本）。
    /// 注意：底层是 native CPU/IO 密集任务，建议调用方自行控制并发度。
    /// </summary>
    public static Task<EasyTidyResultCode> ConvertAsync(string srcPath, string tgtPath)
    {
        return Task.Run(() => Convert(srcPath, tgtPath));
    }

    /// <summary>
    /// UTF-16 string -> UTF-8 + '\0' 的 unmanaged 内存。
    /// Rust 侧按 C 字符串读取，必须保证以 '\0' 终止。
    /// </summary>
    private static IntPtr StringToUtf8Ptr(string s)
    {
        if (s == null) throw new ArgumentNullException(nameof(s));
        byte[] utf8 = Encoding.UTF8.GetBytes(s + "\0");
        IntPtr ptr = Marshal.AllocCoTaskMem(utf8.Length);
        Marshal.Copy(utf8, 0, ptr, utf8.Length);
        return ptr;
    }
}

/// <summary>
/// 业务侧调用示例（可直接参考）。
/// </summary>
public static class EasyTidyUsageExample
{
    public static async Task RunAsync()
    {
        // 1) 应用启动时初始化日志（可选但建议）。
        EasyTidyResultCode initCode = EasyTidyNative.InitLogger();
        if (initCode != EasyTidyResultCode.Success)
        {
            Console.WriteLine($"Logger init returned: {initCode}");
        }

        // 2) 准备输入输出路径（示例：Markdown -> PDF）。
        string src = @"D:\data\input.md";
        string dst = @"D:\data\output.pdf";

        // 3) 异步执行转换。
        EasyTidyResultCode code = await EasyTidyNative.ConvertAsync(src, dst);

        // 4) 统一处理错误码。
        if (code == EasyTidyResultCode.Success)
        {
            Console.WriteLine("Convert success.");
            return;
        }

        // 根据错误码分级处理（重试、告警、用户提示）。
        switch (code)
        {
            case EasyTidyResultCode.InvalidArg:
                Console.WriteLine("参数错误：请检查路径和文件名。");
                break;
            case EasyTidyResultCode.UnsupportedPath:
                Console.WriteLine("格式组合不支持：请检查源/目标扩展名。");
                break;
            case EasyTidyResultCode.IoError:
                Console.WriteLine("IO 错误：检查文件权限、路径和磁盘空间。");
                break;
            case EasyTidyResultCode.DecodeError:
            case EasyTidyResultCode.EncodeError:
                Console.WriteLine("编解码错误：输入文件可能损坏或格式不合法。");
                break;
            case EasyTidyResultCode.Panic:
            case EasyTidyResultCode.Internal:
                Console.WriteLine("引擎内部错误：建议记录日志并上报。");
                break;
            default:
                Console.WriteLine($"未知错误码: {(int)code}");
                break;
        }
    }
}

/// <summary>
/// 单个批处理任务定义。
/// </summary>
public sealed class EasyTidyBatchItem
{
    public required string SourcePath { get; init; }
    public required string TargetPath { get; init; }
}

/// <summary>
/// 单个批处理任务结果。
/// </summary>
public sealed class EasyTidyBatchResult
{
    public required string SourcePath { get; init; }
    public required string TargetPath { get; init; }
    public EasyTidyResultCode Code { get; init; }
    public Exception? Exception { get; init; }
    public bool IsSuccess => Code == EasyTidyResultCode.Success && Exception == null;
}

/// <summary>
/// 并发批量转换示例：
/// - 使用 SemaphoreSlim 控制并发度
/// - 支持 CancellationToken
/// - 每个任务独立返回结果，不因单个失败中断全批次
/// </summary>
public static class EasyTidyBatchExample
{
    public static async Task<IReadOnlyList<EasyTidyBatchResult>> ConvertBatchAsync(
        IEnumerable<EasyTidyBatchItem> items,
        int maxConcurrency = 4,
        CancellationToken cancellationToken = default)
    {
        if (items == null) throw new ArgumentNullException(nameof(items));
        if (maxConcurrency <= 0) throw new ArgumentOutOfRangeException(nameof(maxConcurrency));

        EasyTidyNative.InitLogger();

        var list = items.ToList();
        var results = new EasyTidyBatchResult[list.Count];
        using var gate = new SemaphoreSlim(maxConcurrency, maxConcurrency);

        var tasks = list.Select((item, index) => ProcessOneAsync(item, index, results, gate, cancellationToken));
        await Task.WhenAll(tasks).ConfigureAwait(false);
        return results;
    }

    private static async Task ProcessOneAsync(
        EasyTidyBatchItem item,
        int index,
        EasyTidyBatchResult[] results,
        SemaphoreSlim gate,
        CancellationToken cancellationToken)
    {
        await gate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            cancellationToken.ThrowIfCancellationRequested();
            EasyTidyResultCode code = await EasyTidyNative.ConvertAsync(item.SourcePath, item.TargetPath).ConfigureAwait(false);
            results[index] = new EasyTidyBatchResult
            {
                SourcePath = item.SourcePath,
                TargetPath = item.TargetPath,
                Code = code,
            };
        }
        catch (OperationCanceledException)
        {
            results[index] = new EasyTidyBatchResult
            {
                SourcePath = item.SourcePath,
                TargetPath = item.TargetPath,
                Code = EasyTidyResultCode.Internal,
                Exception = new TaskCanceledException("batch item canceled"),
            };
        }
        catch (Exception ex)
        {
            results[index] = new EasyTidyBatchResult
            {
                SourcePath = item.SourcePath,
                TargetPath = item.TargetPath,
                Code = EasyTidyResultCode.Internal,
                Exception = ex,
            };
        }
        finally
        {
            gate.Release();
        }
    }

    /// <summary>
    /// 运行演示：批量把 Markdown 转 PDF。
    /// </summary>
    public static async Task RunBatchDemoAsync()
    {
        var jobs = new List<EasyTidyBatchItem>
        {
            new() { SourcePath = @"D:\data\a.md", TargetPath = @"D:\data\out\a.pdf" },
            new() { SourcePath = @"D:\data\b.md", TargetPath = @"D:\data\out\b.pdf" },
            new() { SourcePath = @"D:\data\c.md", TargetPath = @"D:\data\out\c.pdf" },
        };

        using var cts = new CancellationTokenSource();
        IReadOnlyList<EasyTidyBatchResult> results = await ConvertBatchAsync(
            jobs,
            maxConcurrency: 3,
            cancellationToken: cts.Token);

        int ok = results.Count(r => r.IsSuccess);
        int fail = results.Count - ok;
        Console.WriteLine($"Batch done. success={ok}, fail={fail}");

        foreach (var r in results.Where(r => !r.IsSuccess))
        {
            Console.WriteLine($"[FAIL] {r.SourcePath} -> {r.TargetPath}, code={r.Code}, ex={r.Exception?.Message}");
        }
    }
}