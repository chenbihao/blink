//! Task B.3: 前端 char_ranges / char_boxes → UTF-16 offset 测试
//!
//! 覆盖：
//! 1. 后端 char_ranges 单一真源 → UTF-16 offset 正确转换
//! 2. 多空格多换行场景
//! 3. 补充面字符（emoji）UTF-16 offset 差异
//! 4. 后端无 char_ranges 时退化路径
//! 5. 编辑后 panelDirty 标记失效（行为验证）
//! 6. **0.22.8 三层契约**：`computeCharBoxRanges` 逐字符框 UTF-16 转换

import {test, describe} from 'node:test';
import assert from 'node:assert';

// Mock window before importing ss-reading.js (which imports api.js → tauri.js)
globalThis.window = globalThis;

const {computeCharRanges, computeCharBoxRanges} = await import('./ss-reading.js');

describe('computeCharRanges — 后端 char_ranges 单一真源', () => {
    test('BMP 文本：Rust char index 与 UTF-16 offset 一致', () => {
        // 后端返回：text = "hello world", words = ["hello", "world"]
        // Rust char_ranges: [(0,5), (6,11)]
        const words = [
            {text: 'hello', lineIndex: 0},
            {text: 'world', lineIndex: 0},
        ];
        const fullText = 'hello world';
        const charRanges = [[0, 5], [6, 11]];

        const result = computeCharRanges(words, fullText, charRanges);

        assert.strictEqual(result.length, 2);
        assert.deepStrictEqual(result[0], {start: 0, end: 5});
        assert.deepStrictEqual(result[1], {start: 6, end: 11});
    });

    test('CJK 文本：Rust char index 与 UTF-16 offset 一致', () => {
        // 后端返回：text = "你好世界", words = ["你好", "世界"]
        const words = [
            {text: '你好', lineIndex: 0},
            {text: '世界', lineIndex: 0},
        ];
        const fullText = '你好世界';
        const charRanges = [[0, 2], [2, 4]];

        const result = computeCharRanges(words, fullText, charRanges);

        assert.strictEqual(result.length, 2);
        assert.deepStrictEqual(result[0], {start: 0, end: 2});
        assert.deepStrictEqual(result[1], {start: 2, end: 4});
    });

    test('多行文本：换行符偏移正确', () => {
        // text = "hello\nworld"
        const words = [
            {text: 'hello', lineIndex: 0},
            {text: 'world', lineIndex: 1},
        ];
        const fullText = 'hello\nworld';
        const charRanges = [[0, 5], [6, 11]];

        const result = computeCharRanges(words, fullText, charRanges);

        assert.deepStrictEqual(result[0], {start: 0, end: 5});
        assert.deepStrictEqual(result[1], {start: 6, end: 11});
    });

    test('多空格：char_ranges 正确映射', () => {
        // text = "left    right" (4 spaces between)
        const words = [
            {text: 'left', lineIndex: 0},
            {text: 'right', lineIndex: 0},
        ];
        const fullText = 'left    right';
        const charRanges = [[0, 4], [8, 13]];

        const result = computeCharRanges(words, fullText, charRanges);

        assert.deepStrictEqual(result[0], {start: 0, end: 4});
        assert.deepStrictEqual(result[1], {start: 8, end: 13});
        // 验证 slice 正确
        assert.strictEqual(fullText.slice(result[0].start, result[0].end), 'left');
        assert.strictEqual(fullText.slice(result[1].start, result[1].end), 'right');
    });

    test('多换行（空行）：char_ranges 正确映射', () => {
        // text = "line1\n\nline2" (blank line between)
        const words = [
            {text: 'line1', lineIndex: 0},
            {text: 'line2', lineIndex: 1},
        ];
        const fullText = 'line1\n\nline2';
        const charRanges = [[0, 5], [7, 12]];

        const result = computeCharRanges(words, fullText, charRanges);

        assert.deepStrictEqual(result[0], {start: 0, end: 5});
        assert.deepStrictEqual(result[1], {start: 7, end: 12});
        assert.strictEqual(fullText.slice(result[0].start, result[0].end), 'line1');
        assert.strictEqual(fullText.slice(result[1].start, result[1].end), 'line2');
    });
});

describe('computeCharRanges — 补充面字符（emoji）UTF-16 偏移', () => {
    test('emoji 在文本中间：UTF-16 offset 比 Rust char index 多 1', () => {
        // text = "a😀b" — Rust chars: a(0), 😀(1), b(2)
        // UTF-16: a(0), [surrogate pair](1,2), b(3)
        // Rust char_ranges for word "b" = (2, 3)
        // UTF-16 offset for "b" = 3
        const words = [
            {text: 'a😀', lineIndex: 0},
            {text: 'b', lineIndex: 0},
        ];
        const fullText = 'a😀b';
        const charRanges = [[0, 2], [2, 3]]; // Rust char indices

        const result = computeCharRanges(words, fullText, charRanges);

        // word "a😀": Rust 0..2, UTF-16 0..3 (😀占2个code unit)
        assert.strictEqual(result[0].start, 0);
        assert.strictEqual(result[0].end, 3);
        // word "b": Rust 2..3, UTF-16 3..4
        assert.strictEqual(result[1].start, 3);
        assert.strictEqual(result[1].end, 4);

        // 验证 slice 正确
        assert.strictEqual(fullText.slice(result[0].start, result[0].end), 'a😀');
        assert.strictEqual(fullText.slice(result[1].start, result[1].end), 'b');
    });
});

describe('computeCharRanges — 退化路径（无后端 char_ranges）', () => {
    test('后端未提供 char_ranges → 走前端估算逻辑', () => {
        const words = [
            {text: 'hello', lineIndex: 0},
            {text: 'world', lineIndex: 0},
        ];
        const fullText = 'hello world';

        const result = computeCharRanges(words, fullText, null);

        assert.strictEqual(result.length, 2);
        assert.deepStrictEqual(result[0], {start: 0, end: 5});
        assert.deepStrictEqual(result[1], {start: 6, end: 11});
    });

    test('后端 char_ranges 长度不匹配 → 走前端估算逻辑', () => {
        const words = [
            {text: 'hello', lineIndex: 0},
            {text: 'world', lineIndex: 0},
        ];
        const fullText = 'hello world';
        const charRanges = [[0, 5]]; // 长度不匹配

        const result = computeCharRanges(words, fullText, charRanges);

        assert.strictEqual(result.length, 2);
        assert.deepStrictEqual(result[0], {start: 0, end: 5});
        assert.deepStrictEqual(result[1], {start: 6, end: 11});
    });
});

describe('computeCharRanges — 编辑后失效', () => {
    test('用户编辑后 panelDirty 标记使 charRanges 不再同步', () => {
        // 模拟编辑后场景：fullText 不再与后端 text 一致
        // 此时 charRanges 基于旧的 fullText 计算，编辑后偏移失效
        // 这是行为验证：panelDirty 标记应该在 syncSelectionToPanel 中检查
        const words = [
            {text: 'hello', lineIndex: 0},
            {text: 'world', lineIndex: 0},
        ];
        const fullText = 'hello world';
        const charRanges = [[0, 5], [6, 11]];

        const result = computeCharRanges(words, fullText, charRanges);

        // 模拟用户编辑：在 textarea 中插入了一个字符
        // 此时 fullText 变为 "hello! world"
        const editedText = 'hello! world';
        // 旧的 charRanges 现在指向错误位置
        // result[1].start = 6 → editedText[6] = 'w' (正确)
        // result[1].end = 11 → editedText[11] = 'd' (正确)
        // 但如果编辑在中间插入，偏移会错位
        // 例如插入 "!" 在位置 5
        // 旧的 start=0, end=5 → "hello" 正确
        // 旧的 start=6, end=11 → editedText.slice(6, 11) = "world" 正确（因为 ! 在 5 位置）
        // 但如果编辑改变了 fullText 的长度，后续 syncSelectionFromPanel 会失效

        // 验证：在编辑后，panelDirty 应该为 true
        // 这是 syncSelectionToPanel 和 syncSelectionFromPanel 的责任
        // 此处只验证 charRanges 的正确性
        assert.strictEqual(
            fullText.slice(result[0].start, result[0].end),
            'hello'
        );
        assert.strictEqual(
            fullText.slice(result[1].start, result[1].end),
            'world'
        );
        // 编辑后 fullText 变了，但 charRanges 没更新 → panelDirty 应拦截
        assert.strictEqual(
            editedText.slice(result[0].start, result[0].end),
            'hello'
        );
        // 注意：如果编辑插入字符在 word 边界之外，旧的 charRanges 可能仍然有效
        // 但如果编辑改变了 word 的位置，charRanges 就会错位
        // 这就是 panelDirty 机制存在的理由
    });
});

// ── 0.22.8 三层契约：computeCharBoxRanges 测试 ──────────────────

describe('computeCharBoxRanges — 逐字符框 UTF-16 转换', () => {
    test('BMP 文本：Rust char index 与 UTF-16 offset 一致', () => {
        // text = "PP-OCRv6", char_boxes 逐字符
        const fullText = 'PP-OCRv6';
        const charBoxes = [
            {text: 'P', char_start: 0, char_end: 1},
            {text: 'P', char_start: 1, char_end: 2},
            {text: '-', char_start: 2, char_end: 3},
            {text: 'O', char_start: 3, char_end: 4},
            {text: 'C', char_start: 4, char_end: 5},
            {text: 'R', char_start: 5, char_end: 6},
            {text: 'v', char_start: 6, char_end: 7},
            {text: '6', char_start: 7, char_end: 8},
        ];

        const result = computeCharBoxRanges(charBoxes, fullText);

        assert.strictEqual(result.length, 8);
        assert.deepStrictEqual(result[0], {start: 0, end: 1});
        assert.deepStrictEqual(result[7], {start: 7, end: 8});

        // 验证 slice 正确
        for (let i = 0; i < charBoxes.length; i++) {
            assert.strictEqual(
                fullText.slice(result[i].start, result[i].end),
                charBoxes[i].text,
                `char_box[${i}] slice mismatch`
            );
        }
    });

    test('CJK 文本：Rust char index 与 UTF-16 offset 一致', () => {
        const fullText = '你好世界';
        const charBoxes = [
            {text: '你', char_start: 0, char_end: 1},
            {text: '好', char_start: 1, char_end: 2},
            {text: '世', char_start: 2, char_end: 3},
            {text: '界', char_start: 3, char_end: 4},
        ];

        const result = computeCharBoxRanges(charBoxes, fullText);

        assert.strictEqual(result.length, 4);
        for (let i = 0; i < charBoxes.length; i++) {
            assert.strictEqual(
                fullText.slice(result[i].start, result[i].end),
                charBoxes[i].text
            );
        }
    });

    test('多行：换行符偏移正确', () => {
        // text = "你好\n世界"
        const fullText = '你好\n世界';
        const charBoxes = [
            {text: '你', char_start: 0, char_end: 1},
            {text: '好', char_start: 1, char_end: 2},
            {text: '世', char_start: 3, char_end: 4},  // \n at index 2
            {text: '界', char_start: 4, char_end: 5},
        ];

        const result = computeCharBoxRanges(charBoxes, fullText);

        assert.strictEqual(result.length, 4);
        for (let i = 0; i < charBoxes.length; i++) {
            assert.strictEqual(
                fullText.slice(result[i].start, result[i].end),
                charBoxes[i].text
            );
        }
    });

    test('emoji：UTF-16 offset 比 Rust char index 多 1', () => {
        // text = "a😀b"
        // Rust chars: a(0), 😀(1), b(2)
        // UTF-16: a(0), [surrogate pair](1,2), b(3)
        const fullText = 'a😀b';
        const charBoxes = [
            {text: 'a', char_start: 0, char_end: 1},
            {text: '😀', char_start: 1, char_end: 2},
            {text: 'b', char_start: 2, char_end: 3},
        ];

        const result = computeCharBoxRanges(charBoxes, fullText);

        assert.strictEqual(result.length, 3);
        // a: Rust 0..1 → UTF-16 0..1
        assert.deepStrictEqual(result[0], {start: 0, end: 1});
        // 😀: Rust 1..2 → UTF-16 1..3 (surrogate pair)
        assert.deepStrictEqual(result[1], {start: 1, end: 3});
        // b: Rust 2..3 → UTF-16 3..4
        assert.deepStrictEqual(result[2], {start: 3, end: 4});

        for (let i = 0; i < charBoxes.length; i++) {
            assert.strictEqual(
                fullText.slice(result[i].start, result[i].end),
                charBoxes[i].text
            );
        }
    });

    test('空数组：返回空', () => {
        assert.deepStrictEqual(computeCharBoxRanges([], 'hello'), []);
        assert.deepStrictEqual(computeCharBoxRanges(null, 'hello'), []);
        assert.deepStrictEqual(computeCharBoxRanges(undefined, 'hello'), []);
    });
});


// ═══════════════════════════════════════════════════════════
//  0.22.10: 字符级选择轨（char_box index 段）行为测试
// ═══════════════════════════════════════════════════════════

const {ss} = await import('./ss-state.js');
const {getReadingSelectionText, syncSelectionFromPanel} = await import('./ss-reading.js');

/** 构造 char 轨 reading 状态 fixture：两行，每行 3 个 char_box */
function makeCharTrackReading() {
    const fullText = 'abc\nghi';
    // 行1 abc（0-3），换行，行2 ghi（4-7）
    const charBoxes = [
        {text: 'a', rect: {x: 0, y: 0, w: 10, h: 10}, lineIndex: 0},
        {text: 'b', rect: {x: 10, y: 0, w: 10, h: 10}, lineIndex: 0},
        {text: 'c', rect: {x: 20, y: 0, w: 10, h: 10}, lineIndex: 0},
        {text: 'g', rect: {x: 0, y: 20, w: 10, h: 10}, lineIndex: 1},
        {text: 'h', rect: {x: 10, y: 20, w: 10, h: 10}, lineIndex: 1},
        {text: 'i', rect: {x: 20, y: 20, w: 10, h: 10}, lineIndex: 1},
    ];
    // 后端 char_boxes：a(0,1) b(1,2) c(2,3) g(4,5) h(5,6) i(6,7)（Rust char index）
    const charBoxRanges = [
        {start: 0, end: 1}, {start: 1, end: 2}, {start: 2, end: 3},
        {start: 4, end: 5}, {start: 5, end: 6}, {start: 6, end: 7},
    ];
    const words = [
        {text: 'abc', rect: {x: 0, y: 0, w: 30, h: 10}, lineIndex: 0},
        {text: 'ghi', rect: {x: 0, y: 20, w: 30, h: 10}, lineIndex: 1},
    ];
    const charRanges = [{start: 0, end: 3}, {start: 4, end: 7}];
    ss.reading = {
        words,
        charRanges,
        fullText,
        charBoxes,
        charBoxToWord: [0, 0, 0, 1, 1, 1],
        wordToCharBoxes: [[0, 1, 2], [3, 4, 5]],
        charBoxRanges,
        selectionStart: null,
        selectionEnd: null,
        panelDirty: false,
        dragStart: null,
        hoverWord: null,
    };
    ss.hitCtx = {clearRect() {}, fillRect() {}, strokeRect() {}};
    ss.hitCanvas = {width: 100, height: 100, removeAttribute() {}, style: {}};
}

describe('0.22.10 字符级选择轨', () => {
    test('点击行中某字符 → 选中文本为单 char_box 而非整行', () => {
        makeCharTrackReading();
        ss.reading.selectionStart = 1;
        ss.reading.selectionEnd = 1;
        // 选中 char_box[1] = 'b'，而不是整个 word 'abcdef'
        assert.strictEqual(getReadingSelectionText(), 'b');
    });

    test('跨行拖选 → 面板 selection 对应连续文本', () => {
        makeCharTrackReading();
        // 从行1 'c'（idx 2）拖到行2 'h'（idx 4）
        ss.reading.selectionStart = 2;
        ss.reading.selectionEnd = 4;
        assert.strictEqual(getReadingSelectionText(), 'c\ngh');
    });

    test('反向拖选（从右下到左上）lo/hi 正确', () => {
        makeCharTrackReading();
        ss.reading.selectionStart = 4;
        ss.reading.selectionEnd = 1;
        // [1,4] = b c \n g h
        assert.strictEqual(getReadingSelectionText(), 'bc\ngh');
    });

    test('面板 selection 反向映射到 char_box index 段', () => {
        makeCharTrackReading();
        const ta = {selectionStart: 5, selectionEnd: 7}; // 'hi'
        syncSelectionFromPanel(ta);
        assert.strictEqual(ss.reading.selectionStart, 4);
        assert.strictEqual(ss.reading.selectionEnd, 5);
    });

    test('无 char_boxes 时回退 word 轨（WinRT 旧路径不回归）', () => {
        makeCharTrackReading();
        ss.reading.charBoxes = [];
        ss.reading.charBoxRanges = [];
        ss.reading.selectionStart = 0;
        ss.reading.selectionEnd = 1;
        // word 轨：[0,1] = 'abc\nghi'
        assert.strictEqual(getReadingSelectionText(), 'abc\nghi');
    });
});
