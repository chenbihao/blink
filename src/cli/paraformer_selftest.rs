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

    // ── 8. 执行 encoder 最小推理（ParaformerOnline 真实输入契约）─────
    //
    // encoder 输入：speech [-1,-1,560]（fbank LFR 特征）+ speech_lengths [-1]。
    // 动态维取 T=64、batch=1 构造全零输入——只验证 Session 可执行，
    // 不评估识别质量。
    const ENC_FEATS: i64 = 64;
    const ENC_DIM: i64 = 560;

    let enc_inputs = encoder_session.inputs();
    if enc_inputs.is_empty() {
        return Err("encoder 模型没有输入".to_string());
    }
    let enc_shape: Vec<i64> = match enc_inputs[0].dtype() {
        ValueType::Tensor { shape, .. } => shape.iter().copied().collect(),
        _ => return Err("encoder 输入非 tensor 类型".to_string()),
    };
    println!("encoder input shape: {enc_shape:?}");
    let speech = ort::value::Value::from_array(ndarray::Array3::<f32>::zeros((
        1,
        ENC_FEATS as usize,
        ENC_DIM as usize,
    )))
    .map_err(|e| format!("encoder speech 输入构造失败: {e}"))?;
    let speech_lengths = ort::value::Value::from_array(ndarray::Array1::<i32>::from_elem(
        ENC_FEATS as usize,
        ENC_FEATS as i32,
    ))
    .map_err(|e| format!("encoder speech_lengths 输入构造失败: {e}"))?;

    match encoder_session.run(ort::inputs![
        "speech" => speech,
        "speech_lengths" => speech_lengths,
    ]) {
        Ok(outputs) => {
            println!("encoder 最小推理成功，输出数量: {}", outputs.len());
        }
        Err(e) => {
            return Err(format!("encoder 最小推理失败（模型/DLL 契约异常）: {e}"));
        }
    }

    // ── 9. 执行 decoder 最小推理（完整输入契约：enc + 16 层 cache）────
    //
    // decoder 输入：enc [-1,-1,512]、enc_len [-1]、acoustic_embeds [-1,-1,512]、
    // acoustic_embeds_len [-1]、in_cache_0..15 [-1,512,10]。token 数取 8。
    const DEC_ENC_T: usize = 64;
    const DEC_DIM: usize = 512;
    const DEC_TOKENS: usize = 8;
    const DEC_CACHE_T: usize = 10;
    const DEC_LAYERS: usize = 16;

    let dec_inputs = decoder_session.inputs();
    if dec_inputs.is_empty() {
        return Err("decoder 模型没有输入".to_string());
    }
    println!(
        "decoder input count: {}（期望 {}）",
        dec_inputs.len(),
        4 + DEC_LAYERS
    );

    let dec_enc =
        ort::value::Value::from_array(ndarray::Array3::<f32>::zeros((1, DEC_ENC_T, DEC_DIM)))
            .map_err(|e| format!("decoder enc 输入构造失败: {e}"))?;
    // enc_len 形状 [-1] 是 batch 维——batch=1 时为 [T]
    let dec_enc_len =
        ort::value::Value::from_array(ndarray::Array1::<i32>::from_elem(1, DEC_ENC_T as i32))
            .map_err(|e| format!("decoder enc_len 输入构造失败: {e}"))?;
    let dec_embeds =
        ort::value::Value::from_array(ndarray::Array3::<f32>::zeros((1, DEC_TOKENS, DEC_DIM)))
            .map_err(|e| format!("decoder acoustic_embeds 输入构造失败: {e}"))?;
    let dec_embeds_len =
        ort::value::Value::from_array(ndarray::Array1::<i32>::from_elem(1, DEC_TOKENS as i32))
            .map_err(|e| format!("decoder acoustic_embeds_len 输入构造失败: {e}"))?;

    // ort::inputs! 宏要求静态元组——16 层 cache 逐层展开
    let c0 =
        ort::value::Value::from_array(ndarray::Array3::<f32>::zeros((1, DEC_DIM, DEC_CACHE_T)))
            .map_err(|e| format!("decoder cache 输入构造失败: {e}"))?;
    // 构造 16 层 cache（每层独立 Value，不能复用）
    macro_rules! mk_cache {
        ($idx:literal) => {
            ort::value::Value::from_array(ndarray::Array3::<f32>::zeros((1, DEC_DIM, DEC_CACHE_T)))
                .map_err(|e| format!("decoder in_cache_{} 构造失败: {e}", $idx))?
        };
    }
    let c1 = mk_cache!(1);
    let c2 = mk_cache!(2);
    let c3 = mk_cache!(3);
    let c4 = mk_cache!(4);
    let c5 = mk_cache!(5);
    let c6 = mk_cache!(6);
    let c7 = mk_cache!(7);
    let c8 = mk_cache!(8);
    let c9 = mk_cache!(9);
    let c10 = mk_cache!(10);
    let c11 = mk_cache!(11);
    let c12 = mk_cache!(12);
    let c13 = mk_cache!(13);
    let c14 = mk_cache!(14);
    let c15 = mk_cache!(15);

    match decoder_session.run(ort::inputs![
        "enc" => dec_enc,
        "enc_len" => dec_enc_len,
        "acoustic_embeds" => dec_embeds,
        "acoustic_embeds_len" => dec_embeds_len,
        "in_cache_0" => c0,
        "in_cache_1" => c1,
        "in_cache_2" => c2,
        "in_cache_3" => c3,
        "in_cache_4" => c4,
        "in_cache_5" => c5,
        "in_cache_6" => c6,
        "in_cache_7" => c7,
        "in_cache_8" => c8,
        "in_cache_9" => c9,
        "in_cache_10" => c10,
        "in_cache_11" => c11,
        "in_cache_12" => c12,
        "in_cache_13" => c13,
        "in_cache_14" => c14,
        "in_cache_15" => c15,
    ]) {
        Ok(outputs) => {
            println!("decoder 最小推理成功，输出数量: {}", outputs.len());
        }
        Err(e) => {
            return Err(format!("decoder 最小推理失败（模型/DLL 契约异常）: {e}"));
        }
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
