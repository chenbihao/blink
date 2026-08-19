import assert from "node:assert/strict";

// Mock window.__TAURI__ for modules that depend on Tauri runtime
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {
        invoke: async () => ({}),
    },
    event: {
        listen: async () => ({unlisten: () => {}}),
    },
};

// Dynamic import after mock is set up
const {fmtTokens, renderContextSection} = await import("./composer-bar-popup.js");

// ── fmtTokens ──

assert.equal(fmtTokens(0), "0");
assert.equal(fmtTokens(999), "999");
assert.equal(fmtTokens(10000), "1.0万");
assert.equal(fmtTokens(12345), "1.2万");
assert.equal(fmtTokens(100000), "10.0万");
assert.equal(fmtTokens(50000), "5.0万");

// ── renderContextSection: 空状态 ──

{
    const html = renderContextSection({context_limit: 0});
    assert.ok(html.includes("未开始"), "context_limit=0 应显示未开始");
    assert.ok(html.includes("发送消息后将显示"), "空状态提示语");
}

// ── renderContextSection: 基本展示 ──

{
    const html = renderContextSection({
        usage_percent: 30,
        context_limit: 8192,
        estimated_tokens: 2000,
        preamble_tokens: 500,
        pending_message_tokens: 100,
        history_tokens: 1400,
        tools_tokens: 0,
        protocol_overhead_tokens: 0,
        multimodal_tokens: 0,
        reserved_output_tokens: 2048,
        safety_margin_tokens: 409,
        effective_input_limit: 5735,
        remaining_tokens: 3735,
        context_limit_source: "configured",
        confidence: "high",
    });

    // 百分比展示
    assert.ok(html.includes("30%"), "应展示百分比 30%");

    // 分项展示
    assert.ok(html.includes("历史消息"), "应展示历史消息行");
    assert.ok(html.includes("系统提示词"), "应展示系统提示词行");
    assert.ok(html.includes("当前消息"), "应展示当前消息行");
    assert.ok(html.includes("输出预留"), "应展示输出预留行");
    assert.ok(html.includes("安全余量"), "应展示安全余量行");
    assert.ok(html.includes("安全剩余"), "应展示安全剩余行");

    // configured 来源不展示标签
    assert.ok(!html.includes("cbp-context-source"), "configured 来源不应展示来源标签");
    assert.ok(!html.includes("cbp-confidence"), "high 置信度不应展示标签");
}

// ── renderContextSection: fallback 来源展示 ──

{
    const html = renderContextSection({
        usage_percent: 50,
        context_limit: 8192,
        estimated_tokens: 4000,
        preamble_tokens: 0,
        pending_message_tokens: 0,
        context_limit_source: "fallback",
        confidence: "high",
    });

    assert.ok(html.includes("cbp-context-source-fallback"), "fallback 来源应展示估算标签");
    assert.ok(html.includes("估算"), "fallback 应显示'估算'文本");
}

// ── renderContextSection: medium 置信度展示 ──

{
    const html = renderContextSection({
        usage_percent: 50,
        context_limit: 8192,
        estimated_tokens: 4000,
        preamble_tokens: 0,
        pending_message_tokens: 0,
        tools_tokens: 300,
        context_limit_source: "configured",
        confidence: "medium",
    });

    assert.ok(html.includes("cbp-confidence"), "medium 置信度应展示标签");
    assert.ok(html.includes("中精度"), "medium 应显示'中精度'文本");
    assert.ok(html.includes("工具定义"), "tools_tokens > 0 时应展示工具定义行");
}

// ── renderContextSection: low 置信度展示 ──

{
    const html = renderContextSection({
        usage_percent: 50,
        context_limit: 8192,
        estimated_tokens: 4000,
        preamble_tokens: 0,
        pending_message_tokens: 0,
        multimodal_tokens: 500,
        context_limit_source: "configured",
        confidence: "low",
    });

    assert.ok(html.includes("cbp-confidence-low"), "low 置信度应展示低精度标签");
    assert.ok(html.includes("低精度"), "low 应显示'低精度'文本");
    assert.ok(html.includes("多模态"), "multimodal_tokens > 0 时应展示多模态行");
}

// ── renderContextSection: 条件渲染（零值不展示） ──

{
    const html = renderContextSection({
        usage_percent: 10,
        context_limit: 8192,
        estimated_tokens: 800,
        preamble_tokens: 0,
        pending_message_tokens: 0,
        tools_tokens: 0,
        protocol_overhead_tokens: 0,
        multimodal_tokens: 0,
        reserved_output_tokens: 0,
        safety_margin_tokens: 0,
        context_limit_source: "configured",
        confidence: "high",
    });

    assert.ok(!html.includes("系统提示词"), "preamble_tokens=0 不应展示系统提示词行");
    assert.ok(!html.includes("当前消息"), "pending_message_tokens=0 不应展示当前消息行");
    assert.ok(!html.includes("工具定义"), "tools_tokens=0 不应展示工具定义行");
    assert.ok(!html.includes("协议开销"), "protocol_overhead_tokens=0 不应展示协议开销行");
    assert.ok(!html.includes("多模态"), "multimodal_tokens=0 不应展示多模态行");
    assert.ok(!html.includes("输出预留"), "reserved_output_tokens=0 不应展示输出预留行");
    assert.ok(!html.includes("安全余量"), "safety_margin_tokens=0 不应展示安全余量行");
}

// ── renderContextSection: 危险区颜色 ──

{
    const html = renderContextSection({
        usage_percent: 85,
        context_limit: 8192,
        estimated_tokens: 7000,
        context_limit_source: "configured",
        confidence: "high",
    });

    // 85% 应使用 danger 颜色变量
    assert.ok(html.includes("--danger"), "85% 应使用 danger 颜色");
}

// ── renderContextSection: 安全剩余回退计算 ──

{
    // 后端未提供 remaining_tokens 时前端自动计算
    const html = renderContextSection({
        usage_percent: 40,
        context_limit: 8192,
        estimated_tokens: 2000,
        effective_input_limit: 5735,
        context_limit_source: "configured",
        confidence: "high",
    });

    // remaining = 5735 - 2000 = 3735
    assert.ok(html.includes("3,735"), "未提供 remaining_tokens 时应自动计算");
}

// ── renderContextSection: 压缩/召回展示 ──

{
    const html = renderContextSection({
        usage_percent: 30,
        context_limit: 8192,
        estimated_tokens: 2000,
        last_compressed: true,
        last_compressed_count: 3,
        last_recall_count: 2,
        context_limit_source: "configured",
        confidence: "high",
    });

    assert.ok(html.includes("已压缩"), "last_compressed=true 应展示已压缩行");
    assert.ok(html.includes("3 条"), "应展示压缩消息条数");
    assert.ok(html.includes("已召回"), "last_recall_count>0 应展示已召回行");
    assert.ok(html.includes("2 条"), "应展示召回消息条数");
}

console.log("Composer bar popup token budget tests passed");
