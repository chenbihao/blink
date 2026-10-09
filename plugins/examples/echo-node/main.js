/**
 * echo 示例脚本插件（Node.js 版，0.25.20 托管解释器链路验证）。
 *
 * JSONL stdio 协议：每行一个完整 JSON。
 * - 收到 {"type":"query","id":...,"query":...} → 回 {"type":"response","id":...,"items":[...]}
 * - items[0] 把查询文本原样回显，action 为 copy（回车复制原文）
 *
 * UTF-8 铁则（0.6）：readline 必须显式 encoding: "utf8"，
 * 否则 Windows 默认代码页下中文双向乱码。
 */

const readline = require("readline");

const rl = readline.createInterface({
    input: process.stdin,
    output: process.stdout,
    terminal: false,
    encoding: "utf8", // 关键！
});

rl.on("line", (line) => {
    line = (line || "").trim();
    if (!line) return;
    let req;
    try {
        req = JSON.parse(line);
    } catch {
        process.stderr.write(`echo-node: 无法解析请求行: ${line.slice(0, 80)}\n`);
        return;
    }
    // cancel / http_response 对 echo 无意义，忽略
    if (req.type !== "query") return;
    const query = req.query || "";
    const items = [
        {
            title: query ? `echo: ${query}` : "echo（无输入）",
            subtitle: "按回车复制原文 · 托管 Node.js 运行时",
            score: 0.5,
            action: {type: "copy", text: query},
        },
    ];
    process.stdout.write(
        JSON.stringify({type: "response", id: req.id, items}) + "\n",
    );
});
