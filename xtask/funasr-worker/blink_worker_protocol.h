// blink_worker_protocol.h — Blink 常驻 GGUF worker 的共享 NDJSON stdin/stdout 协议（v1）。
//
// 由 xtask 构建时复制到 FunASR runtime/llama.cpp/funasr-common/ 下，
// 供 llama-funasr-sensevoice / llama-funasr-paraformer / llama-funasr-cli 三个
// 入口的 --stdin-server 补丁共同引用。模型加载、fbank、计算图与解码保持上游原样；
// 本头文件只承担进程协议（ready 握手、请求循环、身份与指纹回报）。
//
// ## 协议（frozen v1）
//
// 通道铁则：
// - stdout 只输出机器协议，每行一个完整 JSON 对象，UTF-8，行缓冲 flush。
// - 一切诊断/加载日志写 stderr。
// - 身份与模型信息从环境变量读取（不由 argv 提供可执行内容）：
//     BLINK_ENGINE_ID, BLINK_INSTANCE_ID, BLINK_ENGINE_TOKEN,
//     BLINK_MODEL_ID, BLINK_MODEL_REVISION, BLINK_MODEL_PAYLOAD_DIR,
//     BLINK_AUDIO_DIR（允许读取的音频目录前缀）， BLINK_WORKER_THREADS（可选）
//
// 启动：模型与 backend 实际加载完成后（且仅在完成后）输出 ready：
//   {"type":"ready","protocol_version":1,"engine_id":...,"instance_id":...,
//    "token_fingerprint":"fp:xxxxxxxxxxxxxxxx","model_id":...,"model_revision":...,
//    "model_status":"ready","model_content_fingerprint":"<64 hex>",
//    "backend":"cpu","requested_backend":"cpu"}
// ready 中的 model_content_fingerprint 是 worker 亲自对 payload 目录计算的
// directory_aggregate_sha256_v1（与 Blink 安装侧算法一致，见下）。
//
// 请求（stdin，每行一条 JSON）：
//   {"type":"hello","protocol_version":1}
//   {"type":"transcribe","request_id":"...","audio_path":"...","language":"zh"?,"use_itn":true?}
//   {"type":"shutdown"}
//
// 响应（stdout）：
//   {"type":"hello_ok","protocol_version":1,...ready 字段...}
//   {"type":"transcribe_result","request_id":"...","ok":true,"text":"...","elapsed_ms":123}
//   {"type":"transcribe_result","request_id":"...","ok":false,
//    "error":{"code":"...","message":"..."},"elapsed_ms":123}
//   {"type":"error","request_id":"...或null","error":{"code":"...","message":"..."}}
//
// 错误码：bad_json / unknown_type / unsupported_protocol_version /
//         audio_path_rejected / audio_read_failed / inference_failed。
// 畸形 JSON、未知 type、版本不匹配必须返回结构化 error，进程不退出。
//
// 停止：stdin EOF 或 shutdown 请求 → 正常退出（exit 0）。
//
// ## directory_aggregate_sha256_v1（与 src/infra/local_engine/model_storage.rs 一致）
//
// 1. 递归枚举 payload 下普通文件（排除 manifest.json / active.json / .tmp_* /
//    .download_lock）。
// 2. 相对路径用 '/' 分隔；按相对路径字节排序。
// 3. 依次哈希：路径长度 u64-LE、路径字节、文件大小 u64-LE、文件内容。
// 4. 输出小写 64 位 hex。
//
// 许可：本文件为 Blink 仓库自有代码（MIT，随仓库 LICENSE 分发）。

#ifndef BLINK_WORKER_PROTOCOL_H
#define BLINK_WORKER_PROTOCOL_H

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <filesystem>
#include <fstream>
#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

namespace blink_worker {

// ── SHA-256（紧凑公有域式实现）──────────────────────────────────────────

class Sha256 {
public:
    Sha256() { reset(); }
    void reset() {
        datalen_ = 0;
        bitlen_ = 0;
        static const uint32_t K[64] = {
            0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,
            0x923f82a4,0xab1c5ed5,0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,
            0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,0xe49b69c1,0xefbe4786,
            0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
            0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,
            0x06ca6351,0x14292967,0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,
            0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,
            0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
            0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,
            0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,
            0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
        state_[0]=0x6a09e667; state_[1]=0xbb67ae85; state_[2]=0x3c6ef372;
        state_[3]=0xa54ff53a; state_[4]=0x510e527f; state_[5]=0x9b05688c;
        state_[6]=0x1f83d9ab; state_[7]=0x5be0cd19;
    }
    void update(const void* data, size_t len) {
        const uint8_t* p = static_cast<const uint8_t*>(data);
        for (size_t i = 0; i < len; ++i) {
            data_[datalen_] = p[i];
            datalen_++;
            if (datalen_ == 64) { transform(); bitlen_ += 512; datalen_ = 0; }
        }
    }
    void update(uint8_t b) { update(&b, 1); }
    void final(uint8_t out[32]) {
        size_t i = datalen_;
        if (i < 56) {
            data_[i++] = 0x80;
            while (i < 56) data_[i++] = 0x00;
        } else {
            data_[i++] = 0x80;
            while (i < 64) data_[i++] = 0x00;
            transform();
            bitlen_ += 512;
            for (i = 0; i < 56; ++i) data_[i] = 0x00;
        }
        bitlen_ += datalen_ * 8;
        data_[63] = (uint8_t)(bitlen_);
        data_[62] = (uint8_t)(bitlen_ >> 8);
        data_[61] = (uint8_t)(bitlen_ >> 16);
        data_[60] = (uint8_t)(bitlen_ >> 24);
        data_[59] = (uint8_t)(bitlen_ >> 32);
        data_[58] = (uint8_t)(bitlen_ >> 40);
        data_[57] = (uint8_t)(bitlen_ >> 48);
        data_[56] = (uint8_t)(bitlen_ >> 56);
        transform();
        for (int i = 0; i < 8; ++i) {
            for (int j = 0; j < 4; ++j) {
                out[i * 4 + j] = (uint8_t)((state_[i] >> (24 - j * 8)) & 0xff);
            }
        }
    }
private:
    void transform() {
        uint32_t m[64], a, b, c, d, e, f, g, h, t1, t2;
        static const uint32_t K[64] = {
            0x428a2f98,0x71374491,0xb5c0fbcf,0xe9b5dba5,0x3956c25b,0x59f111f1,
            0x923f82a4,0xab1c5ed5,0xd807aa98,0x12835b01,0x243185be,0x550c7dc3,
            0x72be5d74,0x80deb1fe,0x9bdc06a7,0xc19bf174,0xe49b69c1,0xefbe4786,
            0x0fc19dc6,0x240ca1cc,0x2de92c6f,0x4a7484aa,0x5cb0a9dc,0x76f988da,
            0x983e5152,0xa831c66d,0xb00327c8,0xbf597fc7,0xc6e00bf3,0xd5a79147,
            0x06ca6351,0x14292967,0x27b70a85,0x2e1b2138,0x4d2c6dfc,0x53380d13,
            0x650a7354,0x766a0abb,0x81c2c92e,0x92722c85,0xa2bfe8a1,0xa81a664b,
            0xc24b8b70,0xc76c51a3,0xd192e819,0xd6990624,0xf40e3585,0x106aa070,
            0x19a4c116,0x1e376c08,0x2748774c,0x34b0bcb5,0x391c0cb3,0x4ed8aa4a,
            0x5b9cca4f,0x682e6ff3,0x748f82ee,0x78a5636f,0x84c87814,0x8cc70208,
            0x90befffa,0xa4506ceb,0xbef9a3f7,0xc67178f2};
        for (int i = 0; i < 16; ++i)
            m[i] = ((uint32_t)data_[i*4] << 24) | ((uint32_t)data_[i*4+1] << 16) |
                   ((uint32_t)data_[i*4+2] << 8) | (uint32_t)data_[i*4+3];
        for (int i = 16; i < 64; ++i) {
            uint32_t s0 = rotr(m[i-15],7) ^ rotr(m[i-15],18) ^ (m[i-15] >> 3);
            uint32_t s1 = rotr(m[i-2],17) ^ rotr(m[i-2],19) ^ (m[i-2] >> 10);
            m[i] = m[i-16] + s0 + m[i-7] + s1;
        }
        a=state_[0];b=state_[1];c=state_[2];d=state_[3];
        e=state_[4];f=state_[5];g=state_[6];h=state_[7];
        for (int i = 0; i < 64; ++i) {
            uint32_t S1 = rotr(e,6) ^ rotr(e,11) ^ rotr(e,25);
            uint32_t ch = (e & f) ^ (~e & g);
            t1 = h + S1 + ch + K[i] + m[i];
            uint32_t S0 = rotr(a,2) ^ rotr(a,13) ^ rotr(a,22);
            uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
            t2 = S0 + maj;
            h=g; g=f; f=e; e=d+t1; d=c; c=b; b=a; a=t1+t2;
        }
        state_[0]+=a;state_[1]+=b;state_[2]+=c;state_[3]+=d;
        state_[4]+=e;state_[5]+=f;state_[6]+=g;state_[7]+=h;
        (void)K;
    }
    static uint32_t rotr(uint32_t x, uint32_t n) { return (x >> n) | (x << (32 - n)); }
    uint8_t data_[64];
    uint32_t state_[8];
    size_t datalen_;
    uint64_t bitlen_;
};

inline std::string sha256_hex(const void* data, size_t len) {
    Sha256 h;
    h.update(data, len);
    uint8_t out[32];
    h.final(out);
    char hex[65];
    for (int i = 0; i < 32; ++i) sprintf(hex + i * 2, "%02x", out[i]);
    hex[64] = 0;
    return std::string(hex);
}

// ── token fingerprint（与 src/infra/local_engine/port.rs::token_fingerprint 一致）──

inline std::string token_fingerprint(const std::string& token) {
    Sha256 h;
    h.update(token.data(), token.size());
    uint8_t out[32];
    h.final(out);
    char hex[17];
    for (int i = 0; i < 8; ++i) sprintf(hex + i * 2, "%02x", out[i]);
    hex[16] = 0;
    return std::string("fp:") + hex;
}

// ── directory_aggregate_sha256_v1（与 model_storage.rs::compute_content_fingerprint 一致）──

inline void collect_files(const std::filesystem::path& root,
                          const std::filesystem::path& current,
                          std::vector<std::string>& rel_paths) {
    namespace fs = std::filesystem;
    std::error_code ec;
    for (fs::directory_iterator it(current, ec), end; it != end && !ec; it.increment(ec)) {
        const std::string name = it->path().filename().string();
        if (name == "manifest.json" || name == "active.json" ||
            name.rfind(".tmp_", 0) == 0 || name == ".download_lock") {
            continue;
        }
        if (it->is_directory()) {
            collect_files(root, it->path(), rel_paths);
        } else if (it->is_regular_file()) {
            std::string rel = fs::relative(it->path(), root, ec).generic_string();
            if (!ec) rel_paths.push_back(rel);
        }
    }
}

inline std::string dir_content_fingerprint(const std::string& dir) {
    namespace fs = std::filesystem;
    std::vector<std::string> rel_paths;
    std::error_code ec;
    fs::path root(dir);
    if (!fs::is_directory(root, ec)) return std::string();
    collect_files(root, root, rel_paths);
    std::sort(rel_paths.begin(), rel_paths.end(),
              [](const std::string& a, const std::string& b) {
                  return a < b;  // std::string 的 operator< 即按字节序
              });
    Sha256 h;
    // 堆缓冲：1MB 栈数组会溢出 Windows 默认 1MB 栈（MSVC 序言即分配全帧，
    // 曾触发 0xC000040D fail-fast）。
    std::vector<char> buf(1 << 20);
    for (const std::string& rel : rel_paths) {
        uint64_t rel_len = rel.size();
        uint8_t le8[8];
        for (int i = 0; i < 8; ++i) le8[i] = (uint8_t)(rel_len >> (i * 8));
        h.update(le8, 8);
        h.update(rel.data(), rel.size());
        std::ifstream f(root / rel, std::ios::binary);
        if (!f) return std::string();
        f.seekg(0, std::ios::end);
        uint64_t size = (uint64_t)f.tellg();
        for (int i = 0; i < 8; ++i) le8[i] = (uint8_t)(size >> (i * 8));
        h.update(le8, 8);
        f.seekg(0, std::ios::beg);
        while (f) {
            f.read(buf.data(), (std::streamsize)buf.size());
            std::streamsize n = f.gcount();
            if (n > 0) h.update(buf.data(), (size_t)n);
        }
    }
    uint8_t out[32];
    h.final(out);
    char hex[65];
    for (int i = 0; i < 32; ++i) sprintf(hex + i * 2, "%02x", out[i]);
    hex[64] = 0;
    return std::string(hex);
}

// ── JSON 输出辅助 ──────────────────────────────────────────────────────

inline std::string json_escape(const std::string& s) {
    std::string out;
    out.reserve(s.size() + 8);
    for (unsigned char c : s) {
        switch (c) {
            case '"': out += "\\\""; break;
            case '\\': out += "\\\\"; break;
            case '\b': out += "\\b"; break;
            case '\f': out += "\\f"; break;
            case '\n': out += "\\n"; break;
            case '\r': out += "\\r"; break;
            case '\t': out += "\\t"; break;
            default:
                if (c < 0x20) {
                    char b[8];
                    sprintf(b, "\\u%04x", c);
                    out += b;
                } else {
                    out += (char)c;
                }
        }
    }
    return out;
}

// ── JSON 最小字段提取 ──────────────────────────────────────────────────
// 输入来自 Blink 的 Rust client（格式受控），这里做保守扫描：
// 找到 "key" 后跳过空白与 ':'，期待字符串字面量并处理转义。
// 未找到/形态不符返回 found=false。

inline bool json_get_string(const std::string& body, const char* key,
                            std::string& value, bool& found) {
    found = false;
    std::string needle = std::string("\"") + key + "\"";
    size_t pos = 0;
    while ((pos = body.find(needle, pos)) != std::string::npos) {
        size_t p = pos + needle.size();
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        if (p >= body.size() || body[p] != ':') { pos += needle.size(); continue; }
        p++;
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        if (p >= body.size() || body[p] != '"') { pos += needle.size(); continue; }
        p++;
        std::string out;
        bool ok = true;
        while (p < body.size()) {
            char c = body[p];
            if (c == '"') break;
            if (c == '\\') {
                if (p + 1 >= body.size()) { ok = false; break; }
                char e = body[p + 1];
                p += 2;
                switch (e) {
                    case '"': out += '"'; break;
                    case '\\': out += '\\'; break;
                    case '/': out += '/'; break;
                    case 'b': out += '\b'; break;
                    case 'f': out += '\f'; break;
                    case 'n': out += '\n'; break;
                    case 'r': out += '\r'; break;
                    case 't': out += '\t'; break;
                    case 'u': {
                        if (p + 4 > body.size()) { ok = false; break; }
                        unsigned int cp = 0;
                        for (int i = 0; i < 4; ++i) {
                            char h = body[p + i];
                            cp <<= 4;
                            if (h >= '0' && h <= '9') cp |= (unsigned)(h - '0');
                            else if (h >= 'a' && h <= 'f') cp |= (unsigned)(h - 'a' + 10);
                            else if (h >= 'A' && h <= 'F') cp |= (unsigned)(h - 'A' + 10);
                            else { ok = false; break; }
                        }
                        if (!ok) break;
                        p += 4;
                        // 仅编码 BMP 码点；代理对按 UTF-8 组合处理
                        if (cp >= 0xD800 && cp <= 0xDBFF && p + 6 <= body.size() &&
                            body[p] == '\\' && body[p + 1] == 'u') {
                            unsigned int lo = 0;
                            bool lo_ok = true;
                            for (int i = 0; i < 4; ++i) {
                                char h2 = body[p + 2 + i];
                                lo <<= 4;
                                if (h2 >= '0' && h2 <= '9') lo |= (unsigned)(h2 - '0');
                                else if (h2 >= 'a' && h2 <= 'f') lo |= (unsigned)(h2 - 'a' + 10);
                                else if (h2 >= 'A' && h2 <= 'F') lo |= (unsigned)(h2 - 'A' + 10);
                                else { lo_ok = false; break; }
                            }
                            if (lo_ok && lo >= 0xDC00 && lo <= 0xDFFF) {
                                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                p += 6;
                            }
                        }
                        if (cp < 0x80) out += (char)cp;
                        else if (cp < 0x800) {
                            out += (char)(0xC0 | (cp >> 6));
                            out += (char)(0x80 | (cp & 0x3F));
                        } else if (cp < 0x10000) {
                            out += (char)(0xE0 | (cp >> 12));
                            out += (char)(0x80 | ((cp >> 6) & 0x3F));
                            out += (char)(0x80 | (cp & 0x3F));
                        } else {
                            out += (char)(0xF0 | (cp >> 18));
                            out += (char)(0x80 | ((cp >> 12) & 0x3F));
                            out += (char)(0x80 | ((cp >> 6) & 0x3F));
                            out += (char)(0x80 | (cp & 0x3F));
                        }
                        break;
                    }
                    default: ok = false; break;
                }
                if (!ok) break;
                continue;
            }
            if ((unsigned char)c < 0x20) { ok = false; break; }
            out += c;
            p++;
        }
        if (ok && p <= body.size()) {
            value = out;
            found = true;
            return true;
        }
        pos += needle.size();
    }
    return true;
}

inline bool json_get_bool(const std::string& body, const char* key,
                          bool& value, bool& found) {
    found = false;
    std::string needle = std::string("\"") + key + "\"";
    size_t pos = 0;
    while ((pos = body.find(needle, pos)) != std::string::npos) {
        size_t p = pos + needle.size();
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        if (p >= body.size() || body[p] != ':') { pos += needle.size(); continue; }
        p++;
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        if (body.compare(p, 4, "true") == 0) { value = true; found = true; return true; }
        if (body.compare(p, 5, "false") == 0) { value = false; found = true; return true; }
        pos += needle.size();
    }
    return true;
}

inline bool json_get_u32(const std::string& body, const char* key,
                         uint32_t& value, bool& found) {
    found = false;
    std::string needle = std::string("\"") + key + "\"";
    size_t pos = 0;
    while ((pos = body.find(needle, pos)) != std::string::npos) {
        size_t p = pos + needle.size();
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        if (p >= body.size() || body[p] != ':') { pos += needle.size(); continue; }
        p++;
        while (p < body.size() && (body[p] == ' ' || body[p] == '\t')) p++;
        size_t start = p;
        while (p < body.size() && body[p] >= '0' && body[p] <= '9') p++;
        if (p > start) {
            value = (uint32_t)strtoul(body.substr(start, p - start).c_str(), nullptr, 10);
            found = true;
            return true;
        }
        pos += needle.size();
    }
    return true;
}

// 粗校验：一行是否像一个 JSON 对象（首尾大括号配对 + 引号总数为偶数）。
// 不是完整 parser，但足以把明显的非 JSON 输入挡在业务处理之外。
inline bool looks_like_json_object(const std::string& line) {
    size_t first = line.find_first_not_of(" \t\r");
    if (first == std::string::npos || line[first] != '{') return false;
    size_t last = line.find_last_not_of(" \t\r");
    if (last == std::string::npos || line[last] != '}') return false;
    size_t quotes = 0;
    bool in_str = false, esc = false;
    for (char c : line) {
        if (in_str) {
            if (esc) esc = false;
            else if (c == '\\') esc = true;
            else if (c == '"') { in_str = false; quotes++; }
        } else if (c == '"') {
            in_str = true;
            quotes++;
        }
    }
    return !in_str && (quotes % 2 == 0);
}

// ── 环境读取 ───────────────────────────────────────────────────────────

inline std::string env_str(const char* key, const std::string& fallback = std::string()) {
    const char* v = getenv(key);
    return (v && *v) ? std::string(v) : fallback;
}

inline int env_threads(int fallback) {
    const char* v = getenv("BLINK_WORKER_THREADS");
    if (!v || !*v) return fallback;
    int n = atoi(v);
    return n > 0 ? n : fallback;
}

// ── 协议主循环 ─────────────────────────────────────────────────────────
//
// run_segment: (const std::string& wav_path) -> std::string
//   读取 wav → 推理 → 返回文本。抛异常或返回空串由调用方语义决定：
//   本循环把异常归为 inference_failed，把空文本视为正常空结果（ok=true）。
//
// model_ready: 模型与 backend 已在调用前加载完毕（ready 只能在此之后输出）。
// backend_name: 实际执行后端（当前为 "cpu"）。

template <typename RunSegment>
int serve_stdin(RunSegment run_segment, const std::string& backend_name,
                int64_t (*time_us)()) {
    const std::string engine_id = env_str("BLINK_ENGINE_ID");
    const std::string instance_id = env_str("BLINK_INSTANCE_ID");
    const std::string token = env_str("BLINK_ENGINE_TOKEN");
    const std::string model_id = env_str("BLINK_MODEL_ID");
    const std::string model_revision = env_str("BLINK_MODEL_REVISION");
    const std::string payload_dir = env_str("BLINK_MODEL_PAYLOAD_DIR");
    const std::string audio_dir = env_str("BLINK_AUDIO_DIR");

    const std::string fp = dir_content_fingerprint(payload_dir);
    const std::string tfp = token_fingerprint(token);

    // ready：身份 + 模型指纹 + 实际 backend。仅在模型加载完成后才到达这里。
    {
        std::string line = "{\"type\":\"ready\",\"protocol_version\":1";
        line += ",\"engine_id\":\"" + json_escape(engine_id) + "\"";
        line += ",\"instance_id\":\"" + json_escape(instance_id) + "\"";
        line += ",\"token_fingerprint\":\"" + json_escape(tfp) + "\"";
        line += ",\"model_id\":\"" + json_escape(model_id) + "\"";
        line += ",\"model_revision\":\"" + json_escape(model_revision) + "\"";
        line += ",\"model_status\":\"ready\"";
        line += ",\"model_content_fingerprint\":\"" + json_escape(fp) + "\"";
        line += ",\"backend\":\"" + json_escape(backend_name) + "\"";
        line += ",\"requested_backend\":\"" + json_escape(backend_name) + "\"";
        line += "}\n";
        fwrite(line.data(), 1, line.size(), stdout);
        fflush(stdout);
    }

    auto emit_error = [&](const std::string& request_id, bool has_id,
                          const char* code, const std::string& message) {
        std::string line = "{\"type\":\"error\",\"request_id\":";
        line += has_id ? ("\"" + json_escape(request_id) + "\"") : std::string("null");
        line += ",\"error\":{\"code\":\"" + std::string(code) + "\"";
        line += ",\"message\":\"" + json_escape(message) + "\"}}\n";
        fwrite(line.data(), 1, line.size(), stdout);
        fflush(stdout);
    };

    auto emit_result = [&](const std::string& request_id, bool ok,
                           const std::string& text_or_message, double elapsed_ms) {
        char head[128];
        sprintf(head, "{\"type\":\"transcribe_result\",\"request_id\":\"%s\",\"ok\":%s",
                json_escape(request_id).c_str(), ok ? "true" : "false");
        std::string line = head;
        if (ok) {
            line += ",\"text\":\"" + json_escape(text_or_message) + "\"";
        } else {
            line += ",\"error\":{\"code\":\"inference_failed\",\"message\":\"" +
                    json_escape(text_or_message) + "\"}";
        }
        char tail[64];
        sprintf(tail, ",\"elapsed_ms\":%.1f}\n", elapsed_ms);
        line += tail;
        fwrite(line.data(), 1, line.size(), stdout);
        fflush(stdout);
    };

    // 音频目录前缀约束：请求路径（weakly canonical 后）必须位于 audio_dir 之下。
    namespace fs = std::filesystem;
    std::error_code ec;
    fs::path audio_root;
    bool audio_root_valid = false;
    if (!audio_dir.empty()) {
        audio_root = fs::weakly_canonical(fs::path(audio_dir), ec);
        audio_root_valid = !ec;
    }

    std::string req;
    while (std::getline(std::cin, req)) {
        if (!req.empty() && req.back() == '\r') req.pop_back();
        if (req.empty()) continue;
        if (!looks_like_json_object(req)) {
            emit_error("", false, "bad_json", "request line is not a JSON object");
            continue;
        }
        std::string type, request_id, audio_path, language;
        bool has_type = false, has_id = false, has_path = false;
        json_get_string(req, "type", type, has_type);
        json_get_string(req, "request_id", request_id, has_id);
        if (!has_type) {
            emit_error(request_id, has_id, "bad_json", "missing type field");
            continue;
        }
        if (type == "shutdown") {
            fprintf(stderr, "[blink-worker] shutdown requested\n");
            return 0;
        }
        if (type == "hello") {
            uint32_t client_version = 0;
            bool has_version = false;
            json_get_u32(req, "protocol_version", client_version, has_version);
            if (!has_version || client_version != 1) {
                emit_error(request_id, has_id, "unsupported_protocol_version",
                           "this worker speaks protocol_version 1");
                continue;
            }
            std::string line = "{\"type\":\"hello_ok\",\"protocol_version\":1";
            line += ",\"engine_id\":\"" + json_escape(engine_id) + "\"";
            line += ",\"instance_id\":\"" + json_escape(instance_id) + "\"";
            line += ",\"model_id\":\"" + json_escape(model_id) + "\"";
            line += ",\"backend\":\"" + json_escape(backend_name) + "\"}\n";
            fwrite(line.data(), 1, line.size(), stdout);
            fflush(stdout);
            continue;
        }
        if (type == "transcribe") {
            json_get_string(req, "audio_path", audio_path, has_path);
            if (!has_path || audio_path.empty()) {
                emit_error(request_id, has_id, "bad_json", "missing audio_path");
                continue;
            }
            // 路径约束：canonicalize 后必须位于 Blink 管理的音频目录内
            fs::path canon = fs::weakly_canonical(fs::path(audio_path), ec);
            if (ec || !audio_root_valid ||
                canon.native().rfind(audio_root.native(), 0) != 0) {
                emit_error(request_id, has_id, "audio_path_rejected",
                           "audio_path outside BLINK_AUDIO_DIR: " + audio_path);
                continue;
            }
            int64_t t0 = time_us();
            bool ok = true;
            std::string text;
            try {
                text = run_segment(audio_path);
            } catch (const std::exception& e) {
                ok = false;
                text = e.what();
            } catch (...) {
                ok = false;
                text = "unknown inference failure";
            }
            double elapsed_ms = (double)(time_us() - t0) / 1000.0;
            emit_result(request_id, ok, text, elapsed_ms);
            continue;
        }
        emit_error(request_id, has_id, "unknown_type", "unsupported type: " + type);
    }
    // stdin EOF：正常退出
    fprintf(stderr, "[blink-worker] stdin EOF, exiting\n");
    return 0;
}

}  // namespace blink_worker

#endif  // BLINK_WORKER_PROTOCOL_H
