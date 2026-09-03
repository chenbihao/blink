//! ParaformerOnline 隔离自测进程（0.22.9）。
//!
//! 这是 `blink paraformer-selftest` 隐藏 CLI 入口的实现。
//! 由 `ParaformerOnnxProvider::self_test` 通过子进程调用。
//!
//! ## 职责
//!
//! 1. 通过 `ort::init_from(path)` 加载 staging DLL
//! 2. 创建 encoder Session 和 decoder Session
//! 3. 执行最小推理（验证不 crash）
//! 4. 验证二进制协议 v2 帧编解码 roundtrip
//! 5. 成功退出（exit code 0），失败退出（exit code 1 + stderr）
//!
//! ## 设计铁则
//!
//! - **一次性进程**：验证完成后立即退出，不常驻
//! - **不加载主进程 DLL**：通过 `ort::init_from` 显式加载 staging DLL
//! - **不实现生产 ASR**：只做最小验证，不做完整 pipeline
//! - **不注册为用户模型**：self-test 只验证部署可行性
//! - **使用二进制协议 v2**：self-test 通过 `stream_worker_proto` 验证
//!   协议层可用性（帧编解码、消息类型覆盖）

use std::path::PathBuf;

use ort::session::builder::GraphOptimizationLevel;
use ort::value::ValueType;

/// 解析 `--flag value` 参数对。
fn parse_flags(args: &[String]) -> Result<SelfTestArgs, String> {
    let mut dll = None;
    let mut encoder = None;
    let mut decoder = None;
    let mut cmvn = None;
    let mut tokenizer = None;
    let mut intra_op: u32 = 1;
    let mut inter_op: u32 = 1;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dll" => {
                i += 1;
                dll = args.get(i).cloned();
            }
            "--encoder" => {
                i += 1;
                encoder = args.get(i).cloned();
            }
            "--decoder" => {
                i += 1;
                decoder = args.get(i).cloned();
            }
            "--cmvn" => {
                i += 1;
                cmvn = args.get(i).cloned();
            }
            "--tokenizer" => {
                i += 1;
                tokenizer = args.get(i).cloned();
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

    Ok(SelfTestArgs {
        dll: PathBuf::from(dll.ok_or("缺少 --dll 参数")?),
        encoder: PathBuf::from(encoder.ok_or("缺少 --encoder 参数")?),
        decoder: PathBuf::from(decoder.ok_or("缺少 --decoder 参数")?),
        cmvn: PathBuf::from(cmvn.ok_or("缺少 --cmvn 参数")?),
        tokenizer: PathBuf::from(tokenizer.ok_or("缺少 --tokenizer 参数")?),
        intra_op,
        inter_op,
    })
}

struct SelfTestArgs {
    dll: PathBuf,
    encoder: PathBuf,
    decoder: PathBuf,
    cmvn: PathBuf,
    tokenizer: PathBuf,
    intra_op: u32,
    #[allow(dead_code)]
    inter_op: u32,
}

/// 从 CLI 参数运行隔离自测。
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

    match run_selftest(&parsed) {
        Ok(()) => {
            println!("paraformer-selftest: OK");
            0
        }
        Err(e) => {
            eprintln!("paraformer-selftest: FAILED: {e}");
            1
        }
    }
}

/// 执行隔离自测。
///
/// 步骤：
/// 1. 验证文件存在
/// 2. 验证二进制协议 v2 帧编解码 roundtrip
/// 3. 加载 ORT DLL
/// 4. 创建 encoder/decoder Session
/// 5. 执行最小推理
/// 6. 验证结果非空
fn run_selftest(args: &SelfTestArgs) -> Result<(), String> {
    // ── 1. 验证文件存在 ──────────────────────────────────────────────
    tracing::info!("验证文件存在性...");
    for (name, path) in [
        ("DLL", &args.dll),
        ("encoder", &args.encoder),
        ("decoder", &args.decoder),
        ("CMVN", &args.cmvn),
        ("tokenizer", &args.tokenizer),
    ] {
        if !path.exists() {
            return Err(format!("{name} 文件不存在: {}", path.display()));
        }
    }

    // ── 2. 验证二进制协议 v2 帧编解码 ────────────────────────────────
    tracing::info!("验证二进制协议 v2 帧编解码...");
    verify_protocol_roundtrip()?;

    // ── 3. 加载 ORT DLL ──────────────────────────────────────────────
    tracing::info!("加载 ORT DLL...");
    let init_builder = ort::init_from(&args.dll).map_err(|e| format!("ORT DLL 加载失败: {e}"))?;
    let committed = init_builder.commit();
    if committed {
        println!("ORT DLL 加载成功");
    } else {
        println!("ORT 环境已存在（commit 返回 false）");
    }

    // ── 4. 创建 encoder Session ──────────────────────────────────────
    tracing::info!("创建 encoder Session...");
    let mut enc_builder =
        ort::session::Session::builder().map_err(|e| format!("Session builder 构造失败: {e}"))?;
    enc_builder = enc_builder
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| format!("设置优化级别失败: {e}"))?;
    enc_builder = enc_builder
        .with_intra_threads(args.intra_op as usize)
        .map_err(|e| format!("设置 intra_threads 失败: {e}"))?;
    let mut encoder_session = enc_builder
        .commit_from_file(&args.encoder)
        .map_err(|e| format!("encoder Session 创建失败: {e}"))?;

    println!("encoder Session 创建成功");
    println!("encoder inputs: {:?}", encoder_session.inputs());
    println!("encoder outputs: {:?}", encoder_session.outputs());

    // ── 5. 创建 decoder Session ──────────────────────────────────────
    tracing::info!("创建 decoder Session...");
    let mut dec_builder =
        ort::session::Session::builder().map_err(|e| format!("Session builder 构造失败: {e}"))?;
    dec_builder = dec_builder
        .with_optimization_level(GraphOptimizationLevel::Level1)
        .map_err(|e| format!("设置优化级别失败: {e}"))?;
    dec_builder = dec_builder
        .with_intra_threads(args.intra_op as usize)
        .map_err(|e| format!("设置 intra_threads 失败: {e}"))?;
    let mut decoder_session = dec_builder
        .commit_from_file(&args.decoder)
        .map_err(|e| format!("decoder Session 创建失败: {e}"))?;

    println!("decoder Session 创建成功");
    println!("decoder inputs: {:?}", decoder_session.inputs());
    println!("decoder outputs: {:?}", decoder_session.outputs());

    // ── 6. 读取 CMVN（确认可读）──────────────────────────────────────
    let cmvn_content =
        std::fs::read_to_string(&args.cmvn).map_err(|e| format!("CMVN 读取失败: {e}"))?;
    if cmvn_content.is_empty() {
        return Err("CMVN 为空".to_string());
    }
    println!("CMVN 读取成功: {} bytes", cmvn_content.len());

    // ── 7. 读取 tokenizer（确认可读）─────────────────────────────────
    let tokenizer_content =
        std::fs::read_to_string(&args.tokenizer).map_err(|e| format!("tokenizer 读取失败: {e}"))?;
    if tokenizer_content.is_empty() {
        return Err("tokenizer 为空".to_string());
    }
    println!("tokenizer 读取成功: {} bytes", tokenizer_content.len());

    // ── 8. 执行 encoder 最小推理 ─────────────────────────────────────
    let enc_inputs = encoder_session.inputs();
    if enc_inputs.is_empty() {
        return Err("encoder 模型没有输入".to_string());
    }

    // 从 Outlet 的 dtype 中提取 shape
    let enc_shape: Vec<i64> = match enc_inputs[0].dtype() {
        ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => {
            return Err("encoder 输入非 tensor 类型".to_string());
        }
    };
    println!("encoder input shape: {enc_shape:?}");

    // 动态维度使用 64
    let enc_resolved: Vec<usize> = enc_shape
        .iter()
        .map(|d| if *d <= 0 { 64_usize } else { *d as usize })
        .collect();
    let enc_size: usize = enc_resolved.iter().product();

    if enc_size <= 1_000_000 && enc_size > 0 {
        use ndarray::Array;
        let enc_input = Array::<f32, _>::zeros(ndarray::IxDyn(&enc_resolved)).into_dyn();
        let enc_value = ort::value::Value::from_array(enc_input)
            .map_err(|e| format!("encoder 输入 Value 构造失败: {e}"))?;

        match encoder_session.run(ort::inputs![enc_value]) {
            Ok(outputs) => {
                println!("encoder 最小推理成功，输出数量: {}", outputs.len());
            }
            Err(e) => {
                return Err(format!("encoder 最小推理失败（模型/DLL 契约异常）: {e}"));
            }
        }
    } else {
        return Err(format!(
            "encoder 输入维度过大或为零，无法执行最小推理: {enc_size}"
        ));
    }

    // ── 9. 执行 decoder 最小推理 ─────────────────────────────────────
    let dec_inputs = decoder_session.inputs();
    if dec_inputs.is_empty() {
        return Err("decoder 模型没有输入".to_string());
    }

    let dec_shape: Vec<i64> = match dec_inputs[0].dtype() {
        ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => {
            return Err("decoder 输入非 tensor 类型".to_string());
        }
    };
    println!("decoder input shape: {dec_shape:?}");

    let dec_resolved: Vec<usize> = dec_shape
        .iter()
        .map(|d| if *d <= 0 { 64_usize } else { *d as usize })
        .collect();
    let dec_size: usize = dec_resolved.iter().product();

    if dec_size <= 1_000_000 && dec_size > 0 {
        use ndarray::Array;
        let dec_input = Array::<f32, _>::zeros(ndarray::IxDyn(&dec_resolved)).into_dyn();
        let dec_value = ort::value::Value::from_array(dec_input)
            .map_err(|e| format!("decoder 输入 Value 构造失败: {e}"))?;

        match decoder_session.run(ort::inputs![dec_value]) {
            Ok(outputs) => {
                println!("decoder 最小推理成功，输出数量: {}", outputs.len());
            }
            Err(e) => {
                return Err(format!("decoder 最小推理失败（模型/DLL 契约异常）: {e}"));
            }
        }
    } else {
        return Err(format!(
            "decoder 输入维度过大或为零，无法执行最小推理: {dec_size}"
        ));
    }

    // 清理
    drop(encoder_session);
    drop(decoder_session);

    println!("paraformer 隔离验证全部通过");
    Ok(())
}

/// 验证二进制协议 v2 帧编解码 roundtrip。
fn verify_protocol_roundtrip() -> Result<(), String> {
    use crate::infra::local_engine::stream_worker_proto::{
        AudioFrame, MAX_PAYLOAD_LEN, MessageType, PROTOCOL_VERSION, read_frame, write_frame,
    };

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("创建 tokio runtime 失败: {e}"))?;

    rt.block_on(async {
        let (mut writer, mut reader) = tokio::io::duplex(8192);

        // 写入各种消息类型
        let test_frames: [(MessageType, u8, u32, u32, Vec<u8>); 7] = [
            (MessageType::Hello, 0, 1, 0, Vec::new()),
            (MessageType::Begin, 0, 2, 1, Vec::new()),
            (MessageType::Audio, 0, 3, 1, vec![0u8; 1280]),
            (MessageType::End, 0, 4, 1, Vec::new()),
            (MessageType::Ready, 0, 5, 0, Vec::new()),
            (MessageType::Final, 0, 6, 1, b"test result".to_vec()),
            (MessageType::Quit, 0, 7, 0, Vec::new()),
        ];

        for (msg_type, flags, req_id, generation, payload) in &test_frames {
            write_frame(
                &mut writer,
                *msg_type,
                *flags,
                *req_id,
                *generation,
                payload,
            )
            .await
            .map_err(|e| format!("写入帧失败: {e}"))?;
        }

        // 读取并验证
        let mut buf = Vec::new();
        for (msg_type, flags, req_id, generation, payload) in &test_frames {
            let (header, read_payload) = read_frame(&mut reader, &mut buf)
                .await
                .map_err(|e| format!("读取帧失败: {e}"))?
                .ok_or("意外 EOF")?;

            if header.msg_type != *msg_type {
                return Err(format!(
                    "消息类型不匹配: 期望 {:?}, 实际 {:?}",
                    msg_type, header.msg_type
                ));
            }
            if header.flags != *flags {
                return Err(format!(
                    "flags 不匹配: 期望 {}, 实际 {}",
                    flags, header.flags
                ));
            }
            if header.request_id != *req_id {
                return Err(format!(
                    "request_id 不匹配: 期望 {}, 实际 {}",
                    req_id, header.request_id
                ));
            }
            if header.generation != *generation {
                return Err(format!(
                    "generation 不匹配: 期望 {}, 实际 {}",
                    generation, header.generation
                ));
            }
            if read_payload != payload.as_slice() {
                return Err("payload 不匹配".to_string());
            }
        }

        // 验证常量
        assert_eq!(PROTOCOL_VERSION, 2);
        const { assert!(MAX_PAYLOAD_LEN > 0) };

        // 验证 AudioFrame 构造
        let frame = AudioFrame::from_samples(&[0.1f32; 320]);
        frame
            .validate()
            .map_err(|e| format!("AudioFrame 校验失败: {e}"))?;

        Ok::<(), String>(())
    })?;

    tracing::info!("二进制协议 v2 帧编解码验证通过");
    Ok(())
}
