#!/usr/bin/env node
/**
 * Blink Node.js 插件示例：回显输入内容
 *
 * JSONL stdio 协议：
 *   stdin  每行一个 JSON 请求
 *   stdout 每行一个 JSON 响应
 */

// Node.js 编码处理：
// Windows 中文环境下，process.stdin 默认是 GBK，需要显式设置为 UTF-8
// 注意：Node.js 的 stdin.setEncoding('utf8') 只是解码层，
// 实际还需要确保 IO 流的编码一致性

const readline = require('readline');

// 配置 readline 使用 UTF-8 编码
const rl = readline.createInterface({
  input: process.stdin,
  output: process.stdout,
  terminal: false,
  encoding: 'utf8'  // 关键：强制 UTF-8 解码
});

// stderr 也输出 UTF-8（Node.js 默认是 Buffer，需要手动编码）
function log(...args) {
  const msg = args.map(String).join(' ');
  process.stderr.write(Buffer.from(`[echo-node] ${msg}\n`, 'utf8'));
}

log('插件启动');

rl.on('line', (line) => {
  try {
    line = line.trim();
    if (!line) return;

    const req = JSON.parse(line);
    const type = req.type;

    if (type === 'query') {
      const queryId = req.id || '';
      const query = req.query || '';
      const settings = req.settings || {};

      log(`收到查询: id=${queryId}, query=${JSON.stringify(query)}`);

      // 构造回显结果
      const items = [
        {
          title: `Node.js 回显: ${query || '(空)'}`,
          subtitle: `触发词: ${settings.prefix || 'echonode'}, 版本: ${process.version}`,
          score: 1.0,
          action: {
            type: 'copy',
            text: query
          }
        }
      ];

      // 输出 JSON 响应（必须是单行，UTF-8 编码）
      const response = JSON.stringify({ id: queryId, items });
      process.stdout.write(Buffer.from(response + '\n', 'utf8'));
    }
    else if (type === 'cancel') {
      log(`取消请求: ${req.id}`);
    }
  } catch (e) {
    log(`错误: ${e.message}`);
  }
});

rl.on('close', () => {
  log('stdin 关闭，进程退出');
  process.exit(0);
});

process.on('SIGTERM', () => {
  log('收到 SIGTERM，退出');
  process.exit(0);
});

process.on('SIGINT', () => {
  log('收到 SIGINT，退出');
  process.exit(0);
});
