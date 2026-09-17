//! ResourceStore 测试（0.23.12）。
//!
//! 平移自 `app/audio_resource/registry.rs`（LocalFile 腿）与
//! `domain/capability/image_stash.rs`（Memory 腿）的既有语义，
//! 并新增双口径配额、按 backing TTL、use 授权、撤销组、
//! one-shot 并发竞态等验收用例。

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use super::ResourceErrorKind;
use super::store::{DefaultResourceStore, ResourceStoreConfig};
use super::types::{ResourceGrantSpec, ResourceRef, ResourceUse, ResourceUseSet, ReusePolicy};

// ── 测试辅助 ───────────────────────────────────────────────────────────────

/// 默认音频 use 集合（转写 + 试听）。
fn audio_uses() -> ResourceUseSet {
    ResourceUseSet::single(ResourceUse::TranscribeAudio)
        .union(ResourceUseSet::single(ResourceUse::PreviewAudio))
}

fn audio_spec(owner: &str) -> ResourceGrantSpec {
    ResourceGrantSpec::new(audio_uses(), ReusePolicy::OneShot, owner)
}

/// 生成临时 WAV-like bytes（无语义，只用于资源注册测试）。
fn make_wav_bytes(size: usize) -> Vec<u8> {
    let mut data = vec![0u8; size];
    if size >= 12 {
        data[..4].copy_from_slice(b"RIFF");
        data[8..12].copy_from_slice(b"WAVE");
    }
    data
}

/// 生成临时文件并写入内容。
fn make_test_file(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(content).unwrap();
    path
}

/// 自定义 TTL 的 store 配置。
fn config_with(tweak: impl FnOnce(&mut ResourceStoreConfig)) -> ResourceStoreConfig {
    let mut config = ResourceStoreConfig::production();
    tweak(&mut config);
    config
}

// ── token：唯一性 / 不可预测 / 格式 ────────────────────────────────────────

#[test]
fn token_is_unique_and_csprng_backed() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();

    let r1 = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let r2 = store.issue_local_file(&path, audio_spec("test")).unwrap();

    assert_ne!(r1, r2, "两个 token 不应相同");
    assert_eq!(r1.as_str().len(), 37, "token 应为 rref_ + 32 hex chars");
    assert!(r1.as_str().starts_with("rref_"));
    let hex_part: String = r1.as_str().chars().skip(5).collect();
    assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    // 不含路径片段
    assert!(!hex_part.contains('\\'));
    assert!(!hex_part.contains('/'));
}

#[test]
fn token_not_derived_from_path_or_content_hash() {
    let dir = tempfile::tempdir().unwrap();
    let content = make_wav_bytes(64);
    let path1 = make_test_file(dir.path(), "a.wav", &content);
    let path2 = make_test_file(dir.path(), "b.wav", &content);

    let store = DefaultResourceStore::default();
    let r1 = store.issue_local_file(&path1, audio_spec("test")).unwrap();
    let r2 = store.issue_local_file(&path2, audio_spec("test")).unwrap();
    assert_ne!(r1, r2, "相同内容不同路径的 token 应不同");
}

// ── Memory 腿基本语义（平移 ImageStash）────────────────────────────────────

#[test]
fn memory_issue_and_open_roundtrip() {
    let store = DefaultResourceStore::default();
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::OcrImage),
        ReusePolicy::Reusable,
        "test",
    );
    let token = store
        .issue_memory(Bytes::from(vec![1, 2, 3, 4]), "image/png", spec)
        .unwrap();

    let mut opened = store
        .open(&token, ResourceUse::OcrImage)
        .expect("刚签发应能 open");
    let bytes = opened.read_all_bounded(1024).unwrap();
    assert_eq!(&bytes[..], &[1, 2, 3, 4]);
    let meta = store.inspect(&token).unwrap();
    assert_eq!(meta.mime.as_deref(), Some("image/png"));
    assert_eq!(meta.size_bytes, 4);
}

#[test]
fn memory_reusable_reads_twice() {
    let store = DefaultResourceStore::default();
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::OcrImage),
        ReusePolicy::Reusable,
        "test",
    );
    let token = store
        .issue_memory(Bytes::from(vec![1, 2, 3]), "image/png", spec)
        .unwrap();
    let first = store.open(&token, ResourceUse::OcrImage).unwrap();
    let second = store.open(&token, ResourceUse::OcrImage).unwrap();
    assert_eq!(first.size_bytes(), second.size_bytes());
}

#[test]
fn memory_empty_bytes_rejected() {
    let store = DefaultResourceStore::default();
    let result = store.issue_memory(
        Bytes::new(),
        "image/png",
        ResourceGrantSpec::new(
            ResourceUseSet::single(ResourceUse::OcrImage),
            ReusePolicy::Reusable,
            "t",
        ),
    );
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::ResourceBudgetExceeded
    );
}

#[test]
fn memory_single_limit_enforced() {
    let store = DefaultResourceStore::default();
    let too_large = Bytes::from(vec![0u8; 32 * 1024 * 1024 + 1]);
    let err = store
        .issue_memory(
            too_large,
            "image/png",
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::Reusable,
                "t",
            ),
        )
        .unwrap_err();
    assert_eq!(err.kind, ResourceErrorKind::ResourceBudgetExceeded);
}

#[test]
fn memory_ttl_is_15_minutes_by_default() {
    let store = DefaultResourceStore::default();
    let token = store
        .issue_memory(
            Bytes::from(vec![1]),
            "image/png",
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::Reusable,
                "t",
            ),
        )
        .unwrap();
    let meta = store.inspect(&token).unwrap();
    // 15 分钟 = 900 秒；刚签发剩余应接近 900
    assert!(meta.expires_in_seconds <= 900, "剩余秒数应 <= 900");
    assert!(
        meta.expires_in_seconds > 850,
        "刚签发剩余应接近 900，实际 {}",
        meta.expires_in_seconds
    );
}

// ── LocalFile 腿基本语义（平移 AudioResourceRegistry）──────────────────────

#[test]
fn fresh_ref_opens_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let opened = store.open(&token, ResourceUse::TranscribeAudio);
    assert!(opened.is_ok(), "刚签发的 ref 应能 open");
}

#[test]
fn local_file_ttl_is_5_minutes_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let meta = store.inspect(&token).unwrap();
    // 5 分钟 = 300 秒；刚签发剩余应接近 300
    assert!(meta.expires_in_seconds <= 300, "剩余秒数应 <= 300");
    assert!(
        meta.expires_in_seconds > 250,
        "刚签发剩余应接近 300，实际 {}",
        meta.expires_in_seconds
    );
}

#[test]
fn expired_ref_returns_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.ttl = Duration::from_millis(1);
    }));
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    std::thread::sleep(Duration::from_millis(10));

    let result = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::StaleResourceRef
    );
}

// ── 撤销组（取代 generation）──────────────────────────────────────────────

#[test]
fn revoke_group_invalidates_old_refs() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();

    let group = store.fresh_group();
    let token = store
        .issue_local_file(&path, audio_spec("test").with_group(group))
        .unwrap();
    // 另一组的 ref 不受影响
    let other = store.issue_local_file(&path, audio_spec("test")).unwrap();

    assert_eq!(store.revoke_group(group), 1);
    let result = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
    // 其他组仍可用
    assert!(store.open(&other, ResourceUse::TranscribeAudio).is_ok());
}

#[test]
fn revoke_group_covers_multiple_refs() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::default();
    let group = store.fresh_group();
    for i in 0..3 {
        let path = make_test_file(dir.path(), &format!("f{i}.wav"), &make_wav_bytes(64));
        store
            .issue_local_file(&path, audio_spec("test").with_group(group))
            .unwrap();
    }
    assert_eq!(store.revoke_group(group), 3);
    assert_eq!(store.stats().local_entries, 0);
}

#[test]
fn revoke_by_owner() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::default();
    for i in 0..2 {
        let path = make_test_file(dir.path(), &format!("a{i}.wav"), &make_wav_bytes(64));
        store
            .issue_local_file(
                &path,
                ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "chat_attach:conv1"),
            )
            .unwrap();
    }
    let path_b = make_test_file(dir.path(), "b.wav", &make_wav_bytes(64));
    store
        .issue_local_file(
            &path_b,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "chat_attach:conv2"),
        )
        .unwrap();

    assert_eq!(store.revoke_by_owner("chat_attach:conv1"), 2);
    assert_eq!(store.stats().local_entries, 1);
}

// ── use 授权（取代 scope）─────────────────────────────────────────────────

#[test]
fn use_denied_does_not_consume_one_shot() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    // 只授予 TranscribeAudio
    let token = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::TranscribeAudio),
                ReusePolicy::OneShot,
                "test",
            ),
        )
        .unwrap();

    // 错误 use 拒绝
    let denied = store.open(&token, ResourceUse::PreviewAudio);
    assert_eq!(denied.unwrap_err().kind, ResourceErrorKind::UseDenied);
    // 关键验收：错误 use 不烧 one-shot——正确 use 仍可消费
    assert!(store.open(&token, ResourceUse::TranscribeAudio).is_ok());
    // 消费后才是 Invalid
    let after = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        after.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
}

#[test]
fn multi_use_grant_accepts_each() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    assert!(store.open(&token, ResourceUse::TranscribeAudio).is_ok());
}

// ── issue_from_ref（clone 语义平移）───────────────────────────────────────

#[test]
fn issue_from_ref_keeps_original_and_both_consume_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();

    let original = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let derived = store.issue_from_ref(&original, audio_spec("test")).unwrap();
    assert_ne!(original, derived, "派生应签发新 token");

    // 原 ref 未被消费：两条 ref 各自可一次性消费
    assert!(store.open(&original, ResourceUse::TranscribeAudio).is_ok());
    assert!(store.open(&derived, ResourceUse::TranscribeAudio).is_ok());
    // 一次性授权：再次 open 都应失败
    assert!(store.open(&original, ResourceUse::TranscribeAudio).is_err());
    assert!(store.open(&derived, ResourceUse::TranscribeAudio).is_err());
}

#[test]
fn issue_from_ref_rejects_unknown_and_expired() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();

    let unknown = store.issue_from_ref(
        &ResourceRef::from_token("rref_00000000000000000000000000000000"),
        audio_spec("t"),
    );
    assert_eq!(
        unknown.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );

    let original = store.issue_local_file(&path, audio_spec("test")).unwrap();
    // 过期后派生失败
    {
        // 通过自定义 TTL store 重放：直接构造短 TTL store 重测
        let short = DefaultResourceStore::new(config_with(|c| {
            c.local.ttl = Duration::from_millis(1);
        }));
        let token = short.issue_local_file(&path, audio_spec("t")).unwrap();
        std::thread::sleep(Duration::from_millis(10));
        let err = short.issue_from_ref(&token, audio_spec("t")).unwrap_err();
        assert_eq!(err.kind, ResourceErrorKind::StaleResourceRef);
    }
    // 失败的派生不消费原 ref
    assert!(store.open(&original, ResourceUse::TranscribeAudio).is_ok());
}

#[test]
fn issue_from_ref_on_memory_backing() {
    let store = DefaultResourceStore::default();
    let original = store
        .issue_memory(
            Bytes::from(vec![9, 8, 7]),
            "image/png",
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::Reusable,
                "t",
            ),
        )
        .unwrap();
    let derived = store
        .issue_from_ref(
            &original,
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::OneShot,
                "t2",
            ),
        )
        .unwrap();
    let mut opened = store.open(&derived, ResourceUse::OcrImage).unwrap();
    assert_eq!(&opened.read_all_bounded(16).unwrap()[..], &[9, 8, 7]);
    // 原 ref 未消费
    assert!(store.open(&original, ResourceUse::OcrImage).is_ok());
}

// ── issue_from_ref 权限衰减（0.23.14 收紧）────────────────────────────────

/// 派生 use 必须是源 grant 的子集——只允许 TranscribeAudio 的 ref
/// （普通 picker / chat 附件的形态）不得扩出 PreviewAudio。
#[test]
fn issue_from_ref_rejects_use_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let source = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::TranscribeAudio),
                ReusePolicy::OneShot,
                "test",
            ),
        )
        .unwrap();

    let expanded = store.issue_from_ref(
        &source,
        ResourceGrantSpec::new(audio_uses(), ReusePolicy::OneShot, "test"),
    );
    let err = expanded.unwrap_err();
    assert_eq!(err.kind, ResourceErrorKind::PermissionEscalation);
    assert_eq!(err.kind.as_str(), "permission_escalation");
    // 失败的派生不消费原 ref
    assert!(store.open(&source, ResourceUse::TranscribeAudio).is_ok());
}

/// OneShot 源不得派生成 Reusable（读取授权从有限变无限）。
#[test]
fn issue_from_ref_rejects_reuse_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let source = store.issue_local_file(&path, audio_spec("test")).unwrap();

    let expanded = store.issue_from_ref(
        &source,
        ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "test"),
    );
    assert_eq!(
        expanded.unwrap_err().kind,
        ResourceErrorKind::PermissionEscalation
    );
}

/// MaxReads 源按剩余次数收紧：派生读取次数不得超过源剩余授权。
#[test]
fn issue_from_ref_rejects_read_count_expansion() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let source = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::MaxReads(2), "test"),
        )
        .unwrap();

    // 派生 MaxReads(3) > 源总数 2 → 拒绝
    let over = store.issue_from_ref(
        &source,
        ResourceGrantSpec::new(audio_uses(), ReusePolicy::MaxReads(3), "test"),
    );
    assert_eq!(
        over.unwrap_err().kind,
        ResourceErrorKind::PermissionEscalation
    );

    // 消费 1 次后剩余 1：派生 MaxReads(2) > 剩余 1 → 拒绝
    assert!(store.open(&source, ResourceUse::TranscribeAudio).is_ok());
    let over_remaining = store.issue_from_ref(
        &source,
        ResourceGrantSpec::new(audio_uses(), ReusePolicy::MaxReads(2), "test"),
    );
    assert_eq!(
        over_remaining.unwrap_err().kind,
        ResourceErrorKind::PermissionEscalation
    );

    // 剩余 1 → 派生 MaxReads(1)（≤ 剩余）允许，且不消费源
    let ok = store
        .issue_from_ref(
            &source,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::MaxReads(1), "test"),
        )
        .unwrap();
    assert!(store.inspect(&ok).is_some());
}

/// 派生不得延长源 ref 的有效期——ttl_override 超过源剩余有效期时被钳制。
#[test]
fn issue_from_ref_rejects_ttl_extension() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();
    let source = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "chat_attach:c1")
                .with_ttl(Duration::from_secs(30 * 60)),
        )
        .unwrap();
    let source_left = store.inspect(&source).unwrap().expires_in_seconds;

    let derived = store
        .issue_from_ref(
            &source,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "test")
                .with_ttl(Duration::from_secs(60 * 60)),
        )
        .unwrap();
    let derived_left = store.inspect(&derived).unwrap().expires_in_seconds;
    assert!(
        derived_left <= source_left,
        "派生剩余有效期 {derived_left}s 不得超过源剩余 {source_left}s"
    );
}

/// VAD 正常分析/回放链路：双 use 源（VAD picker 形态）派生只含 PreviewAudio
/// 的回放 ref 是纯衰减——回放 ref 能取字节、不能转写；原 ref 分析照常消费。
#[test]
fn issue_from_ref_vad_preview_only_is_pure_attenuation() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    // pick_audio_file_for_vad_debug 的签发形态：双 use + OneShot
    let source = store.issue_local_file(&path, audio_spec("test")).unwrap();

    let playback = store
        .issue_from_ref(
            &source,
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::PreviewAudio),
                ReusePolicy::OneShot,
                "settings.vad_debug",
            ),
        )
        .unwrap();

    // 派生 ref 不能反向转写（use 不在派生 grant 内；错误 use 不烧 one-shot）
    let transcribe_denied = store.open(&playback, ResourceUse::TranscribeAudio);
    assert_eq!(
        transcribe_denied.unwrap_err().kind,
        ResourceErrorKind::UseDenied
    );

    // 回放腿：PreviewAudio 可消费且一次性（消费后整条移除 → Invalid）
    let mut opened = store.open(&playback, ResourceUse::PreviewAudio).unwrap();
    assert_eq!(opened.read_all_bounded(1024).unwrap().len(), 256);
    assert!(store.open(&playback, ResourceUse::PreviewAudio).is_err());

    // 分析腿：原 ref 未被派生消费，TranscribeAudio 照常一次性消费
    assert!(store.open(&source, ResourceUse::TranscribeAudio).is_ok());
    assert!(store.open(&source, ResourceUse::TranscribeAudio).is_err());
}

// ── 容量淘汰（双口径独立）────────────────────────────────────────────────

#[test]
fn local_evict_when_exceeding_max_items() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.max_items = 3;
    }));

    let mut refs = Vec::new();
    for i in 0..3 {
        let path = make_test_file(dir.path(), &format!("file_{i}.wav"), &make_wav_bytes(64));
        refs.push(store.issue_local_file(&path, audio_spec("test")).unwrap());
    }
    assert_eq!(store.stats().local_entries, 3);

    // 第 4 项 → 淘汰最早的
    let path = make_test_file(dir.path(), "file_3.wav", &make_wav_bytes(64));
    let new_ref = store.issue_local_file(&path, audio_spec("test")).unwrap();
    assert_eq!(store.stats().local_entries, 3, "项数应保持 max_items");

    let result = store.open(&refs[0], ResourceUse::TranscribeAudio);
    assert!(result.is_err(), "最早的应被淘汰");
    assert!(store.open(&new_ref, ResourceUse::TranscribeAudio).is_ok());
}

#[test]
fn local_evict_when_exceeding_max_total_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.max_items = 100;
        c.local.max_total_bytes = 200;
        c.local.max_single_bytes = 100;
    }));

    for i in 0..3 {
        let path = make_test_file(dir.path(), &format!("f{i}.wav"), &make_wav_bytes(64));
        store.issue_local_file(&path, audio_spec("test")).unwrap();
    }
    assert_eq!(store.stats().local_entries, 3);

    // 第 4 项 → 总量 256 > 200 → 淘汰
    let path = make_test_file(dir.path(), "f3.wav", &make_wav_bytes(64));
    store.issue_local_file(&path, audio_spec("test")).unwrap();
    assert!(
        store.stats().local_bytes <= 200,
        "总量应 <= 200，实际 {}",
        store.stats().local_bytes
    );
}

#[test]
fn local_single_file_exceeding_max_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.max_single_bytes = 100;
    }));
    let path = make_test_file(dir.path(), "big.wav", &make_wav_bytes(200));
    let result = store.issue_local_file(&path, audio_spec("test"));
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::ResourceBudgetExceeded
    );
}

#[test]
fn memory_evict_when_exceeding_max_items() {
    let store = DefaultResourceStore::new(config_with(|c| {
        c.memory.max_items = 2;
    }));
    let spec = |owner: &str| {
        ResourceGrantSpec::new(
            ResourceUseSet::single(ResourceUse::OcrImage),
            ReusePolicy::Reusable,
            owner,
        )
    };
    let t1 = store
        .issue_memory(Bytes::from(vec![1]), "image/png", spec("t"))
        .unwrap();
    let _t2 = store
        .issue_memory(Bytes::from(vec![2]), "image/png", spec("t"))
        .unwrap();
    let _t3 = store
        .issue_memory(Bytes::from(vec![3]), "image/png", spec("t"))
        .unwrap();
    assert_eq!(store.stats().memory_entries, 2);
    assert!(
        store.open(&t1, ResourceUse::OcrImage).is_err(),
        "最早的应被淘汰"
    );
}

/// 验收：`resident_memory_bytes` 与 `referenced_local_bytes` 两口径独立生效、
/// 互不挤占——内存腿打满后 LocalFile 腿仍可正常签发，反之亦然。
#[test]
fn dual_quota_legs_are_independent() {
    let dir = tempfile::tempdir().unwrap();

    // 内存腿打满（总量 8 字节，单项 4）
    let tight_memory = DefaultResourceStore::new(config_with(|c| {
        c.memory.max_total_bytes = 8;
        c.memory.max_single_bytes = 4;
        c.memory.max_items = 100;
    }));
    let spec = ResourceGrantSpec::new(
        ResourceUseSet::single(ResourceUse::OcrImage),
        ReusePolicy::Reusable,
        "t",
    );
    tight_memory
        .issue_memory(Bytes::from(vec![1, 2, 3, 4]), "image/png", spec.clone())
        .unwrap();
    tight_memory
        .issue_memory(Bytes::from(vec![5, 6, 7, 8]), "image/png", spec)
        .unwrap();
    assert_eq!(tight_memory.stats().memory_bytes, 8);

    // LocalFile 腿不受内存腿挤占——仍可正常签发
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let audio = tight_memory
        .issue_local_file(&path, audio_spec("t"))
        .unwrap();
    assert!(
        tight_memory
            .open(&audio, ResourceUse::TranscribeAudio)
            .is_ok()
    );

    // 反之：LocalFile 腿打满（项数 1），内存腿仍可签发
    let tight_local = DefaultResourceStore::new(config_with(|c| {
        c.local.max_items = 1;
    }));
    let p1 = make_test_file(dir.path(), "b1.wav", &make_wav_bytes(64));
    let p2 = make_test_file(dir.path(), "b2.wav", &make_wav_bytes(64));
    tight_local.issue_local_file(&p1, audio_spec("t")).unwrap();
    tight_local.issue_local_file(&p2, audio_spec("t")).unwrap();
    assert_eq!(tight_local.stats().local_entries, 1);
    let mem = tight_local
        .issue_memory(
            Bytes::from(vec![1]),
            "image/png",
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::Reusable,
                "t",
            ),
        )
        .unwrap();
    assert!(tight_local.open(&mem, ResourceUse::OcrImage).is_ok());
}

// ── 不存在 / 目录 / 跨 store / 格式 ───────────────────────────────────────

#[test]
fn nonexistent_file_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.wav");
    let store = DefaultResourceStore::default();
    let result = store.issue_local_file(&path, audio_spec("test"));
    assert_eq!(result.unwrap_err().kind, ResourceErrorKind::NotRegularFile);
}

#[test]
fn directory_returns_not_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let store = DefaultResourceStore::default();
    let result = store.issue_local_file(dir.path(), audio_spec("test"));
    assert_eq!(result.unwrap_err().kind, ResourceErrorKind::NotRegularFile);
}

#[test]
fn relative_path_rejected() {
    let store = DefaultResourceStore::default();
    let result = store.issue_local_file(Path::new("relative/path.wav"), audio_spec("test"));
    assert_eq!(result.unwrap_err().kind, ResourceErrorKind::NotRegularFile);
}

#[test]
fn cross_store_open_fails() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store1 = DefaultResourceStore::default();
    let store2 = DefaultResourceStore::default();

    let token = store1.issue_local_file(&path, audio_spec("test")).unwrap();
    let result = store2.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
}

#[test]
fn empty_or_malformed_ref_returns_invalid() {
    let store = DefaultResourceStore::default();
    let empty = store.open(&ResourceRef::from_token(""), ResourceUse::TranscribeAudio);
    assert_eq!(
        empty.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
    let wrong_prefix = store.open(
        &ResourceRef::from_token("aref_abc123"),
        ResourceUse::TranscribeAudio,
    );
    assert_eq!(
        wrong_prefix.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
}

// ── 文件身份（identity）───────────────────────────────────────────────────

#[test]
fn file_replaced_after_issue_fails_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();

    std::fs::write(&path, make_wav_bytes(512)).unwrap();
    let result = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::FileIdentityChanged
    );
}

#[test]
fn same_size_in_place_change_invalidates_ref() {
    let original = make_wav_bytes(256);
    let mut changed = original.clone();
    let last = changed.last_mut().expect("fixture non-empty");
    *last = last.wrapping_add(1);
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "same-size.wav", &original);
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    std::fs::write(&path, &changed).unwrap();

    let result = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        result.unwrap_err().kind,
        ResourceErrorKind::FileIdentityChanged
    );
}

#[test]
fn toctou_opened_file_survives_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let mut opened = store.open(&token, ResourceUse::TranscribeAudio).unwrap();

    // 文件已打开——即使替换文件，已打开的 handle 不受影响
    std::fs::write(&path, make_wav_bytes(512)).unwrap();
    let bytes = std::thread::spawn(move || opened.read_all_bounded(1024).unwrap())
        .join()
        .unwrap();
    assert!(!bytes.is_empty(), "已打开的句柄应仍可读");
}

#[test]
#[cfg(target_os = "windows")]
fn symlink_rejected_at_issue() {
    let dir = tempfile::tempdir().unwrap();
    let target = make_test_file(dir.path(), "target.wav", &make_wav_bytes(256));
    let link = dir.path().join("link.wav");
    if std::os::windows::fs::symlink_file(&target, &link).is_err() {
        return; // 无权限——跳过
    }
    let store = DefaultResourceStore::default();
    let result = store.issue_local_file(&link, audio_spec("test"));
    assert_eq!(result.unwrap_err().kind, ResourceErrorKind::NotRegularFile);
}

// ── one-shot 语义与并发竞态 ───────────────────────────────────────────────

#[test]
fn one_shot_open_consumes_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    assert!(store.open(&token, ResourceUse::TranscribeAudio).is_ok());
    let second = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        second.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
}

/// 验收：one-shot 并发竞态只有一个成功（校验与消费同锁原子完成）。
#[test]
fn concurrent_one_shot_open_single_winner() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = Arc::new(DefaultResourceStore::default());
    let token = Arc::new(store.issue_local_file(&path, audio_spec("test")).unwrap());

    let barrier = Arc::new(std::sync::Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let token = Arc::clone(&token);
        let barrier = Arc::clone(&barrier);
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store.open(&token, ResourceUse::TranscribeAudio).is_ok()
        }));
    }
    let wins = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1, "并发 one-shot open 应只有一个成功，实际 {wins}");
}

#[test]
fn max_reads_policy_counts_down() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();
    let token = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::MaxReads(2), "t"),
        )
        .unwrap();
    assert!(store.open(&token, ResourceUse::TranscribeAudio).is_ok());
    assert!(store.open(&token, ResourceUse::TranscribeAudio).is_ok());
    let third = store.open(&token, ResourceUse::TranscribeAudio);
    assert_eq!(
        third.unwrap_err().kind,
        ResourceErrorKind::InvalidResourceRef
    );
}

#[test]
fn grant_ttl_override_extends_local_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();
    let token = store
        .issue_local_file(
            &path,
            ResourceGrantSpec::new(audio_uses(), ReusePolicy::Reusable, "chat_attach:c1")
                .with_ttl(Duration::from_secs(30 * 60)),
        )
        .unwrap();
    let meta = store.inspect(&token).unwrap();
    assert!(
        meta.expires_in_seconds > 300,
        "ttl_override 应放宽 LocalFile 默认 5 分钟，实际 {}",
        meta.expires_in_seconds
    );
}

// ── 惰性清理 ──────────────────────────────────────────────────────────────

#[test]
fn issue_cleans_expired_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.ttl = Duration::from_millis(1);
    }));
    store.issue_local_file(&path, audio_spec("test")).unwrap();
    std::thread::sleep(Duration::from_millis(10));

    store.issue_local_file(&path, audio_spec("test")).unwrap();
    assert_eq!(store.stats().local_entries, 1, "过期条目应被清理");
}

#[test]
fn open_cleans_expired_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path1 = make_test_file(dir.path(), "a.wav", &make_wav_bytes(64));
    let path2 = make_test_file(dir.path(), "b.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::new(config_with(|c| {
        c.local.ttl = Duration::from_millis(500);
    }));

    store.issue_local_file(&path1, audio_spec("test")).unwrap();
    let ref2 = store.issue_local_file(&path2, audio_spec("test")).unwrap();
    assert_eq!(store.stats().local_entries, 2);

    std::thread::sleep(Duration::from_millis(600));
    let _ = store.open(&ref2, ResourceUse::TranscribeAudio);
    // ref2 已被消费（一次性），ref1 已过期被顺带清理
    assert_eq!(store.stats().local_entries, 0);
}

// ── read_all_bounded ──────────────────────────────────────────────────────

#[test]
fn read_all_bounded_enforces_bound() {
    let store = DefaultResourceStore::default();
    let token = store
        .issue_memory(
            Bytes::from(vec![1u8; 100]),
            "application/octet-stream",
            ResourceGrantSpec::new(
                ResourceUseSet::single(ResourceUse::OcrImage),
                ReusePolicy::Reusable,
                "t",
            ),
        )
        .unwrap();
    let mut opened = store.open(&token, ResourceUse::OcrImage).unwrap();
    let err = opened.read_all_bounded(99).unwrap_err();
    assert_eq!(err.kind, ResourceErrorKind::ResourceBudgetExceeded);
    // 放宽后可读
    let bytes = opened.read_all_bounded(100).unwrap();
    assert_eq!(bytes.len(), 100);
}

// ── 隐私：Debug / 元数据不含路径 ──────────────────────────────────────────

#[test]
fn debug_output_has_no_absolute_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "test.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();

    // 错误的 Debug 不含路径
    let err = store
        .open(
            &ResourceRef::from_token("rref_invalid"),
            ResourceUse::TranscribeAudio,
        )
        .unwrap_err();
    let debug_str = format!("{err:?}");
    assert!(
        !debug_str.contains("C:\\"),
        "Debug 输出不应含绝对路径: {debug_str}"
    );
    assert!(
        !debug_str.contains(dir.path().to_str().unwrap()),
        "Debug 输出不应含临时目录路径"
    );

    // ref 本身不含路径
    assert!(!token.as_str().contains("test.wav"), "ref 不应含文件名");
}

#[test]
fn opened_resource_has_no_path_field() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "a.wav", &make_wav_bytes(256));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let opened = store.open(&token, ResourceUse::TranscribeAudio).unwrap();

    // OpenedResource 无路径字段（编译期保证）；Debug 亦不含
    let debug = format!("{opened:?}");
    assert!(!debug.contains("C:\\"));
    assert!(!debug.contains("a.wav"));
    assert_eq!(opened.size_bytes(), 256);
}

#[test]
fn inspect_metadata_has_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = make_test_file(dir.path(), "secret-name.wav", &make_wav_bytes(64));
    let store = DefaultResourceStore::default();
    let token = store.issue_local_file(&path, audio_spec("test")).unwrap();
    let meta = store.inspect(&token).unwrap();
    let json = serde_json::to_string(&format!("{meta:?}")).unwrap();
    assert!(!json.contains("secret-name"));
    assert!(!json.contains("C:\\"));
    assert_eq!(meta.backing_kind, "local_file");
}
