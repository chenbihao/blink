/**
 * Blink 插件宿主 SDK（0.28.2 §3.2 受限桥）——插件页面与宿主的唯一对话面。
 *
 * 由 `blink-plugin` 协议的 `/@host/sdk.js` 提供（宿主内嵌，版本随应用）。
 * 页面通过 `<script src="/@host/sdk.js"></script>` 引用，然后：
 *
 *   const ctx = await Blink.init("builtin.translate");  // 声明自己的 plugin_id
 *   // ctx = { payload: <打开窗口时的初始载荷|null>, context: { language, theme } }
 *   const res = await Blink.invoke("translate", { text: "hello", target_lang: "zh" });
 *   // res = { ok: true, kind: "text", content: "你好", desc: "en→zh · 阿里翻译" }
 *
 * 边界（宿主侧强制，SDK 只是薄封装）：
 * - `init` 声明的 plugin_id 必须与本窗口归属一致——冒用其他插件身份的调用被拒；
 * - `invoke` 只能调本插件 manifest 声明的 tool（CapabilityRegistry owner 复核）；
 * - 主题样式由 init 后自动注入 `/@host/theme.css`（跟随宿主当前主题）。
 *
 * 竞态防护（页面侧责任，spec-frontend §5.1）：invoke 无客户端取消，切方向/
 * 引擎后旧请求仍会返回——页面用自增 epoch 丢弃过期响应。
 */
(function () {
  "use strict";

  var TAU = window.__TAURI__;
  if (!TAU || !TAU.core || !TAU.core.invoke) {
    // 宿主环境缺失时保留可诊断的占位，页面可据此显示降级提示
    window.Blink = {
      unavailable: true,
      init: function () { return Promise.reject(new Error("Blink host SDK unavailable")); },
    };
    return;
  }
  var invoke = TAU.core.invoke;

  function BlinkSDK() { this.payloadHandlers = []; }
  BlinkSDK.prototype.pluginId = null;

  /** 声明页面归属并完成就绪握手；返回初始 payload 与 {language, theme}。 */
  BlinkSDK.prototype.init = function (pluginId) {
    var self = this;
    self.pluginId = String(pluginId);
    var initializing = true;
    var pending;
    var hasPending = false;
    self.payloadChannel = new TAU.core.Channel();
    self.payloadChannel.onmessage = function (payload) {
      if (initializing) {
        pending = payload;
        hasPending = true;
        return;
      }
      self.payloadHandlers.slice().forEach(function (cb) { cb(payload); });
    };
    return invoke("plugin_page_ready", { pluginId: self.pluginId, onPayload: self.payloadChannel }).then(function (res) {
      initializing = false;
      if (hasPending) res.payload = pending;
      var theme = res && res.context && res.context.theme;
      if (theme && document.head) {
        var link = document.createElement("link");
        link.rel = "stylesheet";
        link.href = "/@host/theme.css?name=" + encodeURIComponent(theme);
        document.head.appendChild(link);
      }
      return res;
    });
  };

  /** 调用本插件声明的 tool。返回 {ok, kind, content|items|summary|error, desc}。 */
  BlinkSDK.prototype.invoke = function (tool, args) {
    return invoke("plugin_page_invoke", {
      pluginId: this.pluginId,
      tool: String(tool),
      args: args || {},
    });
  };

  /** 读取本插件非秘密设置（脱敏：密钥字段不回传）。 */
  BlinkSDK.prototype.settings = function () {
    return invoke("plugin_page_get_settings", { pluginId: this.pluginId });
  };

  /** 写入宿主剪贴板（走宿主剪贴板服务，进入剪贴板历史）。 */
  BlinkSDK.prototype.copy = function (text) {
    return invoke("plugin_page_clipboard", {
      pluginId: this.pluginId,
      text: String(text == null ? "" : text),
    });
  };

  /** 隐藏本窗口（每次唤起宿主都会派发一次载荷：带载荷预填，null 为空唤起重置新查询）。 */
  BlinkSDK.prototype.close = function () {
    return invoke("plugin_page_close", { pluginId: this.pluginId });
  };

  /** 订阅宿主推送的唤起载荷（复用窗口再次打开时的新 payload）。 */
  BlinkSDK.prototype.onPayload = function (cb) {
    var handlers = this.payloadHandlers;
    handlers.push(cb);
    return Promise.resolve(function () {
      var index = handlers.indexOf(cb);
      if (index >= 0) handlers.splice(index, 1);
    });
  };

  window.Blink = new BlinkSDK();
})();
