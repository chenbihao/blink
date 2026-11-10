# ── 构建阶段：自包含，材料全部 build 时自动下载 ──
# 默认全官方源（适合可直连外网的环境）；
# 国内环境加参数：
#   --build-arg APT_MIRROR=http://mirrors.tuna.tsinghua.edu.cn/ubuntu
#   --build-arg HF_BASE=https://hf-mirror.com
#   --build-arg GH_PROXY=https://ghproxy.com/   （仅 github clone 不通时）
FROM buildpack-deps:noble AS builder

ARG FUNASR_COMMIT=b054623cca8f015b73ec471dce4f473ac47413da
ARG GH_PROXY=
ARG HF_BASE=https://huggingface.co
ARG APT_MIRROR=

# CMake：apt 安装（cmake 在 main 组件，仅 4-5 个小包）。
# APT_MIRROR 非空时才换源（deb822 格式；国内镜像用 http 避免证书鸡生蛋问题）
RUN if [ -n "$APT_MIRROR" ]; then \
        sed -i 's|^Components:.*|Components: main|' /etc/apt/sources.list.d/ubuntu.sources \
        && sed -i "s|http://archive.ubuntu.com/ubuntu/|${APT_MIRROR}/|g; s|http://security.ubuntu.com/ubuntu/|${APT_MIRROR}/|g" /etc/apt/sources.list.d/ubuntu.sources; \
    fi \
    && apt-get -o Acquire::Retries=8 update \
    && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*

# 源码：容器内直接 clone github.com（实测容器内可达）
RUN git clone ${GH_PROXY}https://github.com/QwenAudio/SenseVoice /src \
    && git -C /src checkout ${FUNASR_COMMIT}

# 编译单 target：sensevoice-server（configure 阶段会经 FetchContent 再拉 llama.cpp，同走 github.com）
WORKDIR /src
RUN cmake -S runtime/llama.cpp -B runtime/llama.cpp/build \
      -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=OFF \
    && cmake --build runtime/llama.cpp/build --target sensevoice-server -j"$(nproc)"

# 模型直链下载（URL 已从官方 download-funasr-model.sh 核实；HF_BASE 可换 hf-mirror.com）
RUN mkdir -p /src/model \
    && curl -fsSL -o /src/model/sensevoice-small-q8.gguf \
         ${HF_BASE}/FunAudioLLM/SenseVoiceSmall-GGUF/resolve/main/sensevoice-small-q8.gguf \
    && curl -fsSL -o /src/model/fsmn-vad.gguf \
         ${HF_BASE}/FunAudioLLM/fsmn-vad-GGUF/resolve/main/fsmn-vad.gguf

# ── 运行阶段：裸 ubuntu + 单二进制，零 apt、零网络依赖 ──
FROM ubuntu:24.04
COPY --from=builder /src/runtime/llama.cpp/build/bin/sensevoice-server /usr/local/bin/
# OpenMP 运行时：builder 的 gcc 套件自带，裸 ubuntu 缺失；两侧同为 noble，直接拷贝兼容
COPY --from=builder /usr/lib/x86_64-linux-gnu/libgomp.so.1 /usr/lib/x86_64-linux-gnu/

ENV MODEL_FILE=sensevoice-small-q8.gguf \
    VAD_FILE=fsmn-vad.gguf

EXPOSE 8040

# 模型经 -v <宿主机models目录>:/models 挂载进来；缺失时给出明确提示
# 性能参数全部环境变量化（compose 里覆盖，无需重建镜像）。默认值依据
# sensevoice-server.cpp 源码审查 + 25.9s 连续语料实测（0.29.1）：
#   SV_VAD_MAXSEG=8000  强制切段上限（默认 30000 太长：FSMN-VAD 的端点静音
#                       阈值写死且以 60s 为步长跳变——会话前 60 秒恒需 ~1850ms
#                       静音才切段，自然停顿达不到；而 partial 对整个开放段
#                       同步重识别且阻塞读循环，段越长积压越重。8s 上界让单次
#                       partial ≤~0.6s，定稿及时，与本地引擎切分窗口
#                       （soft 10s/hard 14s）同量级）
#   SV_PARTIAL_MS=600   partial 节流（默认 400；600 省 1/3 CPU，预览仍流畅）
#   SV_THREADS=4        推理线程（默认 8；按宿主机物理核数调，过度订阅反而抖动）
ENTRYPOINT ["/bin/sh", "-c", \
  "for f in /models/$MODEL_FILE /models/$VAD_FILE; do [ -f \"$f\" ] || { echo \"模型未找到: $f（请挂载: -v <宿主机models目录>:/models，或 -e MODEL_FILE=... 修正文件名）\"; exit 1; }; done; \
   exec sensevoice-server -m /models/$MODEL_FILE -vad /models/$VAD_FILE \
     --max-connections ${SV_MAX_CONN:-4} \
     --vad-maxseg ${SV_VAD_MAXSEG:-8000} \
     --partial-ms ${SV_PARTIAL_MS:-600} \
     --threads ${SV_THREADS:-4} \
     0.0.0.0 8040"]
