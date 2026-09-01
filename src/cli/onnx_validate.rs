//! ONNX 隔离验证进程（0.22.8-B）。
//!
//! 这是 `blink onnx-validate` 隐藏 CLI 入口的实现。
//! 由 `OnnxRuntimeProvider::self_test` 通过子进程调用。
//!
//! ## 职责
//!
//! 1. 通过 `ort::init_from(path)` 加载 staging DLL
//! 2. 创建 det Session 和 rec Session
//! 3. 执行最小推理（验证不 crash）
//! 4. 成功退出（exit code 0），失败退出（exit code 1 + stderr）
//!
//! ## 设计铁则
//!
//! - **一次性进程**：验证完成后立即退出，不常驻
//! - **不加载主进程 DLL**：通过 `ort::init_from` 显式加载 staging DLL
//! - **不实现生产 OCR**：只做最小验证，不做完整 pipeline

use std::path::PathBuf;

use ort::session::builder::GraphOptimizationLevel;
use ort::value::ValueType;

/// 解析 `--flag value` 参数对。
fn parse_flags(args: &[String]) -> Result<ValidateArgs, String> {
    let mut dll = None;
    let mut det = None;
    let mut rec = None;
    let mut dict = None;
    let mut intra_op: u32 = 1;
    let mut inter_op: u32 = 1;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dll" => {
                i += 1;
                dll = args.get(i).cloned();
            }
            "--det" => {
                i += 1;
                det = args.get(i).cloned();
            }
            "--rec" => {
                i += 1;
                rec = args.get(i).cloned();
            }
            "--dict" => {
                i += 1;
                dict = args.get(i).cloned();
            }
            "--intra-op" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    intra_op = v.parse().map_err(|_| format!("无效的 intra_op: {v}"))?;
                }
            }
            "--inter-op" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    inter_op = v.parse().map_err(|_| format!("无效的 inter_op: {v}"))?;
                }
            }
            _ => {
                // 未知参数，忽略
            }
        }
        i += 1;
    }

    Ok(ValidateArgs {
        dll: PathBuf::from(dll.ok_or("缺少 --dll 参数")?),
        det: PathBuf::from(det.ok_or("缺少 --det 参数")?),
        rec: PathBuf::from(rec.ok_or("缺少 --rec 参数")?),
        dict: PathBuf::from(dict.ok_or("缺少 --dict 参数")?),
        intra_op,
        inter_op,
    })
}

struct ValidateArgs {
    dll: PathBuf,
    det: PathBuf,
    rec: PathBuf,
    dict: PathBuf,
    intra_op: u32,
    #[allow(dead_code)]
    inter_op: u32,
}

/// 从 CLI 参数运行隔离验证。
///
/// 返回 exit code（0=成功，1=失败）。
pub fn run_from_args(args: &[String]) -> i32 {
    let parsed = match parse_flags(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("参数解析失败: {e}");
            return 1;
        }
    };

    match run_validation(&parsed) {
        Ok(()) => {
            println!("ONNX 验证通过");
            0
        }
        Err(e) => {
            eprintln!("ONNX 验证失败: {e}");
            1
        }
    }
}

/// 执行验证逻辑。
///
/// 使用 `ort` crate 加载 DLL、创建 Session、执行最小推理。
fn run_validation(args: &ValidateArgs) -> Result<(), String> {
    // ── 1. 检查文件存在 ──
    if !args.dll.exists() {
        return Err(format!("DLL 不存在: {}", args.dll.display()));
    }
    if !args.det.exists() {
        return Err(format!("det 模型不存在: {}", args.det.display()));
    }
    if !args.rec.exists() {
        return Err(format!("rec 模型不存在: {}", args.rec.display()));
    }
    if !args.dict.exists() {
        return Err(format!("dictionary 不存在: {}", args.dict.display()));
    }

    // ── 2. 加载 ORT DLL ──
    // ort::init_from 返回 Result<EnvironmentBuilder, LoadDynamicError>
    // 如果 DLL 不是有效 PE、版本不兼容或缺少 OrtGetApiBase 导出，
    // init_from 会返回错误（不 panic）
    let init_builder = ort::init_from(&args.dll).map_err(|e| format!("ORT DLL 加载失败: {e}"))?;

    // commit() 返回 bool：true=首次成功，false=已有环境
    let committed = init_builder.commit();
    if committed {
        println!("ORT DLL 加载成功");
    } else {
        // 已有环境——对于隔离验证进程不应发生，但不算 fatal
        println!("ORT 环境已存在（commit 返回 false）");
    }

    // ── 3. 创建 det Session ──
    let mut det_builder =
        ort::session::Session::builder().map_err(|e| format!("Session builder 构造失败: {e}"))?;
    det_builder = det_builder
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| format!("设置优化级别失败: {e}"))?;
    det_builder = det_builder
        .with_intra_threads(args.intra_op as usize)
        .map_err(|e| format!("设置 intra_threads 失败: {e}"))?;
    let mut det_session = det_builder
        .commit_from_file(&args.det)
        .map_err(|e| format!("det Session 创建失败: {e}"))?;

    println!("det Session 创建成功");
    println!("det inputs: {:?}", det_session.inputs());
    println!("det outputs: {:?}", det_session.outputs());

    // ── 4. 创建 rec Session ──
    let mut rec_builder =
        ort::session::Session::builder().map_err(|e| format!("Session builder 构造失败: {e}"))?;
    rec_builder = rec_builder
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| format!("设置优化级别失败: {e}"))?;
    rec_builder = rec_builder
        .with_intra_threads(args.intra_op as usize)
        .map_err(|e| format!("设置 intra_threads 失败: {e}"))?;
    let mut rec_session = rec_builder
        .commit_from_file(&args.rec)
        .map_err(|e| format!("rec Session 创建失败: {e}"))?;

    println!("rec Session 创建成功");
    println!("rec inputs: {:?}", rec_session.inputs());
    println!("rec outputs: {:?}", rec_session.outputs());

    // ── 5. 读取 dictionary（确认可读）──
    let dict_content =
        std::fs::read_to_string(&args.dict).map_err(|e| format!("dictionary 读取失败: {e}"))?;
    if dict_content.is_empty() {
        return Err("dictionary 为空".to_string());
    }
    let line_count = dict_content.lines().count();
    println!("dictionary 读取成功: {line_count} 行");

    // ── 6. 执行最小推理（det Session forward）──
    // 使用模型声明的输入维度，创建一个全零的最小输入
    let det_inputs = det_session.inputs();
    if det_inputs.is_empty() {
        return Err("det 模型没有输入".to_string());
    }

    // 从 Outlet 的 dtype 中提取 shape（i64 数组，-1 表示动态维度）
    let input_shape: Vec<i64> = match det_inputs[0].dtype() {
        ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => {
            return Err("det 输入非 tensor 类型".to_string());
        }
    };
    println!("det input shape: {input_shape:?}");

    // 动态维度（-1 或 0）使用 64——不能太小（如 1），否则模型内部的
    // Resize/ConvTranspose 算子会因 buffer shape mismatch 而失败
    // （{1,64,1,1} != {1,64,2,2}）。64 足够让所有算子正常计算，
    // 且总输入大小远低于 OOM 阈值。
    let resolved_shape: Vec<usize> = input_shape
        .iter()
        .map(|d| if *d <= 0 { 64_usize } else { *d as usize })
        .collect();
    let input_size: usize = resolved_shape.iter().product();

    // 只在输入不太大时执行推理（避免 OOM）
    if input_size <= 1_000_000 && input_size > 0 {
        use ndarray::Array;
        let input = Array::<f32, _>::zeros(ndarray::IxDyn(&resolved_shape)).into_dyn();
        let input_value = ort::value::Value::from_array(input)
            .map_err(|e| format!("输入 Value 构造失败: {e}"))?;

        // det 推理——任何执行错误都应导致 self-test 失败
        match det_session.run(ort::inputs![input_value]) {
            Ok(outputs) => {
                println!("det 最小推理成功，输出数量: {}", outputs.len());
            }
            Err(e) => {
                // 推理失败表明模型输入契约或运行时有问题，不能判为安装成功
                return Err(format!("det 最小推理失败（模型/DLL 契约异常）: {e}"));
            }
        }
    } else {
        return Err(format!(
            "det 输入维度过大或为零，无法执行最小推理: {input_size}"
        ));
    }

    // ── 7. 执行 rec 最小推理 ──
    // rec 模型也需要执行一次推理，确保模型可运行
    let rec_inputs = rec_session.inputs();
    if rec_inputs.is_empty() {
        return Err("rec 模型没有输入".to_string());
    }

    let rec_shape: Vec<i64> = match rec_inputs[0].dtype() {
        ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => {
            return Err("rec 输入非 tensor 类型".to_string());
        }
    };
    println!("rec input shape: {rec_shape:?}");

    let rec_resolved: Vec<usize> = rec_shape
        .iter()
        .map(|d| if *d <= 0 { 64_usize } else { *d as usize })
        .collect();
    let rec_size: usize = rec_resolved.iter().product();

    if rec_size <= 1_000_000 && rec_size > 0 {
        use ndarray::Array;
        let rec_input = Array::<f32, _>::zeros(ndarray::IxDyn(&rec_resolved)).into_dyn();
        let rec_value = ort::value::Value::from_array(rec_input)
            .map_err(|e| format!("rec 输入 Value 构造失败: {e}"))?;

        match rec_session.run(ort::inputs![rec_value]) {
            Ok(outputs) => {
                println!("rec 最小推理成功，输出数量: {}", outputs.len());
            }
            Err(e) => {
                return Err(format!("rec 最小推理失败（模型/DLL 契约异常）: {e}"));
            }
        }
    } else {
        return Err(format!(
            "rec 输入维度过大或为零，无法执行最小推理: {rec_size}"
        ));
    }

    // 清理
    drop(det_session);
    drop(rec_session);

    println!("ONNX 隔离验证全部通过");
    Ok(())
}
