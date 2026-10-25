/**
 * 翻译插件页面逻辑（0.28.3）。
 *
 * 会话方向策略（phase §3.4）：页面选择器是「当前插件窗口会话目标」——
 * 优先于已保存默认、但不回写设置；关闭窗口即丢弃，重开回默认。
 *
 * 竞态防护（spec-frontend §5.1 同源原则）：invoke 无客户端取消，切方向/
 * 引擎/新输入都自增 epoch，旧响应到达即丢弃——旧结果不污染新结果。
 * 正文不持久化：仅存于页面内存（关闭隐藏保留、重启不恢复）。
 */
(function () {
  "use strict";

  var zh = {
    targets: [["auto", "自动（中英互换）"], ["zh", "中文"], ["en", "英文"], ["ja", "日文"], ["ko", "韩文"]],
    engines: [["ali", "阿里翻译"], ["baidu", "百度翻译"], ["tencent", "腾讯翻译"], ["youdao", "有道智云"], ["deepl", "DeepL"]],
    copied: "已复制",
    copy: "复制",
    translating: "翻译中…",
    failed: "翻译失败，可重试",
    empty: "输入为空",
  };
  var en = {
    targets: [["auto", "Auto (zh<->en)"], ["zh", "Chinese"], ["en", "English"], ["ja", "Japanese"], ["ko", "Korean"]],
    engines: [["ali", "Alibaba"], ["baidu", "Baidu"], ["tencent", "Tencent"], ["youdao", "Youdao"], ["deepl", "DeepL"]],
    copied: "Copied",
    copy: "Copy",
    translating: "Translating…",
    failed: "Translation failed, retry",
    empty: "Empty input",
  };
  var i18n = zh;

  var input = document.getElementById("input");
  var output = document.getElementById("output");
  var targetSel = document.getElementById("target");
  var engineSel = document.getElementById("engine");
  var statusEl = document.getElementById("status");
  var descEl = document.getElementById("desc");
  var copyBtn = document.getElementById("copy");
  var swapBtn = document.getElementById("swap");

  var epoch = 0;            // 请求代次：旧响应丢弃
  var debounceTimer = 0;

  function fillSelect(sel, pairs, value) {
    sel.innerHTML = "";
    pairs.forEach(function (pair) {
      var opt = document.createElement("option");
      opt.value = pair[0];
      opt.textContent = pair[1];
      sel.appendChild(opt);
    });
    sel.value = value;
  }

  /** 会话方向：auto 也显式传（插件按 auto 互换语义处理；不传会落回保存设置） */
  function translateNow() {
    var text = input.value.trim();
    clearTimeout(debounceTimer);
    var my = ++epoch;
    if (!text) {
      output.value = "";
      statusEl.textContent = "";
      descEl.textContent = "";
      return;
    }
    statusEl.textContent = i18n.translating;
    descEl.textContent = "";
    var args = { text: text, engine: engineSel.value, target_lang: targetSel.value };
    window.Blink.invoke("translate", args).then(function (res) {
      if (my !== epoch) return; // 过期响应丢弃
      if (!res || res.ok !== true) throw new Error("failed");
      output.value = res.content || "";
      descEl.textContent = res.desc || "";
      statusEl.textContent = "";
    }).catch(function () {
      if (my !== epoch) return;
      output.value = "";
      statusEl.textContent = i18n.failed;
      descEl.textContent = "";
    });
  }

  function translateDebounced() {
    clearTimeout(debounceTimer);
    debounceTimer = setTimeout(translateNow, 500);
  }

  /** 中英互换：auto → 按输入检测反向；zh↔en 直接翻转；ja/ko 保持 */
  function swapDirection() {
    var current = targetSel.value;
    if (current === "zh") targetSel.value = "en";
    else if (current === "en") targetSel.value = "zh";
    else if (current === "auto") {
      // 粗检测：含 CJK 汉字 → 译英，否则 → 译中
      var cjk = /[\u4e00-\u9fff]/.test(input.value);
      targetSel.value = cjk ? "en" : "zh";
    }
    translateNow();
  }

  function init() {
    return window.Blink.init("builtin.translate").then(function (ctx) {
      if (ctx && ctx.context && String(ctx.context.language).indexOf("zh") !== 0) i18n = en;
      return window.Blink.settings().then(function (settings) {
        settings = settings || {};
        fillSelect(targetSel, i18n.targets, settings.target_lang || "auto");
        fillSelect(engineSel, i18n.engines, settings.default_engine || "ali");
        // 唤起载荷：主窗带入的待翻译文本
        var payloadText = ctx && ctx.payload && ctx.payload.text;
        if (payloadText) {
          input.value = String(payloadText);
          // 程序化整串写入后显式滚动跟随（spec-frontend §5.8）
          input.scrollTop = input.scrollHeight;
          translateNow();
        }
      });
    });
  }

  input.addEventListener("input", translateDebounced);
  input.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      translateNow();
    }
  });
  targetSel.addEventListener("change", translateNow);
  engineSel.addEventListener("change", translateNow);
  swapBtn.addEventListener("click", swapDirection);
  copyBtn.addEventListener("click", function () {
    if (!output.value) return;
    window.Blink.copy(output.value).then(function () {
      copyBtn.textContent = i18n.copied;
      copyBtn.classList.add("copied");
      setTimeout(function () {
        copyBtn.textContent = i18n.copy;
        copyBtn.classList.remove("copied");
      }, 1200);
    });
  });
  document.addEventListener("keydown", function (e) {
    if (e.key === "Escape") window.Blink.close();
  });

  // 复用窗口再次唤起：新载荷替换输入并立即翻译
  window.Blink.onPayload(function (payload) {
    if (payload && typeof payload.text === "string") {
      input.value = payload.text;
      input.scrollTop = input.scrollHeight;
      translateNow();
    }
  });

  init().catch(function (e) {
    statusEl.textContent = String(e && e.message ? e.message : e);
  });
})();
