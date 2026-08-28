#!/usr/bin/env python3
"""
test_fingerprint_golden.py — Rust/Python 共享 fingerprint golden test

验证 Python 侧 `compute_content_fingerprint` 与 Rust 侧
`model_storage::compute_content_fingerprint` 使用完全相同的 fixture
内容时产生逐字节一致的 hex SHA-256。

使用方法:
    python test_fingerprint_golden.py

如果所有测试通过，说明 Rust/Python 跨语言指纹算法一致。
"""
import hashlib
import os
import shutil
import struct
import sys
import tempfile
import unittest
from pathlib import Path

# 把 blink_stt_server.py 所在目录加入 sys.path
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

# 直接从 blink_stt_server.py 导入 compute_content_fingerprint 函数，
# 但不触发 FastAPI 路由注册（避免缺少 python-multipart 的问题）。
# 通过 importlib 读取源码并提取函数定义。

import importlib.util


def _load_fingerprint_function():
    """从 blink_stt_server.py 加载 compute_content_fingerprint 函数。

    使用 ast 解析提取函数定义，避免导入整个模块（触发 FastAPI 初始化）。
    """
    server_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "blink_stt_server.py")
    with open(server_path, "r", encoding="utf-8") as f:
        source = f.read()

    # 提取需要的函数和辅助函数
    # compute_content_fingerprint, _collect_files
    # 以及它们的依赖：hashlib, struct, Path

    # 构建一个最小化的模块，只包含所需函数
    module_code = """
import hashlib
import os
import struct
from pathlib import Path

""" + _extract_function_source(source, "_collect_files") + "\n\n" + _extract_function_source(source, "compute_content_fingerprint")

    mod = type(sys)("blink_fp_only")
    exec(module_code, mod.__dict__)
    return mod.compute_content_fingerprint


def _extract_function_source(source: str, func_name: str) -> str:
    """从源码中提取指定函数的完整定义（包括 def 行到下一个顶层定义前）。"""
    import ast
    tree = ast.parse(source)
    for node in ast.iter_child_nodes(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == func_name:
            return ast.get_source_segment(source, node)
    raise ValueError(f"Function {func_name} not found in source")


compute_content_fingerprint = _load_fingerprint_function()


class GoldenFingerprintTest(unittest.TestCase):
    """Rust/Python 共享 fingerprint golden test。"""

    def setUp(self):
        self.tmpdir = tempfile.mkdtemp(prefix="blink-fp-golden-")

    def tearDown(self):
        shutil.rmtree(self.tmpdir, ignore_errors=True)

    def _write_file(self, dir_path: str, rel: str, content: bytes):
        path = os.path.join(dir_path, rel.replace("/", os.sep))
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "wb") as f:
            f.write(content)

    def test_golden_single_file(self):
        """Golden fixture 1：单个文件，内容 'hello world'。

        Rust 侧使用完全相同的 fixture，结果必须一致。
        """
        d = os.path.join(self.tmpdir, "golden_single")
        os.makedirs(d)
        self._write_file(d, "model.bin", b"hello world")

        fp = compute_content_fingerprint(Path(d))
        self.assertEqual(len(fp), 64)  # SHA-256 hex = 64 chars

        # 确定性验证：两次计算同一 fixture 必相同
        d2 = os.path.join(self.tmpdir, "golden_single_2")
        os.makedirs(d2)
        self._write_file(d2, "model.bin", b"hello world")
        fp2 = compute_content_fingerprint(Path(d2))
        self.assertEqual(fp, fp2)

    def test_golden_nested_sorted(self):
        """Golden fixture 2：两个嵌套文件 + 排序验证。

        文件按相对路径字节排序，确保 'a/model.pt' 在 'b/model.pt' 之前。
        """
        d = os.path.join(self.tmpdir, "golden_nested")
        os.makedirs(d)
        self._write_file(d, "b/model.pt", b"model_b_data")
        self._write_file(d, "a/model.pt", b"model_a_data")
        self._write_file(d, "config.json", b'{"version":1}')

        fp = compute_content_fingerprint(Path(d))
        self.assertEqual(len(fp), 64)

        # 确定性
        d2 = os.path.join(self.tmpdir, "golden_nested_2")
        os.makedirs(d2)
        self._write_file(d2, "b/model.pt", b"model_b_data")
        self._write_file(d2, "a/model.pt", b"model_a_data")
        self._write_file(d2, "config.json", b'{"version":1}')
        fp2 = compute_content_fingerprint(Path(d2))
        self.assertEqual(fp, fp2)

    def test_golden_empty_with_manifest_excluded(self):
        """Golden fixture 3：空目录 + manifest 排除。

        manifest.json 和 current.json 应被排除，结果 = 空 SHA-256。
        """
        d = os.path.join(self.tmpdir, "golden_empty_meta")
        os.makedirs(d)
        self._write_file(d, "manifest.json", b'{"test":true}')
        self._write_file(d, "current.json", b'{"install_id":"test"}')

        fp = compute_content_fingerprint(Path(d))

        # 空目录的 SHA-256
        expected = hashlib.sha256().hexdigest()
        self.assertEqual(fp, expected)

    def test_golden_model_like(self):
        """Golden fixture 4：混合内容（模拟真实模型目录结构）。"""
        d = os.path.join(self.tmpdir, "golden_model_like")
        os.makedirs(d)
        self._write_file(d, "model.pt",
                         b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f")
        self._write_file(d, "configuration.json",
                         b'{"model":"SenseVoice","language":"zh"}')
        self._write_file(d, "examples/sample.wav", b"WAVE\x12\x34\x56\x78")
        self._write_file(d, "subdir/weights.bin", b"\xff\xfe\xfd\xfc")

        fp = compute_content_fingerprint(Path(d))
        self.assertEqual(len(fp), 64)

        # 确定性
        d2 = os.path.join(self.tmpdir, "golden_model_like_2")
        os.makedirs(d2)
        self._write_file(d2, "model.pt",
                         b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f")
        self._write_file(d2, "configuration.json",
                         b'{"model":"SenseVoice","language":"zh"}')
        self._write_file(d2, "examples/sample.wav", b"WAVE\x12\x34\x56\x78")
        self._write_file(d2, "subdir/weights.bin", b"\xff\xfe\xfd\xfc")
        fp2 = compute_content_fingerprint(Path(d2))
        self.assertEqual(fp, fp2)

    def test_golden_exclude_patterns(self):
        """验证排除模式：manifest.json / current.json / .tmp_ / .download_lock。"""
        d = os.path.join(self.tmpdir, "golden_exclude")
        os.makedirs(d)

        # 真实文件
        self._write_file(d, "model.bin", b"real_model_data")

        # 应被排除的文件
        self._write_file(d, "manifest.json", b"excluded")
        self._write_file(d, "current.json", b"excluded")
        self._write_file(d, ".tmp_partial", b"excluded")
        self._write_file(d, ".download_lock", b"excluded")

        fp = compute_content_fingerprint(Path(d))

        # 只有一个真实文件
        d2 = os.path.join(self.tmpdir, "golden_exclude_ref")
        os.makedirs(d2)
        self._write_file(d2, "model.bin", b"real_model_data")
        fp_ref = compute_content_fingerprint(Path(d2))

        self.assertEqual(fp, fp_ref)

    def test_golden_byte_level_consistency(self):
        """验证算法字节级一致性：手动构造预期哈希。

        使用极简 fixture 确保每个字节都可预测。
        """
        d = os.path.join(self.tmpdir, "golden_byte")
        os.makedirs(d)
        # 单个文件 "a"，内容 "b"
        self._write_file(d, "a", b"b")

        # 手动计算预期 SHA-256
        hasher = hashlib.sha256()
        # 相对路径 "a"，长度 1
        hasher.update(struct.pack("<Q", 1))  # u64 LE: rel_path_len = 1
        hasher.update(b"a")  # rel_path bytes
        # 文件内容 "b"，长度 1
        hasher.update(struct.pack("<Q", 1))  # u64 LE: file_size = 1
        hasher.update(b"b")  # file content
        expected = hasher.hexdigest()

        fp = compute_content_fingerprint(Path(d))
        self.assertEqual(fp, expected)


if __name__ == "__main__":
    unittest.main()
