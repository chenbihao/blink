//! 用户图片编辑会话的唯一状态所有者。
//!
//! 截图 SESSION 只负责捕获；本对象只描述编辑器当前消费的图片来源、底图与输出
//! 生命周期。它不持有 ImageStash 引用，也不读取任何 Tauri API。

export const IMAGE_SOURCE = Object.freeze({
  NONE: 'none',
  SCREENSHOT: 'screenshot',
  LONG_SCREENSHOT: 'long-screenshot',
  CLIPBOARD: 'clipboard',
});

export class ImageEditorSession {
  constructor() {
    this.epoch = 0;
    this.reset();
  }

  reset() {
    this.epoch = (this.epoch || 0) + 1;
    this.source = IMAGE_SOURCE.NONE;
    this.baseCanvas = null;
    this.screenX = 0;
    this.screenY = 0;
    this.ownsScreenshotSession = false;
  }

  beginScreenshotSelection() {
    this.reset();
    this.source = IMAGE_SOURCE.SCREENSHOT;
    this.ownsScreenshotSession = true;
  }

  beginCanvasSource(source, baseCanvas, options = {}) {
    if (!baseCanvas || !Number.isFinite(baseCanvas.width) || !Number.isFinite(baseCanvas.height)
      || baseCanvas.width < 1 || baseCanvas.height < 1) {
      throw new TypeError('图片编辑底图必须是非空 canvas');
    }
    if (![IMAGE_SOURCE.LONG_SCREENSHOT, IMAGE_SOURCE.CLIPBOARD].includes(source)) {
      throw new TypeError(`不支持的图片编辑来源: ${source}`);
    }
    this.epoch = (this.epoch || 0) + 1;
    this.source = source;
    this.baseCanvas = baseCanvas;
    this.screenX = Number.isFinite(options.screenX) ? options.screenX : 0;
    this.screenY = Number.isFinite(options.screenY) ? options.screenY : 0;
    this.ownsScreenshotSession = source === IMAGE_SOURCE.LONG_SCREENSHOT;
  }

  get active() {
    return this.source !== IMAGE_SOURCE.NONE;
  }

  get canvasBacked() {
    return this.baseCanvas !== null;
  }

  get canUseCaptureCropFastPath() {
    return this.source === IMAGE_SOURCE.SCREENSHOT && !this.canvasBacked;
  }
}
