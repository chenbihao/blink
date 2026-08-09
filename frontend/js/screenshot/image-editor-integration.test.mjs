import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const indexSource = await readFile(new URL('./index.js', import.meta.url), 'utf8');
const outputSource = await readFile(new URL('./ss-output.js', import.meta.url), 'utf8');

assert.match(indexSource, /blink-screenshot\.localhost\/editor/,
  '通用图片必须从独立 editor 载荷路径读取');
assert.ok(
  (indexSource.match(/!ss\.screenshot && !ss\.editorSession\.canvasBacked/g) || []).length >= 3,
  'mousedown/mousemove/mouseup/dblclick 不能再把通用图片挡在截图对象门禁外',
);
assert.match(indexSource, /scrollButton\.hidden = source === IMAGE_SOURCE\.CLIPBOARD/,
  '剪贴板编辑必须隐藏截图专属长截图入口');
assert.match(outputSource, /ss\.editorSession\.canUseCaptureCropFastPath/,
  'SESSION 裁剪快路径必须只由截图来源启用');
assert.match(outputSource, /source !== IMAGE_SOURCE\.CLIPBOARD/,
  '输出必须按编辑来源选择截图或通用适配器');
assert.match(outputSource, /imageEditorCancel\(\)/,
  '通用编辑取消不得借用 screenshot_cancel');

console.log('image editor integration contract tests passed');
